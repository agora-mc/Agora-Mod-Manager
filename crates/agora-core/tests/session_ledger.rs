//! The session ledger through real instances: what a launch records, that recording never gets in
//! the way, and how a session ends (MASTER_SPEC §30.4). No game runs; a copy of
//! `agora-session-crasher` stands in for it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{add_content, DeployMode};
use agora_core::game_discovery::{DiscoveredInstall, DiscoveryReport, InstallCapabilities};
use agora_core::game_instance::{
    self, create, GameInstanceRecord, LaunchOptions, LaunchedInstance, VfsFailure,
};
use agora_core::game_launch::{LaunchError, LaunchedGame, Launcher, PreparedLaunch};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::session_ledger::{self, ExitObservation, SessionVerdict};
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId, InstallKind,
    LaunchRecipe, PackageDefinition, RelPath, RuntimeIdentity, StoreId, StoreIdentifier,
    UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("test-game").unwrap(),
        extends: None,
        name: "Test Game".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "12345".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new(exe("Game")).unwrap(),
            },
            arguments: vec![],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        log_paths: vec![],
        crash_paths: vec![],
        // One per-user file, so a launch has a session journal for Stop to read.
        user_files: vec![UserFileMapping {
            source: GamePath::UserData {
                location: UserDataLocation::LocalAppData,
                path: RelPath::new("TestGame/config.txt").unwrap(),
            },
            instance_path: RelPath::new("user/config.txt").unwrap(),
            strategy: UserFileStrategy::JournaledSwap,
            stores: vec![StoreId::new("steam").unwrap()],
        }],
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: Vec::new(),
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        launch_alternatives: Vec::new(),
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

fn exe(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn registry(def: &GameDefinition) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    let package = PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks: vec![],
        tools: vec![],
    };
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(package)),
        )
        .unwrap();
    Arc::new(builder.build())
}

fn install(dir: &Path) -> IdentifiedInstall {
    let store = StoreId::new("steam").unwrap();
    let runtime = RuntimeIdentity {
        game: GameId::new("test-game").unwrap(),
        store: store.clone(),
        version: "1.0.0".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:12345").unwrap(),
        discovered: DiscoveredInstall {
            store,
            product: "12345".into(),
            name: "Test Game".into(),
            kind: InstallKind::BaseGame,
            parent_product: None,
            location: dir.to_path_buf(),
            store_version: Some(runtime.version.clone()),
            store_build: None,
            executables: vec![exe("Game")],
            capabilities: InstallCapabilities {
                executables_readable: true,
                accepts_new_files: true,
                relocatable: true,
            },
            volume,
        },
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

fn add_item(ctx: &CoreContext, name: &str, body: &[u8]) -> String {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("mod.txt"), body).unwrap();
    agora_core::content_store::add_folder(ctx, dir.path(), Some(name))
        .unwrap()
        .item()
        .item_id
        .clone()
}

/// The tests share one player profile (the environment is process-wide), so they run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _serial: std::sync::MutexGuard<'static, ()>,
    _tmp: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    record: GameInstanceRecord,
}

/// The player's own profile for every test in this file: a private root with the one file the
/// definition swaps. Set once, because the environment is shared by the tests' threads.
fn player_profile() {
    static ROOT: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| {
        let root = tempfile::Builder::new()
            .prefix(&format!("agora-ledger-user-{}-", std::process::id()))
            .tempdir()
            .unwrap();
        let file = root
            .path()
            .join("local")
            .join("TestGame")
            .join("config.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"the player's original").unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", root.path());
        root
    });
    let _ = root;
}

fn fixture(mods: usize) -> Fixture {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    player_profile();
    let tmp = tempfile::Builder::new()
        .prefix(&format!("agora-ledger-{}-", std::process::id()))
        .tempdir()
        .unwrap();
    let def = definition();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def));
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    // The stand-in game: a copy of the crasher named like the recipe's executable.
    std::fs::copy(
        env!("CARGO_BIN_EXE_agora-session-crasher"),
        install_dir.join(exe("Game")),
    )
    .unwrap();
    let record = create(
        &ctx,
        &install(&install_dir),
        &def,
        "Ledger",
        None,
        BaseMode::Linked,
        &|_| {},
    )
    .expect("an instance");
    for index in 0..mods {
        let id = add_item(
            &ctx,
            &format!("Mod {index}"),
            format!("mod {index}").as_bytes(),
        );
        add_content(&ctx, &record.instance_id, &id, None, None).unwrap();
    }
    Fixture {
        _serial: serial,
        _tmp: tmp,
        ctx,
        def,
        record,
    }
}

