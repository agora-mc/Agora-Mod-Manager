//! One answer to "is this instance's game running", shared by save restore, loadout apply,
//! redeploy and a second view/wrap. A Linux view session and a game with no journaled files leave
//! only a session-ledger row; an unreadable store is "could not tell", never "not running".

use std::sync::{Arc, Mutex, MutexGuard};

use agora_core::ctx::CoreContext;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_loadout::{self, GameLoadout, GameLoadoutIni};
use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_core::save_backup;
use agora_core::session_ledger::{self, LaunchManifest, Owner};
use agora_game_api::{
    BaseReference, GameDefinition, GameId, GamePackage, InstallId, PackageDefinition,
    RuntimeIdentity, StoreId,
};
use tempfile::TempDir;

mod common;

use common::skyrim_definition as skyrim;

static TEST_LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    _lock: MutexGuard<'static, ()>,
    _data: TempDir,
    user: TempDir,
    ctx: CoreContext,
}

impl Harness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());
        let ctx =
            CoreContext::for_testing(data.path().join("app_data")).with_games(registry(&skyrim()));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        Self {
            _lock: lock,
            _data: data,
            user,
            ctx,
        }
    }

    fn make_instance(&self, id: &str) {
        let dir = self.ctx.paths.instance_dir(id).unwrap();
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
        let base_json = serde_json::to_string(&manifest.base).unwrap();
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let conn =
            agora_core::db::local_state_connection(&self.ctx.paths.local_state_db()).unwrap();
        conn.execute(
            "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, last_launched_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), NULL)",
            rusqlite::params![id, "skyrim-se", id, base_json],
        )
        .unwrap();
    }

    fn shared_saves(&self) -> std::path::PathBuf {
        self.user
            .path()
            .join("documents")
            .join("My Games")
            .join("Skyrim Special Edition")
            .join("Saves")
    }

    /// Open a session the way a Linux view does: only the ledger row, no user-files journal. The
    /// owner is this live process, so the session has not ended.
    fn open_view_session(&self, instance_id: &str) {
        let conn =
            agora_core::db::local_state_connection(&self.ctx.paths.local_state_db()).unwrap();
        session_ledger::begin(
            &conn,
            instance_id,
            "2026-01-01T00:00:00Z",
            &LaunchManifest::default(),
            Owner::current(),
        )
        .unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn registry(def: &GameDefinition) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    let package = PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: Default::default(),
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

fn put(path: &std::path::Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A loadout apply used to read only the user-files journal, so a session with no journaled files
/// (every game that declares none) looked idle and the apply rewrote the instance under a live
/// game. The ledger's open session with a live owner is that launch's record.
#[test]
fn a_loadout_apply_is_refused_by_an_open_session_with_no_journaled_files() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");
    put(
        &h.ctx
            .paths
            .instance_dir("live")
            .unwrap()
            .join("user/Skyrim.ini"),
        b"[General]\nValue=1\n",
    );
    let state = GameLoadout {
        inis: vec![GameLoadoutIni {
            file: "user/Skyrim.ini".into(),
            contents: Some(b"[General]\nValue=2\n".to_vec()),
        }],
        ..GameLoadout::default()
    };
    game_loadout::save_game_profile(&h.ctx, "live", "base", state).unwrap();
    h.open_view_session("live");

    let error = game_loadout::apply_game_profile_with_undo(&h.ctx, "live", &def, "base", false)
        .expect_err("an open session must refuse the apply");
    assert!(
        error.to_string().contains("running"),
        "the refusal says the game is running: {error}"
    );
}

/// A second `games wrap` for the same instance used to delete the live view's `view-work` and
/// rewrite the overlay's lower layer under its mount. The wrapper has not opened its own session
/// yet, so the first session's live owner is what refuses it.
#[test]
fn a_second_view_is_refused_by_the_first_ones_open_session() {
    let h = Harness::new();
    h.make_instance("live");
    h.open_view_session("live");
    agora_core::game_wrap::write_pending(
        &h.ctx,
        &agora_core::game_wrap::PendingLaunch {
            instance_id: "live".to_string(),
            game: "skyrim-se".to_string(),
            created_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
        },
    )
    .unwrap();

    let host = agora_plugin_host::QuickJsHost::new();
    let packs: &[agora_core::crash_packs::LoadedCrashPack] = &[];
    let built_in: &[agora_game_api::crash::BuiltInPack] = &[];
    let live = agora_core::crash_live::LivePacks {
        packs,
        built_in,
        host: &host,
        instances: None,
    };
    let command = vec!["agora-no-such-program".to_string()];

    let error = agora_core::game_wrap::wrap(&h.ctx, &live, &command)
        .expect_err("a second view of a live instance must refuse");
    assert!(
        error.to_string().contains("running"),
        "the refusal says the game is running: {error}"
    );
}

/// A journal that cannot be read is not "no session". A save restore must refuse and say it could
/// not tell, leaving the current save in place.
#[test]
fn a_save_restore_refuses_when_the_session_store_cannot_be_read() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");
    put(
        &h.shared_saves().join("Run.ess"),
        b"the save at backup time",
    );
    let backup = save_backup::backup(&h.ctx, "live", &def).unwrap();
    put(
        &h.shared_saves().join("Run.ess"),
        b"the save the game holds",
    );

    let journal = h.ctx.paths.user_files_journal_path("skyrim-se", "steam");
    put(&journal, b"{ this is not a journal");

    let error = save_backup::restore(&h.ctx, "live", &def, &backup.id)
        .expect_err("an unreadable store must refuse the restore");
    assert!(
        error.to_string().contains("could not tell"),
        "the refusal says it could not tell: {error}"
    );
    assert_eq!(
        std::fs::read(h.shared_saves().join("Run.ess")).unwrap(),
        b"the save the game holds"
    );
}

