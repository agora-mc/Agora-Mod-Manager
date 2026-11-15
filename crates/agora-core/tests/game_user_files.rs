use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agora_core::ctx::CoreContext;
use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_core::game_user_files::{
    is_session_running, record_process, restore, restore_finished_for_instance, status, swap_in,
    Journal, JournaledFile,
};
#[cfg(windows)]
use agora_core::game_user_files::{restore_failure_message, restore_retrying_while_running};
use agora_core::process_identity;
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, PackageDefinition, RelPath,
    StoreId, StoreIdentifier, UserDataLocation, UserFileMapping, UserFileStrategy,
};
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct TestHarness {
    _lock: std::sync::MutexGuard<'static, ()>,
    _data_dir: TempDir,
    user_data_dir: TempDir,
    ctx: CoreContext,
    running_from: PathBuf,
}

impl TestHarness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap();
        let data_dir = TempDir::new().unwrap();
        let user_data_dir = TempDir::new().unwrap();

        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data_dir.path());

        let ctx = CoreContext::for_testing(data_dir.path().join("app_data"));
        let _ = agora_core::db::init_local_state_db(&ctx.paths.local_state_db());

        let running_from = data_dir.path().join("running_from");
        std::fs::create_dir_all(&running_from).unwrap();

        Self {
            _lock: lock,
            _data_dir: data_dir,
            user_data_dir,
            ctx,
            running_from,
        }
    }

    fn local_dir(&self) -> PathBuf {
        self.user_data_dir.path().join("local")
    }

    fn documents_dir(&self) -> PathBuf {
        self.user_data_dir.path().join("documents")
    }

    fn instance_dir(&self, id: &str) -> PathBuf {
        let dir = self.ctx.paths.instances_root().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

fn test_skyrim_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        import_ids: Default::default(),
        session: Default::default(),
        id: GameId::new("skyrim-se").unwrap(),
        extends: None,
        name: "The Elder Scrolls V: Skyrim Special Edition".to_string(),
        stores: vec![
            StoreIdentifier {
                store: StoreId::steam(),
                product: "489830".into(),
            },
            StoreIdentifier {
                store: StoreId::gog(),
                product: "1711230643".into(),
            },
        ],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: None,
        log_paths: vec![],
        crash_paths: vec![],
        user_files: vec![
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Skyrim Special Edition/Plugins.txt").unwrap(),
                },
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::steam()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::Documents,
                    path: RelPath::new("My Games/Skyrim Special Edition/Skyrim.ini").unwrap(),
                },
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::steam()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::LocalAppData,
                    path: RelPath::new("Skyrim Special Edition GOG/Plugins.txt").unwrap(),
                },
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::gog()],
            },
            UserFileMapping {
                source: GamePath::UserData {
                    location: UserDataLocation::Documents,
                    path: RelPath::new("My Games/Skyrim Special Edition GOG/Skyrim.ini").unwrap(),
                },
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                strategy: UserFileStrategy::JournaledSwap,
                stores: vec![StoreId::gog()],
            },
        ],
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

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn skyrim_registry(def: &GameDefinition) -> Arc<GameRegistry> {
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

/// Swap in the one user file the catch-up tests use, with known player and instance contents.
fn swap_one_file(harness: &TestHarness, def: &GameDefinition, instance: &str, ctx: &CoreContext) {
    let real = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(
        &real,
        "Player original
",
    )
    .unwrap();
    let copy = harness.instance_dir(instance).join("user/Plugins.txt");
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::write(
        &copy,
        "Instance copy
",
    )
    .unwrap();
    swap_in(ctx, instance, def, &StoreId::steam(), &harness.running_from).unwrap();
    // The game started and is now gone: its identity replaces the launcher's "starting" mark.
    record_process(
        ctx,
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
}

#[test]
fn catch_up_restores_a_finished_swap_session() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);

    let found = restore_finished_for_instance(&ctx, "instance-a")
        .expect("the finished journal is restored");
    assert!(found.error.is_none(), "{found:?}");
    assert!(found.restored >= 1);
    assert!(
        !ctx.paths
            .user_files_journal_path("skyrim-se", "steam")
            .exists(),
        "the journal is gone after every file is back"
    );
    let real = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    assert_eq!(
        std::fs::read_to_string(real).unwrap(),
        "Player original
"
    );
}

