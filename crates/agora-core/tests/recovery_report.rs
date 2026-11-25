//! `agora doctor`'s read-only recovery report (MASTER_SPEC §32.11, the last bullet of that section).
//!
//! The report explains a recovery that blocks startup: one entry per journal found, with its kind,
//! the resource it covers, its state, and the decision recovery would take. It must not change
//! anything to answer — no lock taken, no file written, renamed or deleted, no lease cleared, no
//! database migration, no WAL switch.
//!
//! Every plugin journal here is built the way the real writer builds it: an install cut short at one
//! of its own crash points, in the real local state database. The markers for the other kinds are
//! written in the shape their real writers write them (the field names are copied from
//! `game_instance/trash.rs` and the lock resources from the kinds' own journals).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agora_core::app_paths::AppPaths;
use agora_core::ctx::CoreContext;
use agora_core::lock_manager::{LockManager, LockResource};
use agora_core::plugins::install::{InstallCrashPoint, InstallRecovery, InstallResolution};
use agora_core::plugins::PluginService;
use agora_core::process_identity::{Lookup, ProcessView};
use agora_core::recovery_report::{
    report, report_with, JournalEntry, JournalKind, RecoveryOutcome, RecoveryReport,
};
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

fn entries_of(report: &RecoveryReport, kind: JournalKind) -> Vec<&JournalEntry> {
    report
        .entries
        .iter()
        .filter(|entry| entry.kind == kind)
        .collect()
}

fn entry(report: &RecoveryReport, kind: JournalKind) -> &JournalEntry {
    let mut found = entries_of(report, kind);
    assert_eq!(
        found.len(),
        1,
        "the report lists exactly one {kind:?} journal, it lists {:?}",
        report.entries
    );
    found.remove(0)
}

fn not_searched_reason(report: &RecoveryReport, kind: JournalKind) -> String {
    report
        .not_searched
        .iter()
        .find(|missing| missing.kind == kind)
        .map(|missing| missing.reason.clone())
        .unwrap_or_else(|| {
            panic!(
                "the {kind:?} kind is reported as not searched, the report says {:?}",
                report.not_searched
            )
        })
}

/// The plugin's package folder and the stash a previous version is moved to.
fn plugin_paths(paths: &AppPaths) -> (PathBuf, PathBuf) {
    (
        paths.plugin_packages_root().join(PLUGIN),
        paths.plugin_rollback_root().join(PLUGIN),
    )
}

/// The ownership marker the journal's operation wrote, wherever it put it.
fn marker_files(paths: &AppPaths) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(paths.plugin_rollback_root())
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default();
    found.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".owner-"))
    });
    found.sort();
    found
}

/// A process table that answers "could not tell" for every pid, which is what a locked-down machine
/// or an OS that refuses a targeted lookup can answer.
struct CantTellProcesses;

impl ProcessView for CantTellProcesses {
    fn lookup(&self, pid: u32) -> Lookup {
        Lookup::CantTell(format!("pid {pid} could not be looked up (this test)"))
    }

    fn job_members(&self, _job: &str) -> Result<usize, String> {
        Err("no job was recorded (this test)".to_string())
    }
}

// ---------------------------------------------------------------------------
// Plugin installs: the prediction a start must live up to
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
        entry.reason.contains("would put the previous package back"),
        "the report says what it would do, in that state's words: {entry:?}"
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
    assert!(
        entry.reason.contains("keep the version that is installed"),
        "{entry:?}"
    );
}

/// A committed uninstall has no package to keep: it finishes by removing the files, and says so.
#[test]
fn a_committed_uninstall_says_it_finishes_removing_the_files() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    install_version(&paths, "1.0.0");
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let id = agora_plugin_api::PluginId::parse(PLUGIN).unwrap();
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    ctx.test_seams
        .set_install_crash(Some(InstallCrashPoint::UninstallCommitted));
    let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        service.uninstall(&id, false).unwrap();
    }));
    ctx.test_seams.set_install_crash(None);
    assert!(crash.is_err(), "the uninstall was cut short at its commit");
    set_writer(
        &paths,
        r#"{"pid":4294967293,"start_time":1,"expected_exe":null}"#,
    );

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::WouldFinish, "{entry:?}");
    assert!(
        entry.reason.contains("finish removing the plugin's files"),
        "a committed uninstall does not keep a version, it removes files: {entry:?}"
    );
}

