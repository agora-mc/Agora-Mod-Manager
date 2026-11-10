//! The first inspector end to end: a crash log, a test instance, and an invented DLL that declares
//! a different game version. With the instance the DLL is named and the answer can reach
//! `very_likely`; without one the same log names nothing new and no inspector runs. Every name here
//! is invented.

#[path = "common/dll.rs"]
mod dll_fixture;

use agora_core::crash_engine::attribution::{InstanceView, LayerOwner, LayerView, Resolver};
use agora_core::crash_engine::{
    analyze, AnalyzeInput, AnalyzeOutput, CrashFile, InstanceAnalysis, InstanceInspector,
};
use agora_core::crash_packs::{load_plugin_packs, LoadedCrashPack};
use agora_game_api::crash::{Family, GameHandle, InspectResult, Level, Owner};
use agora_plugin_api::host::{
    ActivationRequest, GameReads, HostBridge, ScriptHost, ScriptReadError,
};
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

use dll_fixture::{dll, VersionData};

const PLUGIN: &str = "agora.creation-engine";
const GAME: &str = "skyrim-se";
const RUNNING: &str = "1.6.1170";
const DLL_NAME: &str = "InventedPlugin.dll";
const DLL_PATH: &str = "Data/SKSE/Plugins/InventedPlugin.dll";

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
    pack: LoadedCrashPack,
}

fn start() -> Running {
    let root = official_root();
    let text = std::fs::read_to_string(root.join("agora-plugin.json")).unwrap();
    let manifest = PluginManifest::parse(&text).unwrap();
    let report = load_plugin_packs(PLUGIN, &root, &manifest);
    assert!(report.is_clean(), "{:#?}", report.problems);
    let pack = report.packs[0].clone();
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
        pack,
    }
}

/// A Crash Logger 1.16 log whose stack and SKSE plugin list both name `InventedPlugin.dll`.
fn log() -> String {
    format!(
        "Skyrim SSE v{RUNNING}\nCrashLoggerSSE v1-16-0-0 Jan  1 2026 00:00:00\n\n\
Unhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF700001000 SkyrimSE.exe+001000\tmov rax, [rcx]\n\n\
SYSTEM SPECS:\n\tOS: Microsoft Windows 11 Pro v10.0.26200\n\tCPU: InventedVendor Invented CPU 9200\n\
\tGPU #1: Invented Graphics 9200\n\tPHYSICAL MEMORY: 15.50 GB/16.00 GB\n\n\
PROBABLE CALL STACK:\n\t[ 0] 0x7FF700000000 SkyrimSE.exe+001000\n\
\t[ 1] 0x7FF700001000 {DLL_NAME}+001000\n\n\
MODULES:\n\tSkyrimSE.exe                 0x7FF700000000\n\n\
SKSE PLUGINS:\n\t{DLL_NAME} v1.0.0\n\n\
PLUGINS:\n\tLight: 0\tRegular: 2\tTotal: 2\n\t[ 0]     Skyrim.esm\n\t[ 1]     InventedPatch.esp\n"
    )
}

struct OneFile {
    bytes: Vec<u8>,
}

impl GameReads for OneFile {
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        if !path.eq_ignore_ascii_case(DLL_PATH) {
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
        path.eq_ignore_ascii_case(DLL_PATH)
            .then_some(self.bytes.len() as u64)
    }
}

fn view() -> InstanceView {
    InstanceView::new(vec![LayerView::new(
        "bad-mod",
        true,
        LayerOwner::Item {
            key: "bad-mod".to_string(),
            name: "Invented Plugin Mod".to_string(),
        },
        vec![DLL_PATH.to_string()],
    )])
}

fn with_instance(running: &Running, bytes: Vec<u8>) -> AnalyzeOutput {
    let resolver = Resolver::new(
        view(),
        running.pack.pack.ref_maps.clone(),
        running.pack.pack.roles.clone(),
    );
    let scope: Arc<dyn GameReads> = Arc::new(OneFile { bytes });
    let inspector = InstanceInspector::new(
        &running.host,
        std::slice::from_ref(&running.pack),
        &resolver,
        scope,
        GameHandle {
            id: GAME.to_string(),
            runtime_version: Some(RUNNING.to_string()),
        },
    );
    let output = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&running.pack),
        files: &[CrashFile {
            name: "crash-2026-01-01-12-00-00.log".into(),
            bytes: log().into_bytes(),
        }],
        runtime_version: Some(RUNNING),
        host: &running.host,
        built_in: &[],
        instance: Some(InstanceAnalysis {
            game_id: GAME,
            resolver: &resolver,
            inspector: &inspector,
        }),
    })
    .unwrap();
    assert!(output.read_any(), "{:#?}", output.files);
    output
}

fn loose(running: &Running) -> AnalyzeOutput {
    let output = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&running.pack),
        files: &[CrashFile {
            name: "crash-2026-01-01-12-00-00.log".into(),
            bytes: log().into_bytes(),
        }],
        runtime_version: Some(RUNNING),
        host: &running.host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    assert!(output.read_any(), "{:#?}", output.files);
    output
}

