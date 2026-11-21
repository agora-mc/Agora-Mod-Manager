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
        spawning: false,
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

/// Mirrors the marker the real restore writes *inside* the folder it moves, so recovery can
/// prove a destination folder came from this entry (see `trash.rs`).
fn write_restore_marker(folder: &Path, entry_id: &str) {
    std::fs::write(
        folder.join(".agora-restore-marker.json"),
        serde_json::json!({ "entry_id": entry_id }).to_string(),
    )
    .unwrap();
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
    write_restore_marker(&entry.join("instance"), &entry_id);
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
    write_restore_marker(&entry.join("instance"), &entry_id);
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

// ---------------------------------------------------------------------------
// Live and unknown sessions, and files changed after a crash
// ---------------------------------------------------------------------------

/// The user-files setup of the tests below: a registry with the one mapping, the player's real
/// file, the instance's copy, and a `running_from` folder.
struct UserFilesFixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    _data_dir: TempDir,
    _user_data_dir: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    running_from: PathBuf,
    real: PathBuf,
    copy: PathBuf,
}

fn user_files_fixture(player_file: Option<&str>) -> UserFilesFixture {
    let guard = USER_FILES_LOCK.lock().unwrap();
    let data_dir = TempDir::new().unwrap();
    let user_data_dir = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data_dir.path());

    let def = definition(true);
    let ctx = CoreContext::for_testing(data_dir.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry(&def, Arc::new(RecordingServices::default())));
    let running_from = data_dir.path().join("running_from");
    std::fs::create_dir_all(&running_from).unwrap();

    let real = user_data_dir
        .path()
        .join("local")
        .join("Test Game")
        .join("Plugins.txt");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    if let Some(bytes) = player_file {
        std::fs::write(&real, bytes).unwrap();
    }
    let copy = ctx
        .paths
        .instance_dir("instance-a")
        .unwrap()
        .join("user/Plugins.txt");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::write(&copy, "Instance copy\n").unwrap();

    UserFilesFixture {
        _guard: guard,
        _data_dir: data_dir,
        _user_data_dir: user_data_dir,
        ctx,
        def,
        running_from,
        real,
        copy,
    }
}

impl Drop for UserFilesFixture {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

/// A dead process identity: a pid no process can have, verified stale by the OS.
fn dead_identity() -> agora_core::process_identity::ProcessIdentity {
    agora_core::process_identity::ProcessIdentity {
        pid: u32::MAX - 2,
        start_time: 5,
        expected_exe: None,
        job: None,
    }
}

/// The startup pass leaves a session whose recorded process is LIVE alone: liveness detection
/// (pid + start time + exe verified against the OS) is what tells a live session from a dead
/// one, so this test fails if that verification stops working.
#[test]
fn the_startup_pass_leaves_a_live_user_files_session_alone() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    // A live pid: this test process, recorded as the session's game.
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        agora_core::process_identity::capture_self().expect("capture this process"),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Instance copy\n",
        "the live session was restored underneath"
    );
    assert!(
        journal_path.exists(),
        "the journal of a live session is kept"
    );
    assert!(
        report.actions.is_empty(),
        "nothing was recovered: {report:?}"
    );
}

/// A journal that records no process identity at all (a legacy journal, or a self-capture that
/// failed) has unknown liveness: the startup pass does not recover it automatically, it reports
/// it as needing a hand and changes nothing.
#[test]
fn a_user_files_journal_with_no_recorded_identity_needs_a_hand_not_automatic_recovery() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    // Strip the identities, the way a legacy journal or a failed self-capture leaves them.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&journal_path).unwrap()).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("starting_launcher");
    object.insert("processes".into(), serde_json::json!([]));
    std::fs::write(&journal_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Instance copy\n",
        "an unknown-liveness session was restored automatically"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("no process identity") && w.contains("left alone")),
        "the needs-a-hand case is reported: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// A file the player created after a crash — the swap journal says the file was swapped in,