/// The journal cannot say whether its writer still runs: the report says "busy", never "would act".
#[test]
fn a_liveness_answer_of_cannot_tell_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);

    let report = report_with(&paths, &CantTellProcesses);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
    assert!(
        entry.reason.contains("may still be running")
            || entry.reason.contains("may still be going"),
        "busy says why: {entry:?}"
    );
    assert_eq!(
        entry.command, None,
        "a busy journal has no command: the next start retries it"
    );
}

/// A journal whose writer is still running is busy, never "would act": the operation may still be
/// going. The writer here is a real live process — the one that wrote the journal.
#[test]
fn a_journal_whose_writer_is_alive_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
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
    assert_eq!(entry.command, None);
}

/// The journal's ownership marker was removed, so the folders cannot be proven to be its own:
/// startup refuses to touch them, and the report must say the same thing rather than promise a
/// rollback the start would never perform.
#[test]
fn an_install_whose_marker_was_removed_needs_a_hand_with_startups_reason() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    let markers = marker_files(&paths);
    assert!(
        !markers.is_empty(),
        "the real install wrote an ownership marker"
    );
    for marker in &markers {
        std::fs::remove_file(marker).unwrap();
    }

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert!(
        entry.reason.contains("could not be verified"),
        "the reason is the one startup gives: {entry:?}"
    );

    // And that is what a start really does: the same refusal, naming the same marker.
    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    let id = agora_plugin_api::PluginId::parse(PLUGIN).unwrap();
    let outcomes = service
        .recover_interrupted_installs(Some(&id), InstallResolution::Restore)
        .unwrap();
    let startup = match outcomes.as_slice() {
        [(_, InstallRecovery::NeedsHand(message))] => message.clone(),
        other => panic!("startup refuses this journal as needing a hand, it said {other:?}"),
    };
    assert!(
        startup.contains("could not be verified"),
        "startup's message: {startup}"
    );
    let startup_reason = startup
        .split(" needs a hand: ")
        .nth(1)
        .and_then(|rest| rest.split(" Run `").next())
        .unwrap_or_default()
        .trim_end_matches('.');
    assert_eq!(
        entry.reason.trim_end_matches('.'),
        startup_reason,
        "the report repeats startup's reason word for word:\nreport: {}\nstartup: {startup}",
        entry.reason
    );
}

/// The previous package the rollback would put back is gone: startup refuses, and the report says
/// the same. This is a check that only `apply` used to run, so the report used to promise a
/// rollback that could not happen.
#[test]
fn an_install_whose_previous_package_is_missing_needs_a_hand() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    let (destination, stash) = plugin_paths(&paths);
    for path in [&destination, &stash] {
        if path.exists() {
            std::fs::remove_dir_all(path).unwrap();
        }
    }

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert!(
        entry.reason.contains("the previous package is missing"),
        "the reason is startup's: {entry:?}"
    );

    let ctx = CoreContext::for_testing(paths.root().to_path_buf());
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    let id = agora_plugin_api::PluginId::parse(PLUGIN).unwrap();
    let outcomes = service
        .recover_interrupted_installs(Some(&id), InstallResolution::Restore)
        .unwrap();
    match outcomes.as_slice() {
        [(_, InstallRecovery::NeedsHand(message))] => assert!(
            message.contains("the previous package is missing"),
            "startup's message: {message}"
        ),
        other => panic!("startup refuses this journal too, it said {other:?}"),
    }
}

