//! The Creation Engine plugin's record-overlap hook (MASTER_SPEC §31.3), run as
//! the real script with the real WebAssembly module over invented plugin files.
//!
//! The bytes follow UESP's "Skyrim Mod:Mod File Format" (a 24-byte record
//! header, a 24-byte group header whose size includes the header, and zlib
//! compressed fields prefixed by their decompressed size). Every name here is
//! invented.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agora_plugin_api::error::PluginError;
use agora_plugin_api::host::{
    path_matches_any, validate_blob_key, ActivationRequest, GameReads, HostBridge, ScriptHost,
    ScriptReadError, StageBudget, StageLimits, StoredBlobs,
};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use serde_json::json;

const INSTANCE: &str = "test-instance";
const COMPRESSED: u32 = 0x0004_0000;
const LIGHT: u32 = 0x0000_0200;

#[derive(Clone, Default)]
struct FakeBridge {
    storage: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

struct FakeBlobs {
    bridge: FakeBridge,
}

impl StoredBlobs for FakeBlobs {
    fn plugin(&self) -> Option<&str> {
        Some(INSTANCE)
    }

    fn stage(
        &self,
        key: &str,
        destination: &Path,
        limits: &StageLimits,
        budget: &StageBudget,
    ) -> Result<u64, ScriptReadError> {
        validate_blob_key(key)
            .map_err(|message| ScriptReadError::new("invalid_arguments", message))?;
        let bytes = self
            .bridge
            .blobs
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .ok_or_else(|| {
                ScriptReadError::new(
                    "not_found",
                    format!("there is no stored blob named `{key}`"),
                )
            })?;
        budget.spend(limits, bytes.len() as u64)?;
        std::fs::write(destination, &bytes)
            .map_err(|error| ScriptReadError::new("staging_failed", error.to_string()))?;
        Ok(bytes.len() as u64)
    }
}

impl FakeBridge {
    fn storage_snapshot(&self) -> HashMap<String, serde_json::Value> {
        self.storage.lock().unwrap().clone()
    }

    fn blobs_snapshot(&self) -> HashMap<String, Vec<u8>> {
        self.blobs.lock().unwrap().clone()
    }
}

impl HostBridge for FakeBridge {
    fn call(&self, _plugin_id: &PluginId, request: HostRequest) -> HostResponse {
        let id = request.request_id;
        let ok = |value: serde_json::Value| HostResponse::Ok {
            request_id: id,
            value,
        };
        match request.method.as_str() {
            "storage.get" => {
                let key = request.args["key"].as_str().unwrap_or_default();
                let value = self
                    .storage
                    .lock()
                    .unwrap()
                    .get(key)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ok(value)
            }
            "storage.set" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let value = request.args["value"].clone();
                self.storage.lock().unwrap().insert(key, value);
                ok(json!({ "stored": true }))
            }
            "storage.remove" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let removed = self.storage.lock().unwrap().remove(&key).is_some();
                ok(json!({ "removed": removed }))
            }
            "storage.putBytes" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                if let Err(message) = validate_blob_key(&key) {
                    return HostResponse::Error {
                        request_id: id,
                        error: PluginError::invalid_arguments(message),
                    };
                }
                let Some(values) = request.args["bytes"].as_array() else {
                    return HostResponse::Error {
                        request_id: id,
                        error: PluginError::invalid_arguments("`bytes` must be an array"),
                    };
                };
                let bytes: Vec<u8> = values
                    .iter()
                    .map(|value| value.as_u64().unwrap_or(0) as u8)
                    .collect();
                self.blobs.lock().unwrap().insert(key, bytes.clone());
                ok(json!({ "written": bytes.len() }))
            }
            "storage.getBytes" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let value = self.blobs.lock().unwrap().get(&key).cloned();
                ok(match value {
                    Some(bytes) => json!({ "bytes": bytes }),
                    None => serde_json::Value::Null,
                })
            }
            "storage.deleteBytes" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let deleted = self.blobs.lock().unwrap().remove(&key).is_some();
                ok(json!({ "deleted": deleted }))
            }
            _ => ok(serde_json::Value::Null),
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

/// A merged view over a real folder, so a view mount can stage its files.
struct ViewReads {
    root: PathBuf,
}

impl GameReads for ViewReads {
    fn instance(&self) -> Option<&str> {
        Some(INSTANCE)
    }

    fn read_bytes(&self, _path: &str, _offset: u64, _len: u64) -> Result<Vec<u8>, ScriptReadError> {
        Err(ScriptReadError::new("unsupported", "not used by this test"))
    }