/// but its content is not what the session wrote — is never overwritten by recovery: neither
/// the real file nor the instance's copy is touched; the journal is reported as needing a hand.
#[test]
fn a_file_the_player_made_after_a_crash_is_not_overwritten_by_recovery() {
    // The real file was absent when the session swapped in, so Agora created it from the
    // instance's copy.
    let f = user_files_fixture(None);
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // The crash left the journal saying "swapped" before Agora's write landed (the state an
    // older Agora wrote in that window), and the player then created the file themselves.
    std::fs::remove_file(&f.real).unwrap();
    std::fs::write(&f.real, "Player wrote this after the crash\n").unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Player wrote this after the crash\n",
        "the player's file was deleted or overwritten by recovery"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Instance copy\n",
        "the instance's copy was overwritten with the player's bytes"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("changed since the swap")),
        "the refusal is reported: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// The state the current writer leaves when it is cut off between journaling and creating the
/// real file: the entry is NOT marked swapped. Recovery then leaves a file the player made in
/// the meantime exactly as it is (only bytes exactly matching the recorded write are removed).
#[test]
fn a_crash_before_agora_created_the_file_leaves_the_players_file_alone() {
    let f = user_files_fixture(None);
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    // The write-ahead state of the window before Agora creates the real file.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&journal_path).unwrap()).unwrap();
    let file0 = &mut value["files"][0];
    file0["swapped"] = serde_json::json!(false);
    file0["state"] = serde_json::json!("backing_up");
    std::fs::write(&journal_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    // The player created the file after the crash.
    std::fs::write(&f.real, "Player created this\n").unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Player created this\n",
        "a file the player made in the not-yet-swapped window was removed"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Instance copy\n",
        "the instance's copy was touched"
    );
    assert!(
        report.warnings.is_empty(),
        "nothing needs a hand: the session never swapped this file: {report:?}"
    );
}

/// A backup is never written through a symlink retargeted since the swap: the link's target is
/// recorded in the journal at swap time, and recovery refuses when it no longer matches.
#[cfg(unix)]
#[test]
fn a_symlink_retarged_since_the_swap_is_refused_by_recovery() {
    let f = user_files_fixture(None);
    let user_root = f._user_data_dir.path().to_path_buf();
    // The player's real file is a link to target A.
    let target_a = user_root.join("target-a.txt");
    std::fs::write(&target_a, "Player original\n").unwrap();
    let _ = std::fs::remove_file(&f.real);
    std::os::unix::fs::symlink(&target_a, &f.real).unwrap();

    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    // The swap wrote through the link, and the journal recorded where it pointed.
    assert_eq!(
        std::fs::read_to_string(&target_a).unwrap(),
        "Instance copy\n"
    );
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    // After the crash the player retargets the link to another file of theirs.
    let target_b = user_root.join("target-b.txt");
    std::fs::write(&target_b, "B's own bytes\n").unwrap();
    std::fs::remove_file(&f.real).unwrap();
    std::os::unix::fs::symlink(&target_b, &f.real).unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&target_b).unwrap(),
        "B's own bytes\n",
        "recovery wrote the backup through the retargeted link"
    );
    assert_eq!(
        std::fs::read_to_string(&target_a).unwrap(),
        "Instance copy\n",
        "recovery changed the old target too"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be verified to still resolve")),
        "the refusal names the retargeted link: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// Make `link` point at `target` as a directory link: a junction on Windows (which needs no
/// privilege), a symbolic link elsewhere. Used to retarget a folder of the user data root the
/// way a player could between a crash and recovery.
#[cfg(windows)]
fn link_dir(link: &Path, target: &Path) {
    junction::create(target, link).unwrap();
}

/// Make `link` point at `target` as a directory link (see the Windows [`link_dir`]).
#[cfg(unix)]
fn link_dir(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// When BOTH the real file and the instance's copy changed since the swap, recovery cannot
/// tell whose bytes to keep, so it overwrites neither: the real file's current bytes are kept
/// as a clearly named conflict copy next to the instance's copy, and the journal is reported
/// as needing a hand. (Only when at most one side changed may bytes move.)
#[test]
fn both_sides_changed_since_the_swap_are_kept_as_a_conflict_copy_and_reported() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // After the crash, the game changed the real file and the instance's copy changed too.
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    std::fs::write(&f.copy, "Instance copy changed too\n").unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Game changed this after the crash\n",
        "the real file was overwritten"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Instance copy changed too\n",
        "the instance's copy was overwritten"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    let conflict_dir = f.copy.parent().unwrap();
    let conflicts: Vec<PathBuf> = std::fs::read_dir(conflict_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Plugins.txt.agora-conflict-"))
        })
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "exactly one clearly named conflict copy next to the instance's copy: {conflicts:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&conflicts[0]).unwrap(),
        "Game changed this after the crash\n",
        "the conflict copy holds the real file's current bytes"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("both changed since the swap")),
        "the refusal is reported: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// A backup is never written through an ancestor directory replaced by a link since the swap:
