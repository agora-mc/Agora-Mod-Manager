//! Integration tests for the commands that match desktop features: `downloads list|add|ignore`,
//! `registry vote`, `auth refresh`, `auth github`, `skin library`, `server edit|move|ping`,
//! `plugin run|diagnose|repair|launch-checks` and `template update`.
//!
//! Each test runs the compiled `agora` binary against a temporary `--data-dir`. None reaches the
//! network or a credential store: sign-in, voting, refresh and ping are covered by their help
//! text and argument checks only.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");

struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_root() -> TempRoot {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "agora-cli-parity-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("create temp data root");
    TempRoot(path)
}

fn agora(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(AGORA_BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("AGORA_TEST_MSA_CREDENTIALS_JSON")
        .stdin(Stdio::null())
        .output()
        .expect("run agora")
}

fn agora_json(data_dir: &Path, args: &[&str]) -> Output {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    agora(data_dir, &all)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("stdout must be JSON: {e}\n{}", stdout(output)))
}

#[test]
fn every_new_command_has_help() {
    let root = temp_root();
    let commands: &[&[&str]] = &[
        &["downloads", "list"],
        &["downloads", "add"],
        &["downloads", "ignore"],
        &["registry", "vote"],
        &["auth", "refresh"],
        &["auth", "github"],
        &["auth", "github", "login"],
        &["auth", "github", "status"],
        &["auth", "github", "logout"],
        &["skin", "library"],
        &["skin", "library", "list"],
        &["skin", "library", "add"],
        &["skin", "library", "remove"],
        &["server", "edit"],
        &["server", "move"],
        &["server", "ping"],
        &["plugin", "run"],
        &["plugin", "diagnose"],
        &["plugin", "repair"],
        &["plugin", "launch-checks"],
        &["template", "update"],
    ];
    for command in commands {
        let mut args = command.to_vec();
        args.push("--help");
        let output = agora(&root.0, &args);
        assert!(
            output.status.success(),
            "{command:?} --help failed: {}",
            stderr(&output)
        );
        assert!(
            stdout(&output).contains("Usage:"),
            "{command:?} --help printed no usage"
        );
    }
}

#[test]
fn downloads_list_is_an_empty_json_array_on_a_fresh_root() {
    let root = temp_root();
    let output = agora_json(&root.0, &["downloads", "list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(json(&output), serde_json::json!([]));
}

#[test]
fn adding_an_unknown_download_fails() {
    let root = temp_root();
    let output = agora(&root.0, &["downloads", "add", "not-a-download"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
}

#[test]
fn skin_library_lists_nothing_on_a_fresh_root() {
    let root = temp_root();
    let output = agora_json(&root.0, &["skin", "library"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(json(&output), serde_json::json!([]));

    let output = agora_json(&root.0, &["skin", "library", "list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(json(&output), serde_json::json!([]));
}

#[test]
fn skin_library_refuses_a_file_that_is_not_a_skin() {
    let root = temp_root();
    let file = root.0.join("not-a-skin.png");
    std::fs::write(&file, b"plain text").unwrap();
    let output = agora(&root.0, &["skin", "library", "add", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let output = agora_json(&root.0, &["skin", "library"]);
    assert_eq!(json(&output), serde_json::json!([]));
}

#[test]
fn plugin_launch_checks_report_nothing_without_plugins() {
    let root = temp_root();
    let output = agora_json(&root.0, &["plugin", "launch-checks", "no-such-instance"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value = json(&output);
    assert_eq!(value["results"], serde_json::json!([]));
    assert_eq!(value["errors"], serde_json::json!([]));
}

#[test]
fn plugin_run_rejects_arguments_that_are_not_json() {
    let root = temp_root();
    let output = agora(
        &root.0,
        &[
            "plugin",
            "run",
            "example.plugin",
            "go",
            "--args",
            "{not json",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
}

#[test]
fn server_edit_needs_something_to_change() {
    let root = temp_root();
    let output = agora(&root.0, &["server", "edit", "some-instance", "1"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("Nothing to change"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn server_move_only_takes_up_or_down() {
    let root = temp_root();
    let output = agora(
        &root.0,
        &["server", "move", "some-instance", "1", "sideways"],
    );
    assert!(!output.status.success());
}

#[test]
fn registry_vote_only_takes_up_down_or_clear() {
    let root = temp_root();
    let output = agora(&root.0, &["registry", "vote", "some-item", "maybe"]);
    assert!(!output.status.success());
}

#[test]
fn template_update_changes_the_name_and_keeps_other_jvm_settings() {
    let root = temp_root();
    let created = agora_json(
        &root.0,
        &[
            "template",
            "create",
            "Big heap",
            "--memory",
            "8192",
            "--gc",
            "low_latency",
        ],
    );
    assert!(created.status.success(), "{}", stderr(&created));
    let id = json(&created)["id"].as_str().unwrap().to_string();

    let updated = agora_json(
        &root.0,
        &[
            "template",
            "update",
            &id,
            "--name",
            "Bigger heap",
            "--memory",
            "12288",
        ],
    );
    assert!(updated.status.success(), "{}", stderr(&updated));
    let value = json(&updated);
    assert_eq!(value["name"], "Bigger heap");
    assert_eq!(value["jvm"]["jvm_memory_mb"], 12288);
    assert_eq!(value["jvm"]["jvm_gc"], "low_latency");

    let cleared = agora_json(&root.0, &["template", "update", &id, "--clear-jvm"]);
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    assert_eq!(
        json(&cleared)["jvm"]["jvm_memory_mb"],
        serde_json::Value::Null
    );
}

#[test]
fn template_update_without_changes_or_template_fails() {
    let root = temp_root();
    let output = agora(&root.0, &["template", "update", "missing"]);
    assert_eq!(output.status.code(), Some(1));
    let output = agora(&root.0, &["template", "update", "missing", "--name", "X"]);
    assert_eq!(output.status.code(), Some(1));
}