#[test]
fn catch_up_leaves_a_running_swap_session_alone() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);
    record_process(
        &ctx,
        &def.id,
        &StoreId::steam(),
        process_identity::capture(std::process::id()).unwrap(),
    )
    .unwrap();

    assert!(
        restore_finished_for_instance(&ctx, "instance-a").is_none(),
        "a game that may still run is not restored over"
    );
    assert!(ctx
        .paths
        .user_files_journal_path("skyrim-se", "steam")
        .exists());
}

#[test]
fn catch_up_never_calls_a_failed_restore_success() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);
    // Without its backup the restore cannot prove the player's original; it must refuse.
    std::fs::remove_dir_all(ctx.paths.user_files_backup_dir("skyrim-se", "steam")).unwrap();

    let found =
        restore_finished_for_instance(&ctx, "instance-a").expect("the journal is still reported");
    assert!(found.error.is_some(), "{found:?}");
    assert!(ctx
        .paths
        .user_files_journal_path("skyrim-se", "steam")
        .exists());
}

#[test]
fn crash_catch_up_restores_user_files_with_no_open_session() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);

    let host = agora_plugin_host::QuickJsHost::new();
    let report = agora_core::crash_live::catch_up(
        &ctx,
        "instance-a",
        agora_core::crash_live::LivePacks {
            packs: &[],
            built_in: &[],
            host: &host,
            instances: None,
        },
    );
    let user = report
        .user_files
        .expect("the finished swap session is restored");
    assert!(user.error.is_none(), "{user:?}");
    assert!(user.restored >= 1);
    let real = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    assert_eq!(
        std::fs::read_to_string(real).unwrap(),
        "Player original
"
    );
}

#[cfg(windows)]
#[test]
fn a_sharing_violation_is_retried_until_the_holder_lets_go() {
    use std::os::windows::fs::OpenOptionsExt;

    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);
    let real = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    // An invented holder keeps the file open with no sharing, the way a crashing game (or its
    // crash handler) can while it finishes. It lets go after a moment; the restore must wait.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&real)
        .unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        drop(held);
    });

    let report = restore_retrying_while_running(
        &ctx,
        &def,
        &StoreId::steam(),
        std::time::Duration::from_secs(10),
    )
    .expect("a transient sharing violation is waited out, not called a failure");
    release.join().unwrap();
    assert!(!report.files.is_empty());
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "Player original\n",
        "the player's file is back once the holder lets go"
    );
}

#[cfg(windows)]
#[test]
fn a_file_held_past_the_limit_names_the_exact_command() {
    use std::os::windows::fs::OpenOptionsExt;

    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    swap_one_file(&harness, &def, "instance-a", &ctx);
    let real = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&real)
        .unwrap();

    let error = restore_retrying_while_running(
        &ctx,
        &def,
        &StoreId::steam(),
        std::time::Duration::from_millis(250),
    )
    .expect_err("a file still held at the limit is reported, never called restored");
    let message = restore_failure_message(&error.to_string(), "skyrim-se", "steam");
    assert!(message.contains("could not be restored"), "{message}");
    assert!(
        message.contains("agora games user-files restore skyrim-se steam"),
        "the message gives the exact command: {message}"
    );
    assert!(
        ctx.paths
            .user_files_journal_path("skyrim-se", "steam")
            .exists(),
        "the journal is left so a later catch-up can finish the restore"
    );
    drop(held);
}

#[test]
fn catch_up_has_nothing_to_do_without_a_journal() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();
    let ctx = harness.ctx.clone().with_games(skyrim_registry(&def));
    assert!(restore_finished_for_instance(&ctx, "instance-a").is_none());
}

// ---------------------------------------------------------------------------
// Gate 1: Two instances swap and restore
// ---------------------------------------------------------------------------