/// the journal records where the real path fully resolved at swap time (every ancestor
/// included), and recovery refuses when it resolves somewhere else now.
#[test]
fn an_ancestor_directory_replaced_by_a_link_since_the_swap_is_refused_by_recovery() {
    let f = user_files_fixture(Some("Player original\n"));
    let user_root = f._user_data_dir.path().to_path_buf();
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // After the crash the player replaces the 'local' folder with a link to another folder.
    let evil = user_root.join("evil-local");
    std::fs::create_dir_all(evil.join("Test Game")).unwrap();
    std::fs::write(evil.join("Test Game").join("Plugins.txt"), "Evil bytes\n").unwrap();
    std::fs::rename(user_root.join("local"), user_root.join("local-orig")).unwrap();
    link_dir(&user_root.join("local"), &evil);

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(evil.join("Test Game").join("Plugins.txt")).unwrap(),
        "Evil bytes\n",
        "recovery wrote the backup through the ancestor link"
    );
    assert_eq!(
        std::fs::read_to_string(
            user_root
                .join("local-orig")
                .join("Test Game")
                .join("Plugins.txt")
        )
        .unwrap(),
        "Instance copy\n",
        "the original location was changed"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be verified to still resolve")),
        "the refusal names the changed resolution: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// A backup is never written through a chain of directory links retargeted since the swap:
/// 'local' -> 'mid' -> the real folder. The journal records the fully resolved path at swap
/// time, so retargeting 'mid' is a resolution change, and recovery refuses.
#[test]
fn a_directory_link_deeper_in_the_chain_retarged_since_the_swap_is_refused_by_recovery() {
    let f = user_files_fixture(Some("Player original\n"));
    let user_root = f._user_data_dir.path().to_path_buf();
    // The real file sits in 'realdir'; 'local' resolves to it through 'mid'.
    std::fs::rename(user_root.join("local"), user_root.join("realdir")).unwrap();
    link_dir(&user_root.join("mid"), &user_root.join("realdir"));
    link_dir(&user_root.join("local"), &user_root.join("mid"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    assert_eq!(
        std::fs::read_to_string(
            user_root
                .join("realdir")
                .join("Test Game")
                .join("Plugins.txt")
        )
        .unwrap(),
        "Instance copy\n",
        "the swap wrote through the link chain"
    );
    // After the crash the player retargets the inner link of the chain.
    let evil = user_root.join("evil-mid");
    std::fs::create_dir_all(evil.join("Test Game")).unwrap();
    std::fs::write(evil.join("Test Game").join("Plugins.txt"), "Evil bytes\n").unwrap();
    // A junction is removed as a directory on Windows, as a link on Unix.
    #[cfg(windows)]
    std::fs::remove_dir(user_root.join("mid")).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(user_root.join("mid")).unwrap();
    link_dir(&user_root.join("mid"), &evil);

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(evil.join("Test Game").join("Plugins.txt")).unwrap(),
        "Evil bytes\n",
        "recovery wrote the backup through the retargeted inner link"
    );
    assert_eq!(
        std::fs::read_to_string(
            user_root
                .join("realdir")
                .join("Test Game")
                .join("Plugins.txt")
        )
        .unwrap(),
        "Instance copy\n",
        "the old target was changed"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be verified to still resolve")),
        "the refusal names the changed resolution: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// A backup is never written through a chain of file links retargeted since the swap:
/// real -> link1 -> link2 -> target. The journal records the fully resolved target at swap
/// time, so retargeting link2 is a resolution change the link text of the real path cannot
/// show, and recovery refuses.
#[cfg(unix)]
#[test]
fn a_file_link_deeper_in_the_chain_retarged_since_the_swap_is_refused_by_recovery() {
    let f = user_files_fixture(None);
    let user_root = f._user_data_dir.path().to_path_buf();
    // The player's real file is a chain of links to a target file.
    let target1 = user_root.join("target1.txt");
    std::fs::write(&target1, "Player original\n").unwrap();
    let link2 = user_root.join("link2");
    std::os::unix::fs::symlink(&target1, &link2).unwrap();
    let link1 = user_root.join("link1");
    std::os::unix::fs::symlink(&link2, &link1).unwrap();
    let _ = std::fs::remove_file(&f.real);
    std::os::unix::fs::symlink(&link1, &f.real).unwrap();

    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    // The swap wrote through the whole chain, and the journal recorded the resolved target.
    assert_eq!(
        std::fs::read_to_string(&target1).unwrap(),
        "Instance copy\n"
    );
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    // After the crash the player retargets the inner link.
    let target2 = user_root.join("target2.txt");
    std::fs::write(&target2, "Target two's own bytes\n").unwrap();
    std::fs::remove_file(&link2).unwrap();
    std::os::unix::fs::symlink(&target2, &link2).unwrap();

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&target2).unwrap(),
        "Target two's own bytes\n",
        "recovery wrote the backup through the retargeted inner link"
    );
    assert_eq!(
        std::fs::read_to_string(&target1).unwrap(),
        "Instance copy\n",
        "the old target was changed"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be verified to still resolve")),
        "the refusal names the changed resolution: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// Make `link` name the same file as `target`: a hard link on Windows (which needs no
/// privilege), a symbolic link elsewhere.
#[cfg(windows)]
fn link_file(link: &Path, target: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::CreateHardLinkW;
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let ok = unsafe {
        CreateHardLinkW(
            wide(link).as_ptr(),
            wide(target).as_ptr(),
            std::ptr::null_mut(),
        )
    };
    assert!(
        ok != 0,
        "CreateHardLinkW {} -> {} failed (os error {})",
        link.display(),
        target.display(),
        std::io::Error::last_os_error()
    );
}

/// Make `link` name the same file as `target` (see the Windows [`link_file`]).
#[cfg(unix)]
fn link_file(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Equal bytes on both sides are unambiguous ONLY when the instance's copy and the real file
/// are provably different files. When the instance copy is a link to (or a hard link of) the
/// real file, both sides read the same changed bytes, and putting the backup over the shared
/// target would lose the edit — so recovery refuses, keeps the real file's bytes as a conflict
/// copy, and reports. (Before the identity check, recovery proceeded and destroyed the edit.)
#[test]
fn an_instance_copy_that_is_the_real_file_needs_a_hand_not_a_same_bytes_restore() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // After the crash, the instance's copy is a link to the real file, and the game changed the
    // real file: both sides now read the same changed bytes.
    std::fs::remove_file(&f.copy).unwrap();
    link_file(&f.copy, &f.real);
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Game changed this after the crash\n",
        "the copy and the real file are the same file now"
    );

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Game changed this after the crash\n",
        "the backup was written over the shared file and the game's edit was lost"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Game changed this after the crash\n",
        "the instance's side changed"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    let conflicts: Vec<PathBuf> = std::fs::read_dir(f.copy.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Plugins.txt.agora-conflict-"))
        })
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "the real file's bytes are kept as a conflict copy: {conflicts:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&conflicts[0]).unwrap(),
        "Game changed this after the crash\n"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("both changed since the swap")),
        "the refusal is reported: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// A legacy journal (no recorded resolved path) fails closed on ANY recovery that would write,
/// not only when the final component is a link: here an ancestor directory was replaced by a
/// link after the crash, and the final component is an ordinary file, so the old link-text guard
/// could not see it. Recovery must refuse and write nothing through the redirected ancestor.
#[test]
fn a_legacy_journal_with_a_redirected_ancestor_is_refused_by_recovery() {
    let f = user_files_fixture(Some("Player original\n"));
    let user_root = f._user_data_dir.path().to_path_buf();
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert!(journal_path.exists());
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // Make the journal legacy: no recorded resolved path.
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&journal_path).unwrap()).unwrap();
    value["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("real_resolved_path");
    std::fs::write(&journal_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    // After the crash the player replaces the 'local' folder with a link to another folder.
    let evil = user_root.join("evil-local");
    std::fs::create_dir_all(evil.join("Test Game")).unwrap();
    std::fs::write(evil.join("Test Game").join("Plugins.txt"), "Evil bytes\n").unwrap();
    std::fs::rename(user_root.join("local"), user_root.join("local-orig")).unwrap();
    link_dir(&user_root.join("local"), &evil);

    let report = run_at_startup(&f.ctx);

    assert_eq!(
        std::fs::read_to_string(evil.join("Test Game").join("Plugins.txt")).unwrap(),
        "Evil bytes\n",
        "recovery wrote the backup through the redirected ancestor of a legacy journal"
    );
    assert_eq!(
        std::fs::read_to_string(
            user_root
                .join("local-orig")
                .join("Test Game")
                .join("Plugins.txt")
        )
        .unwrap(),
        "Instance copy\n",
        "the original location was changed"
    );
    assert!(journal_path.exists(), "the journal is kept for a hand");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("cannot be verified to still resolve")),
        "the refusal is reported: {report:?}"
    );
    assert!(report.actions.is_empty(), "{report:?}");
}