/// Starts the stand-in game from the folder the launch resolved. `args` say how it behaves.
struct CrasherLauncher {
    args: Vec<String>,
}

impl Launcher for CrasherLauncher {
    fn locate_vfs_dll(&self) -> Result<PathBuf, String> {
        Err("no virtual file system in this test".into())
    }

    fn launch(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        // The game as the launch resolved it: inside the deployed folder, as a real one runs.
        let program = prepared.resolved.program.clone();
        let mut command = std::process::Command::new(&program);
        command
            .args(&self.args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let (child, job) = agora_core::process_job::spawn_in_job(&mut command)?;
        let mut identity = agora_core::process_identity::capture(child.id())
            .map_err(|e| LaunchError::ProcessCapture(e.to_string()))?;
        identity.job = job;
        Ok(LaunchedGame {
            child,
            identity,
            program,
            output: None,
        })
    }
}

fn launch(f: &Fixture, launcher: &CrasherLauncher) -> LaunchedInstance {
    game_instance::launch_with(
        &f.ctx,
        &f.record.instance_id,
        &f.def,
        LaunchOptions {
            launch_anyway: false,
            plain: false,
            deployment: Some(DeployMode::Links),
            on_vfs_failure: VfsFailure::Ask,
            before_launch: Default::default(),
        },
        &DiscoveryReport::default,
        launcher,
    )
    .expect("the launch succeeds")
}

fn ledger(f: &Fixture) -> rusqlite::Connection {
    agora_core::db::local_state_connection(&f.ctx.paths.local_state_db()).unwrap()
}

fn exit_now() -> CrasherLauncher {
    CrasherLauncher {
        args: vec!["exit".into(), "0".into()],
    }
}

fn hold(file: &Path, then: &[&str]) -> CrasherLauncher {
    let mut args = vec!["--hold-until".to_string(), file.display().to_string()];
    args.extend(then.iter().map(|a| a.to_string()));
    CrasherLauncher { args }
}

// -- what a launch records ---------------------------------------------------------------------

#[test]
fn a_launch_records_what_it_ran_with() {
    let f = fixture(3);
    let mut launched = launch(&f, &exit_now());
    let id = launched.session_id.clone().expect("a session was opened");
    let _ = launched.launched.child.wait();

    let record = session_ledger::get(&ledger(&f), &id).unwrap().unwrap();
    assert_eq!(record.instance_id, f.record.instance_id);
    assert!(record.ended_at.is_none() && record.verdict.is_none());
    let manifest = record.manifest.expect("a manifest");
    assert_eq!(manifest.items.len(), 3);
    assert!(manifest
        .items
        .iter()
        .all(|item| item.enabled && item.hash.is_some()));
    assert_eq!(
        manifest.items.iter().map(|i| i.order).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(manifest.runtime, "steam:1.0.0");
    assert!(
        manifest.plan_fingerprint.is_some(),
        "the deployment's fingerprint, read from its record"
    );
    assert!(manifest.load_orders_hash.is_some() && manifest.settings_hash.is_some());
    assert!(manifest.unreadable.is_empty(), "{:?}", manifest.unreadable);
}

#[test]
fn an_empty_mod_list_launches_and_records() {
    let f = fixture(0);
    let launched = launch(&f, &exit_now());
    let id = launched.session_id.clone().expect("a session was opened");
    let manifest = session_ledger::get(&ledger(&f), &id)
        .unwrap()
        .unwrap()
        .manifest
        .unwrap();
    assert!(manifest.items.is_empty());
}

#[test]
fn an_unreadable_item_is_recorded_as_unreadable_not_dropped() {
    let f = fixture(3);
    // Damage the middle item's record after it was added.
    let manifest = game_instance::get_manifest(&f.ctx, &f.record.instance_id).unwrap();
    let damaged = manifest
        .layers
        .layers()
        .iter()
        .filter_map(|l| match &l.source {
            agora_game_api::LayerSource::Content { content } => Some(content.clone()),
            _ => None,
        })
        .nth(1)
        .unwrap();
    std::fs::write(f.ctx.paths.content_item_path(&damaged), b"{ not json").unwrap();

    let built = session_ledger::build_manifest(
        &f.ctx,
        &f.record.instance_id,
        &f.def,
        &StoreId::new("steam").unwrap(),
    );
    assert_eq!(built.items.len(), 3, "the damaged item is still listed");
    let bad: Vec<_> = built
        .items
        .iter()
        .filter(|i| i.unreadable.is_some())
        .collect();
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].hash.as_deref(), Some(damaged.as_str()));
    assert_eq!(
        built
            .items
            .iter()
            .filter(|i| i.unreadable.is_none())
            .count(),
        2
    );
}

