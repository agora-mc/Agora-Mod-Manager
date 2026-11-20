//! The one startup recovery pass (`agora_core::startup_recovery::run_at_startup`, MASTER_SPEC
//! §32.11): every adapter runs it once at startup, it recovers every crash journal, it never
//! touches a journal a live operation in another process still owns, and it reports — never
//! raises — a journal that needs a hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::{self, GameInstanceManifest};
use agora_core::game_registry::{GameRegistry, IdentifiedInstall, RuntimeResolution};
use agora_core::game_tool_copy::CopyKind;
use agora_core::game_tool_swap::{self as swap, SwapJournal, VolumeSeam};
use agora_core::lock_manager::{LockManager, LockResource};
use agora_core::startup_recovery::{run_at_startup, StartupRecoveryReport};
use agora_game_api::{
    BaseReference, DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId,
    InstallKind, LaunchRecipe, LaunchValue, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier, ToolDefinition, ToolId, ToolOutput, ToolScope, UserDataLocation,
    UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

const GAME: &str = "test-game";

// The user-files root is read from a process-wide env var, so the tests that set it serialize.
static USER_FILES_LOCK: Mutex<()> = Mutex::new(());

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

/// A registry with one game and one store, plus a recording services hook.
fn registry(def: &GameDefinition, hook: Arc<RecordingServices>) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    let pkg = PackageDefinition {
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
        .add_compiled("test-pkg", Arc::new(TestPackage(pkg)), hook)
        .expect("register test package");
    Arc::new(builder.build())
}

#[derive(Default)]
struct RecordingServices {
    lines: Mutex<Vec<String>>,
}

impl agora_core::game_hooks::CompiledServices for RecordingServices {
    fn recover_at_startup(&self, _ctx: &CoreContext) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

fn definition(user_files: bool) -> GameDefinition {
    let mut mappings = Vec::new();
    if user_files {
        mappings.push(
            UserFileMapping::new(
                GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Test Game/Plugins.txt").unwrap(),
                },
                RelPath::new("user/Plugins.txt").unwrap(),
                UserFileStrategy::JournaledSwap,
            )
            .with_stores(vec![StoreId::steam()]),
        );
    }
    GameDefinition {
        mo2_game_name: None,
        import_ids: Default::default(),
        session: Default::default(),
        id: GameId::new(GAME).unwrap(),
        extends: None,
        name: "Test Game".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::steam(),
            product: "12345".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![ToolId::new("nemesis").unwrap()],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![LaunchValue::Literal { value: "/c".into() }],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        user_files: mappings,
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        load_orders: vec![],
        runtime_files: vec![],
        save_location: Vec::new(),
        ini_checks: vec![],
        launch_alternatives: vec![],
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        output: ToolOutput::GeneratedLayer,
        output_arg: None,
        output_patterns: Vec::new(),
        id: ToolId::new("nemesis").unwrap(),
        game: GameId::new(GAME).unwrap(),
        name: "Nemesis".into(),
        launch: LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        },
        relevant_settings: Vec::new(),
        after_tools: Vec::new(),
        conflicts_with: Vec::new(),
        uses_install_path: true,
        required_outputs: Vec::new(),
        failure_markers: Vec::new(),
        required_magic: Vec::new(),
        run_only: false,
        executable_names: Vec::new(),
        inputs: Vec::new(),
        scope: ToolScope::default(),
        input_files: Vec::new(),
        before_launch: None,
    }
}

