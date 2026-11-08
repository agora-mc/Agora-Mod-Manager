//! The Creation Engine plugin's sorter hooks (MASTER_SPEC §30.13), run as the real script with a
//! fake network and the real WebAssembly module.
//!
//! These are the behaviour tests `agora-core`'s `loot_masterlist.rs` held before libloot left
//! core: a bad blob hash refuses the download and keeps the saved copy, a failed download keeps
//! it too, and a masterlist note reaches the sorter's reply unchanged.

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
use base64::Engine;

const INSTANCE: &str = "test-instance";
const MASTERLIST: &str = "plugins:\n  - name: 'B.esp'\n    after:\n      - 'A.esp'\n    msg:\n      - type: warn\n        content: 'B needs A first.'\n";

#[derive(Clone, Default)]
struct FakeBridge {
    storage: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    net: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    failing: Arc<Mutex<bool>>,
}

/// The plugin's blob store, as a wasm `stored` mount sees it.
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
    fn serve(&self, url: &str, body: serde_json::Value) {
        self.net.lock().unwrap().insert(url.to_string(), body);
    }

    fn fail_network(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

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
                ok(serde_json::json!({ "stored": true }))
            }
            "storage.remove" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let removed = self.storage.lock().unwrap().remove(&key).is_some();
                ok(serde_json::json!({ "removed": removed }))
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
                ok(serde_json::json!({ "written": bytes.len() }))
            }
            "storage.getBytes" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let value = self.blobs.lock().unwrap().get(&key).cloned();
                ok(match value {
                    Some(bytes) => serde_json::json!({ "bytes": bytes }),
                    None => serde_json::Value::Null,
                })
            }
            "storage.deleteBytes" => {
                let key = request.args["key"].as_str().unwrap_or_default().to_string();
                let deleted = self.blobs.lock().unwrap().remove(&key).is_some();
                ok(serde_json::json!({ "deleted": deleted }))
            }
            "net.fetchJson" => {
                if *self.failing.lock().unwrap() {
                    return ok(serde_json::json!({ "status": 500, "body": {} }));
                }
                let url = request.args["url"].as_str().unwrap_or_default();
                let body = self
                    .net
                    .lock()
                    .unwrap()
                    .get(url)
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                ok(serde_json::json!({ "status": 200, "body": body }))
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
        // The real-data fixture keeps the plugins in the folder itself, named `data`; a merged
        // view calls it `Data`. Match a differently-cased child, then fall back to the root, so
        // the ignored proof can use `.spike/data` as it is.
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
            std::fs::copy(entry.path(), destination.join(&name))
                .map_err(|error| ScriptReadError::new("staging_failed", error.to_string()))?;
            staged += 1;
        }
        Ok(staged)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn git_blob_sha1(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn serve_masterlist(bridge: &FakeBridge, repo: &str, branch: &str, commit: &str, yaml: &str) {
    bridge.serve(
        &format!("https://api.github.com/repos/{repo}/commits/{branch}"),
        serde_json::json!({ "sha": commit }),
    );
    bridge.serve(
        &format!("https://api.github.com/repos/{repo}/contents/masterlist.yaml?ref={commit}"),
        serde_json::json!({
            "sha": git_blob_sha1(yaml.as_bytes()),
            "encoding": "base64",
            "content": base64::engine::general_purpose::STANDARD.encode(yaml),
        }),
    );
}

fn engine() -> (tempfile::TempDir, QuickJsHost, PluginId, Arc<FakeBridge>) {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/official/creation-engine/main.js"),
    )
    .expect("the base plugin's script is in the repository");
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/official/creation-engine/wasm/loot-sort.wasm");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.js"), source).unwrap();
    std::fs::create_dir_all(dir.path().join("wasm")).unwrap();
    std::fs::copy(module, dir.path().join("wasm/loot-sort.wasm"))
        .expect("the module is in the repository");

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
            settings: serde_json::json!({}),
        },
        bridge.clone(),
    )
    .expect("the base plugin activates");
    (dir, host, id, bridge)
}

