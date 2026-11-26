use agora_core::ctx::{Ctx, TestClock};
use agora_core::db;
use agora_core::http_client::ClientCategory;
use agora_core::network_gate::{NetworkGate, SettingsGate};
use agora_core::test_seams::SettingsCacheEvent;
use rusqlite::{config::DbConfig, ffi, Connection, Error};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const CATEGORY: ClientCategory = ClientCategory::ConsentedContent;
const LOCKDOWN: &str = "network_lockdown_enabled";

fn context() -> (tempfile::TempDir, Ctx) {
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_owned());
    db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    db::set_setting(&conn, LOCKDOWN, &json!(false)).unwrap();
    (root, ctx)
}

fn assert_refresh_failure_denies(
    fail_at: SettingsCacheEvent,
    fault: impl Fn() -> Error + Send + Sync + 'static,
    reason: &str,
) {
    let (_root, ctx) = context();
    let attempted = Arc::new(AtomicBool::new(false));
    let armed = Arc::new(AtomicBool::new(false));
    ctx.test_seams.set_settings_cache_hook({
        let attempted = Arc::clone(&attempted);
        let armed = Arc::clone(&armed);
        Arc::new(move |event| {
            if event == fail_at && armed.load(Ordering::SeqCst) {
                attempted.store(true, Ordering::SeqCst);
                return Err(fault());
            }
            Ok(())
        })
    });
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("warm the gate with permission");
    let writer = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    db::set_setting(&writer, "network_modrinth_enabled", &json!(false)).unwrap();
    armed.store(true, Ordering::SeqCst);
    let result = gate.check(CATEGORY);
    assert!(
        attempted.load(Ordering::SeqCst),
        "the {reason} fault at {fail_at:?} was never exercised; the previously allowed gate returned {result:?}"
    );
    let error = result.expect_err("a failed refresh read must revoke cached permission");
    assert_eq!(error.code(), "ERR_NETWORK_SETTINGS_UNREADABLE", "{error}");
    assert!(
        error.to_string().to_ascii_lowercase().contains(reason),
        "the refusal must name the cause ({reason}): {error}"
    );
}

#[test]
fn locked_settings_db_denies_after_an_allowed_request() {
    assert_refresh_failure_denies(
        SettingsCacheEvent::ReadVersion,
        || {
            Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_BUSY),
                Some("database is locked".into()),
            )
        },
        "locked",
    );
}

#[test]
fn unreadable_settings_db_denies_after_an_allowed_request() {
    assert_refresh_failure_denies(
        SettingsCacheEvent::ReadValues,
        || {
            Error::SqliteFailure(
                ffi::Error::new(ffi::SQLITE_CANTOPEN),
                Some("unable to open database file".into()),
            )
        },
        "unable to open",
    );
}

#[test]
fn missing_settings_version_row_denies_after_an_allowed_request() {
    assert_refresh_failure_denies(
        SettingsCacheEvent::ReadVersion,
        || Error::QueryReturnedNoRows,
        "row",
    );
}

