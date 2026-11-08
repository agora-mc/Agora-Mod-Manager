//! The WebAssembly tier (MASTER_SPEC §30.13), exercised against real modules.
//!
//! Each test is one promise the sandbox makes: a module sees what the script
//! handed it, cannot see anything else, and is stopped when it misbehaves.

use agora_plugin_api::host::{
    path_matches_any, validate_blob_key, ActivationRequest, GameReads, HostBridge, ScriptHost,
    ScriptReadError, StageBudget, StageLimits, StoredBlobs,
};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"
import { wasm } from "agora";
export function run(args) {
  return wasm.run(args.module, {
    stdin: args.stdin,
    mounts: args.mounts || [],
    limits: args.limits,
  });
}
"#;

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

/// A stand-in for `agora-core`'s merged view. It stages from a real folder
/// with the same matching rules core uses, and refuses everything else.
struct ViewReads {
    root: PathBuf,
}

impl GameReads for ViewReads {
    fn instance(&self) -> Option<&str> {
        Some("test-instance")
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
        let base = folder
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(self.root.clone(), |path, part| path.join(part));
        let mut files = Vec::new();
        collect(&base, "", &mut files);
        let mut staged = 0;
        for (relative, source) in files {
            if !patterns.is_empty() && !path_matches_any(patterns, &relative) {
                continue;
            }
            let size = std::fs::metadata(&source)
                .map(|meta| meta.len())
                .unwrap_or(0);
            budget.spend(limits, size)?;
            let dest = relative
                .split('/')
                .fold(destination.to_path_buf(), |path, part| path.join(part));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|_| ScriptReadError::new("staging_failed", "mkdir"))?;
            }
            std::fs::copy(&source, &dest)
                .map_err(|_| ScriptReadError::new("staging_failed", "copy"))?;
            staged += 1;
        }
        Ok(staged)
    }
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            collect(&entry.path(), &relative, out);
        } else if meta.is_file() {
            out.push((relative, entry.path()));
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    package: PathBuf,
    data: PathBuf,
    view: PathBuf,
    id: PluginId,
}

fn fixture(modules: &[&str]) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let package = dir.path().join("package");
    let data = dir.path().join("data");
    let view = dir.path().join("view");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&view).unwrap();
    std::fs::write(package.join("main.js"), SCRIPT).unwrap();
    for module in modules {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/wasm-fixtures/wasm")
            .join(module);
        std::fs::copy(source, package.join(module)).expect("fixture module");
    }
    Fixture {
        _dir: dir,
        package,
        data,
        view,
        id: PluginId::parse("acme.wasm-test").unwrap(),
    }
}

fn activate(fixture: &Fixture) -> QuickJsHost {
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: fixture.id.clone(),
            package_root: fixture.package.clone(),
            data_root: fixture.data.clone(),
            entrypoint: "main.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .expect("activation");
    host
}

fn invoke(
    host: &QuickJsHost,
    fixture: &Fixture,
    args: serde_json::Value,
    reads: Option<Arc<ViewReads>>,
) -> agora_plugin_api::error::PluginResult<serde_json::Value> {
    match reads {
        Some(reads) => {
            host.invoke_with_game_reads(&fixture.id, "run", args, Duration::from_secs(30), reads)
        }
        None => host.invoke(&fixture.id, "run", args, Duration::from_secs(30)),
    }
}

fn stage(fixture: &Fixture, name: &str) -> Arc<ViewReads> {
    let data = fixture.view.join("Data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join(name), name).unwrap();
    Arc::new(ViewReads {
        root: fixture.view.clone(),
    })
}

#[test]
fn a_module_echoes_stdin_to_stdout() {
    let fixture = fixture(&["echo.wasm"]);
    let host = activate(&fixture);
    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({ "module": "echo.wasm", "stdin": "hello, module" }),
        None,
    )
    .expect("the module runs");
    assert_eq!(value["exitCode"], 0);
    assert_eq!(value["stdoutText"], "hello, module");
    assert_eq!(value["stderrText"], "echo: done\n");
}

#[test]
fn a_view_mount_sees_only_the_matching_files() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let reads = stage(&fixture, "a.esp");
    std::fs::write(fixture.view.join("Data/b.esm"), "b").unwrap();
    std::fs::write(fixture.view.join("Data/c.esp"), "c").unwrap();
    let host = activate(&fixture);
    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "list /data",
            "mounts": [{ "guest": "/data", "view": { "folder": "Data", "patterns": ["*.esp"] } }],
        }),
        Some(reads),
    )
    .expect("the module runs");
    assert_eq!(value["stdoutText"], "LIST:a.esp,c.esp\n");
}

/// A plugin's blob store, with one blob.
struct OneBlob;

impl StoredBlobs for OneBlob {
    fn plugin(&self) -> Option<&str> {
        Some("acme.wasm-test")
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
        if key != "notes.txt" {
            return Err(ScriptReadError::new(
                "not_found",
                format!("there is no stored blob named `{key}`"),
            ));
        }
        let bytes = b"stored bytes";
        budget.spend(limits, bytes.len() as u64)?;
        std::fs::write(destination, bytes)
            .map_err(|error| ScriptReadError::new("staging_failed", error.to_string()))?;
        Ok(bytes.len() as u64)
    }
}

