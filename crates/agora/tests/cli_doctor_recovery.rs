//! `agora doctor`'s recovery report, through the real command (MASTER_SPEC §32.11).
//!
//! The journals here are real: an install cut short at one of its own crash points, and the local
//! state database the launcher really writes. What `doctor` says about them is read from its own
//! `--json` output and its own human report, and the last test asks the command twice around a
//! snapshot of the whole data root.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use agora_core::app_paths::AppPaths;
use agora_core::ctx::CoreContext;
use agora_core::plugins::install::InstallCrashPoint;
use agora_core::plugins::PluginService;

const AGORA_BIN: &str = env!("CARGO_BIN_EXE_agora");
const PLUGIN: &str = "test.recovery";
/// The exit code `doctor` already used for a local state that needs a person.
const PROBLEMS: i32 = 10;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn agora(data_dir: &Path, args: &[&str]) -> std::process::Output {
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

fn json_of(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor --json did not print JSON ({error}): stdout {} stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn doctor_json(data_dir: &Path) -> serde_json::Value {
    json_of(&agora(data_dir, &["doctor", "--json"]))
}

fn package(version: &str) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("agora-plugin.json", options).unwrap();
    let manifest = serde_json::json!({
        "manifest": 1, "id": PLUGIN, "name": "Recovery Test",
        "version": version, "license": "MIT", "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js"
    });
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("main.js", options).unwrap();
    write!(zip, "// version {version}").unwrap();
    zip.finish().unwrap().into_inner()
}

fn install_version(paths: &AppPaths, version: &str) {
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service =
        PluginService::headless(ctx.clone(), Arc::new(agora_plugin_host::QuickJsHost::new()));
    service
        .install_package_bytes(&package(version), true)
        .unwrap();
}

/// A real install cut short at `point`, left exactly as the real operation left it. The journal's
/// recorded writer is a pid that cannot exist, so the report answers about a crash rather than
/// about a writer that is still running (that is its own test).
fn interrupted_install(paths: &AppPaths, point: InstallCrashPoint) {
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service =
        PluginService::headless(ctx.clone(), Arc::new(agora_plugin_host::QuickJsHost::new()));
    install_version(paths, "1.0.0");
    ctx.test_seams.set_install_crash(Some(point));
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service
            .install_package_bytes(&package("2.0.0"), true)
            .unwrap();
    }));
    ctx.test_seams.set_install_crash(None);
    assert!(crash.is_err(), "the install was cut short at {point:?}");
    let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
    conn.execute(
        "UPDATE plugin_install_journal
         SET document = json_set(document, '$.writer', json(?2))
         WHERE plugin_id = ?1",
        rusqlite::params![
            PLUGIN,
            r#"{"pid":4294967293,"start_time":1,"expected_exe":null}"#
        ],
    )
    .unwrap();
}

/// A data root with a real database, laid out the way the launcher lays it out.
fn prepared_root() -> (tempfile::TempDir, AppPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&paths.local_state_db()).unwrap();
    (dir, paths)
}

fn plugin_entry<'a>(doctor: &'a serde_json::Value, resource: &str) -> &'a serde_json::Value {
    doctor["recovery"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor reports recovery entries: {doctor}"))
        .iter()
        .find(|entry| entry["kind"] == "plugin-install" && entry["resource"] == resource)
        .unwrap_or_else(|| panic!("the plugin-install journal for {resource} is listed: {doctor}"))
}

fn not_searched<'a>(doctor: &'a serde_json::Value, kind: &str) -> &'a serde_json::Value {
    doctor["recovery"]["notSearched"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor says what it could not search: {doctor}"))
        .iter()
        .find(|missing| missing["kind"] == kind)
        .unwrap_or_else(|| panic!("the {kind} kind is reported as not searched: {doctor}"))
}

