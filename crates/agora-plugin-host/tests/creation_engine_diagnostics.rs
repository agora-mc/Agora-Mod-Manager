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
    /// Sizes the listing reports instead of the bytes' length. Lets a test describe a file too
    /// large to build without allocating it.
    listed_sizes: HashMap<String, u64>,
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
            let size = self
                .listed_sizes
                .get(&format!("{prefix}{name}"))
                .copied()
                .unwrap_or(bytes.len() as u64);
            found.insert(name, (is_dir, size));
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
    run_with_listed_sizes(files, Vec::new(), unreadable, runtime, entries)
}

fn run_with_listed_sizes(
    files: Vec<(String, Vec<u8>)>,
    listed_sizes: Vec<(String, u64)>,
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
        listed_sizes: listed_sizes
            .into_iter()
            .map(|(name, size)| (key(&name), size))
            .collect(),
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
fn the_bees_alias_also_makes_a_171_header_acceptable() {
    let diagnostics = run(
        vec![
            ("Data/InventedPlugin.esp".into(), plugin_bytes(1.71, 0, &[])),
            (dll_path("bees.dll"), dll_without_exports()),
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

/// An invented minimal `.ess` header: the plugin block starts right after a short fixed prefix,
/// with the count, length-prefixed names and active byte the parser looks for.
fn ess_bytes(plugins: &[(&str, bool)]) -> Vec<u8> {
    let mut out = b"TESV".to_vec();
    out.extend_from_slice(&[0u8; 8]);
    out.push(u8::try_from(plugins.len()).unwrap());
    for (name, active) in plugins {
        out.push(u8::try_from(name.len()).unwrap());
        out.extend_from_slice(name.as_bytes());
        out.push(u8::from(*active));
    }
    out
}

/// The plugin block at the start of an SE save's decompressed body.
fn se_plugin_block(full: &[&str], light: &[&str]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(u8::try_from(full.len()).unwrap());
    for name in full {
        payload.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(name.as_bytes());
    }
    payload.extend_from_slice(&u16::try_from(light.len()).unwrap().to_le_bytes());
    for name in light {
        payload.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(name.as_bytes());
    }
    let mut out = vec![0x4e];
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// One LZ4 block made only of literals, which is valid and enough for the parser.
fn se_lz4_literals(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut extra = bytes.len();
    let head = extra.min(15);
    out.push(u8::try_from(head << 4).unwrap());
    if head == 15 {
        extra -= 15;
        while extra >= 255 {
            out.push(255);
            extra -= 255;
        }
        out.push(u8::try_from(extra).unwrap());
    }
    out.extend_from_slice(bytes);
    out
}

/// An invented Skyrim SE 1.6 save: the magic, a 79-byte header, a 1x1 screenshot, the decompressed
/// and compressed sizes, and the given LZ4 bytes. No bytes come from a real save.
fn se_save_with_body(uncompressed: usize, compressed: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    header.extend_from_slice(&12u32.to_le_bytes()); // version
    header.extend_from_slice(&1u32.to_le_bytes()); // save number
    header.extend_from_slice(&8u16.to_le_bytes());
    header.extend_from_slice(b"Prisoner");
    header.extend_from_slice(&1u32.to_le_bytes()); // level
    header.extend_from_slice(&6u16.to_le_bytes());
    header.extend_from_slice(b"Skyrim");
    header.extend_from_slice(&9u16.to_le_bytes());
    header.extend_from_slice(b"000.01.33");
    header.extend_from_slice(&8u16.to_le_bytes());
    header.extend_from_slice(b"NordRace");
    header.extend_from_slice(&0u16.to_le_bytes()); // sex
    header.extend_from_slice(&0f32.to_le_bytes()); // current experience
    header.extend_from_slice(&100f32.to_le_bytes()); // experience to the next level
    header.extend_from_slice(&0u64.to_le_bytes()); // file time
    header.extend_from_slice(&1u32.to_le_bytes()); // screenshot width
    header.extend_from_slice(&1u32.to_le_bytes()); // screenshot height
    header.extend_from_slice(&2u16.to_le_bytes()); // screenshot format
    assert_eq!(header.len(), 79);
    let mut out = Vec::new();
    out.extend_from_slice(b"TESV_SAVEGAME");
    out.extend_from_slice(&u32::try_from(header.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&[0u8; 4]); // the 1x1 RGBA screenshot
    out.extend_from_slice(&u32::try_from(uncompressed).unwrap().to_le_bytes());
    out.extend_from_slice(&u32::try_from(compressed.len()).unwrap().to_le_bytes());
    out.extend_from_slice(compressed);
    out
}

fn se_save_bytes(full: &[&str], light: &[&str]) -> Vec<u8> {
    let block = se_plugin_block(full, light);
    let compressed = se_lz4_literals(&block);
    se_save_with_body(block.len(), &compressed)
}

/// An invented SE save whose plugin block is followed by bytes that are not valid LZ4, and whose
/// stated body is much larger than the block. A reader that decodes the whole body fails on the
/// tail; the reader must stop once the plugin block is out. No bytes come from a real save.
fn se_save_with_invalid_tail(full: &[&str], light: &[&str]) -> Vec<u8> {
    let block = se_plugin_block(full, light);
    let mut compressed = se_lz4_literals(&block);
    // No literals and a match offset of zero: not a valid LZ4 sequence.
    compressed.extend_from_slice(&[0x00, 0x00, 0x00]);
    se_save_with_body(block.len() + 4 * 1024 * 1024, &compressed)
}

/// An invented SE save whose plugin block says it is one byte over the decode limit, with only
/// the marker and size present. The reader should refuse it by name without decoding the rest.
fn se_save_with_oversized_plugin_block() -> Vec<u8> {
    const DECLARED: u32 = 8 * 1024 * 1024 + 1;
    let mut head = vec![0x4e];
    head.extend_from_slice(&DECLARED.to_le_bytes());
    let compressed = se_lz4_literals(&head);
    se_save_with_body(head.len() + DECLARED as usize, &compressed)
}

#[test]
fn an_se_save_is_decompressed_and_its_full_and_light_plugins_are_read() {
    let diagnostics = run(
        vec![(
            "@saves/se.ess".to_string(),
            se_save_bytes(&["Skyrim.esm", "Update.esm"], &["InventedLight.esl"]),
        )],
        Vec::new(),
        "1.6.1170",
        &[
            entry("Skyrim.esm"),
            entry("Update.esm"),
            disabled("InventedLight.esl"),
        ],
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic["code"] != "CE_SAVE_PLUGINS_NOT_CHECKED"),
        "{diagnostics:#?}"
    );
    let warned = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_NEEDS_INACTIVE_PLUGIN")
        .unwrap_or_else(|| panic!("expected the save warning, got {diagnostics:#?}"));
    let text = message(warned);
    assert!(text.contains("InventedLight.esl"), "{text}");
    assert!(!text.contains("Skyrim.esm"), "{text}");
}

#[test]
fn an_lz4_save_whose_full_and_light_plugins_are_active_says_nothing() {
    let diagnostics = run(
        vec![(
            "@saves/se-clean.ess".to_string(),
            se_save_bytes(&["Skyrim.esm", "Update.esm"], &["InventedLight.esl"]),
        )],
        Vec::new(),
        "1.6.1170",
        &[
            entry("Skyrim.esm"),
            entry("Update.esm"),
            entry("InventedLight.esl"),
        ],
    );
    assert!(
        diagnostics.iter().all(
            |diagnostic| diagnostic["code"] != "CE_SAVE_NEEDS_INACTIVE_PLUGIN"
                && diagnostic["code"] != "CE_SAVE_PLUGINS_NOT_CHECKED"
        ),
        "{diagnostics:#?}"
    );
}

#[test]
fn an_se_save_stops_decoding_once_the_plugin_block_is_out() {
    let diagnostics = run(
        vec![(
            "@saves/large.ess".to_string(),
            se_save_with_invalid_tail(&["Skyrim.esm"], &["InventedLight.esl"]),
        )],
        Vec::new(),
        "1.6.1170",
        &[entry("Skyrim.esm"), disabled("InventedLight.esl")],
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic["code"] != "CE_SAVE_PLUGINS_NOT_CHECKED"),
        "{diagnostics:#?}"
    );
    let warned = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_NEEDS_INACTIVE_PLUGIN")
        .unwrap_or_else(|| panic!("expected the save warning, got {diagnostics:#?}"));
    assert!(message(warned).contains("InventedLight.esl"), "{warned:#?}");
}

#[test]
fn an_oversized_plugin_block_names_the_decode_limit() {
    let diagnostics = run(
        vec![(
            "@saves/huge.ess".to_string(),
            se_save_with_oversized_plugin_block(),
        )],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    let note = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_PLUGINS_NOT_CHECKED")
        .unwrap_or_else(|| panic!("expected a not-checked note, got {diagnostics:#?}"));
    let text = message(note);
    assert!(text.contains("huge.ess"), "{text}");
    assert!(text.contains("8388609"), "{text}");
    assert!(text.contains("8388608"), "{text}");
    assert!(text.contains("byte limit Agora decodes"), "{text}");
}

#[test]
fn a_save_over_the_read_limit_names_that_limit() {
    const LIMIT: u64 = 128 * 1024 * 1024;
    let diagnostics = run_with_listed_sizes(
        vec![("@saves/enormous.ess".to_string(), b"TESV_SAVEGAME".to_vec())],
        vec![("@saves/enormous.ess".to_string(), LIMIT + 1)],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    let note = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_PLUGINS_NOT_CHECKED")
        .unwrap_or_else(|| panic!("expected a not-checked note, got {diagnostics:#?}"));
    let text = message(note);
    assert!(text.contains("enormous.ess"), "{text}");
    assert!(text.contains(&(LIMIT + 1).to_string()), "{text}");
    assert!(text.contains(&LIMIT.to_string()), "{text}");
    assert!(text.contains("byte limit Agora reads"), "{text}");
}

#[test]
fn a_save_that_needs_a_now_inactive_plugin_is_a_warning_naming_it() {
    let diagnostics = run(
        vec![(
            "@saves/new-save.ess".to_string(),
            ess_bytes(&[("Active.esp", true), ("Switched Off.esp", true)]),
        )],
        Vec::new(),
        "1.6.1170",
        &[entry("Active.esp"), disabled("Switched Off.esp")],
    );
    let warned = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_NEEDS_INACTIVE_PLUGIN")
        .unwrap_or_else(|| panic!("expected the save warning, got {diagnostics:#?}"));
    assert_eq!(severity(warned), "warning");
    let text = message(warned);
    assert!(text.contains("new-save.ess"), "{text}");
    assert!(text.contains("Switched Off.esp"), "{text}");
    assert!(!text.contains("Active.esp"), "{text}");
}

#[test]
fn a_save_whose_plugins_are_all_active_says_nothing() {
    let diagnostics = run(
        vec![(
            "@saves/clean.ess".to_string(),
            ess_bytes(&[("Active.esp", true)]),
        )],
        Vec::new(),
        "1.6.1170",
        &[entry("Active.esp")],
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic["code"] != "CE_SAVE_NEEDS_INACTIVE_PLUGIN"),
        "{diagnostics:#?}"
    );
}

#[test]
fn an_unreadable_save_header_is_reported_not_treated_as_clean() {
    let diagnostics = run(
        vec![(
            "@saves/broken.ess".to_string(),
            b"TESV not a save header".to_vec(),
        )],
        Vec::new(),
        "1.6.1170",
        &[],
    );
    let note = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "CE_SAVE_PLUGINS_NOT_CHECKED")
        .unwrap_or_else(|| panic!("expected a not-checked note, got {diagnostics:#?}"));
    assert_eq!(severity(note), "info");
    assert!(message(note).contains("broken.ess"), "{}", message(note));
}
