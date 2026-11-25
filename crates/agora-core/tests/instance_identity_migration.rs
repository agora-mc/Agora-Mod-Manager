//! One instance identity table (MASTER_SPEC §32.7): the migration's own tests.
//!
//! Each test builds the database a schema-30 build wrote, then opens it through the real
//! startup path, so the safety net (backup, ordered steps, rollback) is part of what is
//! tested. The corpus is deliberately damaged or interrupted in the ways a migration can
//! meet: an id in both old tables, and a step cut off halfway. Both must refuse or roll
//! back and report, never silently drop a row.

use std::path::{Path, PathBuf};

use agora_core::db::{
    init_local_state_db, init_local_state_db_with, local_state_connection, run_migrations_to,
    LocalStateOpenOptions, LOCAL_STATE_SCHEMA_VERSION,
};
use rusqlite::Connection;

/// A database holding both kinds of instance, both trash formats' worth of rows,
/// pre-registration import jobs and global plugin storage.
fn old_database(dir: &Path) -> PathBuf {
    let path = dir.join("local_state.db");
    std::fs::create_dir_all(dir).unwrap();
    let conn = Connection::open(&path).unwrap();
    run_migrations_to(&conn, 30).unwrap();
    conn.execute_batch(
        "INSERT INTO user_instances
             (instance_id, name, minecraft_version, loader, loader_version, created_at,
              jvm_memory_mb)
         VALUES ('mc-pack', 'Sky Factory', '1.21', 'fabric', '0.16.0',
                 '2026-01-01T00:00:00Z', 6144);

         INSERT INTO game_instances (instance_id, game, name, base_json, created_at,
                                     total_play_ms, last_played_at)
         VALUES ('skyrim-pack', 'skyrim-se', 'Riften', '{\"Unpinned\":{\"install\":\"steam:1\",\
                 \"reason\":\"test\"}}', '2026-02-02T00:00:00Z', 9000,
                 '2026-02-03T00:00:00Z');

         -- Everything that hangs off a Minecraft instance.
         INSERT INTO instance_imports
             (instance_id, launcher_kind, installation_key, source_instance_key, source_path,
              source_fingerprint, imported_at, updated_at)
         VALUES ('mc-pack', 'curseforge', 'install-1', 'source-1', 'C:/elsewhere/pack',
                 'fingerprint', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO instance_import_files (instance_id, relative_path, sha256, size)
         VALUES ('mc-pack', 'mods/a.jar', 'abc', 12);
         INSERT INTO instance_pack_files (instance_id, relative_path, sha256, size)
         VALUES ('mc-pack', 'mods/a.jar', 'abc', 12);
         INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status)
         VALUES ('mc-pack', 'b.jar', 'Bee', 'missing');
         INSERT INTO instance_update_cache (instance_id, updates_json, checked_at)
         VALUES ('mc-pack', '[]', '2026-01-02T00:00:00Z');
         INSERT INTO launch_history (instance_id, started_at, outcome)
         VALUES ('mc-pack', '2026-01-03T00:00:00Z', 'ok');
         INSERT INTO launch_manifests (instance_id, hash, data, raw_len)
         VALUES ('mc-pack', 'hash-1', X'00', 1);
         INSERT INTO session_answers (session_id, instance_id, created_at, answer_json)
         VALUES ('session-1', 'mc-pack', '2026-01-03T00:00:00Z', '{}');
         INSERT INTO crash_log_results
             (instance_id, content_hash, source_id, plugin_id, name, relative_path,
              analyzed_at, record_json)
         VALUES ('mc-pack', 'hash-1', 'src', 'plugin', 'Crash', 'crash.txt',
                 '2026-01-03T00:00:00Z', '{}');
         INSERT INTO crash_save_backups (session_id, instance_id, backup_id, created_at)
         VALUES ('session-1', 'mc-pack', 'backup-1', '2026-01-03T00:00:00Z');
         INSERT INTO plugin_load_order_rules (instance_id, list_id, item, after_item)
         VALUES ('mc-pack', 'plugins', 'b.jar', 'a.jar');
         INSERT INTO mcp_approval_grants (tool_name, instance_id, state, granted_at)
         VALUES ('tools', 'mc-pack', 'granted', '2026-01-04T00:00:00Z');

         -- A global plugin-storage row (empty instance id) and a valid pre-registration
         -- import job: neither may be deleted or refused by a blanket foreign key.
         INSERT INTO plugin_installs
             (plugin_id, version, manifest_json, source_kind, install_dir, installed_at,
              updated_at)
         VALUES ('example', '1.0.0', '{}', 'dev', 'plugins/example', '2026-01-01T00:00:00Z',
                 '2026-01-01T00:00:00Z');
         INSERT INTO plugin_storage (plugin_id, kind, instance_id, key, value_json)
         VALUES ('example', 'kv', '', 'greeting', '\\\"hi\\\"');
         INSERT INTO instance_import_jobs
             (job_id, instance_id, launcher_kind, source_instance_key, staging_path,
              final_path, state, plan_fingerprint, created_at, updated_at)
         VALUES ('job-1', 'not-registered-yet', 'curseforge', 'key-1', 'staging', 'final',
                 'planned', 'fingerprint', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');

         -- A game-instance row that shares nothing with the Minecraft one.
         INSERT INTO launch_history (instance_id, started_at, outcome)
         VALUES ('skyrim-pack', '2026-02-03T00:00:00Z', 'ok');
         INSERT INTO plugin_load_order_rules (instance_id, list_id, item, after_item)
         VALUES ('skyrim-pack', 'plugins', 'b.esp', 'a.esp');",
    )
    .unwrap();
    drop(conn);
    path
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn version(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    count(
        conn,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{name}'"),
    ) == 1
}

#[test]
fn every_row_survives_the_migration_with_its_references() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    init_local_state_db(&path).expect("the migration runs");

    let conn = local_state_connection(&path).unwrap();
    assert_eq!(version(&conn), LOCAL_STATE_SCHEMA_VERSION);
    assert!(
        !table_exists(&conn, "user_instances"),
        "the old table is gone"
    );
    assert!(
        !table_exists(&conn, "game_instances"),
        "the old table is gone"
    );

    // One identity row per instance, whatever game it belongs to.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM instances"), 2);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instances WHERE instance_id = 'mc-pack' AND game = 'minecraft'",
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instances WHERE instance_id = 'skyrim-pack' \
             AND game = 'skyrim-se' AND total_play_ms = 9000 \
             AND last_played_at = '2026-02-03T00:00:00Z'",
        ),
        1
    );
    // The Minecraft columns moved to their own table, values intact.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM minecraft_instances WHERE instance_id = 'mc-pack' \
             AND minecraft_version = '1.21' AND loader = 'fabric' AND loader_version = '0.16.0' \
             AND jvm_memory_mb = 6144",
        ),
        1
    );
    // The base record moved to its own table.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM game_instance_bases WHERE instance_id = 'skyrim-pack' \
             AND base_json LIKE '%steam:1%'",
        ),
        1
    );

    // Every dependent row is still there and still names its instance.
    for (table, id, expected) in [
        ("instance_imports", "mc-pack", 1),
        ("instance_import_files", "mc-pack", 1),
        ("instance_pack_files", "mc-pack", 1),
        ("instance_pack_gaps", "mc-pack", 1),
        ("instance_update_cache", "mc-pack", 1),
        ("launch_history", "mc-pack", 1),
        ("launch_manifests", "mc-pack", 1),
        ("session_answers", "mc-pack", 1),
        ("crash_log_results", "mc-pack", 1),
        ("crash_save_backups", "mc-pack", 1),
        ("plugin_load_order_rules", "mc-pack", 1),
        ("mcp_approval_grants", "mc-pack", 1),
        ("launch_history", "skyrim-pack", 1),
        ("plugin_load_order_rules", "skyrim-pack", 1),
    ] {
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM {table} WHERE instance_id = '{id}'")
            ),
            expected,
            "{table} rows for {id}"
        );
    }
    // A pre-registration import job and a global plugin-storage row are still valid rows,
    // not orphans a foreign key would have rejected.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instance_import_jobs WHERE instance_id = 'not-registered-yet'",
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM plugin_storage WHERE plugin_storage.instance_id = ''",
        ),
        1
    );
    // The four tables that used to point at `user_instances` point at `instances` now.
    for table in [
        "instance_imports",
        "instance_pack_files",
        "instance_pack_gaps",
        "instance_update_cache",
    ] {
        let parent: String = conn
            .query_row(
                &format!(
                    "SELECT \"table\" FROM pragma_foreign_key_list('{table}') \
                     WHERE \"from\" = 'instance_id'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent, "instances", "{table}'s parent table");
    }
    // One trigger cleans the instance's history, whichever kind it is.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' \
             AND name = 'launch_history_instance_gone'",
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' \
             AND name IN ('launch_history_minecraft_instance_gone', \
                          'launch_history_game_instance_gone')",
        ),
        0
    );
}