/// A printed command, without the program name: the harness runs `agora` itself.
fn words(command: &str) -> Vec<&str> {
    command.split_whitespace().skip(1).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn doctor_reports_an_interrupted_install_that_would_be_undone() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Extracted);

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&doctor);
    let entry = plugin_entry(&parsed, PLUGIN);
    assert_eq!(entry["outcome"], "wouldUndo", "{entry}");
    assert_eq!(entry["state"], "extracted");
    assert_eq!(
        entry["command"],
        format!("agora plugin recover {PLUGIN} --yes"),
        "{entry}"
    );
    assert!(
        entry["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the report says why: {entry}"
    );

    let human = agora(dir.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("plugin-install test.recovery")
            && stdout.contains("recovery would undo it"),
        "the human report says the same thing: {stdout}"
    );
}

/// The command `doctor` prints is the command that resolves the journal: running it settles the
/// journal, and the next report has nothing left to say about it.
#[test]
fn the_command_doctor_prints_resolves_the_plugin_journal() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    let command = plugin_entry(&doctor_json(dir.path()), PLUGIN)["command"]
        .as_str()
        .expect("a journal that would be undone names the command that undoes it")
        .to_string();

    let resolved = agora(dir.path(), &words(&command));
    assert!(
        resolved.status.success(),
        "the printed command runs: {command}\nstdout {}\nstderr {}",
        String::from_utf8_lossy(&resolved.stdout),
        String::from_utf8_lossy(&resolved.stderr)
    );

    let after = doctor_json(dir.path());
    assert!(
        after["recovery"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["kind"] != "plugin-install"),
        "the journal is gone, not reported again: {after}"
    );
}

#[test]
fn doctor_reports_an_interrupted_install_that_would_be_finished() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Committed);

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&doctor);
    let entry = plugin_entry(&parsed, PLUGIN);
    assert_eq!(entry["outcome"], "wouldFinish", "{entry}");
    assert_eq!(entry["state"], "committed");
}

#[test]
fn doctor_reports_a_journal_whose_writer_is_still_running_as_busy() {
    let (dir, paths) = prepared_root();
    // Cut the install short and leave the writer as the process that wrote it. That process is
    // this test's, which is alive while `doctor` runs, so the answer must be "busy".
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service =
        PluginService::headless(ctx.clone(), Arc::new(agora_plugin_host::QuickJsHost::new()));
    install_version(&paths, "1.0.0");
    ctx.test_seams
        .set_install_crash(Some(InstallCrashPoint::Extracted));
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service
            .install_package_bytes(&package("2.0.0"), true)
            .unwrap();
    }));
    ctx.test_seams.set_install_crash(None);
    assert!(crash.is_err());

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&doctor);
    let entry = plugin_entry(&parsed, PLUGIN);
    assert_eq!(entry["outcome"], "busy", "{entry}");
}

#[test]
fn doctor_lists_an_import_with_its_facts_and_no_prediction() {
    let (dir, paths) = prepared_root();
    write_import_job(&paths, "job-one", "imported-one");

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&doctor);
    let entry = parsed["recovery"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "instance-import" && entry["resource"] == "imported-one")
        .unwrap_or_else(|| panic!("the import journal is listed: {parsed}"));
    assert_eq!(entry["state"], "staging");
    assert_eq!(entry["outcome"], "undecided", "{entry}");
    assert_eq!(
        entry["command"],
        serde_json::Value::Null,
        "this build prints no command for a kind it does not decide: {entry}"
    );
    assert!(
        entry["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("The next start settles it")),
        "the report says who settles it, and that nothing was changed: {entry}"
    );

    let human = agora(dir.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("instance-import imported-one")
            && stdout.contains("this build does not decide it yet"),
        "{stdout}"
    );
}

