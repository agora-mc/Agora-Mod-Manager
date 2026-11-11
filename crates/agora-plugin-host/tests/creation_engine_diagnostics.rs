//! The Creation Engine pre-flight diagnostics, run as the plugin script that owns them.
//!
//! Every DLL and plugin header here is built from invented bytes (`common/dll.rs` for the PE
//! files, a hand-written TES4 record for the plugin headers). The cases are the hostile ones the
//! brief names: a DLL built for another game version, one built for this one, an old-format DLL, an
//! unreadable DLL, an empty plugin folder, no game version, a 1.71 plugin header on an old game,
//! and the same header with the backported ESL support present.

#[path = "../../agora-core/tests/common/dll.rs"]
mod dll_fixture;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agora_plugin_api::host::{
    ActivationRequest, GameReads, HostBridge, ScriptHost, ScriptReadError,
};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;

use dll_fixture::{dll, dll_exporting, dll_without_exports, VersionData};

const PLUGIN: &str = "agora.creation-engine";

struct NoopBridge;

impl HostBridge for NoopBridge {
    fn call(&self, _plugin_id: &PluginId, request: HostRequest) -> HostResponse {
        HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::json!(null),
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

fn key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

struct ViewReads {
    files: HashMap<String, Vec<u8>>,
    /// Paths whose read must fail as if the file could not be opened.
    unreadable: Vec<String>,
}

impl GameReads for ViewReads {
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        let wanted = key(path);
        if self.unreadable.iter().any(|path| key(path) == wanted) {
            return Err(ScriptReadError::new(
                "unreadable",
                format!("there is no readable {path} in this view"),
            ));
        }
        let bytes = self.files.get(&wanted).ok_or_else(|| {
            ScriptReadError::new("not_found", format!("there is no {path} in this view"))
        })?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start >= bytes.len() {
            return Ok(Vec::new());
        }
        let end = start
            .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
            .min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn read_lines(
        &self,
        _path: &str,
        _from_line: u64,
        _max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        Ok(serde_json::json!({ "lines": [] }))
    }

    fn list(&self, path: &str) -> Result<serde_json::Value, ScriptReadError> {
        let wanted = key(path);
        let prefix = if wanted.is_empty() {
            String::new()
        } else {
            format!("{wanted}/")
        };
        let mut found: std::collections::BTreeMap<String, (bool, u64)> =
            std::collections::BTreeMap::new();
        for (file, bytes) in &self.files {
            let Some(rest) = file.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let name = rest.split('/').next().unwrap_or(rest).to_string();
            let is_dir = rest.contains('/');
            found.insert(name, (is_dir, bytes.len() as u64));
        }
        let entries: Vec<serde_json::Value> = found
            .into_iter()
            .map(
                |(name, (dir, size))| serde_json::json!({ "name": name, "dir": dir, "size": size }),
            )
            .collect();
        Ok(serde_json::json!({ "entries": entries, "cut": false }))
    }
}

fn engine_host() -> (tempfile::TempDir, QuickJsHost, PluginId) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/official/creation-engine");
    let source = std::fs::read_to_string(root.join("main.js"))
        .expect("the base plugin's script is in the repository");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    std::fs::create_dir_all(dir.path().join("crash")).unwrap();
    std::fs::copy(
        root.join("crash/addrlib-names.js"),
        dir.path().join("crash/addrlib-names.js"),
    )
    .expect("the name table is in the repository");
    let id = PluginId::parse(PLUGIN).unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: id.clone(),
            package_root: dir.path().to_path_buf(),
            data_root: dir.path().to_path_buf(),
            entrypoint: "main.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .expect("the base plugin activates");
    (dir, host, id)
}

/// A TES4 record with an HEDR subrecord and a MAST/DATA pair per master.
fn plugin_bytes(hedr: f32, flags: u32, masters: &[&str]) -> Vec<u8> {
    fn subrecord(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(kind);
        out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(data);
    }
    let mut header = Vec::new();
    header.extend_from_slice(&hedr.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    let mut body = Vec::new();
    subrecord(&mut body, b"HEDR", &header);
    for master in masters {
        let mut name = master.as_bytes().to_vec();
        name.push(0);
        subrecord(&mut body, b"MAST", &name);
        subrecord(&mut body, b"DATA", &[0u8; 8]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"TES4");
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&body);
    out
}

fn entry(name: &str) -> serde_json::Value {
    serde_json::json!({ "id": name, "enabled": true, "locked": false, "present": true })
}

fn disabled(name: &str) -> serde_json::Value {
    serde_json::json!({ "id": name, "enabled": false, "locked": false, "present": true })
}

fn context(version: &str, entries: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "instance": "test",
        "runtime": { "game": "skyrim-se", "store": "steam", "version": version },
        "load_order": { "entries": entries, "rules": [] }
    })
}

