//! Read-only cross-check of the Creation Engine script against `esplugin`, on real plugin files.
//!
//! Run it with a folder of real plugin files:
//!
//! ```text
//! AGORA_CE_FIXTURE=D:/tmp/lootspike/data \
//!   cargo test -p agora-plugin-host --test creation_engine_crosscheck -- --ignored --nocapture
//! ```
//!
//! Nothing from that folder is read into a test fixture or committed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agora_plugin_api::host::{
    ActivationRequest, GameReads, HostBridge, ScriptHost, ScriptReadError,
};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;

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

struct BytesReads {
    files: HashMap<String, Vec<u8>>,
}

fn key(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

impl GameReads for BytesReads {
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        let bytes = self.files.get(&key(path)).ok_or_else(|| {
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

    fn list(&self, _path: &str) -> Result<serde_json::Value, ScriptReadError> {
        Ok(serde_json::json!({ "entries": [], "cut": false }))
    }
}

fn plugin_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .expect("read the fixture folder")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "esm" | "esp" | "esl"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

#[test]
#[ignore = "reads real plugin files and needs AGORA_CE_FIXTURE=<folder>"]
fn the_script_agrees_with_esplugin_on_every_real_plugin() {
    let folder = std::env::var_os("AGORA_CE_FIXTURE")
        .map(PathBuf::from)
        .expect("AGORA_CE_FIXTURE names a folder of real plugin files");
    let files = plugin_files(&folder);
    assert!(!files.is_empty(), "{} holds no plugins", folder.display());

    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/main.js"),
    )
    .expect("the base plugin's script is in the repository");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), source).unwrap();

    let host = QuickJsHost::new();
    let id = PluginId::parse("agora.creation-engine").unwrap();
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
    .unwrap();

    let mut reads = HashMap::new();
    let mut entries = Vec::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a plugin file name");
        let bytes = std::fs::read(path).expect("read the plugin");
        reads.insert(key(&format!("Data/{name}")), bytes);
        entries.push(serde_json::json!({
            "id": name, "enabled": true, "locked": false, "present": true
        }));
    }
    let args = serde_json::json!({
        "context": { "load_order": { "entries": entries } }
    });
    let reply = host
        .invoke_with_game_reads(
            &id,
            "creationEngineRules",
            args,
            Duration::from_secs(60),
            Arc::new(BytesReads { files: reads }),
        )
        .expect("the rules hook runs");
    host.deactivate(&id).ok();

    let rules = reply["rules"].as_array().unwrap();
    let badges = reply["badges"].as_object().unwrap();
    for path in &files {
        let name = path.file_name().and_then(|name| name.to_str()).unwrap();
        let file = std::fs::File::open(path).unwrap();
        let mut plugin = esplugin::Plugin::new(esplugin::GameId::SkyrimSE, path);
        plugin
            .parse_reader(file, esplugin::ParseOptions::header_only())
            .unwrap_or_else(|error| panic!("esplugin reads {name}: {error}"));
        let expected_masters = plugin.masters().unwrap();
        let expected_master = plugin.is_master_file();
        let expected_light = plugin.is_light_plugin();

        let mut masters: Vec<String> = rules
            .iter()
            .filter(|rule| rule["item"].as_str() == Some(name))
            .map(|rule| rule["master"].as_str().unwrap().to_string())
            .collect();
        masters.sort();
        let mut expected = expected_masters.clone();
        expected.sort();
        assert_eq!(masters, expected, "{name}: masters");

        let marks: Vec<&str> = badges
            .get(name)
            .and_then(|value| value.as_array())
            .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(
            marks.contains(&"Master"),
            expected_master,
            "{name}: master flag"
        );
        assert_eq!(
            marks.contains(&"Light"),
            expected_light,
            "{name}: light flag"
        );
    }
    println!("cross-checked {} real plugins", files.len());
}