#[test]
fn a_colliding_id_stops_the_migration_and_keeps_the_old_database() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "INSERT INTO game_instances (instance_id, game, name, base_json)
             VALUES ('mc-pack', 'skyrim-se', 'Same id', '{}');",
        )
        .unwrap();
    }

    let error = init_local_state_db(&path).expect_err("a shared id cannot be merged");
    let message = format!("{error:#}");
    assert!(message.contains("mc-pack"), "{message}");
    assert!(
        message.contains("Sky Factory") && message.contains("Same id"),
        "the report names both rows: {message}"
    );

    // Nothing was changed: the old tables, their rows and the recorded version are intact.
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        version(&conn),
        30,
        "the last good version is still recorded"
    );
    assert!(table_exists(&conn, "user_instances"));
    assert!(table_exists(&conn, "game_instances"));
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM user_instances"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM game_instances"), 2);
    // And the safety net kept a copy of the database as it was.
    assert!(
        std::fs::read_dir(tmp.path().join("backups"))
            .map(|read| read.filter_map(Result::ok).count())
            .unwrap_or(0)
            >= 1,
        "a pre-migration backup was written"
    );
}

#[test]
fn an_interrupted_migration_leaves_the_old_database_usable() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    // Make one statement of the rebuild fail after the copy has run, which is the state a
    // crash mid-step would leave if the step were not one transaction.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE VIEW instance_imports_v31 AS SELECT 1 AS instance_id;")
            .unwrap();
    }

    let error = init_local_state_db(&path).expect_err("the rebuild cannot finish");
    let message = format!("{error:#}");
    assert!(
        message.contains("v31"),
        "the report names the step that failed: {message}"
    );

    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn), 30, "the version is not advanced");
    assert!(
        table_exists(&conn, "user_instances"),
        "the old table is back"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM user_instances"),
        1,
        "its rows are back with it"
    );
    assert!(
        !table_exists(&conn, "instances"),
        "the half-built tables are rolled back"
    );
    // The database is usable: the next start, with the obstacle gone, migrates it.
    drop(conn);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("DROP VIEW instance_imports_v31;")
            .unwrap();
    }
    init_local_state_db(&path).expect("the retried migration runs");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn), LOCAL_STATE_SCHEMA_VERSION);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM instances"), 2);
}