fn install_of(dir: &Path) -> IdentifiedInstall {
    let store_id = StoreId::steam();
    let runtime = RuntimeIdentity {
        game: GameId::new(GAME).unwrap(),
        store: store_id.clone(),
        version: "1.0.0".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: store_id.clone(),
        product: "12345".into(),
        name: "Test Game".into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: None,
        executables: vec!["Game.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:12345").unwrap(),
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

struct SwapFixture {
    _tmp: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    instance_id: String,
}

/// A swap fixture that works on any OS: `Game.exe` is a copy of this test binary, which is enough
/// for the install scan (no process runs it).
fn swap_fixture() -> SwapFixture {
    let tmp = TempDir::new().unwrap();
    let def = definition(false);
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(install_dir.join("Data")).unwrap();
    std::fs::copy(
        std::env::current_exe().unwrap(),
        install_dir.join("Game.exe"),
    )
    .unwrap();
    std::fs::write(install_dir.join("Data").join("Skyrim.bsa"), b"BSA DATA").unwrap();
    std::fs::write(install_dir.join("Data").join("tweak.ini"), b"ini original").unwrap();
    std::fs::write(install_dir.join("Data").join("delete_me.txt"), b"delete me").unwrap();
    std::fs::write(install_dir.join("base_file.txt"), b"initial base file").unwrap();
    let install = install_of(&install_dir);
    let inst = game_instance::create(
        &ctx,
        &install,
        &def,
        "Tools",
        None,
        BaseMode::Linked,
        &|_| {},
    )
    .expect("create instance");
    let _ = tool_definition();
    SwapFixture {
        _tmp: tmp,
        ctx,
        def,
        instance_id: inst.instance_id,
    }
}

fn install_dir(f: &SwapFixture) -> PathBuf {
    f._tmp.path().join("install")
}

fn real_data(f: &SwapFixture) -> PathBuf {
    install_dir(f).join("Data")
}

fn journal_file(f: &SwapFixture) -> PathBuf {
    f.ctx.paths.tool_swap_journal_path(GAME, "steam")
}

/// A journal for a swap a test sets up by hand, exactly as a run writes it.
fn journal_of(f: &SwapFixture, run: &str, kind: CopyKind) -> SwapJournal {
    let loc = swap::swap_location(&f.ctx, &f.instance_id).unwrap();
    SwapJournal {
        version: swap::JOURNAL_VERSION,
        game: GAME.into(),
        store: "steam".into(),
        instance_id: f.instance_id.clone(),
        tool: "nemesis".into(),
        run: run.into(),
        started_unix_ms: 0,
        install_dir: loc.install.clone(),
        real_data: loc.install.join("Data"),
        aside: loc.install.join(format!("{}{run}", swap::ASIDE_PREFIX)),
        copy_kind: kind,
        copy_dir: agora_core::game_tool_copy::copy_dir_for(kind, &loc, run),
        swap_dir: loc.swap_dir.clone(),
        processes: Vec::new(),
    }
}

/// Simulates a crash in a kept-copy swap after `step`: 4 the real Data is renamed aside; 5 the
/// copy is renamed into its place; 6 the tool has written into it. The journal is on disk.
fn crash_after(f: &SwapFixture, step: u8, kind: CopyKind, run: &str) -> SwapJournal {
    let journal = journal_of(f, run, kind);
    swap::write_journal(&f.ctx, &journal).unwrap();
    let planned = agora_core::game_tool_copy::data_files(&f.ctx, &f.instance_id, &f.def).unwrap();
    agora_core::game_tool_copy::prepare(&f.ctx, &journal, &planned).unwrap();
    if step >= 4 {
        std::fs::rename(&journal.real_data, &journal.aside).unwrap();
    }
    if step >= 5 {
        std::fs::rename(&journal.copy_dir, &journal.real_data).unwrap();
    }
    if step >= 6 {
        let data = &journal.real_data;
        std::fs::write(data.join("tweak.ini"), b"tool wrote").unwrap();
        std::fs::write(data.join("new.txt"), b"tool new").unwrap();
        std::fs::remove_file(data.join("delete_me.txt")).unwrap();
    }
    journal
}

/// Every entry under `root` with its bytes; a missing folder is empty.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Hold the seam that says the copy is on the install's volume for the whole test.
struct SwapSeam;

impl SwapSeam {
    fn same_volume() -> Self {
        swap::set_volume_seam(Some(VolumeSeam {
            same_volume: Some(true),
            free_bytes: Some(u64::MAX),
        }));
        SwapSeam
    }
}

impl Drop for SwapSeam {
    fn drop(&mut self) {
        swap::set_volume_seam(None);
    }
}

// ---------------------------------------------------------------------------
// Tool swaps
// ---------------------------------------------------------------------------

/// The startup pass (the one every adapter now runs, desktop included) puts back an interrupted
/// tool swap: the real Data folder comes back byte for byte, the journal is gone, and the report
/// says what it did.
#[test]
fn the_startup_pass_puts_back_an_interrupted_tool_swap() {
    let _seam = SwapSeam::same_volume();
    let f = swap_fixture();
    let before = snapshot(&real_data(&f));
    crash_after(&f, 6, CopyKind::Kept, "crash");
    assert!(journal_file(&f).exists());

    let report = run_at_startup(&f.ctx);

    assert_eq!(snapshot(&real_data(&f)), before, "the real Data is back");
    assert!(!journal_file(&f).exists(), "the journal is gone");
    assert!(
        names(&install_dir(&f))
            .iter()
            .all(|n| !n.starts_with(swap::ASIDE_PREFIX) && !n.starts_with(swap::COPY_PREFIX)),
        "no aside or copy folder is left"
    );
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.contains("Put back the real Data folder")),
        "the report names the action: {report:?}"
    );
    assert!(
        report.warnings.is_empty(),
        "nothing needs a hand: {report:?}"
    );
}

