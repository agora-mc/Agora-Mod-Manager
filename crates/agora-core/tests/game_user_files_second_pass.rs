//! Second-pass review of the user-file swap fixes (c0a9e633, bfdb645e): proofs of bugs the fixes left or moved.
#![allow(unused_imports, dead_code)]
use std::path::PathBuf;
use std::sync::Mutex;

use agora_core::ctx::CoreContext;
use agora_core::game_user_files::{
    is_session_running, record_process, restore, status, swap_in, Journal, JournaledFile,
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
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("skyrim-se").unwrap(),
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
        native_code_patterns: vec![],
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
        plugin_list: None,
        runtime_files: Vec::new(),
        save_location: Vec::new(),
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

/// Put the disk in the state a stop leaves after the backup copy of entry 0 and before the journal
/// says so: the journal still carries its initial record (swapped=false, no backup recorded).
fn reset_journal_to_initial(jp: &PathBuf) {
    let mut j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(jp).unwrap()).unwrap();
    let f = &mut j["files"][0];
    f["swapped"] = serde_json::json!(false);
    f["existed"] = serde_json::json!(false);
    f["backup_path"] = serde_json::Value::Null;
    f["original_sha256"] = serde_json::Value::Null;
    f["written_sha256"] = serde_json::json!("");
    std::fs::write(jp, serde_json::to_vec(&j).unwrap()).unwrap();
}

#[test]
fn torn_backup_from_a_stop_during_the_copy_overwrites_the_players_intact_file() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let real = plugins_real(&h);
    let original = "original line one\noriginal line two\noriginal line three\n";
    put(&real, original);
    put(
        &h.instance_dir("sp-a").join("user").join("Plugins.txt"),
        "instance\n",
    );
    swap_in(&h.ctx, "sp-a", &def, &StoreId::steam(), &h.running_from).unwrap();

    // A stop during `fs::copy(real, backup)`: the real file is untouched (copy only reads it),
    // the backup holds a prefix, and the journal is still the initial record.
    std::fs::write(&real, original).unwrap();
    reset_journal_to_initial(&journal_path_of(&h, &def));
    std::fs::write(backup_dir_of(&h, &def).join("0"), &original[..10]).unwrap();

    let _ = restore(&h.ctx, &def, &StoreId::steam());

    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        original,
        "the player's real Plugins.txt was replaced by the half-copied backup"
    );
}

#[test]
fn stop_after_backup_before_journal_clobbers_the_instances_saved_copy() {
    let h = TestHarness::new();
    let def = test_skyrim_definition();
    let real = plugins_real(&h);
    put(&real, "original\n");
    let inst_copy = h.instance_dir("sp-b").join("user").join("Plugins.txt");
    put(&inst_copy, "instance profile\n");
    swap_in(&h.ctx, "sp-b", &def, &StoreId::steam(), &h.running_from).unwrap();

    // The real file still holds the original (the write comes after the journal), the backup is
    // complete, the journal is still the initial record.
    std::fs::write(&real, "original\n").unwrap();
    reset_journal_to_initial(&journal_path_of(&h, &def));

    restore(&h.ctx, &def, &StoreId::steam()).unwrap();

    assert_eq!(std::fs::read_to_string(&real).unwrap(), "original\n");
    assert_eq!(
        std::fs::read_to_string(&inst_copy).unwrap(),
        "instance profile\n",
        "the instance's saved copy was replaced by the player's original file"
    );
}

#[test]
fn mappings_sharing_an_instance_path_swap_in_but_can_never_be_restored() {
    let h = TestHarness::new();
    let mut def = test_skyrim_definition();
    // Second Steam mapping (Skyrim.ini) now uses the first one's instance path.
    let first = def.user_files[0].instance_path.clone();
    def.user_files[1].instance_path = first;
    let plugins = plugins_real(&h);
    let ini = ini_real(&h);
    put(&plugins, "orig-plugins\n");
    put(&ini, "orig-ini\n");
    put(
        &h.instance_dir("sp-c").join("user").join("Plugins.txt"),
        "instance\n",
    );
    let refused = swap_in(&h.ctx, "sp-c", &def, &StoreId::steam(), &h.running_from);

    assert!(
        refused.is_err(),
        "two mappings sharing one instance path were swapped in: {refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&plugins).unwrap(),
        "orig-plugins\n",
        "the player's Plugins.txt was touched by a refused duplicate mapping"
    );
    assert_eq!(
        std::fs::read_to_string(&ini).unwrap(),
        "orig-ini\n",
        "the player's Skyrim.ini was touched by a refused duplicate mapping"
    );
}
