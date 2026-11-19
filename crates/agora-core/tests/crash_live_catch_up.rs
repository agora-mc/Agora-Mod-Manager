//! Catch-up windows and the evidence it may still use: a session never claims a later session's
//! crash log, a log another answer already read, or loses a crash the launch path closed first.

use agora_core::crash_engine::attribution::InstanceView;
use agora_core::crash_engine::instance_view::{ExtraInstanceProvider, ResolvedCrashInstance};
use agora_core::crash_live::{
    answer_for_session, catch_up, store_answer, LivePacks, SessionAnswer, StoredAnswer,
};
use agora_core::session_ledger::{self, ExitObservation, LaunchManifest, Owner, SessionFinish};
use agora_game_api::crash::{
    BuiltInPack, CrashAnswer, Level, LocalText, MatchedBy, NextStep, NextStepKind, SessionVerdict,
};
use agora_plugin_host::QuickJsHost;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct DummyProvider;

impl ExtraInstanceProvider for DummyProvider {
    fn resolve(
        &self,
        _ctx: &agora_core::ctx::Ctx,
        _instance_id: &str,
    ) -> agora_core::error::LauncherResult<Option<ResolvedCrashInstance>> {
        Ok(Some(ResolvedCrashInstance {
            game_id: "test-game".to_string(),
            runtime_version: Some("1.0".to_string()),
            view: InstanceView::new(Vec::new()),
            could_not_check: Vec::new(),
        }))
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    ctx: agora_core::ctx::CoreContext,
    logs_dir: PathBuf,
    host: QuickJsHost,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = agora_core::ctx::CoreContext::for_testing(tmp.path().join("data"));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let logs_dir = ctx.paths.instance_dir("inst").unwrap().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        Self {
            _tmp: tmp,
            ctx,
            logs_dir,
            host: QuickJsHost::new(),
        }
    }

    fn conn(&self) -> Connection {
        agora_core::db::local_state_connection(&self.ctx.paths.local_state_db()).unwrap()
    }

    fn write_log(&self, name: &str, modified: SystemTime) {
        let path = self.logs_dir.join(name);
        std::fs::write(&path, "ERRORS\naccess_violation Mod.dll\n").unwrap();
        set_modified(&path, modified);
    }

    fn live<'a>(&'a self, built_in: &'a [BuiltInPack]) -> LivePacks<'a> {
        LivePacks {
            packs: &[],
            built_in,
            host: &self.host,
            instances: Some(&DummyProvider),
        }
    }

    fn catch_up(&self, built_in: &[BuiltInPack]) -> agora_core::crash_live::CatchUpReport {
        catch_up(&self.ctx, "inst", self.live(built_in))
    }
}

fn set_modified(path: &Path, modified: SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(modified).unwrap();
}

fn begin(conn: &Connection, started: SystemTime, owner: Owner) -> String {
    let started = chrono::DateTime::<chrono::Utc>::from(started).to_rfc3339();
    session_ledger::begin(conn, "inst", &started, &LaunchManifest::default(), owner).unwrap()
}

fn time_pack() -> BuiltInPack {
    BuiltInPack {
        file: "test/window/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": ["test-game"],
            "sources": [{
                "id": "crash_log",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "crash-*.log" },
                "format": "probe",
                "bind": [{ "time": { "before_s": 2, "after_s": 10 } }],
                "role": "primary"
            }],
            "parsers": [{
                "id": "probe",
                "detect": { "all": ["(?m)^ERRORS$"] },
                "entry": { "generic": { "name": "text_sections", "config": {
                    "producer": { "format": "probe_log" },
                    "sections": [{
                        "id": "errors",
                        "header": "^ERRORS$",
                        "kind": "core.exception",
                        "lines": [{
                            "pattern": "^(?P<category>\\w+) (?P<mod>\\S+)$",
                            "attrs": { "category": "category" },
                            "refs": [{ "kind": "module", "value": "mod", "strength": "exec" }]
                        }]
                    }]
                } } }
            }],
            "rules": []
        })
        .to_string(),
        parsers: Vec::new(),
    }
}

