use crate::models::InstanceRow;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Expected schema version for the mutable local SQLite database.
/// Migrations are applied sequentially on startup.
pub const LOCAL_STATE_SCHEMA_VERSION: i64 = 31;

/// How many pre-migration database backups are kept.
pub const LOCAL_STATE_BACKUP_RETENTION: usize = 3;

/// Folder beside the database where pre-migration backups live.
pub const LOCAL_STATE_BACKUP_DIR: &str = "backups";

/// Error code for a database written by a newer Agora.
pub const ERR_LOCAL_STATE_NEWER: &str = "ERR_LOCAL_STATE_NEWER";

/// Error code for a failed pre-migration backup.
pub const ERR_LOCAL_STATE_BACKUP_FAILED: &str = "ERR_LOCAL_STATE_BACKUP_FAILED";

/// Choices an adapter may make at startup where the safe default is to stop.
///
/// Both are explicit user overrides, never automatic: the default for each is
/// to refuse and report, because overwriting or migrating without a copy is
/// exactly the case this safety net exists to prevent.
#[derive(Debug, Clone, Default)]
pub struct LocalStateOpenOptions {
    /// A database from a newer Agora may be kept aside and a fresh database
    /// started in its place. The old file is never deleted.
    pub accept_newer_schema: bool,
    /// Migrate even when the pre-migration backup could not be written.
    pub accept_missing_backup: bool,
    /// Test-only: run once the startup lock of *this* database is taken, before anything reads
    /// or writes it. Integration tests cannot use `cfg(test)` in a library, so this is a runtime
    /// hook. It is passed in with the other startup choices, so two initializers — in one process
    /// or two — each carry their own and neither replaces the other. Production passes `None`.
    #[doc(hidden)]
    pub startup_lock_hook: Option<StartupLockHook>,
}

/// The test-only hook [`LocalStateOpenOptions::startup_lock_hook`] runs.
#[doc(hidden)]
#[derive(Clone)]
pub struct StartupLockHook(Arc<dyn Fn(&Path) + Send + Sync>);

impl StartupLockHook {
    pub fn new(hook: impl Fn(&Path) + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    fn fire(&self, db_path: &Path) {
        (self.0)(db_path);
    }
}

impl std::fmt::Debug for StartupLockHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartupLockHook(..)")
    }
}

/// What startup did to the local database, for the adapter to tell the player.
#[derive(Debug, Clone, Default)]
pub struct LocalStateStartupReport {
    /// Set when a corrupt database was moved aside and either restored from a
    /// backup or replaced with a fresh one.
    pub recovered_corrupt: Option<CorruptRecovery>,
    /// Set when a newer-schema database was kept aside under an explicit
    /// override and a fresh database started.
    pub newer_schema_kept: Option<NewerSchemaKept>,
    /// The pre-migration backup, when one was made.
    pub backup_path: Option<PathBuf>,
    /// Old backups removed by retention in this run.
    pub removed_backups: Vec<PathBuf>,
    /// The schema version found before migration.
    pub migrated_from: i64,
    /// The schema version recorded after migration.
    pub migrated_to: i64,
    /// Problems that were left alone rather than guessed at, for the adapters to report.
    pub warnings: Vec<String>,
}

/// A corrupt database kept on disk and how startup recovered from it.
#[derive(Debug, Clone)]
pub struct CorruptRecovery {
    /// Where the corrupt file was moved. It is never deleted.
    pub kept_at: PathBuf,
    /// The backup it was restored from, or `None` when a fresh database was
    /// started because no backup passed the integrity check.
    pub restored_from: Option<PathBuf>,
}

/// A newer-schema database kept on disk under an explicit override.
#[derive(Debug, Clone)]
pub struct NewerSchemaKept {
    pub found: i64,
    pub supported: i64,
    /// Where the newer database was moved. It is never deleted.
    pub kept_at: PathBuf,
}

/// Open a read-write connection to the local state database.
///
/// Caller is responsible for ensuring the directory exists; this function
/// does not create parent directories.
pub fn local_state_connection(db_path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(db_path)?;
    ensure_wal(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(conn)
}

/// Put the database in WAL mode, tolerating another process doing the same.
///
/// WAL is a property of the file, so this only does work the first time a
/// database is opened. That is exactly when the desktop app and an agent's CLI
/// call may both be starting for the first time, and switching the journal
/// mode answers "database is locked" at once rather than waiting the busy
/// timeout. Checking first keeps every later open lock-free; the retry covers
/// the first-open race.
fn ensure_wal(conn: &Connection) -> anyhow::Result<()> {
    const ATTEMPTS: u32 = 50;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
    let mut attempt = 0;
    loop {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        if mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        match conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy && attempt < ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Open a read-only connection to the cached registry database.
///
/// Caller is responsible for ensuring the file exists.
pub fn registry_connection(db_path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}

/// Open the local state database read-only, changing nothing (MASTER_SPEC §32.11).
///
/// `agora doctor` reads journals out of the database before core initialises, and a report may not
/// migrate, copy or repair what it reports on. So this opens the file with
/// `SQLITE_OPEN_READ_ONLY` — no migration, no repair, and deliberately no
/// [`ensure_wal`] switch — and refuses writes at the connection level as well
/// (`PRAGMA query_only`). A missing database is an error here: callers that can do without one
/// check first, rather than having "no database" look like "no journals".
pub fn local_state_connection_read_only(db_path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}

/// The schema version a database records, for a caller that must not migrate it.
pub fn recorded_schema_version(conn: &Connection) -> anyhow::Result<i64> {
    schema_version(conn)
}

/// Initialize the local SQLite database on first run and apply migrations.
pub fn init_local_state_db(db_path: &std::path::Path) -> anyhow::Result<()> {
    init_local_state_db_with(db_path, &LocalStateOpenOptions::default()).map(|_| ())
}

/// Initialize the local database, applying the same safety net as
/// [`init_local_state_db`] and reporting what was done.
///
/// The order is deliberate: check integrity before opening the database for
/// writing, recover a corrupt file before touching it, refuse a newer schema
/// before any step can run, then copy the database before migrating. A backup
/// is not made for a brand-new (empty) database: there is nothing to keep, and
/// an empty backup restored later would be worse than none.
pub fn init_local_state_db_with(
    db_path: &std::path::Path,
    options: &LocalStateOpenOptions,
) -> anyhow::Result<LocalStateStartupReport> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // One cross-process lock for the whole open: integrity check, any quarantine/restore, and
    // migrations. The CLI and the desktop can start together; without this, the later process
    // could restore the same backup over the first one's live database and remove its WAL/SHM
    // sidecars. The second starter waits here and then reads the result. The lock file lives on
    // the same data root as the database, so both adapters use the same key.
    let locks_root = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("locks");
    let locks = crate::lock_manager::LockManager::new(locks_root);
    let _db_guard = locks
        .acquire(
            crate::lock_manager::LockResource::LocalStateDatabase,
            "init-local-state-db",
        )
        .map_err(|error| {
            anyhow::anyhow!("Could not lock the local database for startup: {error}")
        })?;
    // Test seam only: production passes no hook, so this does nothing.
    if let Some(hook) = &options.startup_lock_hook {
        hook.fire(db_path);
    }

    let mut report = LocalStateStartupReport::default();

    // 1. Integrity check, and recovery before anything writes to the file.
    //    The recovery marker is written before the live file is moved, so a
    //    launch that fails to restore a backup leaves evidence that recovery
    //    is unfinished. Without it, the absent database would look like a first
    //    run on the next start and recovery would be skipped.
    let identity = ensure_identity(db_path)?;
    let recovery_marker = recovery_marker_path(db_path);
    if recovery_marker.is_file() {
        match read_recovery_marker(&recovery_marker)
            .filter(|marker| marker_belongs_to(marker, &identity, db_path))
        {
            Some(_) => {
                if let Some(recovery) = recover_pending_database(db_path, &recovery_marker)? {
                    report.recovered_corrupt = Some(recovery);
                }
            }
            None => {
                // A foreign or unreadable marker is "could not tell", never permission to
                // restore a backup here or to clear the marker. Keep both and report.
                report.warnings.push(format!(
                    "A database recovery marker beside {} is not from this database and was left \
                     alone; no backup was restored.",
                    db_path.display()
                ));
            }
        }
    } else if db_path.exists() {
        match check_integrity(db_path, IntegrityAccess::Startup)? {
            Integrity::Ok => {}
            Integrity::Corrupt(reason) => {
                write_recovery_marker(db_path, &recovery_marker, &identity)?;
                let kept_at = quarantine_database(db_path, "corrupt")?;
                let restored_from = restore_newest_backup(db_path)?;
                clear_recovery_marker(&recovery_marker);
                report.recovered_corrupt = Some(CorruptRecovery {
                    kept_at,
                    restored_from,
                });
                let _ = reason;
            }
        }
    }

    // 2. Open only after recovery. A newer database is never migrated down or
    //    overwritten; it is moved aside only under an explicit override.
    let mut conn = local_state_connection(db_path)?;
    let target = LOCAL_STATE_SCHEMA_VERSION;
    let found = schema_version(&conn)?;
    if found > target {
        if !options.accept_newer_schema {
            return Err(local_state_error(
                ERR_LOCAL_STATE_NEWER,
                format!(
                    "local_state.db at {} was written by a newer Agora (schema version {found}); \
                     this build supports up to {target}. The database was not changed. Keep the \
                     newer Agora, or start with a fresh database and keep this file: run the CLI \
                     with --fresh-database, or choose \"Start with a fresh database\" when the \
                     desktop offers it.",
                    db_path.display()
                ),
            ));
        }
        drop(conn);
        let kept_at = quarantine_database(db_path, &format!("newer-v{found}"))?;
        report.newer_schema_kept = Some(NewerSchemaKept {
            found,
            supported: target,
            kept_at,
        });
        conn = local_state_connection(db_path)?;
    }

    // 3. Back up before any pending step, then migrate. Each step commits on
    //    its own, so the last successful version stays recorded on failure.
    let current = schema_version(&conn)?;
    report.migrated_from = current;
    // A truly empty file (a first run, or a fresh database after recovery) has
    // nothing to preserve. Anything with schema in it is copied before a step
    // runs, including a very old database with no `schema_version` table.
    if current < target && !database_is_empty(&conn)? {
        match backup_before_migration(&conn, db_path, current) {
            Ok((path, removed)) => {
                report.backup_path = Some(path);
                report.removed_backups = removed;
            }
            Err(error) if options.accept_missing_backup => {
                let _ = error;
            }
            Err(error) => {
                return Err(local_state_error(
                    ERR_LOCAL_STATE_BACKUP_FAILED,
                    format!(
                        "Could not back up local_state.db before migrating it: {error}. Nothing \
                         was changed and the existing database is still in place. Free disk space \
                         or fix the backups folder, or migrate without a backup with \
                         --migrate-without-backup (CLI) or the desktop's confirmation."
                    ),
                ));
            }
        }
    }
    if current < target {
        run_migrations(&conn)?;
    }
    report.migrated_to = schema_version(&conn)?;

    seed_default_settings(&conn)?;
    Ok(report)
}

/// Write the default settings that must exist on every database.
///
/// Runs in its own transaction so two processes starting at once cannot leave
/// it half-done. Reads through the typed registry: a key that is declared and
/// absent takes its declared default, so seeding only stores what the defaults
/// table expects.
fn seed_default_settings(conn: &Connection) -> anyhow::Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;

    // Network feature toggles default to enabled for backward compatibility.
    // Pre-existing keys default to enabled for backward compatibility.
    for key in [
        "network_modrinth_enabled",
        "network_modrinth_cdn_enabled",
        "network_registry_sync_enabled",
        "network_github_oauth_enabled",
        "network_msa_enabled",
        "network_adoptium_enabled",
        "network_server_ping_enabled",
        "network_log_upload_enabled",
    ] {
        if get_setting(conn, key).ok().flatten().is_none() {
            set_setting(conn, key, &serde_json::Value::Bool(true))?;
        }
    }

    // Launch-network toggles for Mojang metadata, Mojang content, and modloader
    // are first-party enabled: they default to true so that a fresh or upgraded DB
    // allows network access. Cached files continue to work when disabled; the user
    // must explicitly disable the category to block downloads.
    for key in [
        "network_mojang_metadata_enabled",
        "network_mojang_content_enabled",
        "network_loader_enabled",
    ] {
        if get_setting(conn, key).ok().flatten().is_none() {
            set_setting(conn, key, &serde_json::Value::Bool(true))?;
        }
    }

    // Feature toggles are stored as genuine JSON booleans so that readers
    // comparing with `true` / `&Value::Bool(true)` match correctly. They are
    // integrations rather than prerequisites, so a new player must opt in
    // during onboarding before any of them can be used or auto-started.
    for key in [
        "modrinth_enabled",
        "ai_mcp_enabled",
        "install_auto_confirm_clean",
        "install_always_auto_confirm",
        // A plugin's outbound network access is opt-in. (The old `plugins_enabled`
        // key is no longer seeded or read: it meant "community plugins on", the
        // opposite of the emergency stop that replaced it. A stored value is
        // ignored, so it cannot engage or release the stop by accident.)
        "network_plugins_enabled",
        // This governs automatic background checking only. A manual check
        // works whenever plugins are on.
        "plugin_updates_enabled",
    ] {
        if get_setting(conn, key).ok().flatten().is_none() {
            set_setting(conn, key, &serde_json::Value::Bool(false))?;
        }
    }

    // Update sweep interval in hours. 12h is the default: twice daily balances
    // freshness (mods rarely release more than once a day) against Modrinth
    // traffic for users who open the launcher many times a day. 0 disables
    // automatic refresh (manual "Check for updates" only).
    if get_setting(conn, "update_sweep_interval_hours")
        .ok()
        .flatten()
        .is_none()
    {
        set_setting(conn, "update_sweep_interval_hours", &serde_json::json!(12))?;
    }

    crate::settings_registry::migrate(conn)?;

    tx.commit()?;
    Ok(())
}

fn local_state_error(code: &str, message: String) -> anyhow::Error {
    anyhow::Error::new(crate::error::LauncherError::Generic {
        code: code.to_string(),
        message,
    })
}

enum Integrity {
    Ok,
    Corrupt(String),
}

/// Whether a SQLite error says the file is damaged rather than merely busy.
///
/// Only these errors may trigger recovery. "Busy" or "cannot open" is "could
/// not tell", which must not be mistaken for "corrupt" and overwrite a file.
fn is_definite_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            )
    )
}

/// The same question for an error that may have been wrapped on its way out of the check.
fn anyhow_is_definite_corruption(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<rusqlite::Error>()
        .is_some_and(is_definite_corruption)
}

/// Who is asking for the integrity check, because the answer differs.
///
/// A **report** may not change the file it reports on, so it opens the database read-only. A
/// **startup** check cannot: a database a crash left with a hot journal has to be rolled back by its
/// first writer, and a read-only connection is refused that, so startup would see "unreadable" where
/// it used to see a recoverable file. That would change what startup does, which is not a reporting
/// decision to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegrityAccess {
    /// Read the file without writing to it, for a report.
    ReadOnly,
    /// May roll back a hot journal, for the startup safety net.
    Startup,
}

/// Run `PRAGMA quick_check` and report whether the file is usable.
///
/// quick_check reads every page and its b-tree links but not indexes. It is
/// the same shape as `integrity_check` at a fraction of the cost, which is why
/// startup can afford it on every open.
fn check_integrity(db_path: &Path, access: IntegrityAccess) -> anyhow::Result<Integrity> {
    let result: anyhow::Result<Vec<String>> = (|| {
        let conn = match access {
            IntegrityAccess::ReadOnly => local_state_connection_read_only(db_path)?,
            IntegrityAccess::Startup => Connection::open(db_path)?,
        };
        let mut statement = conn.prepare("PRAGMA quick_check")?;
        let mut rows = statement.query([])?;
        let mut problems = Vec::new();
        while let Some(row) = rows.next()? {
            let line: String = row.get(0)?;
            if !line.eq_ignore_ascii_case("ok") {
                problems.push(line);
            }
        }
        Ok(problems)
    })();
    match result {
        Ok(problems) if problems.is_empty() => Ok(Integrity::Ok),
        Ok(problems) => Ok(Integrity::Corrupt(problems.join("; "))),
        Err(error) if anyhow_is_definite_corruption(&error) => {
            Ok(Integrity::Corrupt(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

/// A sortable timestamp with the process id and a per-process counter, so two
/// backups made in the same millisecond — even by different processes — never
/// collide in a shared folder.
fn unique_stamp() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-p{}-{:04}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed) % 10_000
    )
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn rename_if_present(from: &Path, to: &Path) -> anyhow::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Move a database (and its WAL sidecars) aside, never deleting it.
fn quarantine_database(db_path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    let name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local_state.db");
    let kept_at = db_path.with_file_name(format!("{name}.{label}-{}", unique_stamp()));
    rename_if_present(db_path, &kept_at)?;
    for suffix in ["-wal", "-shm"] {
        rename_if_present(
            &sidecar_path(db_path, suffix),
            &sidecar_path(&kept_at, suffix),
        )?;
    }
    Ok(kept_at)
}

/// Marker beside the database that says a corrupt file was moved aside and the
/// backup restore did not finish. It is written before the move and removed
/// once recovery has either restored a backup or deliberately started fresh.
fn recovery_marker_path(db_path: &Path) -> PathBuf {
    sidecar_path(db_path, ".recovery")
}

/// A random id kept beside the database. The recovery marker carries this id and the database's
/// path, so a marker copied from another data root (or from another database) is recognised as
/// foreign and left alone instead of driving a backup restore here.
fn identity_path(db_path: &Path) -> PathBuf {
    sidecar_path(db_path, ".identity")
}

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryMarker {
    identity: String,
    db_path: PathBuf,
}

/// Read the database's identity, creating it on a first run. Called under the startup lock, so
/// two starting processes cannot write different ids.
fn ensure_identity(db_path: &Path) -> anyhow::Result<String> {
    let path = identity_path(db_path);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let identity = text.trim();
        if !identity.is_empty() {
            return Ok(identity.to_string());
        }
    }
    let identity = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, identity.as_bytes())?;
    Ok(identity)
}

fn read_recovery_marker(marker: &Path) -> Option<RecoveryMarker> {
    std::fs::read_to_string(marker)
        .ok()
        .and_then(|text| serde_json::from_str::<RecoveryMarker>(&text).ok())
}

fn marker_belongs_to(marker: &RecoveryMarker, identity: &str, db_path: &Path) -> bool {
    marker.identity == identity && marker.db_path == db_path
}

fn write_recovery_marker(db_path: &Path, marker: &Path, identity: &str) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&RecoveryMarker {
        identity: identity.to_string(),
        db_path: db_path.to_path_buf(),
    })?;
    std::fs::write(marker, bytes)?;
    Ok(())
}