/// Two processes: a swap journal whose game/store lock another Agora process holds belongs to a
/// live operation, so the startup pass leaves it completely alone and reports that it did.
#[test]
fn the_startup_pass_leaves_a_swap_alone_while_another_process_holds_its_lock() {
    let _seam = SwapSeam::same_volume();
    let f = swap_fixture();
    let before = snapshot(&real_data(&f));
    let journal = crash_after(&f, 6, CopyKind::Kept, "live");

    // A second lock manager on the same locks root is what another Agora process sees: the OS
    // lock conflicts across handles, so the startup pass cannot take it.
    let other_process = LockManager::new(f.ctx.paths.locks_root());
    let _held = other_process
        .acquire(
            LockResource::GameUserFiles(GameId::new(GAME).unwrap(), StoreId::steam()),
            "tool-swap-run",
        )
        .expect("the live operation takes the lock");

    let report = run_at_startup(&f.ctx);

    // Nothing was recovered: the journal, the aside and the swapped-in copy are all untouched.
    assert!(journal_file(&f).exists(), "the journal is still there");
    assert!(journal.aside.is_dir(), "the real Data is still aside");
    assert!(
        real_data(&f).join("new.txt").exists(),
        "the copy is still in place"
    );
    assert_ne!(snapshot(&real_data(&f)), before);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("left alone") && w.contains("lock")),
        "the skip is reported: {report:?}"
    );
    assert!(
        report.actions.is_empty(),
        "nothing was recovered: {report:?}"
    );

    // Once the live operation lets go, the next startup recovers the swap.
    drop(_held);
    let report = run_at_startup(&f.ctx);
    assert_eq!(
        snapshot(&real_data(&f)),
        before,
        "recovered after the lock was released"
    );
    assert!(!journal_file(&f).exists());
    assert!(report
        .actions
        .iter()
        .any(|a| a.contains("Put back the real Data folder")));
}

/// A journal that does not describe a swap Agora makes needs a hand: the startup pass reports it
/// and changes nothing — it does not raise, and it does not touch the folder it points at.
#[test]
fn a_swap_journal_that_needs_a_hand_is_reported_not_raised() {
    let _seam = SwapSeam::same_volume();
    let f = swap_fixture();
    let victim = f._tmp.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), b"keep").unwrap();
    let mut journal = journal_of(&f, "tamper", CopyKind::Kept);
    journal.copy_dir = victim.clone();
    swap::write_journal(&f.ctx, &journal).unwrap();
    std::fs::rename(real_data(&f), &journal.aside).unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(std::fs::read(victim.join("keep.txt")).unwrap(), b"keep");
    assert!(
        journal.aside.is_dir(),
        "the real Data is still aside, untouched"
    );
    assert!(!real_data(&f).exists());
    assert!(journal_file(&f).exists(), "the journal is kept for a human");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("was not put back") && w.contains("does not describe a swap")),
        "the needs-a-hand case is reported: {report:?}"
    );
    assert!(report.actions.is_empty());
}

// ---------------------------------------------------------------------------
// The registry hook and an empty pass
// ---------------------------------------------------------------------------

/// Game-specific recovery stays behind the game registry's hook, and the startup pass runs it.
#[test]
fn the_startup_pass_runs_the_game_registry_hook() {
    let def = definition(false);
    let hook = Arc::new(RecordingServices::default());
    hook.lines
        .lock()
        .unwrap()
        .push("recovered one import job".into());
    let tmp = TempDir::new().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def, hook));

    let report = run_at_startup(&ctx);

    assert!(
        report
            .warnings
            .iter()
            .any(|w| w == "recovered one import job"),
        "the hook's line is in the report: {report:?}"
    );
}

/// With nothing pending anywhere, the pass is silent: no actions, no warnings.
#[test]
fn a_clean_startup_pass_reports_nothing() {
    let def = definition(false);
    let tmp = TempDir::new().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));

    let report = run_at_startup(&ctx);

    assert!(report.is_empty(), "{report:?}");
    assert_eq!(report, StartupRecoveryReport::default());
}

// ---------------------------------------------------------------------------
// Recently deleted (trash)
// ---------------------------------------------------------------------------

fn trash_manifest(id: &str, name: &str) -> GameInstanceManifest {
    let runtime = RuntimeIdentity {
        game: GameId::new("skyrim-se").unwrap(),
        store: StoreId::steam(),
        version: "1.0.0".into(),
        build: None,
    };
    GameInstanceManifest::new(
        runtime.game.clone(),
        id,
        name,
        Some(runtime.clone()),
        BaseReference::Pinned {
            id: "base-1".into(),
            runtime,
            mode: BaseMode::Linked,
        },
    )
}

fn write_instance(ctx: &CoreContext, manifest: &GameInstanceManifest) -> PathBuf {
    let dir = ctx
        .paths
        .instance_dir(&manifest.instance_id)
        .expect("an instance dir");
    std::fs::create_dir_all(&dir).expect("the instance folder");
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, total_play_ms)
         VALUES (?1, ?2, ?3, ?4, '2026-11-06T00:00:00Z', 0)",
        rusqlite::params![
            manifest.instance_id,
            manifest.game.as_str(),
            manifest.name,
            serde_json::to_string(&manifest.base).unwrap(),
        ],
    )
    .unwrap();
    dir
}

