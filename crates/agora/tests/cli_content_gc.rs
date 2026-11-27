//! `agora games content gc` and `agora games content migrate`, through the real binary
//! (MASTER_SPEC §32.8 "the collection's report goes back to the caller", "Migration"; §32.11 "every
//! needs-a-hand result has a way out").
//!
//! Written before the commands exist (owner's routing, 2026-11-26). What each test asserts may not
//! change; a name, a JSON key spelling or an exit code may, with the reason in the report.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use agora_core::app_paths::AppPaths;
use sha2::{Digest, Sha256};

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");
/// The "dependency, decision, or migration failure" range's migration code (docs/CLI.md).
const MIGRATION_NEEDS_A_HAND: i32 = 72;

fn agora(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(AGORA_BIN)
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("AGORA_TEST_MSA_CREDENTIALS_JSON")
        .env("AGORA_HOME", data_dir)
        .stdin(Stdio::null())
        .output()
        .expect("run agora")
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "--json did not print one JSON value ({error}): stdout {} stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn prepared_root() -> (tempfile::TempDir, AppPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&paths.local_state_db()).unwrap();
    (dir, paths)
}

/// An object nothing names, written `age` ago: the collector's grace (an hour) is judged by the
/// file's modification time.
fn orphan_object(paths: &AppPaths, bytes: &[u8], age: Duration) -> PathBuf {
    let path = paths.content_object_path(&sha256_hex(bytes));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
    path
}