/// Remove the marker. A failure here is not fatal: the next launch sees the
/// marker, finds a healthy database, and clears it then.
fn clear_recovery_marker(marker: &Path) {
    let _ = std::fs::remove_file(marker);
}

/// Retry recovery an earlier launch could not finish.
///
/// The earlier launch wrote the marker and moved the corrupt file aside, then
/// failed to read or restore a backup. The absent database must not be taken
/// for a first run: the backups are tried again, and a fresh start happens
/// only when there is genuinely no readable backup.
fn recover_pending_database(
    db_path: &Path,
    marker: &Path,
) -> anyhow::Result<Option<CorruptRecovery>> {
    if db_path.exists() {
        match check_integrity(db_path, IntegrityAccess::Startup)? {
            Integrity::Ok => {
                // The restore already happened; the marker outlived it.
                clear_recovery_marker(marker);
                return Ok(None);
            }
            Integrity::Corrupt(_) => {
                // The marker was written but the move never happened.
                let kept_at = quarantine_database(db_path, "corrupt")?;
                let restored_from = restore_newest_backup(db_path)?;
                clear_recovery_marker(marker);
                return Ok(Some(CorruptRecovery {
                    kept_at,
                    restored_from,
                }));
            }
        }
    }
    let kept_at = newest_kept_corrupt(db_path).unwrap_or_else(|| marker.to_path_buf());
    let restored_from = restore_newest_backup(db_path)?;
    clear_recovery_marker(marker);
    Ok(Some(CorruptRecovery {
        kept_at,
        restored_from,
    }))
}

/// The newest corrupt database a previous launch moved aside, if it is still
/// there. The names start with a sortable timestamp, so the last one is the
/// most recent.
fn newest_kept_corrupt(db_path: &Path) -> Option<PathBuf> {
    let name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local_state.db");
    let prefix = format!("{name}.corrupt-");
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    files.sort();
    files.pop()
}

/// Whether the database holds no SQLite objects at all (a brand-new file).
fn database_is_empty(conn: &Connection) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type IN ('table', 'index', 'view', 'trigger')
            AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 0)
}

/// The schema version recorded, or 0 when the table does not exist yet.
fn schema_version(conn: &Connection) -> anyhow::Result<i64> {
    let present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if present == 0 {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?)
}

fn backups_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LOCAL_STATE_BACKUP_DIR)
}

/// Pre-migration backups, oldest first (the names start with a sortable time).
fn list_backup_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("local_state-") && name.ends_with(".db") && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Copy the database with `VACUUM INTO`, verify the copy, and retain three.
fn backup_before_migration(
    conn: &Connection,
    db_path: &Path,
    from_version: i64,
) -> anyhow::Result<(PathBuf, Vec<PathBuf>)> {
    let dir = backups_dir(db_path);
    std::fs::create_dir_all(&dir)?;
    let destination = dir.join(format!("local_state-{}-v{from_version}.db", unique_stamp()));
    let destination_text = destination
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("backup path is not valid UTF-8"))?;
    conn.execute("VACUUM INTO ?1", rusqlite::params![destination_text])
        .map_err(|error| anyhow::anyhow!("writing {} failed: {error}", destination.display()))?;
    match check_integrity(&destination, IntegrityAccess::Startup)? {
        Integrity::Ok => {}
        Integrity::Corrupt(reason) => {
            let _ = std::fs::remove_file(&destination);
            anyhow::bail!(
                "the backup written to {} failed its integrity check: {reason}",
                destination.display()
            );
        }
    }
    let removed = prune_backups(&dir)?;
    Ok((destination, removed))
}

/// Keep the newest [LOCAL_STATE_BACKUP_RETENTION] backups.
///
/// Older files are deleted after a new backup is written. A failed delete is
/// left alone rather than stopping the migration: the copies are still valid
/// and a later run tries again.
fn prune_backups(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = list_backup_files(dir)?;
    let mut removed = Vec::new();
    while files.len() > LOCAL_STATE_BACKUP_RETENTION {
        let oldest = files.remove(0);
        if std::fs::remove_file(&oldest).is_ok() {
            removed.push(oldest);
        }
    }
    Ok(removed)
}

/// What `agora doctor` can say about the local database without opening it for
/// writing or changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalStateIntegrity {
    Ok,
    Corrupt(String),
    Missing,
    /// The file exists but could not be read. This is not the same as "ok".
    Unreadable(String),
}

#[derive(Debug, Clone)]
pub struct LocalStateInspection {
    pub path: PathBuf,
    pub integrity: LocalStateIntegrity,
    /// The recorded schema version, or `None` when it could not be read.
    pub schema_version: Option<i64>,
    pub supported_version: i64,
    /// Pre-migration backups, oldest first.
    pub backups: Vec<PathBuf>,
    /// Set when the backups folder could not be read; an empty list then means
    /// "could not tell", not "none".
    pub backups_error: Option<String>,
    /// Corrupt or newer databases kept aside by a previous start.
    pub kept_aside: Vec<PathBuf>,
    /// Set when the folder could not be read, as above.
    pub kept_aside_error: Option<String>,
}

/// Inspect the local database for reporting. Read-only: nothing is migrated,
/// copied, renamed or deleted.
/// What `agora doctor` and the desktop's data health panel report about the local database:
/// the inspection in a form both can print, and whether any of it is a problem.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalStateReport {
    pub path: String,
    /// ok, corrupt, missing or unreadable.
    pub integrity: &'static str,
    pub integrity_detail: String,
    pub schema_version: Option<i64>,
    pub supported_version: i64,
    pub database_is_newer: bool,
    /// Pre-migration backups, newest first.
    pub backups: Vec<String>,
    pub backups_error: Option<String>,
    pub backups_kept: usize,
    /// Files a recovery moved aside.
    pub kept_aside: Vec<String>,
    pub kept_aside_error: Option<String>,
    /// Anything above that needs attention.
    pub problems: bool,
}

/// Inspect the local database and say what is wrong with it, if anything.
pub fn local_state_report(db_path: &Path) -> LocalStateReport {
    let info = inspect_local_state(db_path);
    let (integrity, detail) = match &info.integrity {
        LocalStateIntegrity::Ok => ("ok", String::new()),
        LocalStateIntegrity::Corrupt(reason) => ("corrupt", reason.clone()),
        LocalStateIntegrity::Missing => ("missing", String::new()),
        LocalStateIntegrity::Unreadable(reason) => ("unreadable", reason.clone()),
    };
    let newer = info
        .schema_version
        .is_some_and(|version| version > info.supported_version);
    let unreadable_schema =
        !matches!(info.integrity, LocalStateIntegrity::Missing) && info.schema_version.is_none();
    let problems = matches!(
        info.integrity,
        LocalStateIntegrity::Corrupt(_) | LocalStateIntegrity::Unreadable(_)
    ) || unreadable_schema
        || newer
        || info.backups_error.is_some()
        || info.kept_aside_error.is_some();
    LocalStateReport {
        path: info.path.display().to_string(),
        integrity,
        integrity_detail: detail,
        schema_version: info.schema_version,
        supported_version: info.supported_version,
        database_is_newer: newer,
        backups: info
            .backups
            .iter()
            .rev()
            .map(|path| path.display().to_string())
            .collect(),
        backups_error: info.backups_error.clone(),
        backups_kept: LOCAL_STATE_BACKUP_RETENTION,
        kept_aside: info
            .kept_aside
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        kept_aside_error: info.kept_aside_error.clone(),
        problems,
    }
}

pub fn inspect_local_state(db_path: &Path) -> LocalStateInspection {
    let mut inspection = LocalStateInspection {
        path: db_path.to_path_buf(),
        integrity: LocalStateIntegrity::Missing,
        schema_version: None,
        supported_version: LOCAL_STATE_SCHEMA_VERSION,
        backups: Vec::new(),
        backups_error: None,
        kept_aside: Vec::new(),
        kept_aside_error: None,
    };
    match list_backup_files(&backups_dir(db_path)) {
        Ok(backups) => inspection.backups = backups,
        Err(error) => inspection.backups_error = Some(error.to_string()),
    }
    match list_kept_aside(db_path) {
        Ok(kept) => inspection.kept_aside = kept,
        Err(error) => inspection.kept_aside_error = Some(error.to_string()),
    }
    match agora_game_api::probe::probe(db_path) {
        // The one answer that means there is no database: a first run.
        agora_game_api::probe::Probe::Absent => return inspection,
        // Anything else that went wrong is "could not read", never "not created yet".
        agora_game_api::probe::Probe::CantTell(error) => {
            inspection.integrity = LocalStateIntegrity::Unreadable(format!(
                "{} could not be examined ({error})",
                db_path.display()
            ));
            return inspection;
        }
        agora_game_api::probe::Probe::Present(_) => {}
    }
    inspection.integrity = match check_integrity(db_path, IntegrityAccess::ReadOnly) {
        Ok(Integrity::Ok) => LocalStateIntegrity::Ok,
        Ok(Integrity::Corrupt(reason)) => LocalStateIntegrity::Corrupt(reason),
        Err(error) => LocalStateIntegrity::Unreadable(error.to_string()),
    };
    if matches!(inspection.integrity, LocalStateIntegrity::Ok) {
        // Read-only: reading the schema version must not roll back or checkpoint anything.
        inspection.schema_version = local_state_connection_read_only(db_path)
            .ok()
            .and_then(|conn| schema_version(&conn).ok());
    }
    inspection
}

/// Whether a database recovery is waiting for the next start (MASTER_SPEC §32.11).
///
/// Written before a damaged file is moved aside, so an unfinished restore is visible instead of an
/// absent database looking like a first run. Read here without touching it: the report only says
/// what startup will do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDatabaseRecovery {
    /// No marker beside the database: nothing is waiting.
    None,
    /// A marker written for this very database: the next start finishes the restore.
    Pending {
        /// The marker file.
        marker: PathBuf,
        /// Whether an identity file sits beside it, which is what makes a marker this database's.
        identity_present: bool,
    },
    /// A marker is there but could not be read, or belongs to another database: startup leaves it
    /// alone and reports it, so saying anything about the restore would be a guess.
    CantTell {
        /// The marker file.
        marker: PathBuf,
        /// Why nothing can be said about it.
        reason: String,
    },
}

/// Read the pending-recovery marker beside the database, changing nothing.
pub fn pending_database_recovery(db_path: &Path) -> PendingDatabaseRecovery {
    let marker = recovery_marker_path(db_path);
    let text = match std::fs::read_to_string(&marker) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PendingDatabaseRecovery::None
        }
        Err(error) => {
            return PendingDatabaseRecovery::CantTell {
                marker,
                reason: error.to_string(),
            }
        }
    };
    let identity = match std::fs::read_to_string(identity_path(db_path)) {
        Ok(identity) => identity.trim().to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return PendingDatabaseRecovery::CantTell {
                marker,
                reason: format!("its identity could not be read ({error})"),
            }
        }
    };
    match serde_json::from_str::<RecoveryMarker>(&text) {
        Ok(record) if !identity.is_empty() && marker_belongs_to(&record, &identity, db_path) => {
            PendingDatabaseRecovery::Pending {
                marker,
                identity_present: true,
            }
        }
        Ok(_) => PendingDatabaseRecovery::CantTell {
            marker,
            reason: "it is not from this database, so startup leaves it alone and restores no \
                     backup"
                .into(),
        },
        Err(error) => PendingDatabaseRecovery::CantTell {
            marker,
            reason: format!("it could not be read ({error})"),
        },
    }
}

fn list_kept_aside(db_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    let name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local_state.db");
    let prefixes = [format!("{name}.corrupt-"), format!("{name}.newer-v")];
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if prefixes.iter().any(|prefix| file_name.starts_with(prefix)) && entry.path().is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Restore the newest backup that passes `quick_check`.
///
/// Each candidate is copied to a temporary sibling first, checked, then renamed
/// into place, so a backup that fails the check is never used and the live path
/// never holds a half-copied file.
fn restore_newest_backup(db_path: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut backups = list_backup_files(&backups_dir(db_path))?;
    backups.reverse();
    let mut unreadable: Option<anyhow::Error> = None;
    for backup in backups {
        let candidate = db_path.with_file_name(format!(
            "{}.restore-{}",
            db_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("local_state.db"),
            unique_stamp()
        ));
        if let Err(error) = std::fs::copy(&backup, &candidate) {
            // "Could not read" is not "this backup is bad": remember it, and
            // do not silently start fresh while an unreadable copy might hold
            // the player's data.
            unreadable = Some(error.into());
            continue;
        }
        match check_integrity(&candidate, IntegrityAccess::Startup) {
            Ok(Integrity::Ok) => {
                let _ = std::fs::remove_file(sidecar_path(db_path, "-wal"));
                let _ = std::fs::remove_file(sidecar_path(db_path, "-shm"));
                match std::fs::rename(&candidate, db_path) {
                    Ok(()) => return Ok(Some(backup)),
                    Err(error) => {
                        let _ = std::fs::remove_file(&candidate);
                        unreadable = Some(error.into());
                    }
                }
            }
            Ok(Integrity::Corrupt(_)) => {
                // This backup is genuinely unusable; try the next one.
                let _ = std::fs::remove_file(&candidate);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&candidate);
                unreadable = Some(error);
            }
        }
    }
    match unreadable {
        Some(error) => Err(error.context("no backup could be read after the database was damaged")),
        None => Ok(None),
    }
}

/// The global AlwaysPreTouch switch from Settings, if the user set one.
///
/// Settings writes `always_pre_touch`. Launch code used to read
/// `jvm_always_pre_touch`, which nothing in the app writes, so the switch had
/// no effect; that key is still honoured as a fallback for anyone who set it
/// by hand.
pub fn global_always_pre_touch(conn: &Connection) -> Option<bool> {
    ["always_pre_touch", "jvm_always_pre_touch"]
        .iter()
        .find_map(|key| get_setting(conn, key).ok().flatten())
        .and_then(|value| match value {
            serde_json::Value::Bool(b) => Some(b),
            serde_json::Value::Number(n) => Some(n.as_i64() != Some(0)),
            serde_json::Value::String(s) => match s.as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        })
}

/// Apply sequential migrations up to [LOCAL_STATE_SCHEMA_VERSION].
///
/// Each step runs in its own transaction, which also records the step's
/// version, so a failure leaves the last successful version recorded and the
/// database usable. When no step is pending, nothing is executed at all.
pub fn run_migrations(conn: &Connection) -> anyhow::Result<()> {
    run_migration_steps(conn, MIGRATIONS, LOCAL_STATE_SCHEMA_VERSION)
}

/// Apply migrations only up to `target`. Tests use this to build a database as
/// an earlier release wrote it, then open it with [`init_local_state_db`].
#[doc(hidden)]
pub fn run_migrations_to(conn: &Connection, target: i64) -> anyhow::Result<()> {
    run_migration_steps(conn, MIGRATIONS, target)
}

struct MigrationStep {
    version: i64,
    name: &'static str,
    /// True for a step that rebuilds a table other tables reference (SQLite's procedure for
    /// changing a foreign key). The pragma is ignored inside a transaction, so the runner turns
    /// foreign keys off *before* `BEGIN IMMEDIATE` and checks them again before `COMMIT`: a
    /// violation rolls the step back and is reported rather than committed.
    foreign_keys_off: bool,
    run: fn(&Connection) -> anyhow::Result<()>,
}

