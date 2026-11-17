//! Live collection reads a candidate's own process facts, so the `pid_in_facts` and
//! `working_dir_in_facts` binds work there instead of sees nothing.

use agora_core::crash_engine::attribution::InstanceView;
use agora_core::crash_engine::evidence::{CrashSession, RejectedReason, SessionWindow};
use agora_core::crash_engine::instance_view::{ExtraInstanceProvider, ResolvedCrashInstance};
use agora_core::crash_live::{collect_instance_evidence, LivePacks};
use agora_game_api::crash::{BuiltInPack, MatchedBy};
use agora_plugin_host::QuickJsHost;
use std::path::PathBuf;
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

    fn write_log(&self, name: &str, text: &str) {
        std::fs::write(self.logs_dir.join(name), text).unwrap();
    }

    fn session(&self, window: SessionWindow) -> CrashSession {
        CrashSession {
            session_id: "s1".to_string(),
            pid: Some(1234),
            working_dir: Some(self.logs_dir.clone()),
            window,
        }
    }

    fn evidence(
        &self,
        built_in: &[BuiltInPack],
        session: &CrashSession,
    ) -> agora_core::crash_live::InstanceEvidence {
        collect_instance_evidence(
            &self.ctx,
            "inst",
            &LivePacks {
                packs: &[],
                built_in,
                host: &self.host,
                instances: Some(&DummyProvider),
            },
            session,
            &[],
        )
        .unwrap()
    }
}

fn process_pack(bind: serde_json::Value, detect: &str, line: &str, attrs: serde_json::Value) -> BuiltInPack {
    BuiltInPack {
        file: "test/process-pack/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": ["test-game"],
            "sources": [{
                "id": "logger",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "*.log" },
                "format": "probe",
                "bind": bind,
                "role": "primary"
            }],
            "parsers": [{
                "id": "probe",
                "detect": { "all": [detect] },
                "entry": { "generic": { "name": "text_sections", "config": {
                    "producer": { "format": "probe_log" },
                    "sections": [{
                        "id": "proc",
                        "header": detect,
                        "kind": "core.process",
                        "lines": [{ "pattern": line, "attrs": attrs }]
                    }]
                } } }
            }],
            "rules": []
        })
        .to_string(),
        parsers: Vec::new(),
    }
}

fn wide_window() -> SessionWindow {
    let now = SystemTime::now();
    SessionWindow::new(
        now - Duration::from_secs(3600),
        now + Duration::from_secs(3600),
    )
}

fn past_window() -> SessionWindow {
    let now = SystemTime::now();
    SessionWindow::new(
        now - Duration::from_secs(7200),
        now - Duration::from_secs(3600),
    )
}

#[test]
fn a_matching_process_fact_binds_by_pid_outside_the_time_window() {
    let fixture = Fixture::new();
    fixture.write_log("crash-1.log", "HEADER\nProcess ID: 1234\n");
    let pack = process_pack(
        serde_json::json!([
            "pid_in_facts",
            { "time": { "before_s": 2, "after_s": 2 } }
        ]),
        "(?m)^HEADER$",
        "^Process ID: (?P<pid>\\d+)$",
        serde_json::json!({ "pid": "pid" }),
    );

    let evidence = fixture.evidence(std::slice::from_ref(&pack), &fixture.session(past_window()));

    assert_eq!(evidence.collection.bound.len(), 1);
    assert_eq!(evidence.collection.bound[0].matched_by, MatchedBy::Pid);
}

#[test]
fn a_matching_working_folder_binds_even_when_the_name_and_time_do_not() {
    let fixture = Fixture::new();
    let cwd = fixture.logs_dir.to_string_lossy().into_owned();
    fixture.write_log("crash-2.log", &format!("HEADER\nWorking Directory: {cwd}\n"));
    let pack = process_pack(
        serde_json::json!([
            "working_dir_in_facts",
            { "time": { "before_s": 2, "after_s": 2 } }
        ]),
        "(?m)^HEADER$",
        "^Working Directory: (?P<cwd>.+)$",
        serde_json::json!({ "cwd": "cwd" }),
    );

    let evidence = fixture.evidence(std::slice::from_ref(&pack), &fixture.session(past_window()));

    assert_eq!(evidence.collection.bound.len(), 1);
    assert_eq!(
        evidence.collection.bound[0].matched_by,
        MatchedBy::WorkingDir
    );
}

#[test]
fn a_foreign_working_folder_vetoes_a_log_the_time_window_would_bind() {
    let fixture = Fixture::new();
    fixture.write_log(
        "crash-3.log",
        "HEADER\nWorking Directory: C:/somewhere/else\n",
    );
    let pack = process_pack(
        serde_json::json!([
            "working_dir_in_facts",
            { "time": { "before_s": 3600, "after_s": 3600 } }
        ]),
        "(?m)^HEADER$",
        "^Working Directory: (?P<cwd>.+)$",
        serde_json::json!({ "cwd": "cwd" }),
    );

    let evidence = fixture.evidence(std::slice::from_ref(&pack), &fixture.session(wide_window()));

    assert!(
        evidence.collection.bound.is_empty(),
        "another folder's log must not bind through the time fallback"
    );
    assert!(evidence.collection.seen.iter().any(|seen| {
        seen.name == "crash-3.log"
            && seen.reason == RejectedReason::WorkingDirMismatch
    }));
}

#[test]
fn a_log_no_parser_recognises_still_binds_by_time() {
    let fixture = Fixture::new();
    fixture.write_log("crash-4.log", "SOMETHING ELSE\nProcess ID: 1234\n");
    let pack = process_pack(
        serde_json::json!([
            "pid_in_facts",
            { "time": { "before_s": 3600, "after_s": 3600 } }
        ]),
        "(?m)^HEADER$",
        "^Process ID: (?P<pid>\\d+)$",
        serde_json::json!({ "pid": "pid" }),
    );

    let evidence = fixture.evidence(std::slice::from_ref(&pack), &fixture.session(wide_window()));

    assert_eq!(evidence.collection.bound.len(), 1);
    assert_eq!(evidence.collection.bound[0].matched_by, MatchedBy::Time);
}
