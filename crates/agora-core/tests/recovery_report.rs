//! `agora doctor`'s read-only recovery report (MASTER_SPEC §32.11, the last bullet of that section).
//!
//! The report explains a recovery that blocks startup: one entry per journal found, with its kind,
//! the resource it covers, its state, and the decision recovery would take. It must not change
//! anything to answer — no lock taken, no file written, renamed or deleted, no lease cleared, no
//! database migration, no WAL switch.
//!
//! Every journal here is built the way the real writer builds it: an interrupted plugin install is
//! a real install cut short at one of its own crash points, and the database is the real local
//! state database.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agora_core::app_paths::AppPaths;
use agora_core::ctx::CoreContext;
use agora_core::plugins::install::InstallCrashPoint;
use agora_core::plugins::PluginService;
use agora_core::recovery_report::{report, JournalKind, RecoveryOutcome};
use agora_plugin_host::QuickJsHost;

const PLUGIN: &str = "test.recovery";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn init_database(paths: &AppPaths) {
    paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&paths.local_state_db()).unwrap();
}

fn install_version(paths: &AppPaths, version: &str) {
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    service
        .install_package_bytes(&package(version), true)
        .unwrap();
}

/// A real install of `version` cut short at `point`, leaving the journal and the folders the real
/// operation leaves. The writer recorded in the journal is then made provably dead, so the report
/// answers about a crash rather than about a writer that is still running (that is its own test).
fn interrupted_install(paths: &AppPaths, point: InstallCrashPoint) {
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    install_version(paths, "1.0.0");
    ctx.test_seams.set_install_crash(Some(point));
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service
            .install_package_bytes(&package("2.0.0"), true)
            .unwrap();
    }));
    ctx.test_seams.set_install_crash(None);
    assert!(
        crash.is_err(),
        "the install was cut short at {point:?} and must not have finished"
    );
    set_writer(
        paths,
        r#"{"pid":4294967293,"start_time":1,"expected_exe":null}"#,
    );
}

/// The journal's recorded writer, replaced so liveness is decided rather than guessed.
fn set_writer(paths: &AppPaths, writer: &str) {
    let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
    conn.execute(
        "UPDATE plugin_install_journal
         SET document = json_set(document, '$.writer', json(?2))
         WHERE plugin_id = ?1",
        rusqlite::params![PLUGIN, writer],
    )
    .unwrap();
}

fn data_root() -> (tempfile::TempDir, AppPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(dir.path().to_path_buf());
    (dir, paths)
}

fn entry<'a>(
    report: &'a agora_core::recovery_report::RecoveryReport,
    kind: JournalKind,
) -> &'a agora_core::recovery_report::JournalEntry {
    report
        .entries
        .iter()
        .find(|entry| entry.kind == kind)
        .unwrap_or_else(|| {
            panic!(
                "the report must list the {kind:?} journal, it lists {:?}",
                report.entries
            )
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An install cut short before its commit is undone: the previous version comes back.
#[test]
fn an_interrupted_install_at_extracted_would_be_undone() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.resource, PLUGIN);
    assert_eq!(entry.outcome, RecoveryOutcome::WouldUndo, "{entry:?}");
    assert_eq!(entry.state.as_deref(), Some("extracted"));
    assert!(
        entry
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "the report says why: {entry:?}"
    );
    assert_eq!(
        entry.command.as_deref(),
        Some("agora plugin recover test.recovery --yes")
    );
}

/// An install cut short after its commit is finished: the new version stays and the previous
/// package is cleaned up.
#[test]
fn an_interrupted_install_at_committed_would_be_finished() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Committed);

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::WouldFinish, "{entry:?}");
    assert_eq!(entry.state.as_deref(), Some("committed"));
}

/// A journal whose writer is still running is busy, never "would act": the operation may still be
/// going. The writer here is a real live process — the one that wrote the journal.
#[test]
fn a_journal_whose_writer_is_alive_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    // Cut the install short, but leave the journal's writer as the very process that wrote it:
    // this process, which is alive for as long as the test runs.
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
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

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
}

/// A journal that is not JSON is listed with its path and the parse error, never skipped.
#[test]
fn a_malformed_import_journal_is_listed_with_its_error() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    let staging = paths.staging_root().join("import-broken");
    std::fs::create_dir_all(&staging).unwrap();
    // The journal is the job row; its ownership marker is the file recovery reads inside the
    // staging folder. Both are present, and the marker is unreadable.
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

    let report = report(&paths);
    let entry = entry(&report, JournalKind::InstanceImport);
    assert_eq!(entry.resource, "imported-one");
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert_eq!(
        entry.path,
        marker.display().to_string(),
        "the entry names the file that could not be read"
    );
    let reason = entry.reason.clone().unwrap_or_default();
    assert!(
        reason.contains("could not be read") || reason.contains("not JSON"),
        "the parse error is reported: {reason}"
    );
}

