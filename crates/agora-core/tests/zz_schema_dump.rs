use agora_core::db::run_migrations_to;
use rusqlite::Connection;

#[test]
fn dump_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dump.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    run_migrations_to(&conn, 30).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name",
        )
        .unwrap();
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    for (kind, name, sql) in rows {
        println!("-- {kind} {name}\n{sql};\n");
    }
}