/// The swap records where the real path will resolve even when the file does not exist yet
/// (Agora is about to create it): the journal of an absent-at-swap file carries a resolved path,
/// so its normal recovery (remove Agora's own write) is not mistaken for a legacy journal. The
/// recovery itself must still succeed.
#[test]
fn a_file_absent_at_swap_records_its_would_be_resolved_path_and_recovers_normally() {
    let f = user_files_fixture(None);
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&journal_path).unwrap()).unwrap();
    let recorded = value["files"][0]["real_resolved_path"]
        .as_str()
        .unwrap_or("");
    assert!(
        recorded.to_lowercase().ends_with("plugins.txt"),
        "the journal records where the absent file will resolve: {recorded}"
    );
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");

    let report = run_at_startup(&f.ctx);

    assert!(
        !f.real.exists(),
        "Agora's own write (byte-identical to the recorded one) was removed: {}",
        std::fs::read_to_string(&f.real).unwrap_or_default()
    );
    assert!(!journal_path.exists(), "the journal is gone after recovery");
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.contains("Put back the user files")),
        "the recovery ran: {report:?}"
    );
}

/// The explicit restore command's override: on a both-changed conflict the plain restore (and
/// automatic startup recovery, which never forces) refuses and archives the real file's bytes;
/// the forced restore archives the instance edit it is about to overwrite and then puts the files
/// back — the original goes back to the real file, the game's changed bytes become the instance's
/// copy.
#[test]
fn a_both_changed_conflict_is_forced_only_by_the_explicit_restore_command() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    let journal_path = f.ctx.paths.user_files_journal_path(GAME, "steam");
    assert_eq!(std::fs::read_to_string(&f.real).unwrap(), "Instance copy\n");
    // After the crash, both sides changed to different bytes.
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    std::fs::write(&f.copy, "Instance copy changed too\n").unwrap();

    // Automatic startup recovery refuses and never forces.
    let report = run_at_startup(&f.ctx);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("both changed since the swap")),
        "startup recovery must report the conflict: {report:?}"
    );
    assert!(
        report.actions.is_empty(),
        "startup recovery forced nothing: {report:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Game changed this after the crash\n"
    );

    // The explicit restore without --force refuses the same way and names the override.
    let err = agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("both changed since the swap") && err.contains("--force"),
        "the refusal must name the forced command: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Game changed this after the crash\n",
        "the unforced restore changed the real file"
    );
    assert!(journal_path.exists(), "the journal is kept");

    // The forced restore: conflict copy first, then the put-back.
    let forced = agora_core::game_user_files::restore_forced(&f.ctx, &f.def, &StoreId::steam())
        .expect("the forced restore completes");
    assert!(
        forced.files.iter().any(|file| file.changed),
        "the game's change is reported: {forced:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Player original\n",
        "the original is back on the real file"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Game changed this after the crash\n",
        "the game's changed bytes were saved into the instance's copy"
    );
    let conflicts: Vec<PathBuf> = std::fs::read_dir(f.copy.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Plugins.txt.agora-conflict-"))
        })
        .collect();
    // The unforced refusals archived the real file's bytes; the forced restore archives the
    // instance edit it overwrites. Both versions stay on disk.
    let archived: Vec<String> = conflicts
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    assert!(
        archived
            .iter()
            .any(|bytes| bytes == "Game changed this after the crash\n"),
        "the unforced refusals kept the real file's bytes: {archived:?}"
    );
    assert!(
        archived
            .iter()
            .any(|bytes| bytes == "Instance copy changed too\n"),
        "the forced restore archived the instance edit it overwrote: {archived:?}"
    );
    assert!(
        !journal_path.exists(),
        "the journal is cleared by the forced restore"
    );
}