#[test]
fn two_instances_swap_and_restore() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    // 1. Setup user original real file
    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "User Original Plugins\n").unwrap();

    // 2. Setup instance A and instance B files
    let inst_a_dir = harness.instance_dir("instance-a");
    let inst_a_plugin = inst_a_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_a_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_a_plugin, "Instance A Plugins\n").unwrap();

    let inst_b_dir = harness.instance_dir("instance-b");
    let inst_b_plugin = inst_b_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_b_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_b_plugin, "Instance B Plugins\n").unwrap();

    // --- Instance A session ---
    let outcome_a = swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();
    assert!(outcome_a.swapped_files > 0);

    // Real file has instance A's content
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Instance A Plugins\n"
    );

    // Simulate game modifying the file during session
    std::fs::write(
        &real_plugin_file,
        "Instance A Plugins Modified During Session\n",
    )
    .unwrap();

    // Restore session A
    let report_a = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert!(report_a.files.iter().any(|f| f.changed));

    // Real file is back to user's original byte-for-byte
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "User Original Plugins\n"
    );

    // Instance A's copy now holds what the game wrote
    assert_eq!(
        std::fs::read_to_string(&inst_a_plugin).unwrap(),
        "Instance A Plugins Modified During Session\n"
    );

    // --- Instance B session ---
    let outcome_b = swap_in(
        &harness.ctx,
        "instance-b",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();
    assert!(outcome_b.swapped_files > 0);

    // Real file has instance B's content
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Instance B Plugins\n"
    );

    // Simulate game modifying file during session B
    std::fs::write(
        &real_plugin_file,
        "Instance B Plugins Modified During Session\n",
    )
    .unwrap();

    // Restore session B
    let report_b = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert!(report_b.files.iter().any(|f| f.changed));

    // Real file is back to user's original byte-for-byte
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "User Original Plugins\n"
    );

    // Instance B's copy holds what game wrote
    assert_eq!(
        std::fs::read_to_string(&inst_b_plugin).unwrap(),
        "Instance B Plugins Modified During Session\n"
    );
    // Instance A's copy remains intact
    assert_eq!(
        std::fs::read_to_string(&inst_a_plugin).unwrap(),
        "Instance A Plugins Modified During Session\n"
    );
}

// ---------------------------------------------------------------------------
// Gate 2: Two launches contend, second refused naming first
// ---------------------------------------------------------------------------

#[test]
fn two_launches_contend_second_refused_naming_first() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "Original Plugins\n").unwrap();

    let inst_a_dir = harness.instance_dir("instance-a");
    let inst_a_plugin = inst_a_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_a_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_a_plugin, "Instance A\n").unwrap();

    let inst_b_dir = harness.instance_dir("instance-b");
    let inst_b_plugin = inst_b_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_b_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_b_plugin, "Instance B\n").unwrap();

    // Launch instance A
    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // Simulate A's session running with a live process identity
    let live_id = process_identity::capture(std::process::id()).unwrap();
    record_process(&harness.ctx, &def.id, &StoreId::steam(), live_id).unwrap();

    // Instance B contends on the same game & store
    let err = swap_in(
        &harness.ctx,
        "instance-b",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("instance-a"),
        "error must name instance A that holds it: {msg}"
    );
    assert!(
        msg.contains("is running as instance instance-a; close it first"),
        "error must explain why: {msg}"
    );

    // Clean up journal so test completes cleanly
    let journal_path = harness
        .ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str());
    let _ = std::fs::remove_file(journal_path);
}

// ---------------------------------------------------------------------------
// Gate 3: Failed launch restores cleanly
// ---------------------------------------------------------------------------

#[test]
fn failed_launch_restores_cleanly() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "Original Plugins\n").unwrap();

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance A Plugins\n").unwrap();

    // 1. Swap in succeeds
    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Instance A Plugins\n"
    );

    // 2. Simulated spawn failure immediately triggers restore
    let report = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert!(!report.files.is_empty());

    // 3. Real file is left exactly as before and no journal remains
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Original Plugins\n"
    );
    assert!(status(&harness.ctx, &def.id, &StoreId::steam())
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// Gate 4: Agora killed while running, then restarted
// ---------------------------------------------------------------------------

#[test]
fn agora_killed_while_running_then_restarted() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "Original Plugins\n").unwrap();

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance A Plugins\n").unwrap();

    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // Record live process identity
    let live_id = process_identity::capture(std::process::id()).unwrap();
    record_process(&harness.ctx, &def.id, &StoreId::steam(), live_id).unwrap();

    // While process is alive, restore refuses and changes nothing
    let restore_err = restore(&harness.ctx, &def, &StoreId::steam()).unwrap_err();
    assert!(
        restore_err.to_string().contains("is active"),
        "must refuse while running: {restore_err}"
    );
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Instance A Plugins\n"
    );

    // Now simulate process gone: replace process in journal with a dead PID
    let journal_path = harness
        .ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str());
    let text = std::fs::read_to_string(&journal_path).unwrap();
    let mut journal: Journal = serde_json::from_str(&text).unwrap();
    journal.processes = vec![process_identity::ProcessIdentity {
        pid: 999_999_999,
        start_time: 12345,
        expected_exe: None,
        job: None,
    }];
    std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();

    assert!(!is_session_running(&journal));

    // Once process is gone, restore succeeds
    let report = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert!(!report.files.is_empty());
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Original Plugins\n"
    );
    assert!(status(&harness.ctx, &def.id, &StoreId::steam())
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// Gate 5: File edited externally before recovery is kept
// ---------------------------------------------------------------------------

#[test]
fn file_edited_externally_before_recovery_is_kept() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "Original Plugins\n").unwrap();

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance A Plugins\n").unwrap();

    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // After session, external tool edits the real file
    std::fs::write(&real_plugin_file, "Externally Edited By Player\n").unwrap();

    let report = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    let file_report = report
        .files
        .iter()
        .find(|f| f.instance_path == "user/Plugins.txt")
        .unwrap();
    assert!(
        file_report.changed,
        "external edit must be reported as changed"
    );

    // The edited contents are kept in the instance's copy
    assert_eq!(
        std::fs::read_to_string(&inst_plugin).unwrap(),
        "Externally Edited By Player\n"
    );

    // And the original is back in place in the user profile
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Original Plugins\n"
    );
}

