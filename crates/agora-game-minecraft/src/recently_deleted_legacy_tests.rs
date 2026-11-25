//! An entry written before instances had one identity table (MASTER_SPEC §32.7) still holds its
//! `user_instances` row, which no table in this build has. These tests build such an entry from
//! the schema-30 database the pre-split build wrote, then restore it through the same validation
//! and insert path a live restore uses: every row comes back where its columns live now, and an
//! entry that cannot be placed whole is refused with nothing written.

use super::*;
use rusqlite::types::Value as SqlValue;

/// The schema-30 database a pre-split build wrote, with one Minecraft instance and a row in
/// every table that hangs off it.
fn old_database(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    agora_core::db::run_migrations_to(&conn, 30).unwrap();
    conn.execute_batch(
        "INSERT INTO user_instances
             (instance_id, name, minecraft_version, loader, loader_version, created_at,
              jvm_memory_mb)
         VALUES ('mc-old', 'Sky Factory', '1.21', 'fabric', '0.16.0',
                 '2026-01-01T00:00:00Z', 6144);
         INSERT INTO launch_history (instance_id, started_at, outcome)
         VALUES ('mc-old', '2026-01-03T00:00:00Z', 'ok');
         INSERT INTO launch_manifests (instance_id, hash, data, raw_len)
         VALUES ('mc-old', 'hash-1', X'00', 1);
         INSERT INTO plugin_load_order_rules (instance_id, list_id, item, after_item)
         VALUES ('mc-old', 'plugins', 'b.jar', 'a.jar');
         INSERT INTO instance_imports
             (instance_id, launcher_kind, installation_key, source_instance_key, source_path,
              source_fingerprint, imported_at, updated_at)
         VALUES ('mc-old', 'curseforge', 'install-1', 'source-1', 'C:/elsewhere/pack',
                 'fingerprint', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO instance_pack_files (instance_id, relative_path, sha256, size)
         VALUES ('mc-old', 'mods/a.jar', 'abc', 12);
         INSERT INTO instance_pack_gaps (instance_id, filename, display_name, status)
         VALUES ('mc-old', 'b.jar', 'Bee', 'missing');
         INSERT INTO mcp_approval_grants (tool_name, instance_id, state, granted_at)
         VALUES ('tools', 'mc-old', 'granted', '2026-01-04T00:00:00Z');",
    )
    .unwrap();
    conn
}

/// Every table that holds rows for one instance, read from the schema rather than listed by
/// hand, so the fixture has the shape the database had and not an invented one.
fn instance_keyed_tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    names
        .into_iter()
        .filter(|name| {
            conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = 'instance_id'",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        })
        .collect()
}

/// One table's rows for one instance, in the shape the pre-split capture wrote: every column,
/// every value as JSON.
fn capture_like_the_old_build(conn: &Connection, table: &str, instance_id: &str) -> TableDump {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM {} WHERE instance_id = ?1",
            quote_ident(table)
        ))
        .unwrap();
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let width = columns.len();
    let mut rows = Vec::new();
    let mut cursor = stmt.query([instance_id]).unwrap();
    while let Some(row) = cursor.next().unwrap() {
        let mut values = Vec::with_capacity(width);
        for index in 0..width {
            values.push(sql_to_json(row.get::<_, SqlValue>(index).unwrap()).unwrap());
        }
        rows.push(values);
    }
    TableDump {
        table: table.to_string(),
        columns,
        via: None,
        rows,
    }
}

/// The `entry.json` the pre-split build left in the Recently-deleted folder.
fn legacy_entry(conn: &Connection, instance_id: &str, entry_id: &str) -> StoredEntry {
    let mut db = Vec::new();
    for table in instance_keyed_tables(conn) {
        let dump = capture_like_the_old_build(conn, &table, instance_id);
        if table == LEGACY_ROOT_TABLE {
            // The root row first, as the old capture wrote it.
            db.insert(0, dump);
        } else {
            db.push(dump);
        }
    }
    StoredEntry {
        version: ENTRY_VERSION,
        info: RecentlyDeletedEntry {
            entry_id: entry_id.to_string(),
            instance_id: instance_id.to_string(),
            name: "Sky Factory".to_string(),
            minecraft_version: "1.21".to_string(),
            loader: "fabric".to_string(),
            import_source: None,
            deleted_at: "2026-02-01T00:00:00Z".to_string(),
            expires_at: "2026-03-03T00:00:00Z".to_string(),
            size_bytes: 0,
        },
        db,
    }
}

fn write_entry(root: &Path, entry: &StoredEntry) {
    let dir = root.join(&entry.info.entry_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(ENTRY_FILE),
        serde_json::to_string_pretty(entry).unwrap(),
    )
    .unwrap();
}

fn upgraded(path: &Path) -> Connection {
    let conn = agora_core::db::local_state_connection(path).unwrap();
    agora_core::db::run_migrations(&conn).unwrap();
    conn
}