/// A forced restore must archive the bytes it displaces — the independently edited instance copy —
/// before overwriting them. This starts from a fresh fixture and calls only `restore_forced`, so
/// the archive in this test cannot be a leftover from an earlier unforced call.
#[test]
fn a_forced_restore_archives_the_instance_edit_it_overwrites() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    std::fs::write(&f.copy, "Instance copy changed too\n").unwrap();

    let forced = agora_core::game_user_files::restore_forced(&f.ctx, &f.def, &StoreId::steam())
        .expect("the forced restore completes");
    assert!(forced.files.iter().any(|file| file.changed));

    let conflicts: Vec<PathBuf> = std::fs::read_dir(f.copy.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Plugins.txt.agora-conflict-"))
        })
        .collect();
    assert_eq!(
        conflicts.len(),
        1,
        "one archive from this forced call: {conflicts:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&conflicts[0]).unwrap(),
        "Instance copy changed too\n",
        "the archive holds the instance edit that the restore overwrote"
    );
    assert_eq!(
        std::fs::read_to_string(&f.copy).unwrap(),
        "Game changed this after the crash\n",
        "the game's changed bytes are the instance's copy now"
    );
    assert_eq!(
        std::fs::read_to_string(&f.real).unwrap(),
        "Player original\n"
    );
}

