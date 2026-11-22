//! Safety net for `local_state.db`: ordered migrations, a pre-migration
//! backup, an integrity check, recovery instead of a fatal error, and a
//! schema-too-new refusal. Each test builds the database the way an earlier
//! release wrote it, then opens it through the real startup path.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agora_core::db::{
    init_local_state_db, init_local_state_db_with, local_state_connection, run_migrations,
    run_migrations_to, CorruptRecovery, LocalStateOpenOptions, ERR_LOCAL_STATE_BACKUP_FAILED,
    ERR_LOCAL_STATE_NEWER, LOCAL_STATE_BACKUP_RETENTION, LOCAL_STATE_SCHEMA_VERSION,
};
use agora_core::lock_manager::{LockManager, LockResource};
use rusqlite::Connection;

fn database_at(dir: &Path, name: &str, version: i64) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    run_migrations_to(&conn, version).unwrap();
    drop(conn);
    path
}

fn set_setting(path: &Path, key: &str, value_json: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO user_settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
        rusqlite::params![key, value_json],
    )
    .unwrap();
}

fn get_setting(path: &Path, key: &str) -> Option<String> {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT value_json FROM user_settings WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .ok()
}

fn schema_version(path: &Path) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn backups(dir: &Path) -> Vec<PathBuf> {
    let dir = dir.join("backups");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("local_state-") && name.ends_with(".db"))
        })
        .collect();
    files.sort();
    files
}

fn error_code(error: &anyhow::Error) -> Option<String> {
    error
        .downcast_ref::<agora_core::error::LauncherError>()
        .map(|error| error.code())
}

/// Mirrors the marker `db.rs` writes before moving a corrupt database aside.
fn recovery_marker(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.recovery",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local_state.db")
    ))
}

fn identity_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.identity",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local_state.db")
    ))
}

/// Mirrors the identity sidecar and marker the writer leaves for a genuinely interrupted restore.
fn write_valid_recovery_marker(path: &Path) {
    std::fs::write(identity_path(path), b"test-identity").unwrap();
    std::fs::write(
        recovery_marker(path),
        serde_json::json!({
            "identity": "test-identity",
            "db_path": path.to_string_lossy(),
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn the_v4_and_v6_columns_are_really_added() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 3);

    init_local_state_db(&path).unwrap();

    let conn = Connection::open(&path).unwrap();
    let mut statement = conn
        .prepare("SELECT name FROM pragma_table_info('user_instances')")
        .unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for expected in [
        "java_path",
        "java_incompatible_override",
        "icon_path",
        "launch_mode_override",
        "jvm_memory_mode",
    ] {
        assert!(
            columns.iter().any(|column| column == expected),
            "{expected} was not added; columns: {columns:?}"
        );
    }
}

#[test]
fn no_alter_runs_at_a_start_with_nothing_pending() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);

    {
        // Make any attempted ALTER on this table fail: `user_instances` is now a
        // view without the pre-touch column, so v28's conditional ALTER would
        // see it missing and error. A start with nothing pending must not run it.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE user_instances;
             CREATE VIEW user_instances AS SELECT 1 AS instance_id;",
        )
        .unwrap();
    }

    run_migrations(&local_state_connection(&path).unwrap())
        .expect("nothing pending means no step and no ALTER");
    init_local_state_db(&path).expect("startup with nothing pending succeeds");

    let conn = Connection::open(&path).unwrap();
    let is_view: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'view' AND name = 'user_instances'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(is_view, 1, "the view survived, so no ALTER touched it");
}

