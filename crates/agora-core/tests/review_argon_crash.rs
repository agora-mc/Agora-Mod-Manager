//! Review proofs for crash session diagnosis, evidence binding, and answer selection.

use agora_core::crash_engine::{
    answer, consensus, AnswerInput, ConsensusInput, ConsensusProfile, EvaluateOutput,
    PluginIdentity,
};
use agora_core::crash_live::{
    answer_for_session, latest_answer, latest_answer_with_context, store_answer, LivePacks,
    SessionAnswer, StoredAnswer,
};
use agora_core::session_ledger::{self, ExitObservation, LaunchManifest, Owner, SessionVerdict};
use agora_game_api::crash::{
    BuiltInPack, Cause, CrashAnswer, Family, Finding, Level, LocalText, MatchedBy, NextStep,
    NextStepKind, Owner as CrashOwner, Scope, Signal,
};
use agora_plugin_host::QuickJsHost;
use rusqlite::Connection;
use std::collections::BTreeMap;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    agora_core::db::run_migrations(&conn).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    conn
}

fn make_answer(session: &str, instance: &str, created: &str, headline: &str) -> SessionAnswer {
    let mut text = LocalText::new();
    text.insert("en".to_string(), headline.to_string());
    SessionAnswer {
        session_id: session.to_string(),
        instance_id: instance.to_string(),
        created_at: created.to_string(),
        answer: StoredAnswer::Analyzed {
            answer: Box::new(CrashAnswer {
                contract: agora_game_api::crash::CRASH_CONTRACT_VERSION,
                verdict: SessionVerdict::Crashed {
                    how: MatchedBy::ExitStatus,
                },
                headline: text,
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
            artifact_hashes: Vec::new(),
            artifact_occurrences: Vec::new(),
        },
    }
}

#[test]
fn latest_answer_returns_older_caught_up_session_instead_of_newest_session() {
    let conn = db();
    // Session 1 ran at 01:00 and was unwatched; Session 2 ran at 02:00 and was diagnosed immediately.
    conn.execute(
        "INSERT INTO launch_history (instance_id, started_at, ended_at, session_id)
         VALUES ('inst', '2026-01-01T01:00:00Z', '2026-01-01T01:10:00Z', 'session-old')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO launch_history (instance_id, started_at, ended_at, session_id)
         VALUES ('inst', '2026-01-01T02:00:00Z', '2026-01-01T02:10:00Z', 'session-newest')",
        [],
    )
    .unwrap();

    // Session 2 is diagnosed right when it finishes at 02:10:05.
    store_answer(
        &conn,
        &make_answer(
            "session-newest",
            "inst",
            "2026-01-01T02:10:05Z",
            "Newest crash",
        ),
    )
    .unwrap();
    // Later at 03:00, catch-up or re-diagnosis stores an answer for the older session-old.
    store_answer(
        &conn,
        &make_answer("session-old", "inst", "2026-01-01T03:00:00Z", "Older crash"),
    )
    .unwrap();

    let with_ctx = latest_answer_with_context(&conn, "inst").unwrap();
    let direct = latest_answer(&conn, "inst").unwrap().unwrap();

    assert_eq!(
        with_ctx.answer.unwrap().session_id,
        "session-newest",
        "answers_for_instance orders by session time"
    );
    assert_eq!(
        direct.session_id, "session-newest",
        "latest_answer ordered by created_at instead of session time, returning the older session"
    );
}

#[test]
fn pid_in_facts_bind_never_vetoes_foreign_pid_during_live_collection() {
    use agora_core::crash_engine::attribution::InstanceView;
    use agora_core::crash_engine::instance_view::{ExtraInstanceProvider, ResolvedCrashInstance};

    struct DummyProvider {
        dir: std::path::PathBuf,
    }
    impl ExtraInstanceProvider for DummyProvider {
        fn resolve(
            &self,
            _ctx: &agora_core::ctx::Ctx,
            _instance_id: &str,
        ) -> agora_core::error::LauncherResult<Option<ResolvedCrashInstance>> {
            let _ = &self.dir;
            Ok(Some(ResolvedCrashInstance {
                game_id: "test-game".to_string(),
                runtime_version: Some("1.0".to_string()),
                view: InstanceView::new(Vec::new()),
                could_not_check: Vec::new(),
            }))
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let ctx = agora_core::ctx::CoreContext::for_testing(tmp.path().join("data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let inst_dir = ctx.paths.instance_dir("inst-a").unwrap();
    let logs_dir = inst_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();

    // Write a crash log whose body states Process ID: 9999 (a different process/instance),
    // while our session ran with PID 1234.
    std::fs::write(
        logs_dir.join("crash-foreign.log"),
        "HEADER\nProcess ID: 9999\n",
    )
    .unwrap();

    let built_in = [BuiltInPack {
        file: "test/pid-facts/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": ["test-game"],
            "sources": [{
                "id": "crash_logger",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "crash-*.log" },
                "format": "probe",
                "bind": [
                    "pid_in_facts",
                    { "time": { "before_s": 3600, "after_s": 3600 } }
                ],
                "role": "primary"
            }],
            "parsers": [{
                "id": "probe",
                "detect": { "all": ["(?m)^HEADER$"] },
                "entry": { "generic": { "name": "text_sections", "config": {
                    "producer": { "format": "probe_log" },
                    "sections": [{
                        "id": "proc",
                        "header": "^HEADER$",
                        "kind": "core.process",
                        "lines": [{
                            "pattern": "^Process ID: (?P<pid>\\d+)$",
                            "attrs": { "pid": "pid" }
                        }]
                    }]
                } } }
            }],
            "rules": []
        })
        .to_string(),
        parsers: Vec::new(),
    }];

    let host = QuickJsHost::new();
    let provider = DummyProvider { dir: inst_dir };
    let now = std::time::SystemTime::now();
    let session = agora_core::crash_engine::evidence::CrashSession {
        session_id: "s1".to_string(),
        pid: Some(1234),
        working_dir: None,
        window: agora_core::crash_engine::evidence::SessionWindow::new(
            now - std::time::Duration::from_secs(60),
            now + std::time::Duration::from_secs(60),
        ),
    };

    let evidence = agora_core::crash_live::collect_instance_evidence(
        &ctx,
        "inst-a",
        &LivePacks {
            packs: &[],
            built_in: &built_in,
            host: &host,
            instances: Some(&provider),
        },
        &session,
        &[],
    )
    .unwrap();

    assert!(
        evidence.collection.bound.is_empty(),
        "foreign PID 9999 log was bound to session with PID 1234 because NoArtifactFacts was used: {:?}",
        evidence.collection.bound
    );
}

#[test]
fn unwatched_session_has_infinite_window_and_steals_later_sessions_crash_log() {
    use agora_core::crash_engine::attribution::InstanceView;
    use agora_core::crash_engine::instance_view::{ExtraInstanceProvider, ResolvedCrashInstance};

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

    let tmp = tempfile::tempdir().unwrap();
    let ctx = agora_core::ctx::CoreContext::for_testing(tmp.path().join("data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let inst_dir = ctx.paths.instance_dir("inst").unwrap();
    let logs_dir = inst_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    // Session 1 started 2 hours ago and was abandoned (owner_pid is dead) with no crash log.
    let s1_start = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    let s1 = session_ledger::begin(
        &conn,
        "inst",
        &s1_start,
        &LaunchManifest::default(),
        Owner {
            pid: u32::MAX - 2,
            started: 5,
        },
    )
    .unwrap();

    // Session 2 started 1 minute ago, crashed just now, and wrote a crash log now.
    let s2_start = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
    let s2_end = chrono::Utc::now().to_rfc3339();
    let s2 = session_ledger::begin(
        &conn,
        "inst",
        &s2_start,
        &LaunchManifest::default(),
        Owner::current(),
    )
    .unwrap();
    std::fs::write(
        logs_dir.join("crash-now.log"),
        "ERRORS\naccess_violation Mod.dll\n",
    )
    .unwrap();
    session_ledger::finish(
        &conn,
        &s2,
        &session_ledger::SessionFinish {
            ended_at: s2_end,
            duration_ms: 60_000,
            exit: ExitObservation::Code {
                pid: 4242,
                image: "Game.exe".to_string(),
                code: 0xC000_0005,
            },
            game_runtime_ms: Some(60_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
    drop(conn);

    let built_in = [BuiltInPack {
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
    }];

    let host = QuickJsHost::new();
    let provider = DummyProvider;
    let live = LivePacks {
        packs: &[],
        built_in: &built_in,
        host: &host,
        instances: Some(&provider),
    };
    let _ = agora_core::crash_live::diagnose_session(&ctx, &s2, live);

    // Now catch_up runs (as `crash last` does).
    let report = agora_core::crash_live::catch_up(
        &ctx,
        "inst",
        LivePacks {
            packs: &[],
            built_in: &built_in,
            host: &host,
            instances: Some(&provider),
        },
    );

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let s1_ans = answer_for_session(&conn, &s1).unwrap();
    assert!(
        s1_ans.is_none(),
        "unwatched session s1 from 2 hours ago stretched its window to `now` and stole s2's crash log: {report:?}"
    );
}

#[test]
fn abstained_consensus_lets_single_family_finding_win_headline_and_level() {
    let plugin = PluginIdentity {
        id: "test.plugin".to_string(),
        name: "Test Plugin".to_string(),
        version: "1.0.0".to_string(),
    };
    let owner_a = CrashOwner::Item {
        key: "mod-a".to_string(),
        name: "Mod A".to_string(),
    };
    let owner_b = CrashOwner::Item {
        key: "mod-b".to_string(),
        name: "Mod B".to_string(),
    };
    // Both Mod A and Mod B have identical core score 20, so consensus abstains (level = NotSure).
    let signals = vec![
        Signal {
            family: Family::Execution,
            candidate: owner_a,
            weight: 20,
            cause: Some(Cause::Mod),
            also_provided_by: Vec::new(),
            source: plugin.parser_source("p"),
            evidence: Vec::new(),
        },
        Signal {
            family: Family::Execution,
            candidate: owner_b,
            weight: 20,
            cause: Some(Cause::Mod),
            also_provided_by: Vec::new(),
            source: plugin.parser_source("p"),
            evidence: Vec::new(),
        },
    ];
    let roles = BTreeMap::new();
    let scored = consensus(
        &ConsensusInput {
            signals: &signals,
            roles: &roles,
            level_caps: &[],
        },
        &ConsensusProfile::default(),
    );
    assert!(scored.abstained);
    assert_eq!(scored.level, Level::NotSure);

    // An environment finding with level Likely (or Possible) is also present.
    let mut env_headline = LocalText::new();
    env_headline.insert("en".to_string(), "System memory was 90% full.".to_string());
    let evaluated = EvaluateOutput {
        signals,
        findings: vec![Finding {
            scope: Scope::Environment,
            level: Level::Possible,
            cause: Cause::Memory,
            candidate: None,
            headline: env_headline,
            why_not_sure: None,
            source: plugin.parser_source("p"),
            evidence: Vec::new(),
        }],
        ..EvaluateOutput::default()
    };

    let ans = answer(&AnswerInput {
        verdict: SessionVerdict::Crashed {
            how: MatchedBy::Supplied,
        },
        evaluated: &evaluated,
        consensus: &scored,
    });

    // Because consensus abstained between two tied mods, the headline should explain the tie
    // ("Not sure: Mod A and Mod B both have evidence..."), not behijacked by a weak side finding.
    let headline = ans.headline.get("en").unwrap();
    assert!(
        headline.contains("Mod A") && headline.contains("Mod B"),
        "abstained tie between two strong mod candidates was hidden by a Possible environment finding: {headline}"
    );
}