#[test]
fn a_ledger_that_cannot_write_never_fails_the_launch() {
    let f = fixture(2);
    // Break the ledger's storage, and nothing else.
    ledger(&f)
        .execute_batch("DROP TABLE launch_manifests;")
        .unwrap();
    let launched = launch(&f, &exit_now());
    assert!(launched.session_id.is_none(), "nothing was recorded");
    // The launch itself carried on: the instance shows as launched.
    let (_, started): (i64, Option<String>) = ledger(&f)
        .query_row(
            "SELECT 1, last_launched_at FROM game_instances WHERE instance_id = ?1",
            [&f.record.instance_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(started.is_some());
}

// -- how a session ends -----------------------------------------------------------------------

#[test]
fn a_watched_normal_quit_is_recorded_as_normal() {
    let f = fixture(1);
    let mut launched = launch(&f, &exit_now());
    let report = game_instance::watch_session(
        &f.ctx,
        &f.def,
        launched.session_id.as_deref(),
        &launched.running_from,
        &mut launched.launched,
        Duration::from_millis(20),
        Duration::from_millis(200),
    );
    assert!(matches!(report.exit, ExitObservation::Code { code: 0, .. }));
    let record = session_ledger::get(&ledger(&f), launched.session_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.verdict, Some(SessionVerdict::Normal));
    assert!(record.ended_at.is_some());
}

#[test]
fn a_stop_during_the_run_is_recorded_as_stopped() {
    let f = fixture(1);
    let gate = f._tmp.path().join("never");
    let ready = f._tmp.path().join("ready");
    // The game announces that it runs, so Stop does not race its start-up.
    let mut launcher = hold(&gate, &["exit", "0"]);
    launcher
        .args
        .splice(0..0, ["--ready".to_string(), ready.display().to_string()]);
    let mut launched = launch(&f, &launcher);
    let session = launched.session_id.clone().unwrap();
    let running_from = launched.running_from.clone();
    let ctx = f.ctx.clone();
    let def = f.def.clone();
    let watcher = std::thread::spawn({
        let session = session.clone();
        move || {
            game_instance::watch_session(
                &ctx,
                &def,
                Some(&session),
                &running_from,
                &mut launched.launched,
                Duration::from_millis(20),
                Duration::from_millis(200),
            )
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the game never started"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(300));

    game_instance::stop(&f.ctx, &f.record.instance_id, &f.def).expect("the stop works");
    let report = watcher.join().unwrap();

    let record = session_ledger::get(&ledger(&f), &session).unwrap().unwrap();
    assert!(
        record.stop_requested,
        "the Stop was recorded on the session"
    );
    assert_eq!(record.verdict, Some(SessionVerdict::Stopped));
    // Where the platform can tell, the code is Agora's own and not a crash's 1.
    if let ExitObservation::Code { code, .. } = report.exit {
        assert_eq!(code, agora_core::process_job::STOP_EXIT_CODE);
    }
}

// -- live crash diagnosis ------------------------------------------------------------------

fn live_pack() -> agora_game_api::crash::BuiltInPack {
    agora_game_api::crash::BuiltInPack {
            file: "test/crash/pack.json".to_string(),
            json: serde_json::json!({
                "contract": 1,
                "games": ["test-game"],
                "sources": [{
                    "id": "crash_log",
                    "where": { "root": "instance", "path": "logs" },
                    "match": { "glob": "crash-*.txt" },
                    "format": "probe",
                    "bind": [{ "time": { "before_s": 3600, "after_s": 3600 } }],
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
                                "pattern": "^(?P<category>\\w+) (?P<module>\\S+)$",
                                "attrs": { "category": "category" },
                                "refs": [{ "kind": "module", "value": "module", "strength": "exec" }]
                            }]
                        }]
                    } } }
                }],
                "rules": [{
                    "id": "something_faulted",
                    "version": 1,
                    "when": { "fact": { "kind": "core.exception", "ref": { "kind": "module" } } },
                    "effects": [{ "context": { "text": { "en": "The log names a module at the fault." } } }],
                    "text": { "headline": { "en": "The log names a module at the fault." } }
                }]
            })
            .to_string(),
            parsers: Vec::new(),
        }
}