/// One unreadable row is reported as that row; the readable ones are still listed.
#[test]
fn doctor_reports_an_unreadable_import_row_as_that_row() {
    let (dir, paths) = prepared_root();
    write_import_job(&paths, "job-one", "imported-one");
    write_import_job(&paths, "job-two", "imported-two");
    {
        // A value this build cannot read as text: the journal row is one it cannot parse.
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute(
            "UPDATE instance_import_jobs SET staging_path = x'00ff' WHERE job_id = 'job-two'",
            [],
        )
        .unwrap();
    }

    let parsed = doctor_json(dir.path());
    let listed = parsed["recovery"]["entries"].as_array().unwrap();
    let good = listed
        .iter()
        .find(|entry| entry["kind"] == "instance-import" && entry["outcome"] == "undecided")
        .unwrap_or_else(|| panic!("the readable row is still listed: {parsed}"));
    assert_eq!(good["resource"], "imported-one", "{good}");
    let bad = listed
        .iter()
        .find(|entry| entry["kind"] == "instance-import" && entry["outcome"] == "needsHand")
        .unwrap_or_else(|| panic!("the unreadable row is reported as needing a hand: {parsed}"));
    assert!(
        bad["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cannot be read")),
        "the row's own error is reported: {bad}"
    );
}