fn run(
    files: Vec<(String, Vec<u8>)>,
    unreadable: Vec<String>,
    runtime: &str,
    entries: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let (_dir, host, id) = engine_host();
    let reads = ViewReads {
        files: files
            .into_iter()
            .map(|(name, bytes)| (key(&name), bytes))
            .collect(),
        unreadable: unreadable.into_iter().map(|name| key(&name)).collect(),
    };
    let args = serde_json::json!({ "context": context(runtime, entries) });
    let reply = host
        .invoke_with_game_reads(
            &id,
            "creationEngineDiagnostics",
            args,
            Duration::from_secs(5),
            Arc::new(reads),
        )
        .expect("the diagnostics hook runs");
    host.deactivate(&id).ok();
    reply
        .as_array()
        .expect("the diagnostics hook returns a list")
        .clone()
}

fn message(diagnostic: &serde_json::Value) -> &str {
    diagnostic["message"].as_str().unwrap_or("")
}

fn severity(diagnostic: &serde_json::Value) -> &str {
    diagnostic["severity"].as_str().unwrap_or("")
}

fn warnings(diagnostics: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    diagnostics
        .iter()
        .filter(|diagnostic| severity(diagnostic) == "warning")
        .collect()
}

const DLL_DIR: &str = "Data/SKSE/Plugins";

fn dll_path(name: &str) -> String {
    format!("{DLL_DIR}/{name}")
}

