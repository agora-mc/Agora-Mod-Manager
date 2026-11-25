//! One identity table (MASTER_SPEC §32.7): how the shared identity is written.
//!
//! An instance is an identity row plus its game's detail row. These tests are about what happens
//! when the second half cannot be written, when an id belongs to another game, and when an update
//! carries no statistics: the identity must never be left half an instance, and a writer that
//! knows less than the row it writes must not erase what the row knows.

use std::path::PathBuf;

use agora_core::db::{
    init_local_state_db, instance_id_exists, local_state_connection, upsert_instance,
};
use agora_core::models::InstanceRow;
use rusqlite::Connection;

fn database() -> (tempfile::TempDir, PathBuf, Connection) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("local_state.db");
    init_local_state_db(&path).unwrap();
    let conn = local_state_connection(&path).unwrap();
    (tmp, path, conn)
}

fn instance_row(instance_id: &str) -> InstanceRow {
    InstanceRow {
        instance_id: instance_id.into(),
        name: "Sky Factory".into(),
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
        created_at: "2026-01-01T00:00:00Z".into(),
        java_path: None,
        java_incompatible_override: false,
        icon_path: None,
        launch_mode_override: "auto".into(),
        import_source: None,
        total_play_ms: 0,
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn text(conn: &Connection, sql: &str) -> String {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn a_detail_row_that_cannot_be_written_leaves_no_identity_row() {
    let (_tmp, _path, conn) = database();
    // The detail write is the one that fails: the identity must not be left behind alone.
    conn.execute_batch(
        "CREATE TRIGGER refuse_minecraft_detail BEFORE INSERT ON minecraft_instances
         BEGIN
             SELECT RAISE(ABORT, 'the detail row is refused');
         END;",
    )
    .unwrap();

    let error = upsert_instance(&conn, "minecraft", &instance_row("new-instance"))
        .expect_err("the detail write fails");
    assert!(
        error.to_string().contains("the detail row is refused"),
        "{error}"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM instances"),
        0,
        "half an instance is not an instance: no identity row survives"
    );
}

#[test]
fn a_detail_row_that_cannot_be_updated_leaves_the_identity_unchanged() {
    let (_tmp, _path, conn) = database();
    upsert_instance(&conn, "minecraft", &instance_row("kept")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_minecraft_detail_update BEFORE UPDATE ON minecraft_instances
         BEGIN
             SELECT RAISE(ABORT, 'the detail row is refused');
         END;",
    )
    .unwrap();

    let mut renamed = instance_row("kept");
    renamed.name = "Renamed".into();
    let error = upsert_instance(&conn, "minecraft", &renamed)
        .expect_err("the detail write fails after the identity write");
    assert!(
        error.to_string().contains("the detail row is refused"),
        "{error}"
    );
    assert_eq!(
        text(
            &conn,
            "SELECT name FROM instances WHERE instance_id = 'kept'"
        ),
        "Sky Factory",
        "the identity row was rolled back with the detail row"
    );
}

#[test]
fn an_id_owned_by_another_game_is_not_free_for_minecraft() {
    let (_tmp, _path, conn) = database();
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES ('taken', 'skyrim-se', 'Riften', '2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();

    let error = upsert_instance(&conn, "minecraft", &instance_row("taken"))
        .expect_err("a Skyrim instance's id is not free for Minecraft");
    let message = error.to_string();
    assert!(message.contains("skyrim-se"), "{message}");
    assert!(message.contains("taken"), "{message}");
    assert_eq!(
        text(
            &conn,
            "SELECT game FROM instances WHERE instance_id = 'taken'"
        ),
        "skyrim-se",
        "the other game's identity is untouched"
    );
    assert_eq!(
        text(
            &conn,
            "SELECT name FROM instances WHERE instance_id = 'taken'"
        ),
        "Riften"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM minecraft_instances"),
        0,
        "and no Minecraft detail row was written for it"
    );
}

#[test]
fn an_update_keeps_the_play_statistics_it_did_not_bring() {
    let (_tmp, _path, conn) = database();
    upsert_instance(&conn, "minecraft", &instance_row("played")).unwrap();
    conn.execute(
        "UPDATE instances
            SET total_play_ms = 5000, last_played_at = '2026-01-02T00:00:00Z'
          WHERE instance_id = 'played'",
        [],
    )
    .unwrap();

    // What a launcher import does when it updates an instance it already knows: a row built from
    // the import, which carries no play statistics at all.
    let mut imported = instance_row("played");
    imported.name = "Renamed by import".into();
    imported.total_play_ms = 0;
    upsert_instance(&conn, "minecraft", &imported).unwrap();

    assert_eq!(
        text(
            &conn,
            "SELECT name FROM instances WHERE instance_id = 'played'"
        ),
        "Renamed by import",
        "the write it did bring landed"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT total_play_ms FROM instances WHERE instance_id = 'played'"
        ),
        5000,
        "only the play-session code changes playtime"
    );
    assert_eq!(
        text(
            &conn,
            "SELECT last_played_at FROM instances WHERE instance_id = 'played'"
        ),
        "2026-01-02T00:00:00Z",
        "and the time it was last played"
    );
}

#[test]
fn instance_id_exists_answers_for_every_kind_of_instance() {
    let (_tmp, _path, conn) = database();
    assert!(!instance_id_exists(&conn, "missing").unwrap());
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES ('a-game', 'skyrim-se', 'Riften', '2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
    assert!(
        instance_id_exists(&conn, "a-game").unwrap(),
        "another game's instance owns its id, even with no Minecraft detail row"
    );
    upsert_instance(&conn, "minecraft", &instance_row("a-minecraft")).unwrap();
    assert!(instance_id_exists(&conn, "a-minecraft").unwrap());

    // A question that cannot be answered is an error, never "free".
    conn.execute_batch("DROP TABLE instances").unwrap();
    assert!(instance_id_exists(&conn, "a-game").is_err());
}

#[test]
fn a_base_row_needs_an_identity_that_is_not_minecraft() {
    let (_tmp, _path, conn) = database();
    // An identity that says Minecraft but has no Minecraft detail row yet: a base row for it
    // would be a base on a Minecraft instance.
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES ('mc-identity', 'minecraft', 'Sky Factory', '2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
    let error = conn
        .execute(
            "INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('mc-identity', '{}')",
            [],
        )
        .unwrap_err();
    assert!(error.to_string().contains("Minecraft"), "{error}");

    // And an identity that already has a base cannot become a Minecraft instance.
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES ('game-identity', 'skyrim-se', 'Riften', '2026-01-01T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('game-identity', '{}')",
        [],
    )
    .unwrap();
    let error = conn
        .execute(
            "UPDATE instances SET game = 'minecraft' WHERE instance_id = 'game-identity'",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("base"),
        "the refusal says which detail row is in the way: {error}"
    );
}

#[test]
fn a_detail_row_that_cannot_be_written_inside_a_callers_transaction_leaves_no_identity_row() {
    let (_tmp, _path, conn) = database();
    conn.execute_batch(
        "CREATE TRIGGER refuse_minecraft_detail BEFORE INSERT ON minecraft_instances
         BEGIN
             SELECT RAISE(ABORT, 'the detail row is refused');
         END;",
    )
    .unwrap();

    // A caller that already holds a transaction gets the same promise as one that does not:
    // the failed detail write takes the identity write with it, so the caller's commit
    // publishes neither half.
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = upsert_instance(&conn, "minecraft", &instance_row("new-instance"))
        .expect_err("the detail write fails");
    assert!(
        error.to_string().contains("the detail row is refused"),
        "{error}"
    );
    conn.execute_batch("COMMIT").unwrap();

    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM instances"),
        0,
        "half an instance reached the caller's commit: no identity row may survive"
    );
}
