//! The Creation Engine pack's SKSE declaration inspector, through the real QuickJS host.
//!
//! Every DLL here is built by `common::dll` from invented bytes. Nothing is downloaded, and no real
//! DLL is copied. The cases are the hostile ones the brief names: no export, a wrong export, a
//! truncated file, a non-PE file, an old data version, and a header that tries to make the reader
//! read far outside the file.

#[path = "common/dll.rs"]
mod dll_fixture;

use agora_core::crash_packs::{
    artifact_handle, load_plugin_packs, parser_request, run_inspector, InspectorDispatch,
    MemoryArtifact,
};
use agora_game_api::crash::{
    AttrValue, Fact, GameHandle, InspectResult, InspectorDecl, ParserLimits,
};
use agora_plugin_api::host::{
    ActivationRequest, GameReads, HostBridge, ScriptHost, ScriptReadError,
};
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

use dll_fixture::{dll, dll_exporting, dll_without_exports, VersionData};

const PLUGIN: &str = "agora.creation-engine";

struct NoopBridge;

impl HostBridge for NoopBridge {
    fn call(&self, _plugin_id: &PluginId, request: HostRequest) -> HostResponse {
        HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

fn official_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/official/creation-engine")
}

struct Running {
    _data: TempDir,
    host: QuickJsHost,
    inspector: InspectorDecl,
}

fn start() -> Running {
    let root = official_root();
    let text = std::fs::read_to_string(root.join("agora-plugin.json")).unwrap();
    let manifest = PluginManifest::parse(&text).unwrap();
    let report = load_plugin_packs(PLUGIN, &root, &manifest);
    assert!(report.is_clean(), "{:#?}", report.problems);
    let inspector = report.packs[0]
        .pack
        .inspectors
        .iter()
        .find(|inspector| inspector.id == "skse_plugin_declaration")
        .expect("the pack declares the SKSE inspector")
        .clone();
    let data = tempfile::tempdir().unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: PluginId::parse(PLUGIN).unwrap(),
            package_root: root,
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
    Running {
        _data: data,
        host,
        inspector,
    }
}

fn inspect(running: &Running, bytes: &[u8], runtime: Option<&str>) -> InspectorDispatch {
    let limits = ParserLimits::DEFAULT;
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("target", bytes.to_vec());
    let request = parser_request(
        "skse_plugin_declaration",
        artifact_handle(
            "target",
            "InventedPlugin.dll",
            bytes.len() as u64,
            Vec::new(),
        ),
        GameHandle {
            id: "skyrim-se".into(),
            runtime_version: runtime.map(str::to_string),
        },
        limits,
    );
    run_inspector(
        &running.host,
        PLUGIN,
        &running.inspector,
        &request,
        Arc::new(artifact),
    )
}

fn output(dispatch: &InspectorDispatch) -> &agora_game_api::crash::InspectorOutput {
    match dispatch {
        InspectorDispatch::Inspected(output) => output,
        InspectorDispatch::Failed(failure) => panic!("the inspector failed: {failure:?}"),
    }
}

fn result(dispatch: &InspectorDispatch) -> InspectResult {
    output(dispatch).result
}

fn declaration(output: &agora_game_api::crash::InspectorOutput) -> &Fact {
    output
        .facts
        .iter()
        .find(|fact| fact.kind == "creation.skse_plugin_declaration")
        .expect("the declaration fact")
}

fn text_attr<'a>(fact: &'a Fact, name: &str) -> Option<&'a str> {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(agora_game_api::crash::Scalar::Text(text))) => Some(text),
        _ => None,
    }
}

fn bool_attr(fact: &Fact, name: &str) -> Option<bool> {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(agora_game_api::crash::Scalar::Bool(value))) => Some(*value),
        _ => None,
    }
}

