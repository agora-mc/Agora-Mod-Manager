//! Storing, reading back and trimming the crash answer attached to a session. No game runs.

use agora_core::crash_live::{
    answer_for_session, answers_for_instance, latest_answer, store_answer, SessionAnswer,
    StoredAnswer,
};
use agora_core::session_ledger::{ChangeKind, ManifestChange};
use agora_game_api::crash::{CrashAnswer, Level, NextStepKind, SessionVerdict};
use rusqlite::Connection;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    agora_core::db::run_migrations(&conn).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    conn
}

fn answer(session: &str, instance: &str, created: &str, headline: &str) -> SessionAnswer {
    let mut text = agora_game_api::crash::LocalText::new();
    text.insert("en".to_string(), headline.to_string());
    SessionAnswer {
        session_id: session.to_string(),
        instance_id: instance.to_string(),
        created_at: created.to_string(),
        answer: StoredAnswer::Analyzed {
            answer: Box::new(CrashAnswer {
                contract: agora_game_api::crash::CRASH_CONTRACT_VERSION,
                verdict: SessionVerdict::Crashed {
                    how: agora_game_api::crash::MatchedBy::ExitStatus,
                },
                headline: text,
                level: Level::NotSure,
                candidates: Vec::new(),
                findings: Vec::new(),
                inspections: Vec::new(),
                could_not_check: Vec::new(),
                trust_notes: Vec::new(),
                next: agora_game_api::crash::NextStep {
                    kind: NextStepKind::SecondOpinion,
                    keys: Vec::new(),
                },
                repairs: Vec::new(),
                actions: Vec::new(),
                artifacts: Vec::new(),
                cause: None,
            }),
            files: Vec::new(),
            changes: Vec::new(),
            record: None,
        },
    }
}

fn insert_session(conn: &Connection, session: &str, instance: &str, started: &str) {
    conn.execute(
        "INSERT INTO launch_history (instance_id, started_at, session_id)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![instance, started, session],
    )
    .unwrap();
}

#[test]
fn an_answer_round_trips_by_session_and_as_the_latest() {
    let conn = db();
    insert_session(&conn, "s1", "inst", "2026-01-01T00:00:00Z");
    insert_session(&conn, "s2", "inst", "2026-01-01T01:00:00Z");
    store_answer(
        &conn,
        &answer("s1", "inst", "2026-01-01T00:01:00Z", "first"),
    )
    .unwrap();
    store_answer(
        &conn,
        &answer("s2", "inst", "2026-01-01T01:01:00Z", "second"),
    )
    .unwrap();

    let found = answer_for_session(&conn, "s1").unwrap().unwrap();
    assert_eq!(found.session_id, "s1");
    let latest = latest_answer(&conn, "inst").unwrap().unwrap();
    assert_eq!(latest.session_id, "s2");
}

#[test]
fn a_could_not_analyze_answer_is_stored_as_an_answer_not_an_absence() {
    let conn = db();
    insert_session(&conn, "s1", "inst", "2026-01-01T00:00:00Z");
    let mut stored = answer("s1", "inst", "2026-01-01T00:01:00Z", "unused");
    stored.answer = StoredAnswer::CouldNotAnalyze {
        why: "the parser threw".to_string(),
    };
    store_answer(&conn, &stored).unwrap();

    let found = answer_for_session(&conn, "s1").unwrap().unwrap();
    assert!(matches!(found.answer, StoredAnswer::CouldNotAnalyze { .. }));
}

#[test]
fn trimming_history_removes_the_answers_of_the_rows_it_dropped() {
    let conn = db();
    for index in 0..(agora_core::launch_history::HISTORY_LIMIT + 5) {
        let session = format!("s{index}");
        insert_session(
            &conn,
            &session,
            "inst",
            &format!("2026-01-01T00:{:02}:00Z", index % 60),
        );
        store_answer(
            &conn,
            &answer(&session, "inst", "2026-01-01T00:00:00Z", "x"),
        )
        .unwrap();
    }
    agora_core::launch_history::prune_history(&conn, "inst").unwrap();

    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM launch_history", [], |row| row.get(0))
        .unwrap();
    let answers: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_answers", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows as usize, agora_core::launch_history::HISTORY_LIMIT);
    assert_eq!(answers, rows, "an answer lives exactly as long as its row");
}

#[test]
fn deleting_an_instance_deletes_its_answers() {
    let conn = db();
    conn.execute(
        "INSERT INTO game_instances (instance_id, game, name, base_json)
         VALUES ('inst', 'demo-game', 'Demo', '{}')",
        [],
    )
    .unwrap();
    insert_session(&conn, "s1", "inst", "2026-01-01T00:00:00Z");
    store_answer(&conn, &answer("s1", "inst", "2026-01-01T00:01:00Z", "x")).unwrap();

    conn.execute("DELETE FROM game_instances WHERE instance_id = 'inst'", [])
        .unwrap();
    let answers: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_answers", [], |row| row.get(0))
        .unwrap();
    assert_eq!(answers, 0);
}

#[test]
fn answers_for_instance_are_newest_session_first() {
    let conn = db();
    insert_session(&conn, "old", "inst", "2026-01-01T00:00:00Z");
    conn.execute(
        "UPDATE launch_history SET ended_at = '2026-01-01T00:10:00Z' WHERE session_id = 'old'",
        [],
    )
    .unwrap();
    insert_session(&conn, "new", "inst", "2026-01-01T01:00:00Z");
    conn.execute(
        "UPDATE launch_history SET ended_at = '2026-01-01T01:10:00Z' WHERE session_id = 'new'",
        [],
    )
    .unwrap();
    // Stored in the other order, so the query's ordering is what the test proves.
    store_answer(&conn, &answer("new", "inst", "2026-01-01T01:01:00Z", "new")).unwrap();
    store_answer(&conn, &answer("old", "inst", "2026-01-01T00:01:00Z", "old")).unwrap();

    let answers = answers_for_instance(&conn, "inst").unwrap();
    let ids: Vec<&str> = answers
        .iter()
        .map(|answer| answer.session_id.as_str())
        .collect();
    assert_eq!(ids, vec!["new", "old"]);
    assert!(answers_for_instance(&conn, "another").unwrap().is_empty());
}

#[test]
fn the_change_summary_survives_a_round_trip() {
    let conn = db();
    insert_session(&conn, "s1", "inst", "2026-01-01T00:00:00Z");
    let mut stored = answer("s1", "inst", "2026-01-01T00:01:00Z", "x");
    stored.answer = StoredAnswer::Analyzed {
        answer: match stored.answer {
            StoredAnswer::Analyzed { answer, .. } => answer,
            _ => unreachable!(),
        },
        files: Vec::new(),
        changes: vec![ManifestChange {
            kind: ChangeKind::Updated,
            key: Some("acme.sky".to_string()),
            from_version: Some("1.0".to_string()),
            to_version: Some("2.0".to_string()),
        }],
        record: None,
    };
    store_answer(&conn, &stored).unwrap();

    let found = answer_for_session(&conn, "s1").unwrap().unwrap();
    let StoredAnswer::Analyzed { changes, .. } = found.answer else {
        panic!("the answer should still be analyzed")
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].key.as_deref(), Some("acme.sky"));
    assert_eq!(changes[0].to_version.as_deref(), Some("2.0"));
}