fn conflict_archives(f: &UserFilesFixture) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(f.copy.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("Plugins.txt.agora-conflict-"))
        })
        .collect();
    paths.sort();
    paths
}

fn conflicting_user_files(f: &UserFilesFixture) {
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    std::fs::write(&f.copy, "Instance copy changed too\n").unwrap();
}

/// A case-only retarget must be refused by recovery itself, not just by the guard helper: the
/// resolved path recorded at swap time is compared exactly with where the file resolves now.
/// Case-insensitive filesystems cannot distinguish the two names, so they skip with a clear reason.
#[cfg(unix)]
#[test]
fn a_case_only_retarget_is_refused_by_recovery() {
    fn case_sensitive(dir: &Path) -> bool {
        let probe = dir.join("CaseProbe");
        std::fs::write(&probe, b"a").unwrap();
        let sensitive = std::fs::metadata(dir.join("caseprobe")).is_err();
        let _ = std::fs::remove_file(&probe);
        sensitive
    }

    let f = user_files_fixture(Some("Player original\n"));
    if !case_sensitive(f._user_data_dir.path()) {
        eprintln!("skipping: this filesystem does not distinguish case in file names");
        return;
    }
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();

    // After the crash the exact path is a link to a different file whose name differs only by the
    // case of its final component.
    let retarget = f._user_data_dir.path().join("local/Test Game/plugins.txt");
    std::fs::write(&retarget, "Game changed this after the crash\n").unwrap();
    std::fs::remove_file(&f.real).unwrap();
    std::os::unix::fs::symlink(&retarget, &f.real).unwrap();

    let error = agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cannot be verified to still resolve"),
        "recovery must refuse the case-only retarget: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(&retarget).unwrap(),
        "Game changed this after the crash\n",
        "the retargeted file was overwritten"
    );
}

/// An instance copy whose bytes cannot be read is "can't tell", not "unchanged": neither the plain
/// nor the forced restore may overwrite it. A directory at the path is unreadable as bytes on every
/// platform; a Unix-only write-only-file fixture would exercise the same guard with a renameable
/// file.
#[test]
fn an_unreadable_instance_copy_is_kept_by_both_restores() {
    let f = user_files_fixture(Some("Player original\n"));
    agora_core::game_user_files::swap_in(
        &f.ctx,
        "instance-a",
        &f.def,
        &StoreId::steam(),
        &f.running_from,
    )
    .unwrap();
    agora_core::game_user_files::record_process(
        &f.ctx,
        &f.def.id,
        &StoreId::steam(),
        dead_identity(),
    )
    .unwrap();
    std::fs::write(&f.real, "Game changed this after the crash\n").unwrap();
    std::fs::remove_file(&f.copy).unwrap();
    std::fs::create_dir(&f.copy).unwrap();
    std::fs::write(f.copy.join("keep"), "do not touch").unwrap();

    for force in [false, true] {
        let error = if force {
            agora_core::game_user_files::restore_forced(&f.ctx, &f.def, &StoreId::steam())
                .unwrap_err()
        } else {
            agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err()
        }
        .to_string();
        assert!(
            error.contains("could not be read"),
            "a read failure must be reported as can't-tell (force={force}): {error}"
        );
        assert!(
            f.copy.join("keep").exists(),
            "the unreadable instance copy was overwritten (force={force})"
        );
        assert_eq!(
            std::fs::read_to_string(&f.real).unwrap(),
            "Game changed this after the crash\n",
            "the real file was changed (force={force})"
        );
    }
}

