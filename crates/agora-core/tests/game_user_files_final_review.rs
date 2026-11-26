//! Final review of the multi-game branch: proofs of bugs in the per-user files swap.
#![allow(unused_imports, dead_code)]
use std::path::PathBuf;
use std::sync::Mutex;

use agora_core::ctx::CoreContext;
use agora_core::game_user_files::{
    finish_starting, is_session_running, record_process, restore, status, swap_in, Journal,
    JournaledFile,
};
use agora_core::process_identity;
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePath, RelPath, StoreId, StoreIdentifier,
    UserDataLocation, UserFileMapping, UserFileStrategy,
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
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        snapshot: None,
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

fn journal_path_of(h: &TestHarness, def: &GameDefinition) -> PathBuf {
    h.ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str())
}

fn backup_dir_of(h: &TestHarness, def: &GameDefinition) -> PathBuf {
    h.ctx
        .paths
        .user_files_backup_dir(def.id.as_str(), StoreId::steam().as_str())
}

fn plugins_real(h: &TestHarness) -> PathBuf {
    h.local_dir()
        .join("Skyrim Special Edition")
        .join("Plugins.txt")
}

fn ini_real(h: &TestHarness) -> PathBuf {
    h.documents_dir()
        .join("My Games")
        .join("Skyrim Special Edition")
        .join("Skyrim.ini")
}

fn put(path: &PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A journaled path whose parent is a junction to a folder that has since been
/// deleted cannot be resolved. The swap must refuse it and leave no journal,
/// backup or instance copy behind. Junctions need no administrator rights.
#[cfg(windows)]
#[test]
fn a_dangling_junction_above_a_journaled_file_refuses_the_swap() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();

    let parent = h.local_dir().join("Skyrim Special Edition");
    let target = h.user_data_dir.path().join("junction-target");
    std::fs::create_dir_all(h.local_dir()).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    junction::create(&target, &parent).unwrap();
    put(&parent.join("Plugins.txt"), "*Base.esp");
    assert!(parent.join("Plugins.txt").is_file());

    // Delete the junction's target, leaving the journaled path's parent
    // dangling. Nothing can say where the real file would be written.
    std::fs::remove_dir_all(&target).unwrap();
    assert!(
        !parent.join("Plugins.txt").exists(),
        "the junction is dangling"
    );

    let error = swap_in(
        &h.ctx,
        "dangling-junction",
        &def,
        &StoreId::steam(),
        &h.running_from,
    )
    .expect_err("a swap through a dangling junction must be refused");

    assert!(
        !journal_path_of(&h, &def).exists(),
        "no journal should survive a refused swap: {error}"
    );
    assert!(
        !backup_dir_of(&h, &def).join("0").exists(),
        "no backup should be written: {error}"
    );
    assert!(
        !h.instance_dir("dangling-junction")
            .join("user/Plugins.txt")
            .exists(),
        "no instance copy should be written: {error}"
    );
    assert!(
        std::fs::symlink_metadata(&parent).is_ok(),
        "the junction itself must be left alone"
    );
}

#[test]
fn crash_between_overwrite_and_journal_update_loses_the_users_file() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let real = plugins_real(&h);
    put(&real, "original\n");
    put(
        &h.instance_dir("final-a").join("user").join("Plugins.txt"),
        "instance\n",
    );
    swap_in(&h.ctx, "final-a", &def, &StoreId::steam(), &h.running_from).unwrap();
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "instance\n");

    // What the disk holds if the process dies after `fs::write(real)` but before the
    // per-file journal update: the backup exists, the real file is overwritten, and the
    // journal still carries its initial record (swapped=false, no backup recorded).
    let jp = journal_path_of(&h, &def);
    let mut j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&jp).unwrap()).unwrap();
    let f = &mut j["files"][0];
    f["swapped"] = serde_json::json!(false);
    f["existed"] = serde_json::json!(false);
    f["backup_path"] = serde_json::Value::Null;
    f["original_sha256"] = serde_json::Value::Null;
    f["written_sha256"] = serde_json::json!("");
    std::fs::write(&jp, serde_json::to_vec(&j).unwrap()).unwrap();
    assert!(backup_dir_of(&h, &def).join("0").exists());

    let _ = restore(&h.ctx, &def, &StoreId::steam());
    assert!(
        std::fs::read_to_string(&real).unwrap() == "original\n"
            || backup_dir_of(&h, &def).join("0").exists(),
        "the user's original Plugins.txt is gone from both its place and the backup"
    );
}