#[test]
fn a_backup_is_made_only_when_a_step_is_pending_and_retention_keeps_three() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    assert!(
        report.backup_path.is_some(),
        "a pending step must be preceded by a backup"
    );
    assert_eq!(backups(dir.path()).len(), 1);

    // A second start has nothing pending: no new backup.
    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    assert!(report.backup_path.is_none());
    assert_eq!(backups(dir.path()).len(), 1);

    // Four other old databases in the same folder exercise retention.
    for n in 0..4 {
        let other = database_at(dir.path(), &format!("other-{n}.db"), 27);
        init_local_state_db_with(&other, &LocalStateOpenOptions::default()).unwrap();
    }
    let kept = backups(dir.path());
    assert_eq!(
        kept.len(),
        LOCAL_STATE_BACKUP_RETENTION,
        "retention keeps exactly {LOCAL_STATE_BACKUP_RETENTION} backups"
    );
    assert_eq!(
        LOCAL_STATE_BACKUP_RETENTION, 3,
        "the brief names three backups"
    );
}

#[test]
fn a_corrupt_file_is_restored_from_the_newest_good_backup_and_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);
    set_setting(&path, "safety_probe", "\"kept\"");

    // A good backup from the same schema, written where the recovery path looks.
    let backup_dir = dir.path().join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup_path = backup_dir.join("local_state-20000101-000000-000-0000-v27.db");
    std::fs::copy(&path, &backup_path).unwrap();

    // Corrupt the live file without deleting it.
    let garbage = b"this is not a sqlite database";
    std::fs::write(&path, garbage).unwrap();

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    let recovery = report
        .recovered_corrupt
        .as_ref()
        .expect("a corrupt file is recovered");
    assert_eq!(
        std::fs::read(&recovery.kept_at).unwrap(),
        garbage,
        "the corrupt file is kept exactly as it was"
    );
    assert_eq!(
        recovery.restored_from.as_deref(),
        Some(backup_path.as_path())
    );
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"kept\"")
    );
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
}

/// Two startups racing the `.recovery` restore. A second process must wait for the first
/// instead of running its own backup restore, which would delete the first process's WAL/SHM
/// and rename a fresh database over its now-live one.
#[test]
fn two_startups_cannot_race_the_database_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);
    set_setting(&path, "safety_probe", "\"kept\"");
    let backup_dir = dir.path().join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup = backup_dir.join("local_state-20000101-000000-000-0000-v28.db");
    std::fs::copy(&path, &backup).unwrap();
    std::fs::write(&path, b"this is not a sqlite database").unwrap();

    // Another process holds the database recovery lock while it restores.
    let other_process = LockManager::new(dir.path().join("locks"));
    let held = other_process
        .acquire(LockResource::LocalStateDatabase, "other-startup")
        .expect("the first startup holds the lock");

    let (tx, rx) = mpsc::channel();
    let other_path = path.clone();
    let handle = thread::spawn(move || {
        let result = init_local_state_db_with(&other_path, &LocalStateOpenOptions::default());
        tx.send(result).unwrap();
    });

    assert!(
        rx.recv_timeout(Duration::from_secs(3)).is_err(),
        "the second startup ran while another held the database recovery lock"
    );

    drop(held);
    let report = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the second startup finished once the lock was free")
        .expect("startup succeeded");
    handle.join().unwrap();
    assert!(
        report.recovered_corrupt.is_some(),
        "one recovery was reported"
    );
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"kept\"")
    );
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
}

/// A marker copied from another data root is foreign. Even beside a healthy database it must be
/// reported and kept, not silently cleared.
#[test]
fn a_foreign_recovery_marker_is_reported_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);
    set_setting(&path, "safety_probe", "\"kept\"");
    std::fs::write(identity_path(&path), b"local-identity").unwrap();
    std::fs::write(
        recovery_marker(&path),
        serde_json::json!({
            "identity": "copied-from-elsewhere",
            "db_path": path.to_string_lossy(),
        })
        .to_string(),
    )
    .unwrap();

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();

    assert!(report.recovered_corrupt.is_none());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("not from this database")),
        "the foreign marker is reported: {:?}",
        report.warnings
    );
    assert!(
        recovery_marker(&path).exists(),
        "the foreign marker was kept"
    );
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"kept\"")
    );
}