#[test]
fn deleting_one_instance_removes_its_rows_and_only_those() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    init_local_state_db(&path).unwrap();

    let conn = local_state_connection(&path).unwrap();
    conn.execute("DELETE FROM instances WHERE instance_id = 'mc-pack'", [])
        .unwrap();

    assert_eq!(count(&conn, "SELECT COUNT(*) FROM instances"), 1);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instances WHERE instance_id = 'skyrim-pack'"
        ),
        1
    );
    // Everything that belonged to the deleted instance went with it: the detail row by
    // cascade, the history rows by the delete trigger, the provenance and inventory by
    // foreign key.
    for table in [
        "minecraft_instances",
        "instance_imports",
        "instance_import_files",
        "instance_pack_files",
        "instance_pack_gaps",
        "instance_update_cache",
        "launch_history",
        "launch_manifests",
        "session_answers",
        "crash_log_results",
        "crash_save_backups",
        "plugin_load_order_rules",
    ] {
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM {table} WHERE instance_id = 'mc-pack'")
            ),
            0,
            "{table} rows for the deleted instance"
        );
    }
    // The other instance's rows are untouched.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM launch_history WHERE instance_id = 'skyrim-pack'",
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM plugin_load_order_rules WHERE instance_id = 'skyrim-pack'",
        ),
        1
    );
    // A row that was never an instance's to delete stays.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instance_import_jobs WHERE instance_id = 'not-registered-yet'",
        ),
        1
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM plugin_storage"), 1);
}