#[test]
fn a_stored_mount_reaches_the_module() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let host = activate(&fixture);
    let value = host
        .invoke_with_scopes(
            &fixture.id,
            "run",
            serde_json::json!({
                "module": "fs_probe.wasm",
                "stdin": "read /input/notes.txt",
                "mounts": [{ "guest": "/input", "stored": ["notes.txt"] }],
            }),
            Duration::from_secs(30),
            None,
            Some(Arc::new(OneBlob)),
        )
        .expect("the module runs");
    assert_eq!(
        value["stdoutText"],
        "READ:stored bytes
"
    );
}

#[test]
fn a_module_cannot_write_to_a_mount() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let reads = stage(&fixture, "a.esp");
    let host = activate(&fixture);
    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "write /data/new.txt",
            "mounts": [{ "guest": "/data", "view": { "folder": "Data", "patterns": ["*.esp"] } }],
        }),
        Some(reads),
    )
    .expect("the module runs");
    assert_eq!(value["stdoutText"], "WRITE_FAILED\n");
    assert!(!fixture.view.join("Data/new.txt").exists());
}

#[test]
fn a_module_cannot_see_the_real_instance_folder() {
    let fixture = fixture(&["fs_probe.wasm", "env_probe.wasm"]);
    let reads = stage(&fixture, "a.esp");
    let real = fixture.view.join("Data/a.esp");
    let real_text = real.to_string_lossy().to_string();
    let host = activate(&fixture);

    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": format!("open {real_text}"),
            "mounts": [{ "guest": "/data", "view": { "folder": "Data", "patterns": ["*.esp"] } }],
        }),
        Some(reads.clone()),
    )
    .expect("the module runs");
    assert_eq!(value["stdoutText"], "OPEN_FAILED\n");

    let probe = invoke(
        &host,
        &fixture,
        serde_json::json!({ "module": "env_probe.wasm" }),
        Some(reads),
    )
    .expect("the probe runs");
    let output = probe["stdoutText"].as_str().unwrap();
    assert!(output.contains("ENV:0\n"), "{output}");
    assert!(output.contains("ARGS:1\n"), "{output}");
    assert!(!output.contains(&real_text), "{output}");
}

#[test]
fn a_module_path_outside_the_plugin_is_refused() {
    let fixture = fixture(&["echo.wasm"]);
    let host = activate(&fixture);
    for module in ["../outside.wasm", "C:/outside.wasm", "/outside.wasm"] {
        let error = invoke(
            &host,
            &fixture,
            serde_json::json!({ "module": module }),
            None,
        )
        .expect_err("a module outside the package is refused");
        assert!(
            error.message.contains(module),
            "the error names the path: {}",
            error.message
        );
        assert!(
            error.message.contains("outside") || error.message.contains("absolute"),
            "the error says why: {}",
            error.message
        );
    }
}

#[test]
fn a_view_mount_without_game_read_is_refused() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let host = activate(&fixture);
    let error = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "list /data",
            "mounts": [{ "guest": "/data", "view": { "folder": "Data" } }],
        }),
        None,
    )
    .expect_err("a view mount needs game:read");
    assert!(
        error.message.contains("game:read"),
        "the error says what is missing: {}",
        error.message
    );
}

#[test]
fn a_runaway_module_is_stopped_at_the_deadline() {
    let fixture = fixture(&["runaway.wasm"]);
    let host = activate(&fixture);
    let started = Instant::now();
    let error = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "runaway.wasm",
            "limits": { "timeoutMs": 200 },
        }),
        None,
    )
    .expect_err("a runaway module is stopped");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "stopped quickly: {elapsed:?}"
    );
    assert!(
        error.message.to_lowercase().contains("time"),
        "the error says it timed out: {}",
        error.message
    );
}

#[test]
fn a_module_over_its_memory_cap_fails_cleanly() {
    let fixture = fixture(&["memory_hog.wasm"]);
    let host = activate(&fixture);
    let outcome = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "memory_hog.wasm",
            "limits": { "memoryBytes": 32 * 1024 * 1024 },
        }),
        None,
    );
    match outcome {
        Ok(value) => assert_ne!(value["exitCode"], 0, "the module did not finish normally"),
        Err(error) => assert!(
            !error.message.is_empty(),
            "the failure is reported, not swallowed"
        ),
    }
}

#[test]
fn the_staging_folder_is_removed_after_a_trap() {
    let fixture = fixture(&["runaway.wasm"]);
    let host = activate(&fixture);
    invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "runaway.wasm",
            "limits": { "timeoutMs": 200 },
        }),
        None,
    )
    .expect_err("the module is stopped");
    let staging = fixture.data.join("staging");
    let leftovers = std::fs::read_dir(&staging)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "the staging folder was deleted");
}