/// With the database missing, a marker that does not match this database's identity must not
/// trigger a local backup restore.
#[test]
fn a_recovery_marker_without_a_matching_identity_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local_state.db");
    let backup_dir = dir.path().join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let good = database_at(dir.path(), "backup-source.db", LOCAL_STATE_SCHEMA_VERSION);
    set_setting(&good, "safety_probe", "\"kept\"");
    std::fs::copy(
        &good,
        backup_dir.join("local_state-20000101-000000-000-0000-v28.db"),
    )
    .unwrap();
    std::fs::write(
        recovery_marker(&path),
        serde_json::json!({
            "identity": "foreign",
            "db_path": "/somewhere/else/local_state.db",
        })
        .to_string(),
    )
    .unwrap();

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();

    assert!(
        report.recovered_corrupt.is_none(),
        "no restore was driven by a foreign marker"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("not from this database")),
        "the foreign marker is reported: {:?}",
        report.warnings
    );
    assert!(recovery_marker(&path).exists(), "the marker was kept");
    assert_eq!(
        get_setting(&path, "safety_probe"),
        None,
        "no backed-up setting was restored"
    );
}

/// Two real recoveries starting together: the cross-process lock serialises them, so exactly one
/// restores and the other opens the finished database.
#[test]
fn two_concurrent_recoveries_restore_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);
    set_setting(&path, "safety_probe", "\"kept\"");
    let backup_dir = dir.path().join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    std::fs::copy(
        &path,
        backup_dir.join("local_state-20000101-000000-000-0000-v28.db"),
    )
    .unwrap();
    std::fs::write(&path, b"this is not a sqlite database").unwrap();

    // A test hook holds the first initializer inside the critical section. The second cannot
    // finish until it is released, whatever the scheduler does — no shared start barrier. The
    // hook is handed to the first initializer only, and fires once: it is passed in with the
    // startup choices, so it is this open's hook and no other's.
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    // The hook may be shared, so its receiver sits behind a lock.
    let release_rx = std::sync::Mutex::new(release_rx);
    let once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let first_options = LocalStateOpenOptions {
        startup_lock_hook: Some(agora_core::db::StartupLockHook::new(move |_path: &Path| {
            if !once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(Duration::from_secs(30))
                .expect("the test releases the first initializer");
        })),
        ..LocalStateOpenOptions::default()
    };

    let first_path = path.clone();
    let first =
        thread::spawn(move || init_local_state_db_with(&first_path, &first_options).unwrap());
    entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the first initializer entered the critical section");

    let (done_tx, done_rx) = mpsc::channel();
    let second_path = path.clone();
    let second = thread::spawn(move || {
        let report = init_local_state_db_with(&second_path, &LocalStateOpenOptions::default())
            .expect("the second initializer succeeds");
        done_tx.send(report).unwrap();
    });
    let second_finished_early = done_rx.recv_timeout(Duration::from_secs(3)).is_ok();

    // Release before asserting so a failing control does not leave the first thread waiting on a
    // receiver the test has already dropped.
    release_tx.send(()).unwrap();
    assert!(
        !second_finished_early,
        "the second initializer finished while the first held the startup lock"
    );
    let first_report = first.join().unwrap();
    let second_report = done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the second initializer finished once the lock was free");
    second.join().unwrap();

    assert_eq!(
        usize::from(first_report.recovered_corrupt.is_some())
            + usize::from(second_report.recovered_corrupt.is_some()),
        1,
        "exactly one of the two starts restored the database"
    );
    assert!(
        first_report.warnings.is_empty() && second_report.warnings.is_empty(),
        "no ambiguous state was left behind"
    );
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"kept\"")
    );
}

#[test]
fn corrupt_with_no_backup_gives_a_fresh_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local_state.db");
    let garbage = b"still not a database";
    std::fs::write(&path, garbage).unwrap();

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    let CorruptRecovery {
        kept_at,
        restored_from,
    } = report.recovered_corrupt.expect("corruption is reported");
    assert!(restored_from.is_none());
    assert_eq!(std::fs::read(&kept_at).unwrap(), garbage);
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
}