/// A committed install whose package folder is gone cannot be finished by cleanup: startup refuses
/// it, and the report says so instead of "would finish".
#[test]
fn an_install_whose_committed_package_is_missing_needs_a_hand() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Committed);
    let (destination, _) = plugin_paths(&paths);
    std::fs::remove_dir_all(&destination).unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert!(
        entry
            .reason
            .contains("the committed package directory is missing"),
        "{entry:?}"
    );
}

// ---------------------------------------------------------------------------
// Launcher imports: their facts, and no second copy of their recovery
// ---------------------------------------------------------------------------

fn import_job(paths: &AppPaths, job_id: &str, instance_id: &str, state: &str) -> String {
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
        state: state.into(),
        plan_fingerprint: "fingerprint".into(),
        error: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    agora_core::db::upsert_instance_import_job(&conn, &job).unwrap();
    job.staging_path
}

/// An import is reported with its facts — which instance, which phase, which folders — and with no
/// prediction at all: this build re-implements no part of that kind's recovery.
#[test]
fn an_import_is_listed_with_its_facts_and_no_prediction() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    let staging = import_job(&paths, "job-one", "imported-one", "staging");

    let report = report(&paths);
    let entry = entry(&report, JournalKind::InstanceImport);
    assert_eq!(entry.resource, "imported-one");
    assert_eq!(entry.state.as_deref(), Some("staging"));
    assert_eq!(entry.path, staging);
    assert_eq!(entry.outcome, RecoveryOutcome::Undecided, "{entry:?}");
    assert!(
        entry.reason.contains("The next start settles it"),
        "the report says who decides, and that nothing was changed: {entry:?}"
    );
    assert_eq!(entry.command, None);
}

/// A running import holds the instance's lock for its whole life, so a held lock means the import
/// may still be running: busy, not "undecided".
#[test]
fn an_import_whose_instance_lock_is_held_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    import_job(&paths, "job-one", "imported-one", "staging");
    let locks = LockManager::new(paths.locks_root());
    let held = locks
        .acquire(LockResource::Instance("imported-one".to_string()), "import")
        .unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::InstanceImport);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
    assert_eq!(entry.command, None);
    drop(held);
}

/// One unreadable row is that row's answer; the other rows are still listed, and the kind is not
/// called "not searched".
#[test]
fn one_unreadable_import_row_does_not_hide_the_others() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    import_job(&paths, "job-one", "imported-one", "staging");
    import_job(&paths, "job-two", "imported-two", "staging");
    {
        // A value this build cannot read as text: the row is a journal it cannot parse, which is
        // reported as that row rather than as a listing failure.
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute(
            "UPDATE instance_import_jobs SET staging_path = x'00ff' WHERE job_id = 'job-two'",
            [],
        )
        .unwrap();
    }

    let report = report(&paths);
    assert!(
        report
            .not_searched
            .iter()
            .all(|missing| missing.kind != JournalKind::InstanceImport),
        "one bad row does not make the kind unsearchable: {:?}",
        report.not_searched
    );
    let imports = entries_of(&report, JournalKind::InstanceImport);
    assert_eq!(imports.len(), 2, "{imports:?}");
    let good = imports
        .iter()
        .find(|entry| entry.resource == "imported-one")
        .expect("the readable row is still listed");
    assert_eq!(good.outcome, RecoveryOutcome::Undecided, "{good:?}");
    let bad = imports
        .iter()
        .find(|entry| entry.resource == "imported-two" || entry.resource.contains("cannot be read"))
        .expect("the unreadable row is listed as itself");
    assert_eq!(bad.outcome, RecoveryOutcome::NeedsHand, "{bad:?}");
    assert!(
        bad.reason.contains("cannot be read"),
        "the row's own error is reported: {bad:?}"
    );
}