/// The startup pass finishes an interrupted Recently deleted restore: the instance row comes
/// back, the entry is tidied, and the report names the action.
#[test]
fn the_startup_pass_finishes_an_interrupted_recently_deleted_restore() {
    let tmp = TempDir::new().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let def = definition(false);
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));
    write_instance(&ctx, &trash_manifest("grounded-1", "Grounded"));
    let entry_id = game_instance::delete(&ctx, "grounded-1")
        .expect("the delete")
        .trash_entry_id
        .expect("an entry");
    // The state a crash leaves: the marker written, the folder moved, no row yet.
    let entry = ctx.paths.recently_deleted_games_root().join(&entry_id);
    std::fs::write(
        entry.join("restoring.json"),
        br#"{"new_id":"grounded-1","new_name":"Grounded"}"#,
    )
    .expect("the marker");
    std::fs::rename(
        entry.join("instance"),
        ctx.paths.instance_dir("grounded-1").expect("a dir"),
    )
    .expect("the move");
    assert!(game_instance::get(&ctx, "grounded-1").unwrap().is_none());

    let report = run_at_startup(&ctx);

    let record = game_instance::get(&ctx, "grounded-1")
        .expect("a read")
        .expect("the instance came back");
    assert_eq!(record.name, "Grounded");
    assert!(!entry.exists(), "the finished entry was tidied");
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.contains("interrupted restore of instance grounded-1")),
        "the report names the action: {report:?}"
    );
}

/// A trash entry whose instance lock another live process holds is left alone and reported.
#[test]
fn the_startup_pass_leaves_a_busy_trash_entry_alone() {
    let tmp = TempDir::new().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let def = definition(false);
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));
    write_instance(&ctx, &trash_manifest("busy-1", "Busy"));
    let entry_id = game_instance::delete(&ctx, "busy-1")
        .expect("the delete")
        .trash_entry_id
        .expect("an entry");
    let entry = ctx.paths.recently_deleted_games_root().join(&entry_id);
    std::fs::write(
        entry.join("restoring.json"),
        br#"{"new_id":"busy-1","new_name":"Busy"}"#,
    )
    .expect("the marker");
    std::fs::rename(
        entry.join("instance"),
        ctx.paths.instance_dir("busy-1").expect("a dir"),
    )
    .expect("the move");

    let other_process = LockManager::new(ctx.paths.locks_root());
    let _held = other_process
        .acquire(LockResource::Instance("busy-1".to_string()), "live-delete")
        .expect("the live operation takes the instance lock");

    let report = run_at_startup(&ctx);

    assert!(
        game_instance::get(&ctx, "busy-1").unwrap().is_none(),
        "not recovered"
    );
    assert!(
        entry.join("restoring.json").exists(),
        "the entry is untouched"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("busy-1") && w.contains("left alone")),
        "the skip is reported: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// User files
// ---------------------------------------------------------------------------

/// The startup pass puts back the user files of a session whose game is no longer running, and
/// removes the journal.
#[test]
fn the_startup_pass_restores_the_user_files_of_a_dead_session() {
    let _guard = USER_FILES_LOCK.lock().unwrap();
    let data_dir = TempDir::new().unwrap();
    let user_data_dir = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data_dir.path());

    let def = definition(true);
    let ctx = CoreContext::for_testing(data_dir.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));
    let running_from = data_dir.path().join("running_from");
    std::fs::create_dir_all(&running_from).unwrap();

    // The player's real file and the instance's copy.
    let real = user_data_dir
        .path()
        .join("local")
        .join("Test Game")
        .join("Plugins.txt");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, "Player original\n").unwrap();
    let copy = ctx
        .paths
        .instance_dir("instance-a")
        .unwrap()
        .join("user/Plugins.txt");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::write(&copy, "Instance copy\n").unwrap();

    // A launch that swapped the files in and then died: the recorded process is gone.
    agora_core::game_user_files::swap_in(
        &ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &ctx,
        &def.id,
        &StoreId::steam(),
        agora_core::process_identity::ProcessIdentity {
            pid: u32::MAX - 2,
            start_time: 5,
            expected_exe: None,
            job: None,
        },
    )
    .unwrap();
    let journal_path = ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists(), "the session is journaled");
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "Instance copy\n");

    let report = run_at_startup(&ctx);

    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "Player original\n",
        "the player's file is back"
    );
    assert!(!journal_path.exists(), "the journal is gone");
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.contains("Put back the user files")),
        "the report names the action: {report:?}"
    );

    std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
}