const MIGRATIONS: &[MigrationStep] = &[
    MigrationStep {
        version: 1,
        name: "initial schema",
        foreign_keys_off: false,
        run: migrate_v1,
    },
    MigrationStep {
        version: 2,
        name: "flag submissions",
        foreign_keys_off: false,
        run: migrate_v2,
    },
    MigrationStep {
        version: 3,
        name: "crash investigator tables",
        foreign_keys_off: false,
        run: migrate_v3,
    },
    MigrationStep {
        version: 4,
        name: "java path and incompatibility override",
        foreign_keys_off: false,
        run: migrate_v4,
    },
    MigrationStep {
        version: 5,
        name: "gc selector baseline",
        foreign_keys_off: false,
        run: migrate_v5,
    },
    MigrationStep {
        version: 6,
        name: "import provenance and jobs",
        foreign_keys_off: false,
        run: migrate_v6,
    },
    MigrationStep {
        version: 7,
        name: "import quarantine path",
        foreign_keys_off: false,
        run: migrate_v7,
    },
    MigrationStep {
        version: 8,
        name: "modrinth content metadata cache",
        foreign_keys_off: false,
        run: migrate_v8,
    },
    MigrationStep {
        version: 9,
        name: "instance memory policy",
        foreign_keys_off: false,
        run: migrate_v9,
    },
    MigrationStep {
        version: 10,
        name: "retire browse curated only",
        foreign_keys_off: false,
        run: migrate_v10,
    },
    MigrationStep {
        version: 11,
        name: "update caches",
        foreign_keys_off: false,
        run: migrate_v11,
    },
    MigrationStep {
        version: 12,
        name: "pack file inventory",
        foreign_keys_off: false,
        run: migrate_v12,
    },
    MigrationStep {
        version: 13,
        name: "launch history",
        foreign_keys_off: false,
        run: migrate_v13,
    },
    MigrationStep {
        version: 14,
        name: "plugin installs and storage",
        foreign_keys_off: false,
        run: migrate_v14,
    },
    MigrationStep {
        version: 15,
        name: "plugin trust",
        foreign_keys_off: false,
        run: migrate_v15,
    },
    MigrationStep {
        version: 16,
        name: "instance pack gaps",
        foreign_keys_off: false,
        run: migrate_v16,
    },
    MigrationStep {
        version: 17,
        name: "lifetime play time",
        foreign_keys_off: false,
        run: migrate_v17,
    },
    MigrationStep {
        version: 18,
        name: "game instances",
        foreign_keys_off: false,
        run: migrate_v18,
    },
    MigrationStep {
        version: 19,
        name: "official plugins",
        foreign_keys_off: false,
        run: migrate_v19,
    },
    MigrationStep {
        version: 20,
        name: "plugin script consent",
        foreign_keys_off: false,
        run: migrate_v20,
    },
    MigrationStep {
        version: 21,
        name: "consent code hash",
        foreign_keys_off: false,
        run: migrate_v21,
    },
    MigrationStep {
        version: 22,
        name: "game play time",
        foreign_keys_off: false,
        run: migrate_v22,
    },
    MigrationStep {
        version: 23,
        name: "session ledger",
        foreign_keys_off: false,
        run: migrate_v23,
    },
    MigrationStep {
        version: 24,
        name: "session answers",
        foreign_keys_off: false,
        run: migrate_v24,
    },
    MigrationStep {
        version: 25,
        name: "crash log results",
        foreign_keys_off: false,
        run: migrate_v25,
    },
    MigrationStep {
        version: 26,
        name: "crash save backups",
        foreign_keys_off: false,
        run: migrate_v26,
    },
    MigrationStep {
        version: 27,
        name: "load order rules",
        foreign_keys_off: false,
        run: migrate_v27,
    },
    MigrationStep {
        version: 28,
        name: "pre-touch column",
        foreign_keys_off: false,
        run: migrate_v28,
    },
    MigrationStep {
        version: 29,
        name: "plugin package journal and ledger process identity",
        foreign_keys_off: false,
        run: migrate_v29,
    },
    MigrationStep {
        version: 30,
        name: "committed plugin game packages",
        foreign_keys_off: false,
        run: migrate_v30,
    },
    MigrationStep {
        version: 31,
        name: "one instance identity table",
        foreign_keys_off: true,
        run: migrate_v31,
    },
];

/// Apply every step up to `target`, skipping any already recorded.
///
/// A step opens `BEGIN IMMEDIATE` when the caller has no transaction open, so
/// two processes starting at once cannot apply the same step twice; inside an
/// existing transaction it uses a savepoint instead. The version is re-read
/// under that lock before running, which is what makes the second starter skip.
fn run_migration_steps(
    conn: &Connection,
    steps: &[MigrationStep],
    target: i64,
) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             version INTEGER PRIMARY KEY
         );",
    )?;
    let mut current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    if current > target {
        anyhow::bail!("local_state.db schema version {current} is newer than supported {target}");
    }
    for step in steps {
        if step.version <= current || step.version > target {
            continue;
        }
        let token = format!("migration_v{}", step.version);
        let standalone = conn.is_autocommit();
        if step.foreign_keys_off && !standalone {
            // The pragma is a no-op inside a transaction, so a rebuild run inside a caller's
            // transaction would silently delete child rows through a cascade instead of moving
            // them. Refuse rather than damage the database.
            anyhow::bail!(
                "local_state.db migration v{} ({}) rebuilds tables and cannot run inside another \
                 transaction",
                step.version,
                step.name
            );
        }
        let broken_references = if step.foreign_keys_off {
            // Recorded before the rebuild, so the check after it reports what this step broke
            // rather than a reference that was already broken when the database arrived.
            conn.execute_batch("PRAGMA foreign_keys = OFF")?;
            foreign_key_violations(conn)?
        } else {
            Vec::new()
        };
        if standalone {
            conn.execute_batch("BEGIN IMMEDIATE")?;
        } else {
            conn.execute_batch(&format!("SAVEPOINT {token}"))?;
        }
        let outcome = (|| -> anyhow::Result<bool> {
            let recorded: i64 = conn.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )?;
            if step.version <= recorded {
                return Ok(false);
            }
            (step.run)(conn)?;
            if step.foreign_keys_off {
                // Inside the transaction, before the commit: a row whose parent this step did not
                // create would otherwise be committed as an orphan.
                let now = foreign_key_violations(conn)?;
                let new: Vec<String> = now
                    .into_iter()
                    .filter(|violation| !broken_references.contains(violation))
                    .collect();
                if !new.is_empty() {
                    anyhow::bail!(
                        "the rebuild left rows that point at no instance: {}",
                        new.join("; ")
                    );
                }
            }
            conn.execute(
                "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
                rusqlite::params![step.version],
            )?;
            Ok(true)
        })();
        let result: anyhow::Result<bool> = match outcome {
            Ok(applied) => if standalone {
                conn.execute_batch("COMMIT")
            } else {
                conn.execute_batch(&format!("RELEASE {token}"))
            }
            .map(|()| applied)
            .map_err(Into::into),
            Err(error) => {
                if standalone {
                    let _ = conn.execute_batch("ROLLBACK");
                } else {
                    let _ = conn.execute_batch(&format!("ROLLBACK TO {token}; RELEASE {token}"));
                }
                Err(error)
            }
        };
        if step.foreign_keys_off {
            // Always put the pragma back, whether the step committed or not: every caller opens
            // its connection through `local_state_connection`, which turns it on, and a step that
            // failed must not leave the database open to cascades.
            conn.execute_batch("PRAGMA foreign_keys = ON")?;
        }
        match result {
            Ok(applied) => {
                if applied {
                    current = step.version;
                }
            }
            Err(error) => {
                return Err(error.context(format!(
                    "local_state.db migration v{} ({}) failed; version {current} is still recorded",
                    step.version, step.name
                )));
            }
        }
    }
    Ok(())
}

/// Every row that references a parent row which does not exist, one line each.
///
/// `PRAGMA foreign_key_check` names the child table and the rowid, but not which instance is
/// missing, so the parent table and key are added for a report a person can act on.
fn foreign_key_violations(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT \"table\", rowid, parent FROM pragma_foreign_key_check")?;
    let rows = stmt.query_map([], |row| {
        let table: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        let parent: String = row.get(2)?;
        Ok(format!(
            "{table} row {} references {parent}",
            rowid.map_or_else(|| "?".to_string(), |id| id.to_string())
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    out.sort();
    Ok(out)
}

/// Add a column only when it is missing, failing on a real ALTER error.
///
/// Migrations are replayed by tests and can run over a table that already has
/// the column (a fresh v1 table, or a database whose version row was lost).
/// Treating "duplicate column" as success keeps both cases working; any other
/// error is reported rather than swallowed.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> anyhow::Result<()> {
    let present: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        rusqlite::params![column],
        |row| row.get(0),
    )?;
    if present == 0 {
        conn.execute(alter_sql, [])?;
    }
    Ok(())
}

fn migrate_v1(conn: &Connection) -> anyhow::Result<()> {
    // initial schema
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_settings (
             key TEXT PRIMARY KEY,
             value_json TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS user_instances (
             instance_id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             minecraft_version TEXT NOT NULL,
             loader TEXT NOT NULL,
             loader_version TEXT NOT NULL,
             is_modpack BOOLEAN NOT NULL DEFAULT 0,
             is_locked BOOLEAN NOT NULL DEFAULT 0,
             last_launched_at TEXT,
             jvm_memory_mb INTEGER NOT NULL DEFAULT 4096,
             jvm_memory_mode TEXT NOT NULL DEFAULT 'manual',
              jvm_gc TEXT NOT NULL DEFAULT 'auto',
             jvm_custom_args TEXT NOT NULL DEFAULT '',
             jvm_always_pre_touch INTEGER NOT NULL DEFAULT 1,
             created_at TEXT NOT NULL DEFAULT (datetime('now'))
         );

         CREATE TABLE IF NOT EXISTS local_crash_telemetry (
             mod_a_id TEXT NOT NULL,
             mod_b_id TEXT NOT NULL,
             crash_count INTEGER NOT NULL DEFAULT 1,
             last_seen_at TEXT NOT NULL,
             PRIMARY KEY (mod_a_id, mod_b_id)
         );

          CREATE TABLE IF NOT EXISTS mcp_approval_grants (
              tool_name TEXT NOT NULL,
              instance_id TEXT NOT NULL,
              state TEXT NOT NULL,
              granted_at TEXT NOT NULL,
              expires_at TEXT,
              PRIMARY KEY (tool_name, instance_id)
          );

          CREATE TABLE IF NOT EXISTS flag_submissions (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              timestamp INTEGER NOT NULL
          );",
    )?;
    Ok(())
}

fn migrate_v2(conn: &Connection) -> anyhow::Result<()> {
    // flag submissions
    // Migration v2: add flag_submissions table for comment-flag rate limiting (§5.5).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS flag_submissions (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             timestamp INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

fn migrate_v3(conn: &Connection) -> anyhow::Result<()> {
    // crash investigator tables
    // Migration v3: add crash-investigator tables for dynamic scoring algorithm.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS crash_events (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             instance_id TEXT NOT NULL,
             fingerprint TEXT NOT NULL,
             exception_class TEXT NOT NULL,
             top_frames_json TEXT NOT NULL,
             signature_name TEXT,
             occurred_at TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS crash_survivals (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             instance_id TEXT NOT NULL,
             occurred_at TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS crash_survival_mods (
             survival_id INTEGER NOT NULL,
             mod_id TEXT NOT NULL,
             PRIMARY KEY (survival_id, mod_id),
             FOREIGN KEY (survival_id) REFERENCES crash_survivals(id)
         );

         CREATE TABLE IF NOT EXISTS crash_attribution (
             fingerprint TEXT NOT NULL,
             mod_id TEXT NOT NULL,
             confirm_count INTEGER NOT NULL DEFAULT 0,
             last_confirmed_at TEXT,
             PRIMARY KEY (fingerprint, mod_id)
         );

         CREATE TABLE IF NOT EXISTS crash_ruled_out (
             fingerprint TEXT NOT NULL,
             mod_id TEXT NOT NULL,
             ruled_out_at TEXT NOT NULL,
             PRIMARY KEY (fingerprint, mod_id)
         );",
    )?;
    Ok(())
}

fn migrate_v4(conn: &Connection) -> anyhow::Result<()> {
    // java path and incompatibility override
    // Migration v4: add java_path and java_incompatible_override columns.
    add_column_if_missing(
        conn,
        "user_instances",
        "java_path",
        "ALTER TABLE user_instances ADD COLUMN java_path TEXT",
    )?;
    add_column_if_missing(
        conn,
        "user_instances",
        "java_incompatible_override",
        "ALTER TABLE user_instances ADD COLUMN java_incompatible_override INTEGER NOT NULL DEFAULT 0",
    )?;
    // Default java_runtime_mode setting to 'automatic'
    let current_mode = get_setting(conn, "java_runtime_mode").ok().flatten();
    if current_mode.is_none() {
        set_setting(conn, "java_runtime_mode", &serde_json::json!("automatic"))?;
    }
    Ok(())
}

fn migrate_v5(_conn: &Connection) -> anyhow::Result<()> {
    // gc selector baseline
    // Migration v5: new instances use the hardware- and Java-aware GC
    // selector. Existing explicit choices remain untouched.
    Ok(())
}

fn migrate_v6(conn: &Connection) -> anyhow::Result<()> {
    // import provenance and jobs
    // Migration v6: launcher migration provenance, per-instance launch policy,
    // and crash-recoverable import jobs.
    add_column_if_missing(
        conn,
        "user_instances",
        "icon_path",
        "ALTER TABLE user_instances ADD COLUMN icon_path TEXT",
    )?;
    add_column_if_missing(
        conn,
        "user_instances",
        "launch_mode_override",
        "ALTER TABLE user_instances ADD COLUMN launch_mode_override TEXT NOT NULL DEFAULT 'auto'",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_imports (
             instance_id TEXT PRIMARY KEY,
             launcher_kind TEXT NOT NULL,
             installation_key TEXT NOT NULL,
             source_instance_key TEXT NOT NULL,
             source_path TEXT NOT NULL,
             source_metadata_json TEXT NOT NULL DEFAULT '{}',
             imported_settings_json TEXT NOT NULL DEFAULT '{}',
             source_fingerprint TEXT NOT NULL,
             imported_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             last_result TEXT NOT NULL DEFAULT 'imported',
             FOREIGN KEY (instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS instance_import_files (
             instance_id TEXT NOT NULL,
             relative_path TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             size INTEGER NOT NULL,
             source_modified_ns INTEGER,
             PRIMARY KEY (instance_id, relative_path),
             FOREIGN KEY (instance_id) REFERENCES instance_imports(instance_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS instance_import_jobs (
             job_id TEXT PRIMARY KEY,
             instance_id TEXT NOT NULL,
             launcher_kind TEXT NOT NULL,
             source_instance_key TEXT NOT NULL,
             staging_path TEXT NOT NULL,
             final_path TEXT NOT NULL,
             state TEXT NOT NULL,
             plan_fingerprint TEXT NOT NULL,
             error TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_instance_import_source
             ON instance_imports (launcher_kind, installation_key, source_instance_key);
         CREATE INDEX IF NOT EXISTS idx_instance_import_jobs_state
             ON instance_import_jobs (state);",
    )?;
    Ok(())
}

fn migrate_v7(conn: &Connection) -> anyhow::Result<()> {
    // import quarantine path
    // Migration v7: persist the exact update quarantine so crash recovery
    // never has to guess among similarly named directories.
    add_column_if_missing(
        conn,
        "instance_import_jobs",
        "quarantine_path",
        "ALTER TABLE instance_import_jobs ADD COLUMN quarantine_path TEXT",
    )?;
    Ok(())
}

fn migrate_v8(conn: &Connection) -> anyhow::Result<()> {
    // modrinth content metadata cache
    // Migration v8: cache installed Modrinth project metadata by the physical
    // content identity so unchanged rows never trigger another API fetch.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS modrinth_content_metadata_cache (
             cache_key TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             title TEXT NOT NULL,
             author TEXT,
             icon_url TEXT,
             fetched_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_modrinth_content_cache_project
             ON modrinth_content_metadata_cache (project_id);",
    )?;
    Ok(())
}

fn migrate_v9(conn: &Connection) -> anyhow::Result<()> {
    // instance memory policy
    // Migration v9: explicit automatic/manual instance memory policy. Existing
    // rows remain manual because their previous 4 GB value does not prove intent.
    add_column_if_missing(
        conn,
        "user_instances",
        "jvm_memory_mode",
        "ALTER TABLE user_instances ADD COLUMN jvm_memory_mode TEXT NOT NULL DEFAULT 'manual'",
    )?;
    Ok(())
}

fn migrate_v10(conn: &Connection) -> anyhow::Result<()> {
    // retire browse curated only
    // Migration v10: retire `browse_curated_only`. Live third-party browsing is
    // already off by default and has its own per-source toggles, so the master
    // switch was a second way of saying the same thing. Anyone who had turned it
    // on wanted no live browsing, so fold that intent into the toggles it
    // suppressed before dropping the key — otherwise removing it would silently
    // re-enable a source they had opted out of.
    let curated_only = matches!(
        get_setting(conn, "browse_curated_only").ok().flatten(),
        Some(serde_json::Value::Bool(true))
    );
    if curated_only {
        set_setting(conn, "modrinth_enabled", &serde_json::json!(false))?;
        set_setting(conn, "technic_enabled", &serde_json::json!(false))?;
    }
    conn.execute(
        "DELETE FROM user_settings WHERE key = 'browse_curated_only'",
        [],
    )?;
    Ok(())
}

fn migrate_v11(conn: &Connection) -> anyhow::Result<()> {
    // update caches
    // Migration v11: persist update-check results and candidate lists so they
    // survive restart and can be read back without network. Mirrors the
    // modrinth_content_metadata_cache pattern from v8 (db.rs:370, modrinth.rs:94,147).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_update_cache (
             instance_id TEXT PRIMARY KEY,
             updates_json TEXT NOT NULL,
             checked_at TEXT NOT NULL,
             FOREIGN KEY(instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_instance_update_cache_checked_at
             ON instance_update_cache (checked_at);
         CREATE TABLE IF NOT EXISTS update_candidate_cache (
             cache_key TEXT PRIMARY KEY,
             candidates_json TEXT NOT NULL,
             fetched_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_update_candidate_cache_fetched_at
             ON update_candidate_cache (fetched_at);",
    )?;
    Ok(())
}

fn migrate_v12(conn: &Connection) -> anyhow::Result<()> {
    // pack file inventory
    // Migration v12: inventory of files contributed by the instance's pack.
    // Full list lives here in the DB; only the content hash is stored in the
    // manifest so manifests stay small even for large packs, while drift can
    // still be detected if the DB is lost. Follows instance_import_files
    // (db.rs:334) for table shape and accessor placement.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_pack_files (
             instance_id TEXT NOT NULL,
             relative_path TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             size INTEGER NOT NULL,
             PRIMARY KEY (instance_id, relative_path),
             FOREIGN KEY (instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_instance_pack_files_instance
             ON instance_pack_files (instance_id);",
    )?;
    Ok(())
}

fn migrate_v13(conn: &Connection) -> anyhow::Result<()> {
    // launch history
    // Local-only launch history. Nothing here leaves the machine; it exists
    // so the launcher can answer "did that change make startup worse?"
    // without anyone running a service to collect it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS launch_history (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             instance_id TEXT NOT NULL,
             started_at TEXT NOT NULL,
             prep_ms INTEGER,
             duration_ms INTEGER,
             outcome TEXT,
             enabled_mod_count INTEGER NOT NULL DEFAULT 0,
             minecraft_version TEXT NOT NULL DEFAULT '',
             loader TEXT NOT NULL DEFAULT '',
             peak_memory_mb INTEGER,
             FOREIGN KEY (instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_launch_history_instance
             ON launch_history (instance_id, started_at DESC);",
    )?;
    Ok(())
}

fn migrate_v14(conn: &Connection) -> anyhow::Result<()> {
    // plugin installs and storage
    // Community plugins. `plugin_installs` is the record of what the user
    // consented to: which plugins exist, which are switched on, and which
    // capabilities they were granted at install time. Capabilities are
    // stored alongside the manifest rather than re-read from it on every
    // load, so a plugin cannot quietly widen its own permissions by
    // shipping an update that asks for more — the grant has to be renewed.
    //
    // `plugin_storage` holds both halves of a plugin's state, split by
    // `kind` so that plugin-written `data` can never overwrite a `setting`
    // the host declared and validates.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_installs (
             plugin_id TEXT PRIMARY KEY,
             version TEXT NOT NULL,
             manifest_json TEXT NOT NULL,
             granted_capabilities TEXT NOT NULL DEFAULT '[]',
             source_kind TEXT NOT NULL,
             source_path TEXT,
             install_dir TEXT NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 0,
             installed_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             data_version INTEGER NOT NULL DEFAULT 1,
             last_error TEXT
         );

         CREATE TABLE IF NOT EXISTS plugin_storage (
             plugin_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             instance_id TEXT NOT NULL DEFAULT '',
             key TEXT NOT NULL,
             value_json TEXT NOT NULL,
             PRIMARY KEY (plugin_id, kind, instance_id, key),
             FOREIGN KEY (plugin_id) REFERENCES plugin_installs(plugin_id) ON DELETE CASCADE
         );

         CREATE TABLE IF NOT EXISTS plugin_data_checkpoints (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             plugin_id TEXT NOT NULL,
             from_version TEXT NOT NULL,
             data_version INTEGER NOT NULL,
             captured_at TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             FOREIGN KEY (plugin_id) REFERENCES plugin_installs(plugin_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_plugin_checkpoints
             ON plugin_data_checkpoints (plugin_id, captured_at DESC);",
    )?;
    Ok(())
}

fn migrate_v15(conn: &Connection) -> anyhow::Result<()> {
    // plugin trust
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_trust (
             plugin_id TEXT PRIMARY KEY,
             update_url TEXT NOT NULL,
             keys_json TEXT NOT NULL,
             highest_sequence INTEGER NOT NULL DEFAULT 0,
             last_checked_at TEXT,
             last_result TEXT,
             FOREIGN KEY (plugin_id) REFERENCES plugin_installs(plugin_id) ON DELETE CASCADE
         );",
    )?;
    Ok(())
}

fn migrate_v16(conn: &Connection) -> anyhow::Result<()> {
    // instance pack gaps
    // Pack entries the instance still lacks (failed, or waiting on a user
    // action). Kept here so the instance page can say "not ready" after a
    // restart, when the import report is gone. No uniqueness on purpose:
    // the list is replaced as a whole, and an unnamed entry must not
    // collide with another one.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_pack_gaps (
             instance_id TEXT NOT NULL,
             filename TEXT NOT NULL,
             display_name TEXT NOT NULL,
             status TEXT NOT NULL,
             reason TEXT,
             FOREIGN KEY (instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_instance_pack_gaps_instance
             ON instance_pack_gaps (instance_id);",
    )?;
    Ok(())
}

fn migrate_v17(conn: &Connection) -> anyhow::Result<()> {
    // lifetime play time
    // Lifetime play time. `launch_history` is trimmed to its newest rows,
    // so the total has to live with the instance. Seeded from whatever
    // history survives, which is the best figure available.
    //
    // Replayed after v31 the table this seeds no longer exists (its rows are in `instances`
    // by then and already carry the total), so the step does nothing rather than fail: the
    // migration tests replay history over a migrated database.
    if !table_exists(conn, "user_instances")? {
        return Ok(());
    }
    add_column_if_missing(
        conn,
        "user_instances",
        "total_play_ms",
        "ALTER TABLE user_instances ADD COLUMN total_play_ms INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute(
        "UPDATE user_instances
            SET total_play_ms = COALESCE(
                (SELECT SUM(duration_ms) FROM launch_history
                  WHERE launch_history.instance_id = user_instances.instance_id),
                0)",
        [],
    )?;
    Ok(())
}

fn migrate_v18(conn: &Connection) -> anyhow::Result<()> {
    // game instances
    // Instances of games other than Minecraft (MASTER_SPEC section 26). Created here, after
    // master's versions 16 and 17, so a database that already ran this branch's earlier
    // version 16 still gets master's pack-gap table and play-time column; both statements are
    // idempotent.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS game_instances (
             instance_id TEXT PRIMARY KEY,
             game TEXT NOT NULL,
             name TEXT NOT NULL,
             base_json TEXT NOT NULL,
             created_at TEXT NOT NULL DEFAULT (datetime('now')),
             last_launched_at TEXT
         );
         CREATE TABLE IF NOT EXISTS instance_pack_gaps (
             instance_id TEXT NOT NULL,
             filename TEXT NOT NULL,
             display_name TEXT NOT NULL,
             status TEXT NOT NULL,
             reason TEXT,
             FOREIGN KEY (instance_id) REFERENCES user_instances(instance_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_instance_pack_gaps_instance
             ON instance_pack_gaps (instance_id);",
    )?;
    Ok(())
}

fn migrate_v19(conn: &Connection) -> anyhow::Result<()> {
    // official plugins
    // Which installed plugins were installed from a signed registry entry
    // (official plugins). A table beside `plugin_installs` rather than
    // columns on it, so the manifest-derived record stays exactly what the
    // package said and "official" is only ever what the installer wrote
    // here. A plain install replaces the plugin's row and clears this one.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_official (
             plugin_id TEXT PRIMARY KEY,
             entry_id TEXT NOT NULL,
             entry_version TEXT NOT NULL,
             entry_sha256 TEXT NOT NULL,
             tree_sha256 TEXT NOT NULL,
             recorded_at TEXT NOT NULL,
             FOREIGN KEY (plugin_id) REFERENCES plugin_installs(plugin_id) ON DELETE CASCADE
         );",
    )?;
    Ok(())
}

fn migrate_v20(conn: &Connection) -> anyhow::Result<()> {
    // plugin script consent
    // Whether the user allowed an installed plugin's code to run. One row per
    // plugin with code; no row means never allowed. Beside `plugin_installs`
    // rather than in it, so the manifest-derived record stays exactly what
    // the package said and permission is only what the user chose.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_script_consent (
             plugin_id TEXT PRIMARY KEY,
             allowed INTEGER NOT NULL,
             decided_at TEXT NOT NULL,
             FOREIGN KEY (plugin_id) REFERENCES plugin_installs(plugin_id) ON DELETE CASCADE
         );",
    )?;
    Ok(())
}

fn migrate_v21(conn: &Connection) -> anyhow::Result<()> {
    // consent code hash
    // Which code the consent was given for. Without it a code-less version
    // in between would let a later version inherit an answer given for
    // different code. A digest of the manifest's entrypoint is enough to
    // tell "the code the user allowed" from "new code".
    // The table may already carry the column when an older schema_version
    // row was lost (the migration tests delete rows to replay history), so
    // add it only when it is missing.
    let has_code_hash: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('plugin_script_consent') WHERE name = 'code_hash'",
        [],
        |row| row.get(0),
    )?;
    if has_code_hash == 0 {
        conn.execute(
            "ALTER TABLE plugin_script_consent ADD COLUMN code_hash TEXT",
            [],
        )?;
    }
    Ok(())
}

fn migrate_v22(conn: &Connection) -> anyhow::Result<()> {
    // game play time
    // How long the player has spent in a generic game instance, and when it last ended.
    // `last_launched_at` (v18) is when the session began; the total is summed on a normal or
    // stopped exit, so a crash does not count a night of idle time. The columns are added only
    // when missing, because a migration test can replay history over a table that already has
    // them.
    let has_total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('game_instances') WHERE name = 'total_play_ms'",
        [],
        |row| row.get(0),
    )?;
    if has_total == 0 {
        conn.execute(
            "ALTER TABLE game_instances ADD COLUMN total_play_ms INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    let has_played: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('game_instances') WHERE name = 'last_played_at'",
        [],
        |row| row.get(0),
    )?;
    if has_played == 0 {
        conn.execute(
            "ALTER TABLE game_instances ADD COLUMN last_played_at TEXT",
            [],
        )?;
    }
    Ok(())
}

fn migrate_v23(conn: &Connection) -> anyhow::Result<()> {
    // session ledger
    // The session ledger (MASTER_SPEC section 30.4). `launch_history` was Minecraft-only, tied
    // to `user_instances` by a foreign key, so a game instance could not have a row. The table
    // is rebuilt without that key and gains the ledger columns; two triggers keep the old
    // promise that history cannot outlive the instance (deleting either kind of instance
    // deletes its rows and manifests). Manifests are stored once per distinct manifest.
    // The triggers go first: replaying this step over a database that already has them would
    // otherwise fail when the table they name is dropped. That includes step 31's trigger on
    // `instances`, which names `launch_history` too and is put back at the end of the step.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_instance_gone;
         DROP TABLE IF EXISTS launch_history_v23;
         CREATE TABLE launch_history_v23 (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             instance_id TEXT NOT NULL,
             started_at TEXT NOT NULL,
             prep_ms INTEGER,
             duration_ms INTEGER,
             outcome TEXT,
             enabled_mod_count INTEGER NOT NULL DEFAULT 0,
             minecraft_version TEXT NOT NULL DEFAULT '',
             loader TEXT NOT NULL DEFAULT '',
             peak_memory_mb INTEGER,
             session_id TEXT,
             ended_at TEXT,
             exit_json TEXT,
             stop_requested INTEGER NOT NULL DEFAULT 0,
             verdict TEXT,
             manifest_hash TEXT,
             owner_pid INTEGER,
             owner_start INTEGER
         );
         INSERT INTO launch_history_v23
             (id, instance_id, started_at, prep_ms, duration_ms, outcome, enabled_mod_count,
              minecraft_version, loader, peak_memory_mb)
         SELECT id, instance_id, started_at, prep_ms, duration_ms, outcome, enabled_mod_count,
                minecraft_version, loader, peak_memory_mb
           FROM launch_history;
         DROP TABLE launch_history;
         ALTER TABLE launch_history_v23 RENAME TO launch_history;
         CREATE INDEX IF NOT EXISTS idx_launch_history_instance
             ON launch_history (instance_id, started_at DESC);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_launch_history_session
             ON launch_history (session_id) WHERE session_id IS NOT NULL;
         CREATE TABLE IF NOT EXISTS launch_manifests (
             instance_id TEXT NOT NULL,
             hash TEXT NOT NULL,
             data BLOB NOT NULL,
             raw_len INTEGER NOT NULL,
             PRIMARY KEY (instance_id, hash)
         ) WITHOUT ROWID;
         CREATE TRIGGER IF NOT EXISTS launch_history_minecraft_instance_gone
             AFTER DELETE ON user_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
         END;
         CREATE TRIGGER IF NOT EXISTS launch_history_game_instance_gone
             AFTER DELETE ON game_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    // Put back the promise on the one identity table when this step is replayed over a
    // database that step 31 already migrated.
    if table_exists(conn, "instances")? {
        conn.execute_batch(instance_delete_trigger())?;
    }
    Ok(())
}

/// The trigger that keeps an instance's history from outliving it, on the one identity table.
///
/// Step 31 creates it; step 23 puts it back when it is replayed over a database that step 31
/// already migrated (the migration tests replay history that way).
fn instance_delete_trigger() -> &'static str {
    "CREATE TRIGGER IF NOT EXISTS launch_history_instance_gone
         AFTER DELETE ON instances
     BEGIN
         DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
         DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
         DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
         DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
         DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
         DELETE FROM plugin_load_order_rules WHERE instance_id = OLD.instance_id;
     END;"
}