/// A `SavesChoice` is not needed for the activity answer; the instance's own record and manifest
/// name the game and store. This pins the one core entry point the four callers share.
#[test]
fn the_activity_answer_is_idle_with_no_records_and_running_with_a_live_lease() {
    let h = Harness::new();
    h.make_instance("demo");
    assert_eq!(
        agora_core::instance_runtime::game_activity(&h.ctx, "demo"),
        agora_core::instance_runtime::GameActivity::Idle
    );

    let _generation = agora_core::instance_runtime::publish_starting(&h.ctx.paths, "demo").unwrap();
    assert_eq!(
        agora_core::instance_runtime::game_activity(&h.ctx, "demo"),
        agora_core::instance_runtime::GameActivity::Running
    );
}

/// Editing an INI under a live game is exactly what the one guard is for: the game rewrites the
/// file when it saves. `--while-running` is the explicit override.
#[test]
fn an_ini_edit_is_refused_by_a_live_session_unless_overridden() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");
    put(
        &h.ctx
            .paths
            .instance_dir("live")
            .unwrap()
            .join("user/Skyrim.ini"),
        b"[General]\nValue=1\n",
    );
    h.open_view_session("live");

    let refused = agora_core::game_ini::set_value_while_running(
        &h.ctx,
        "live",
        &def,
        "user/Skyrim.ini",
        "General",
        "Value",
        "2",
        false,
    )
    .expect_err("a live session must refuse the edit");
    assert!(refused.to_string().contains("running"), "{refused}");

    agora_core::game_ini::set_value_while_running(
        &h.ctx,
        "live",
        &def,
        "user/Skyrim.ini",
        "General",
        "Value",
        "2",
        true,
    )
    .expect("the override must allow the edit");
}

/// Reordering the plugin list under a live game is refused, and allowed only with the override.
#[test]
fn a_plugin_list_edit_is_refused_by_a_live_session_unless_overridden() {
    use agora_game_api::{
        LoadOrderDefinition, LoadOrderFormat, LoadOrderItems, LoadOrderRules, RelPath,
    };

    let h = Harness::new();
    let mut def = skyrim();
    def.load_orders = vec![LoadOrderDefinition {
        id: "plugins".into(),
        name: "Plugins".into(),
        file: RelPath::new("user/Plugins.txt").unwrap(),
        destination: Default::default(),
        items: LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esp".into()],
            folders: false,
        },
        format: LoadOrderFormat::Lines {
            active_prefix: "*".into(),
            header: vec![],
        },
        rules: LoadOrderRules {
            kind: "none".into(),
            entry: None,
            implicit: Vec::new(),
            implicit_list_file: None,
            sorter: None,
            overlaps: None,
        },
        intro: None,
        item_heading: None,
    }];
    h.make_instance("live");
    h.open_view_session("live");
    let lines = [("A.esp".to_string(), true), ("B.esp".to_string(), true)];

    let refused = agora_core::game_plugins::import_list_in_while_running(
        &h.ctx,
        "live",
        &def,
        None,
        &lines,
        &[],
        &[],
        false,
    )
    .expect_err("a live session must refuse the reorder");
    assert!(refused.to_string().contains("running"), "{refused}");

    agora_core::game_plugins::import_list_in_while_running(
        &h.ctx,
        "live",
        &def,
        None,
        &lines,
        &[],
        &[],
        true,
    )
    .expect("the override must allow the reorder");
}

/// Undeploying under a live game is refused, and allowed only with the override. An unpinned
/// instance has nothing to undeploy, so the override reaches that answer instead of the guard.
#[test]
fn an_undeploy_is_refused_by_a_live_session_unless_overridden() {
    let h = Harness::new();
    h.make_instance("live");
    h.open_view_session("live");

    let refused = agora_core::game_deploy::undeploy_while_running(&h.ctx, "live", false)
        .expect_err("a live session must refuse the undeploy");
    assert!(refused.to_string().contains("running"), "{refused}");

    agora_core::game_deploy::undeploy_while_running(&h.ctx, "live", true)
        .expect("the override must get past the guard");
}

/// A caller that already holds the instance guard (the launch path) uses the `*_locked` form,
/// which must neither refuse nor re-take the non-re-entrant instance lock.
#[test]
fn the_launch_locked_path_does_not_retake_the_instance_lock() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");

    let guard = agora_core::instance_runtime::InstanceGuard::for_mutation(&h.ctx, "live", "launch")
        .unwrap();
    agora_core::game_plugins::sync_without_content_locked(&h.ctx, &guard, &def, &StoreId::steam())
        .unwrap();
    agora_core::game_deploy::set_deployment_locked(&h.ctx, &guard, None).unwrap();
}

/// Switching the folder a running game saves into is what the guard is for. The choice is
/// refused under a live session and allowed only with the explicit override.
#[test]
fn a_save_choice_is_refused_by_a_live_session_unless_overridden() {
    let h = Harness::new();
    let def = skyrim();
    h.make_instance("live");
    h.open_view_session("live");

    let refused = agora_core::game_saves::set_choice_while_running(
        &h.ctx,
        "live",
        &def,
        SavesChoice::Own,
        false,
    )
    .expect_err("a live session must refuse the save choice");
    assert!(refused.to_string().contains("running"), "{refused}");

    agora_core::game_saves::set_choice_while_running(&h.ctx, "live", &def, SavesChoice::Own, true)
        .expect("the override must allow the save choice");
}
