//! A Minecraft launch that crashes stores an engine answer, over a fake session and a temp
//! instance. No game runs and no file leaves the test's temporary folder.

use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::lkg::LaunchOutcome;
use agora_core::models::{InstanceManifest, InstanceRow, CURRENT_MANIFEST_VERSION};
use agora_core::session_ledger;
use agora_game_api::crash::SessionVerdict;
use agora_plugin_api::host::ScriptHost;

fn instance_row(instance_id: &str) -> InstanceRow {
    InstanceRow {
        instance_id: instance_id.to_string(),
        name: "Example Instance".to_string(),
        minecraft_version: "1.21".to_string(),
        loader: "fabric".to_string(),
        loader_version: "0.16.0".to_string(),
        is_modpack: false,
        is_locked: false,
        last_launched_at: None,
        jvm_memory_mb: 4096,
        jvm_memory_mode: "auto".to_string(),
        jvm_gc: "g1".to_string(),
        jvm_custom_args: String::new(),
        jvm_always_pre_touch: false,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        java_path: None,
        java_incompatible_override: false,
        icon_path: None,
        launch_mode_override: "direct".to_string(),
        import_source: None,
        total_play_ms: 0,
    }
}

fn manifest(instance_id: &str) -> InstanceManifest {
    InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: instance_id.to_string(),
        name: "Example Instance".to_string(),
        created_from_pack: None,
        minecraft_version: "1.21".to_string(),
        loader: "fabric".to_string(),
        loader_version: "0.16.0".to_string(),
        is_locked: false,
        mods: Vec::new(),
        resourcepacks: Vec::new(),
        shaders: Vec::new(),
        datapacks: Vec::new(),
        worlds: Vec::new(),
        user_preferences: serde_json::Value::Null,
    }
}

/// A temp context with one instance, its manifest, and a crash report whose mtime is inside the
/// session the ledger opens.
fn fixture() -> (tempfile::TempDir, CoreContext, String) {
    let tmp = tempfile::Builder::new()
        .prefix(&format!("agora-mc-live-{}-", std::process::id()))
        .tempdir()
        .unwrap();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let instance_id = "example-instance".to_string();
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    agora_core::db::upsert_instance(&conn, &instance_row(&instance_id)).unwrap();
    drop(conn);

    let manifest_path = ctx.paths.instance_manifest(&instance_id).unwrap();
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest(&instance_id)).unwrap(),
    )
    .unwrap();

    let instance_dir = ctx.paths.instance_dir(&instance_id).unwrap();
    let reports = instance_dir.join("crash-reports");
    std::fs::create_dir_all(&reports).unwrap();
    let report = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("agora-game-minecraft")
        .join("tests")
        .join("crash_fixtures")
        .join("crash_fabric_missing_dependency.txt");
    // Write rather than copy: a copy keeps the fixture file's timestamp, but the game writes a
    // crash report during the session, and the source binds it by time.
    let bytes = std::fs::read(report).unwrap();
    std::fs::write(reports.join("crash-example.txt"), bytes).unwrap();
    (tmp, ctx, instance_id)
}

fn diagnose(ctx: &CoreContext, session: &str) {
    let host: std::sync::Arc<dyn agora_plugin_api::host::ScriptHost> =
        std::sync::Arc::new(agora_plugin_host::QuickJsHost::new());
    agora_game_minecraft::crash::diagnose_minecraft_session(ctx, session, host)
        .expect("the crash is diagnosed");
}

#[test]
fn a_crashed_minecraft_session_stores_an_engine_answer() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    let verdict = session_ledger::finish_minecraft(
        &ctx,
        &opened.session_id,
        4242,
        &LaunchOutcome::Crash,
        60_000,
    )
    .expect("the session is closed");
    assert_eq!(
        verdict,
        SessionVerdict::Crashed {
            how: agora_game_api::crash::MatchedBy::Time
        }
    );

    diagnose(&ctx, &opened.session_id);

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let stored = agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
        .unwrap()
        .expect("an answer is stored");
    match stored.answer {
        agora_core::crash_live::StoredAnswer::Analyzed { answer, .. } => {
            assert!(
                !answer.findings.is_empty(),
                "the engine found something: {answer:?}"
            );
            assert_eq!(
                answer.contract,
                agora_game_api::crash::CRASH_CONTRACT_VERSION
            );
            assert!(
                answer
                    .findings
                    .iter()
                    .all(|finding| finding.source.plugin_name == "Agora"),
                "a built-in pack's findings name Agora: {answer:?}"
            );
        }
        other => panic!("expected an analyzed answer, got {other:?}"),
    }
}