fn legacy_object(paths: &AppPaths, bytes: &[u8]) -> PathBuf {
    let hash = sha256_hex(bytes);
    let path = paths
        .root()
        .join("snapshot-objects")
        .join(&hash[..2])
        .join(&hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn journal(paths: &AppPaths) -> PathBuf {
    paths
        .content_root()
        .join("migrations")
        .join("snapshot-objects.json")
}

fn damage_journal(paths: &AppPaths) -> PathBuf {
    let path = journal(paths);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{ not a journal").unwrap();
    path
}

/// An instance whose only snapshot manifest cannot be read.
fn instance_with_unreadable_snapshot(paths: &AppPaths, id: &str) -> PathBuf {
    let dir = paths.instance_dir(id).unwrap();
    fs::create_dir_all(dir.join(".agora_snapshots")).unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 2, "instance_id": id, "name": id,
        "minecraft_version": "1.20.1", "loader": "fabric", "loader_version": "0.15.11",
        "mods": [], "resourcepacks": [], "shaders": [], "datapacks": [], "worlds": []
    });
    fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let snapshot = dir.join(".agora_snapshots").join("broken-snap.json");
    fs::write(&snapshot, b"{ half a manifest").unwrap();
    snapshot
}

// ---------------------------------------------------------------------------
// agora games content gc
// ---------------------------------------------------------------------------

#[test]
fn gc_reclaims_an_old_object_nothing_names_and_says_so() {
    let (dir, paths) = prepared_root();
    let orphan = orphan_object(&paths, b"nobody's", Duration::from_secs(3 * 3600));

    let output = agora(dir.path(), &["games", "content", "gc"]);

    assert!(output.status.success(), "{}", text(&output));
    assert!(!orphan.exists(), "the orphan was reclaimed: {}", text(&output));
    let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
    assert!(
        stdout.contains("reclaimed") || stdout.contains("freed"),
        "the command prints what the collection did: {stdout}"
    );
}

#[test]
fn gc_json_carries_the_collection_report() {
    let (dir, paths) = prepared_root();
    let orphan = orphan_object(&paths, b"json orphan", Duration::from_secs(3 * 3600));

    let output = agora(dir.path(), &["games", "content", "gc", "--json"]);

    assert!(output.status.success(), "{}", text(&output));
    let parsed = json_of(&output);
    let collection = &parsed["collection"];
    assert_eq!(collection["reclaimed"], 1, "{parsed}");
    assert_eq!(collection["refused"], false, "{parsed}");
    assert!(collection["summary"].is_string(), "{parsed}");
    assert!(!orphan.exists());
}

#[test]
fn gc_keeps_a_new_object_and_says_why() {
    let (dir, paths) = prepared_root();
    let fresh = orphan_object(&paths, b"just written", Duration::ZERO);

    let output = agora(dir.path(), &["games", "content", "gc", "--json"]);

    assert!(output.status.success(), "{}", text(&output));
    let parsed = json_of(&output);
    assert_eq!(parsed["collection"]["reclaimed"], 0, "{parsed}");
    assert!(fresh.exists(), "an object inside the grace period is kept");
    // The sentence names the rule, not a duration: the collector's grace is a context seam, so a
    // summary that hardcoded "an hour" would be wrong for a round that runs with another one.
    assert!(
        parsed["collection"]["summary"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("grace period"),
        "the summary says why nothing was reclaimed: {parsed}"
    );
}

#[test]
fn gc_deletes_nothing_while_a_snapshot_manifest_cannot_be_read() {
    let (dir, paths) = prepared_root();
    let _snapshot = instance_with_unreadable_snapshot(&paths, "damaged");
    let orphan = orphan_object(&paths, b"kept by doubt", Duration::from_secs(3 * 3600));

    let output = agora(dir.path(), &["games", "content", "gc", "--json"]);

    let parsed = json_of(&output);
    assert_eq!(parsed["collection"]["refused"], true, "{parsed}");
    assert_eq!(parsed["collection"]["reclaimed"], 0, "{parsed}");
    assert!(orphan.exists(), "nothing is deleted while an owner record cannot be read");
    assert!(
        parsed["collection"]["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("broken-snap"),
        "the refusal names the snapshot it could not read: {parsed}"
    );
}

#[test]
fn gc_with_the_override_names_each_ignored_snapshot_and_its_risk() {
    let (dir, paths) = prepared_root();
    let _snapshot = instance_with_unreadable_snapshot(&paths, "damaged");
    let orphan = orphan_object(&paths, b"freed by choice", Duration::from_secs(3 * 3600));

    let output = agora(
        dir.path(),
        &["games", "content", "gc", "--ignore-unreadable-snapshots"],
    );

    assert!(output.status.success(), "{}", text(&output));
    assert!(!orphan.exists(), "the person chose to go ahead: {}", text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("broken-snap") && stderr.to_lowercase().contains("restore"),
        "stderr names the ignored snapshot and that it may not restore: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// agora games content migrate
// ---------------------------------------------------------------------------

/// Every command runs the startup pass first, and the pass moves the old store.
#[test]
fn any_command_moves_the_old_snapshot_store_first() {
    let (dir, paths) = prepared_root();
    let legacy = legacy_object(&paths, b"old store");

    let output = agora(dir.path(), &["games", "content", "list"]);

    assert!(output.status.success(), "{}", text(&output));
    assert!(!legacy.exists(), "{}", text(&output));
    assert!(paths.content_object_path(&sha256_hex(b"old store")).is_file());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_lowercase()
            .contains("snapshot"),
        "the pass says what it moved, on stderr: {}",
        text(&output)
    );
}

#[test]
fn migrate_with_nothing_to_move_says_so() {
    let (dir, _paths) = prepared_root();
    let output = agora(dir.path(), &["games", "content", "migrate", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let parsed = json_of(&output);
    assert!(
        parsed["status"] == "nothing_to_do" || parsed["status"] == "done",
        "{parsed}"
    );
}

#[test]
fn migrate_reports_a_damaged_journal_with_the_way_out() {
    let (dir, paths) = prepared_root();
    let legacy = legacy_object(&paths, b"waiting for a hand");
    let damaged = damage_journal(&paths);

    let output = agora(dir.path(), &["games", "content", "migrate", "--json"]);

    assert_eq!(
        output.status.code(),
        Some(MIGRATION_NEEDS_A_HAND),
        "{}",
        text(&output)
    );
    let parsed = json_of(&output);
    assert_eq!(parsed["status"], "needs_hand", "{parsed}");
    assert!(parsed["reason"].is_string(), "{parsed}");
    assert_eq!(
        parsed["command"], "agora games content migrate --restart",
        "{parsed}"
    );
    assert!(legacy.exists(), "nothing moves under a journal nobody can read");
    assert_eq!(fs::read(&damaged).unwrap(), b"{ not a journal");
}

#[test]
fn migrate_restart_sets_the_damaged_journal_aside_and_finishes() {
    let (dir, paths) = prepared_root();
    let legacy = legacy_object(&paths, b"moved after a hand");
    let damaged = damage_journal(&paths);

    let output = agora(
        dir.path(),
        &["games", "content", "migrate", "--restart", "--json"],
    );

    assert!(output.status.success(), "{}", text(&output));
    let parsed = json_of(&output);
    assert_eq!(parsed["status"], "done", "{parsed}");
    assert_eq!(parsed["moved"], 1, "{parsed}");
    assert!(!legacy.exists());
    let set_aside = fs::read_dir(damaged.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|entry| fs::read(entry.path()).ok().as_deref() == Some(b"{ not a journal"))
        .count();
    assert_eq!(set_aside, 1, "the damaged journal is kept, beside the new one");
}

#[test]
fn migrate_lists_kept_mismatches_by_path() {
    let (dir, paths) = prepared_root();
    let claimed = sha256_hex(b"what the name promises");
    let bad = paths
        .root()
        .join("snapshot-objects")
        .join(&claimed[..2])
        .join(&claimed);
    fs::create_dir_all(bad.parent().unwrap()).unwrap();
    fs::write(&bad, b"something else").unwrap();
    // The startup pass of this very command finishes the move; `migrate` then reports it.
    let output = agora(dir.path(), &["games", "content", "migrate", "--json"]);

    assert!(output.status.success(), "{}", text(&output));
    let parsed = json_of(&output);
    let mismatched = parsed["mismatched"]
        .as_array()
        .unwrap_or_else(|| panic!("the report lists kept mismatches: {parsed}"));
    assert!(
        mismatched
            .iter()
            .any(|path| path.as_str().is_some_and(|path| path.contains(&claimed))),
        "{parsed}"
    );
    assert!(bad.exists());
}

// ---------------------------------------------------------------------------
// agora doctor
// ---------------------------------------------------------------------------

/// Doctor only reads: it names a migration that needs a hand, with the command that resolves it,
/// and moves nothing.
#[test]
fn doctor_names_a_stuck_migration_and_moves_nothing() {
    let (dir, paths) = prepared_root();
    let legacy = legacy_object(&paths, b"doctor looks");
    let damaged = damage_journal(&paths);

    let output = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&output);

    let entry = parsed["recovery"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor reports recovery entries: {parsed}"))
        .iter()
        .find(|entry| entry["kind"] == "snapshot-object-migration")
        .unwrap_or_else(|| panic!("the migration journal is listed: {parsed}"));
    assert!(
        entry["command"]
            .as_str()
            .is_some_and(|command| command.contains("agora games content migrate --restart")),
        "{entry}"
    );
    assert!(legacy.exists(), "doctor moves nothing");
    assert_eq!(fs::read(&damaged).unwrap(), b"{ not a journal");
}
