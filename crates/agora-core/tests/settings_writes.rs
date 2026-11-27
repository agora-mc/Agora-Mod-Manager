use agora_core::{db, settings};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

type WriteSetting = fn(&Connection, &str, bool) -> anyhow::Result<()>;

#[test]
fn every_settings_writer_commits_value_and_version_together() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("local_state.db");
    db::init_local_state_db(&path).unwrap();
    let writer = db::local_state_connection(&path).unwrap();
    let reader = db::local_state_connection(&path).unwrap();
    let writers: [(&str, WriteSetting); 2] = [
        ("db::set_setting", |conn, key, value| {
            db::set_setting(conn, key, &json!(value))
        }),
        ("settings::set", |conn, key, value| {
            settings::set(conn, key, if value { "true" } else { "false" }).map(drop)
        }),
    ];
    let mut failures = Vec::new();
    for (name, write) in writers {
        let key = format!("test.versioned.{name}");
        for value in [false, true] {
            let before = db::settings_version(&reader);
            let commits = Arc::new(AtomicUsize::new(0));
            writer.commit_hook(Some({
                let commits = Arc::clone(&commits);
                move || {
                    commits.fetch_add(1, Ordering::SeqCst);
                    false
                }
            }));
            write(&writer, &key, value).unwrap();
            writer.commit_hook(None::<fn() -> bool>);
            assert_eq!(
                commits.load(Ordering::SeqCst),
                1,
                "{name} must publish each setting with one SQLite commit"
            );
            assert_eq!(
                db::get_setting(&reader, &key).unwrap(),
                Some(Value::Bool(value))
            );

            let after = db::settings_version(&reader);
            match (before, after) {
                (Ok(before), Ok(after)) if after > before => {}
                (before, after) => failures.push(format!("{name} writing {value} failed to advance a readable version: {before:?} -> {after:?}")),
            }
        }

        let before = db::settings_version(&reader);
        let vetoes = Arc::new(AtomicUsize::new(0));
        writer.commit_hook(Some({
            let vetoes = Arc::clone(&vetoes);
            move || {
                vetoes.fetch_add(1, Ordering::SeqCst);
                true
            }
        }));
        let result = write(&writer, &key, false);
        writer.commit_hook(None::<fn() -> bool>);
        assert_eq!(
            vetoes.load(Ordering::SeqCst),
            1,
            "the injected commit veto did not reach {name}"
        );
        assert!(result.is_err(), "{name} must report a failed commit");
        assert_eq!(
            db::get_setting(&reader, &key).unwrap(),
            Some(json!(true)),
            "{name} left a value behind after its commit was vetoed"
        );
        if let Ok(before) = before {
            assert_eq!(
                db::settings_version(&reader).unwrap(),
                before,
                "{name} left a version bump behind after its commit was vetoed"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn state_db() -> (tempfile::TempDir, Connection) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("local_state.db");
    db::init_local_state_db(&path).unwrap();
    let raw = Connection::open(path).unwrap();
    (root, raw)
}

fn assert_raw_write_is_versioned(setup: &str, sql: &str) {
    let (_root, raw) = state_db();
    raw.execute_batch(setup).unwrap();
    let before = db::settings_version(&raw).unwrap();
    let tx = raw.unchecked_transaction().unwrap();
    assert_eq!(tx.execute(sql, []).unwrap(), 1);
    let after = db::settings_version(&tx).unwrap();
    tx.rollback().unwrap();
    assert_eq!(
        db::settings_version(&raw).unwrap(),
        before,
        "the version bump must roll back with the settings write"
    );
    assert!(
        after > before,
        "raw settings SQL did not advance the version: {before} -> {after} ({sql})"
    );
}

const SEED: &str = "INSERT INTO user_settings (key, value_json) VALUES ('test.raw', 'false')";

#[test]
fn c3_fix_raw_insert_bumps_the_version_inside_its_transaction() {
    assert_raw_write_is_versioned("", SEED);
}

#[test]
fn c3_fix_raw_update_with_a_sql_comment_bumps_the_version_inside_its_transaction() {
    assert_raw_write_is_versioned(
        SEED,
        "UPDATE /* comment */ user_settings SET value_json = 'true' WHERE key = 'test.raw'",
    );
}

#[test]
fn c3_fix_raw_delete_bumps_the_version_inside_its_transaction() {
    assert_raw_write_is_versioned(SEED, "DELETE FROM user_settings WHERE key = 'test.raw'");
}

fn assert_schema_32_refuses_missing_version(
    erase: &str,
    write: impl FnOnce(&Connection) -> anyhow::Result<()>,
) {
    let (_root, raw) = state_db();
    assert_eq!(db::recorded_schema_version(&raw).unwrap(), 32);
    raw.execute(SEED, []).unwrap();
    raw.execute_batch(erase).unwrap();
    let result = write(&raw);
    let stored = db::get_setting(&raw, "test.raw").unwrap();
    assert!(result.is_err() && stored == Some(json!(false)), "schema 32 must refuse and roll back a write when {erase} has run: result={result:?}, stored={stored:?}");
}

#[test]
fn c3_fix_settings_writer_refuses_when_schema_32_has_no_version_table() {
    assert_schema_32_refuses_missing_version("DROP TABLE settings_version", |conn| {
        db::set_setting(conn, "test.raw", &json!(true))
    });
}

#[test]
fn c3_fix_raw_settings_writer_refuses_when_the_version_row_is_missing() {
    assert_schema_32_refuses_missing_version("DELETE FROM settings_version", |conn| {
        conn.execute(
            "UPDATE user_settings SET value_json = 'true' WHERE key = 'test.raw'",
            [],
        )?;
        Ok(())
    });
}

#[test]
fn c4_a_settings_version_trigger_dropped_by_hand_is_reinstalled_at_startup() {
    let (root, raw) = state_db();
    let path = root.path().join("local_state.db");
    raw.execute_batch("DROP TRIGGER user_settings_version_update")
        .unwrap();

    let report =
        db::init_local_state_db_with(&path, &db::LocalStateOpenOptions::default()).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("user_settings_version_update")),
        "startup must say it reinstalled the missing trigger: {:?}",
        report.warnings
    );

    let writer = db::local_state_connection(&path).unwrap();
    let before = db::settings_version(&writer).unwrap();
    db::set_setting(&writer, "network_lockdown_enabled", &json!(true)).unwrap();
    let after = db::settings_version(&writer).unwrap();
    assert_eq!(
        after,
        before + 1,
        "a settings write updates a row, so the reinstalled UPDATE trigger must publish a version"
    );
}