#[test]
fn a_clean_minecraft_session_stores_no_answer() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    let verdict = session_ledger::finish_minecraft(
        &ctx,
        &opened.session_id,
        4242,
        &LaunchOutcome::Success,
        600_000,
    )
    .expect("the session is closed");
    assert_eq!(verdict, SessionVerdict::Normal);
    assert!(
        agora_game_minecraft::crash::diagnose_minecraft_session(
            &ctx,
            &opened.session_id,
            std::sync::Arc::new(agora_plugin_host::QuickJsHost::new()),
        )
        .is_none(),
        "a normal end is not a crash to diagnose"
    );
}

#[test]
fn a_stopped_minecraft_session_is_stopped_not_a_crash() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    let verdict = session_ledger::finish_minecraft(
        &ctx,
        &opened.session_id,
        4242,
        &LaunchOutcome::Cancelled,
        30_000,
    )
    .expect("the session is closed");
    assert_eq!(verdict, SessionVerdict::Stopped);
}

#[test]
fn a_live_session_reads_the_captured_launch_output_when_no_report_was_written() {
    let (_tmp, ctx, instance_id) = fixture();
    let dir = ctx.paths.instance_dir(&instance_id).unwrap();
    // The fixture's report is not part of this session.
    std::fs::remove_file(dir.join("crash-reports").join("crash-example.txt")).unwrap();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("agora-launch-output.log"),
        "# Agora captured Java output (sanitized, last 200 lines)\n\
         # exit_code=1\n\
         # runtime_ms=42000\n\
         [stdout] [01:02:03] [main/INFO]: Loading Minecraft 1.21 with Fabric Loader 0.16.0\n\
         [stderr] [01:02:04] [main/ERROR]: java.lang.OutOfMemoryError: Java heap space\n",
    )
    .unwrap();
    session_ledger::finish_minecraft(
        &ctx,
        &opened.session_id,
        4242,
        &LaunchOutcome::Crash,
        60_000,
    )
    .expect("the session is closed");

    diagnose(&ctx, &opened.session_id);

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let stored = agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
        .unwrap()
        .expect("an answer is stored");
    match stored.answer {
        agora_core::crash_live::StoredAnswer::Analyzed { answer, files, .. } => {
            assert!(
                files
                    .iter()
                    .any(|file| file.name == "agora-launch-output.log"),
                "the captured output was read: {files:?}"
            );
            assert!(
                answer.findings.iter().any(|finding| finding
                    .headline
                    .get("en")
                    .is_some_and(|text| text.to_lowercase().contains("out of memory"))),
                "the memory finding is in the answer: {answer:?}"
            );
        }
        other => panic!("expected an analyzed answer, got {other:?}"),
    }
}

/// Open a session whose owner process is gone, as a CLI or desktop that closed mid-session would
/// leave it. The game is not running: this fixture writes no runtime lease.
fn abandon_owner(ctx: &CoreContext, session_id: &str) {
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "UPDATE launch_history SET owner_pid = ?2, owner_start = ?3 WHERE session_id = ?1",
        rusqlite::params![session_id, u32::MAX - 2, 5_i64],
    )
    .unwrap();
}

fn catch_up(ctx: &CoreContext, instance_id: &str) -> agora_core::crash_live::CatchUpReport {
    let host: Arc<dyn ScriptHost> = Arc::new(agora_plugin_host::QuickJsHost::new());
    let installed = agora_core::crash_live::load_installed_packs(ctx, host.clone()).unwrap();
    let built_in = agora_game_minecraft::crash::built_in_packs();
    let instances = agora_game_minecraft::installed_content::MinecraftInstanceProvider;
    agora_core::crash_live::catch_up(
        ctx,
        instance_id,
        agora_core::crash_live::LivePacks {
            packs: &installed.packs,
            built_in: &built_in,
            host: host.as_ref(),
            instances: Some(&instances),
        },
    )
}

#[test]
fn an_unwatched_session_is_closed_and_diagnosed() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    abandon_owner(&ctx, &opened.session_id);

    let caught = catch_up(&ctx, &instance_id);
    assert_eq!(caught.sessions.len(), 1, "{caught:?}");
    assert!(
        caught.sessions[0].answer.is_some(),
        "the crash report written during the session is diagnosed"
    );

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let record = session_ledger::get(&conn, &opened.session_id)
        .unwrap()
        .unwrap();
    assert!(record.ended_at.is_some(), "the session is closed");
    // Crash evidence inside the window decides the verdict, even though nothing watched the end.
    assert_eq!(
        record.verdict,
        Some(SessionVerdict::Crashed {
            how: agora_game_api::crash::MatchedBy::Time
        })
    );
    assert!(
        agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
            .unwrap()
            .is_some(),
        "an answer is stored beside the closed session"
    );
}

