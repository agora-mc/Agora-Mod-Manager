use rusqlite::Connection;

use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How long loaded settings are trusted when the version has not changed.
///
/// The version catches every write Agora makes. This period is the backstop for one it does
/// not: a settings row changed by something else (a hand edit, a restored older database) with
/// no version bump is still noticed within half a minute (MASTER_SPEC §32.9).
pub const SETTINGS_REFRESH_PERIOD: Duration = Duration::from_secs(30);

/// The database, clock and test seams one settings read uses.
///
/// Passed in rather than read from a static, so a test's clock and seams reach the read on
/// whatever thread it happens, and no other context sees them (§32.9).
#[derive(Clone)]
pub struct SettingsReads {
    db_path: PathBuf,
    clock: Arc<dyn crate::ctx::Clock>,
    seams: crate::test_seams::TestSeams,
}

impl SettingsReads {
    /// The reads a context makes: its own database, clock and seams.
    pub fn of(ctx: &Ctx) -> Self {
        Self {
            db_path: ctx.paths.local_state_db(),
            clock: Arc::clone(&ctx.clock),
            seams: ctx.test_seams.clone(),
        }
    }

    /// Reads with no seams and the wall clock, for a caller that only has a database path.
    pub fn standalone(db_path: PathBuf) -> Self {
        Self {
            db_path,
            clock: Arc::new(crate::ctx::WallClock),
            seams: crate::test_seams::TestSeams::default(),
        }
    }
}

/// A settings version and the values read in the same snapshot as it.
///
/// Handed out by [`SettingsCache::load`]; holding one is holding a decision that was true when
/// it was read, which is why the gate asks for a fresh one before every request.
pub struct SettingsSnapshot {
    version: i64,
    /// When the values were read: the backstop period counts from here, so a warm check cannot
    /// postpone the reload that catches a write nobody versioned.
    loaded_at: SystemTime,
    /// When this snapshot was last checked: a clock that is now earlier than this has moved
    /// backwards, and the time it has been trusted is unknown.
    checked_at: SystemTime,
    values: HashMap<String, serde_json::Value>,
}

impl SettingsSnapshot {
    /// The settings version these values were published with.
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Whether Lockdown Mode forbids outbound requests in this snapshot.
    ///
    /// Reads as it does from the database: a value that is present but not understood denies
    /// rather than being read as off.
    pub fn lockdown_denies_network(&self) -> LauncherResult<bool> {
        crate::db::lockdown_value_denies(self.value("network_lockdown_enabled"))
    }

    /// Whether the declared network switch `key` permits its request in this snapshot.
    pub fn network_enabled(&self, key: &str) -> bool {
        // Lockdown overrides every endpoint, exactly as it does when read from the database.
        if self.lockdown_denies_network().unwrap_or(true) {
            return false;
        }
        crate::db::switch_enabled(key, self.value(key))
    }

    fn value(&self, key: &str) -> Option<serde_json::Value> {
        self.values.get(key).cloned()
    }
}

/// One settings cache per context, shared by its clones (MASTER_SPEC §32.9).
///
/// A check reads the version on a retained connection — one single-row read — and reloads the
/// values only when the version changed or the backstop period expired. The version and the
/// values are read inside one read transaction, so a write that lands between the two reads
/// cannot pair a new version with the values the user has just revoked.
///
/// A refresh that fails for any reason denies and forgets what it had: it never falls back to
/// the last cached "allowed".
#[derive(Default)]
pub struct SettingsCache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    snapshot: Option<Arc<SettingsSnapshot>>,
    /// The connection the version is read on, retained so a warm check costs one read and no
    /// open. Dropped at every periodic refresh and after any read error, so a database file
    /// that was replaced on disk is opened again rather than read through the old handle.
    connection: Option<Connection>,
}

