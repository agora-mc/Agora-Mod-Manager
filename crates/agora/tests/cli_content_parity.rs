//! The crash doctor, pack, mod, provider, registry and instance commands that match desktop
//! actions: each prints help, and the read-only ones answer in JSON or fail cleanly with exit 1.

use std::path::Path;
use std::process::{Command, Stdio};

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");

fn agora(data_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(AGORA_BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("AGORA_TEST_MSA_CREDENTIALS_JSON")
        .stdin(Stdio::null())
        .output()
        .expect("run agora")
}

const NEW_COMMANDS: &[&[&str]] = &[
    &["crash", "fixed"],
    &["crash", "still-crashing"],
    &["crash", "suspect-file"],
    &["crash", "report"],
    &["snapshots", "drift"],
    &["pack", "discard"],
    &["pack", "retry"],
    &["pack", "mods"],
    &["pack", "extract-overrides"],
    &["pack", "install-catalog"],
    &["mod", "add-stored"],
    &["mod", "versions"],
    &["loadout", "unlisted"],
    &["instance", "migrate"],
    &["instance", "history"],
    &["instance", "stop"],
    &["instance", "snapshot-versions"],
    &["provider", "show"],
    &["provider", "versions"],
    &["provider", "preview"],
    &["registry", "item"],
];

#[test]
fn every_new_command_prints_help() {
    let tmp = tempfile::tempdir().unwrap();
    for command in NEW_COMMANDS {
        let mut args = command.to_vec();
        args.push("--help");
        let output = agora(tmp.path(), &args);
        assert!(
            output.status.success(),
            "`agora {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
}

#[test]
fn still_crashing_with_no_stored_answer_offers_no_suspects_in_json() {
    let tmp = tempfile::tempdir().unwrap();
    let output = agora(
        tmp.path(),
        &["--json", "crash", "still-crashing", "nothing", "item:one"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value["suspects"], serde_json::json!([]));
    assert_eq!(value["ruled_out"], serde_json::json!(["item:one"]));
}

#[test]
fn crash_report_for_an_instance_with_no_evidence_says_so_in_json() {
    let tmp = tempfile::tempdir().unwrap();
    let output = agora(tmp.path(), &["--json", "crash", "report", "nothing"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(value["report"]
        .as_str()
        .is_some_and(|report| report.contains("no crash evidence")));
}

#[test]
fn content_add_refuses_unwritable_names_and_imports_without_them_on_request() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let archive = work.path().join("mod.zip");
    {
        use std::io::Write;
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in [("ok.txt", b"fine" as &[u8]), ("Data/con.txt", b"1")] {
            zip.start_file(name, options).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }
    let archive_arg = archive.to_string_lossy().to_string();

    // Refused by default, with the entry and why on the command line.
    let refused = agora(tmp.path(), &["games", "content", "add", &archive_arg]);
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("Data/con.txt"), "{stderr}");
    assert!(stderr.contains("device name"), "{stderr}");
    assert!(stderr.contains("--without-unwritable-names"), "{stderr}");

    // The same refusal as data, so a front end lists the names instead of parsing them.
    let refused_json = agora(
        tmp.path(),
        &["--json", "games", "content", "add", &archive_arg],
    );
    assert_eq!(refused_json.status.code(), Some(1));
    let envelope: serde_json::Value = serde_json::from_str(
        String::from_utf8_lossy(&refused_json.stderr)
            .lines()
            .skip_while(|line| !line.starts_with('{'))
            .collect::<Vec<_>>()
            .join("\n")
            .as_str(),
    )
    .expect("a JSON error envelope");
    assert_eq!(envelope["unwritableNames"][0]["name"], "Data/con.txt");

    // Opted in: the files that can be written are stored, and the rest are listed in the result.
    let imported = agora(
        tmp.path(),
        &[
            "--json",
            "games",
            "content",
            "add",
            &archive_arg,
            "--without-unwritable-names",
        ],
    );
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let outcome: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&imported.stdout).as_ref())
            .expect("the added item as JSON");
    assert_eq!(outcome["item"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(outcome["skipped"][0]["name"], "Data/con.txt");
}

#[test]
fn reading_a_missing_instance_or_item_fails_with_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        &["--json", "instance", "history", "nothing"][..],
        &["--json", "loadout", "unlisted", "nothing", "profile"],
        &["--json", "snapshots", "drift", "nothing", "snap"],
        &["--json", "instance", "stop", "nothing"],
    ] {
        let output = agora(tmp.path(), args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "`agora {}` should fail: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // A fresh data root has no cached catalog, which the exit-code table documents as 13
    // ("catalog missing"), not the generic 1 a missing instance or file uses.
    let output = agora(tmp.path(), &["--json", "registry", "item", "nothing"]);
    assert_eq!(
        output.status.code(),
        Some(13),
        "`agora registry item` with no catalog should fail with 13: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