#[test]
fn version_and_values_share_one_snapshot_when_a_write_lands_between_reads() {
    let (_root, ctx) = context();
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("warm the gate with permission");
    let path = ctx.paths.local_state_db();
    let writer = db::local_state_connection(&path).unwrap();
    // Invalidate the warmed cache without changing the category's policy.
    db::set_setting(&writer, "network_modrinth_enabled", &json!(false)).unwrap();

    let commits = Arc::new(AtomicUsize::new(0));
    ctx.test_seams.set_settings_cache_hook({
        let commits = Arc::clone(&commits);
        Arc::new(move |event| {
            if event == SettingsCacheEvent::BeforeValuesRead && commits.load(Ordering::SeqCst) == 0
            {
                // This callback is synchronous. The version reader cannot reach the values
                // until this separate WAL connection has committed the new policy.
                let writer = db::local_state_connection(&path).unwrap();
                writer.busy_timeout(Duration::from_secs(2)).unwrap();
                db::set_setting(&writer, LOCKDOWN, &json!(true)).unwrap();
                commits.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    });

    let during_write = gate.check(CATEGORY);
    assert_eq!(
        commits.load(Ordering::SeqCst),
        1,
        "writer never committed in the window after version read and before values read"
    );
    during_write.expect("the in-flight check must see the policy in its version's snapshot");
    let next = gate
        .check(CATEGORY)
        .expect_err("the next check must see the committed Lockdown and its version");
    assert_eq!(next.code(), "ERR_NETWORK_LOCKDOWN", "{next}");
}

#[test]
fn periodic_backstop_picks_up_an_unversioned_change_at_expiry() {
    let (_root, ctx) = context();
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH));
    let ctx = ctx.with_clock(clock.clone());
    let period = Duration::from_secs(23);
    ctx.test_seams.set_settings_refresh_period(period);
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("warm the gate with permission");

    let writer = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    // Model a writer outside Agora, even if the eventual bump uses SQL triggers.
    writer
        .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .unwrap();
    assert_eq!(
        writer
            .execute(
                "UPDATE user_settings SET value_json = 'true' WHERE key = ?1",
                [LOCKDOWN]
            )
            .unwrap(),
        1
    );

    clock.advance(period - Duration::from_secs(1));
    let before_expiry = gate.check(CATEGORY);
    clock.advance(Duration::from_secs(1));
    let at_expiry = gate.check(CATEGORY);
    assert!(ctx.test_seams.settings_refresh_period_hits() > 0,
        "the backstop period override was never read; before expiry: {before_expiry:?}, at expiry: {at_expiry:?}");
    before_expiry.expect("an unchanged version still uses its cached values before expiry");
    let error = at_expiry.expect_err("at expiry the gate must pick up the unversioned Lockdown");
    assert_eq!(error.code(), "ERR_NETWORK_LOCKDOWN", "{error}");
}

#[test]
fn unchanged_requests_read_versions_without_opening_connections_or_loading_values() {
    let (_root, ctx) = context();
    let ctx = ctx.with_clock(Arc::new(TestClock::new(SystemTime::UNIX_EPOCH)));
    let work = Arc::new(Mutex::new(Vec::new()));
    ctx.test_seams.set_settings_cache_hook({
        let work = Arc::clone(&work);
        Arc::new(move |event| {
            work.lock().unwrap().push(event);
            Ok(())
        })
    });
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("warm the gate with permission");
    let warmed = std::mem::take(&mut *work.lock().unwrap());
    assert!(
        warmed.contains(&SettingsCacheEvent::OpenConnection),
        "the initial settings connection-open observation never fired: {warmed:?}"
    );
    assert!(
        warmed.contains(&SettingsCacheEvent::ReadValues),
        "the cold settings-load observation never fired: {warmed:?}"
    );

    const REQUESTS: usize = 5;
    for _ in 0..REQUESTS {
        gate.check(CATEGORY)
            .expect("unchanged settings still permit the request");
    }
    let work = work.lock().unwrap();
    let count = |event| work.iter().filter(|seen| **seen == event).count();
    assert_eq!(
        (
            count(SettingsCacheEvent::ReadVersion),
            count(SettingsCacheEvent::OpenConnection),
            count(SettingsCacheEvent::ReadValues)
        ),
        (REQUESTS, 0, 0),
        "warm requests must do one version read each, with zero connection opens or settings loads"
    );
}

#[test]
fn schema_step_32_adds_a_version_to_an_existing_database_and_preserves_settings() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("local_state.db");
    let conn = db::local_state_connection(&path).unwrap();
    db::run_migrations_to(&conn, 31).unwrap();
    conn.execute(
        "INSERT INTO user_settings (key, value_json) VALUES (?1, 'true')",
        [LOCKDOWN],
    )
    .unwrap();
    drop(conn);

    db::init_local_state_db(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        db::recorded_schema_version(&conn).unwrap(),
        32,
        "startup did not apply the settings version schema step"
    );
    assert_eq!(db::get_setting(&conn, LOCKDOWN).unwrap(), Some(json!(true)));
    let version =
        db::settings_version(&conn).expect("step 32 must create a readable settings version");
    db::init_local_state_db(&path).unwrap();
    assert_eq!(
        db::settings_version(&conn).unwrap(),
        version,
        "reopening a migrated database with no settings writes must preserve its version"
    );
}