// ---------------------------------------------------------------------------
// Gate 6: Interrupted swap restores correctly
// ---------------------------------------------------------------------------

#[test]
fn interrupted_swap_restores_correctly() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(real_plugin_file.parent().unwrap()).unwrap();
    std::fs::write(&real_plugin_file, "Original Plugins\n").unwrap();

    let real_ini_file = harness
        .documents_dir()
        .join("My Games")
        .join("Skyrim Special Edition")
        .join("Skyrim.ini");
    std::fs::create_dir_all(real_ini_file.parent().unwrap()).unwrap();
    std::fs::write(&real_ini_file, "Original Skyrim.ini\n").unwrap();

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance A Plugins\n").unwrap();

    let inst_ini = inst_dir.join("user").join("Skyrim.ini");
    std::fs::create_dir_all(inst_ini.parent().unwrap()).unwrap();
    std::fs::write(&inst_ini, "Instance A Skyrim.ini\n").unwrap();

    // Setup an interrupted session on disk:
    // File 0 (Plugins.txt) was swapped (swapped: true, backup written, real file updated)
    // File 1 (Skyrim.ini) was NOT swapped (swapped: false, real file untouched)
    let backup_dir = harness
        .ctx
        .paths
        .user_files_backup_dir(def.id.as_str(), StoreId::steam().as_str());
    std::fs::create_dir_all(&backup_dir).unwrap();
    let backup_0 = backup_dir.join("0");
    std::fs::copy(&real_plugin_file, &backup_0).unwrap();
    std::fs::write(&real_plugin_file, "Swapped Plugins Content\n").unwrap();

    let journal = Journal {
        game: def.id.clone(),
        store: StoreId::steam(),
        instance_id: "instance-a".to_string(),
        created_at: "2026-10-05T00:00:00Z".to_string(),
        running_from: harness.running_from.clone(),
        processes: vec![],
        starting_launcher: None,
        files: vec![
            JournaledFile {
                real_path: real_plugin_file.clone(),
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                instance_copy_path: inst_plugin.clone(),
                existed: true,
                backup_path: Some(backup_0),
                original_sha256: Some(sha256_hex("Original Plugins\n".as_bytes())),
                written_sha256: sha256_hex("Swapped Plugins Content\n".as_bytes()),
                swapped: true,
                state: None,
                restored: false,
            },
            JournaledFile {
                real_path: real_ini_file.clone(),
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                instance_copy_path: inst_ini.clone(),
                existed: true,
                backup_path: None,
                original_sha256: None,
                written_sha256: String::new(),
                swapped: false,
                state: None,
                restored: false,
            },
        ],
    };

    let journal_path = harness
        .ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str());
    std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();

    // Restore should correctly restore file 0 and skip file 1
    let report = restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert_eq!(report.files.len(), 1);

    // File 0 is restored to its original
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Original Plugins\n"
    );

    // File 1 is untouched
    assert_eq!(
        std::fs::read_to_string(&real_ini_file).unwrap(),
        "Original Skyrim.ini\n"
    );

    // Journal and backups deleted
    assert!(!journal_path.exists());
    assert!(!backup_dir.exists());
}

// ---------------------------------------------------------------------------
// Gate 7: First launch with no instance copy keeps user file unchanged
// ---------------------------------------------------------------------------