fn migrate_v24(conn: &Connection) -> anyhow::Result<()> {
    // session answers
    // The answer the crash doctor reached for one finished session (MASTER_SPEC §30.4). It is
    // kept beside the history row, never inside it, so a failed analysis is still a row with a
    // reason. Answers are trimmed with their session row by `launch_history::prune_history`,
    // and the triggers below delete them with their instance exactly as they delete history.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_answers (
             session_id TEXT PRIMARY KEY,
             instance_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             answer_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_session_answers_instance
             ON session_answers (instance_id, created_at DESC);
         DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;
         CREATE TRIGGER launch_history_minecraft_instance_gone
             AFTER DELETE ON user_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
         END;
         CREATE TRIGGER launch_history_game_instance_gone
             AFTER DELETE ON game_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    Ok(())
}

fn migrate_v25(conn: &Connection) -> anyhow::Result<()> {
    // crash log results
    // The compact result of one older crash log, keyed by its content hash so a sweep never
    // analyses the same file twice (MASTER_SPEC §30.4). The log's bytes are never copied; only
    // this small record is kept, and it lives exactly as long as its instance.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS crash_log_results (
             instance_id TEXT NOT NULL,
             content_hash TEXT NOT NULL,
             source_id TEXT NOT NULL,
             plugin_id TEXT NOT NULL,
             name TEXT NOT NULL,
             relative_path TEXT NOT NULL,
             modified_at TEXT,
             analyzed_at TEXT NOT NULL,
             record_json TEXT NOT NULL,
             PRIMARY KEY (instance_id, content_hash)
         );
         CREATE INDEX IF NOT EXISTS idx_crash_log_results_instance
             ON crash_log_results (instance_id, modified_at DESC);
         DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;
         CREATE TRIGGER launch_history_minecraft_instance_gone
             AFTER DELETE ON user_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
         END;
         CREATE TRIGGER launch_history_game_instance_gone
             AFTER DELETE ON game_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    Ok(())
}

fn migrate_v26(conn: &Connection) -> anyhow::Result<()> {
    // crash save backups
    // The save backup taken before one session's crash test (MASTER_SPEC §30.4). Keyed by
    // session so the Restore button survives clearing user settings; the backup itself lives
    // in the instance folder, and this row only says which one belongs to the session.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS crash_save_backups (
             session_id TEXT PRIMARY KEY,
             instance_id TEXT NOT NULL,
             backup_id TEXT NOT NULL,
             created_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_crash_save_backups_instance
             ON crash_save_backups (instance_id, created_at DESC);
         DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;
         CREATE TRIGGER launch_history_minecraft_instance_gone
             AFTER DELETE ON user_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
         END;
         CREATE TRIGGER launch_history_game_instance_gone
             AFTER DELETE ON game_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    Ok(())
}

fn migrate_v27(conn: &Connection) -> anyhow::Result<()> {
    // load order rules
    // The load-after rules a player sets for one instance's list (MASTER_SPEC §31.4). They
    // live beside the instance rather than in the plugin list, so turning a mod off and on
    // keeps them, and a rule whose item leaves the list stays and reads as unused. The
    // triggers delete them with their instance, exactly as the other per-session tables are.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_load_order_rules (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             instance_id TEXT NOT NULL,
             list_id TEXT NOT NULL,
             item TEXT NOT NULL COLLATE NOCASE,
             after_item TEXT NOT NULL COLLATE NOCASE,
             created_at TEXT NOT NULL DEFAULT (datetime('now')),
             UNIQUE (instance_id, list_id, item, after_item)
         );
         CREATE INDEX IF NOT EXISTS idx_plugin_load_order_rules_instance
             ON plugin_load_order_rules (instance_id, list_id, id);
         DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;
         CREATE TRIGGER launch_history_minecraft_instance_gone
             AFTER DELETE ON user_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
             DELETE FROM plugin_load_order_rules WHERE instance_id = OLD.instance_id;
         END;
         CREATE TRIGGER launch_history_game_instance_gone
             AFTER DELETE ON game_instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
             DELETE FROM plugin_load_order_rules WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    Ok(())
}

fn migrate_v28(conn: &Connection) -> anyhow::Result<()> {
    // Every user_instances table created at v1 has this column, but a database
    // that predates the column and recorded its version some other way does
    // not. Add it when missing; never swallow a real failure.
    add_column_if_missing(
        conn,
        "user_instances",
        "jvm_always_pre_touch",
        "ALTER TABLE user_instances ADD COLUMN jvm_always_pre_touch INTEGER NOT NULL DEFAULT 1",
    )
}

fn migrate_v29(conn: &Connection) -> anyhow::Result<()> {
    // The first install and a purging uninstall need a journal without a plugin record.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS plugin_install_journal (
             plugin_id TEXT PRIMARY KEY,
             document TEXT NOT NULL
         );",
    )?;
    add_column_if_missing(
        conn,
        "launch_history",
        "game_process_json",
        "ALTER TABLE launch_history ADD COLUMN game_process_json TEXT",
    )
}

fn migrate_v30(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "plugin_installs",
        "game_packages_json",
        "ALTER TABLE plugin_installs ADD COLUMN game_packages_json TEXT",
    )
}

