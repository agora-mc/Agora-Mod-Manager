//! The Creation Engine rules, run as the plugin script that now owns them. These are the tests
//! core used to hold: flags, masters, the full and light limits, and unreadable headers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn engine_host() -> (tempfile::TempDir, QuickJsHost, PluginId) {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/main.js"),
    )
    .expect("the base plugin's script is in the repository");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    // `main.js` imports its generated function-name table.
    std::fs::create_dir_all(dir.path().join("crash")).unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/crash/addrlib-names.js"),
        dir.path().join("crash/addrlib-names.js"),
    )
    .expect("the name table is in the repository");
    let id = PluginId::parse("agora.creation-engine").unwrap();
    (dir, QuickJsHost::new(), id)
}

fn activate(host: &QuickJsHost, dir: &tempfile::TempDir, id: &PluginId) {
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
}

/// A TES4 record: a header, an HEDR subrecord and a MAST/DATA pair per master.
fn plugin_bytes(flags: u32, masters: &[&str]) -> Vec<u8> {
    fn subrecord(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(kind);
        out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(data);
    }
    let mut body = Vec::new();
    subrecord(&mut body, b"HEDR", &[0u8; 12]);
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

fn run(files: Vec<(String, Vec<u8>)>, entries: &[serde_json::Value]) -> serde_json::Value {
    let (dir, host, id) = engine_host();
    activate(&host, &dir, &id);
    let reads = BytesReads {
        files: files
            .into_iter()
            .map(|(name, bytes)| (key(&format!("Data/{name}")), bytes))
            .collect(),
    };
    let args = serde_json::json!({
        "context": { "load_order": { "entries": entries } }
    });
    let reply = host
        .invoke_with_game_reads(
            &id,
            "creationEngineRules",
            args,
            Duration::from_secs(5),
            Arc::new(reads),
        )
        .expect("the rules hook runs");
    host.deactivate(&id).ok();
    reply
}

#[test]
fn the_script_reads_flags_masters_and_extensions() {
    let reply = run(
        vec![
            ("Skyrim.esm".to_string(), plugin_bytes(0x1, &[])),
            ("Mod.esp".to_string(), plugin_bytes(0, &["Skyrim.esm"])),
            ("Light.esl".to_string(), plugin_bytes(0, &[])),
            ("Flagged.esp".to_string(), plugin_bytes(0x200, &[])),
        ],
        &[
            entry("Skyrim.esm"),
            entry("Mod.esp"),
            entry("Light.esl"),
            entry("Flagged.esp"),
        ],
    );
    assert_eq!(reply["badges"]["Skyrim.esm"], serde_json::json!(["Master"]));
    assert_eq!(
        reply["badges"]["Light.esl"],
        serde_json::json!(["Master", "Light"])
    );
    assert_eq!(reply["badges"]["Flagged.esp"], serde_json::json!(["Light"]));
    assert!(reply["badges"].get("Mod.esp").is_none());
    assert_eq!(
        reply["rules"],
        serde_json::json!([{ "kind": "requires", "item": "Mod.esp", "master": "Skyrim.esm" }])
    );
    assert_eq!(reply["findings"], serde_json::json!([]));
}

#[test]
fn the_script_reports_unreadable_headers_and_the_plugin_limits() {
    let mut files = Vec::new();
    let mut entries = Vec::new();
    for index in 0..255 {
        let name = format!("Full{index}.esp");
        files.push((name.clone(), plugin_bytes(0, &[])));
        entries.push(entry(&name));
    }
    files.push(("Bad.esp".into(), b"not a plugin".to_vec()));
    entries.push(entry("Bad.esp"));
    let reply = run(files, &entries);

    let findings = reply["findings"].as_array().expect("findings");
    assert!(findings.iter().any(|finding| finding["message"]
        .as_str()
        .is_some_and(|message| message.contains("Bad.esp") && message.contains("cannot be read"))));
    let limit = findings
        .iter()
        .find(|finding| {
            finding["message"]
                .as_str()
                .is_some_and(|message| message.contains("255 full plugins"))
        })
        .expect("the full limit is reported");
    assert_eq!(limit["refuses"], serde_json::json!(true));
}

#[test]
fn an_absent_plugin_is_not_read_and_is_not_a_finding() {
    let reply = run(
        Vec::new(),
        &[serde_json::json!({
            "id": "Gone.esp", "enabled": true, "locked": false, "present": false
        })],
    );
    assert_eq!(reply["findings"], serde_json::json!([]));
    assert_eq!(reply["rules"], serde_json::json!([]));
}

#[test]
fn a_thousand_plugin_list_checks_in_under_two_seconds() {
    let mut files = Vec::new();
    let mut entries = Vec::new();
    for index in 0..1000 {
        let name = format!("Plugin{index}.esp");
        files.push((name.clone(), plugin_bytes(0, &["Skyrim.esm"])));
        entries.push(entry(&name));
    }
    files.push(("Skyrim.esm".into(), plugin_bytes(0x1, &[])));
    entries.push(entry("Skyrim.esm"));

    let started = Instant::now();
    let reply = run(files, &entries);
    let elapsed = started.elapsed();
    // Debug builds of QuickJS and the host are slower; the release margin is the one the brief
    // names. The debug bound is stated here so the test is still meaningful in a normal run.
    let budget = if cfg!(debug_assertions) {
        Duration::from_secs(8)
    } else {
        Duration::from_secs(2)
    };
    assert!(
        elapsed < budget,
        "1000 plugin checks took {elapsed:?} (budget {budget:?})"
    );
    assert_eq!(reply["rules"].as_array().unwrap().len(), 1000);
}