/// Repeated startups with the same conflict must reuse the archive they already wrote, not pile up
/// numbered copies of identical bytes.
#[test]
fn repeated_conflicts_reuse_the_identical_archive() {
    let f = user_files_fixture(Some("Player original\n"));
    conflicting_user_files(&f);

    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();
    let first = conflict_archives(&f);
    assert_eq!(first.len(), 1, "the first refusal wrote one archive");

    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();
    let second = conflict_archives(&f);
    assert_eq!(
        second.len(),
        1,
        "a second refusal wrote a duplicate archive: {second:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&second[0]).unwrap(),
        "Game changed this after the crash\n"
    );
}

/// A conflict archive uses create-new: a file already at that name is left exactly as it was and
/// the archive is written under a new number.
#[test]
fn an_existing_file_at_the_conflict_name_is_not_replaced() {
    let f = user_files_fixture(Some("Player original\n"));
    conflicting_user_files(&f);
    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();
    let taken = conflict_archives(&f)
        .into_iter()
        .next()
        .expect("one archive");
    std::fs::write(&taken, "you may not replace me").unwrap();

    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();

    assert_eq!(
        std::fs::read_to_string(&taken).unwrap(),
        "you may not replace me",
        "the file at the conflict name was replaced"
    );
    assert_eq!(
        conflict_archives(&f).len(),
        2,
        "the second refusal wrote a numbered archive instead of replacing the first"
    );
}

/// A symlink at the conflict name must not be followed: the archive goes to a new name and the
/// link's target is untouched. Creating file symlinks needs a privilege on some Windows hosts, so
/// this fixture is Unix-only; the directory case below covers the same create-new rule everywhere.
#[cfg(unix)]
#[test]
fn a_symlink_at_the_conflict_name_is_not_followed() {
    let f = user_files_fixture(Some("Player original\n"));
    conflicting_user_files(&f);
    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();
    let taken = conflict_archives(&f)
        .into_iter()
        .next()
        .expect("one archive");
    let sentinel = f.copy.parent().unwrap().join("sentinel.txt");
    std::fs::write(&sentinel, "do not touch").unwrap();
    std::fs::remove_file(&taken).unwrap();
    std::os::unix::fs::symlink(&sentinel, &taken).unwrap();

    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();

    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "do not touch",
        "the conflict archive was written through the symlink"
    );
    assert!(
        std::fs::symlink_metadata(&taken)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink at the conflict name was replaced"
    );
    assert_eq!(conflict_archives(&f).len(), 2);
}

/// A directory already at the conflict name is an existing name too: create-new refuses it and the
/// directory survives. This runs everywhere, including hosts where file symlinks need a privilege.
#[test]
fn a_directory_at_the_conflict_name_is_not_replaced() {
    let f = user_files_fixture(Some("Player original\n"));
    conflicting_user_files(&f);
    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();
    let taken = conflict_archives(&f)
        .into_iter()
        .next()
        .expect("one archive");
    std::fs::remove_file(&taken).unwrap();
    std::fs::create_dir(&taken).unwrap();
    std::fs::write(taken.join("keep"), "do not touch").unwrap();

    agora_core::game_user_files::restore(&f.ctx, &f.def, &StoreId::steam()).unwrap_err();

    assert_eq!(
        std::fs::read_to_string(taken.join("keep")).unwrap(),
        "do not touch",
        "the directory at the conflict name was replaced"
    );
    assert_eq!(conflict_archives(&f).len(), 2);
}
