//! The core inspector path: an instance finds an inspector in the calling plugin's pack, resolves
//! the target ref, and hands the script one file. A script that throws, returns garbage, asks for
//! another artifact, or names a file the inspector does not accept must never look like a pass.

use agora_core::crash_engine::attribution::{InstanceView, LayerOwner, LayerView, Resolver};
use agora_core::crash_engine::{Inspection, InspectionRequest, Inspector, InstanceInspector};
use agora_core::crash_packs::LoadedCrashPack;
use agora_game_api::crash::{
    CodeEntry, CrashPack, InspectorDecl, NameMatch, Ref, RefLookup, RefMap, Strength,
};
use agora_plugin_api::host::{ActivationRequest, GameReads, ScriptHost, ScriptReadError};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_host::QuickJsHost;
use std::path::Path;
use std::sync::Arc;

const PLUGIN: &str = "acme.inspector";
const TARGET: &str = "Data/SKSE/Plugins/Good.dll";

const SCRIPT: &str = r#"
import { crash } from "agora";
export async function pass(request) {
  const bytes = new Uint8Array(crash.readBytes(request.artifact.id, 0, 4));
  return {
    contract: 1,
    result: "pass",
    detail: "read " + bytes.length + " bytes",
    facts: [{ kind: "test.seen", attrs: { size: bytes.length }, refs: [] }],
  };
}
export async function fail(request) {
  return { contract: 1, result: "fail", detail: "the file is bad", facts: [] };
}
export async function throws(request) { throw new Error("boom"); }
export async function garbage(request) { return { hello: 1 }; }
export async function wrong_contract(request) {
  return { contract: 2, result: "pass", detail: "old", facts: [] };
}
export async function other(request) {
  crash.readBytes("other", 0, 1);
  return { contract: 1, result: "pass" };
}
"#;

struct OneFile {
    path: String,
    bytes: Vec<u8>,
}

impl OneFile {
    fn new() -> Self {
        Self {
            path: TARGET.to_string(),
            bytes: b"invented bytes".to_vec(),
        }
    }
}

impl GameReads for OneFile {
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        if !path.eq_ignore_ascii_case(&self.path) {
            return Err(ScriptReadError::new(
                "not_found",
                "that path is not in this instance",
            ));
        }
        let start = (offset as usize).min(self.bytes.len());
        let end = start.saturating_add(len as usize).min(self.bytes.len());
        Ok(self.bytes[start..end].to_vec())
    }

    fn read_lines(
        &self,
        _path: &str,
        _from_line: u64,
        _max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        Err(ScriptReadError::new(
            "unreadable",
            "this test reader has no lines",
        ))
    }

    fn list(&self, _path: &str) -> Result<serde_json::Value, ScriptReadError> {
        Err(ScriptReadError::new(
            "unreadable",
            "this test reader has no list",
        ))
    }

    fn size(&self, path: &str) -> Option<u64> {
        path.eq_ignore_ascii_case(&self.path)
            .then_some(self.bytes.len() as u64)
    }
}

fn resolver() -> Resolver {
    let view = InstanceView::new(vec![LayerView::new(
        "mod",
        true,
        LayerOwner::Item {
            key: "mod".to_string(),
            name: "Mod".to_string(),
        },
        vec![TARGET.to_string()],
    )]);
    Resolver::new(
        view,
        vec![RefMap {
            kind: "module".to_string(),
            lookup: RefLookup::Path {
                patterns: vec!["Data/SKSE/Plugins/{value}".to_string()],
                archives: false,
            },
            matching: NameMatch::Casefold,
            ignore: Vec::new(),
        }],
        Vec::new(),
    )
}

fn pack(export: &str, accepts: &[&str]) -> LoadedCrashPack {
    LoadedCrashPack {
        plugin_id: PLUGIN.to_string(),
        plugin_name: "Acme Inspector".to_string(),
        plugin_version: "1.0.0".to_string(),
        file: "crash/pack.json".to_string(),
        pack: CrashPack {
            contract: 1,
            games: Vec::new(),
            sources: Vec::new(),
            parsers: Vec::new(),
            inspectors: vec![InspectorDecl {
                id: "probe".to_string(),
                accepts: accepts.iter().map(|pattern| pattern.to_string()).collect(),
                entry: CodeEntry::Script {
                    export: export.to_string(),
                },
            }],
            ref_maps: Vec::new(),
            roles: Vec::new(),
            save_risk: None,
            rules: Vec::new(),
        },
    }
}

fn host() -> (tempfile::TempDir, tempfile::TempDir, QuickJsHost) {
    let package = tempfile::tempdir().unwrap();
    std::fs::write(Path::new(package.path()).join("main.js"), SCRIPT).unwrap();
    let data = tempfile::tempdir().unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: PluginId::parse(PLUGIN).unwrap(),
            package_root: package.path().to_path_buf(),
            data_root: data.path().to_path_buf(),
            entrypoint: "main.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .unwrap();
    (package, data, host)
}

struct NoopBridge;