fn write_log(f: &Fixture) {
    let dir = f.ctx.paths.instance_dir(&f.record.instance_id).unwrap();
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("crash-1.txt"),
        "ERRORS\naccess_violation Example.dll\n",
    )
    .unwrap();
}

fn manifest(items: &[&str]) -> agora_core::session_ledger::LaunchManifest {
    agora_core::session_ledger::LaunchManifest {
        plan_fingerprint: None,
        items: items
            .iter()
            .enumerate()
            .map(|(order, key)| agora_core::session_ledger::ManifestItem {
                key: (*key).to_string(),
                version: Some("1.0".to_string()),
                hash: Some(format!("hash-{key}")),
                enabled: true,
                order: order as u32,
                unreadable: None,
            })
            .collect(),
        load_orders_hash: Some("load".to_string()),
        runtime: "test:1.0".to_string(),
        settings_hash: Some("settings".to_string()),
        unreadable: Vec::new(),
    }
}

fn start_session(f: &Fixture, started: &str, items: &[&str]) -> String {
    session_ledger::begin(
        &ledger(f),
        &f.record.instance_id,
        started,
        &manifest(items),
        session_ledger::Owner { pid: 1, started: 7 },
    )
    .unwrap()
}

fn end_session(f: &Fixture, id: &str, ended: &str, code: u32) {
    session_ledger::finish(
        &ledger(f),
        id,
        &session_ledger::SessionFinish {
            ended_at: ended.to_string(),
            duration_ms: 60_000,
            exit: session_ledger::ExitObservation::Code {
                pid: 4242,
                image: "Game.exe".to_string(),
                code,
            },
            game_runtime_ms: Some(60_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        },
    )
    .unwrap();
}

fn now_minus(seconds: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339()
}

fn diagnose(f: &Fixture, session: &str) -> Option<agora_core::crash_live::SessionAnswer> {
    let host = agora_plugin_host::QuickJsHost::new();
    let packs: Vec<agora_core::crash_packs::LoadedCrashPack> = Vec::new();
    let built_in = [live_pack()];
    agora_core::crash_live::diagnose_session(
        &f.ctx,
        session,
        agora_core::crash_live::LivePacks {
            packs: &packs,
            built_in: &built_in,
            host: &host,
        },
    )
}

fn stored(f: &Fixture, session: &str) -> agora_core::crash_live::SessionAnswer {
    agora_core::crash_live::answer_for_session(&ledger(f), session)
        .unwrap()
        .expect("an answer is stored")
}

fn headlines(answer: &agora_core::crash_live::StoredAnswer) -> Vec<String> {
    match answer {
        agora_core::crash_live::StoredAnswer::Analyzed { answer, .. } => answer
            .findings
            .iter()
            .filter_map(|finding| finding.headline.get("en").cloned())
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn a_crash_with_logs_is_analyzed_and_its_answer_is_stored() {
    let f = fixture(1);
    write_log(&f);
    let started = now_minus(30);
    let id = start_session(&f, &started, &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    let answer = diagnose(&f, &id).expect("the crash is diagnosed");
    assert!(matches!(
        answer.answer,
        agora_core::crash_live::StoredAnswer::Analyzed { .. }
    ));
    let stored = stored(&f, &id);
    assert!(
        headlines(&stored.answer)
            .iter()
            .any(|line| line.contains("The log names a module")),
        "{:?}",
        stored.answer
    );
}

#[test]
fn a_crash_with_no_logs_says_so_and_stores_that() {
    let f = fixture(1);
    let id = start_session(&f, &now_minus(30), &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    let answer = diagnose(&f, &id).expect("the crash is recorded");
    assert!(matches!(
        answer.answer,
        agora_core::crash_live::StoredAnswer::NothingToRead { .. }
    ));
    match stored(&f, &id).answer {
        agora_core::crash_live::StoredAnswer::NothingToRead { why } => {
            assert!(why.contains("No crash log"), "{why}");
        }
        other => panic!("expected nothing-to-read, got {other:?}"),
    }
}

#[cfg(windows)]
#[test]
fn a_log_locked_before_the_copy_is_reported_as_unreadable_not_as_no_log() {
    use std::os::windows::fs::OpenOptionsExt;

    let f = fixture(1);
    write_log(&f);
    let log = f
        .ctx
        .paths
        .instance_dir(&f.record.instance_id)
        .unwrap()
        .join("logs")
        .join("crash-1.txt");
    let _lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&log)
        .unwrap();
    let id = start_session(&f, &now_minus(30), &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    let answer = diagnose(&f, &id).expect("the crash is recorded");
    match answer.answer {
        agora_core::crash_live::StoredAnswer::NothingToRead { why } => {
            assert!(why.contains("could not be copied"), "{why}");
        }
        other => panic!("expected nothing-to-read, got {other:?}"),
    }
}

#[test]
fn a_normal_exit_stores_no_answer_at_all() {
    let f = fixture(1);
    let id = start_session(&f, &now_minus(30), &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0);

    assert!(diagnose(&f, &id).is_none());
    assert!(agora_core::crash_live::answer_for_session(&ledger(&f), &id)
        .unwrap()
        .is_none());
}

#[test]
fn two_crashes_back_to_back_each_get_their_own_answer() {
    let f = fixture(1);
    write_log(&f);
    let first = start_session(&f, &now_minus(120), &["mod-a"]);
    end_session(&f, &first, &now_minus(90), 0xC000_0005);
    let second = start_session(&f, &now_minus(60), &["mod-a"]);
    end_session(&f, &second, &now_minus(30), 0xC000_0005);

    diagnose(&f, &first).expect("the first crash is diagnosed");
    diagnose(&f, &second).expect("the second crash is diagnosed");

    let first = stored(&f, &first);
    let second = stored(&f, &second);
    assert_ne!(first.session_id, second.session_id);
    assert!(!firstheads_empty(&first));
    assert!(!firstheads_empty(&second));
}

fn firstheads_empty(answer: &agora_core::crash_live::SessionAnswer) -> bool {
    match &answer.answer {
        agora_core::crash_live::StoredAnswer::Analyzed { answer, .. } => answer.findings.is_empty(),
        _ => true,
    }
}

#[test]
fn with_no_previous_good_session_agora_invents_no_change() {
    let f = fixture(1);
    write_log(&f);
    let id = start_session(&f, &now_minus(30), &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    diagnose(&f, &id).expect("the crash is diagnosed");
    let lines = headlines(&stored(&f, &id).answer);
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("This started after")),
        "{lines:?}"
    );
}

#[test]
fn a_change_since_the_last_good_session_is_listed_when_nobody_is_named() {
    let f = fixture(1);
    write_log(&f);
    let good = start_session(&f, &now_minus(180), &["mod-a"]);
    end_session(&f, &good, &now_minus(120), 0);
    let id = start_session(&f, &now_minus(60), &["mod-a", "mod-b"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    diagnose(&f, &id).expect("the crash is diagnosed");
    let lines = headlines(&stored(&f, &id).answer);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("This started after you changed") && line.contains("mod-b")),
        "{lines:?}"
    );
}

#[test]
fn an_analysis_that_cannot_run_is_stored_as_could_not_analyze() {
    let f = fixture(1);
    // A session whose instance has no manifest: the analysis cannot even read the instance.
    let id = session_ledger::begin(
        &ledger(&f),
        "missing-instance",
        &now_minus(30),
        &manifest(&["mod-a"]),
        session_ledger::Owner { pid: 1, started: 7 },
    )
    .unwrap();
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    let answer = diagnose(&f, &id).expect("the failure is recorded");
    match answer.answer {
        agora_core::crash_live::StoredAnswer::CouldNotAnalyze { why } => {
            assert!(why.contains("instance"), "{why}");
        }
        other => panic!("expected could-not-analyze, got {other:?}"),
    }
    // The crash still exists as a recorded session, independent of the analysis.
    assert_eq!(
        session_ledger::get(&ledger(&f), &id)
            .unwrap()
            .unwrap()
            .verdict,
        Some(SessionVerdict::Crashed {
            how: agora_core::session_ledger::MatchedBy::ExitStatus
        })
    );
}

#[test]
fn a_session_left_open_when_agora_closed_ends_unknown_at_the_next_launch() {
    let f = fixture(1);
    let first = launch(&f, &exit_now());
    let first_id = first.session_id.clone().unwrap();
    // The Agora that owned it is gone: nothing will ever finish this row.
    ledger(&f)
        .execute(
            "UPDATE launch_history SET owner_pid = ?2 WHERE session_id = ?1",
            rusqlite::params![first_id, u32::MAX - 1],
        )
        .unwrap();

    let _second = launch(&f, &exit_now());
    let record = session_ledger::get(&ledger(&f), &first_id)
        .unwrap()
        .unwrap();
    assert_eq!(record.verdict, Some(SessionVerdict::ClosedUnknown));
    assert!(matches!(
        record.exit,
        Some(ExitObservation::Unavailable { .. })
    ));
    assert_ne!(record.verdict, Some(SessionVerdict::Normal));
}