/// An error reading one kind's journals must not mark another kind unsearched, and the other way
/// round.
#[test]
fn an_error_in_one_kind_does_not_mark_the_other_kinds_unsearched() {
    // The plugin-install table is gone; the import rows are fine.
    let (_dir, paths) = data_root();
    init_database(&paths);
    import_job(&paths, "job-one", "imported-one", "staging");
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute("DROP TABLE plugin_install_journal", [])
            .unwrap();
    }
    let without_plugin_table = report(&paths);
    assert!(
        not_searched_reason(&without_plugin_table, JournalKind::PluginInstall)
            .contains("could not be listed"),
        "{:?}",
        without_plugin_table.not_searched
    );
    assert_eq!(
        entry(&without_plugin_table, JournalKind::InstanceImport).outcome,
        RecoveryOutcome::Undecided,
        "the import journals are still read: {without_plugin_table:?}"
    );

    // And the other way round: the import table is gone, the plugin journal is fine.
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute("DROP TABLE instance_import_jobs", []).unwrap();
    }
    let without_import_table = report(&paths);
    assert!(
        not_searched_reason(&without_import_table, JournalKind::InstanceImport)
            .contains("could not be read"),
        "{:?}",
        without_import_table.not_searched
    );
    assert_eq!(
        entry(&without_import_table, JournalKind::PluginInstall).outcome,
        RecoveryOutcome::WouldUndo,
        "the plugin journal is still read: {without_import_table:?}"
    );
}

// ---------------------------------------------------------------------------
// Kinds that are files
// ---------------------------------------------------------------------------

/// A journal path that is a folder is not something to skip: it is a journal that cannot be read,
/// named with the path and the reason.
#[test]
fn a_journal_path_that_is_a_folder_is_listed_as_unreadable() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(swaps.join("skyrim-se_steam.json")).unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::ToolSwap);
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert_eq!(
        entry.path,
        swaps.join("skyrim-se_steam.json").display().to_string()
    );
    assert!(
        entry.reason.contains("is a folder, not a journal file"),
        "the reason says what is there: {entry:?}"
    );
}

/// A tool swap whose game is being run holds the game/store lock: busy, with no command, because
/// the next start retries it once the run is over.
#[test]
fn a_journal_whose_game_lock_is_held_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(&swaps).unwrap();
    let journal = swaps.join("skyrim-se_steam.json");
    std::fs::write(
        &journal,
        serde_json::json!({ "state": "installing", "game": "skyrim-se", "store": "steam" })
            .to_string(),
    )
    .unwrap();
    let locks = LockManager::new(paths.locks_root());
    let resource = LockResource::GameUserFiles(
        agora_game_api::GameId::new("skyrim-se").unwrap(),
        agora_game_api::StoreId::new("steam").unwrap(),
    );
    let held = locks.acquire(resource, "launch").unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::ToolSwap);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
    assert_eq!(entry.state.as_deref(), Some("installing"));
    assert_eq!(
        entry.command, None,
        "busy has no command: the run has to end first"
    );
    drop(held);
}

/// A lock that cannot be asked about may still be held, which is busy — never "undecided".
#[test]
fn a_lock_that_cannot_be_asked_about_is_busy() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    let swaps = paths.tool_swaps_root();
    std::fs::create_dir_all(&swaps).unwrap();
    std::fs::write(
        swaps.join("skyrim-se_steam.json"),
        serde_json::json!({ "state": "installing", "game": "skyrim-se", "store": "steam" })
            .to_string(),
    )
    .unwrap();
    // A lock directory that cannot be listed is a lock nobody can read.
    let locks = LockManager::new(paths.locks_root());
    let held = locks
        .acquire(
            LockResource::GameUserFiles(
                agora_game_api::GameId::new("skyrim-se").unwrap(),
                agora_game_api::StoreId::new("steam").unwrap(),
            ),
            "launch",
        )
        .unwrap();
    let lock_file = held.path().to_path_buf();
    drop(held);
    // Replace the file by a folder: the probe cannot read it, so it cannot say it is free.
    std::fs::remove_file(&lock_file).unwrap();
    std::fs::create_dir_all(&lock_file).unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::ToolSwap);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
    assert_eq!(entry.command, None);
}

// ---------------------------------------------------------------------------
// The local database
// ---------------------------------------------------------------------------