    fn read_lines(
        &self,
        _path: &str,
        _from_line: u64,
        _max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        Err(ScriptReadError::new("unsupported", "not used by this test"))
    }

    fn list(&self, _path: &str) -> Result<serde_json::Value, ScriptReadError> {
        Err(ScriptReadError::new("unsupported", "not used by this test"))
    }

    fn stage_view(
        &self,
        folder: &str,
        patterns: &[String],
        destination: &Path,
        limits: &StageLimits,
        budget: &StageBudget,
    ) -> Result<u64, ScriptReadError> {
        let mut base = folder
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(self.root.clone(), |path, part| path.join(part));
        if !base.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&self.root) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(folder)
                    {
                        base = entry.path();
                    }
                }
            }
        }
        if !base.is_dir() {
            base = self.root.clone();
        }
        let mut staged = 0;
        let entries = std::fs::read_dir(&base)
            .map_err(|error| ScriptReadError::new("not_found", error.to_string()))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !patterns.is_empty() && !path_matches_any(patterns, &name) {
                continue;
            }
            if !entry.path().is_file() {
                continue;
            }
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            budget.spend(limits, bytes)?;
            let source = entry.path();
            let target = destination.join(&name);
            if std::fs::hard_link(&source, &target).is_err() {
                std::fs::copy(&source, &target)
                    .map_err(|error| ScriptReadError::new("staging_failed", error.to_string()))?;
                if let Ok(metadata) = std::fs::metadata(&source) {
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&target) {
                            let _ = file.set_modified(modified);
                        }
                    }
                }
            }
            staged += 1;
        }
        Ok(staged)
    }
}

fn subrecord(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(kind);
    out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    out.extend_from_slice(data);
}

fn zstring(text: &str) -> Vec<u8> {
    let mut out = text.as_bytes().to_vec();
    out.push(0);
    out
}

fn editor_id(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    subrecord(&mut out, b"EDID", &zstring(text));
    out
}

fn record(kind: &[u8; 4], flags: u32, form_id: u32, fields: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(kind);
    out.extend_from_slice(&u32::try_from(fields.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&form_id.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(fields);
    out
}

fn group(label: &[u8; 4], group_type: i32, children: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"GRUP");
    out.extend_from_slice(&u32::try_from(24 + children.len()).unwrap().to_le_bytes());
    out.extend_from_slice(label);
    out.extend_from_slice(&group_type.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(children);
    out
}

fn plugin(masters: &[&str], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut fields = Vec::new();
    subrecord(&mut fields, b"HEDR", &[0u8; 12]);
    for master in masters {
        subrecord(&mut fields, b"MAST", &zstring(master));
        subrecord(&mut fields, b"DATA", &[0u8; 8]);
    }
    let mut out = record(b"TES4", flags, 0, &fields);
    out.extend_from_slice(body);
    out
}

fn gmst(form_id: u32, value: i32, edid: &str) -> Vec<u8> {
    let mut fields = editor_id(edid);
    subrecord(&mut fields, b"DATA", &value.to_le_bytes());
    record(b"GMST", 0, form_id, &fields)
}

/// A zlib stream of stored (uncompressed) deflate blocks, so the test needs no
/// compression crate.
fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < data.len() {
        let length = (data.len() - offset).min(65_535);
        let last = offset + length == data.len();
        out.push(if last { 0x01 } else { 0x00 });
        out.extend_from_slice(&u16::try_from(length).unwrap().to_le_bytes());
        out.extend_from_slice(&(!u16::try_from(length).unwrap()).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + length]);
        offset += length;
    }
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn compressed_gmst(form_id: u32, value: i32, edid: &str) -> Vec<u8> {
    let mut fields = editor_id(edid);
    subrecord(&mut fields, b"DATA", &value.to_le_bytes());
    let mut data = u32::try_from(fields.len()).unwrap().to_le_bytes().to_vec();
    data.extend(zlib_store(&fields));
    record(b"GMST", COMPRESSED, form_id, &data)
}

fn engine() -> (tempfile::TempDir, QuickJsHost, PluginId, Arc<FakeBridge>) {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/main.js"),
    )
    .expect("the base plugin's script is in the repository");
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/official/creation-engine/wasm/plugin-records.wasm");
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let dir = tempfile::Builder::new().tempdir_in(&scratch).unwrap();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    std::fs::create_dir_all(dir.path().join("wasm")).unwrap();
    std::fs::copy(module, dir.path().join("wasm/plugin-records.wasm"))
        .expect("the module is in the repository");
    std::fs::create_dir_all(dir.path().join("crash")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/crash/addrlib-names.js"),
        dir.path().join("crash/addrlib-names.js"),
    )
    .expect("the name table is in the repository");

    let id = PluginId::parse("agora.creation-engine").unwrap();
    let host = QuickJsHost::new();
    let bridge = Arc::new(FakeBridge::default());
    host.activate(
        ActivationRequest {
            plugin_id: id.clone(),
            package_root: dir.path().to_path_buf(),
            data_root: dir.path().to_path_buf(),
            entrypoint: "main.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: json!({}),
        },
        bridge.clone(),
    )
    .expect("the base plugin activates");
    (dir, host, id, bridge)
}

