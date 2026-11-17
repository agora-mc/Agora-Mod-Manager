//! Pre-merge data-safety review (review/fable): proofs of the top findings. Each test states the
//! behaviour the design promises and fails on the PR head. Nothing here is a fix.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use agora_core::ctx::CoreContext;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_loadout::{self, GameLoadout, GameLoadoutIni};
use agora_core::game_user_files::{self, restore, status, swap_in};
use agora_core::save_backup;
use agora_game_api::{BaseReference, GameId, InstallId, RuntimeIdentity, StoreId};
use tempfile::TempDir;

mod common;

use common::skyrim_definition as skyrim;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    _lock: MutexGuard<'static, ()>,
    data: TempDir,
    user: TempDir,
    ctx: CoreContext,
    running_from: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());
        let ctx = CoreContext::for_testing(data.path().join("app_data"));
        let _ = agora_core::db::init_local_state_db(&ctx.paths.local_state_db());
        let running_from = data.path().join("running_from");
        std::fs::create_dir_all(&running_from).unwrap();
        Self {
            _lock: lock,
            data,
            user,
            ctx,
            running_from,
        }
    }

    fn real_plugins(&self) -> PathBuf {
        self.user
            .path()
            .join("local")
            .join("Skyrim Special Edition")
            .join("Plugins.txt")
    }

    fn shared_saves(&self) -> PathBuf {
        self.user
            .path()
            .join("documents")
            .join("My Games")
            .join("Skyrim Special Edition")
            .join("Saves")
    }

    fn instance_dir(&self, id: &str) -> PathBuf {
        self.ctx.paths.instance_dir(id).unwrap()
    }

    fn make_instance(&self, id: &str) {
        let dir = self.instance_dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        let game = GameId::new("skyrim-se").unwrap();
        let runtime = RuntimeIdentity {
            game: game.clone(),
            store: StoreId::steam(),
            version: "1.6.1170.0".into(),
            build: None,
        };
        let mut manifest = GameInstanceManifest::new(
            game,
            id,
            id,
            Some(runtime),
            BaseReference::Unpinned {
                install: InstallId::new("steam:489830").unwrap(),
                reason: "test".into(),
            },
        );
        manifest.saves = SavesChoice::Shared;
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

fn put(path: &PathBuf, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// `game_user_files::restore` lets "the process that started the launch" restore a session that
/// is still starting, and tells that process by pid. The desktop is one long-lived process: its
/// exit-watcher thread for the previous session (`games/instances.rs`, `end_session`) and its
/// Restore command (`games/user_files.rs`) share the pid of the thread that is between `swap_in`
/// and the spawn of the next launch. Either one puts the player's files back under that launch;
/// the game then starts on the player's real Plugins.txt/INIs with no journal, and
/// `record_process` returns Ok on the missing journal, so nothing reports it.
#[test]
fn a_restore_in_the_launching_process_must_not_undo_a_launch_that_is_still_starting() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("starting");
    put(&h.real_plugins(), b"the player's list\n");
    put(
        &h.instance_dir("starting").join("user").join("Plugins.txt"),
        b"the instance's list\n",
    );

    // A launch has swapped in and has not spawned the game yet.
    swap_in(&h.ctx, "starting", &def, &StoreId::steam(), &h.running_from).unwrap();
    assert_eq!(
        std::fs::read(h.real_plugins()).unwrap(),
        b"the instance's list\n"
    );
    assert!(
        status(&h.ctx, &def.id, &StoreId::steam())
            .unwrap()
            .unwrap()
            .running,
        "a launch between the swap and the spawn counts as running"
    );

    // Another thread of the same process (the previous session's exit watcher, or the Restore
    // button) restores now.
    let restored = restore(&h.ctx, &def, &StoreId::steam());

    assert!(
        restored.is_err(),
        "a restore undid a launch that had not spawned its game yet"
    );
    assert_eq!(
        std::fs::read(h.real_plugins()).unwrap(),
        b"the instance's list\n",
        "the game is about to start on the player's real file, with no journal to put it back"
    );
}

/// `save_backup::restore` says it refuses while the game is running, but it asks
/// `instance_runtime::check_idle`, a lease only the Minecraft launch publishes. A game instance
/// never has one, so the restore renames and replaces saves under a live game.
#[test]
fn a_save_restore_is_refused_while_the_instances_game_session_is_running() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");
    put(&h.shared_saves().join("Run.ess"), b"the save at backup time");
    let backup = save_backup::backup(&h.ctx, "live", &def).unwrap();

    // The game is launched (the session counts as running: the same check loadouts and stop use)
    // and writes a newer save.
    put(&h.real_plugins(), b"the player's list\n");
    swap_in(&h.ctx, "live", &def, &StoreId::steam(), &h.running_from).unwrap();
    assert!(
        status(&h.ctx, &def.id, &StoreId::steam())
            .unwrap()
            .unwrap()
            .running
    );
    put(&h.shared_saves().join("Run.ess"), b"the save the game holds");

    let restored = save_backup::restore(&h.ctx, "live", &def, &backup.id);

    // Clean up the session before asserting, so a failure leaves no swapped files behind.
    let _ = game_user_files::finish_starting(&h.ctx, &def.id, &StoreId::steam());
    let _ = restore(&h.ctx, &def, &StoreId::steam());

    assert!(
        restored.is_err(),
        "saves were restored while the instance's session was running"
    );
    assert_eq!(
        std::fs::read(h.shared_saves().join("Run.ess")).unwrap(),
        b"the save the game holds"
    );
}