impl SettingsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check the version and, when it changed, reload the settings this context uses.
    ///
    /// This is the admission check for a major action (§32.9): `Err` refuses the action and
    /// says why.
    pub fn refresh(&self, ctx: &Ctx) -> LauncherResult<()> {
        self.load(&SettingsReads::of(ctx)).map(drop)
    }

    /// The settings as of now, reading the version and reloading only when they changed.
    pub fn load(&self, reads: &SettingsReads) -> LauncherResult<Arc<SettingsSnapshot>> {
        // A poisoned lock means a refresh panicked mid-read, with the values it was building and
        // a connection whose transaction nobody finished. Neither is a snapshot anyone verified,
        // so both are dropped: the next check opens the database again and reads it fresh.
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.snapshot = None;
                state.connection = None;
                state
            }
        };
        let period = reads
            .seams
            .settings_refresh_period_override()
            .unwrap_or(SETTINGS_REFRESH_PERIOD);
        let now = reads.clock.now();
        let expired = state.snapshot.as_ref().is_none_or(|snapshot| {
            // The backstop period counts from the last *load*: a request every few seconds must
            // not keep postponing the reload that catches a change nobody versioned.
            let period_passed = match now.duration_since(snapshot.loaded_at) {
                Ok(elapsed) => elapsed >= period,
                // The clock moved backwards, so no elapsed time can be established. That is not
                // evidence the period has not passed.
                Err(_) => true,
            };
            // And a clock that moved backwards since the last *check* means the time this
            // snapshot has been trusted is unknown, so it is not trusted further.
            let clock_moved_backwards = now < snapshot.checked_at;
            period_passed || clock_moved_backwards
        });
        if expired {
            state.connection = None;
        }
        match read_settings(&mut state, reads, now, expired) {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                state.snapshot = None;
                state.connection = None;
                Err(error)
            }
        }
    }
}

/// Read the version, and the values too when they can no longer be trusted.
fn read_settings(
    state: &mut CacheState,
    reads: &SettingsReads,
    now: SystemTime,
    expired: bool,
) -> LauncherResult<Arc<SettingsSnapshot>> {
    if state.connection.is_none() {
        reads
            .seams
            .settings_cache_event(crate::test_seams::SettingsCacheEvent::OpenConnection)
            .map_err(|error| unreadable("could not open the settings database", &error))?;
        state.connection = Some(
            crate::db::local_state_connection(&reads.db_path)
                .map_err(|error| unreadable("could not open the settings database", &error))?,
        );
    }
    let conn = state
        .connection
        .as_ref()
        .expect("the connection was opened above");
    // One snapshot for both reads: a version read outside the transaction that reads the
    // values could pair the new version with the old values, and so keep a permission the
    // user has just revoked alive for the whole period.
    let transaction = conn
        .unchecked_transaction()
        .map_err(|error| unreadable("could not begin a settings read", &error))?;
    let version = crate::db::settings_version(conn)
        .map_err(|error| unreadable("could not read the settings version", &error))?;
    reads
        .seams
        .settings_cache_event(crate::test_seams::SettingsCacheEvent::ReadVersion)
        .map_err(|error| unreadable("could not read the settings version", &error))?;
    let cached = state
        .snapshot
        .as_ref()
        .filter(|snapshot| snapshot.version == version && !expired)
        .cloned();
    let snapshot = match cached {
        Some(snapshot) => {
            // The version still matches and nothing expired, so the values stand. The check is
            // recorded for the backwards-clock rule, but the period keeps running from the load.
            Arc::new(SettingsSnapshot {
                version: snapshot.version,
                loaded_at: snapshot.loaded_at,
                checked_at: now,
                values: snapshot.values.clone(),
            })
        }
        None => {
            reads
                .seams
                .settings_cache_event(crate::test_seams::SettingsCacheEvent::BeforeValuesRead)
                .map_err(|error| unreadable("could not read the settings", &error))?;
            reads
                .seams
                .settings_cache_event(crate::test_seams::SettingsCacheEvent::ReadValues)
                .map_err(|error| unreadable("could not read the settings", &error))?;
            let values = load_values(conn).map_err(|error| {
                unreadable("could not read the settings from the database", &error)
            })?;
            Arc::new(SettingsSnapshot {
                version,
                loaded_at: now,
                checked_at: now,
                values,
            })
        }
    };
    drop(transaction);
    state.snapshot = Some(Arc::clone(&snapshot));
    Ok(snapshot)
}

/// Every stored setting, as the database holds it.
///
/// A value that is not readable JSON is `Null`, as [`crate::db::get_setting`] returns it, so a
/// damaged value denies exactly as it does when read row by row.
fn load_values(conn: &Connection) -> anyhow::Result<HashMap<String, serde_json::Value>> {
    let rows = list(conn)?;
    Ok(rows
        .into_iter()
        .map(|(key, text)| {
            let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            (key, value)
        })
        .collect())
}

/// The one refusal for a settings read that failed: it denies, and says what could not be read.
fn unreadable(what: &str, error: &dyn std::fmt::Display) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_NETWORK_SETTINGS_UNREADABLE".into(),
        message: format!(
            "Refusing: the settings could not be checked, so what the user permits is unknown \
             ({what}: {error}). Try again once the settings database can be read."
        ),
    }
}