/// The one identity table (MASTER_SPEC §32.7).
///
/// `user_instances` (Minecraft) and `game_instances` (every other game) become one
/// `instances` table holding what an instance is whatever game it belongs to, with each
/// game's own columns in a detail table keyed by the instance id: `minecraft_instances`
/// and `game_instance_bases`. The detail tables stay in core's migrations, so core still
/// holds the schema without gaining game logic.
///
/// This is a table rebuild, so it runs with foreign keys off (the step's
/// `foreign_keys_off`): dropping `user_instances` while they were on would cascade-delete
/// every child row before it could be copied. The runner checks the references again
/// before it commits.
///
/// The two old tables are separate id namespaces, so an id in both is refused before
/// anything is copied: the report names both rows and the version stays recorded, which
/// leaves the database exactly as it was (and the safety net's backup on disk).
fn migrate_v31(conn: &Connection) -> anyhow::Result<()> {
    // Already done: the version row was lost and the step is replayed, or a later step is
    // being re-run over a migrated database. There is nothing left to copy from.
    if !table_exists(conn, "user_instances")? && !table_exists(conn, "game_instances")? {
        return Ok(());
    }
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS launch_history_minecraft_instance_gone;
         DROP TRIGGER IF EXISTS launch_history_game_instance_gone;

         CREATE TABLE IF NOT EXISTS instances (
             instance_id TEXT PRIMARY KEY,
             game TEXT NOT NULL,
             name TEXT NOT NULL,
             created_at TEXT NOT NULL DEFAULT (datetime('now')),
             last_launched_at TEXT,
             total_play_ms INTEGER NOT NULL DEFAULT 0,
             last_played_at TEXT
         );

         CREATE TABLE IF NOT EXISTS minecraft_instances (
             instance_id TEXT PRIMARY KEY,
             minecraft_version TEXT NOT NULL,
             loader TEXT NOT NULL,
             loader_version TEXT NOT NULL,
             is_modpack BOOLEAN NOT NULL DEFAULT 0,
             is_locked BOOLEAN NOT NULL DEFAULT 0,
             jvm_memory_mb INTEGER NOT NULL DEFAULT 4096,
             jvm_memory_mode TEXT NOT NULL DEFAULT 'manual',
             jvm_gc TEXT NOT NULL DEFAULT 'auto',
             jvm_custom_args TEXT NOT NULL DEFAULT '',
             jvm_always_pre_touch INTEGER NOT NULL DEFAULT 1,
             java_path TEXT,
             java_incompatible_override INTEGER NOT NULL DEFAULT 0,
             icon_path TEXT,
             launch_mode_override TEXT NOT NULL DEFAULT 'auto',
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );

         CREATE TABLE IF NOT EXISTS game_instance_bases (
             instance_id TEXT PRIMARY KEY,
             base_json TEXT NOT NULL,
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );",
    )?;

    // The two tables are separate id namespaces. A collision is not resolved by guessing:
    // the report names both rows and the migration stops with nothing changed.
    let collisions = shared_instance_ids(conn)?;
    if !collisions.is_empty() {
        anyhow::bail!(
            "two instances share an id, so they cannot be merged into one instance table: {}. \
             Nothing was changed; rename one of them and start Agora again.",
            collisions.join("; ")
        );
    }

    if table_exists(conn, "user_instances")? {
        conn.execute_batch(
            "INSERT INTO instances
                 (instance_id, game, name, created_at, last_launched_at, total_play_ms)
             SELECT instance_id, 'minecraft', name, created_at, last_launched_at, total_play_ms
               FROM user_instances;
             INSERT INTO minecraft_instances
                 (instance_id, minecraft_version, loader, loader_version, is_modpack, is_locked,
                  jvm_memory_mb, jvm_memory_mode, jvm_gc, jvm_custom_args, jvm_always_pre_touch,
                  java_path, java_incompatible_override, icon_path, launch_mode_override)
             SELECT instance_id, minecraft_version, loader, loader_version, is_modpack, is_locked,
                    jvm_memory_mb, jvm_memory_mode, jvm_gc, jvm_custom_args, jvm_always_pre_touch,
                    java_path, java_incompatible_override, icon_path, launch_mode_override
               FROM user_instances;",
        )?;
    }
    if table_exists(conn, "game_instances")? {
        conn.execute_batch(
            "INSERT INTO instances
                 (instance_id, game, name, created_at, last_launched_at, total_play_ms,
                  last_played_at)
             SELECT instance_id, game, name, created_at, last_launched_at, total_play_ms,
                    last_played_at
               FROM game_instances;
             INSERT INTO game_instance_bases (instance_id, base_json)
             SELECT instance_id, base_json FROM game_instances;",
        )?;
    }

    // The four tables that pointed at `user_instances` must point at `instances` instead.
    // A foreign key cannot be altered, so each is rebuilt with its rows carried over: SQLite's
    // procedure for a table rebuild, which needs the foreign keys off (this step's flag) or the
    // `DROP TABLE` below would delete the rows that reference the table being replaced.
    conn.execute_batch(
        "DROP TABLE IF EXISTS instance_imports_v31;
         CREATE TABLE instance_imports_v31 (
             instance_id TEXT PRIMARY KEY,
             launcher_kind TEXT NOT NULL,
             installation_key TEXT NOT NULL,
             source_instance_key TEXT NOT NULL,
             source_path TEXT NOT NULL,
             source_metadata_json TEXT NOT NULL DEFAULT '{}',
             imported_settings_json TEXT NOT NULL DEFAULT '{}',
             source_fingerprint TEXT NOT NULL,
             imported_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             last_result TEXT NOT NULL DEFAULT 'imported',
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );
         INSERT INTO instance_imports_v31
             (instance_id, launcher_kind, installation_key, source_instance_key, source_path,
              source_metadata_json, imported_settings_json, source_fingerprint, imported_at,
              updated_at, last_result)
         SELECT instance_id, launcher_kind, installation_key, source_instance_key, source_path,
                source_metadata_json, imported_settings_json, source_fingerprint, imported_at,
                updated_at, last_result
           FROM instance_imports;
         DROP TABLE instance_imports;
         ALTER TABLE instance_imports_v31 RENAME TO instance_imports;
         CREATE INDEX IF NOT EXISTS idx_instance_import_source
             ON instance_imports (launcher_kind, installation_key, source_instance_key);

         DROP TABLE IF EXISTS instance_pack_files_v31;
         CREATE TABLE instance_pack_files_v31 (
             instance_id TEXT NOT NULL,
             relative_path TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             size INTEGER NOT NULL,
             PRIMARY KEY (instance_id, relative_path),
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );
         INSERT INTO instance_pack_files_v31 (instance_id, relative_path, sha256, size)
         SELECT instance_id, relative_path, sha256, size FROM instance_pack_files;
         DROP TABLE instance_pack_files;
         ALTER TABLE instance_pack_files_v31 RENAME TO instance_pack_files;
         CREATE INDEX IF NOT EXISTS idx_instance_pack_files_instance
             ON instance_pack_files (instance_id);

         DROP TABLE IF EXISTS instance_pack_gaps_v31;
         CREATE TABLE instance_pack_gaps_v31 (
             instance_id TEXT NOT NULL,
             filename TEXT NOT NULL,
             display_name TEXT NOT NULL,
             status TEXT NOT NULL,
             reason TEXT,
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );
         INSERT INTO instance_pack_gaps_v31
             (instance_id, filename, display_name, status, reason)
         SELECT instance_id, filename, display_name, status, reason FROM instance_pack_gaps;
         DROP TABLE instance_pack_gaps;
         ALTER TABLE instance_pack_gaps_v31 RENAME TO instance_pack_gaps;
         CREATE INDEX IF NOT EXISTS idx_instance_pack_gaps_instance
             ON instance_pack_gaps (instance_id);

         DROP TABLE IF EXISTS instance_update_cache_v31;
         CREATE TABLE instance_update_cache_v31 (
             instance_id TEXT PRIMARY KEY,
             updates_json TEXT NOT NULL,
             checked_at TEXT NOT NULL,
             FOREIGN KEY (instance_id) REFERENCES instances(instance_id) ON DELETE CASCADE
         );
         INSERT INTO instance_update_cache_v31 (instance_id, updates_json, checked_at)
         SELECT instance_id, updates_json, checked_at FROM instance_update_cache;
         DROP TABLE instance_update_cache;
         ALTER TABLE instance_update_cache_v31 RENAME TO instance_update_cache;
         CREATE INDEX IF NOT EXISTS idx_instance_update_cache_checked_at
             ON instance_update_cache (checked_at);

         DROP TABLE IF EXISTS user_instances;
         DROP TABLE IF EXISTS game_instances;

         CREATE TRIGGER IF NOT EXISTS launch_history_instance_gone
             AFTER DELETE ON instances
         BEGIN
             DELETE FROM launch_history WHERE instance_id = OLD.instance_id;
             DELETE FROM launch_manifests WHERE instance_id = OLD.instance_id;
             DELETE FROM session_answers WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_log_results WHERE instance_id = OLD.instance_id;
             DELETE FROM crash_save_backups WHERE instance_id = OLD.instance_id;
             DELETE FROM plugin_load_order_rules WHERE instance_id = OLD.instance_id;
         END;",
    )?;
    Ok(())
}

/// True when the database has a table (not a view) of that name.
fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Instances whose id is in both old tables, each described by the rows that share it.
fn shared_instance_ids(conn: &Connection) -> anyhow::Result<Vec<String>> {
    if !table_exists(conn, "user_instances")? || !table_exists(conn, "game_instances")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT user_instances.instance_id, user_instances.name, game_instances.game,
                game_instances.name
           FROM user_instances
           JOIN game_instances ON game_instances.instance_id = user_instances.instance_id
          ORDER BY user_instances.instance_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(format!(
            "'{}' (Minecraft instance '{}' and {} instance '{}')",
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Read a JSON-encoded setting from user_settings.
pub fn get_setting(conn: &Connection, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut stmt = conn.prepare("SELECT value_json FROM user_settings WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    if let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

/// Check if a specific network feature is enabled in settings.
///
/// Declared switches read through the typed registry. A value that is present
/// but unreadable (`0`, `null`, a truncated string) reads as that switch's fail
/// direction — off for every network or privacy switch — and a read error
/// denies rather than enabling. A cleanly absent row keeps the declared default
/// (on for the first-party endpoints, off for `network_plugins_enabled`).
/// Lockdown Mode (`network_lockdown_enabled`) overrides every endpoint.
///
/// A network switch must be a declared boolean. An undeclared key, a declared
/// key of another type, or a store that cannot be read all mean **denied**; a
/// misspelled switch name is a code bug and is logged once rather than quietly
/// becoming permission.
pub fn is_network_enabled(conn: &Connection, key: &str) -> bool {
    // Lockdown Mode blocks every network endpoint regardless of the per-endpoint
    // toggle, so the backend never sends outbound feature requests while on.
    if is_lockdown_enabled(conn) {
        return false;
    }
    match crate::settings_registry::read_bool(conn, key) {
        Ok(Some(enabled)) => enabled,
        Ok(None) => {
            crate::settings_registry::report_undeclared_switch(key);
            false
        }
        // Could not tell: that is not permission.
        Err(_) => false,
    }
}

/// Read a declared boolean setting, failing closed: an unreadable value or a
/// store that cannot be read means `false`.
///
/// Game packages reach this through `db`, which they already import, instead of
/// growing their `agora_core` reference budget with a second module path.
pub fn declared_bool(conn: &Connection, key: &str) -> bool {
    match crate::settings_registry::read_bool(conn, key) {
        Ok(Some(value)) => value,
        Ok(None) | Err(_) => false,
    }
}

/// Whether Lockdown Mode (global network block) is enabled.
///
/// Lockdown overrides every per-endpoint toggle. It defaults to off when the
/// setting has never been written.
pub fn is_lockdown_enabled(conn: &Connection) -> bool {
    // Kept for callers that only need a best-effort answer for display.
    // Enforcement must use `lockdown_denies_network`, which distinguishes
    // "the user turned lockdown off" from "we could not tell".
    lockdown_denies_network(conn).unwrap_or(true)
}

/// Whether Lockdown Mode forbids network access, failing closed.
///
/// This parses lockdown on its own terms. Strict booleans and the documented
/// legacy strings are honoured; a value that is present but not understood, or
/// a store that cannot be read, denies rather than guessing — a kill switch
/// that fails open is not a kill switch. A cleanly absent setting keeps the
/// documented default of off.
pub fn lockdown_denies_network(conn: &Connection) -> crate::error::LauncherResult<bool> {
    let raw = get_setting(conn, "network_lockdown_enabled").map_err(|error| {
        crate::error::LauncherError::Generic {
            code: "ERR_NETWORK_SETTINGS_UNREADABLE".into(),
            message: format!("Could not read the Lockdown Mode setting: {error}"),
        }
    })?;
    let Some(value) = raw else {
        return Ok(false);
    };
    match &value {
        serde_json::Value::Bool(enabled) => Ok(*enabled),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(crate::error::LauncherError::Generic {
                code: "ERR_NETWORK_SETTINGS_MALFORMED".into(),
                message: format!(
                    "The Lockdown Mode setting holds {text:?}, which is not a yes-or-no value. \
                     Network access is blocked until it is corrected in Settings."
                ),
            }),
        },
        other => Err(crate::error::LauncherError::Generic {
            code: "ERR_NETWORK_SETTINGS_MALFORMED".into(),
            message: format!(
                "The Lockdown Mode setting holds {other}, which is not a yes-or-no value. \
                 Network access is blocked until it is corrected in Settings."
            ),
        }),
    }
}

/// Upsert a JSON-encoded setting into user_settings.
pub fn set_setting(conn: &Connection, key: &str, value: &serde_json::Value) -> anyhow::Result<()> {
    let text = serde_json::to_string(value)?;
    conn.execute(
        "INSERT INTO user_settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
        rusqlite::params![key, text],
    )?;
    Ok(())
}

/// Insert or update an instance row.
/// Insert or update an instance: its identity row in `instances`, and the Minecraft
/// columns in `minecraft_instances`.
///
/// `game` is the identity's game id, which only the caller knows (the identity table is
/// shared by every game). The detail row's foreign key means an instance whose identity
/// could not be written fails here rather than existing in half a database.
pub fn upsert_instance(conn: &Connection, game: &str, row: &InstanceRow) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at, last_launched_at, total_play_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(instance_id) DO UPDATE SET
             name = excluded.name,
             last_launched_at = excluded.last_launched_at,
             total_play_ms = excluded.total_play_ms",
        rusqlite::params![
            row.instance_id,
            game,
            row.name,
            row.created_at,
            row.last_launched_at,
            row.total_play_ms,
        ],
    )?;
    conn.execute(
        "INSERT INTO minecraft_instances (
             instance_id, minecraft_version, loader, loader_version,
             is_modpack, is_locked,
             jvm_memory_mb, jvm_gc, jvm_custom_args, jvm_always_pre_touch,
              java_path, java_incompatible_override, icon_path, launch_mode_override,
              jvm_memory_mode
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(instance_id) DO UPDATE SET
             minecraft_version = excluded.minecraft_version,
             loader = excluded.loader,
             loader_version = excluded.loader_version,
             is_modpack = excluded.is_modpack,
             is_locked = excluded.is_locked,
             jvm_memory_mb = excluded.jvm_memory_mb,
             jvm_gc = excluded.jvm_gc,
             jvm_custom_args = excluded.jvm_custom_args,
             jvm_always_pre_touch = excluded.jvm_always_pre_touch,
             java_path = excluded.java_path,
              java_incompatible_override = excluded.java_incompatible_override,
              icon_path = excluded.icon_path,
               launch_mode_override = excluded.launch_mode_override,
               jvm_memory_mode = excluded.jvm_memory_mode",
        rusqlite::params![
            row.instance_id,
            row.minecraft_version,
            row.loader,
            row.loader_version,
            row.is_modpack,
            row.is_locked,
            row.jvm_memory_mb,
            row.jvm_gc,
            row.jvm_custom_args,
            row.jvm_always_pre_touch as i64,
            row.java_path,
            row.java_incompatible_override as i64,
            row.icon_path,
            normalized_launch_mode(&row.launch_mode_override),
            normalized_memory_mode(&row.jvm_memory_mode),
        ],
    )?;
    Ok(())
}

/// Update per-instance Java settings. `custom_args` is optional so Java
/// runtime recovery can change only the executable path and compatibility flag.
pub fn update_instance_java(
    conn: &Connection,
    instance_id: &str,
    java_path: Option<&str>,
    allow_incompatible: bool,
    custom_args: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(custom_args) = custom_args {
        conn.execute(
            "UPDATE minecraft_instances SET java_path = ?1, java_incompatible_override = ?2, jvm_custom_args = ?3 WHERE instance_id = ?4",
            rusqlite::params![java_path, allow_incompatible as i64, custom_args, instance_id],
        )?;
    } else {
        conn.execute(
            "UPDATE minecraft_instances SET java_path = ?1, java_incompatible_override = ?2 WHERE instance_id = ?3",
            rusqlite::params![java_path, allow_incompatible as i64, instance_id],
        )?;
    }
    Ok(())
}

/// Update the structured JVM tuning controls for an instance.
pub fn update_instance_jvm(
    conn: &Connection,
    instance_id: &str,
    memory_mb: i64,
    gc: &str,
    always_pre_touch: bool,
    custom_args: &str,
    memory_mode: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE minecraft_instances
         SET jvm_memory_mb = ?1,
             jvm_gc = ?2,
              jvm_always_pre_touch = ?3,
              jvm_custom_args = ?4,
              jvm_memory_mode = ?5
          WHERE instance_id = ?6",
        rusqlite::params![
            memory_mb,
            gc,
            always_pre_touch as i64,
            custom_args,
            normalized_memory_mode(memory_mode),
            instance_id,
        ],
    )?;
    Ok(())
}

/// Set the locked flag for an instance.
pub fn set_locked(conn: &Connection, instance_id: &str, locked: bool) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE minecraft_instances SET is_locked = ?1 WHERE instance_id = ?2",
        rusqlite::params![locked, instance_id],
    )?;
    Ok(())
}

/// Narrow, parameterized loader-tuple update for loader version switches.
///
/// The `WHERE` clause checks the instance's expected loader tuple so a
/// concurrent modification fails the update instead of silently overwriting
/// it. Returns the number of affected rows (0 when the instance is absent or
/// its tuple changed) so callers can detect and roll back a committed
/// manifest.
pub fn update_instance_loader_version(
    conn: &Connection,
    instance_id: &str,
    expected_loader: &str,
    expected_minecraft_version: &str,
    expected_loader_version: &str,
    new_loader_version: &str,
) -> anyhow::Result<u64> {
    let affected = conn.execute(
        "UPDATE minecraft_instances
         SET loader_version = ?1
         WHERE instance_id = ?2
           AND loader = ?3
           AND minecraft_version = ?4
           AND loader_version = ?5",
        rusqlite::params![
            new_loader_version,
            instance_id,
            expected_loader,
            expected_minecraft_version,
            expected_loader_version
        ],
    )?;
    Ok(affected as u64)
}