fn data_folder(files: &[(&str, Vec<u8>)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    std::fs::create_dir_all(&data).unwrap();
    for (name, bytes) in files {
        std::fs::write(data.join(name), bytes).unwrap();
    }
    dir
}

fn args(names: &[&str]) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "active": true,
                "locked": false,
                "implicit": false,
                "present": true,
            })
        })
        .collect();
    json!({
        "context": { "instance": INSTANCE },
        "listId": "plugins",
        "options": {},
        "entries": entries,
        "active": names,
        "locked": [],
        "implicit": [],
    })
}

fn invoke(
    host: &QuickJsHost,
    id: &PluginId,
    names: &[&str],
    data: &Path,
    bridge: &FakeBridge,
) -> serde_json::Value {
    host.invoke_with_scopes(
        id,
        "overlaps",
        args(names),
        Duration::from_secs(120),
        Some(Arc::new(ViewReads {
            root: data.to_path_buf(),
        })),
        Some(Arc::new(FakeBlobs {
            bridge: bridge.clone(),
        })),
    )
    .unwrap_or_else(|error| panic!("overlaps failed: {}", error.message))
}

fn items(reply: &serde_json::Value, key: &str) -> Vec<(String, String)> {
    let keys = reply["keys"].as_array().expect("keys");
    let entry = keys
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("no key {key} in {reply}"));
    entry["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["name"].as_str().unwrap().to_string(),
                item["digest"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn label(reply: &serde_json::Value, key: &str) -> String {
    reply["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap()["label"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn two_plugins_that_change_one_master_record_differently_report_a_conflict() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "First.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "Second.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 3, "JumpSetting")),
            ),
        ),
    ]);
    let reply = invoke(
        &host,
        &id,
        &["Master.esm", "First.esp", "Second.esp"],
        data.path(),
        &bridge,
    );
    assert!(reply["notes"].as_array().unwrap().is_empty(), "{reply}");
    let found = items(&reply, "master.esm:000abc");
    assert_eq!(
        found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Master.esm", "First.esp", "Second.esp"]
    );
    assert_ne!(found[0].1, found[1].1);
    assert_ne!(found[1].1, found[2].1);
    assert_eq!(label(&reply, "master.esm:000abc"), "GMST JumpSetting");
    host.deactivate(&id).ok();
}

#[test]
fn the_same_change_and_an_identical_to_master_record_agree_and_are_not_reported() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "Identical.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "Same.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "SameToo.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
    ]);
    let reply = invoke(
        &host,
        &id,
        &["Master.esm", "Identical.esp", "Same.esp", "SameToo.esp"],
        data.path(),
        &bridge,
    );
    assert!(
        reply["keys"].as_array().unwrap().is_empty(),
        "agreeing records are never a conflict: {reply}"
    );
    assert!(reply["notes"].as_array().unwrap().is_empty(), "{reply}");
    host.deactivate(&id).ok();
}

#[test]
fn a_light_plugin_and_a_compressed_record_are_read() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "Light.esl",
            plugin(
                &["Master.esm"],
                LIGHT,
                &group(b"GMST", 0, &gmst(0x0100_0def, 5, "LightSetting")),
            ),
        ),
        (
            "Plain.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "Packed.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &compressed_gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "Third.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 3, "JumpSetting")),
            ),
        ),
    ]);
    let reply = invoke(
        &host,
        &id,
        &[
            "Master.esm",
            "Light.esl",
            "Plain.esp",
            "Packed.esp",
            "Third.esp",
        ],
        data.path(),
        &bridge,
    );
    let found = items(&reply, "master.esm:000abc");
    let plain = found.iter().find(|(name, _)| name == "Plain.esp").unwrap();
    let packed = found.iter().find(|(name, _)| name == "Packed.esp").unwrap();
    let third = found.iter().find(|(name, _)| name == "Third.esp").unwrap();
    assert_eq!(
        plain.1, packed.1,
        "a compressed record digests like its plain form"
    );
    assert_ne!(plain.1, third.1);
    host.deactivate(&id).ok();
}