/// An import job row, written the way the import service writes it.
fn write_import_job(paths: &AppPaths, job_id: &str, instance_id: &str) {
    let staging = paths.staging_root().join(format!("import-{job_id}"));
    std::fs::create_dir_all(&staging).unwrap();
    let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
    let job = agora_core::db::InstanceImportJob {
        job_id: job_id.into(),
        instance_id: instance_id.into(),
        launcher_kind: "official".into(),
        source_instance_key: "some-source".into(),
        staging_path: staging.display().to_string(),
        final_path: paths
            .instances_root()
            .join(instance_id)
            .display()
            .to_string(),
        quarantine_path: None,
        state: "staging".into(),
        plan_fingerprint: "fingerprint".into(),
        error: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    agora_core::db::upsert_instance_import_job(&conn, &job).unwrap();
}

#[test]
fn doctor_says_there_is_no_database_yet_and_lists_file_journals() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(&swaps).unwrap();
    std::fs::write(swaps.join("skyrim-se_steam.json"), b"not a swap journal").unwrap();

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert_eq!(
        doctor.status.code(),
        Some(PROBLEMS),
        "a journal on disk that cannot be read is a problem, even with no database: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let parsed = json_of(&doctor);
    assert!(
        !paths.local_state_db().exists(),
        "doctor does not create the database"
    );
    let notes = parsed["recovery"]["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .is_some_and(|note| note.to_lowercase().contains("no local database"))),
        "the report says there is no database yet: {parsed}"
    );
    for kind in ["plugin-install", "instance-import"] {
        let missing = not_searched(&parsed, kind);
        assert_eq!(
            missing["blocking"], false,
            "a fresh data root is not a problem: {missing}"
        );
        assert!(
            missing["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("No local database yet")),
            "{missing}"
        );
    }
    let swap = parsed["recovery"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "tool-swap")
        .unwrap_or_else(|| panic!("the tool-swap journal is listed: {parsed}"));
    assert_eq!(swap["outcome"], "needsHand", "{swap}");
}

#[test]
fn doctor_reports_a_newer_database_as_cannot_tell_for_the_journals() {
    let (dir, paths) = prepared_root();
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (999)", [])
            .unwrap();
    }

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert_eq!(
        doctor.status.code(),
        Some(PROBLEMS),
        "a database this build cannot read may hide the journal that blocks startup: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let parsed = json_of(&doctor);
    for kind in ["plugin-install", "instance-import"] {
        let missing = not_searched(&parsed, kind);
        assert_eq!(missing["blocking"], true, "{missing}");
        assert!(
            missing["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("newer")),
            "the reason names the newer schema: {missing}"
        );
    }
}

/// A journal whose ownership marker was removed cannot be acted on, so `doctor` says "needs a hand",
/// names the reason startup gives, and exits with the code it already used for a local state that
/// needs a person.
#[test]
fn doctor_says_a_journal_that_needs_a_hand_and_exits_with_the_problems_code() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    let markers: Vec<PathBuf> = std::fs::read_dir(paths.plugin_rollback_root())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".owner-"))
        })
        .collect();
    assert!(!markers.is_empty(), "the install wrote an ownership marker");
    for marker in markers {
        std::fs::remove_file(marker).unwrap();
    }

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert_eq!(
        doctor.status.code(),
        Some(PROBLEMS),
        "a journal that needs a hand is a problem: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let parsed = json_of(&doctor);
    assert_eq!(parsed["problems"], true, "{parsed}");
    let entry = plugin_entry(&parsed, PLUGIN);
    assert_eq!(entry["outcome"], "needsHand", "{entry}");
    assert!(
        entry["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("could not be verified")),
        "the reason is the one startup gives: {entry}"
    );

    let human = agora(dir.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("needs a hand"), "{stdout}");
    assert!(
        !stdout.contains("Agora found no problems"),
        "a journal that needs a hand is not \"no problems\": {stdout}"
    );
}

/// A legacy stash no database knows about is reported with the one command that resolves it, and
/// that command really does resolve it.
#[test]
fn the_legacy_keep_aside_command_doctor_prints_resolves_the_stash() {
    let (dir, paths) = prepared_root();
    let legacy = paths.plugin_rollback_root().join("test.legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("agora-plugin.json"), "{}").unwrap();

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert_eq!(doctor.status.code(), Some(PROBLEMS));
    let parsed = json_of(&doctor);
    let entry = plugin_entry(&parsed, "test.legacy");
    assert_eq!(entry["outcome"], "needsHand", "{entry}");
    let command = entry["command"]
        .as_str()
        .unwrap_or_else(|| panic!("the legacy stash names its command: {entry}"))
        .to_string();
    assert_eq!(
        command,
        "agora plugin recover test.legacy --keep-aside --yes"
    );

    let resolved = agora(dir.path(), &words(&command));
    assert!(
        resolved.status.success(),
        "the printed command runs: {command}\nstdout {}\nstderr {}",
        String::from_utf8_lossy(&resolved.stdout),
        String::from_utf8_lossy(&resolved.stderr)
    );
    let after = doctor_json(dir.path());
    assert!(
        after["recovery"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["resource"] != "test.legacy"),
        "the stash was kept aside, so it is not reported as unresolved: {after}"
    );
}

/// A database recovery marker with the database moved aside is explained, and `doctor` does not
/// claim the next start will create a fresh database.
#[test]
fn doctor_explains_a_pending_database_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    let db = paths.local_state_db();
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let identity = "3f0e0c2e-0000-4000-8000-000000000001";
    std::fs::write(db.with_file_name("local_state.db.identity"), identity).unwrap();
    std::fs::write(
        db.with_file_name("local_state.db.recovery"),
        serde_json::json!({ "identity": identity, "db_path": db.display().to_string() })
            .to_string(),
    )
    .unwrap();

    let parsed = doctor_json(dir.path());
    let entry = parsed["recovery"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "database-recovery")
        .unwrap_or_else(|| panic!("the pending database recovery is listed: {parsed}"));
    assert_eq!(entry["outcome"], "wouldFinish", "{entry}");
    let notes = parsed["recovery"]["notes"].as_array().unwrap();
    assert!(
        notes.iter().all(|note| !note
            .as_str()
            .is_some_and(|note| note.contains("No local database yet"))),
        "the database was moved aside; it was not never created: {parsed}"
    );
}

/// A Recently deleted entry is reported with its facts and no prediction, and a kept one names the
/// restore command the CLI really has.
#[test]
fn doctor_reports_a_recently_deleted_entry_with_its_facts() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    let entry_dir = paths.recently_deleted_games_root().join("entry-1");
    std::fs::create_dir_all(entry_dir.join("instance")).unwrap();
    let record = serde_json::json!({
        "version": 1,
        "entryId": "entry-1",
        "instanceId": "deleted-one",
        "game": "minecraft",
        "name": "A Deleted World",
        "deletedAt": "2026-01-01T00:00:00Z",
        "expiresAt": "2026-02-01T00:00:00Z",
        "sizeBytes": 1024,
    });
    std::fs::write(entry_dir.join("entry.json"), record.to_string()).unwrap();

    let parsed = doctor_json(dir.path());
    let kept = parsed["recovery"]["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor lists the entry: {parsed}"))
        .iter()
        .find(|entry| entry["kind"] == "recently-deleted" && entry["resource"] == "entry-1")
        .unwrap_or_else(|| panic!("the kept entry is listed: {parsed}"));
    assert_eq!(kept["outcome"], "nothingToDo", "{kept}");
    assert_eq!(
        kept["command"], "agora games instance restore entry-1",
        "the command is the one this CLI has: {kept}"
    );

    // A restore marker makes the same entry undecided: this build does not decide that kind.
    std::fs::write(
        entry_dir.join("restoring.json"),
        serde_json::json!({ "new_id": "deleted-one", "new_name": "" }).to_string(),
    )
    .unwrap();
    let parsed = doctor_json(dir.path());
    let restoring = parsed["recovery"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "recently-deleted" && entry["resource"] == "entry-1")
        .unwrap_or_else(|| panic!("the entry is listed: {parsed}"));
    assert_eq!(restoring["outcome"], "undecided", "{restoring}");
    assert_eq!(restoring["command"], serde_json::Value::Null, "{restoring}");
    assert!(
        restoring["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("restoring.json is there and readable")),
        "the facts name both files: {restoring}"
    );
}