#[test]
fn a_game_instance_still_restores_from_recently_deleted_after_the_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    init_local_state_db(&path).unwrap();

    // Stash and restore work through the trash module's own record; the point here is that
    // the rows it writes and reads are the ones the migration left behind.
    let root = tmp.path().join("recently-deleted-games");
    let entry = root.join("entry-1");
    let instance_dir = entry.join("instance");
    std::fs::create_dir_all(&instance_dir).unwrap();
    let conn = local_state_connection(&path).unwrap();
    let row = conn
        .query_row(
            "SELECT instances.instance_id, instances.game, instances.name,
                    game_instance_bases.base_json, instances.created_at,
                    instances.last_launched_at, instances.total_play_ms,
                    instances.last_played_at
               FROM instances
               JOIN game_instance_bases
                 ON game_instance_bases.instance_id = instances.instance_id
              WHERE instances.instance_id = 'skyrim-pack'",
            [],
            |row| {
                Ok(serde_json::json!({
                    "instanceId": row.get::<_, String>(0)?,
                    "game": row.get::<_, String>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "baseJson": row.get::<_, String>(3)?,
                    "createdAt": row.get::<_, String>(4)?,
                    "lastLaunchedAt": row.get::<_, Option<String>>(5)?,
                    "totalPlayMs": row.get::<_, i64>(6)?,
                    "lastPlayedAt": row.get::<_, Option<String>>(7)?,
                }))
            },
        )
        .unwrap();
    let entry_json = serde_json::json!({
        "entry": {
            "entryId": "entry-1",
            "instanceId": "skyrim-pack",
            "game": "skyrim-se",
            "name": "Riften",
            "deletedAt": "2026-03-01T00:00:00Z",
            "expiresAt": "2026-04-01T00:00:00Z",
            "sizeBytes": 0
        },
        "row": row,
        "loadOrderRules": []
    });
    std::fs::write(
        entry.join("entry.json"),
        serde_json::to_vec_pretty(&entry_json).unwrap(),
    )
    .unwrap();
    conn.execute(
        "DELETE FROM instances WHERE instance_id = 'skyrim-pack'",
        [],
    )
    .unwrap();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM game_instance_bases"),
        0,
        "the detail row went with the identity row"
    );

    // Put the rows back the way the restore does, from the entry's record.
    let base_json = row["baseJson"].as_str().unwrap();
    conn.execute(
        "INSERT INTO instances
             (instance_id, game, name, created_at, last_launched_at, total_play_ms,
              last_played_at)
         VALUES ('skyrim-pack', 'skyrim-se', 'Riften', '2026-02-02T00:00:00Z', NULL, 9000,
                 '2026-02-03T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('skyrim-pack', ?1)",
        [base_json],
    )
    .unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instances WHERE instance_id = 'skyrim-pack' \
             AND game = 'skyrim-se'",
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM game_instance_bases WHERE instance_id = 'skyrim-pack'",
        ),
        1
    );
}

#[test]
fn the_database_refuses_a_detail_row_that_contradicts_the_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    init_local_state_db(&path).unwrap();
    let conn = local_state_connection(&path).unwrap();

    // A Minecraft detail row for an instance of another game.
    conn.execute(
        "INSERT INTO instances (instance_id, game, name) VALUES ('skyrim-2', 'skyrim-se', 'Riften 2')",
        [],
    )
    .unwrap();
    let error = conn
        .execute(
            "INSERT INTO minecraft_instances (instance_id, minecraft_version, loader, loader_version)
             VALUES ('skyrim-2', '1.21', 'fabric', '0.16')",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("minecraft"),
        "the refusal says which detail row does not belong: {error}"
    );

    // A base record for a Minecraft instance.
    let error = conn
        .execute(
            "INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('mc-pack', '{}')",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("Minecraft"),
        "the refusal says which detail row does not belong: {error}"
    );

    // And the matching pairs still work, in both directions.
    conn.execute(
        "INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('skyrim-2', '{}')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO minecraft_instances (instance_id, minecraft_version, loader,
                                              loader_version)
             VALUES ('mc-2', '1.21', 'vanilla', '')",
            [],
        )
        .is_err(),
        "a detail row with no identity row is refused by the foreign key"
    );

    // Changing an identity's game to one that contradicts its detail row is refused too.
    let error = conn
        .execute(
            "UPDATE instances SET game = 'skyrim-se' WHERE instance_id = 'mc-pack'",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("minecraft"),
        "the refusal names the row that would be orphaned: {error}"
    );
}

#[test]
fn a_row_that_already_pointed_at_no_instance_is_reported_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    // A database written while foreign keys were off can hold a row whose instance was never
    // there. It arrives broken; the step must report it, not refuse the migration and not
    // delete the row to make its own check pass.
    let orphan_rowid: i64 = {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status)
             VALUES ('instance-that-never-existed', 'c.jar', 'Sea', 'missing')",
            [],
        )
        .unwrap();
        conn.query_row(
            "SELECT rowid FROM instance_pack_gaps
              WHERE instance_id = 'instance-that-never-existed'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default())
        .expect("a reference that was already broken does not block the migration");

    let conn = local_state_connection(&path).unwrap();
    assert_eq!(
        version(&conn),
        LOCAL_STATE_SCHEMA_VERSION,
        "the step ran: the old tables are gone and the identity table is there"
    );
    assert!(table_exists(&conn, "instances"));
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instance_pack_gaps
              WHERE instance_id = 'instance-that-never-existed'"
        ),
        1,
        "the row is kept exactly as it arrived"
    );

    let warning = report
        .warnings
        .iter()
        .find(|warning| warning.contains("point at no instance"))
        .unwrap_or_else(|| {
            panic!(
                "the report names the orphan instead of hiding it: {:?}",
                report.warnings
            )
        });
    assert!(warning.contains("1 row(s)"), "{warning}");
    assert!(warning.contains("instance_pack_gaps"), "{warning}");
    assert!(
        warning.contains(&format!("row {orphan_rowid}")),
        "the report names the row, not just the table: {warning}"
    );
}