/// No database yet: the two kinds that live in it are "not searched", with the reason, and every
/// journal that is a file is still listed.
#[test]
fn no_database_yet_is_not_searched_and_a_legacy_stash_is_still_listed() {
    let (_dir, paths) = data_root();
    let legacy = paths.plugin_rollback_root().join("test.legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("agora-plugin.json"), "{}").unwrap();

    let report = report(&paths);
    assert!(
        !paths.local_state_db().exists(),
        "asking for the report must not create the database"
    );
    for kind in [JournalKind::PluginInstall, JournalKind::InstanceImport] {
        let reason = not_searched_reason(&report, kind);
        assert!(
            reason.contains("No local database yet"),
            "the reason says why: {reason}"
        );
    }
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.resource, "test.legacy");
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert_eq!(
        entry.command.as_deref(),
        Some("agora plugin recover test.legacy --keep-aside --yes"),
        "the way out of a journal no database knows is --keep-aside"
    );
}

/// A database this build does not know is "could not tell", with the reason — never "no journals".
#[test]
fn a_newer_database_schema_reports_both_database_kinds_as_cannot_tell() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (999)", [])
            .unwrap();
    }

    let report = report(&paths);
    let reason = not_searched_reason(&report, JournalKind::PluginInstall);
    assert!(
        reason.contains("newer"),
        "the reason names the newer schema: {reason}"
    );
    let import = not_searched_reason(&report, JournalKind::InstanceImport);
    assert!(
        import.contains("newer"),
        "the import journals come from the same file: {import}"
    );
}

/// A database recovery marker with the database moved aside is a startup blocker in its own right,
/// and the report must not say the next start will create the database.
#[test]
fn a_pending_database_recovery_is_listed_as_something_the_next_start_finishes() {
    let (_dir, paths) = data_root();
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

    let report = report(&paths);
    let entry = entry(&report, JournalKind::DatabaseRecovery);
    assert_eq!(entry.outcome, RecoveryOutcome::WouldFinish, "{entry:?}");
    assert_eq!(
        entry.path,
        db.with_file_name("local_state.db.recovery")
            .display()
            .to_string()
    );
    assert!(
        entry.reason.contains("finishes the restore"),
        "it says what the next start does: {entry:?}"
    );
    assert!(
        report
            .notes
            .iter()
            .all(|note| !note.contains("No local database yet")),
        "a recovery marker means the database was moved aside, not that there has never been one: \
         {:?}",
        report.notes
    );
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("recovery marker")),
        "the missing database is explained by the marker: {:?}",
        report.notes
    );
}

// ---------------------------------------------------------------------------
// Recently deleted
// ---------------------------------------------------------------------------

/// Write the two files a trash entry is made of, in the shape its real writer writes them.
fn trash_entry(paths: &AppPaths, entry_id: &str, restoring: Option<(&str, &str)>) {
    let dir = paths.recently_deleted_games_root().join(entry_id);
    std::fs::create_dir_all(dir.join("instance")).unwrap();
    let entry = serde_json::json!({
        "version": 1,
        "entryId": entry_id,
        "instanceId": "deleted-one",
        "game": "minecraft",
        "name": "A Deleted World",
        "deletedAt": "2026-01-01T00:00:00Z",
        "expiresAt": "2026-02-01T00:00:00Z",
        "sizeBytes": 1024,
    });
    std::fs::write(dir.join("entry.json"), entry.to_string()).unwrap();
    if let Some((new_id, new_name)) = restoring {
        let restoring = serde_json::json!({ "new_id": new_id, "new_name": new_name });
        std::fs::write(dir.join("restoring.json"), restoring.to_string()).unwrap();
    }
}

/// A restore that a crash cut in half is a startup blocker: the next start finishes it.
#[test]
fn an_interrupted_trash_restore_is_listed_as_something_the_next_start_finishes() {
    let (_dir, paths) = data_root();
    trash_entry(&paths, "entry-1", Some(("deleted-one", "A Deleted World")));

    let report = report(&paths);
    let entry = entry(&report, JournalKind::RecentlyDeleted);
    assert_eq!(entry.resource, "deleted-one");
    assert_eq!(entry.state.as_deref(), Some("restoring"));
    assert_eq!(entry.outcome, RecoveryOutcome::WouldFinish, "{entry:?}");
    assert!(
        entry.reason.contains("the next start finishes it"),
        "{entry:?}"
    );
}

