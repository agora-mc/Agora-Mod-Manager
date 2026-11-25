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
fn doctor_lists_a_malformed_import_journal_with_its_path_and_error() {
    let (dir, paths) = prepared_root();
    let staging = paths.staging_root().join("import-broken");
    std::fs::create_dir_all(&staging).unwrap();
    let marker = staging.join(".agora-import-owner.json");
    std::fs::write(&marker, b"{ this is not json").unwrap();
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        let job = agora_core::db::InstanceImportJob {
            job_id: "job-broken".into(),
            instance_id: "imported-one".into(),
            launcher_kind: "official".into(),
            source_instance_key: "some-source".into(),
            staging_path: staging.display().to_string(),
            final_path: paths
                .instances_root()
                .join("imported-one")
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

    let doctor = agora(dir.path(), &["doctor", "--json"]);
    let parsed = json_of(&doctor);
    let entry = parsed["recovery"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "instance-import")
        .unwrap_or_else(|| panic!("the import journal is listed: {parsed}"));
    assert_eq!(entry["resource"], "imported-one");
    assert_eq!(entry["outcome"], "needsHand", "{entry}");
    assert_eq!(entry["path"], marker.display().to_string());
    let reason = entry["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("not JSON") || reason.contains("could not be read"),
        "the parse error is reported: {reason}"
    );
}

#[test]
fn doctor_says_there_is_no_database_yet_and_lists_file_journals() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(&swaps).unwrap();
    std::fs::write(swaps.join("skyrim-se_steam.json"), b"not a swap journal").unwrap();

    let doctor = agora(dir.path(), &["doctor", "--json"]);
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
    let parsed = json_of(&doctor);
    let not_searched = parsed["recovery"]["notSearched"]
        .as_array()
        .unwrap_or_else(|| panic!("doctor says what it could not search: {parsed}"));
    let plugin = not_searched
        .iter()
        .find(|kind| kind["kind"] == "plugin-install")
        .unwrap_or_else(|| panic!("the plugin-install kind is reported: {parsed}"));
    assert!(
        plugin["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("newer")),
        "the reason names the newer schema: {plugin}"
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

/// `agora doctor` is read-only: every file the data root had when it was asked is unchanged
/// afterwards, byte for byte, and nothing is removed.
///
/// SQLite materialises its own `-shm` and `-wal` sidecars for any reader of a WAL database; they
/// hold no part of the data root's content, the database file itself is unchanged, and a read-write
/// open (which `doctor` already does to inspect the database) deletes them again. Declaring the file
/// immutable to avoid them would mean trusting a database another Agora may be writing to.
///
/// The command's own support log (`logs/agora-cli.log`, the record of what a run printed) is
/// written by every CLI run, before and after this change, and is not part of the data.
#[test]
fn asking_doctor_changes_nothing_in_the_data_root() {
    let (dir, paths) = prepared_root();
    interrupted_install(&paths, InstallCrashPoint::Extracted);

    let before = snapshot(dir.path());
    let doctor = agora(dir.path(), &["doctor", "--json"]);
    assert!(
        doctor.status.success() || doctor.status.code() == Some(10),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let after = snapshot(dir.path());

    let mut differences: Vec<String> = Vec::new();
    for (path, value) in &before {
        match after.get(path) {
            Some(other) if other == value => {}
            Some(_) => differences.push(format!("changed: {path}")),
            None => differences.push(format!("removed: {path}")),
        }
    }
    for path in after.keys() {
        if !before.contains_key(path)
            && !(path.ends_with("local_state.db-shm") || path.ends_with("local_state.db-wal"))
            && !path.contains("\\logs")
            && !path.contains("/logs")
        {
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
