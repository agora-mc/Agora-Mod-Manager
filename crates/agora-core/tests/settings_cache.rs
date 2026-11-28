use agora_core::ctx::{Clock, Ctx, TestClock};
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
    assert!(
        db::recorded_schema_version(&conn).unwrap() >= 32,
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

fn assert_blob_denies_refresh(key: &str) {
    let (_root, ctx) = context();
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("initial permission");
    let writer = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    db::set_setting(&writer, key, &json!(false)).unwrap();
    assert_eq!(
        writer
            .execute(
                "UPDATE user_settings SET value_json = X'74727565' WHERE key = ?1",
                [key]
            )
            .unwrap(),
        1
    );

    let error = gate
        .check(CATEGORY)
        .expect_err("a settings row that cannot decode from SQLite must deny the refresh");
    assert_eq!(error.code(), "ERR_NETWORK_SETTINGS_UNREADABLE", "{error}");
}

#[test]
fn c3_fix_a_lockdown_row_stored_as_a_blob_denies_the_refresh() {
    assert_blob_denies_refresh(LOCKDOWN);
}

#[test]
fn c3_fix_an_unrelated_setting_stored_as_a_blob_denies_the_refresh() {
    assert_blob_denies_refresh("test.unreadable.setting");
}

fn write_lockdown_without_a_bump(ctx: &Ctx) {
    let writer = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    writer
        .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .unwrap();
    let version = db::settings_version(&writer).unwrap();
    assert_eq!(
        writer
            .execute(
                "UPDATE user_settings SET value_json = 'true' WHERE key = ?1",
                [LOCKDOWN]
            )
            .unwrap(),
        1
    );
    assert_eq!(
        db::settings_version(&writer).unwrap(),
        version,
        "fixture must leave the version unchanged"
    );
}

#[test]
fn c3_fix_a_panicking_refresh_discards_its_connection_and_snapshot() {
    let (_root, ctx) = context();
    let gate = SettingsGate::for_context(&ctx);
    let armed = Arc::new(AtomicBool::new(false));
    let panicked = Arc::new(AtomicBool::new(false));
    let opens = Arc::new(AtomicUsize::new(0));
    ctx.test_seams.set_settings_cache_hook({
        let (armed, panicked, opens) = (armed.clone(), panicked.clone(), opens.clone());
        Arc::new(move |event| {
            if event == SettingsCacheEvent::OpenConnection {
                opens.fetch_add(1, Ordering::SeqCst);
            }
            if event == SettingsCacheEvent::ReadVersion && armed.swap(false, Ordering::SeqCst) {
                panicked.store(true, Ordering::SeqCst);
                panic!("interrupt refresh while the cache mutex is held");
            }
            Ok(())
        })
    });
    gate.check(CATEGORY).expect("warm with allowed settings");
    let before = opens.load(Ordering::SeqCst);
    assert!(
        before > 0,
        "connection observation did not run on the cold check"
    );
    armed.store(true, Ordering::SeqCst);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gate.check(CATEGORY))).is_err()
    );
    assert!(
        panicked.load(Ordering::SeqCst),
        "the mutex poison hook never ran"
    );
    write_lockdown_without_a_bump(&ctx);

    let first = gate
        .check(CATEGORY)
        .expect_err("a poisoned refresh must never reuse cached permission");
    let denied = if first.code() == "ERR_NETWORK_SETTINGS_UNREADABLE" {
        gate.check(CATEGORY)
            .expect_err("the next read must recover and see Lockdown")
    } else {
        first
    };
    assert_eq!(denied.code(), "ERR_NETWORK_LOCKDOWN", "{denied}");
    assert!(
        opens.load(Ordering::SeqCst) > before,
        "recovery never opened a new connection"
    );
}

#[test]
fn c3_fix_clock_rollback_since_the_last_warm_check_expires_permission() {
    #[derive(Default)]
    struct RewindableClock {
        seconds: AtomicUsize,
        reads: AtomicUsize,
    }
    impl Clock for RewindableClock {
        fn now(&self) -> SystemTime {
            self.reads.fetch_add(1, Ordering::SeqCst);
            SystemTime::UNIX_EPOCH + Duration::from_secs(self.seconds.load(Ordering::SeqCst) as u64)
        }
    }
    let (_root, ctx) = context();
    let clock = Arc::new(RewindableClock::default());
    let ctx = ctx.with_clock(clock.clone());
    let gate = SettingsGate::for_context(&ctx);
    gate.check(CATEGORY).expect("load permission at t=0");
    clock.seconds.store(29, Ordering::SeqCst);
    gate.check(CATEGORY).expect("warm check at t=29");
    write_lockdown_without_a_bump(&ctx);
    clock.seconds.store(1, Ordering::SeqCst);
    let reads = clock.reads.load(Ordering::SeqCst);
    let result = gate.check(CATEGORY);
    assert!(
        reads >= 2 && clock.reads.load(Ordering::SeqCst) > reads,
        "the test clock must be consulted on the warm and backwards checks"
    );
    let denied = result
        .expect_err("t=1 is backwards from the last check at t=29 and must reload permission");
    assert_eq!(denied.code(), "ERR_NETWORK_LOCKDOWN", "{denied}");
}

#[test]
fn c4_a_warm_check_hands_out_the_snapshot_it_already_holds() {
    use agora_core::settings::{SettingsCache, SettingsReads};

    let (_root, ctx) = context();
    let cache = SettingsCache::new();
    let reads = SettingsReads::of(&ctx);
    let first = cache
        .load(&reads)
        .expect("the first check loads the settings");
    let second = cache
        .load(&reads)
        .expect("the warm check reads the version");
    assert!(
        Arc::ptr_eq(&first, &second),
        "a warm check copied every setting instead of handing out the snapshot it already had"
    );

    let writer = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    db::set_setting(&writer, LOCKDOWN, &json!(true)).unwrap();
    let third = cache.load(&reads).expect("the changed version reloads");
    assert!(
        !Arc::ptr_eq(&second, &third),
        "a version that changed must produce a new snapshot, not the cached one"
    );
    assert_eq!(
        third.version(),
        second.version() + 1,
        "the new snapshot carries the version the write published"
    );
    assert!(
        third.lockdown_denies_network().unwrap_or(false),
        "the new snapshot carries the values the write stored"
    );
}

#[test]
fn c4_an_admission_check_never_creates_a_settings_database() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("local_state.db");
    let error = agora_core::settings::admit_action_at(&db_path)
        .expect_err("an admission against a data root with no database must refuse");
    assert_eq!(error.code(), "ERR_NETWORK_SETTINGS_UNREADABLE", "{error}");
    assert!(
        !db_path.exists(),
        "the admission check created the settings database it could not read"
    );
    for sidecar in ["local_state.db-wal", "local_state.db-shm"] {
        assert!(
            !root.path().join(sidecar).exists(),
            "the admission check created {sidecar}"
        );
    }
}