#[test]
fn a_script_cannot_raise_the_limits() {
    let fixture = fixture(&["echo.wasm"]);
    let host = activate(&fixture);
    for limits in [
        serde_json::json!({ "memoryBytes": 2_u64 * 1024 * 1024 * 1024 }),
        serde_json::json!({ "timeoutMs": 999_999 }),
        serde_json::json!({ "mountBytes": 5_u64 * 1024 * 1024 * 1024 }),
        serde_json::json!({ "mountFiles": 100_001 }),
    ] {
        let error = invoke(
            &host,
            &fixture,
            serde_json::json!({ "module": "echo.wasm", "limits": limits }),
            None,
        )
        .expect_err("raising a limit is refused");
        assert!(
            error.message.contains("not raise"),
            "the error explains the limit: {}",
            error.message
        );
    }
}

#[test]
fn environment_variables_do_not_reach_the_module() {
    let fixture = fixture(&["env_probe.wasm"]);
    let host = activate(&fixture);
    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({ "module": "env_probe.wasm" }),
        None,
    )
    .expect("the probe runs");
    let output = value["stdoutText"].as_str().unwrap();
    assert!(output.contains("ENV:0\n"), "{output}");
    assert!(output.contains("ARGS:1\n"), "{output}");
}

#[test]
fn a_files_mount_hands_bytes_to_the_module_read_only() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let host = activate(&fixture);
    let value = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "read /input/name.yaml\nwrite /input/new.txt",
            "mounts": [{
                "guest": "/input",
                "files": { "name.yaml": "hello: world\n" }
            }],
        }),
        None,
    )
    .expect("the module runs");
    assert_eq!(value["stdoutText"], "READ:hello: world\n\nWRITE_FAILED\n");
}

#[test]
fn a_blocking_host_call_is_stopped_by_the_time_limit() {
    let fixture = fixture(&["sleep.wasm"]);
    let host = activate(&fixture);
    let started = Instant::now();
    let outcome = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "sleep.wasm",
            "stdin": "3000",
            "limits": { "timeoutMs": 200 },
        }),
        None,
    );
    let elapsed = started.elapsed();
    // `std::thread::sleep` on wasm32-wasip1 is one `poll_oneoff` clock call,
    // which the host runs on its own thread. The epoch deadline is only checked
    // when wasm executes again, so the module outlives its 200 ms limit by the
    // whole sleep. A sandbox that promises a time limit must not allow this.
    assert!(
        elapsed < Duration::from_secs(2),
        "the time limit was missed while the module was blocked in a host call: {elapsed:?}"
    );
    assert!(
        outcome.is_err(),
        "the module should have been stopped at its deadline: {outcome:?}"
    );
}

#[test]
fn the_memory_cap_covers_a_module_table() {
    let fixture = fixture(&["table_hog.wasm"]);
    let host = activate(&fixture);
    // table_hog declares a 4,000,000-entry funcref table (tens of megabytes of
    // host memory) and no linear memory at all.
    let outcome = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "table_hog.wasm",
            "limits": { "memoryBytes": 1024 * 1024 },
        }),
        None,
    );
    if let Ok(value) = outcome {
        panic!(
            "a table larger than the module's memory cap was allocated and the module ran: {value}"
        );
    }
}

#[test]
fn a_module_file_over_the_size_cap_is_refused() {
    let fixture = fixture(&[]);
    let huge = fixture.package.join("huge.wasm");
    // A sparse file: the size is over the cap without writing 64 MiB of zeros.
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    let host = activate(&fixture);
    let error = invoke(
        &host,
        &fixture,
        serde_json::json!({ "module": "huge.wasm" }),
        None,
    )
    .expect_err("a module over the file size cap is refused");
    assert!(
        error.message.contains("at most") || error.message.to_lowercase().contains("large"),
        "the error names the size cap: {}",
        error.message
    );
}

#[test]
fn a_view_mount_over_its_cap_names_the_mount() {
    let fixture = fixture(&["fs_probe.wasm"]);
    // `stage` writes `a.esp` with five bytes of content.
    let reads = stage(&fixture, "a.esp");
    let host = activate(&fixture);
    let error = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "list /data",
            "mounts": [{ "guest": "/data", "view": { "folder": "Data", "patterns": ["*.esp"] } }],
            "limits": { "mountBytes": 4 },
        }),
        Some(reads),
    )
    .expect_err("a view mount over its cap is refused");
    assert!(
        error.message.contains("mount `/data`"),
        "the error names the mount: {}",
        error.message
    );
    assert!(
        error.message.contains("more than"),
        "the error says what cap was passed: {}",
        error.message
    );
}

#[test]
fn a_files_mount_over_its_cap_names_the_mount() {
    let fixture = fixture(&["fs_probe.wasm"]);
    let host = activate(&fixture);
    let error = invoke(
        &host,
        &fixture,
        serde_json::json!({
            "module": "fs_probe.wasm",
            "stdin": "list /input",
            "mounts": [{ "guest": "/input", "files": { "name.yaml": "hello" } }],
            "limits": { "mountBytes": 4 },
        }),
        None,
    )
    .expect_err("a files mount over its cap is refused");
    assert!(
        error.message.contains("mount `/input`"),
        "the error names the mount: {}",
        error.message
    );
}