/// Overwrite the manifest-derived columns of an instance row (name, Minecraft
/// version, loader, loader version, locked flag) with the manifest's values.
///
/// For paths that replace `instance_manifest.json` wholesale (snapshot
/// restore): the row caches these fields, so without this the editor and
/// library keep showing the pre-restore version. Returns the affected rows
/// (0 when the instance has no row).
pub fn sync_instance_row_from_manifest(
    conn: &Connection,
    instance_id: &str,
    manifest: &crate::models::InstanceManifest,
) -> anyhow::Result<u64> {
    let affected = conn.execute(
        "UPDATE minecraft_instances
         SET minecraft_version = ?1, loader = ?2, loader_version = ?3, is_locked = ?4
         WHERE instance_id = ?5",
        rusqlite::params![
            manifest.minecraft_version,
            manifest.loader,
            manifest.loader_version,
            manifest.is_locked,
            instance_id,
        ],
    )?;
    conn.execute(
        "UPDATE instances SET name = COALESCE(NULLIF(TRIM(?2), ''), name) WHERE instance_id = ?1",
        rusqlite::params![instance_id, manifest.name],
    )?;
    Ok(affected as u64)
}

/// Persist the relative path of an Agora-owned custom instance icon.
pub fn set_instance_icon_path(
    conn: &Connection,
    instance_id: &str,
    icon_path: Option<&str>,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE minecraft_instances SET icon_path = ?1 WHERE instance_id = ?2",
        rusqlite::params![icon_path, instance_id],
    )?;
    Ok(())
}

/// Rename an instance in the DB.
pub fn rename_instance(conn: &Connection, instance_id: &str, new_name: &str) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE instances SET name = ?1 WHERE instance_id = ?2",
        rusqlite::params![new_name, instance_id],
    )?;
    Ok(())
}

/// Delete an instance row.
pub fn delete_instance(conn: &Connection, instance_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM instances WHERE instance_id = ?1",
        rusqlite::params![instance_id],
    )?;
    Ok(())
}

/// List all instances, newest launched first.
pub fn list_instances(conn: &Connection) -> anyhow::Result<Vec<InstanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT instances.instance_id, instances.name, minecraft_instances.minecraft_version,
                minecraft_instances.loader, minecraft_instances.loader_version,
                minecraft_instances.is_modpack, minecraft_instances.is_locked,
                instances.last_launched_at,
                minecraft_instances.jvm_memory_mb, minecraft_instances.jvm_gc,
                minecraft_instances.jvm_custom_args, minecraft_instances.jvm_always_pre_touch,
                instances.created_at,
                 minecraft_instances.java_path,
                 minecraft_instances.java_incompatible_override, minecraft_instances.icon_path,
                 minecraft_instances.launch_mode_override,
                 minecraft_instances.jvm_memory_mode,
                 (SELECT launcher_kind FROM instance_imports
                  WHERE instance_imports.instance_id = instances.instance_id LIMIT 1),
                 instances.total_play_ms
          FROM instances
          JOIN minecraft_instances ON minecraft_instances.instance_id = instances.instance_id
         ORDER BY instances.last_launched_at DESC NULLS LAST, instances.created_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_instance)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Fetch a single instance by id.
pub fn get_instance(conn: &Connection, instance_id: &str) -> anyhow::Result<Option<InstanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT instances.instance_id, instances.name, minecraft_instances.minecraft_version,
                minecraft_instances.loader, minecraft_instances.loader_version,
                minecraft_instances.is_modpack, minecraft_instances.is_locked,
                instances.last_launched_at,
                minecraft_instances.jvm_memory_mb, minecraft_instances.jvm_gc,
                minecraft_instances.jvm_custom_args, minecraft_instances.jvm_always_pre_touch,
                instances.created_at,
                 minecraft_instances.java_path,
                 minecraft_instances.java_incompatible_override, minecraft_instances.icon_path,
                 minecraft_instances.launch_mode_override,
                 minecraft_instances.jvm_memory_mode,
                 (SELECT launcher_kind FROM instance_imports
                  WHERE instance_imports.instance_id = instances.instance_id LIMIT 1),
                 instances.total_play_ms
          FROM instances
          JOIN minecraft_instances ON minecraft_instances.instance_id = instances.instance_id
         WHERE instances.instance_id = ?1",
    )?;
    let mut rows = stmt.query_map([instance_id], row_to_instance)?;
    if let Some(r) = rows.next() {
        Ok(Some(r?))
    } else {
        Ok(None)
    }
}

/// Update last_launched_at for an instance.
pub fn touch_last_launched(
    conn: &Connection,
    instance_id: &str,
    timestamp: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE instances SET last_launched_at = ?1 WHERE instance_id = ?2",
        rusqlite::params![timestamp, instance_id],
    )?;
    Ok(())
}

/// Count instances sharing a loader version (used to decide whether the loader
/// version JSON can be removed when deleting an instance).
pub fn count_instances_by_loader_version(
    conn: &Connection,
    loader: &str,
    minecraft_version: &str,
    loader_version: &str,
) -> anyhow::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM minecraft_instances
         WHERE loader = ?1 AND minecraft_version = ?2 AND loader_version = ?3",
        rusqlite::params![loader, minecraft_version, loader_version],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Normalize a mod pair so the lexicographically smaller ID always comes first.
/// This ensures (sodium, iris) and (iris, sodium) map to the same row.
pub fn normalize_pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Record a co-crash for a pair of mods (§4.1b).
pub fn record_co_crash(conn: &Connection, mod_a: &str, mod_b: &str) -> anyhow::Result<()> {
    let (a, b) = normalize_pair(mod_a, mod_b);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO local_crash_telemetry (mod_a_id, mod_b_id, crash_count, last_seen_at)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(mod_a_id, mod_b_id) DO UPDATE SET
             crash_count = crash_count + 1,
             last_seen_at = excluded.last_seen_at",
        rusqlite::params![a, b, now],
    )?;
    Ok(())
}

/// Purge stale crash telemetry records per §4.1b retention rules:
/// - Records older than 90 days.
/// - Pairs with crash_count < 2.
pub fn purge_stale_crash_telemetry(conn: &Connection) -> anyhow::Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(90);
    conn.execute(
        "DELETE FROM local_crash_telemetry
         WHERE last_seen_at < ?1 OR crash_count < 2",
        rusqlite::params![cutoff.to_rfc3339()],
    )?;
    Ok(())
}

/// Record a flag submission for rate-limit tracking (§5.5).
pub fn record_flag_submission(conn: &Connection, now_unix: i64) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO flag_submissions (timestamp) VALUES (?1)",
        rusqlite::params![now_unix],
    )?;
    Ok(())
}

/// Return the number of flag submissions at or after since_unix.
pub fn count_flags_since(conn: &Connection, since_unix: i64) -> anyhow::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM flag_submissions WHERE timestamp >= ?1",
        rusqlite::params![since_unix],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Rate-limit status for comment-flag submissions (§5.5).
#[derive(Serialize)]
pub struct FlagRateLimit {
    pub remaining_hour: i64,
    pub remaining_day: i64,
    pub reset_hour_at_unix: i64,
    pub reset_day_at_unix: i64,
    pub can_flag: bool,
}

const MAX_FLAGS_PER_HOUR: i64 = 5;
const MAX_FLAGS_PER_DAY: i64 = 20;

/// Compute the current flag rate-limit status for a connection at `now_unix`.
pub fn get_flag_rate_limit_status(
    conn: &Connection,
    now_unix: i64,
) -> anyhow::Result<FlagRateLimit> {
    let hour_window_start = now_unix - 3600;
    let day_window_start = now_unix - 86400;

    let hour_count = count_flags_since(conn, hour_window_start)?;
    let day_count = count_flags_since(conn, day_window_start)?;

    let remaining_hour = (MAX_FLAGS_PER_HOUR - hour_count).max(0);
    let remaining_day = (MAX_FLAGS_PER_DAY - day_count).max(0);

    let reset_hour_at_unix = if hour_count > 0 {
        let mut stmt =
            conn.prepare("SELECT MIN(timestamp) FROM flag_submissions WHERE timestamp >= ?1")?;
        let oldest_hour: i64 = stmt.query_row([hour_window_start], |row| row.get(0))?;
        oldest_hour + 3600
    } else {
        now_unix + 3600
    };

    let reset_day_at_unix = if day_count > 0 {
        let mut stmt =
            conn.prepare("SELECT MIN(timestamp) FROM flag_submissions WHERE timestamp >= ?1")?;
        let oldest_day: i64 = stmt.query_row([day_window_start], |row| row.get(0))?;
        oldest_day + 86400
    } else {
        now_unix + 86400
    };

    let can_flag = remaining_hour > 0 && remaining_day > 0;

    Ok(FlagRateLimit {
        remaining_hour,
        remaining_day,
        reset_hour_at_unix,
        reset_day_at_unix,
        can_flag,
    })
}

fn row_to_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceRow> {
    Ok(InstanceRow {
        instance_id: row.get(0)?,
        name: row.get(1)?,
        minecraft_version: row.get(2)?,
        loader: row.get(3)?,
        loader_version: row.get(4)?,
        is_modpack: row.get(5)?,
        is_locked: row.get(6)?,
        last_launched_at: row.get(7)?,
        jvm_memory_mb: row.get(8)?,
        jvm_gc: row.get(9)?,
        jvm_custom_args: row.get(10)?,
        jvm_always_pre_touch: row.get::<_, i64>(11)? != 0,
        created_at: row.get(12)?,
        java_path: row.get(13)?,
        java_incompatible_override: row.get::<_, i64>(14)? != 0,
        icon_path: row.get(15)?,
        launch_mode_override: row.get(16)?,
        jvm_memory_mode: row.get(17)?,
        import_source: row.get(18)?,
        total_play_ms: row.get(19)?,
    })
}

fn normalized_launch_mode(value: &str) -> &str {
    match value {
        "direct" => "direct",
        "delegated" => "delegated",
        _ => "auto",
    }
}