fn options() -> serde_json::Value {
    serde_json::json!({
        "masterlist_repo": "loot/test",
        "branch": "v1",
        "game_type": "SkyrimSE",
    })
}

fn update_args() -> serde_json::Value {
    serde_json::json!({
        "context": { "instance": INSTANCE },
        "listId": "plugins",
        "options": options(),
        "entries": [],
        "active": [],
        "locked": [],
        "implicit": [],
        "update": true,
    })
}

fn sort_args() -> serde_json::Value {
    serde_json::json!({
        "context": { "instance": INSTANCE },
        "listId": "plugins",
        "options": options(),
        "entries": [
            { "name": "B.esp", "active": true, "locked": false, "implicit": false, "present": true },
            { "name": "A.esp", "active": true, "locked": false, "implicit": false, "present": true },
        ],
        "active": ["A.esp", "B.esp"],
        "locked": [],
        "implicit": [],
    })
}

fn plugin_bytes(masters: &[&str]) -> Vec<u8> {
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
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&body);
    out
}

fn data_folder() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("Data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("A.esp"), plugin_bytes(&[])).unwrap();
    std::fs::write(data.join("B.esp"), plugin_bytes(&[])).unwrap();
    dir
}

fn invoke(
    host: &QuickJsHost,
    id: &PluginId,
    export: &str,
    args: serde_json::Value,
    data: &Path,
    bridge: &FakeBridge,
) -> serde_json::Value {
    host.invoke_with_scopes(
        id,
        export,
        args,
        Duration::from_secs(120),
        Some(Arc::new(ViewReads {
            root: data.to_path_buf(),
        })),
        Some(Arc::new(FakeBlobs {
            bridge: bridge.clone(),
        })),
    )
    .unwrap_or_else(|error| panic!("{export} failed: {}", error.message))
}

#[test]
fn an_update_sorts_and_maps_a_note_on_the_next_sort() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder();
    serve_masterlist(&bridge, "loot/test", "v1", "deadbeef", MASTERLIST);

    let status = invoke(
        &host,
        &id,
        "lootUpdate",
        update_args(),
        data.path(),
        &bridge,
    );
    assert!(status["fetchedAt"].is_string(), "{status}");
    assert!(
        status["source"]
            .as_str()
            .is_some_and(|source| source.contains("deadbeef")),
        "{status}"
    );
    assert_eq!(status["dataVersion"], serde_json::json!("deadbeef"));

    let reply = invoke(&host, &id, "lootSort", sort_args(), data.path(), &bridge);
    assert_eq!(reply["order"], serde_json::json!(["A.esp", "B.esp"]));
    let notes = reply["notes"].as_array().expect("notes");
    assert!(
        notes.iter().any(|note| note["item"] == "B.esp"
            && note["level"] == "warn"
            && note["text"] == "B needs A first."),
        "{reply}"
    );
    host.deactivate(&id).ok();
}

#[test]
fn a_wrong_blob_hash_is_refused_and_the_saved_copy_stays() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder();
    serve_masterlist(&bridge, "loot/test", "v1", "deadbeef", MASTERLIST);
    invoke(
        &host,
        &id,
        "lootUpdate",
        update_args(),
        data.path(),
        &bridge,
    );
    let before = bridge.storage_snapshot();
    let before_blobs = bridge.blobs_snapshot();

    bridge.serve(
        "https://api.github.com/repos/loot/test/contents/masterlist.yaml?ref=deadbeef",
        serde_json::json!({
            "sha": "0000000000000000000000000000000000000000",
            "encoding": "base64",
            "content": base64::engine::general_purpose::STANDARD.encode("plugins: []\n"),
        }),
    );
    let error = host
        .invoke_with_game_reads(
            &id,
            "lootUpdate",
            update_args(),
            Duration::from_secs(120),
            Arc::new(ViewReads {
                root: data.path().to_path_buf(),
            }),
        )
        .expect_err("a wrong hash must refuse the update");
    assert!(
        error.message.contains("do not match the hash"),
        "{}",
        error.message
    );
    assert_eq!(
        bridge.storage_snapshot(),
        before,
        "the saved masterlist was left alone"
    );
    assert_eq!(
        bridge.blobs_snapshot(),
        before_blobs,
        "the saved blobs were left alone"
    );
    host.deactivate(&id).ok();
}

