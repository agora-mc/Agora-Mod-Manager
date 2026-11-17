//! Applying a game-instance loadout applies its save choice for real: the game's INI names the
//! folder the loadout chose, not only the manifest field. Fixtures are invented.

#![cfg(windows)]

use std::sync::{Mutex, MutexGuard};

use agora_core::ctx::CoreContext;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_loadout::{self, GameLoadout};
use agora_core::game_saves;
use agora_game_api::{BaseReference, GameId, InstallId, RuntimeIdentity, StoreId};
use tempfile::TempDir;

mod common;

use common::skyrim_definition as skyrim;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    _lock: MutexGuard<'static, ()>,
    _data: TempDir,
    _user: TempDir,
    ctx: CoreContext,
    instance: &'static str,
}

impl Harness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());
        let ctx = CoreContext::for_testing(data.path().join("app_data"));
        let _ = agora_core::db::init_local_state_db(&ctx.paths.local_state_db());

        let instance = "save-choice";
        let dir = ctx.paths.instance_dir(instance).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let game = GameId::new("skyrim-se").unwrap();
        let runtime = RuntimeIdentity {
            game: game.clone(),
            store: StoreId::steam(),
            version: "1.6.1170.0".into(),
            build: None,
        };
        let manifest = GameInstanceManifest::new(
            game,
            instance,
            instance,
            Some(runtime),
            BaseReference::Unpinned {
                install: InstallId::new("steam:489830").unwrap(),
                reason: "test".into(),
            },
        );
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        Self {
            _lock: lock,
            _data: data,
            _user: user,
            ctx,
            instance,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

#[test]
fn applying_a_loadout_sets_the_saves_ini_to_the_loadouts_folder() {
    let h = Harness::new();
    let def = skyrim();
    let manifest = agora_core::game_instance::get_manifest(&h.ctx, h.instance).unwrap();
    assert_eq!(
        manifest.saves,
        SavesChoice::Shared,
        "starts on the shared saves"
    );

    let state = GameLoadout {
        saves: SavesChoice::Own,
        save_folder: Some("Own Saves".to_string()),
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, h.instance, "Own Saves", state).unwrap();
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Own Saves")
        .expect("the loadout applies");

    let rule = game_saves::rule_for(&def, &StoreId::steam()).expect("a save rule");
    let expected = game_saves::own_value_scoped(rule, h.instance, Some("Own Saves"));
    let status = game_saves::status(&h.ctx, h.instance, &def).expect("the saves status reads");
    assert_eq!(status.choice, SavesChoice::Own);
    assert_eq!(
        status.setting_value.as_deref(),
        Some(expected.as_str()),
        "the INI names the loadout's own save folder"
    );

    let read_back = agora_core::game_instance::get_manifest(&h.ctx, h.instance).unwrap();
    assert_eq!(read_back.saves, SavesChoice::Own);
}

#[test]
fn the_first_own_loadout_adopts_the_instances_existing_save_folder() {
    let h = Harness::new();
    let def = skyrim();
    let rule = game_saves::rule_for(&def, &StoreId::steam()).expect("a save rule");
    let existing = game_saves::own_folder(rule, h.instance).expect("the single own folder");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("Save1.ess"), b"THE PLAYER'S SAVE").unwrap();
    let mut manifest = agora_core::game_instance::get_manifest(&h.ctx, h.instance).unwrap();
    manifest.saves = SavesChoice::Own;
    manifest.save_scope = None;
    std::fs::write(
        h.ctx
            .paths
            .instance_dir(h.instance)
            .unwrap()
            .join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // The first own-saves loadout adopts the folder the player has been using.
    let first = game_loadout::create_game_profile(&h.ctx, h.instance, &def, "First Setup")
        .expect("the first loadout is created");
    assert_eq!(
        first.game.as_ref().unwrap().save_folder,
        None,
        "the first loadout records the instance's existing folder"
    );
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "First Setup")
        .expect("the first loadout applies");
    let status = game_saves::status(&h.ctx, h.instance, &def).expect("the saves status");
    assert_eq!(
        status.setting_value.as_deref(),
        Some(game_saves::own_value(rule, h.instance).as_str()),
        "the INI still names the existing folder"
    );
    assert_eq!(
        status.in_use.path, existing,
        "the game reads the existing folder"
    );
    assert_eq!(status.in_use.saves, 1, "the player's save is still listed");

    // The second loadout gets a new folder; the first folder and its save stay untouched.
    let second = game_loadout::create_game_profile(&h.ctx, h.instance, &def, "Second Setup")
        .expect("the second loadout is created");
    assert_eq!(
        second.game.as_ref().unwrap().save_folder.as_deref(),
        Some("Second Setup"),
        "a later loadout gets its own folder"
    );
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Second Setup")
        .expect("the second loadout applies");
    let second_folder =
        game_saves::own_folder_scoped(rule, h.instance, Some("Second Setup")).unwrap();
    let status = game_saves::status(&h.ctx, h.instance, &def).expect("the saves status");
    assert_eq!(status.in_use.path, second_folder);
    assert_ne!(existing, second_folder);
    assert!(
        existing.join("Save1.ess").is_file(),
        "the adopted folder's save was not moved"
    );

    // Going back to the shared saves after both own folders still restores the player's setting.
    let shared = GameLoadout {
        saves: SavesChoice::Shared,
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, h.instance, "Shared Setup", shared).unwrap();
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Shared Setup")
        .expect("the shared loadout applies");
    let status = game_saves::status(&h.ctx, h.instance, &def).expect("the saves status");
    assert_eq!(status.choice, SavesChoice::Shared);
    assert_eq!(
        status.setting_value, None,
        "the player's shared setting is back after two own folders"
    );
}

#[test]
fn a_shared_loadout_goes_back_to_the_shared_saves() {
    let h = Harness::new();
    let def = skyrim();
    let own = GameLoadout {
        saves: SavesChoice::Own,
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, h.instance, "Own Setup", own).unwrap();
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Own Setup")
        .expect("the own loadout applies");

    let shared = GameLoadout {
        saves: SavesChoice::Shared,
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, h.instance, "Shared Setup", shared).unwrap();
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Shared Setup")
        .expect("the shared loadout applies");

    let status = game_saves::status(&h.ctx, h.instance, &def).expect("the saves status");
    assert_eq!(status.choice, SavesChoice::Shared);
    assert_eq!(
        status.setting_value, None,
        "the game's own shared setting is back"
    );
}