#[test]
fn a_failed_recovery_is_retried_on_the_next_launch() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);
    set_setting(&path, "safety_probe", "\"kept\"");

    let backup_dir = dir.path().join("backups");
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup_path = backup_dir.join("local_state-20000101-000000-000-0000-v27.db");
    std::fs::copy(&path, &backup_path).unwrap();

    std::fs::write(&path, b"this is not a sqlite database").unwrap();

    // Make the backup unreadable for the first launch: a file where the backups
    // folder should be makes reading the folder fail.
    let hidden = dir.path().join("backups-hidden");
    std::fs::rename(&backup_dir, &hidden).unwrap();
    std::fs::write(&backup_dir, b"not a folder").unwrap();

    let error = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap_err();
    assert!(
        !path.exists(),
        "the failed launch left no database at the live path"
    );
    assert!(
        recovery_marker(&path).exists(),
        "the recovery marker survives the failed launch"
    );
    let _ = error;

    // The next launch finds the backups folder readable again.
    std::fs::remove_file(&backup_dir).unwrap();
    std::fs::rename(&hidden, &backup_dir).unwrap();

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    let recovery = report
        .recovered_corrupt
        .as_ref()
        .expect("the retried recovery is reported");
    assert_eq!(
        recovery.restored_from.as_deref(),
        Some(backup_path.as_path()),
        "the backup that became readable is the one restored"
    );
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"kept\""),
        "the setting written before the corruption survives"
    );
    assert!(
        !recovery_marker(&path).exists(),
        "the marker is cleared once recovery succeeds"
    );
}

#[test]
fn a_recovery_marker_with_no_backup_starts_fresh_and_keeps_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local_state.db");
    let garbage = b"still not a database";
    std::fs::write(&path, garbage).unwrap();

    // Simulate the interrupted launch: the corrupt file was moved aside and the
    // marker was written, but no backup was ever readable.
    let kept_at = dir
        .path()
        .join("local_state.db.corrupt-20000101-000000-000-p1-0000");
    std::fs::rename(&path, &kept_at).unwrap();
    write_valid_recovery_marker(&path);

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    let CorruptRecovery {
        kept_at: reported,
        restored_from,
    } = report
        .recovered_corrupt
        .expect("the marker is reported as a recovery");
    assert!(
        restored_from.is_none(),
        "with no backup the report says a fresh start, not a restore"
    );
    assert_eq!(reported, kept_at);
    assert_eq!(
        std::fs::read(&kept_at).unwrap(),
        garbage,
        "the old file is kept"
    );
    assert!(
        !recovery_marker(&path).exists(),
        "the marker is cleared once recovery is decided"
    );
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
    assert_eq!(
        get_setting(&path, "safety_probe"),
        None,
        "a fresh database has no pre-corruption setting"
    );
}

#[test]
fn a_newer_version_is_refused_without_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);
    let conn = Connection::open(&path).unwrap();
    conn.execute("INSERT INTO schema_version (version) VALUES (999)", [])
        .unwrap();
    drop(conn);

    let error = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap_err();
    assert_eq!(error_code(&error).as_deref(), Some(ERR_LOCAL_STATE_NEWER));
    assert_eq!(schema_version(&path), 999, "the file was not touched");

    let report = init_local_state_db_with(
        &path,
        &LocalStateOpenOptions {
            accept_newer_schema: true,
            ..LocalStateOpenOptions::default()
        },
    )
    .unwrap();
    let kept = report
        .newer_schema_kept
        .as_ref()
        .expect("the override keeps the old file");
    assert_eq!(kept.found, 999);
    assert_eq!(kept.supported, LOCAL_STATE_SCHEMA_VERSION);
    assert!(kept.kept_at.exists(), "the newer database is kept");
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
}

