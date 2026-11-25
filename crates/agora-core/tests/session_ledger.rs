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
        snapshot: None,
        mo2_game_name: None,
        import_ids: Default::default(),
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
        ini_checks: Vec::new(),
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
fn a_manifest_records_ini_keys_so_a_settings_change_names_one() {
    use agora_core::session_ledger::ChangeKind;

    let f = fixture(0);
    let store = StoreId::new("steam").unwrap();
    let settings = f
        .ctx
        .paths
        .instance_dir(&f.record.instance_id)
        .unwrap()
        .join("user")
        .join("config.txt");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "[Display]\nuGridsToLoad=11\n").unwrap();
    let before = session_ledger::build_manifest(&f.ctx, &f.record.instance_id, &f.def, &store);
    assert_eq!(before.settings.len(), 1, "{:?}", before.settings);
    assert_eq!(before.settings[0].key, "uGridsToLoad");

    std::fs::write(&settings, "[Display]\nuGridsToLoad=5\n").unwrap();
    let after = session_ledger::build_manifest(&f.ctx, &f.record.instance_id, &f.def, &store);
    let changes = session_ledger::diff_manifests(&before, &after);
    let named: Vec<_> = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Settings)
        .collect();
    assert_eq!(named.len(), 1, "{changes:#?}");
    let setting = named[0]
        .setting
        .as_ref()
        .expect("the changed key is named, not just the file");
    assert_eq!(setting.file, "user/config.txt");
    assert_eq!(setting.section, "Display");
    assert_eq!(setting.key, "uGridsToLoad");
    assert_eq!(setting.from.as_deref(), Some("11"));
    assert_eq!(setting.to.as_deref(), Some("5"));
    assert_eq!(
        agora_core::crash_engine::change::describe(named[0]),
        "changed uGridsToLoad in config.txt from 11 to 5"
    );
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
            "SELECT 1, last_launched_at FROM instances WHERE instance_id = ?1",
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
        &f.record.instance_id,
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
    let instance_id = f.record.instance_id.clone();
    let watcher = std::thread::spawn({
        let session = session.clone();
        move || {
            game_instance::watch_session(
                &ctx,
                &instance_id,
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

/// A pack whose supplementary log names a load the game refused. The fact is core's own
/// `core.load_verdict`, so the decision to answer a clean session is game-free.
fn refusal_pack() -> agora_game_api::crash::BuiltInPack {
    agora_game_api::crash::BuiltInPack {
        file: "test/crash/refusal-pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "games": ["test-game"],
            "sources": [{
                "id": "startup_log",
                "where": { "root": "instance", "path": "logs" },
                "match": { "glob": "startup-*.txt" },
                "format": "probe",
                "bind": [{ "time": { "before_s": 3600, "after_s": 3600 } }],
                "role": "supplementary"
            }],
            "parsers": [{
                "id": "probe",
                "detect": { "all": ["(?m)^REFUSED$"] },
                "entry": { "generic": { "name": "text_sections", "config": {
                    "producer": { "format": "probe_log" },
                    "sections": [{
                        "id": "load",
                        "header": "^REFUSED$",
                        "kind": "core.load_verdict",
                        "lines": [{
                            "pattern": "^(?P<outcome>refused) (?P<module>\\S+) (?P<reason>\\S+)$",
                            "attrs": { "outcome": "outcome", "reason": "reason" },
                            "refs": [{ "kind": "module", "value": "module", "strength": "mention" }]
                        }]
                    }]
                } } }
            }],
            "rules": [{
                "id": "refused_plugin",
                "version": 1,
                "when": { "fact": { "kind": "core.load_verdict", "where": { "outcome": "refused" }, "ref": { "kind": "module", "bind": "d" } } },
                "effects": [{ "finding": { "cause": "version_mismatch", "scope": "mechanism", "level": "possible" } }],
                "text": { "headline": { "en": "The game refused to load {d} because it is built for another game version. Update it or turn it off." } }
            }]
        })
        .to_string(),
        parsers: Vec::new(),
    }
}

fn write_startup_log(f: &Fixture) {
    let logs = f
        .ctx
        .paths
        .instance_dir(&f.record.instance_id)
        .unwrap()
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("startup-1.txt"),
        "REFUSED\nrefused ExampleStartup.dll different_game_version\n",
    )
    .unwrap();
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
        load_order: Vec::new(),
        runtime: "test:1.0".to_string(),
        settings_hash: Some("settings".to_string()),
        settings: Vec::new(),
        unreadable: Vec::new(),
        owner_layers: Vec::new(),
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
    end_session_after(f, id, ended, code, 60_000);
}

fn end_session_after(f: &Fixture, id: &str, ended: &str, code: u32, runtime_ms: u64) {
    session_ledger::finish(
        &ledger(f),
        id,
        &session_ledger::SessionFinish {
            ended_at: ended.to_string(),
            duration_ms: runtime_ms as i64,
            exit: session_ledger::ExitObservation::Code {
                pid: 4242,
                image: "Game.exe".to_string(),
                code,
            },
            game_runtime_ms: Some(runtime_ms),
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
    let built_in = [live_pack()];
    diagnose_with(f, session, &built_in)
}

fn diagnose_with(
    f: &Fixture,
    session: &str,
    built_in: &[agora_game_api::crash::BuiltInPack],
) -> Option<agora_core::crash_live::SessionAnswer> {
    let host = agora_plugin_host::QuickJsHost::new();
    let packs: Vec<agora_core::crash_packs::LoadedCrashPack> = Vec::new();
    agora_core::crash_live::diagnose_session(
        &f.ctx,
        session,
        agora_core::crash_live::LivePacks {
            packs: &packs,
            built_in,
            host: &host,
            instances: None,
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
fn a_long_nonzero_session_with_a_crash_log_is_crashed_and_answered() {
    let f = fixture(1);
    write_log(&f);
    let id = start_session(&f, &now_minus(2_400), &["mod-a"]);
    // 38 minutes and exit code 1: the ledger cannot call this a crash on the code alone, so the
    // verdict starts as ClosedUnknown. A log inside the window still has to become the answer.
    end_session_after(&f, &id, &now_minus(1), 1, 38 * 60 * 1_000);
    assert_eq!(
        session_ledger::get(&ledger(&f), &id)
            .unwrap()
            .unwrap()
            .verdict,
        Some(SessionVerdict::ClosedUnknown),
        "the race this reproduces: no artifact was known when the verdict was written"
    );

    let answer = diagnose(&f, &id).expect("the crash log inside the window is an answer");
    assert!(
        matches!(
            answer.answer,
            agora_core::crash_live::StoredAnswer::Analyzed { .. }
        ),
        "{:?}",
        answer.answer
    );
    assert!(
        !headlines(&stored(&f, &id).answer).is_empty(),
        "the log's finding reaches the stored answer"
    );
    assert!(
        matches!(
            session_ledger::get(&ledger(&f), &id)
                .unwrap()
                .unwrap()
                .verdict,
            Some(SessionVerdict::Crashed { .. })
        ),
        "a bound crash log corrects the verdict whatever the exit code said"
    );
}

#[test]
fn a_long_nonzero_session_with_no_crash_log_stores_a_reason() {
    let f = fixture(1);
    let id = start_session(&f, &now_minus(2_400), &["mod-a"]);
    end_session_after(&f, &id, &now_minus(1), 1, 38 * 60 * 1_000);

    let answer = diagnose(&f, &id).expect("an abnormal ending never gets a blank row");
    match answer.answer {
        agora_core::crash_live::StoredAnswer::CouldNotAnalyze { why } => {
            assert!(why.contains("exit code 1"), "{why}");
            assert!(why.contains("no crash log"), "{why}");
        }
        other => panic!("expected a stored reason, got {other:?}"),
    }
    assert_eq!(
        session_ledger::get(&ledger(&f), &id)
            .unwrap()
            .unwrap()
            .verdict,
        Some(SessionVerdict::ClosedUnknown),
        "no evidence says it was a crash, so the verdict is not invented"
    );
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
fn a_clean_session_that_refused_a_plugin_gets_a_normal_answer() {
    let f = fixture(1);
    write_startup_log(&f);
    let id = start_session(&f, &now_minus(120), &["mod-a"]);
    end_session_after(&f, &id, &now_minus(1), 0, 100_000);

    let built_in = [refusal_pack()];
    let answer = diagnose_with(&f, &id, &built_in).expect("a refused plugin is worth an answer");
    let agora_core::crash_live::StoredAnswer::Analyzed { answer, .. } = answer.answer else {
        panic!("expected an analyzed answer, got a different one");
    };
    assert_eq!(
        answer.verdict,
        SessionVerdict::Normal,
        "the process exited normally; the answer explains a refusal, not a crash"
    );
    assert!(answer.candidates.is_empty(), "nobody is accused of a crash");
    let headline = answer.headline.get("en").cloned().unwrap_or_default();
    assert!(headline.contains("refused to load"), "{headline}");
    assert!(headline.contains("ExampleStartup.dll"), "{headline}");
    assert!(
        headline.contains("Update it or turn it off"),
        "the answer says what to do: {headline}"
    );
    assert_eq!(
        session_ledger::get(&ledger(&f), &id)
            .unwrap()
            .unwrap()
            .verdict,
        Some(SessionVerdict::Normal),
        "a refusal at startup does not rewrite a clean exit as a crash"
    );
}

#[test]
fn an_ordinary_clean_session_with_no_refusal_gets_no_answer() {
    let f = fixture(1);
    let id = start_session(&f, &now_minus(120), &["mod-a"]);
    end_session_after(&f, &id, &now_minus(1), 0, 100_000);

    let built_in = [refusal_pack()];
    assert!(diagnose_with(&f, &id, &built_in).is_none());
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
    // Each crash writes its own log. The first one is already that session's evidence and may not
    // be reused as the second's, so this crash leaves a fresh file.
    write_named_log(&f, "crash-2.txt", "2000");
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

fn write_named_log(f: &Fixture, name: &str, rva: &str) {
    let logs = f
        .ctx
        .paths
        .instance_dir(&f.record.instance_id)
        .unwrap()
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join(name),
        format!("ERRORS\naccess_violation Example.dll\n[rva {rva}]\n"),
    )
    .unwrap();
}

#[test]
fn a_live_diagnosed_crash_is_not_counted_again_when_its_log_is_swept() {
    let f = fixture(1);
    write_named_log(&f, "crash-1.txt", "1000");
    let first = start_session(&f, &now_minus(180), &["mod-a"]);
    end_session(&f, &first, &now_minus(150), 0xC000_0005);
    diagnose(&f, &first).expect("the first crash is diagnosed");

    write_named_log(&f, "crash-2.txt", "2000");
    let second = start_session(&f, &now_minus(60), &["mod-a"]);
    end_session(&f, &second, &now_minus(30), 0xC000_0005);
    diagnose(&f, &second).expect("the second crash is diagnosed");

    let prior =
        agora_core::crash_history::load_recorded(&ledger(&f), &f.record.instance_id, &[], None)
            .unwrap();
    assert_eq!(
        prior.len(),
        2,
        "there were two crashes, but the first was stored once for its session and again \
         for its swept log: {prior:#?}"
    );
}

#[test]
fn a_log_over_the_copy_limit_is_not_counted_as_its_own_history() {
    let f = fixture(1);
    let log_name = "crash-big.txt";
    let logs = f
        .ctx
        .paths
        .instance_dir(&f.record.instance_id)
        .unwrap()
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let mut bytes = b"ERRORS\n[0] Example.dll+0x1000\n".to_vec();
    while bytes.len() < 9 * 1024 * 1024 {
        bytes.extend_from_slice(b"padding line for a very large invented log\n");
    }
    std::fs::write(logs.join(log_name), &bytes).unwrap();

    let id = start_session(&f, &now_minus(30), &["mod-a"]);
    end_session(&f, &id, &now_minus(1), 0xC000_0005);

    // This pack gives the log a crash place and says so when the crash was counted in its own
    // earlier history.
    let built_in = [agora_game_api::crash::BuiltInPack {
        file: "test/self-history/pack.json".to_string(),
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
                        "lines": [{
                            "pattern": "^\\[(?P<index>\\d+)\\] (?P<mod>[A-Za-z0-9_.-]+)\\+0x(?P<rva>[0-9a-fA-F]+)$",
                            "kind": "core.frame",
                            "attrs": { "index": "index", "rva": "rva" },
                            "refs": [{ "kind": "module", "value": "mod", "strength": "exec" }]
                        }]
                    }]
                } } }
            }],
            "rules": [{
                "id": "counted_in_own_history",
                "version": 1,
                "when": { "fact": { "kind": "core.history", "where": { "same_bucket_count": { "gte": 1 } } } },
                "effects": [{ "context": { "text": { "en": "the crash was counted in its own history" } } }],
                "text": { "headline": { "en": "A frame was seen." } }
            }]
        })
        .to_string(),
        parsers: Vec::new(),
    }];
    let host = agora_plugin_host::QuickJsHost::new();
    let empty_packs: Vec<agora_core::crash_packs::LoadedCrashPack> = Vec::new();
    let answer = agora_core::crash_live::diagnose_session(
        &f.ctx,
        &id,
        agora_core::crash_live::LivePacks {
            packs: &empty_packs,
            built_in: &built_in,
            host: &host,
            instances: None,
        },
    )
    .expect("the crash is diagnosed");

    let agora_core::crash_live::StoredAnswer::Analyzed { answer, .. } = answer.answer else {
        panic!("expected an analyzed answer, got {answer:?}");
    };
    assert!(
        answer.findings.iter().all(|finding| {
            finding.headline.get("en").map(String::as_str)
                != Some("the crash was counted in its own history")
        }),
        "a log over the copy limit was hashed from its head-and-tail copy, so the whole file \
         the sweep read was not recognised as this crash and the crash boosted itself: {:#?}",
        answer.findings
    );
}
