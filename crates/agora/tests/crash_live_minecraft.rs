//! A Minecraft launch that crashes stores an engine answer, over a fake session and a temp
//! instance. No game runs and no file leaves the test's temporary folder.

use std::path::Path;

use agora_core::ctx::CoreContext;
use agora_core::lkg::LaunchOutcome;
use agora_core::models::{InstanceManifest, InstanceRow, CURRENT_MANIFEST_VERSION};
use agora_core::session_ledger;
use agora_game_api::crash::SessionVerdict;

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