#[test]
fn a_finished_session_is_not_diagnosed_twice() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    abandon_owner(&ctx, &opened.session_id);

    let first = catch_up(&ctx, &instance_id);
    assert_eq!(first.sessions.len(), 1);
    let created_at = first.sessions[0]
        .answer
        .as_ref()
        .expect("the first catch-up answers")
        .created_at
        .clone();

    let second = catch_up(&ctx, &instance_id);
    assert!(
        second.sessions.is_empty(),
        "an ended session is left alone: {second:?}"
    );

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let stored = agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.created_at, created_at, "the answer was not replaced");
}

#[test]
fn a_session_whose_game_might_still_run_stays_open() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    abandon_owner(&ctx, &opened.session_id);

    // This test process is a live process with a verifiable identity.
    let identity = agora_core::process_identity::capture(std::process::id()).unwrap();
    let generation =
        agora_core::instance_runtime::publish_starting(&ctx.paths, &instance_id).unwrap();
    agora_core::instance_runtime::publish_running(&ctx.paths, &instance_id, &generation, identity)
        .unwrap();

    let caught = catch_up(&ctx, &instance_id);
    assert!(
        caught.sessions.is_empty(),
        "a game that may run keeps its session open"
    );

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    assert!(session_ledger::get(&conn, &opened.session_id)
        .unwrap()
        .unwrap()
        .ended_at
        .is_none());
    assert!(
        agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
            .unwrap()
            .is_none()
    );

    agora_core::instance_runtime::clear(&ctx.paths, &instance_id, Some(&generation)).unwrap();
}

#[test]
fn crash_last_closes_an_unwatched_session_and_stores_its_answer() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    abandon_owner(&ctx, &opened.session_id);

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_agora"))
        .arg("--data-dir")
        .arg(ctx.paths.root())
        .args(["crash", "last", &instance_id])
        .output()
        .expect("the agora binary runs");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let record = session_ledger::get(&conn, &opened.session_id)
        .unwrap()
        .unwrap();
    assert!(record.ended_at.is_some(), "crash last closed the session");
    let stored = agora_core::crash_live::answer_for_session(&conn, &opened.session_id)
        .unwrap()
        .expect("crash last stored an answer");
    assert!(matches!(
        stored.answer,
        agora_core::crash_live::StoredAnswer::Analyzed { .. }
    ));
}

#[test]
fn an_unwatched_session_with_no_evidence_stays_open() {
    let (_tmp, ctx, instance_id) = fixture();
    let dir = ctx.paths.instance_dir(&instance_id).unwrap();
    std::fs::remove_file(dir.join("crash-reports").join("crash-example.txt")).unwrap();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    abandon_owner(&ctx, &opened.session_id);

    // No runtime record and no crash report: a delegated game may still be running.
    let caught = catch_up(&ctx, &instance_id);
    assert!(
        caught.sessions.is_empty(),
        "no evidence means no close: {caught:?}"
    );
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    assert!(session_ledger::get(&conn, &opened.session_id)
        .unwrap()
        .unwrap()
        .ended_at
        .is_none());
}

#[test]
fn a_failed_to_start_verdict_is_corrected_when_the_crash_log_is_found() {
    let (_tmp, ctx, instance_id) = fixture();
    let opened = session_ledger::open_minecraft(&ctx, &instance_id).expect("a session is opened");
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let done = session_ledger::SessionFinish {
        ended_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
        duration_ms: 59_000,
        exit: session_ledger::ExitObservation::Code {
            pid: 4242,
            image: "javaw".to_string(),
            code: 1,
        },
        game_runtime_ms: Some(59_000),
        normal_exit_codes: vec![0],
        artifact: None,
        froze: false,
    };
    assert_eq!(
        session_ledger::finish(&conn, &opened.session_id, &done).unwrap(),
        Some(SessionVerdict::FailedToStart)
    );
    drop(conn);

    diagnose(&ctx, &opened.session_id);

    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let record = session_ledger::get(&conn, &opened.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.verdict,
        Some(SessionVerdict::Crashed {
            how: agora_game_api::crash::MatchedBy::Time
        }),
        "the crash report written during the session is the verdict, not the short exit"
    );
}