fn stored_answer(session: &str, hashes: Vec<String>) -> SessionAnswer {
    let mut headline = LocalText::new();
    headline.insert("en".to_string(), "Already answered elsewhere.".to_string());
    SessionAnswer {
        session_id: session.to_string(),
        instance_id: "inst".to_string(),
        created_at: chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339(),
        answer: StoredAnswer::Analyzed {
            answer: Box::new(CrashAnswer {
                contract: agora_game_api::crash::CRASH_CONTRACT_VERSION,
                verdict: SessionVerdict::Crashed {
                    how: MatchedBy::Time,
                },
                headline,
                level: Level::Likely,
                candidates: Vec::new(),
                findings: Vec::new(),
                inspections: Vec::new(),
                could_not_check: Vec::new(),
                trust_notes: Vec::new(),
                next: NextStep {
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
            artifact_hashes: hashes,
            artifact_occurrences: Vec::new(),
        },
    }
}

#[test]
fn an_orphan_closed_before_the_catch_up_is_still_diagnosed() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let s1_start = now - Duration::from_secs(2 * 3600);
    let s1 = begin(
        &conn,
        s1_start,
        Owner {
            pid: u32::MAX - 2,
            started: 5,
        },
    );
    // The crash happened 30 minutes ago, inside the orphan's window and before the next session.
    fixture.write_log("crash-old.log", now - Duration::from_secs(30 * 60));
    // The launch path closes the orphan itself, then opens the next session.
    let now_text = chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339();
    session_ledger::close_orphans(&conn, &now_text, &|_, _| false, &|_| false).unwrap();
    let s2_start = now + Duration::from_secs(1);
    let s2 = begin(&conn, s2_start, Owner::current());
    let closed = session_ledger::get(&conn, &s1).unwrap().unwrap();
    assert_eq!(closed.verdict, Some(SessionVerdict::ClosedUnknown));
    drop(conn);

    let pack = time_pack();
    let report = fixture.catch_up(std::slice::from_ref(&pack));

    let conn = fixture.conn();
    let answer = answer_for_session(&conn, &s1).unwrap();
    assert!(
        answer.is_some(),
        "a session the launch path closed first must still be diagnosed: {report:?}"
    );
    assert!(answer_for_session(&conn, &s2).unwrap().is_none());
}

#[test]
fn a_log_another_sessions_answer_already_read_is_not_used_again() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let log_modified = now - Duration::from_secs(90 * 60);
    fixture.write_log("crash-shared.log", log_modified);
    let hash =
        agora_core::crash_history::content_hash_file(&fixture.logs_dir.join("crash-shared.log"))
            .unwrap();
    // Another session of the same instance already answered this exact log.
    let answered = begin(&conn, now - Duration::from_secs(4 * 3600), Owner::current());
    store_answer(&conn, &stored_answer(&answered, vec![hash])).unwrap();

    // This session ran at the same time (an overlap, or a stale row) and its window covers the log.
    let s1_start = now - Duration::from_secs(2 * 3600);
    let s1 = begin(&conn, s1_start, Owner::current());
    let ended = now - Duration::from_secs(60 * 60);
    session_ledger::finish(
        &conn,
        &s1,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ended).to_rfc3339(),
            duration_ms: 3_600_000,
            exit: ExitObservation::Code {
                pid: 4242,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(3_600_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let pack = time_pack();
    let live = fixture.live(std::slice::from_ref(&pack));
    let answer = agora_core::crash_live::diagnose_session(&fixture.ctx, &s1, live)
        .expect("a crashed session gets an answer");

    match answer.answer {
        StoredAnswer::NothingToRead { .. } => {}
        other => panic!(
            "the shared log belongs to another session's answer, so this one has nothing to read: \
             {other:?}"
        ),
    }
}

#[test]
fn without_a_later_session_the_window_ends_now_and_still_sees_the_crash() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let s1_start = now - Duration::from_secs(2 * 3600);
    let s1 = begin(
        &conn,
        s1_start,
        Owner {
            pid: u32::MAX - 2,
            started: 5,
        },
    );
    fixture.write_log("crash-now.log", now - Duration::from_secs(60));
    drop(conn);

    let pack = time_pack();
    let _ = fixture.catch_up(std::slice::from_ref(&pack));

    let conn = fixture.conn();
    let answer = answer_for_session(&conn, &s1).unwrap();
    assert!(
        answer.is_some(),
        "with no later session the window ends at now, so the crash is still its evidence"
    );
}


/// Two separate crashes can produce byte-identical logs. Each session must still read its own
/// evidence; a content hash alone does not make them the same event.
#[test]
fn identical_logs_in_separate_session_windows_are_both_evidence() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let pack = time_pack();

    // Session 1 crashes and writes its own log.
    let s1_start = now - Duration::from_secs(4 * 3600);
    let s1_end = s1_start + Duration::from_secs(30);
    fixture.write_log("crash-first.log", s1_start + Duration::from_secs(5));
    let s1 = begin(&conn, s1_start, Owner::current());
    session_ledger::finish(
        &conn,
        &s1,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(s1_end).to_rfc3339(),
            duration_ms: 30_000,
            exit: ExitObservation::Code {
                pid: 1111,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(30_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();

    // A separate crash later writes a different log file with identical bytes.
    let s2_start = now - Duration::from_secs(2 * 3600);
    let s2_end = s2_start + Duration::from_secs(30);
    fixture.write_log("crash-second.log", s2_start + Duration::from_secs(5));
    let s2 = begin(&conn, s2_start, Owner::current());
    session_ledger::finish(
        &conn,
        &s2,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(s2_end).to_rfc3339(),
            duration_ms: 30_000,
            exit: ExitObservation::Code {
                pid: 2222,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(30_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let packs = [pack];
    let a1 = agora_core::crash_live::diagnose_session(&fixture.ctx, &s1, fixture.live(&packs))
        .expect("session 1 is diagnosable");
    let a2 = agora_core::crash_live::diagnose_session(&fixture.ctx, &s2, fixture.live(&packs))
        .expect("session 2 is diagnosable");

    assert!(
        matches!(a1.answer, StoredAnswer::Analyzed { .. }),
        "session 1 should read its own crash log: {:?}",
        a1.answer
    );
    assert!(
        matches!(a2.answer, StoredAnswer::Analyzed { .. }),
        "session 2's identical log is a separate crash and must not be dropped: {:?}",
        a2.answer
    );
}

/// The same log file occurrence that another session already read must not be consumed again.
#[test]
fn a_lingering_log_occurrence_across_session_windows_is_used_once() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let pack = time_pack();

    let log_modified = now - Duration::from_secs(90 * 60);
    fixture.write_log("crash-shared.log", log_modified);

    // Session 1 answers this log.
    let s1_start = now - Duration::from_secs(2 * 3600);
    let s1 = begin(&conn, s1_start, Owner::current());
    let ended = now - Duration::from_secs(80 * 60);
    session_ledger::finish(
        &conn,
        &s1,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ended).to_rfc3339(),
            duration_ms: 2_400_000,
            exit: ExitObservation::Code {
                pid: 4242,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(2_400_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let packs = [pack];
    let a1 = agora_core::crash_live::diagnose_session(&fixture.ctx, &s1, fixture.live(&packs))
        .expect("session 1 is diagnosable");
    assert!(
        matches!(a1.answer, StoredAnswer::Analyzed { .. }),
        "session 1 should read the crash log: {:?}",
        a1.answer
    );

    // Another session window still covers the same lingering log file occurrence.
    let conn = fixture.conn();
    let s2_start = now - Duration::from_secs(100 * 60);
    let s2 = begin(&conn, s2_start, Owner::current());
    let ended = now - Duration::from_secs(70 * 60);
    session_ledger::finish(
        &conn,
        &s2,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ended).to_rfc3339(),
            duration_ms: 1_800_000,
            exit: ExitObservation::Code {
                pid: 4343,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(1_800_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let a2 = agora_core::crash_live::diagnose_session(&fixture.ctx, &s2, fixture.live(&packs))
        .expect("session 2 is diagnosable");
    match a2.answer {
        StoredAnswer::NothingToRead { .. } => {}
        other => panic!(
            "the same lingering log occurrence belongs to session 1, so session 2 has nothing to read: {other:?}"
        ),
    }
}

fn facts_only_pack() -> BuiltInPack {
    BuiltInPack {
        file: "test/facts-only/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": ["test-game"],
            "sources": [{
                "id": "facts_log",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "facts-*.log" },
                "format": "probe",
                "bind": [{ "time": { "before_s": 2, "after_s": 10 } }],
                "role": "primary",
                "copy": "facts_only"
            }],
            "parsers": [{
                "id": "probe",
                "detect": { "all": ["(?m)^ERRORS$"] },
                "entry": { "generic": { "name": "text_sections", "config": {
                    "producer": { "format": "probe_log" },
                    "sections": [{
                        "id": "errors",
                        "header": "^ERRORS$",
                        "kind": "core.exception",
                        "lines": [{
                            "pattern": "^(?P<category>\\w+) (?P<mod>\\S+)$",
                            "attrs": { "category": "category" },
                            "refs": [{ "kind": "module", "value": "mod", "strength": "exec" }]
                        }]
                    }]
                } } }
            }],
            "rules": []
        })
        .to_string(),
        parsers: Vec::new(),
    }
}

/// A facts-only artifact is parsed in place and contributes to diagnosis, but its raw file is never
/// copied into the evidence store.
#[test]
fn a_facts_only_artifact_contributes_without_being_copied() {
    let fixture = Fixture::new();
    let conn = fixture.conn();
    let now = SystemTime::now();
    let pack = facts_only_pack();

    let started = now - Duration::from_secs(2 * 3600);
    let ended = started + Duration::from_secs(30);
    let path = fixture.logs_dir.join("facts-crash.log");
    std::fs::write(&path, "ERRORS\naccess_violation Mod.dll\n").unwrap();
    set_modified(&path, started + Duration::from_secs(5));
    let session = begin(&conn, started, Owner::current());
    session_ledger::finish(
        &conn,
        &session,
        &SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ended).to_rfc3339(),
            duration_ms: 30_000,
            exit: ExitObservation::Code {
                pid: 5555,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(30_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let answer = agora_core::crash_live::diagnose_session(
        &fixture.ctx,
        &session,
        fixture.live(std::slice::from_ref(&pack)),
    )
    .expect("facts-only session is diagnosable");

    match answer.answer {
        StoredAnswer::Analyzed { .. } => {}
        other => panic!("facts-only artifact should contribute to diagnosis: {other:?}"),
    }

    // The raw evidence must not be copied to the store.
    let session_dir = fixture
        .ctx
        .paths
        .root()
        .join(agora_core::crash_engine::evidence::EVIDENCE_STORE_DIR)
        .join(&session);
    if session_dir.exists() {
        let mut count = 0usize;
        for entry in std::fs::read_dir(&session_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                count += 1;
            }
        }
        assert_eq!(count, 0, "facts-only raw file must not be copied: {session_dir:?}");
    }
}
