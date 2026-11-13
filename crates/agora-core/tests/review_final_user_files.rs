//! Final-review probes of the per-user file journal (MASTER_SPEC §26.5).

use std::path::PathBuf;
use std::sync::Mutex;

use agora_core::ctx::CoreContext;
use agora_core::game_user_files::{restore, swap_in, Journal, JournaledFile};
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePath, RelPath, StoreId, StoreIdentifier,
    UserDataLocation, UserFileMapping, UserFileStrategy,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    _lock: std::sync::MutexGuard<'static, ()>,
    _data_dir: TempDir,
    user_data_dir: TempDir,
    ctx: CoreContext,
    running_from: PathBuf,
}

impl Harness {
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

    fn plugins(&self) -> PathBuf {
        self.user_data_dir
            .path()
            .join("local")
            .join("Skyrim Special Edition")
            .join("Plugins.txt")
    }

    fn ini(&self) -> PathBuf {
        self.user_data_dir
            .path()
            .join("documents")
            .join("My Games")
            .join("Skyrim Special Edition")
            .join("Skyrim.ini")
    }

    fn instance_file(&self, instance: &str, name: &str) -> PathBuf {
        let dir = self.ctx.paths.instances_root().join(instance).join("user");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

fn put(path: &PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn definition() -> GameDefinition {
    let mapping = |location, path: &str, instance: &str| UserFileMapping {
        source: GamePath::UserData {
            location,
            path: RelPath::new(path).unwrap(),
        },
        instance_path: RelPath::new(instance).unwrap(),
        strategy: UserFileStrategy::JournaledSwap,
        stores: vec![StoreId::steam()],
    };
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("skyrim-se").unwrap(),
        extends: None,
        name: "Skyrim SE".to_string(),
        stores: vec![StoreIdentifier {
            store: StoreId::steam(),
            product: "489830".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: None,
        log_paths: vec![],
        crash_paths: vec![],
        user_files: vec![
            mapping(
                UserDataLocation::LocalAppData,
                "Skyrim Special Edition/Plugins.txt",
                "user/Plugins.txt",
            ),
            mapping(
                UserDataLocation::Documents,
                "My Games/Skyrim Special Edition/Skyrim.ini",
                "user/Skyrim.ini",
            ),
        ],
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

/// A restore that fails half way (the second file is held read-only, as a sharing lock would hold
/// it) leaves the journal in place, and a re-run is meant to finish the job. But the re-run treats
/// the first file's now-restored real content as "what the game left" and writes it over the
/// instance copy that the first attempt saved, so the game's changes to that file are lost.
#[test]
fn bug_a_second_restore_overwrites_the_instance_copy_the_first_one_saved() {
    let h = Harness::new();
    let def = definition();
    put(&h.plugins(), "Original Plugins\n");
    put(&h.ini(), "Original Ini\n");
    put(&h.instance_file("inst-a", "Plugins.txt"), "A Plugins\n");
    put(&h.instance_file("inst-a", "Skyrim.ini"), "A Ini\n");

    swap_in(&h.ctx, "inst-a", &def, &StoreId::steam(), &h.running_from).unwrap();
    // The game ran and changed both files.
    std::fs::write(h.plugins(), "GAME CHANGED PLUGINS\n").unwrap();
    std::fs::write(h.ini(), "GAME CHANGED INI\n").unwrap();

    // The second file cannot be written back.
    let mut perms = std::fs::metadata(h.ini()).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(h.ini(), perms).unwrap();
    let first = restore(&h.ctx, &def, &StoreId::steam());
    assert!(first.is_err(), "the read-only file must stop the restore");
    let saved = std::fs::read_to_string(h.instance_file("inst-a", "Plugins.txt")).unwrap();
    assert_eq!(
        saved, "GAME CHANGED PLUGINS\n",
        "the first attempt saved the game's plugins"
    );

    // The player closes whatever held the file; the re-run finishes.
    let mut perms = std::fs::metadata(h.ini()).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(h.ini(), perms).unwrap();
    restore(&h.ctx, &def, &StoreId::steam()).expect("the re-run restores");

    assert_eq!(
        std::fs::read_to_string(h.plugins()).unwrap(),
        "Original Plugins\n"
    );
    let saved = std::fs::read_to_string(h.instance_file("inst-a", "Plugins.txt")).unwrap();
    assert_eq!(
        saved, "GAME CHANGED PLUGINS\n",
        "the instance's copy of the game's changes was overwritten by the restored original"
    );
}

/// `swap_in` overwrites the real file and only then records `swapped: true` and the backup in the
/// journal (game_user_files.rs, the `std::fs::write(&real_path, ..)` before `write_journal`). Agora
/// killed between the two leaves exactly this state: the backup copy on disk, the real file
/// overwritten, and a journal that still says `swapped: false` with no backup. Restore skips an
/// unswapped entry and then deletes the whole backup folder, so the player's original is gone.
///
/// The state is built by hand: the window between the two statements cannot be hit from a test.
#[test]
fn bug_crash_between_overwrite_and_journal_loses_the_original_file() {
    let h = Harness::new();
    let def = definition();
    put(&h.plugins(), "Original Plugins\n");
    put(&h.instance_file("inst-a", "Plugins.txt"), "A Plugins\n");

    // What swap_in has done for file 0 when it dies: journal written up front...
    let backup_dir = h
        .ctx
        .paths
        .user_files_backup_dir(def.id.as_str(), StoreId::steam().as_str());
    std::fs::create_dir_all(&backup_dir).unwrap();
    // ...backup copied...
    std::fs::copy(h.plugins(), backup_dir.join("0")).unwrap();
    // ...real file overwritten, journal not yet updated.
    std::fs::write(h.plugins(), "A Plugins\n").unwrap();
    let journal = Journal {
        game: def.id.clone(),
        store: StoreId::steam(),
        instance_id: "inst-a".to_string(),
        created_at: "2026-11-01T00:00:00Z".to_string(),
        running_from: h.running_from.clone(),
        processes: vec![],
        starting_launcher: None,
        files: vec![
            JournaledFile {
                real_path: h.plugins(),
                instance_path: RelPath::new("user/Plugins.txt").unwrap(),
                instance_copy_path: h.instance_file("inst-a", "Plugins.txt"),
                existed: false,
                backup_path: None,
                original_sha256: None,
                written_sha256: String::new(),
                swapped: false,
                state: None,
                restored: false,
            },
            JournaledFile {
                real_path: h.ini(),
                instance_path: RelPath::new("user/Skyrim.ini").unwrap(),
                instance_copy_path: h.instance_file("inst-a", "Skyrim.ini"),
                existed: false,
                backup_path: None,
                original_sha256: None,
                written_sha256: String::new(),
                swapped: false,
                state: None,
                restored: false,
            },
        ],
    };
    let journal_path = h
        .ctx
        .paths
        .user_files_journal_path(def.id.as_str(), StoreId::steam().as_str());
    std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();

    // The next launch (or `restore`) recovers.
    let _ = restore(&h.ctx, &def, &StoreId::steam());

    let on_disk = std::fs::read_to_string(h.plugins()).unwrap_or_default();
    let backups_kept = backup_dir.join("0").exists();
    assert!(
        on_disk == "Original Plugins\n" || backups_kept,
        "the player's original Plugins.txt is nowhere: real file is {on_disk:?}, backup kept: {backups_kept} (hash was {})",
        sha("Original Plugins\n")
    );
}

/// Between `swap_in` and `record_process` (which comes after deploying a step-down, injecting the
/// VFS and spawning) the journal has no process, and `running_from` has none either, so the session
/// reads as "not running". A second launch (double click, a second window, `games user-files
/// restore`) then restores the first launch's files and starts its own, instead of being told the
/// game is starting.
#[test]
fn bug_a_second_launch_during_the_first_ones_spawn_restores_its_files() {
    let h = Harness::new();
    let def = definition();
    put(&h.plugins(), "Original Plugins\n");
    put(&h.instance_file("inst-a", "Plugins.txt"), "A Plugins\n");
    put(&h.instance_file("inst-b", "Plugins.txt"), "B Plugins\n");

    swap_in(&h.ctx, "inst-a", &def, &StoreId::steam(), &h.running_from).unwrap();
    assert_eq!(std::fs::read_to_string(h.plugins()).unwrap(), "A Plugins\n");
    // Launch A is now deploying a step-down / injecting / spawning: no process is recorded yet.

    let second = swap_in(&h.ctx, "inst-b", &def, &StoreId::steam(), &h.running_from);
    assert!(
        second.is_err(),
        "launch B was allowed while launch A had swapped its files in and not yet started"
    );
}

/// Two mappings that name the same per-user file (a definition typo, or a hostile one; the registry
/// does not check for it) are swapped one after the other: the second backs up what the first just
/// wrote, so restoring in order puts the first mapping's instance content back as the "original".
#[test]
fn bug_two_mappings_for_one_real_file_do_not_restore_the_original() {
    let h = Harness::new();
    let mut def = definition();
    let mut second = def.user_files[0].clone();
    second.instance_path = RelPath::new("user/Plugins-second.txt").unwrap();
    def.user_files = vec![def.user_files[0].clone(), second];
    put(&h.plugins(), "Original Plugins\n");
    put(&h.instance_file("inst-a", "Plugins.txt"), "A Plugins\n");
    put(
        &h.instance_file("inst-a", "Plugins-second.txt"),
        "A Second\n",
    );

    let refused = swap_in(&h.ctx, "inst-a", &def, &StoreId::steam(), &h.running_from);

    assert!(
        refused.is_err(),
        "two mappings for one real file were swapped in: {refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(h.plugins()).unwrap(),
        "Original Plugins\n",
        "the player's own Plugins.txt was touched by a refused duplicate mapping"
    );
    assert_eq!(
        std::fs::read_to_string(h.instance_file("inst-a", "Plugins.txt")).unwrap(),
        "A Plugins\n"
    );
}
