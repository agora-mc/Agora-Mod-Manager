//! Safety net for `local_state.db`: ordered migrations, a pre-migration
//! backup, an integrity check, recovery instead of a fatal error, and a
//! schema-too-new refusal. Each test builds the database the way an earlier
//! release wrote it, then opens it through the real startup path.

use std::path::{Path, PathBuf};

use agora_core::db::{
    init_local_state_db, init_local_state_db_with, local_state_connection, run_migrations,
    run_migrations_to, CorruptRecovery, LocalStateOpenOptions, ERR_LOCAL_STATE_BACKUP_FAILED,
    ERR_LOCAL_STATE_NEWER, LOCAL_STATE_BACKUP_RETENTION, LOCAL_STATE_SCHEMA_VERSION,
};
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
            accept_missing_backup: false,
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