#[test]
fn rerun_of_an_interrupted_restore_overwrites_the_sessions_instance_copy() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let plugins = plugins_real(&h);
    let ini = ini_real(&h);
    put(&plugins, "orig-plugins\n");
    put(&ini, "orig-ini\n");
    let inst = h.instance_dir("final-b");
    put(&inst.join("user").join("Plugins.txt"), "inst-plugins\n");
    put(&inst.join("user").join("Skyrim.ini"), "inst-ini\n");
    swap_in(&h.ctx, "final-b", &def, &StoreId::steam(), &h.running_from).unwrap();

    // The game changes both files during the session.
    std::fs::write(&plugins, "session-plugins\n").unwrap();
    std::fs::write(&ini, "session-ini\n").unwrap();
    finish_starting(&h.ctx, &def.id, &StoreId::steam()).unwrap();

    // The second backup is unreadable as the original once (disk error, AV, ...), so restore
    // gets through entry 0 and stops on entry 1 with the journal kept.
    let bak1 = backup_dir_of(&h, &def).join("1");
    std::fs::write(&bak1, "garbage").unwrap();
    assert!(restore(&h.ctx, &def, &StoreId::steam()).is_err());
    assert_eq!(std::fs::read_to_string(&plugins).unwrap(), "orig-plugins\n");

    // The user repairs it and presses restore again.
    std::fs::write(&bak1, "orig-ini\n").unwrap();
    restore(&h.ctx, &def, &StoreId::steam()).unwrap();

    assert_eq!(
        std::fs::read_to_string(inst.join("user").join("Plugins.txt")).unwrap(),
        "session-plugins\n",
        "the instance's saved Plugins.txt was replaced by the user's original on the re-run"
    );
}

#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn read_only_user_file_blocks_restore_and_every_later_launch() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let ini = ini_real(&h);
    put(&ini, "orig-ini\n");
    // First launch of a new instance: there is no instance copy, so swap_in writes nothing.
    let mut perms = std::fs::metadata(&ini).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&ini, perms).unwrap();
    swap_in(&h.ctx, "final-c", &def, &StoreId::steam(), &h.running_from).unwrap();

    // The game ends without touching the file. Restore only has to put the same bytes back.
    finish_starting(&h.ctx, &def.id, &StoreId::steam()).unwrap();
    let restored = restore(&h.ctx, &def, &StoreId::steam());
    let next = swap_in(&h.ctx, "final-c", &def, &StoreId::steam(), &h.running_from);

    let mut perms = std::fs::metadata(&ini).unwrap().permissions();
    perms.set_readonly(false);
    let _ = std::fs::set_permissions(&ini, perms);
    assert!(
        restored.is_ok() && next.is_ok(),
        "restore of an unchanged read-only file failed ({:?}) and the next launch is refused ({:?})",
        restored.err().map(|e| e.to_string()),
        next.err().map(|e| e.to_string())
    );
}

/// Control for `rerun_of_an_interrupted_restore_overwrites_the_sessions_instance_copy`: an
/// uninterrupted restore keeps the session's content in the instance copy.
#[test]
fn control_uninterrupted_restore_keeps_the_session_in_the_instance_copy() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let plugins = plugins_real(&h);
    put(&plugins, "orig-plugins\n");
    let inst = h.instance_dir("final-d");
    put(&inst.join("user").join("Plugins.txt"), "inst-plugins\n");
    swap_in(&h.ctx, "final-d", &def, &StoreId::steam(), &h.running_from).unwrap();
    std::fs::write(&plugins, "session-plugins\n").unwrap();
    finish_starting(&h.ctx, &def.id, &StoreId::steam()).unwrap();
    restore(&h.ctx, &def, &StoreId::steam()).unwrap();
    assert_eq!(std::fs::read_to_string(&plugins).unwrap(), "orig-plugins\n");
    assert_eq!(
        std::fs::read_to_string(inst.join("user").join("Plugins.txt")).unwrap(),
        "session-plugins\n"
    );
}