#[test]
fn a_record_defined_by_a_light_plugin_is_shared_with_its_referrers() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "Light.esl",
            plugin(
                &["Master.esm"],
                LIGHT,
                &group(b"GMST", 0, &gmst(0x0100_0def, 5, "LightSetting")),
            ),
        ),
        (
            "Other.esp",
            plugin(
                &["Light.esl"],
                0,
                &group(b"GMST", 0, &gmst(0x000def, 6, "LightSetting")),
            ),
        ),
        (
            "Third.esp",
            plugin(
                &["Light.esl"],
                0,
                &group(b"GMST", 0, &gmst(0x000def, 7, "LightSetting")),
            ),
        ),
    ]);
    let reply = invoke(
        &host,
        &id,
        &["Master.esm", "Light.esl", "Other.esp", "Third.esp"],
        data.path(),
        &bridge,
    );
    let found = items(&reply, "light.esl:000def");
    assert_eq!(
        found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Light.esl", "Other.esp", "Third.esp"]
    );
    assert_ne!(found[0].1, found[1].1);
    assert_ne!(found[1].1, found[2].1);
    host.deactivate(&id).ok();
}

#[test]
fn a_nested_cell_reference_is_compared() {
    let (_dir, host, id, bridge) = engine();
    fn nested_reference(value: &[u8]) -> Vec<u8> {
        let mut fields = Vec::new();
        subrecord(&mut fields, b"NAME", value);
        let reference = record(b"REFR", 0, 0x000def, &fields);
        group(
            b"CELL",
            0,
            &group(&[0, 0, 0, 0], 8, &group(b"REFR", 0, &reference)),
        )
    }
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(&[], 0, &nested_reference(&[1, 2, 3, 4])),
        ),
        (
            "Nested.esp",
            plugin(&["Master.esm"], 0, &nested_reference(&[1, 2, 3, 5])),
        ),
        (
            "Third.esp",
            plugin(&["Master.esm"], 0, &nested_reference(&[1, 2, 3, 6])),
        ),
    ]);
    let reply = invoke(
        &host,
        &id,
        &["Master.esm", "Nested.esp", "Third.esp"],
        data.path(),
        &bridge,
    );
    let found = items(&reply, "master.esm:000def");
    assert_eq!(
        found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Master.esm", "Nested.esp", "Third.esp"]
    );
    host.deactivate(&id).ok();
}

#[test]
fn an_unreadable_file_is_a_note_and_the_others_are_still_checked() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "First.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "Third.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 3, "JumpSetting")),
            ),
        ),
        ("Broken.esp", b"this is not a plugin".to_vec()),
    ]);
    let reply = invoke(
        &host,
        &id,
        &["Master.esm", "First.esp", "Third.esp", "Broken.esp"],
        data.path(),
        &bridge,
    );
    let notes = reply["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|note| note["item"] == "Broken.esp"
            && note["text"].as_str().unwrap().contains("could not be read")),
        "{reply}"
    );
    let found = items(&reply, "master.esm:000abc");
    assert_eq!(found.len(), 3, "the readable files were still compared");
    host.deactivate(&id).ok();
}

#[test]
fn a_second_call_reuses_the_saved_index_and_a_changed_file_is_reindexed() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[
        (
            "Master.esm",
            plugin(
                &[],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
            ),
        ),
        (
            "First.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
            ),
        ),
        (
            "Second.esp",
            plugin(
                &["Master.esm"],
                0,
                &group(b"GMST", 0, &gmst(0x000abc, 3, "JumpSetting")),
            ),
        ),
    ]);
    let names = ["Master.esm", "First.esp", "Second.esp"];
    let first = invoke(&host, &id, &names, data.path(), &bridge);
    let before = bridge.blobs_snapshot();
    let manifest = bridge.storage_snapshot();
    assert!(
        manifest.contains_key("overlaps.index"),
        "the index manifest was saved"
    );
    assert!(!before.is_empty(), "the indexes were saved as blobs");

    let second = invoke(&host, &id, &names, data.path(), &bridge);
    assert_eq!(
        items(&first, "master.esm:000abc"),
        items(&second, "master.esm:000abc")
    );
    assert_eq!(
        before,
        bridge.blobs_snapshot(),
        "a warm call rewrote nothing"
    );

    std::fs::write(
        data.path().join("Data/First.esp"),
        plugin(
            &["Master.esm"],
            0,
            &group(b"GMST", 0, &gmst(0x000abc, 7, "JumpSettingChanged")),
        ),
    )
    .unwrap();
    let third = invoke(&host, &id, &names, data.path(), &bridge);
    let changed = items(&third, "master.esm:000abc");
    let first_before = items(&first, "master.esm:000abc");
    assert_ne!(
        first_before[1].1, changed[1].1,
        "the changed file was read again"
    );
    host.deactivate(&id).ok();
}