#[test]
fn a_compatible_version_list_passes_and_reports_what_it_declares() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 6, 1170, 0]],
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), Some("1.6.1170"));
    assert_eq!(
        result(&dispatch),
        InspectResult::Pass,
        "{:?}",
        output(&dispatch)
    );
    let fact = declaration(output(&dispatch));
    assert_eq!(text_attr(fact, "plugin_version"), Some("1.2.3"));
    assert_eq!(text_attr(fact, "name"), Some("Invented Plugin"));
    assert_eq!(bool_attr(fact, "address_independent"), Some(false));
    assert_eq!(bool_attr(fact, "uses_no_structs"), Some(false));
    match fact.attrs.get("compatible_runtimes") {
        Some(AttrValue::List(list)) => assert_eq!(
            list,
            &vec![agora_game_api::crash::Scalar::Text("1.6.1170".into())]
        ),
        other => panic!("expected the compatible runtimes, got {other:?}"),
    }
    assert!(output(&dispatch)
        .detail
        .as_deref()
        .unwrap_or("")
        .contains("supports game version 1.6.1170"));
}

#[test]
fn a_different_version_list_fails_without_claiming_certainty() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 5, 97, 0]],
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Fail);
    let detail = output(&dispatch).detail.clone().unwrap_or_default();
    assert!(
        detail.contains("built for another game version"),
        "{detail}"
    );
    assert!(!detail.to_lowercase().contains("definitely"), "{detail}");
    assert_eq!(
        declaration(output(&dispatch)).kind,
        "creation.skse_plugin_declaration"
    );
}

#[test]
fn an_address_independent_plugin_without_the_struct_flag_passes_before_629_and_fails_after() {
    let running = start();
    // AddressLibraryPostAE, no NoStructUse, no StructsPost629: the shape the brief warns about.
    let data = VersionData {
        version_independence: 0x1,
        version_independence_ex: 0x2,
        ..VersionData::default()
    };
    let bytes = dll(&data);
    // 1.6.629 is the change, so 1.6.353 is before it and 1.6.640 and 1.6.1170 are after.
    assert_eq!(
        result(&inspect(&running, &bytes, Some("1.6.353"))),
        InspectResult::Pass
    );
    for after in ["1.6.640", "1.6.1170"] {
        let dispatch = inspect(&running, &bytes, Some(after));
        assert_eq!(result(&dispatch), InspectResult::Fail, "{after}");
        let detail = output(&dispatch).detail.clone().unwrap_or_default();
        assert!(
            detail.contains("structs that changed in 1.6.629"),
            "{detail}"
        );
    }
}

#[test]
fn the_no_struct_use_flag_makes_a_new_game_safe() {
    let running = start();
    let data = VersionData {
        version_independence: 0x1,
        version_independence_ex: 0x3,
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Pass);
    assert_eq!(
        bool_attr(declaration(output(&dispatch)), "uses_no_structs"),
        Some(true)
    );
}

#[test]
fn the_post_629_struct_flag_makes_a_new_game_safe_and_an_old_game_unsafe() {
    let running = start();
    let data = VersionData {
        version_independence: 0x5,
        ..VersionData::default()
    };
    let bytes = dll(&data);
    assert_eq!(
        result(&inspect(&running, &bytes, Some("1.6.1170"))),
        InspectResult::Pass
    );
    assert_eq!(
        result(&inspect(&running, &bytes, Some("1.6.640"))),
        InspectResult::Pass
    );
    assert_eq!(
        result(&inspect(&running, &bytes, Some("1.6.353"))),
        InspectResult::Fail
    );
}

#[test]
fn a_missing_export_is_unknown() {
    let running = start();
    let data = VersionData::default();
    let dispatch = inspect(
        &running,
        &dll_exporting("SomethingElse", Some(&data)),
        Some("1.6.1170"),
    );
    assert_eq!(result(&dispatch), InspectResult::Unknown);
    assert!(output(&dispatch).facts.is_empty());
}