#[test]
fn a_failed_download_keeps_the_saved_copy() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder();
    serve_masterlist(&bridge, "loot/test", "v1", "deadbeef", MASTERLIST);
    invoke(
        &host,
        &id,
        "lootUpdate",
        update_args(),
        data.path(),
        &bridge,
    );
    let before = bridge.storage_snapshot();
    let before_blobs = bridge.blobs_snapshot();

    bridge.fail_network(true);
    let error = host
        .invoke_with_game_reads(
            &id,
            "lootUpdate",
            update_args(),
            Duration::from_secs(120),
            Arc::new(ViewReads {
                root: data.path().to_path_buf(),
            }),
        )
        .expect_err("a failed download must be an error");
    assert!(error.message.contains("500"), "{}", error.message);
    assert_eq!(bridge.storage_snapshot(), before);
    assert_eq!(bridge.blobs_snapshot(), before_blobs);
    host.deactivate(&id).ok();
}

#[test]
fn no_saved_masterlist_returns_the_current_order_and_a_note() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder();
    let reply = invoke(&host, &id, "lootSort", sort_args(), data.path(), &bridge);
    assert_eq!(reply["order"], serde_json::json!(["B.esp", "A.esp"]));
    assert_eq!(
        reply["general"],
        serde_json::json!([{ "level": "info", "text": "Update the masterlist first" }])
    );
    host.deactivate(&id).ok();
}