/// What stashing did in the pre-split build: the row and every row hanging off it are gone,
/// so the entry is the only copy left. The old database the tests migrate therefore holds
/// nothing for the instance.
fn clear_instance(conn: &Connection, instance_id: &str) {
    for table in instance_keyed_tables(conn) {
        conn.execute(
            &format!("DELETE FROM {} WHERE instance_id = ?1", quote_ident(&table)),
            [instance_id],
        )
        .unwrap();
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn root_dump(dumps: &[TableDump]) -> TableDump {
    dumps
        .iter()
        .find(|dump| dump.table == LEGACY_ROOT_TABLE)
        .cloned()
        .unwrap_or_else(|| panic!("the entry holds an old root row: {:?}", dumps.len()))
}

#[test]
fn a_pre_split_entry_restores_into_the_identity_and_detail_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("local_state.db");
    let old = old_database(&path);
    let entry = legacy_entry(&old, "mc-old", "entry-1");
    write_entry(tmp.path(), &entry);
    clear_instance(&old, "mc-old");
    let captured: Vec<(String, usize)> = entry
        .db
        .iter()
        .map(|dump| (dump.table.clone(), dump.rows.len()))
        .filter(|(_, rows)| *rows > 0)
        .collect();
    assert!(
        captured.iter().any(|(table, _)| table == LEGACY_ROOT_TABLE),
        "the fixture holds the old table's row: {captured:?}"
    );
    drop(old);

    // Read the entry back the way a restore does, then put it back under a new id.
    let stored = read_entry_file(tmp.path(), "entry-1").unwrap();
    let mut conn = upgraded(&path);
    replay_db_state(&mut conn, &stored.db, "mc-old", "mc-restored", None).unwrap();

    let game: String = conn
        .query_row(
            "SELECT game FROM instances WHERE instance_id = 'mc-restored'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(game, GAME_ID, "the identity says which game it belongs to");
    let name: String = conn
        .query_row(
            "SELECT name FROM instances WHERE instance_id = 'mc-restored'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "Sky Factory");
    let created: String = conn
        .query_row(
            "SELECT created_at FROM instances WHERE instance_id = 'mc-restored'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(created, "2026-01-01T00:00:00Z", "identity columns kept");
    let detail: (String, String, String, i64) = conn
        .query_row(
            "SELECT minecraft_version, loader, loader_version, jvm_memory_mb
               FROM minecraft_instances WHERE instance_id = 'mc-restored'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        detail,
        (
            "1.21".to_string(),
            "fabric".to_string(),
            "0.16.0".to_string(),
            6144
        ),
        "the Minecraft columns land in the detail table"
    );

    // Every table the entry carried has as many rows as it came with, except the permission
    // grants, which are deliberately not carried onto a different id.
    for (table, rows) in captured {
        if table == LEGACY_ROOT_TABLE || PERMISSION_TABLES.contains(&table.as_str()) {
            continue;
        }
        let restored = count(
            &conn,
            &format!(
                "SELECT COUNT(*) FROM {} WHERE instance_id = 'mc-restored'",
                quote_ident(&table)
            ),
        );
        assert_eq!(restored, rows as i64, "{table} rows came back");
    }
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM mcp_approval_grants WHERE instance_id = 'mc-restored'"
        ),
        0,
        "a grant is not carried to a different instance"
    );
}

#[test]
fn a_pre_split_entry_restored_under_its_own_id_keeps_every_row() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("local_state.db");
    let old = old_database(&path);
    let entry = legacy_entry(&old, "mc-old", "entry-2");
    write_entry(tmp.path(), &entry);
    clear_instance(&old, "mc-old");
    drop(old);

    let stored = read_entry_file(tmp.path(), "entry-2").unwrap();
    let mut conn = upgraded(&path);
    replay_db_state(&mut conn, &stored.db, "mc-old", "mc-old", None).unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM mcp_approval_grants WHERE instance_id = 'mc-old'"
        ),
        1,
        "same id: the grant comes back with everything else"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM instances WHERE instance_id = 'mc-old'"
        ),
        1
    );
}

#[test]
fn an_entry_that_cannot_be_placed_whole_is_refused_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("local_state.db");
    let old = old_database(&path);
    let good = legacy_entry(&old, "mc-old", "entry-3");
    clear_instance(&old, "mc-old");
    drop(old);
    let mut conn = upgraded(&path);

    // Two instance rows in one entry: an entry holds one instance.
    let mut two_rows = good.db.clone();
    let mut second_row = root_dump(&two_rows).rows[0].clone();
    second_row[0] = Json::String("mc-other".to_string());
    two_rows
        .iter_mut()
        .find(|dump| dump.table == LEGACY_ROOT_TABLE)
        .unwrap()
        .rows
        .push(second_row);
    // A row for another instance is refused as well.
    let mut wrong_id = good.db.clone();
    wrong_id
        .iter_mut()
        .find(|dump| dump.table == LEGACY_ROOT_TABLE)
        .unwrap()
        .rows[0][0] = Json::String("mc-other".to_string());
    // Rows in the other namespace cannot be placed at all.
    let mut other_namespace = good.db.clone();
    let empty_game_instances = other_namespace
        .iter()
        .position(|dump| dump.table == "game_instances")
        .expect("the old capture saw the other table");
    other_namespace[empty_game_instances]
        .rows
        .push(vec![Json::String("skyrim-pack".to_string())]);
    // A column this build does not have.
    let mut unknown_column = good.db.clone();
    let legacy = unknown_column
        .iter_mut()
        .find(|dump| dump.table == LEGACY_ROOT_TABLE)
        .unwrap();
    legacy.columns.push("column_from_the_future".to_string());
    for row in &mut legacy.rows {
        row.push(Json::Null);
    }

    for (what, dumps) in [
        ("two instance rows", two_rows),
        ("a row for another instance", wrong_id),
        ("rows in the other namespace", other_namespace),
        ("an unknown column", unknown_column),
    ] {
        let error = replay_db_state(&mut conn, &dumps, "mc-old", "mc-restored", None)
            .expect_err(&format!("{what} is refused"));
        assert!(
            matches!(error, LauncherError::Generic { .. }),
            "{what}: {error:?}"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM instances"),
            0,
            "{what}: nothing is written"
        );
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM launch_history"),
            0,
            "{what}: no dependent row is written either"
        );
    }
}