#[test]
fn a_pe_without_exports_is_unknown() {
    let running = start();
    let dispatch = inspect(&running, &dll_without_exports(), Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn a_truncated_file_is_unknown() {
    let running = start();
    let full = dll(&VersionData::default());
    let dispatch = inspect(&running, &full[..100], Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn a_non_pe_file_is_unknown() {
    let running = start();
    let dispatch = inspect(&running, b"this is not a DLL", Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn no_running_version_is_unknown_but_the_declaration_is_still_reported() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 6, 1170, 0]],
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), None);
    assert_eq!(result(&dispatch), InspectResult::Unknown);
    assert_eq!(
        text_attr(declaration(output(&dispatch)), "plugin_version"),
        Some("1.2.3")
    );
}

#[test]
fn an_older_data_version_is_unknown() {
    let running = start();
    let data = VersionData {
        data_version: 2,
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn a_version_specific_plugin_with_no_versions_is_unknown_not_a_pass() {
    let running = start();
    let data = VersionData {
        version_independence: 0,
        version_independence_ex: 0x2,
        compatible: Vec::new(),
        ..VersionData::default()
    };
    let dispatch = inspect(&running, &dll(&data), Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn a_hostile_dos_header_cannot_make_it_pass() {
    let running = start();
    let mut bytes = dll(&VersionData::default());
    bytes[0x3c..0x40].copy_from_slice(&0xffff_ff00u32.to_le_bytes());
    let dispatch = inspect(&running, &bytes, Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

#[test]
fn a_hostile_export_name_count_cannot_make_it_pass() {
    let running = start();
    let mut bytes = dll(&VersionData::default());
    // The section starts at file offset 0x200; NumberOfNames is 24 bytes into the export directory.
    bytes[0x200 + 24..0x200 + 28].copy_from_slice(&u32::MAX.to_le_bytes());
    let dispatch = inspect(&running, &bytes, Some("1.6.1170"));
    assert_eq!(result(&dispatch), InspectResult::Unknown);
}

/// The same DLL, reached through the pre-flight hook's `game.*` scope.
struct PreflightView {
    name: String,
    bytes: Vec<u8>,
}

impl GameReads for PreflightView {
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        let wanted = format!("Data/SKSE/Plugins/{}", self.name);
        if !path.eq_ignore_ascii_case(&wanted) {
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
        Err(ScriptReadError::new("unreadable", "no lines here"))
    }

    fn list(&self, path: &str) -> Result<serde_json::Value, ScriptReadError> {
        if !path.eq_ignore_ascii_case("Data/SKSE/Plugins") {
            return Err(ScriptReadError::new("not_found", "no such folder"));
        }
        Ok(serde_json::json!({
            "entries": [{ "name": self.name, "dir": false, "size": self.bytes.len() }],
            "cut": false,
        }))
    }
}

fn preflight(running: &Running, name: &str, bytes: &[u8], runtime: &str) -> Vec<serde_json::Value> {
    let reads = Arc::new(PreflightView {
        name: name.to_string(),
        bytes: bytes.to_vec(),
    });
    let args = serde_json::json!({
        "context": {
            "instance": "test",
            "runtime": { "game": "skyrim-se", "store": "steam", "version": runtime },
            "load_order": { "entries": [], "rules": [] }
        }
    });
    running
        .host
        .invoke_with_game_reads(
            &PluginId::parse(PLUGIN).unwrap(),
            "creationEngineDiagnostics",
            args,
            Duration::from_secs(5),
            reads,
        )
        .expect("the pre-flight hook runs")
        .as_array()
        .expect("the hook returns a list")
        .clone()
}

fn flags_another_game_version(diagnostics: &[serde_json::Value]) -> bool {
    diagnostics.iter().any(|diagnostic| {
        diagnostic["code"] == "CE_SKSE_PLUGIN_OTHER_GAME_VERSION"
            && diagnostic["severity"] == "warning"
    })
}

#[test]
fn the_inspector_and_the_preflight_give_the_same_verdict() {
    let running = start();
    let cases = [
        (
            "a plugin for this game",
            dll(&VersionData {
                compatible: vec![[1, 6, 1170, 0]],
                ..VersionData::default()
            }),
            InspectResult::Pass,
        ),
        (
            "a plugin for another game",
            dll(&VersionData {
                compatible: vec![[1, 5, 97, 0]],
                ..VersionData::default()
            }),
            InspectResult::Fail,
        ),
        (
            "an old-format plugin",
            dll_exporting("SomethingElse", None),
            InspectResult::Unknown,
        ),
    ];
    for (label, bytes, expected) in cases {
        let dispatch = inspect(&running, &bytes, Some("1.6.1170"));
        assert_eq!(result(&dispatch), expected, "{label}: inspector");
        let diagnostics = preflight(&running, "InventedPlugin.dll", &bytes, "1.6.1170");
        assert_eq!(
            flags_another_game_version(&diagnostics),
            expected == InspectResult::Fail,
            "{label}: the pre-flight disagrees: {diagnostics:#?}"
        );
    }
}