#[test]
fn first_launch_no_instance_copy_keeps_user_file_unchanged() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_ini_file = harness
        .documents_dir()
        .join("My Games")
        .join("Skyrim Special Edition")
        .join("Skyrim.ini");
    std::fs::create_dir_all(real_ini_file.parent().unwrap()).unwrap();
    std::fs::write(&real_ini_file, "User Original Settings INI\n").unwrap();

    // Instance has NO copy yet
    let inst_dir = harness.instance_dir("instance-a");
    let inst_ini = inst_dir.join("user").join("Skyrim.ini");
    assert!(!inst_ini.exists());

    // Swap in
    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // First launch keeps user's file unchanged!
    assert_eq!(
        std::fs::read_to_string(&real_ini_file).unwrap(),
        "User Original Settings INI\n"
    );

    // Game writes new settings during session
    std::fs::write(&real_ini_file, "Modified Settings INI From Game\n").unwrap();

    // Restore
    restore(&harness.ctx, &def, &StoreId::steam()).unwrap();

    // Real file has user's original back
    assert_eq!(
        std::fs::read_to_string(&real_ini_file).unwrap(),
        "User Original Settings INI\n"
    );

    // Instance's copy now exists and holds the game's settings
    assert_eq!(
        std::fs::read_to_string(&inst_ini).unwrap(),
        "Modified Settings INI From Game\n"
    );
}

// ---------------------------------------------------------------------------
// Gate 8: Mapping for another store not touched; Plugins.txt matches plugins.txt
// ---------------------------------------------------------------------------

#[test]
fn mapping_for_another_store_not_touched_and_case_insensitive_matching() {
    let harness = TestHarness::new();

    // Create a definition where Steam mapping uses lowercase "plugins.txt"
    let mut def = test_skyrim_definition();
    def.user_files[0].source = GamePath::UserData {
        location: UserDataLocation::LocalAppData,
        path: RelPath::new("Skyrim Special Edition/plugins.txt").unwrap(),
    };

    // On disk, create "Plugins.txt" (TitleCase) for Steam, and GOG file
    let steam_real_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    std::fs::create_dir_all(steam_real_file.parent().unwrap()).unwrap();
    std::fs::write(&steam_real_file, "Steam Original Plugins\n").unwrap();

    let gog_real_file = harness
        .local_dir()
        .join("Skyrim Special Edition GOG")
        .join("Plugins.txt");
    std::fs::create_dir_all(gog_real_file.parent().unwrap()).unwrap();
    std::fs::write(&gog_real_file, "GOG Original Plugins\n").unwrap();

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance A Swapped Plugins\n").unwrap();

    // Swap in Steam
    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // GOG mapping for another store was completely untouched
    assert_eq!(
        std::fs::read_to_string(&gog_real_file).unwrap(),
        "GOG Original Plugins\n"
    );

    // Plugins.txt matched the mapping that said plugins.txt
    assert_eq!(
        std::fs::read_to_string(&steam_real_file).unwrap(),
        "Instance A Swapped Plugins\n"
    );

    // Restore
    restore(&harness.ctx, &def, &StoreId::steam()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&steam_real_file).unwrap(),
        "Steam Original Plugins\n"
    );
    assert_eq!(
        std::fs::read_to_string(&gog_real_file).unwrap(),
        "GOG Original Plugins\n"
    );
}

// ---------------------------------------------------------------------------
// Gate 9: Real file not existing before session deleted on restore
// ---------------------------------------------------------------------------

#[test]
fn real_file_not_existing_before_session_deleted_on_restore() {
    let harness = TestHarness::new();
    let def = test_skyrim_definition();

    let real_plugin_file = harness
        .local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt");
    assert!(!real_plugin_file.exists());

    let inst_dir = harness.instance_dir("instance-a");
    let inst_plugin = inst_dir.join("user").join("Plugins.txt");
    std::fs::create_dir_all(inst_plugin.parent().unwrap()).unwrap();
    std::fs::write(&inst_plugin, "Instance Initial Plugins\n").unwrap();

    // Swap in
    swap_in(
        &harness.ctx,
        "instance-a",
        &def,
        &StoreId::steam(),
        &harness.running_from,
    )
    .unwrap();

    // Real file was created with instance contents
    assert_eq!(
        std::fs::read_to_string(&real_plugin_file).unwrap(),
        "Instance Initial Plugins\n"
    );

    // Game writes new content
    std::fs::write(&real_plugin_file, "Game Created Content\n").unwrap();

    // Restore
    restore(&harness.ctx, &def, &StoreId::steam()).unwrap();

    // Real file that did not exist before the session is deleted on restore
    assert!(
        !real_plugin_file.exists(),
        "file must be deleted on restore if it did not exist before"
    );

    // And its session contents go to the instance
    assert_eq!(
        std::fs::read_to_string(&inst_plugin).unwrap(),
        "Game Created Content\n"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