#[test]
fn a_dll_declaring_another_game_version_is_a_warning() {
    let diagnostics = run(
        vec![(
            dll_path("InventedPlugin.dll"),
            dll(&VersionData {
                compatible: vec![[1, 5, 97, 0]],
                ..VersionData::default()
            }),
        )],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    let warned = warnings(&diagnostics);
    assert_eq!(warned.len(), 1, "{diagnostics:#?}");
    assert_eq!(warned[0]["code"], "CE_SKSE_PLUGIN_OTHER_GAME_VERSION");
    let text = message(warned[0]).to_lowercase();
    assert!(text.contains("inventedplugin.dll"), "{text}");
    assert!(text.contains("another game version"), "{text}");
}

#[test]
fn a_dll_declaring_this_game_version_is_quiet() {
    let diagnostics = run(
        vec![(
            dll_path("InventedPlugin.dll"),
            dll(&VersionData {
                compatible: vec![[1, 6, 1170, 0]],
                ..VersionData::default()
            }),
        )],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
}

#[test]
fn an_old_format_dll_is_counted_once_not_warned() {
    let diagnostics = run(
        vec![(
            dll_path("OldFormat.dll"),
            dll_exporting("SomethingElse", None),
        )],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
    let info: Vec<&serde_json::Value> = diagnostics
        .iter()
        .filter(|diagnostic| severity(diagnostic) == "info")
        .collect();
    assert_eq!(info.len(), 1, "{diagnostics:#?}");
    assert!(message(info[0]).contains('1'), "{}", message(info[0]));
    assert!(
        message(info[0]).contains("could not be judged"),
        "{}",
        message(info[0])
    );
}

#[test]
fn an_unreadable_dll_is_counted_not_a_warning() {
    let diagnostics = run(
        vec![(dll_path("Broken.dll"), vec![0u8; 8])],
        vec![dll_path("Broken.dll")],
        "1.6.1170",
        &[],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| message(diagnostic).contains("could not be judged")),
        "{diagnostics:#?}"
    );
}

#[test]
fn an_empty_plugin_folder_says_there_is_nothing_to_read() {
    let diagnostics = run(
        vec![("Data/SomethingElse.txt".into(), b"hello".to_vec())],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
    assert!(
        diagnostics.iter().any(|diagnostic| message(diagnostic)
            .to_lowercase()
            .contains("no skse plugins")),
        "{diagnostics:#?}"
    );
}

#[test]
fn without_a_game_version_the_check_says_once_it_could_not_run() {
    let diagnostics = run(
        vec![(dll_path("InventedPlugin.dll"), dll_without_exports())],
        Vec::new(),
        "",
        &[],
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(severity(&diagnostics[0]), "warning");
    let text = message(&diagnostics[0]).to_lowercase();
    assert!(text.contains("game version"), "{text}");
    assert!(text.contains("could not"), "{text}");
}

#[test]
fn a_171_header_on_an_old_game_is_a_warning_naming_the_plugin() {
    let diagnostics = run(
        vec![("Data/InventedPlugin.esp".into(), plugin_bytes(1.71, 0, &[]))],
        Vec::new(),
        "1.5.97",
        &[entry("InventedPlugin.esp")],
    );
    let warned = warnings(&diagnostics);
    assert_eq!(warned.len(), 1, "{diagnostics:#?}");
    assert_eq!(warned[0]["code"], "CE_PLUGIN_HEADER_NEEDS_NEWER_GAME");
    assert!(
        message(warned[0]).contains("InventedPlugin.esp"),
        "{}",
        message(warned[0])
    );
    assert!(
        message(warned[0]).contains("1.6.1130"),
        "{}",
        message(warned[0])
    );
}

#[test]
fn a_170_header_a_new_enough_game_and_a_disabled_plugin_are_all_quiet() {
    // 1.70 is the older header format.
    let old = run(
        vec![("Data/InventedPlugin.esp".into(), plugin_bytes(1.70, 0, &[]))],
        Vec::new(),
        "1.5.97",
        &[entry("InventedPlugin.esp")],
    );
    assert!(warnings(&old).is_empty(), "{old:#?}");
    // 1.71 is fine on a game that has the format.
    let new = run(
        vec![("Data/InventedPlugin.esp".into(), plugin_bytes(1.71, 0, &[]))],
        Vec::new(),
        "1.6.1170",
        &[entry("InventedPlugin.esp")],
    );
    assert!(warnings(&new).is_empty(), "{new:#?}");
    // A plugin the game does not load is not checked.
    let off = run(
        vec![("Data/InventedPlugin.esp".into(), plugin_bytes(1.71, 0, &[]))],
        Vec::new(),
        "1.5.97",
        &[disabled("InventedPlugin.esp")],
    );
    assert!(warnings(&off).is_empty(), "{off:#?}");
}

#[test]
fn the_backported_esl_plugin_makes_a_171_header_acceptable() {
    let diagnostics = run(
        vec![
            ("Data/InventedPlugin.esp".into(), plugin_bytes(1.71, 0, &[])),
            (dll_path("BackportedESLSupport.dll"), dll_without_exports()),
        ],
        Vec::new(),
        "1.5.97",
        &[entry("InventedPlugin.esp")],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
}

#[test]
fn an_unreadable_plugin_header_is_not_a_warning() {
    let diagnostics = run(
        vec![("Data/Broken.esp".into(), b"not a plugin".to_vec())],
        Vec::new(),
        "1.5.97",
        &[entry("Broken.esp")],
    );
    assert!(warnings(&diagnostics).is_empty(), "{diagnostics:#?}");
}