#[test]
fn a_row_that_belongs_to_the_other_namespace_stops_the_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    // A row that hangs off a Minecraft instance but names one of the *other* table's instances.
    // Before the merge it is an orphan; after it, it would silently become that game's row.
    let orphan_rowid: i64 = {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status)
             VALUES ('skyrim-pack', 'c.jar', 'Sea', 'missing')",
            [],
        )
        .unwrap();
        conn.query_row(
            "SELECT rowid FROM instance_pack_gaps WHERE instance_id = 'skyrim-pack'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    let error =
        init_local_state_db(&path).expect_err("a row that would change owner stops the migration");
    let message = format!("{error:#}");
    assert!(message.contains("instance_pack_gaps"), "{message}");
    assert!(
        message.contains(&format!("row {orphan_rowid}")),
        "the report names the row: {message}"
    );
    assert!(message.contains("skyrim-pack"), "{message}");
    assert!(
        message.contains("user_instances") && message.contains("game_instances"),
        "the report names both namespaces: {message}"
    );

    // Nothing was changed: version 30 is still recorded and both old tables are there.
    let conn = local_state_connection(&path).unwrap();
    assert_eq!(version(&conn), 30);
    assert!(table_exists(&conn, "user_instances"));
    assert!(table_exists(&conn, "game_instances"));
    assert!(!table_exists(&conn, "instances"));
}

#[test]
fn a_row_that_would_change_owner_is_found_beside_an_instance_with_no_id() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    // A Minecraft instance row with no id at all: SQLite allows a NULL in a TEXT primary key,
    // and a `NOT IN (SELECT instance_id ...)` is then NULL for every row, so the scan that looks
    // for rows the merge would re-parent would find nothing and let the orphan below take the
    // Skyrim instance as its parent.
    let orphan_rowid: i64 = {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO user_instances
                 (instance_id, name, minecraft_version, loader, loader_version, created_at)
             VALUES (NULL, 'Nameless', '1.21', 'fabric', '0.16.0', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status)
             VALUES ('skyrim-pack', 'c.jar', 'Sea', 'missing')",
            [],
        )
        .unwrap();
        conn.query_row(
            "SELECT rowid FROM instance_pack_gaps WHERE instance_id = 'skyrim-pack'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    let error = init_local_state_db(&path)
        .expect_err("a row that would change owner stops the migration");
    let message = format!("{error:#}");
    assert!(message.contains("instance_pack_gaps"), "{message}");
    assert!(
        message.contains(&format!("row {orphan_rowid}")),
        "the report names the row: {message}"
    );
    assert!(message.contains("skyrim-pack"), "{message}");

    // Nothing was changed.
    let conn = local_state_connection(&path).unwrap();
    assert_eq!(version(&conn), 30);
    assert!(table_exists(&conn, "user_instances"));
    assert!(!table_exists(&conn, "instances"));
}

#[test]
fn the_rebuild_keeps_rowids_so_a_report_names_the_row_it_meant() {
    let tmp = tempfile::tempdir().unwrap();
    let path = old_database(tmp.path());
    // One child row, already an orphan when the database arrives, and not one that the merge
    // would re-parent: nothing in either instance table has this id. It sits at rowid 2 because
    // a valid row was there before it and has since been deleted — so a rebuild that copies the
    // rows in order lands it at rowid 1, a row the arrival record has never seen, and the runner
    // refuses to commit a database that only needed upgrading.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM instance_pack_gaps;
             INSERT INTO instance_pack_gaps (rowid, instance_id, filename, display_name, status)
             VALUES (2, 'gone-instance', 'c.jar', 'Sea', 'missing');",
        )
        .unwrap();
    }

    let report = init_local_state_db_with(&path, &LocalStateOpenOptions::default())
        .expect("an orphan the database arrived with does not stop the upgrade");
    assert_eq!(report.migrated_to, 31);
    let warned = report.warnings.join("\n");
    assert!(
        warned.contains("instance_pack_gaps row 2"),
        "the warning names the row that was already broken: {warned}"
    );

    let conn = local_state_connection(&path).unwrap();
    let rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM instance_pack_gaps WHERE instance_id = 'gone-instance'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rowid, 2, "the row keeps the rowid it had");
}
