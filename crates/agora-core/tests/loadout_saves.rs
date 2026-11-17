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
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, h.instance, "Own Saves", state).unwrap();
    game_loadout::apply_game_profile_with_undo(&h.ctx, h.instance, &def, "Own Saves")
        .expect("the loadout applies");

    let rule = game_saves::rule_for(&def, &StoreId::steam()).expect("a save rule");
    let expected = game_saves::own_value(rule, h.instance);
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