/// An entry that is merely kept has nothing pending, and says how to bring it back.
#[test]
fn a_kept_trash_entry_has_nothing_pending_and_names_the_restore_command() {
    let (_dir, paths) = data_root();
    trash_entry(&paths, "entry-1", None);

    let report = report(&paths);
    let entry = entry(&report, JournalKind::RecentlyDeleted);
    assert_eq!(entry.resource, "deleted-one");
    assert_eq!(entry.outcome, RecoveryOutcome::NothingToDo, "{entry:?}");
    assert!(
        entry.reason.contains("agora instance restore entry-1"),
        "{entry:?}"
    );
    assert!(!report.needs_a_hand(), "{report:?}");
}

/// An entry whose own record cannot be read is reported as unreadable, with the error: the files are
/// there, but nothing can be told about them and no restore can bring them back.
#[test]
fn a_trash_entry_whose_record_is_not_json_is_listed_as_unreadable() {
    let (_dir, paths) = data_root();
    trash_entry(&paths, "entry-1", None);
    std::fs::write(
        paths
            .recently_deleted_games_root()
            .join("entry-1/entry.json"),
        b"{ broken",
    )
    .unwrap();

    let report = report(&paths);
    let entry = entry(&report, JournalKind::RecentlyDeleted);
    assert_eq!(entry.resource, "entry-1", "the folder names the entry");
    assert_eq!(entry.outcome, RecoveryOutcome::NeedsHand, "{entry:?}");
    assert!(
        entry.reason.contains("record cannot be read"),
        "the reason carries the parse error: {entry:?}"
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
    import_job(&paths, "job-one", "imported-one", "staging");
    trash_entry(&paths, "entry-1", Some(("deleted-one", "A Deleted World")));

    let before = snapshot(paths.root());
    let report = report(&paths);
    let after = snapshot(paths.root());

    let differences: Vec<String> = changed(&before, &after)
        .into_iter()
        .filter(|line| !is_sqlite_sidecar(line))
        .collect();
    assert_eq!(
        differences,
        Vec::<String>::new(),
        "the report wrote, created, renamed or deleted something in the data root"
    );
    assert_eq!(
        entry(&report, JournalKind::PluginInstall).outcome,
        RecoveryOutcome::WouldUndo
    );
}

/// A journal whose lock is held is busy, and asking about it neither takes the lock nor touches the
/// lock file: the report says an operation may still be running and leaves it alone.
#[test]
fn a_journal_whose_lock_is_held_is_busy_and_the_lock_is_left_alone() {
    let (_dir, paths) = data_root();
    init_database(&paths);
    interrupted_install(&paths, InstallCrashPoint::Extracted);
    let locks = LockManager::new(paths.locks_root());
    let held = locks.acquire(LockResource::PluginInstall, "test").unwrap();

    let lock_file = held.path().to_path_buf();
    let lock_bytes = std::fs::read(&lock_file).unwrap();
    let before = snapshot(paths.root());
    let report = report(&paths);
    let after = snapshot(paths.root());

    let differences: Vec<String> = changed(&before, &after)
        .into_iter()
        .filter(|line| !is_sqlite_sidecar(line))
        .collect();
    assert_eq!(
        differences,
        Vec::<String>::new(),
        "asking about a held lock must not take it or rewrite it"
    );
    assert_eq!(
        std::fs::read(&lock_file).ok(),
        Some(lock_bytes),
        "the live holder's owner record is untouched"
    );
    drop(held);
    let entry = entry(&report, JournalKind::PluginInstall);
    assert_eq!(entry.outcome, RecoveryOutcome::Busy, "{entry:?}");
    assert_eq!(entry.command, None);
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