fn normalized_memory_mode(value: &str) -> &str {
    if value == "auto" {
        "auto"
    } else {
        "manual"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceImportRecord {
    pub instance_id: String,
    pub launcher_kind: String,
    pub installation_key: String,
    pub source_instance_key: String,
    pub source_path: String,
    pub source_metadata_json: String,
    pub imported_settings_json: String,
    pub source_fingerprint: String,
    pub imported_at: String,
    pub updated_at: String,
    pub last_result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceImportFileRecord {
    pub relative_path: String,
    pub sha256: String,
    pub size: u64,
    pub source_modified_ns: Option<i64>,
}

pub fn get_instance_import(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Option<InstanceImportRecord>> {
    let mut stmt = conn.prepare(
        "SELECT instance_id, launcher_kind, installation_key, source_instance_key,
                source_path, source_metadata_json, imported_settings_json,
                source_fingerprint, imported_at, updated_at, last_result
         FROM instance_imports WHERE instance_id = ?1",
    )?;
    let mut rows = stmt.query([instance_id])?;
    Ok(rows.next()?.map(row_to_instance_import).transpose()?)
}

pub fn find_instance_import_by_source(
    conn: &Connection,
    launcher_kind: &str,
    installation_key: &str,
    source_instance_key: &str,
) -> anyhow::Result<Option<InstanceImportRecord>> {
    let mut stmt = conn.prepare(
        "SELECT instance_id, launcher_kind, installation_key, source_instance_key,
                source_path, source_metadata_json, imported_settings_json,
                source_fingerprint, imported_at, updated_at, last_result
         FROM instance_imports
         WHERE launcher_kind = ?1 AND installation_key = ?2 AND source_instance_key = ?3
         ORDER BY updated_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(rusqlite::params![
        launcher_kind,
        installation_key,
        source_instance_key
    ])?;
    Ok(rows.next()?.map(row_to_instance_import).transpose()?)
}

fn row_to_instance_import(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceImportRecord> {
    Ok(InstanceImportRecord {
        instance_id: row.get(0)?,
        launcher_kind: row.get(1)?,
        installation_key: row.get(2)?,
        source_instance_key: row.get(3)?,
        source_path: row.get(4)?,
        source_metadata_json: row.get(5)?,
        imported_settings_json: row.get(6)?,
        source_fingerprint: row.get(7)?,
        imported_at: row.get(8)?,
        updated_at: row.get(9)?,
        last_result: row.get(10)?,
    })
}

pub fn list_instance_import_files(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<InstanceImportFileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT relative_path, sha256, size, source_modified_ns
         FROM instance_import_files WHERE instance_id = ?1 ORDER BY relative_path",
    )?;
    let rows = stmt.query_map([instance_id], |row| {
        Ok(InstanceImportFileRecord {
            relative_path: row.get(0)?,
            sha256: row.get(1)?,
            size: row.get::<_, i64>(2)? as u64,
            source_modified_ns: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn replace_instance_import(
    conn: &Connection,
    record: &InstanceImportRecord,
    files: &[InstanceImportFileRecord],
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO instance_imports (
             instance_id, launcher_kind, installation_key, source_instance_key,
             source_path, source_metadata_json, imported_settings_json,
             source_fingerprint, imported_at, updated_at, last_result
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(instance_id) DO UPDATE SET
             launcher_kind = excluded.launcher_kind,
             installation_key = excluded.installation_key,
             source_instance_key = excluded.source_instance_key,
             source_path = excluded.source_path,
             source_metadata_json = excluded.source_metadata_json,
             imported_settings_json = excluded.imported_settings_json,
             source_fingerprint = excluded.source_fingerprint,
             updated_at = excluded.updated_at,
             last_result = excluded.last_result",
        rusqlite::params![
            record.instance_id,
            record.launcher_kind,
            record.installation_key,
            record.source_instance_key,
            record.source_path,
            record.source_metadata_json,
            record.imported_settings_json,
            record.source_fingerprint,
            record.imported_at,
            record.updated_at,
            record.last_result,
        ],
    )?;
    tx.execute(
        "DELETE FROM instance_import_files WHERE instance_id = ?1",
        [record.instance_id.as_str()],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO instance_import_files
                 (instance_id, relative_path, sha256, size, source_modified_ns)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for file in files {
            stmt.execute(rusqlite::params![
                record.instance_id,
                file.relative_path,
                file.sha256,
                file.size as i64,
                file.source_modified_ns,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// A file contributed by the instance's pack, stored in `instance_pack_files`.
///
/// Full per-file list lives here in the DB; only [`crate::models::PackOrigin::pack_content_hash`]
/// is stored in the manifest so manifests stay small while drift can still be
/// detected if the DB is lost. Shape follows [`InstanceImportFileRecord`]
/// but omits `source_modified_ns`, which matters only for launcher-import
/// baseline tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstancePackFile {
    pub relative_path: String,
    pub sha256: String,
    pub size: u64,
}

pub fn list_instance_pack_files(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<InstancePackFile>> {
    let mut stmt = conn.prepare(
        "SELECT relative_path, sha256, size FROM instance_pack_files WHERE instance_id = ?1 ORDER BY relative_path",
    )?;
    let rows = stmt.query_map([instance_id], |row| {
        Ok(InstancePackFile {
            relative_path: row.get(0)?,
            sha256: row.get(1)?,
            size: row.get::<_, i64>(2)? as u64,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn delete_instance_pack_files(conn: &Connection, instance_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM instance_pack_files WHERE instance_id = ?1",
        [instance_id],
    )?;
    Ok(())
}

/// Replace the entire pack file inventory for `instance_id`.
///
/// A pack install defines the whole set at once, so this is replace-all
/// rather than incremental. Runs in a single transaction.
pub fn replace_instance_pack_files(
    conn: &Connection,
    instance_id: &str,
    files: &[InstancePackFile],
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM instance_pack_files WHERE instance_id = ?1",
        [instance_id],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO instance_pack_files (instance_id, relative_path, sha256, size) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for file in files {
            stmt.execute(rusqlite::params![
                instance_id,
                file.relative_path,
                file.sha256,
                file.size as i64
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// A pack entry that is still missing from an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstancePackGap {
    pub filename: String,
    pub display_name: String,
    /// `failed` or `needs_action`.
    pub status: String,
    pub reason: Option<String>,
}

/// Replace the list of pack entries an instance is still missing.
pub fn replace_instance_pack_gaps(
    conn: &Connection,
    instance_id: &str,
    gaps: &[InstancePackGap],
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM instance_pack_gaps WHERE instance_id = ?1",
        [instance_id],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for gap in gaps {
            stmt.execute(rusqlite::params![
                instance_id,
                gap.filename,
                gap.display_name,
                gap.status,
                gap.reason
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_instance_pack_gaps(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<InstancePackGap>> {
    let mut stmt = conn.prepare(
        "SELECT filename, display_name, status, reason FROM instance_pack_gaps
         WHERE instance_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map([instance_id], |row| {
        Ok(InstancePackGap {
            filename: row.get(0)?,
            display_name: row.get(1)?,
            status: row.get(2)?,
            reason: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceImportJob {
    pub job_id: String,
    pub instance_id: String,
    pub launcher_kind: String,
    pub source_instance_key: String,
    pub staging_path: String,
    pub final_path: String,
    pub quarantine_path: Option<String>,
    pub state: String,
    pub plan_fingerprint: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn upsert_instance_import_job(
    conn: &Connection,
    job: &InstanceImportJob,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO instance_import_jobs (
             job_id, instance_id, launcher_kind, source_instance_key,
             staging_path, final_path, quarantine_path, state, plan_fingerprint, error,
             created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(job_id) DO UPDATE SET
             state = excluded.state,
             error = excluded.error,
             updated_at = excluded.updated_at",
        rusqlite::params![
            job.job_id,
            job.instance_id,
            job.launcher_kind,
            job.source_instance_key,
            job.staging_path,
            job.final_path,
            job.quarantine_path,
            job.state,
            job.plan_fingerprint,
            job.error,
            job.created_at,
            job.updated_at,
        ],
    )?;
    Ok(())
}

pub fn delete_instance_import_job(conn: &Connection, job_id: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM instance_import_jobs WHERE job_id = ?1",
        [job_id],
    )?;
    Ok(())
}

pub fn list_instance_import_jobs(conn: &Connection) -> anyhow::Result<Vec<InstanceImportJob>> {
    let mut statement = conn.prepare(
        "SELECT job_id, instance_id, launcher_kind, source_instance_key,
                staging_path, final_path, quarantine_path, state, plan_fingerprint, error,
                created_at, updated_at
         FROM instance_import_jobs ORDER BY created_at",
    )?;
    let rows = statement.query_map([], row_to_import_job)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// One `instance_import_jobs` row, as far as it can be read.
///
/// A row this build cannot parse is that row's problem, never the listing's: the other rows are
/// real journals, and one bad row must not hide them.
pub struct InstanceImportRow {
    /// The instance the row names, when even that column is readable.
    pub instance_id: Option<String>,
    /// The job, or why this row cannot be read.
    pub job: std::result::Result<InstanceImportJob, String>,
}

/// Every `instance_import_jobs` row, each one's own trouble kept with it.
pub fn instance_import_job_rows(conn: &Connection) -> anyhow::Result<Vec<InstanceImportRow>> {
    let mut statement = conn.prepare(
        "SELECT job_id, instance_id, launcher_kind, source_instance_key,
                staging_path, final_path, quarantine_path, state, plan_fingerprint, error,
                created_at, updated_at
         FROM instance_import_jobs ORDER BY created_at",
    )?;
    let mut rows = statement.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        // The instance's own name is read separately, so a row that cannot be parsed can still be
        // reported as the row it is. A name this build cannot read is part of that row's error,
        // never the row's absence.
        let instance_id = match row.get::<_, rusqlite::types::Value>(1) {
            Ok(rusqlite::types::Value::Text(instance_id)) => Some(instance_id),
            _ => None,
        };
        out.push(InstanceImportRow {
            instance_id,
            job: row_to_import_job(row).map_err(|error| error.to_string()),
        });
    }
    Ok(out)
}

fn row_to_import_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<InstanceImportJob> {
    Ok(InstanceImportJob {
        job_id: row.get(0)?,
        instance_id: row.get(1)?,
        launcher_kind: row.get(2)?,
        source_instance_key: row.get(3)?,
        staging_path: row.get(4)?,
        final_path: row.get(5)?,
        quarantine_path: row.get(6)?,
        state: row.get(7)?,
        plan_fingerprint: row.get(8)?,
        error: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

// ---------------------------------------------------------------------------
// Crash Investigator tables (v3 schema)
// ---------------------------------------------------------------------------

/// An attribution row: (mod_id, confirm_count, last_confirmed_at).
#[derive(Debug, Clone, Serialize)]
pub struct CrashAttribution {
    pub mod_id: String,
    pub confirm_count: i64,
    pub last_confirmed_at: Option<String>,
}

/// Insert a crash event and return the new row id.
pub fn insert_crash_event(
    conn: &Connection,
    instance_id: &str,
    fingerprint: &str,
    exception_class: &str,
    top_frames_json: &str,
    signature_name: Option<&str>,
) -> anyhow::Result<i64> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO crash_events (instance_id, fingerprint, exception_class, top_frames_json, signature_name, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![instance_id, fingerprint, exception_class, top_frames_json, signature_name, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert a survival (successful launch) with associated mod ids.
pub fn insert_survival(
    conn: &Connection,
    instance_id: &str,
    mod_ids: &[String],
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO crash_survivals (instance_id, occurred_at) VALUES (?1, ?2)",
        rusqlite::params![instance_id, now],
    )?;
    let survival_id = conn.last_insert_rowid();
    for mod_id in mod_ids {
        conn.execute(
            "INSERT INTO crash_survival_mods (survival_id, mod_id) VALUES (?1, ?2)",
            rusqlite::params![survival_id, mod_id],
        )?;
    }
    Ok(())
}

/// Return confirmed attributions for a fingerprint, ordered by confirm_count DESC.
pub fn get_confirmed_attribution(
    conn: &Connection,
    fingerprint: &str,
) -> anyhow::Result<Vec<CrashAttribution>> {
    let mut stmt = conn.prepare(
        "SELECT mod_id, confirm_count, last_confirmed_at
         FROM crash_attribution
         WHERE fingerprint = ?1
         ORDER BY confirm_count DESC",
    )?;
    let rows = stmt.query_map([fingerprint], |row| {
        Ok(CrashAttribution {
            mod_id: row.get(0)?,
            confirm_count: row.get(1)?,
            last_confirmed_at: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Every confirmed attribution on this PC, as `(bucket or fingerprint, mod id, confirm count)`.
/// The crash engine reads this to add its boost-only local-confirmation evidence; an engine bucket
/// and an older fingerprint simply never collide.
pub fn all_confirmed_attributions(conn: &Connection) -> anyhow::Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT fingerprint, mod_id, confirm_count FROM crash_attribution ORDER BY fingerprint, mod_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Upsert a confirmed attribution: insert with confirm_count=1 or increment + update last_confirmed_at.
pub fn increment_confirmation(
    conn: &Connection,
    fingerprint: &str,
    mod_id: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO crash_attribution (fingerprint, mod_id, confirm_count, last_confirmed_at)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(fingerprint, mod_id) DO UPDATE SET
             confirm_count = confirm_count + 1,
             last_confirmed_at = excluded.last_confirmed_at",
        rusqlite::params![fingerprint, mod_id, now],
    )?;
    Ok(())
}

/// Idempotently add a ruled-out mod for a fingerprint.
pub fn add_ruled_out(conn: &Connection, fingerprint: &str, mod_id: &str) -> anyhow::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO crash_ruled_out (fingerprint, mod_id, ruled_out_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![fingerprint, mod_id, now],
    )?;
    Ok(())
}

/// Check whether a mod has been ruled out for a fingerprint.
pub fn is_ruled_out(conn: &Connection, fingerprint: &str, mod_id: &str) -> anyhow::Result<bool> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM crash_ruled_out WHERE fingerprint = ?1 AND mod_id = ?2")?;
    Ok(stmt.exists([fingerprint, mod_id])?)
}

/// Return the mod_ids ruled out for a fingerprint.
pub fn get_ruled_out_mods(conn: &Connection, fingerprint: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT mod_id FROM crash_ruled_out WHERE fingerprint = ?1")?;
    let rows = stmt.query_map([fingerprint], |row| row.get(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Return the number of survivals a mod appears in.
pub fn get_mod_survival_count(conn: &Connection, mod_id: &str) -> anyhow::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM crash_survival_mods WHERE mod_id = ?1",
        rusqlite::params![mod_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Return the number of survivals where both mods a and b appear together.
pub fn get_pair_survival_count(conn: &Connection, a: &str, b: &str) -> anyhow::Result<i64> {
    let (first, second) = normalize_pair(a, b);
    conn.query_row(
        "SELECT COUNT(*) FROM crash_survival_mods x
         JOIN crash_survival_mods y ON x.survival_id = y.survival_id
         WHERE x.mod_id = ?1 AND y.mod_id = ?2",
        rusqlite::params![first, second],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Return the co-crash count for a mod pair from local_crash_telemetry.
/// Returns 0 when no row exists for the pair.
pub fn get_pair_crash_count(conn: &Connection, a: &str, b: &str) -> anyhow::Result<i64> {
    let (first, second) = normalize_pair(a, b);
    let result: Result<i64, _> = conn.query_row(
        "SELECT crash_count FROM local_crash_telemetry WHERE mod_a_id = ?1 AND mod_b_id = ?2",
        rusqlite::params![first, second],
        |row| row.get(0),
    );
    Ok(result.unwrap_or(0))
}

/// Return the total number of crash_survivals rows.
pub fn get_total_survival_count(conn: &Connection) -> anyhow::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM crash_survivals", [], |row| row.get(0))
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper: create a unique temp-file-backed test database with migrations applied.
    fn test_db() -> (Connection, PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Each database gets its own folder, so a replay's `backups/` folder is
        // not shared with another test running at the same time.
        let dir = std::env::temp_dir().join(format!("agora-test-{}-{}", std::process::id(), n));
        // A recycled process id must not inherit a previous run's rows.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test db folder");
        let path = dir.join("local_state.db");
        init_local_state_db(&path).expect("failed to init test db");
        let conn = Connection::open(&path).expect("failed to open test db");
        (conn, path)
    }

    /// Helper: a database as an earlier release wrote it, for a test that replays a step.
    ///
    /// Replaying a step means deleting its version row, which only describes a database that
    /// predates that step's successor: an old step cannot be re-run over a schema whose tables
    /// it no longer has.
    fn test_db_at(version: i64) -> (Connection, PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agora-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test db folder");
        let path = dir.join("local_state.db");
        let conn = Connection::open(&path).expect("failed to open test db");
        run_migrations_to(&conn, version).expect("failed to build the old test db");
        (conn, path)
    }

    fn a_good_step(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch("CREATE TABLE kept (value TEXT); INSERT INTO kept VALUES ('yes');")?;
        Ok(())
    }

    fn a_failing_step(_conn: &Connection) -> anyhow::Result<()> {
        anyhow::bail!("invented failure")
    }

    #[test]
    fn a_failing_step_leaves_the_earlier_version_recorded_and_the_database_usable() {
        let conn = Connection::open_in_memory().unwrap();
        let steps = [
            MigrationStep {
                version: 1,
                name: "good",
                foreign_keys_off: false,
                run: a_good_step,
            },
            MigrationStep {
                version: 2,
                name: "bad",
                foreign_keys_off: false,
                run: a_failing_step,
            },
        ];
        let error = run_migration_steps(&conn, &steps, 2).unwrap_err();
        assert!(
            format!("{error:#}").contains("invented failure"),
            "{error:#}"
        );
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 1, "the last successful version stays recorded");
        let kept: String = conn
            .query_row("SELECT value FROM kept", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, "yes", "the earlier step's work is still there");
    }

    // ---- normalize_pair (pure, no DB) ----

    #[test]
    fn test_normalize_pair_lexicographic() {
        let (a, b) = normalize_pair("zinc", "sodium");
        assert_eq!(a, "sodium");
        assert_eq!(b, "zinc");
    }

    #[test]
    fn test_normalize_pair_already_ordered() {
        let (a, b) = normalize_pair("a", "b");
        assert_eq!(a, "a");
        assert_eq!(b, "b");
    }

    #[test]
    fn test_normalize_pair_symmetric() {
        assert_eq!(normalize_pair("a", "b"), normalize_pair("b", "a"));
    }

    #[test]
    fn test_normalize_pair_same_id() {
        let (a, b) = normalize_pair("x", "x");
        assert_eq!(a, "x");
        assert_eq!(b, "x");
    }

    // ---- get_setting / set_setting ----

    #[test]
    fn test_get_setting_absent_returns_none() {
        let (conn, _path) = test_db();
        assert!(get_setting(&conn, "nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_set_setting_roundtrip() {
        let (conn, _path) = test_db();
        set_setting(&conn, "key", &serde_json::json!("value")).unwrap();
        let val = get_setting(&conn, "key").unwrap();
        assert_eq!(val, Some(serde_json::json!("value")));
    }

    #[test]
    fn test_new_launch_network_keys_default_to_true() {
        let (conn, _path) = test_db();
        for key in [
            "network_mojang_metadata_enabled",
            "network_mojang_content_enabled",
            "network_loader_enabled",
        ] {
            let val = get_setting(&conn, key).unwrap();
            assert_eq!(
                val,
                Some(serde_json::json!(true)),
                "{key} should default to true"
            );
        }
        // Verify pre-existing network keys still default to true.
        for key in [
            "network_modrinth_enabled",
            "network_modrinth_cdn_enabled",
            "network_registry_sync_enabled",
            "network_github_oauth_enabled",
            "network_msa_enabled",
            "network_adoptium_enabled",
        ] {
            let val = get_setting(&conn, key).unwrap();
            assert_eq!(
                val,
                Some(serde_json::json!(true)),
                "{key} should default to true"
            );
        }
    }

    #[test]
    fn global_pre_touch_reads_the_key_settings_writes() {
        let (conn, _path) = test_db();
        set_setting(&conn, "always_pre_touch", &serde_json::json!(false)).unwrap();
        assert_eq!(global_always_pre_touch(&conn), Some(false));
        set_setting(&conn, "always_pre_touch", &serde_json::json!(true)).unwrap();
        assert_eq!(global_always_pre_touch(&conn), Some(true));
    }

    #[test]
    fn test_init_normalizes_legacy_boolean_settings() {
        let (conn, path) = test_db();
        set_setting(&conn, "modrinth_enabled", &serde_json::json!("true")).unwrap();
        set_setting(&conn, "always_pre_touch", &serde_json::json!(0)).unwrap();
        drop(conn);

        init_local_state_db(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            get_setting(&conn, "modrinth_enabled").unwrap(),
            Some(serde_json::json!(true))
        );
        assert_eq!(
            get_setting(&conn, "always_pre_touch").unwrap(),
            Some(serde_json::json!(false))
        );
    }

    #[test]
    fn retiring_curated_only_keeps_live_browsing_off() {
        // Someone who had switched curated-only on wanted no live third-party
        // browsing. Dropping the switch must not hand that back to them.
        let (conn, path) = test_db_at(30);
        set_setting(&conn, "browse_curated_only", &serde_json::json!(true)).unwrap();
        set_setting(&conn, "modrinth_enabled", &serde_json::json!(true)).unwrap();
        set_setting(&conn, "technic_enabled", &serde_json::json!(true)).unwrap();
        conn.execute("DELETE FROM schema_version WHERE version >= 10", [])
            .unwrap();
        drop(conn);

        init_local_state_db(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            get_setting(&conn, "modrinth_enabled").unwrap(),
            Some(serde_json::json!(false))
        );
        assert_eq!(
            get_setting(&conn, "technic_enabled").unwrap(),
            Some(serde_json::json!(false))
        );
        assert_eq!(get_setting(&conn, "browse_curated_only").unwrap(), None);
    }

    #[test]
    fn retiring_curated_only_leaves_an_opted_in_user_alone() {
        // Curated-only off means the per-source toggles were already the whole
        // story; they must survive the key being dropped.
        let (conn, path) = test_db_at(30);
        set_setting(&conn, "browse_curated_only", &serde_json::json!(false)).unwrap();
        set_setting(&conn, "modrinth_enabled", &serde_json::json!(true)).unwrap();
        conn.execute("DELETE FROM schema_version WHERE version >= 10", [])
            .unwrap();
        drop(conn);

        init_local_state_db(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            get_setting(&conn, "modrinth_enabled").unwrap(),
            Some(serde_json::json!(true))
        );
        assert_eq!(get_setting(&conn, "browse_curated_only").unwrap(), None);
    }

    #[test]
    fn test_optional_integrations_default_to_disabled() {
        let (conn, _path) = test_db();
        for key in [
            "modrinth_enabled",
            "ai_mcp_enabled",
            "install_auto_confirm_clean",
            "install_always_auto_confirm",
        ] {
            assert_eq!(
                get_setting(&conn, key).unwrap(),
                Some(serde_json::json!(false))
            );
        }
    }

    #[test]
    fn automatic_plugin_update_checks_default_to_disabled() {
        let (conn, _path) = test_db();
        // Background checks are an extra source of traffic; a manual check is
        // still available when the user has enabled the plugin system.
        assert_eq!(
            get_setting(&conn, "plugin_updates_enabled").unwrap(),
            Some(serde_json::json!(false))
        );
    }

    #[test]
    fn test_set_setting_overwrite() {
        let (conn, _path) = test_db();
        set_setting(&conn, "key", &serde_json::json!("v1")).unwrap();
        set_setting(&conn, "key", &serde_json::json!("v2")).unwrap();
        let val = get_setting(&conn, "key").unwrap();
        assert_eq!(val, Some(serde_json::json!("v2")));
    }

    // ---- is_network_enabled: declared switches only, fail closed ----

    #[test]
    fn an_undeclared_switch_reads_as_off() {
        let (conn, _path) = test_db();
        // A misspelled or undeclared key is a code bug: it must never be
        // treated as permission, even if a row says `true`.
        set_setting(
            &conn,
            "network_nonexistent_switch",
            &serde_json::Value::Bool(true),
        )
        .unwrap();
        assert!(!is_network_enabled(&conn, "network_nonexistent_switch"));
        // A declared key that is not a boolean is not a switch either.
        assert!(!is_network_enabled(&conn, "launch_mode"));
    }

    #[test]
    fn test_is_network_enabled_json_bool_true() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::Bool(true),
        )
        .unwrap();
        assert!(is_network_enabled(&conn, "network_mojang_metadata_enabled"));
    }

    #[test]
    fn test_is_network_enabled_json_bool_false() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::Bool(false),
        )
        .unwrap();
        assert!(!is_network_enabled(
            &conn,
            "network_mojang_metadata_enabled"
        ));
    }

    #[test]
    fn test_is_network_enabled_string_true() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::String("true".into()),
        )
        .unwrap();
        assert!(is_network_enabled(&conn, "network_mojang_metadata_enabled"));
    }

    #[test]
    fn test_is_network_enabled_string_false() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::String("false".into()),
        )
        .unwrap();
        assert!(!is_network_enabled(
            &conn,
            "network_mojang_metadata_enabled"
        ));
    }

    #[test]
    fn test_is_network_enabled_roundtrip_bool() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::Bool(false),
        )
        .unwrap();
        assert!(!is_network_enabled(
            &conn,
            "network_mojang_metadata_enabled"
        ));
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::Bool(true),
        )
        .unwrap();
        assert!(is_network_enabled(&conn, "network_mojang_metadata_enabled"));
    }

    #[test]
    fn test_is_network_enabled_roundtrip_string() {
        let (conn, _path) = test_db();
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::String("false".into()),
        )
        .unwrap();
        assert!(!is_network_enabled(
            &conn,
            "network_mojang_metadata_enabled"
        ));
        set_setting(
            &conn,
            "network_mojang_metadata_enabled",
            &serde_json::Value::String("true".into()),
        )
        .unwrap();
        assert!(is_network_enabled(&conn, "network_mojang_metadata_enabled"));
    }

    // ---- Lockdown Mode (global network block) ----

    #[test]
    fn test_lockdown_defaults_to_off() {
        let (conn, _path) = test_db();
        assert!(!is_lockdown_enabled(&conn));
    }

    #[test]
    fn test_lockdown_disables_all_endpoints() {
        let (conn, _path) = test_db();
        set_setting(&conn, "network_modrinth_enabled", &serde_json::json!(true)).unwrap();
        set_setting(
            &conn,
            "network_registry_sync_enabled",
            &serde_json::json!(true),
        )
        .unwrap();
        set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(true)).unwrap();

        assert!(is_lockdown_enabled(&conn));
        // Every endpoint is blocked, even ones that are individually enabled
        // and keys that have never been written.
        assert!(!is_network_enabled(&conn, "network_modrinth_enabled"));
        assert!(!is_network_enabled(&conn, "network_registry_sync_enabled"));
        assert!(!is_network_enabled(&conn, "network_github_oauth_enabled"));
        assert!(!is_network_enabled(&conn, "nonexistent_network_key"));
    }

    #[test]
    fn test_lockdown_off_does_not_block_endpoints() {
        let (conn, _path) = test_db();
        set_setting(&conn, "network_lockdown_enabled", &serde_json::json!(false)).unwrap();
        set_setting(&conn, "network_modrinth_enabled", &serde_json::json!(true)).unwrap();

        assert!(!is_lockdown_enabled(&conn));
        assert!(is_network_enabled(&conn, "network_modrinth_enabled"));
    }

    // ---- record_co_crash ----

    #[test]
    fn test_record_co_crash_increments() {
        let (conn, _path) = test_db();
        record_co_crash(&conn, "a", "b").unwrap();
        record_co_crash(&conn, "a", "b").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT crash_count FROM local_crash_telemetry WHERE mod_a_id = ?1 AND mod_b_id = ?2",
                rusqlite::params!["a", "b"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_record_co_crash_symmetric() {
        let (conn, _path) = test_db();
        record_co_crash(&conn, "a", "b").unwrap();
        record_co_crash(&conn, "b", "a").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT crash_count FROM local_crash_telemetry WHERE mod_a_id = ?1 AND mod_b_id = ?2",
                rusqlite::params!["a", "b"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    // ---- flag rate limiting ----

    #[test]
    fn test_flag_rate_limit_empty_can_flag() {
        let (conn, _path) = test_db();
        let now = 1_000_000_000i64;
        let status = get_flag_rate_limit_status(&conn, now).unwrap();
        assert!(status.can_flag);
        assert_eq!(status.remaining_hour, 5);
        assert_eq!(status.remaining_day, 20);
    }

    #[test]
    fn test_flag_rate_limit_after_three() {
        let (conn, _path) = test_db();
        let now = 1_000_000_000i64;
        for _ in 0..3 {
            record_flag_submission(&conn, now).unwrap();
        }
        let status = get_flag_rate_limit_status(&conn, now).unwrap();
        assert_eq!(status.remaining_hour, 2);
        assert_eq!(status.remaining_day, 17);
    }

    #[test]
    fn test_flag_rate_limit_hourly_exceeded() {
        let (conn, _path) = test_db();
        let now = 1_000_000_000i64;
        for _ in 0..5 {
            record_flag_submission(&conn, now).unwrap();
        }
        let status = get_flag_rate_limit_status(&conn, now).unwrap();
        assert!(!status.can_flag);
        assert_eq!(status.remaining_hour, 0);
    }

    #[test]
    fn test_flag_rate_limit_daily_exceeded() {
        let (conn, _path) = test_db();
        let now = 1_000_000_000i64;
        for _ in 0..20 {
            record_flag_submission(&conn, now).unwrap();
        }
        let status = get_flag_rate_limit_status(&conn, now).unwrap();
        assert!(!status.can_flag);
        assert_eq!(status.remaining_day, 0);
    }

    // ---- crash attribution ----

    #[test]
    fn test_increment_confirmation_inserts() {
        let (conn, _path) = test_db();
        increment_confirmation(&conn, "fp1", "mod_a").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT confirm_count FROM crash_attribution WHERE fingerprint = ?1 AND mod_id = ?2",
                rusqlite::params!["fp1", "mod_a"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_increment_confirmation_increments() {
        let (conn, _path) = test_db();
        increment_confirmation(&conn, "fp1", "mod_a").unwrap();
        increment_confirmation(&conn, "fp1", "mod_a").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT confirm_count FROM crash_attribution WHERE fingerprint = ?1 AND mod_id = ?2",
                rusqlite::params!["fp1", "mod_a"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_get_confirmed_attribution() {
        let (conn, _path) = test_db();
        increment_confirmation(&conn, "fp1", "mod_b").unwrap();
        increment_confirmation(&conn, "fp1", "mod_a").unwrap();
        increment_confirmation(&conn, "fp1", "mod_a").unwrap();
        let results = get_confirmed_attribution(&conn, "fp1").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].mod_id, "mod_a");
        assert_eq!(results[0].confirm_count, 2);
        assert_eq!(results[1].mod_id, "mod_b");
        assert_eq!(results[1].confirm_count, 1);
    }

    #[test]
    fn test_schema_v6_import_provenance_roundtrip_and_cascade() {
        let (conn, _path) = test_db();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, LOCAL_STATE_SCHEMA_VERSION);

        let row = InstanceRow {
            instance_id: "imported-instance".into(),
            name: "Imported Instance".into(),
            minecraft_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: Some("instance-icon.png".into()),
            launch_mode_override: "delegated".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let record = InstanceImportRecord {
            instance_id: row.instance_id.clone(),
            launcher_kind: "prism".into(),
            installation_key: "prism:test".into(),
            source_instance_key: "source".into(),
            source_path: "C:/Prism/instances/source/minecraft".into(),
            source_metadata_json: "{}".into(),
            imported_settings_json: "{}".into(),
            source_fingerprint: "fingerprint".into(),
            imported_at: now.clone(),
            updated_at: now,
            last_result: "imported".into(),
        };
        let files = vec![InstanceImportFileRecord {
            relative_path: "mods/example.jar".into(),
            sha256: "abc".into(),
            size: 3,
            source_modified_ns: None,
        }];
        replace_instance_import(&conn, &record, &files).unwrap();
        assert_eq!(
            find_instance_import_by_source(&conn, "prism", "prism:test", "source")
                .unwrap()
                .unwrap()
                .instance_id,
            row.instance_id
        );
        assert_eq!(
            list_instance_import_files(&conn, &row.instance_id).unwrap(),
            files
        );

        delete_instance(&conn, &row.instance_id).unwrap();
        assert!(get_instance_import(&conn, &row.instance_id)
            .unwrap()
            .is_none());
        assert!(list_instance_import_files(&conn, &row.instance_id)
            .unwrap()
            .is_empty());
    }

    // -----------------------------------------------------------------------
    // Schema v4 migration and java columns
    // -----------------------------------------------------------------------

    #[test]
    fn test_schema_v4_adds_java_columns() {
        let (_conn, path) = test_db();
        // Drop and recreate at v3 to test migration
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS schema_version;
             DROP TABLE IF EXISTS user_instances;
             DROP TABLE IF EXISTS user_settings;
             DROP TABLE IF EXISTS local_crash_telemetry;
             DROP TABLE IF EXISTS mcp_approval_grants;
             DROP TABLE IF EXISTS flag_submissions;
             DROP TABLE IF EXISTS crash_events;
             DROP TABLE IF EXISTS crash_survivals;
             DROP TABLE IF EXISTS crash_survival_mods;
             DROP TABLE IF EXISTS crash_attribution;
             DROP TABLE IF EXISTS crash_ruled_out;",
        )
        .unwrap();
        drop(conn);

        // Fresh DB + migration
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        run_migrations(&conn).unwrap();

        // Verify java columns exist
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(minecraft_instances)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            cols.contains(&"java_path".to_string()),
            "java_path column should exist: {cols:?}"
        );
        assert!(
            cols.contains(&"java_incompatible_override".to_string()),
            "java_incompatible_override column should exist: {cols:?}"
        );

        // Verify default setting
        let val = get_setting(&conn, "java_runtime_mode").unwrap().unwrap();
        assert_eq!(val.as_str(), Some("automatic"));
    }

    #[test]
    fn test_update_instance_java_roundtrip() {
        let (conn, _path) = test_db();
        // Create a test instance
        let row = InstanceRow {
            instance_id: "java-test-instance".into(),
            name: "Java Test".into(),
            minecraft_version: "1.21".into(),
            loader: "fabric".into(),
            loader_version: "0.16.0".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "g1gc".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();

        // Set per-instance Java path
        update_instance_java(
            &conn,
            "java-test-instance",
            Some("/custom/java"),
            false,
            None,
        )
        .unwrap();

        // Read back and verify
        let fetched = get_instance(&conn, "java-test-instance").unwrap().unwrap();
        assert_eq!(fetched.java_path.as_deref(), Some("/custom/java"));
        assert!(!fetched.java_incompatible_override);

        // Clear per-instance Java path
        update_instance_java(&conn, "java-test-instance", None, true, Some("-XX:+UseZGC")).unwrap();
        let fetched = get_instance(&conn, "java-test-instance").unwrap().unwrap();
        assert!(fetched.java_path.is_none());
        assert!(fetched.java_incompatible_override);
        assert_eq!(fetched.jvm_custom_args, "-XX:+UseZGC");
    }

    #[test]
    fn test_list_instances_includes_java_columns() {
        let (conn, _path) = test_db();
        let row = InstanceRow {
            instance_id: "list-java-test".into(),
            name: "List Java Test".into(),
            minecraft_version: "1.21".into(),
            loader: "neoforge".into(),
            loader_version: "21.1.0".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 2048,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "zgc".into(),
            jvm_custom_args: "-XX:+AlwaysPreTouch".into(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: Some("/opt/java21/bin/java".into()),
            java_incompatible_override: true,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();

        let instances = list_instances(&conn).unwrap();
        let found = instances
            .iter()
            .find(|i| i.instance_id == "list-java-test")
            .unwrap();
        assert_eq!(found.java_path.as_deref(), Some("/opt/java21/bin/java"));
        assert!(found.java_incompatible_override);
        assert_eq!(found.jvm_memory_mode, "manual");
    }

    // ---- get_pair_crash_count ----

    #[test]
    fn test_get_pair_crash_count_basic() {
        let (conn, _path) = test_db();
        // No data yet
        let count = get_pair_crash_count(&conn, "mod-a", "mod-b").unwrap();
        assert_eq!(count, 0);

        record_co_crash(&conn, "mod-a", "mod-b").unwrap();
        let count = get_pair_crash_count(&conn, "mod-a", "mod-b").unwrap();
        assert_eq!(count, 1);

        // Symmetric lookup
        let count = get_pair_crash_count(&conn, "mod-b", "mod-a").unwrap();
        assert_eq!(count, 1);

        // Non-existent pair
        let count = get_pair_crash_count(&conn, "mod-a", "mod-z").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_instance_pack_files_roundtrip_and_replace() {
        let (_conn, path) = test_db();
        // Use FK-enforcing connection for all operations, otherwise the cascade
        // and FK checks prove nothing (foreign_keys defaults OFF on raw open).
        let conn = local_state_connection(&path).unwrap();
        let row = InstanceRow {
            instance_id: "pack-inst-1".into(),
            name: "Pack Inst".into(),
            minecraft_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();

        let mut files = vec![
            InstancePackFile {
                relative_path: "mods/a.jar".into(),
                sha256: "a".repeat(64),
                size: 123,
            },
            InstancePackFile {
                relative_path: "config/b.toml".into(),
                sha256: "b".repeat(64),
                size: 456,
            },
        ];
        files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        replace_instance_pack_files(&conn, &row.instance_id, &files).unwrap();
        let listed = list_instance_pack_files(&conn, &row.instance_id).unwrap();
        assert_eq!(listed, files);

        // Replace-all must replace, not append.
        let replacement = vec![InstancePackFile {
            relative_path: "mods/c.jar".into(),
            sha256: "c".repeat(64),
            size: 789,
        }];
        replace_instance_pack_files(&conn, &row.instance_id, &replacement).unwrap();
        let listed2 = list_instance_pack_files(&conn, &row.instance_id).unwrap();
        assert_eq!(listed2, replacement);
    }

    #[test]
    fn test_instance_pack_files_delete() {
        let (_conn, path) = test_db();
        let conn = local_state_connection(&path).unwrap();
        let row = InstanceRow {
            instance_id: "pack-inst-del".into(),
            name: "Del".into(),
            minecraft_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();
        let files = vec![InstancePackFile {
            relative_path: "mods/a.jar".into(),
            sha256: "a".repeat(64),
            size: 1,
        }];
        replace_instance_pack_files(&conn, &row.instance_id, &files).unwrap();
        delete_instance_pack_files(&conn, &row.instance_id).unwrap();
        assert!(list_instance_pack_files(&conn, &row.instance_id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_instance_pack_files_cascade_on_instance_delete() {
        let (_conn, path) = test_db();
        let conn = local_state_connection(&path).unwrap();
        let row = InstanceRow {
            instance_id: "pack-inst-cascade".into(),
            name: "Cascade".into(),
            minecraft_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();
        let files = vec![InstancePackFile {
            relative_path: "mods/a.jar".into(),
            sha256: "a".repeat(64),
            size: 1,
        }];
        replace_instance_pack_files(&conn, &row.instance_id, &files).unwrap();
        // Must be FK-enforced: deleting the instance must cascade.
        delete_instance(&conn, &row.instance_id).unwrap();
        // Re-open with FK ON to verify no orphan remains (raw open would hide the bug).
        let conn2 = local_state_connection(&path).unwrap();
        assert!(list_instance_pack_files(&conn2, &row.instance_id)
            .unwrap()
            .is_empty());
        // Direct SQL check also: no rows for that instance_id
        let count: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM instance_pack_files WHERE instance_id = ?1",
                [&row.instance_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_instance_pack_files_empty_replace_clears() {
        let (_conn, path) = test_db();
        let conn = local_state_connection(&path).unwrap();
        let row = InstanceRow {
            instance_id: "pack-inst-empty".into(),
            name: "Empty".into(),
            minecraft_version: "1.21.1".into(),
            loader: "fabric".into(),
            loader_version: "0.16.9".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "auto".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();
        let files = vec![InstancePackFile {
            relative_path: "mods/a.jar".into(),
            sha256: "a".repeat(64),
            size: 1,
        }];
        replace_instance_pack_files(&conn, &row.instance_id, &files).unwrap();
        replace_instance_pack_files(&conn, &row.instance_id, &[]).unwrap();
        assert!(list_instance_pack_files(&conn, &row.instance_id)
            .unwrap()
            .is_empty());
    }

    // -----------------------------------------------------------------------
    // Several processes sharing one database
    // -----------------------------------------------------------------------

    fn fresh_path() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("agora-test-shared-{}-{n}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    #[test]
    fn concurrent_first_starts_all_migrate_cleanly() {
        let path = fresh_path();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    init_local_state_db(&path).map_err(|e| e.to_string())
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("every concurrent start succeeds");
        }
        let conn = local_state_connection(&path).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, LOCAL_STATE_SCHEMA_VERSION);
    }

    /// rusqlite gives every connection a 5 s busy timeout, which is what lets
    /// the desktop app and the CLI write to one database. Pinned here so a
    /// change of library or open flags cannot quietly turn it into an instant
    /// "database is locked".
    #[test]
    fn a_write_waits_for_another_connections_write() {
        let path = fresh_path();
        init_local_state_db(&path).unwrap();

        let holder = local_state_connection(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            holder.execute_batch("COMMIT").unwrap();
        });

        let writer = local_state_connection(&path).unwrap();
        let started = std::time::Instant::now();
        set_setting(&writer, "busy_probe", &serde_json::json!(true))
            .expect("waits for the lock instead of failing");
        assert!(started.elapsed() >= std::time::Duration::from_millis(200));
        release.join().unwrap();
    }

    #[test]
    fn row_resync_follows_the_manifest_name_and_versions() {
        let (conn, _path) = test_db();
        let row = InstanceRow {
            instance_id: "resync".into(),
            name: "Old Name".into(),
            minecraft_version: "1.20.1".into(),
            loader: "fabric".into(),
            loader_version: "0.15.0".into(),
            is_modpack: false,
            is_locked: false,
            last_launched_at: None,
            jvm_memory_mb: 4096,
            jvm_memory_mode: "manual".into(),
            jvm_gc: "auto".into(),
            jvm_custom_args: String::new(),
            jvm_always_pre_touch: false,
            created_at: chrono::Utc::now().to_rfc3339(),
            java_path: None,
            java_incompatible_override: false,
            icon_path: None,
            launch_mode_override: "delegated".into(),
            import_source: None,
            total_play_ms: 0,
        };
        upsert_instance(&conn, "minecraft", &row).unwrap();
        let mut manifest: crate::models::InstanceManifest =
            serde_json::from_value(serde_json::json!({
                "instance_id": "resync",
                "name": "Restored Name",
                "minecraft_version": "1.21.1",
                "loader": "fabric",
                "loader_version": "0.16.9",
                "is_locked": true,
                "mods": []
            }))
            .unwrap();
        assert_eq!(
            sync_instance_row_from_manifest(&conn, "resync", &manifest).unwrap(),
            1
        );
        let (name, version, locked): (String, String, bool) = conn
            .query_row(
                "SELECT instances.name, minecraft_instances.minecraft_version,
                        minecraft_instances.is_locked
                   FROM instances
                   JOIN minecraft_instances
                     ON minecraft_instances.instance_id = instances.instance_id
                  WHERE instances.instance_id = 'resync'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (name.as_str(), version.as_str(), locked),
            ("Restored Name", "1.21.1", true)
        );

        // A manifest with no usable name keeps the row's name.
        manifest.name = "  ".into();
        sync_instance_row_from_manifest(&conn, "resync", &manifest).unwrap();
        let name: String = conn
            .query_row(
                "SELECT name FROM instances WHERE instance_id = 'resync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "Restored Name");
    }
}