/// Refuse a major action whose settings could not be checked (MASTER_SPEC §32.9).
///
/// A major action changes what is on the player's disk or what leaves this machine, and the
/// settings decide part of that: a pack file outside the usual folders is only allowed under
/// reduced security mode, a runtime is only installed for the game it belongs to. Reading a
/// stale answer is how an action proceeds on a permission the user has revoked, so a failed
/// check refuses the action and says to try again rather than guessing.
pub fn admit_major_action(ctx: &Ctx) -> LauncherResult<()> {
    ctx.settings_cache.refresh(ctx)
}

/// Refuse a major action whose settings could not be checked, from a database path alone.
///
/// For the jobs that run outside a context — a scheduled catalog sync, a framework repair that
/// only holds the app paths — there is no cache to share and nobody to share it with, so the
/// check is one read of its own. It is still a version check against the same database the
/// context's cache reads, which is what makes "a setting the user just changed" and "a settings
/// store nobody can read" arrive as different answers.
pub fn admit_action_at(db_path: &std::path::Path) -> LauncherResult<()> {
    SettingsCache::new()
        .load(&SettingsReads::standalone(db_path.to_path_buf()))
        .map(drop)
}

// ---------------------------------------------------------------------------
// SettingsService — core-owned typed service
// ---------------------------------------------------------------------------

/// Core-owned service for reading user settings from `local_state.db`.
///
/// Every method opens a fresh read-only connection via `Ctx` paths; desktop
/// adapters must use this service rather than opening the database directly.
#[derive(Clone)]
pub struct SettingsService {
    ctx: Ctx,
}

/// **Reduced security mode.** One switch for the things Agora allows but
/// cannot vouch for: plugins that reach any public host or declare a long
/// host list, and pack files outside the usual content folders. Off by
/// default; turning it on is the user saying they accept that.
pub const REDUCED_SECURITY_SETTING: &str = "reduced_security_mode";

/// Whether the user has turned reduced security mode on.
pub fn reduced_security_enabled(ctx: &crate::ctx::Ctx) -> bool {
    SettingsService::new(ctx.clone())
        .get_bool(REDUCED_SECURITY_SETTING)
        .unwrap_or(false)
}

impl SettingsService {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }

    fn connection(&self) -> LauncherResult<rusqlite::Connection> {
        crate::db::local_state_connection(&self.ctx.paths.local_state_db()).map_err(|error| {
            LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: error.to_string(),
            }
        })
    }

    /// Get a setting value by key, through the typed registry when the key is
    /// declared there. A declared switch that holds a value Agora cannot use
    /// reads as its fail direction rather than as a different answer from the
    /// one the network gate reached. Returns `Ok(None)` for an undeclared key
    /// that does not exist.
    pub fn get(&self, key: &str) -> LauncherResult<Option<serde_json::Value>> {
        let conn = self.connection()?;
        if crate::settings_registry::is_registered(key) {
            return crate::settings_registry::read(&conn, key);
        }
        get(&conn, key).map_err(|error| LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        })
    }

    /// Set a setting value by key. The key must be declared in the registry and
    /// the value must match its type; undeclared keys and wrong types are
    /// refused with the reason and nothing is stored. Use
    /// [`set_registered`](Self::set_registered) when the caller wants the value
    /// as stored (a legacy boolean `0` becomes `false`).
    pub fn set(&self, key: &str, value: &serde_json::Value) -> LauncherResult<()> {
        self.set_registered(key, value).map(|_| ())
    }

    /// Set a declared setting, refusing an undeclared key or a value of the
    /// wrong type. Returns the value as stored.
    pub fn set_registered(
        &self,
        key: &str,
        value: &serde_json::Value,
    ) -> LauncherResult<serde_json::Value> {
        let stored = crate::settings_registry::validate(key, value)?;
        self.store(key, &stored)?;
        Ok(stored)
    }

    fn store(&self, key: &str, value: &serde_json::Value) -> LauncherResult<()> {
        let conn = self.connection()?;
        let text = serde_json::to_string(value).map_err(|error| LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        })?;
        set(&conn, key, &text).map_err(|error| LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        })?;
        Ok(())
    }

    /// Get a setting as a boolean. Accepts genuine JSON booleans and legacy
    /// string/number encodings left by older frontend versions. Returns
    /// `false` for missing or unexpected values.
    pub fn get_bool(&self, key: &str) -> LauncherResult<bool> {
        Ok(self.get(key)?.map(parse_bool_value).unwrap_or(false))
    }

    /// Get a setting as a boolean, falling back to `default` when the key is
    /// absent.
    ///
    /// [`get_bool`](Self::get_bool) reports a missing key as `Ok(false)`, so
    /// `get_bool(..).unwrap_or(true)` does NOT default an unset setting to on
    /// — the `unwrap_or` only covers the error case. Opt-out settings must use
    /// this instead.
    pub fn get_bool_or(&self, key: &str, default: bool) -> LauncherResult<bool> {
        Ok(self.get(key)?.map(parse_bool_value).unwrap_or(default))
    }

    /// Get a setting as an optional string. Returns `Ok(None)` when the key
    /// is absent or the value is not a JSON string.
    pub fn get_string(&self, key: &str) -> LauncherResult<Option<String>> {
        Ok(self.get(key)?.and_then(|v| v.as_str().map(String::from)))
    }

    /// List all settings as (key, raw JSON string) pairs.
    pub fn list(&self) -> LauncherResult<Vec<(String, String)>> {
        let conn = self.connection()?;
        list(&conn).map_err(|error| LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        })
    }

    /// List all settings as (key, parsed JSON value) pairs.
    pub fn list_parsed(&self) -> LauncherResult<Vec<(String, serde_json::Value)>> {
        let conn = self.connection()?;
        list_parsed(&conn).map_err(|error| LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        })
    }
}