fn names(output: &AnalyzeOutput) -> Vec<String> {
    output
        .answer
        .candidates
        .iter()
        .map(|candidate| match &candidate.owner {
            Owner::Item { name, .. } => name.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn rule_fired(output: &AnalyzeOutput, rule: &str) -> bool {
    output
        .answer
        .findings
        .iter()
        .any(|finding| finding.source.rule_id.as_deref() == Some(rule))
}

#[test]
fn an_instance_names_a_listed_dll_that_declares_another_game_version() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 5, 97, 0]],
        ..VersionData::default()
    };
    let output = with_instance(&running, dll(&data));

    assert_eq!(
        names(&output),
        vec!["Invented Plugin Mod"],
        "{:#?}",
        output.answer
    );
    assert_eq!(output.answer.level, Level::VeryLikely);
    assert!(rule_fired(
        &output,
        "skse_plugin_built_for_another_game_version"
    ));
    assert!(rule_fired(
        &output,
        "stack_module_built_for_another_game_version"
    ));
    let inspection = output
        .answer
        .inspections
        .iter()
        .find(|inspection| inspection.inspector == "skse_plugin_declaration")
        .expect("the answer shows the inspection");
    assert_eq!(inspection.result, InspectResult::Fail);
    assert_eq!(inspection.target, DLL_NAME);
    assert!(
        !inspection.facts.is_empty(),
        "the declaration reaches the answer"
    );
    assert!(inspection
        .detail
        .as_deref()
        .unwrap_or("")
        .contains("built for another game version"));
}

#[test]
fn a_dll_that_declares_this_game_version_names_nobody() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 6, 1170, 0]],
        ..VersionData::default()
    };
    let output = with_instance(&running, dll(&data));
    // The older, log-only rules may still name the mod through the stack; the inspector must not.
    assert!(!rule_fired(
        &output,
        "skse_plugin_built_for_another_game_version"
    ));
    assert!(!rule_fired(
        &output,
        "stack_module_built_for_another_game_version"
    ));
    assert!(output
        .answer
        .candidates
        .iter()
        .all(|candidate| !candidate.families.contains(&Family::VerifiedAsset)));
    assert!(output.answer.level < Level::VeryLikely);
    let inspection = output
        .answer
        .inspections
        .iter()
        .find(|inspection| inspection.inspector == "skse_plugin_declaration")
        .expect("the answer shows the inspection");
    assert_eq!(inspection.result, InspectResult::Pass);
}

#[test]
fn without_an_instance_the_same_log_names_nothing_new_and_runs_no_inspector() {
    let running = start();
    let output = loose(&running);
    // The loose file may still name the DLL as itself through the older log-only rules; it must not
    // reach the mod the instance would have provided, and no inspector may run.
    assert!(!names(&output)
        .iter()
        .any(|name| name == "Invented Plugin Mod"));
    assert!(!rule_fired(
        &output,
        "skse_plugin_built_for_another_game_version"
    ));
    assert!(!rule_fired(
        &output,
        "stack_module_built_for_another_game_version"
    ));
    assert!(output.answer.inspections.is_empty());
    assert!(!output
        .answer
        .could_not_check
        .iter()
        .any(|check| check.subject == "skse_plugin_declaration"));
}

#[test]
fn the_inspector_only_runs_for_the_instance_game() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 5, 97, 0]],
        ..VersionData::default()
    };
    let resolver = Resolver::new(
        view(),
        running.pack.pack.ref_maps.clone(),
        running.pack.pack.roles.clone(),
    );
    let scope: Arc<dyn GameReads> = Arc::new(OneFile { bytes: dll(&data) });
    let inspector = InstanceInspector::new(
        &running.host,
        std::slice::from_ref(&running.pack),
        &resolver,
        scope,
        GameHandle {
            id: "another-game".to_string(),
            runtime_version: Some(RUNNING.to_string()),
        },
    );
    let output = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&running.pack),
        files: &[CrashFile {
            name: "crash-2026-01-01-12-00-00.log".into(),
            bytes: log().into_bytes(),
        }],
        runtime_version: Some(RUNNING),
        host: &running.host,
        built_in: &[],
        instance: Some(InstanceAnalysis {
            game_id: "another-game",
            resolver: &resolver,
            inspector: &inspector,
        }),
    })
    .unwrap();
    // The pack is for skyrim-se, so another game's analysis does not load it.
    assert!(output.answer.inspections.is_empty());
    assert!(output.answer.candidates.is_empty());
}

#[test]
fn a_ref_that_is_not_in_the_instance_is_not_inspected() {
    let running = start();
    let data = VersionData {
        compatible: vec![[1, 5, 97, 0]],
        ..VersionData::default()
    };
    // The DLL is in the log, but the instance's layer does not provide it.
    let resolver = Resolver::new(
        InstanceView::new(vec![LayerView::new(
            "bad-mod",
            true,
            LayerOwner::Item {
                key: "bad-mod".to_string(),
                name: "Invented Plugin Mod".to_string(),
            },
            Vec::new(),
        )]),
        running.pack.pack.ref_maps.clone(),
        running.pack.pack.roles.clone(),
    );
    let scope: Arc<dyn GameReads> = Arc::new(OneFile { bytes: dll(&data) });
    let inspector = InstanceInspector::new(
        &running.host,
        std::slice::from_ref(&running.pack),
        &resolver,
        scope,
        GameHandle {
            id: GAME.to_string(),
            runtime_version: Some(RUNNING.to_string()),
        },
    );
    let output = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&running.pack),
        files: &[CrashFile {
            name: "crash-2026-01-01-12-00-00.log".into(),
            bytes: log().into_bytes(),
        }],
        runtime_version: Some(RUNNING),
        host: &running.host,
        built_in: &[],
        instance: Some(InstanceAnalysis {
            game_id: GAME,
            resolver: &resolver,
            inspector: &inspector,
        }),
    })
    .unwrap();
    assert!(
        output.answer.inspections.is_empty(),
        "{:#?}",
        output.answer.inspections
    );
    assert!(!rule_fired(
        &output,
        "skse_plugin_built_for_another_game_version"
    ));
}