#[test]
fn fewer_than_two_plugins_have_nothing_to_compare() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder(&[(
        "Master.esm",
        plugin(
            &[],
            0,
            &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
        ),
    )]);
    let reply = invoke(&host, &id, &["Master.esm"], data.path(), &bridge);
    assert_eq!(reply["keys"], json!([]));
    assert_eq!(reply["notes"], json!([]));
    host.deactivate(&id).ok();
}

/// The measurement from the task, kept as an ignored proof: point
/// `AGORA_OVERLAPS_DATA` at a real Data folder and run it with `--ignored`.
#[test]
#[ignore = "needs AGORA_OVERLAPS_DATA pointing at a read-only real Data folder"]
fn real_data_cold_and_warm() {
    let root = std::env::var("AGORA_OVERLAPS_DATA")
        .expect("set AGORA_OVERLAPS_DATA to the real Data folder");
    let data = PathBuf::from(root);
    let mut names: Vec<String> = std::fs::read_dir(&data)
        .expect("the Data folder is readable")
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            name.ends_with(".esm") || name.ends_with(".esp") || name.ends_with(".esl")
        })
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    if let Ok(only) = std::env::var("AGORA_OVERLAPS_PLUGINS") {
        names = only
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
    }
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();

    let (_dir, host, id, bridge) = engine();
    let start = std::time::Instant::now();
    let cold = host
        .invoke_with_scopes(
            &id,
            "overlaps",
            args(&name_refs),
            Duration::from_secs(900),
            Some(Arc::new(ViewReads { root: data.clone() })),
            Some(Arc::new(FakeBlobs {
                bridge: (*bridge).clone(),
            })),
        )
        .unwrap_or_else(|error| panic!("cold overlaps failed: {}", error.message));
    let cold = cold.as_object().cloned().unwrap_or_default();
    let cold_ms = start.elapsed().as_millis();

    let blobs = bridge.blobs_snapshot();
    let index_bytes: usize = blobs.values().map(Vec::len).sum();

    let start = std::time::Instant::now();
    let warm = host
        .invoke_with_scopes(
            &id,
            "overlaps",
            args(&name_refs),
            Duration::from_secs(900),
            Some(Arc::new(ViewReads { root: data.clone() })),
            Some(Arc::new(FakeBlobs {
                bridge: (*bridge).clone(),
            })),
        )
        .unwrap_or_else(|error| panic!("warm overlaps failed: {}", error.message));
    let warm_ms = start.elapsed().as_millis();
    let warm = warm.as_object().cloned().unwrap_or_default();

    let key = serde_json::Value::Object(warm.clone())
        .get("keys")
        .and_then(|keys| keys.as_array())
        .and_then(|keys| {
            keys.iter()
                .find(|entry| entry["key"] == "skyrim.esm:0abef6")
        })
        .cloned();
    let found: Vec<String> = key
        .as_ref()
        .and_then(|entry| entry["items"].as_array())
        .map(|items| {
            items
                .iter()
                .map(|item| item["name"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        found.iter().any(|name| name == "AgoraTest - Moon Jump.esp"),
        "Moon Jump must share skyrim.esm:0abef6: {found:?}"
    );
    assert!(
        found
            .iter()
            .any(|name| name == "AgoraTest - Heavy Boots.esp"),
        "Heavy Boots must share skyrim.esm:0abef6: {found:?}"
    );

    let cold_keys = cold["keys"].as_array().map(Vec::len).unwrap_or(0);
    let warm_keys = warm["keys"].as_array().map(Vec::len).unwrap_or(0);
    eprintln!(
        "cold {cold_ms} ms, warm {warm_ms} ms, index {} bytes in {} blobs, \
         keys {cold_keys}/{warm_keys}, skyrim.esm:0abef6 {:?}",
        index_bytes,
        blobs.len(),
        found
    );
    host.deactivate(&id).ok();
}