fn parse_bool_value(value: serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => value,
        serde_json::Value::String(value) => value == "true" || value == "1",
        serde_json::Value::Number(value) => value.as_i64() == Some(1),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Free functions (preserved, take &Connection)
// ---------------------------------------------------------------------------

/// List all settings as (key, raw_JSON_string) pairs.
///
/// A row SQLite cannot hand back as text (a value stored with another type, a damaged page)
/// fails the whole read rather than being skipped: a setting nobody can read is not a setting
/// that is absent, and the callers of this function decide permissions from it.
pub fn list(conn: &Connection) -> anyhow::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value_json FROM user_settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// List all settings as (key, parsed_JSON_value) pairs.
///
/// Entries whose `value_json` is not valid JSON yield `Value::Null`.
pub fn list_parsed(conn: &Connection) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    let raw = list(conn)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let val = serde_json::from_str(&v).unwrap_or(serde_json::Value::Null);
            (k, val)
        })
        .collect())
}

/// Get a single setting by key.
///
/// Returns `None` when the key does not exist.
pub fn get(conn: &Connection, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let mut stmt = conn.prepare("SELECT value_json FROM user_settings WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => {
            let text: String = row.get(0)?;
            let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// Set a setting. The value string is parsed as JSON; if parsing fails it is
/// stored as a plain JSON string.  Returns the stored JSON value.
///
/// Delegates the write to [`crate::db::set_setting`], so this path publishes the settings
/// version in the same transaction as the value too. Two writers that each owned their own
/// `INSERT` is how a settings write could land without a version and be ignored by every
/// other process until the next backstop refresh.
pub fn set(conn: &Connection, key: &str, value: &str) -> anyhow::Result<serde_json::Value> {
    let parsed: serde_json::Value =
        serde_json::from_str(value).unwrap_or(serde_json::Value::String(value.to_owned()));
    crate::db::set_setting(conn, key, &parsed)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS user_settings (
                 key TEXT PRIMARY KEY,
                 value_json TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn list_empty_db() {
        let conn = mem_conn();
        let rows = list(&conn).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_parsed_empty_db() {
        let conn = mem_conn();
        let rows = list_parsed(&conn).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn get_bool_or_defaults_when_key_absent() {
        // Regression: `get_bool` reports a missing key as Ok(false), so
        // `get_bool(..).unwrap_or(true)` silently yields false for an unset
        // opt-out setting. That emptied the curated browse whitelist and made
        // Browse show nothing at all on a fresh profile.
        let conn = mem_conn();
        assert_eq!(
            get(&conn, "curated_source_modrinth_id_enabled").unwrap(),
            None
        );

        let absent = get(&conn, "curated_source_modrinth_id_enabled")
            .unwrap()
            .map(parse_bool_value);
        assert!(absent.unwrap_or(true), "absent key must honor the default");
        assert!(!absent.unwrap_or(false));

        // An explicit false still wins over the default.
        set(&conn, "curated_source_modrinth_id_enabled", "false").unwrap();
        let explicit = get(&conn, "curated_source_modrinth_id_enabled")
            .unwrap()
            .map(parse_bool_value);
        assert!(
            !explicit.unwrap_or(true),
            "explicit false must not be defaulted to true"
        );
    }

    #[test]
    fn set_and_get() {
        let conn = mem_conn();
        let stored = set(&conn, "test_key", "\"hello\"").unwrap();
        assert_eq!(stored, serde_json::Value::String("hello".into()));

        let got = get(&conn, "test_key").unwrap();
        assert_eq!(got, Some(serde_json::Value::String("hello".into())));
    }

    #[test]
    fn set_plain_string_fallback() {
        let conn = mem_conn();
        let stored = set(&conn, "plain", "hello").unwrap();
        assert_eq!(stored, serde_json::Value::String("hello".into()));

        let got = get(&conn, "plain").unwrap();
        assert_eq!(got, Some(serde_json::Value::String("hello".into())));
    }

    #[test]
    fn set_json_value() {
        let conn = mem_conn();
        let stored = set(&conn, "count", "42").unwrap();
        assert_eq!(stored, serde_json::Value::Number(42.into()));

        let got = get(&conn, "count").unwrap();
        assert_eq!(got, Some(serde_json::Value::Number(42.into())));
    }

    #[test]
    fn get_missing_key() {
        let conn = mem_conn();
        let got = get(&conn, "nope").unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn list_returns_all_keys() {
        let conn = mem_conn();
        set(&conn, "a", "\"1\"").unwrap();
        set(&conn, "b", "\"2\"").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "a");
        assert_eq!(rows[1].0, "b");
    }

    #[test]
    fn set_overwrites_existing() {
        let conn = mem_conn();
        set(&conn, "k", "\"old\"").unwrap();
        set(&conn, "k", "\"new\"").unwrap();
        let got = get(&conn, "k").unwrap();
        assert_eq!(got, Some(serde_json::Value::String("new".into())));
    }

    #[test]
    fn list_parsed_returns_valid_json() {
        let conn = mem_conn();
        set(&conn, "bool", "true").unwrap();
        set(&conn, "num", "99").unwrap();
        set(&conn, "str", "\"text\"").unwrap();
        let rows = list_parsed(&conn).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, serde_json::Value::Bool(true));
        assert_eq!(rows[1].1, serde_json::Value::Number(99.into()));
        assert_eq!(rows[2].1, serde_json::Value::String("text".into()));
    }

    // -----------------------------------------------------------------------
    // SettingsService tests
    // -----------------------------------------------------------------------

    fn seeded_ctx() -> (Ctx, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "agora-settings-service-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let ctx = Ctx::for_testing(root.clone());
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        // Seed known values via the free function so the service can read them.
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        set(&conn, "service_bool_key", "true").unwrap();
        set(&conn, "service_str_key", "\"hello\"").unwrap();
        conn.close().unwrap();
        (ctx, root)
    }

    #[test]
    fn service_get_returns_seeded_value() {
        let (ctx, root) = seeded_ctx();
        let svc = SettingsService::new(ctx);
        let val = svc.get("service_str_key").unwrap();
        assert_eq!(val, Some(serde_json::Value::String("hello".into())));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn service_get_missing_returns_none() {
        let (ctx, root) = seeded_ctx();
        let svc = SettingsService::new(ctx);
        let val = svc.get("nonexistent").unwrap();
        assert_eq!(val, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn service_get_bool_true() {
        let (ctx, root) = seeded_ctx();
        let svc = SettingsService::new(ctx);
        assert!(svc.get_bool("service_bool_key").unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn service_get_bool_missing_is_false() {
        let (ctx, root) = seeded_ctx();
        let svc = SettingsService::new(ctx);
        assert!(!svc.get_bool("nonexistent").unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn service_get_bool_accepts_legacy_string_true() {
        let (ctx, root) = seeded_ctx();
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        set(&conn, "string_true", "\"true\"").unwrap();
        conn.close().unwrap();
        let svc = SettingsService::new(ctx);
        assert!(svc.get_bool("string_true").unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn service_get_bool_accepts_legacy_number_one() {
        let (ctx, root) = seeded_ctx();
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        set(&conn, "number_one", "1").unwrap();
        conn.close().unwrap();
        let svc = SettingsService::new(ctx);
        assert!(svc.get_bool("number_one").unwrap());
        let _ = std::fs::remove_dir_all(root);
    }
}