/// A session that ends with the game's file being a strict prefix of what Agora wrote (the game
/// dropped the last plugin from Plugins.txt, or emptied a file) is taken for a torn write, and the
/// game's change is not kept in the instance's copy. Nothing says so.
#[test]
fn a_list_the_game_shortened_is_kept_as_the_instances_copy() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("shorter");
    let copy = h.instance_dir("shorter").join("user").join("Plugins.txt");
    put(&h.real_plugins(), b"the player's list\n");
    put(&copy, b"*A.esp\n*B.esp\n");
    swap_in(&h.ctx, "shorter", &def, &StoreId::steam(), &h.running_from).unwrap();

    // The game ran and removed the last plugin from its list.
    put(&h.real_plugins(), b"*A.esp\n");

    game_user_files::finish_starting(&h.ctx, &def.id, &StoreId::steam()).unwrap();
    restore(&h.ctx, &def, &StoreId::steam()).unwrap();

    assert_eq!(
        std::fs::read(h.real_plugins()).unwrap(),
        b"the player's list\n"
    );
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        b"*A.esp\n",
        "the game's shortened list was dropped as if it were a torn write"
    );
}

/// A loadout's INI entry is joined to the instance folder as it is written in the profile file:
/// no check that it is one of the game's per-user files, or that it stays inside the instance.
/// Applying a profile writes, or with no contents deletes, any file the path reaches.
#[test]
fn a_loadout_ini_entry_cannot_reach_outside_the_instance() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("reach");
    let app_data = h.data.path().join("app_data");
    let written = app_data.join("written-by-loadout.txt");
    let victim = app_data.join("victim.txt");
    put(&victim, b"a file that is not the instance's");

    let state = GameLoadout {
        inis: vec![
            GameLoadoutIni {
                file: "../../written-by-loadout.txt".into(),
                contents: Some(b"outside".to_vec()),
            },
            GameLoadoutIni {
                file: "../../victim.txt".into(),
                contents: None,
            },
        ],
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, "reach", "reach", state).unwrap();

    let applied = game_loadout::apply_game_profile_with_undo(&h.ctx, "reach", &def, "reach");

    assert!(
        !written.exists(),
        "applying a loadout wrote a file outside the instance (apply returned {applied:?})"
    );
    assert!(
        victim.exists(),
        "applying a loadout deleted a file outside the instance"
    );
}
