//! The `games instance` commands that match the desktop's game instance screens: rename, clone,
//! stop, Recently deleted, save backups, mod INI files, tool outputs and inputs, and user tools.
//!
//! Each test runs the compiled `agora` binary against an empty temporary `--data-dir`, with no
//! terminal and no network.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");

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

fn data_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary data folder")
}

/// The JSON error envelope on stderr. Startup warnings (such as the embedded catalog notice) are
/// plain lines before it, so the envelope starts at the first line that opens an object.
fn stderr_json(output: &Output) -> Result<serde_json::Value, serde_json::Error> {
    let text = String::from_utf8_lossy(&output.stderr);
    let start = text.find("\n{").map(|at| at + 1).unwrap_or(0);
    serde_json::from_str(&text[start..])
}

const NEW_COMMANDS: &[&[&str]] = &[
    &["games", "instance", "rename"],
    &["games", "instance", "clone"],
    &["games", "instance", "stop"],
    &["games", "instance", "deleted"],
    &["games", "instance", "restore"],
    &["games", "instance", "purge"],
    &["games", "instance", "save-backup", "create"],
    &["games", "instance", "save-backup", "list"],
    &["games", "instance", "save-backup", "restore"],
    &["games", "instance", "mod-ini", "list"],
    &["games", "instance", "mod-ini", "get"],
    &["games", "instance", "mod-ini", "set"],
    &["games", "instance", "mod-ini", "keep-game"],
    &["games", "instance", "mod-ini", "reset"],
    &["games", "instance", "mod-ini", "restore-mine"],
    &["games", "instance", "tools", "outputs"],
    &["games", "instance", "tools", "use-output"],
    &["games", "instance", "tools", "set-input"],
    &["games", "instance", "user-tools", "list"],
    &["games", "instance", "user-tools", "find"],
    &["games", "instance", "user-tools", "add"],
    &["games", "instance", "user-tools", "remove"],
    &["games", "instance", "user-tools", "run"],
    &["games", "instance", "user-tools", "outputs"],
    &["games", "instance", "user-tools", "use-output"],
];

#[test]
fn every_new_command_prints_its_help() {
    let tmp = data_dir();
    for command in NEW_COMMANDS {
        let mut args = command.to_vec();
        args.push("--help");
        let output = agora(tmp.path(), &args);
        assert!(
            output.status.success(),
            "`agora {}` exited {:?}: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage:"),
            "`agora {}` printed no usage",
            args.join(" ")
        );
    }
}

#[test]
fn deleted_lists_nothing_for_an_empty_data_folder() {
    let tmp = data_dir();
    let output = agora(
        tmp.path(),
        &["--json", "games", "instance", "deleted", "skyrimse"],
    );
    assert!(
        output.status.success(),
        "exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let out: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("the output is JSON");
    assert_eq!(out["game"], "skyrimse");
    assert_eq!(out["entries"], serde_json::json!([]));
    assert!(out["retention_days"].is_number(), "{out}");
}

#[test]
fn deleted_refuses_minecraft() {
    let tmp = data_dir();
    let output = agora(
        tmp.path(),
        &["--json", "games", "instance", "deleted", "minecraft"],
    );
    assert_eq!(output.status.code(), Some(1));
    let err = stderr_json(&output).expect("the error is JSON");
    assert_eq!(err["status"], "error");
}

#[test]
fn an_unknown_instance_is_an_error_with_exit_1() {
    let tmp = data_dir();
    for args in [
        &["--json", "games", "instance", "rename", "nope", "New name"][..],
        &["--json", "games", "instance", "save-backup", "list", "nope"],
        &["--json", "games", "instance", "mod-ini", "list", "nope"],
        &["--json", "games", "instance", "user-tools", "list", "nope"],
        &[
            "--json", "games", "instance", "tools", "outputs", "nope", "nemesis",
        ],
    ] {
        let output = agora(tmp.path(), args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`agora {}` did not exit 1",
            args.join(" ")
        );
        let err = stderr_json(&output)
            .unwrap_or_else(|e| panic!("`agora {}` error is not JSON: {e}", args.join(" ")));
        assert!(
            err["error"].as_str().unwrap_or_default().contains("nope"),
            "{err}"
        );
        assert!(output.stdout.is_empty(), "printed to stdout on failure");
    }
}

#[test]
fn purging_all_of_an_empty_game_needs_no_confirmation() {
    let tmp = data_dir();
    let output = agora(
        tmp.path(),
        &["--json", "games", "instance", "purge", "skyrimse"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(out["purged"], 0);
}

#[test]
fn purging_an_unknown_entry_fails_without_prompting() {
    let tmp = data_dir();
    let output = agora(
        tmp.path(),
        &[
            "--json",
            "games",
            "instance",
            "purge",
            "skyrimse",
            "no-such-entry",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
}