#[test]
fn a_backup_failure_stops_migration_unless_explicitly_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);
    // A file where the backup folder must go makes VACUUM INTO fail.
    std::fs::write(dir.path().join("backups"), b"not a folder").unwrap();

    let error = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap_err();
    assert_eq!(
        error_code(&error).as_deref(),
        Some(ERR_LOCAL_STATE_BACKUP_FAILED)
    );
    assert_eq!(schema_version(&path), 27, "nothing was migrated");

    let report = init_local_state_db_with(
        &path,
        &LocalStateOpenOptions {
            accept_newer_schema: false,
            accept_missing_backup: true,
            ..LocalStateOpenOptions::default()
        },
    )
    .unwrap();
    assert!(report.backup_path.is_none());
    assert_eq!(schema_version(&path), LOCAL_STATE_SCHEMA_VERSION);
}

#[test]
fn a_database_written_by_todays_schema_migrates_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", 27);
    set_setting(&path, "safety_probe", "\"from-v27\"");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO user_instances (
                 instance_id, name, minecraft_version, loader, loader_version
             ) VALUES ('kept-instance', 'Kept', '1.21', 'vanilla', '')",
            [],
        )
        .unwrap();
    }

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default()).unwrap();
    assert_eq!(report.migrated_from, 27);
    assert_eq!(report.migrated_to, LOCAL_STATE_SCHEMA_VERSION);
    assert_eq!(
        get_setting(&path, "safety_probe").as_deref(),
        Some("\"from-v27\"")
    );
    let conn = Connection::open(&path).unwrap();
    let name: String = conn
        .query_row(
            "SELECT name FROM user_instances WHERE instance_id = 'kept-instance'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "Kept");
}

#[test]
#[ignore = "timing measurement, run once with --run-ignored"]
fn quick_check_time_on_a_realistic_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = database_at(dir.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);
    {
        let conn = Connection::open(&path).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO user_instances (
                 instance_id, name, minecraft_version, loader, loader_version
             ) VALUES ('instance-1', 'Instance', '1.21', 'fabric', '0.16')",
            [],
        )
        .unwrap();
        for n in 0..20_000 {
            tx.execute(
                "INSERT INTO launch_history (
                     instance_id, started_at, duration_ms, outcome, enabled_mod_count,
                     minecraft_version, loader
                 ) VALUES ('instance-1', ?1, ?2, 'ok', 120, '1.21', 'fabric')",
                rusqlite::params![format!("2026-01-01T00:00:{:02}Z", n % 60), n],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let started = std::time::Instant::now();
        let mut statement = conn.prepare("PRAGMA quick_check").unwrap();
        let mut rows = statement.query([]).unwrap();
        while rows.next().unwrap().is_some() {}
        eprintln!(
            "quick_check on 20,000 launch rows took {:?}",
            started.elapsed()
        );

        let started = std::time::Instant::now();
        let mut statement = conn.prepare("PRAGMA integrity_check").unwrap();
        let mut rows = statement.query([]).unwrap();
        while rows.next().unwrap().is_some() {}
        eprintln!(
            "integrity_check on 20,000 launch rows took {:?}",
            started.elapsed()
        );
    }
}

/// A startup-lock hook belongs to the database open it was given to: two hooks, on two databases,
/// each fire for their own open. (One process-wide slot could not describe two: the second
/// installer replaced the first, so the first database's hook never fired.)
#[test]
fn a_startup_lock_hook_belongs_to_the_database_it_was_given_to() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let path_a = database_at(dir_a.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);
    let path_b = database_at(dir_b.path(), "local_state.db", LOCAL_STATE_SCHEMA_VERSION);

    let fired_a = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fired_b = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let options_for = |fired: &std::sync::Arc<std::sync::atomic::AtomicUsize>| {
        let fired = fired.clone();
        LocalStateOpenOptions {
            startup_lock_hook: Some(agora_core::db::StartupLockHook::new(move |_path: &Path| {
                fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })),
            ..LocalStateOpenOptions::default()
        }
    };
    let options_a = options_for(&fired_a);
    let options_b = options_for(&fired_b);

    init_local_state_db_with(&path_a, &options_a).expect("a opens");
    init_local_state_db_with(&path_b, &options_b).expect("b opens");

    assert_eq!(
        fired_a.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the hook given to a never fired"
    );
    assert_eq!(
        fired_b.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the hook given to b never fired"
    );
}