/// No database yet: the report says so, and still lists every journal that is a file.
#[test]
fn no_database_yet_is_reported_and_file_journals_are_still_listed() {
    let (_dir, paths) = data_root();
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(&swaps).unwrap();
    std::fs::write(swaps.join("skyrim-se_steam.json"), b"not a swap journal").unwrap();

    let report = report(&paths);
    assert!(
        report.notes.iter().any(
            |note| note.contains("no local database yet") || note.contains("No local database")
        ),
        "the report says there is no database yet: {report:?}"
    );
    let entry = entry(&report, JournalKind::ToolSwap);
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand);
    assert!(
        !report.entries.is_empty(),
        "a journal on disk is listed even without a database"
    );
}

/// A database this build does not know is "could not tell", with the reason — never "no journals".
#[test]
fn a_newer_database_schema_reports_the_plugin_journals_as_cannot_tell() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (999)", [])
            .unwrap();
    }

    let report = report(&paths);
    let not_searched = report
        .not_searched
        .iter()
        .find(|kind| kind.kind == JournalKind::PluginInstall)
        .unwrap_or_else(|| panic!("a newer schema is reported as not searched: {report:?}"));
    assert!(
        not_searched.reason.contains("newer"),
        "the reason names the newer schema: {}",
        not_searched.reason
    );
}

// ---------------------------------------------------------------------------
// Read-only, all the way
// ---------------------------------------------------------------------------

fn snapshot(root: &Path) -> Vec<(String, u64, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(String, u64, Vec<u8>)>) {
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("could not list {}: {error}", dir.display()),
        };
        entries.sort();
        for path in entries {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                out.push((path.display().to_string(), 0, Vec::new()));
                walk(&path, out);
            } else {
                out.push((
                    path.display().to_string(),
                    metadata.len(),
                    std::fs::read(&path).unwrap_or_default(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// The report is read-only: every file the data root had is byte-for-byte what it was, and no
/// journal or other file is added, removed or touched.
///
/// One exception, named rather than hidden: SQLite materialises its `-shm` and `-wal` sidecars for
/// any reader of a WAL database. They hold no part of the data root's content, the database file
/// itself is unchanged, and a read-write connection (which is what `agora doctor` already uses to
/// inspect the database) deletes them again on close. Reading the database without them would mean
/// declaring it immutable, which trusts a file another Agora may be writing to: reporting a journal
/// the writer has not committed yet, or missing one it just wrote, is the fail-open bug this whole
/// slice exists to remove.
#[test]
fn asking_for_the_report_changes_nothing_in_the_data_root() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    // A journal whose lock is held, so the report answers the question that is most tempting to
    // settle by taking the lock itself.
    let locks = agora_core::lock_manager::LockManager::new(paths.locks_root());
    let held = locks
        .acquire(
            agora_core::lock_manager::LockResource::PluginInstall,
            "test",
        )
        .unwrap();

    let before = snapshot(paths.root());
    let report = report(&paths);
    let after = snapshot(paths.root());

    drop(held);
    let differences: Vec<String> = changed(&before, &after)
        .into_iter()
        .filter(|line| !is_sqlite_sidecar(line))
        .collect();
    assert_eq!(
        differences,
        Vec::<String>::new(),
        "the report wrote, created, renamed or deleted something in the data root"
    );
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
}

/// Whether a difference is SQLite adding one of its own `-shm` / `-wal` sidecars.
fn is_sqlite_sidecar(line: &str) -> bool {
    match line.strip_prefix("added: ") {
        Some(path) => path.ends_with("local_state.db-shm") || path.ends_with("local_state.db-wal"),
        None => false,
    }
}

/// What differs between two snapshots, as names only — a snapshot of a database is not readable in
/// a failure message.
fn changed(before: &[(String, u64, Vec<u8>)], after: &[(String, u64, Vec<u8>)]) -> Vec<String> {
    let mut out = Vec::new();
    let before: std::collections::BTreeMap<_, _> = before
        .iter()
        .map(|(p, l, b)| (p.clone(), (*l, b.clone())))
        .collect();
    let after: std::collections::BTreeMap<_, _> = after
        .iter()
        .map(|(p, l, b)| (p.clone(), (*l, b.clone())))
        .collect();
    for (path, value) in &before {
        match after.get(path) {
            Some(other) if other == value => {}
            Some(_) => out.push(format!("changed: {path}")),
            None => out.push(format!("removed: {path}")),
        }
    }
    for path in after.keys() {
        if !before.contains_key(path) {
            out.push(format!("added: {path}"));
        }
    }
    out
}