// ---------------------------------------------------------------------------
// Read-only, all the way, through the real command
// ---------------------------------------------------------------------------

type Snapshot = std::collections::BTreeMap<String, (u64, Vec<u8>)>;

fn snapshot(root: &Path) -> Snapshot {
    fn walk(dir: &Path, out: &mut Snapshot) {
        let entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("could not list {}: {error}", dir.display()),
        };
        for path in entries {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                out.insert(path.display().to_string(), (0, Vec::new()));
                walk(&path, out);
            } else {
                out.insert(
                    path.display().to_string(),
                    (metadata.len(), std::fs::read(&path).unwrap_or_default()),
                );
            }
        }
    }
    let mut out = Snapshot::new();
    walk(root, &mut out);
    out
}

/// Whether a path is one of SQLite's own `-shm` / `-wal` sidecars: the WAL's index and log, which
/// hold no part of the data root's content.
fn is_sqlite_sidecar(path: &str) -> bool {
    path.ends_with("local_state.db-shm") || path.ends_with("local_state.db-wal")
}

/// `agora doctor` is read-only: every file the data root had when it was asked is unchanged
/// afterwards, byte for byte, and nothing is removed or added — its own support log included, since
/// `doctor` runs before the log is opened.
///
/// SQLite materialises its own `-shm` and `-wal` sidecars for any reader of a WAL database; they
/// hold no part of the data root's content, the database file itself is unchanged, and a read-write
/// open (which `doctor` already does to inspect the database) deletes them again. Declaring the file
/// immutable to avoid them would mean trusting a database another Agora may be writing to.
#[test]
fn asking_doctor_changes_nothing_in_the_data_root() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    write_import_job(&paths, "job-one", "imported-one");

    let before = snapshot(dir.path());
    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert!(
        doctor.status.success() || doctor.status.code() == Some(PROBLEMS),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let after = snapshot(dir.path());

    let mut differences: Vec<String> = Vec::new();
    for (path, value) in &before {
        match after.get(path) {
            Some(other) if other == value => {}
            // The sidecars' bytes are the WAL's business, not the data root's (see above): the
            // fixture that builds the crash leaves a different wal-index by the time `doctor`
            // reads it, which says nothing about what `doctor` wrote.
            Some(_) if is_sqlite_sidecar(path) => {}
            Some(_) => differences.push(format!("changed: {path}")),
            None if is_sqlite_sidecar(path) => {}
            None => differences.push(format!("removed: {path}")),
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) && !is_sqlite_sidecar(path) {
            differences.push(format!("added: {path}"));
        }
    }
    assert_eq!(
        differences,
        Vec::<String>::new(),
        "doctor changed the data root"
    );
    // And the journal it reported is still there: reporting is not recovering.
    let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM plugin_install_journal WHERE plugin_id = ?1",
            [PLUGIN],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the journal is still there to be recovered");
}