/// Proof on real data, not run by default. Set `AGORA_CE_FIXTURE`, `AGORA_CE_MASTERLIST` and
/// `AGORA_CE_PRELUDE`, then run with `--ignored`. The fixture is git-excluded.
#[test]
#[ignore = "needs AGORA_CE_FIXTURE, AGORA_CE_MASTERLIST and AGORA_CE_PRELUDE"]
fn the_hook_and_the_module_agree_on_real_plugins() {
    let fixture = PathBuf::from(
        std::env::var("AGORA_CE_FIXTURE").expect("AGORA_CE_FIXTURE names the plugin folder"),
    );
    let masterlist = std::fs::read_to_string(
        std::env::var("AGORA_CE_MASTERLIST").expect("AGORA_CE_MASTERLIST names the YAML"),
    )
    .expect("the masterlist reads");
    let prelude = std::fs::read_to_string(
        std::env::var("AGORA_CE_PRELUDE").expect("AGORA_CE_PRELUDE names the YAML"),
    )
    .expect("the prelude reads");

    let mut names: Vec<String> = std::fs::read_dir(&fixture)
        .expect("the fixture folder reads")
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| {
            let lower = name.to_lowercase();
            lower.ends_with(".esm") || lower.ends_with(".esl") || lower.ends_with(".esp")
        })
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    assert!(!names.is_empty(), "the fixture has no plugins");

    let (dir, host, id, bridge) = engine();
    let bundle = serde_json::json!({
        "masterlist": masterlist,
        "prelude": prelude,
        "status": { "fetchedAt": "2026-11-07T00:00:00Z", "source": "fixture" },
    });
    let generation = "fixture";
    let masterlist_key = format!("masterlist.{generation}.yaml");
    let prelude_key = format!("prelude.{generation}.yaml");
    {
        let mut blobs = bridge.blobs.lock().unwrap();
        blobs.insert(
            masterlist_key.clone(),
            bundle["masterlist"].as_str().unwrap().as_bytes().to_vec(),
        );
        blobs.insert(
            prelude_key.clone(),
            bundle["prelude"].as_str().unwrap().as_bytes().to_vec(),
        );
        let mut storage = bridge.storage.lock().unwrap();
        storage.insert(
            "sorter.status".to_string(),
            serde_json::json!({
                "fetchedAt": "2026-11-07T00:00:00Z",
                "source": "fixture",
                "dataVersion": generation,
                "masterlistKey": masterlist_key,
                "preludeKey": prelude_key,
            }),
        );
    }

    let entries: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name, "active": true, "locked": false, "implicit": false, "present": true
            })
        })
        .collect();
    let args = serde_json::json!({
        "context": { "instance": INSTANCE },
        "listId": "plugins",
        "options": { "game_type": "SkyrimSE" },
        "entries": entries,
        "active": names,
        "locked": [],
        "implicit": [],
    });

    let started = std::time::Instant::now();
    let hook = invoke(&host, &id, "lootSort", args, &fixture, &bridge);
    let hook_time = started.elapsed();

    // The same request straight to the module, with no script or storage in between.
    let request = serde_json::json!({
        "game_type": "SkyrimSE",
        "data": "/data",
        "load_order": names,
        "active": names,
        "masterlist": "masterlist.yaml",
        "prelude": null,
        "masterlist_yaml": bundle["masterlist"],
        "prelude_yaml": bundle["prelude"],
    });
    let options = serde_json::json!({
        "stdin": serde_json::to_string(&request).unwrap(),
        "mounts": [{ "guest": "/data", "view": { "folder": "Data", "patterns": ["*.esm", "*.esl", "*.esp"] } }],
        "limits": { "timeoutMs": 90000 },
    });
    // Run the module through a one-line script, with no masterlist storage or sorter mapping in
    // between. Its stdout is the module's own answer.
    let direct_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        direct_dir.path().join("main.js"),
        "import { wasm } from \"agora\";\nexport function direct(args) {\n  return wasm.run(args.module, args.options);\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(direct_dir.path().join("wasm")).unwrap();
    std::fs::copy(
        dir.path().join("wasm/loot-sort.wasm"),
        direct_dir.path().join("wasm/loot-sort.wasm"),
    )
    .unwrap();
    let direct_id = PluginId::parse("test.direct-module").unwrap();
    let direct_host = QuickJsHost::new();
    direct_host
        .activate(
            ActivationRequest {
                plugin_id: direct_id.clone(),
                package_root: direct_dir.path().to_path_buf(),
                data_root: direct_dir.path().to_path_buf(),
                entrypoint: "main.js".into(),
                memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
                stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
                api_version: agora_plugin_api::host_api_version_string(),
                settings: serde_json::json!({}),
            },
            Arc::new(FakeBridge::default()),
        )
        .unwrap();
    let result = direct_host
        .invoke_with_game_reads(
            &direct_id,
            "direct",
            serde_json::json!({ "module": "wasm/loot-sort.wasm", "options": options }),
            Duration::from_secs(120),
            Arc::new(ViewReads {
                root: fixture.clone(),
            }),
        )
        .expect("the module runs directly");
    direct_host.deactivate(&direct_id).ok();
    let direct: serde_json::Value =
        serde_json::from_str(result["stdoutText"].as_str().expect("stdout text")).unwrap();

    assert_eq!(
        hook["order"], direct["order"],
        "the hook and the module agree"
    );
    eprintln!(
        "sorted {} plugins through the hook in {hook_time:?}; the module alone gave the same order",
        names.len()
    );
    host.deactivate(&id).ok();
}

#[test]
fn status_reads_the_saved_copy_without_downloading() {
    let (_dir, host, id, bridge) = engine();
    let data = data_folder();
    serve_masterlist(&bridge, "loot/test", "v1", "deadbeef", MASTERLIST);
    invoke(
        &host,
        &id,
        "lootUpdate",
        update_args(),
        data.path(),
        &bridge,
    );
    bridge.fail_network(true);

    let mut args = update_args();
    args["update"] = serde_json::json!(false);
    let status = invoke(&host, &id, "lootUpdate", args, data.path(), &bridge);
    assert!(status["fetchedAt"].is_string(), "{status}");
    host.deactivate(&id).ok();
}