impl agora_plugin_api::host::HostBridge for NoopBridge {
    fn call(
        &self,
        _plugin_id: &PluginId,
        request: agora_plugin_api::protocol::HostRequest,
    ) -> agora_plugin_api::protocol::HostResponse {
        agora_plugin_api::protocol::HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(
        &self,
        _plugin_id: &PluginId,
        _level: agora_plugin_api::protocol::LogLevel,
        _message: &str,
    ) {
    }
}

fn inspect_with(export: &str, accepts: &[&str], value: &str) -> Inspection {
    let (_package, _data, host) = host();
    let packs = vec![pack(export, accepts)];
    let resolver = resolver();
    let inspector = InstanceInspector::new(
        &host,
        &packs,
        &resolver,
        Arc::new(OneFile::new()),
        agora_game_api::crash::GameHandle {
            id: "test-game".to_string(),
            runtime_version: Some("1.0".to_string()),
        },
    );
    let target = Ref {
        kind: "module".to_string(),
        value: value.to_string(),
        strength: Strength::Mention,
    };
    inspector
        .inspect(&InspectionRequest {
            plugin_id: PLUGIN,
            inspector: "probe",
            target: &target,
        })
        .result
}

#[test]
fn a_valid_script_returns_its_detail_and_facts() {
    let (_package, _data, host) = host();
    let packs = vec![pack("pass", &["*.dll"])];
    let resolver = resolver();
    let inspector = InstanceInspector::new(
        &host,
        &packs,
        &resolver,
        Arc::new(OneFile::new()),
        agora_game_api::crash::GameHandle {
            id: "test-game".to_string(),
            runtime_version: None,
        },
    );
    let target = Ref {
        kind: "module".to_string(),
        value: "Good.dll".to_string(),
        strength: Strength::Mention,
    };
    let outcome = inspector.inspect(&InspectionRequest {
        plugin_id: PLUGIN,
        inspector: "probe",
        target: &target,
    });
    assert_eq!(outcome.result, Inspection::Pass);
    assert_eq!(outcome.detail.as_deref(), Some("read 4 bytes"));
    assert_eq!(outcome.facts.len(), 1);
    assert_eq!(outcome.facts[0].kind, "test.seen");
}

#[test]
fn a_throwing_script_is_failed_not_pass() {
    match inspect_with("throws", &["*.dll"], "Good.dll") {
        Inspection::Failed(message) => {
            assert!(message.contains("Acme Inspector"), "{message}");
            assert!(message.contains("boom"), "{message}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_garbage_output_is_failed_not_pass() {
    assert!(matches!(
        inspect_with("garbage", &["*.dll"], "Good.dll"),
        Inspection::Failed(_)
    ));
}

#[test]
fn a_wrong_contract_is_failed_not_pass() {
    assert!(matches!(
        inspect_with("wrong_contract", &["*.dll"], "Good.dll"),
        Inspection::Failed(_)
    ));
}

#[test]
fn a_ref_outside_the_instance_is_missing() {
    assert_eq!(
        inspect_with("pass", &["*.dll"], "Missing.dll"),
        Inspection::Missing
    );
}

#[test]
fn a_target_its_accepts_refuses_is_missing() {
    assert_eq!(
        inspect_with("pass", &["*.nif"], "Good.dll"),
        Inspection::Missing
    );
}

#[test]
fn a_script_cannot_read_another_artifact() {
    assert!(matches!(
        inspect_with("other", &["*.dll"], "Good.dll"),
        Inspection::Failed(_)
    ));
}

#[test]
fn an_inspector_from_another_plugin_is_missing() {
    let (_package, _data, host) = host();
    let packs = vec![pack("pass", &["*.dll"])];
    let resolver = resolver();
    let inspector = InstanceInspector::new(
        &host,
        &packs,
        &resolver,
        Arc::new(OneFile::new()),
        agora_game_api::crash::GameHandle {
            id: "test-game".to_string(),
            runtime_version: None,
        },
    );
    let target = Ref {
        kind: "module".to_string(),
        value: "Good.dll".to_string(),
        strength: Strength::Mention,
    };
    let outcome = inspector.inspect(&InspectionRequest {
        plugin_id: "other.plugin",
        inspector: "probe",
        target: &target,
    });
    assert_eq!(outcome.result, Inspection::Missing);
}

#[test]
fn a_missing_inspector_id_is_missing() {
    let (_package, _data, host) = host();
    let packs = vec![pack("pass", &["*.dll"])];
    let resolver = resolver();
    let inspector = InstanceInspector::new(
        &host,
        &packs,
        &resolver,
        Arc::new(OneFile::new()),
        agora_game_api::crash::GameHandle {
            id: "test-game".to_string(),
            runtime_version: None,
        },
    );
    let target = Ref {
        kind: "module".to_string(),
        value: "Good.dll".to_string(),
        strength: Strength::Mention,
    };
    let outcome = inspector.inspect(&InspectionRequest {
        plugin_id: PLUGIN,
        inspector: "nope",
        target: &target,
    });
    assert_eq!(outcome.result, Inspection::Missing);
}
