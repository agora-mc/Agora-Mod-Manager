//! Proves the generic game paths with a second, deliberately differently shaped game.
//!
//! "Neon Courier" (see `common/neon_courier.rs`) has no plugin list, no INI files Agora edits,
//! two mod roots, save folders instead of save files, and its own executable name. Every path a
//! player touches is run through the same core functions the CLI and desktop call. A test that
//! fails because core assumes Skyrim is kept and marked `#[ignore = "second-game: ..."]`, so the
//! assumption is visible and reproducible; the report in `.spike/out/report.md` ranks them.

#![cfg(windows)]

#[path = "common/neon_courier.rs"]
#[allow(dead_code)]
mod neon_courier;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use agora_core::content_store;
use agora_core::crash_engine::attribution::from_instance;
use agora_core::crash_engine::instance_view::resolve_crash_instance;
use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{self, add_to_instance, AddToInstance, DeployMode, DeployOutcome};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_import::{self, Mo2Request, RunOptions};
use agora_core::game_instance::{
    self, create, get_manifest, CloneOptions, GameInstanceRecord, SavesChoice,
};
use agora_core::game_loadout;
use agora_core::game_registry::{GameInventory, IdentifiedInstall, RuntimeResolution};
use agora_core::snapshot_service::SnapshotService;
use agora_core::{game_ini, game_saves, instance_runtime, save_backup};
use agora_game_api::{
    BaseReference, GameDefinition, GameId, InstallId, InstallKind, RuntimeIdentity, StoreId,
};
use rusty_leveldb::{Options, DB};
use tempfile::TempDir;

/// `AGORA_TEST_USER_DATA_ROOT` is a process-wide setting; the binary runs its tests one at a time.
static USER_DATA: Mutex<()> = Mutex::new(());

struct Fixture {
    _lock: MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    install: IdentifiedInstall,
}

impl Fixture {
    fn new(def: GameDefinition) -> Self {
        let lock = USER_DATA.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user.path());

        let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let ctx = ctx.with_games(neon_courier::registry(def.clone()));

        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(install_dir.join("bin")).unwrap();
        std::fs::write(install_dir.join("bin/NeonCourier.exe"), b"neon courier").unwrap();
        std::fs::create_dir_all(install_dir.join("archive/mods")).unwrap();
        std::fs::create_dir_all(install_dir.join("bin/plugins")).unwrap();

        let install = install_of(&install_dir);
        Self {
            _lock: lock,
            _tmp: tmp,
            _user: user,
            ctx,
            def,
            install,
        }
    }

    fn create(&self, name: &str) -> GameInstanceRecord {
        create(
            &self.ctx,
            &self.install,
            &self.def,
            name,
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("the instance is created")
    }

    fn add(&self, instance: &str, name: &str, files: &[(&str, &[u8])]) -> AddToInstance {
        let dir = TempDir::new().unwrap();
        for (rel, bytes) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let outcome = content_store::add_folder(&self.ctx, dir.path(), Some(name)).unwrap();
        let item = outcome.item().item_id.clone();
        add_to_instance(&self.ctx, instance, &item).expect("the item is placed")
    }

    fn saves_root(&self) -> PathBuf {
        self._user
            .path()
            .join("documents")
            .join("Neon Courier")
            .join("Saves")
    }

    /// Two folder-saves, each a folder with `meta.json` and `data.bin`.
    fn write_folder_saves(&self) -> PathBuf {
        let root = self.saves_root();
        for name in ["slot-one", "slot-two"] {
            let folder = root.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("meta.json"), br#"{"name":"slot"}"#).unwrap();
            std::fs::write(folder.join("data.bin"), b"save bytes").unwrap();
        }
        root
    }
}

const CHARACTER: &str = "slot-one/data.bin";

struct SaveFixture {
    game: Fixture,
    source: String,
    private: [String; 2],
    characters: [PathBuf; 2],
}

impl SaveFixture {
    fn new(def: GameDefinition) -> Self {
        let game = Fixture::new(def);
        let source = game.create("Shared Courier").instance_id;
        let private = ["Amber Courier", "Indigo Courier"].map(|name| game.create(name).instance_id);
        game.write_folder_saves();
        let characters = private.each_ref().map(|id| {
            let change =
                game_saves::set_choice(&game.ctx, id, &game.def, SavesChoice::Own).unwrap();
            change.in_use.path.join(CHARACTER)
        });
        for (path, bytes) in characters.iter().zip([b"amber".as_slice(), b"indigo"]) {
            write(path, bytes);
        }
        Self {
            game,
            source,
            private,
            characters,
        }
    }

    fn character(&self, private: usize) -> &Path {
        &self.characters[private]
    }

    fn assert_private(&self) {
        assert_eq!(std::fs::read(self.character(0)).unwrap(), b"amber");
        assert_eq!(std::fs::read(self.character(1)).unwrap(), b"indigo");
    }

    fn source_dir(&self) -> PathBuf {
        self.game.ctx.paths.instance_dir(&self.source).unwrap()
    }

    fn clone(
        &self,
        name: &str,
        copy_saves: bool,
    ) -> Result<game_instance::ClonedInstance, game_instance::InstanceError> {
        game_instance::clone_instance(
            &self.game.ctx,
            &self.source,
            name,
            &self.game.def,
            CloneOptions { copy_saves },
        )
    }

    fn mark_running(&self) {
        let paths = &self.game.ctx.paths;
        let generation = instance_runtime::publish_starting(paths, &self.source).unwrap();
        instance_runtime::publish_running(
            paths,
            &self.source,
            &generation,
            agora_core::process_identity::capture(std::process::id()).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
    }
}

fn install_of(dir: &Path) -> IdentifiedInstall {
    let runtime = RuntimeIdentity {
        game: GameId::new(neon_courier::GAME_ID).unwrap(),
        store: StoreId::steam(),
        version: "1.0.0".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: StoreId::steam(),
        product: "990001".into(),
        name: neon_courier::MO2_NAME.into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: None,
        executables: vec!["bin/NeonCourier.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:990001").unwrap(),
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

fn mount_of(added: AddToInstance) -> String {
    match added {
        AddToInstance::Added { layer, .. } => layer.mount_path.as_str().to_string(),
        other => panic!("expected the item to be placed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Registration and instance lifecycle
// ---------------------------------------------------------------------------

#[test]
fn a_second_game_registers_from_its_package() {
    let f = Fixture::new(neon_courier::definition());
    let game = f
        .ctx
        .games
        .game(&GameId::new(neon_courier::GAME_ID).unwrap())
        .expect("the game is registered");
    assert_eq!(game.name, neon_courier::MO2_NAME);
    assert!(
        game.load_orders.is_empty(),
        "Neon Courier has no plugin list"
    );
    assert!(
        game.save_location.is_empty(),
        "Neon Courier has no save INI"
    );
    assert_eq!(game.save_paths.len(), 1, "its folder saves are declared");
}

#[test]
fn an_instance_is_created_for_the_second_game() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Courier Run");
    assert_eq!(inst.name, "Courier Run");
    let manifest = get_manifest(&f.ctx, &inst.instance_id).expect("the manifest reads");
    assert_eq!(manifest.game.as_str(), neon_courier::GAME_ID);
    assert!(matches!(manifest.base, BaseReference::Pinned { .. }));
}

#[test]
fn mods_are_added_to_both_roots() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Two Roots");

    let archive = f.add(
        &inst.instance_id,
        "ArchivePack",
        &[
            ("ArchivePack/archive.json", b"{}"),
            ("ArchivePack/pack.bin", b"archive"),
        ],
    );
    assert_eq!(mount_of(archive), "archive/mods/ArchivePack");

    let plugin = f.add(
        &inst.instance_id,
        "PluginPack",
        &[
            ("PluginPack/plugin.json", b"{}"),
            ("PluginPack/plug.dll", b"plugin"),
        ],
    );
    assert_eq!(mount_of(plugin), "bin/plugins/PluginPack");
}

#[test]
fn a_deploy_reaches_both_roots_in_a_temp_folder() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Deploy");
    f.add(
        &inst.instance_id,
        "ArchivePack",
        &[
            ("ArchivePack/archive.json", b"{}"),
            ("ArchivePack/pack.bin", b"archive"),
        ],
    );
    f.add(
        &inst.instance_id,
        "PluginPack",
        &[
            ("PluginPack/plugin.json", b"{}"),
            ("PluginPack/plug.dll", b"plugin"),
        ],
    );

    let plan = game_deploy::plan(&f.ctx, &inst.instance_id, &f.def, DeployMode::Links)
        .expect("the deploy plans");
    assert!(plan
        .files
        .iter()
        .any(|file| file.path.as_str() == "archive/mods/ArchivePack/pack.bin"));
    assert!(plan
        .files
        .iter()
        .any(|file| file.path.as_str() == "bin/plugins/PluginPack/plug.dll"));

    let outcome = game_deploy::deploy(&f.ctx, &inst.instance_id, &f.def, DeployMode::Links)
        .expect("the deploy runs");
    assert!(matches!(outcome, DeployOutcome::Built { .. }));
    let deployed = game_deploy::deployment_dir(&f.ctx, &inst.instance_id)
        .unwrap()
        .expect("a deployment folder");
    assert!(deployed.join("archive/mods/ArchivePack/pack.bin").is_file());
    assert!(deployed.join("bin/plugins/PluginPack/plug.dll").is_file());
}

#[test]
fn clone_copies_layers_but_not_saves() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Source");
    f.add(
        &inst.instance_id,
        "ArchivePack",
        &[("ArchivePack/archive.json", b"{}")],
    );

    let cloned = game_instance::clone_instance(
        &f.ctx,
        &inst.instance_id,
        "Copy",
        &f.def,
        game_instance::CloneOptions::default(),
    )
    .expect("the clone is made");
    let source = get_manifest(&f.ctx, &inst.instance_id).unwrap();
    let copy = get_manifest(&f.ctx, &cloned.instance_id).unwrap();
    assert_eq!(copy.layers, source.layers);
    assert_eq!(copy.saves, SavesChoice::Shared);
}

#[test]
fn delete_goes_to_recently_deleted_and_restores() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Trash Me");
    let entry = game_instance::delete(&f.ctx, &inst.instance_id)
        .expect("the delete")
        .trash_entry_id
        .expect("a trash entry");
    assert!(get_manifest(&f.ctx, &inst.instance_id).is_err());

    let restored =
        agora_core::game_instance::trash::restore(&f.ctx, &entry, None).expect("the restore");
    assert_eq!(restored.instance_id, inst.instance_id);
    assert_eq!(
        get_manifest(&f.ctx, &inst.instance_id).unwrap().name,
        "Trash Me"
    );
}

#[test]
fn a_loadout_saves_and_applies_the_layer_set() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Loadout");
    let added = f.add(
        &inst.instance_id,
        "ArchivePack",
        &[("ArchivePack/archive.json", b"{}")],
    );
    let item = match added {
        AddToInstance::Added { item_id, .. } => item_id,
        other => panic!("expected a layer, got {other:?}"),
    };

    game_loadout::create_game_profile(&f.ctx, &inst.instance_id, &f.def, "Everything On")
        .expect("the loadout is saved");
    game_deploy::set_content_enabled(&f.ctx, &inst.instance_id, &item, false)
        .expect("the layer is disabled");
    game_loadout::apply_game_profile_with_undo(
        &f.ctx,
        &inst.instance_id,
        &f.def,
        "Everything On",
        false,
    )
    .expect("the loadout applies");

    let manifest = get_manifest(&f.ctx, &inst.instance_id).unwrap();
    let layer = manifest
        .layers
        .layers()
        .iter()
        .find(|layer| matches!(&layer.source, agora_game_api::LayerSource::Content { content } if content == &item))
        .expect("the layer is there");
    assert!(layer.enabled, "the loadout turned the layer back on");
}

#[test]
fn a_snapshot_restores_the_instance_manifest() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Snapshot");
    let snapshots = SnapshotService::new(f.ctx.clone());
    let snapshot = snapshots
        .create(&inst.instance_id, Some("before"))
        .expect("the snapshot");

    let added = f.add(
        &inst.instance_id,
        "After Snapshot",
        &[("After/archive.json", b"{}")],
    );
    let added_id = match added {
        AddToInstance::Added { item_id, .. } => item_id,
        other => panic!("the item was not added: {other:?}"),
    };
    let names_it = |manifest: &agora_core::game_instance::GameInstanceManifest| {
        manifest
            .layers
            .layers()
            .iter()
            .any(|layer| matches!(&layer.source, agora_game_api::LayerSource::Content { content } if content == &added_id))
    };
    assert!(
        names_it(&get_manifest(&f.ctx, &inst.instance_id).unwrap()),
        "the layer added after the snapshot is in the manifest"
    );

    game_instance::rename(&f.ctx, &inst.instance_id, "Renamed").expect("the rename");
    snapshots
        .restore(&inst.instance_id, &snapshot.id)
        .expect("the restore");

    let restored = get_manifest(&f.ctx, &inst.instance_id).unwrap();
    assert!(
        !names_it(&restored),
        "the layer added after the snapshot is gone"
    );
    assert_eq!(restored.name, "Renamed", "a restore keeps the live name");
}

#[test]
fn the_crash_doctor_reads_an_instance_with_no_plugin_list() {
    let f = Fixture::new(neon_courier::definition());
    let inst = f.create("Crashed");
    f.add(
        &inst.instance_id,
        "ArchivePack",
        &[("ArchivePack/archive.json", b"{}")],
    );

    let view = from_instance(&f.ctx, &inst.instance_id).expect("attribution reads the instance");
    assert!(
        !view.layers().is_empty(),
        "the crash doctor saw the layers even without a plugin list"
    );

    let resolved = resolve_crash_instance(&f.ctx, &inst.instance_id, None)
        .expect("the crash view resolves without a plugin list");
    assert_eq!(resolved.game_id, neon_courier::GAME_ID);
    assert!(resolved.could_not_check.is_empty());
}

// ---------------------------------------------------------------------------
// Saves: the deliberately harder shape
// ---------------------------------------------------------------------------

#[test]
fn the_save_survey_counts_folder_saves() {
    let saves = SaveFixture::new(neon_courier::definition());
    let f = &saves.game;

    let status = game_saves::status(&f.ctx, &saves.source, &f.def)
        .expect("the save survey reads the game's declared save folder");
    assert_eq!(
        status.in_use.saves,
        Some(2),
        "both folder-saves are counted"
    );
    assert_eq!(status.in_use.path, f.saves_root());
    saves.assert_private();
}

#[test]
fn the_save_survey_does_not_require_the_ess_extension() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;

    let status = game_saves::status(&f.ctx, &saves.source, &f.def)
        .expect("the save survey reads the shared folder");
    assert_eq!(
        status.in_use.saves,
        Some(2),
        "both folder-saves are counted"
    );
    saves.assert_private();
}

#[test]
fn an_instance_can_own_and_share_its_saves() {
    let saves = SaveFixture::new(neon_courier::definition());
    let f = &saves.game;
    let id = &saves.source;

    let change = game_saves::set_choice(&f.ctx, id, &f.def, SavesChoice::Own)
        .expect("a game with folder saves can take saves of its own");
    assert_eq!(change.choice, SavesChoice::Own);
    let status = game_saves::status(&f.ctx, id, &f.def).expect("the status");
    assert_eq!(status.choice, SavesChoice::Own);
    assert_eq!(
        status.in_use.path,
        f.saves_root().join("Agora").join(id).join("characters")
    );
    assert!(status.in_use.path.is_dir());
    saves.assert_private();

    let shared = game_saves::set_choice(&f.ctx, id, &f.def, SavesChoice::Shared)
        .expect("the instance can go back to the shared saves");
    assert_eq!(shared.in_use.path, f.saves_root());
    saves.assert_private();
}

#[test]
fn package_registration_refuses_escaping_own_saves() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let mut def = saves.game.def.clone();
    def.save_location[0].own_value = r"Saves\Agora\{instance}\..\Indigo".into();
    let result = neon_courier::try_registry(def);
    saves.assert_private();
    assert!(matches!(
        result,
        Err(agora_core::game_registry::GameRegistryError::InvalidSaveLocation { .. })
    ));
}

#[test]
fn choosing_own_saves_validates_the_path_before_editing_the_ini() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    let mut def = f.def.clone();
    def.save_location[0].own_value = format!(r"Saves\Agora\{{instance}}\..\{}", saves.private[1]);
    let result = game_saves::set_choice(&f.ctx, &saves.source, &def, SavesChoice::Own);
    saves.assert_private();
    assert!(result.is_err(), "the unsafe path was accepted: {result:?}");
    assert!(!saves.source_dir().join("user/settings.ini").exists());
}

#[test]
fn a_shared_save_backup_never_copies_private_saves() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    let backup = save_backup::backup(&f.ctx, &saves.source, &f.def).unwrap();
    saves.assert_private();
    assert_eq!(backup.file_count, 4);
    assert!(!backup.path.join("files/Agora").exists());
}

#[test]
fn a_legacy_shared_save_backup_never_restores_private_saves() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    let backup = save_backup::backup(&f.ctx, &saves.source, &f.def).unwrap();
    let manifest = backup.path.join("backup.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let relative = format!("Agora/{}/{CHARACTER}", saves.private[1]);
    let files = stored["files"].as_array_mut().unwrap();
    files.retain(|file| file["path"] != relative);
    files.push(serde_json::json!({"path": relative, "size": 8}));
    let count = files.len();
    let bytes = files
        .iter()
        .map(|file| file["size"].as_u64().unwrap())
        .sum::<u64>();
    stored["file_count"] = count.into();
    stored["bytes"] = bytes.into();
    write(&backup.path.join("files").join(relative), b"outdated");
    write(&manifest, &serde_json::to_vec(&stored).unwrap());
    write(&f.saves_root().join(CHARACTER), b"changed shared save");

    let restored = save_backup::restore(&f.ctx, &saves.source, &f.def, &backup.id).unwrap();
    saves.assert_private();
    assert_eq!(restored.restored, 4);
    assert_eq!(
        std::fs::read(f.saves_root().join(CHARACTER)).unwrap(),
        b"save bytes"
    );
}

#[test]
fn save_restore_rejects_a_destination_owned_by_another_instance() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    let backup = save_backup::backup(&f.ctx, &saves.source, &f.def).unwrap();
    let manifest = backup.path.join("backup.json");
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    stored["source"] = f
        .saves_root()
        .join("Agora")
        .join(&saves.private[1])
        .to_str()
        .unwrap()
        .into();
    write(&manifest, &serde_json::to_vec(&stored).unwrap());

    let result = save_backup::restore(&f.ctx, &saves.source, &f.def, &backup.id);
    saves.assert_private();
    assert!(
        result.is_err(),
        "the foreign destination was accepted: {result:?}"
    );
}

#[test]
fn a_live_no_journal_session_refuses_save_backup_without_an_override() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    saves.mark_running();
    let backup = save_backup::backup(&f.ctx, &saves.source, &f.def);
    saves.assert_private();
    let error = backup.unwrap_err();
    assert!(error.to_string().contains("running"), "{error}");
    let backup = save_backup::backup_while_running(&f.ctx, &saves.source, &f.def, true).unwrap();
    assert_eq!(backup.file_count, 4);
    saves.assert_private();
}

#[test]
fn a_live_no_journal_session_refuses_a_save_clone() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    saves.mark_running();
    let clone = saves.clone("Active Copy", true).unwrap_err();
    saves.assert_private();
    assert!(
        matches!(clone, game_instance::InstanceError::Running(_)),
        "{clone:?}"
    );
}

#[test]
fn an_unrecognised_save_format_has_no_count() {
    let mut def = neon_courier::with_save_rule();
    def.save_location[0].recognition = None;
    let saves = SaveFixture::new(def);
    let f = &saves.game;
    let status = game_saves::status(&f.ctx, &saves.source, &f.def).unwrap();
    saves.assert_private();
    assert!(serde_json::to_value(status).unwrap()["in_use"]["saves"].is_null());
}

#[test]
fn a_shared_save_clone_copies_only_eligible_shared_files() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let clone = saves.clone("Shared Copy", true).unwrap();
    let clone_folder = saves
        .game
        .saves_root()
        .join("Agora")
        .join(clone.instance_id);
    saves.assert_private();
    assert_eq!(
        std::fs::read(clone_folder.join(CHARACTER)).unwrap(),
        b"save bytes"
    );
    assert!(!clone_folder.join("Agora").exists());
}

#[test]
fn a_clone_copies_loadouts_without_their_undo_or_save_backup_history() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    write(
        &saves.source_dir().join(".agora_loadouts/undo/last.json"),
        b"undo",
    );
    write(
        &saves.source_dir().join(".agora_loadouts/profile.json"),
        b"profile",
    );
    write(
        &saves
            .source_dir()
            .join(".agora_save_backups/old/backup.json"),
        b"backup",
    );
    let clone = saves.clone("History Copy", false).unwrap();
    let dir = saves
        .game
        .ctx
        .paths
        .instance_dir(&clone.instance_id)
        .unwrap();
    saves.assert_private();
    assert_eq!(
        std::fs::read(dir.join(".agora_loadouts/profile.json")).unwrap(),
        b"profile"
    );
    assert!(!dir.join(".agora_loadouts/undo").exists());
    assert!(!dir.join(".agora_save_backups").exists());
}

#[test]
fn a_save_clone_never_follows_a_junction_to_private_saves() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    junction::create(
        saves.character(1).parent().unwrap(),
        saves.source_dir().join("linked"),
    )
    .unwrap();
    let result = saves.clone("Linked Copy", false);
    saves.assert_private();
    assert!(
        result.is_err(),
        "a private save was copied through the junction: {result:?}"
    );
}

#[test]
fn sharing_an_own_save_clone_restores_the_pre_isolation_setting() {
    let saves = SaveFixture::new(neon_courier::with_save_rule());
    let f = &saves.game;
    write(
        &saves.source_dir().join("user/settings.ini"),
        b"[Saves]\nPath=Saves\\\n",
    );
    let own = game_saves::set_choice(&f.ctx, &saves.source, &f.def, SavesChoice::Own).unwrap();
    write(&own.in_use.path.join(CHARACTER), b"private source");
    let clone = saves.clone("Own Copy", false).unwrap();
    let shared =
        game_saves::set_choice(&f.ctx, &clone.instance_id, &f.def, SavesChoice::Shared).unwrap();
    saves.assert_private();
    let ini = game_ini::read(&f.ctx, &clone.instance_id, &f.def, "user/settings.ini").unwrap();
    let value = ini.document.get("Saves", "Path").unwrap();
    assert_eq!(value, r"Saves\");
    let actual = f.saves_root().parent().unwrap().join(value);
    assert_eq!(shared.in_use.path, f.saves_root());
    assert_eq!(
        std::fs::read(actual.join(CHARACTER)).unwrap(),
        b"save bytes"
    );
    assert_eq!(
        std::fs::read(own.in_use.path.join(CHARACTER)).unwrap(),
        b"private source"
    );
}

// ---------------------------------------------------------------------------
// Imports: the importers accept the game by package, then have to place two roots
// ---------------------------------------------------------------------------

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn assert_mod_roots(f: &Fixture, instance_id: &str, archive: &str, plugin: &str) {
    let plan = game_deploy::plan(&f.ctx, instance_id, &f.def, DeployMode::Links)
        .expect("the imported instance plans");
    let files: Vec<_> = plan.files.iter().map(|file| file.path.as_str()).collect();
    assert!(
        files.contains(&format!("archive/mods/{archive}/pack.bin").as_str()),
        "{files:?}"
    );
    assert!(
        files.contains(&format!("bin/plugins/{plugin}/plug.dll").as_str()),
        "{files:?}"
    );
}

#[test]
fn an_mo2_setup_imports_mods_into_both_roots() {
    let f = Fixture::new(neon_courier::definition());
    let setup = f._tmp.path().join("mo2");
    write(
        &setup.join("ModOrganizer.ini"),
        format!(
            "[General]\r\ngameName={}\r\nselected_profile=@ByteArray(Neon)\r\ngamePath={}\r\n",
            neon_courier::MO2_NAME,
            f.install
                .discovered
                .location
                .to_string_lossy()
                .replace('\\', "\\\\")
        )
        .as_bytes(),
    );
    write(&setup.join("mods/ArchivePack/archive.json"), b"{}");
    write(&setup.join("mods/ArchivePack/pack.bin"), b"archive");
    write(&setup.join("mods/PluginPack/plugin.json"), b"{}");
    write(&setup.join("mods/PluginPack/plug.dll"), b"plugin");
    write(
        &setup.join("profiles/Neon/modlist.txt"),
        b"+ArchivePack\r\n+PluginPack\r\n",
    );

    let inventory = GameInventory {
        installs: vec![f.install.clone()],
        unsupported: vec![],
    };
    let request = Mo2Request {
        ini_path: setup.join("ModOrganizer.ini"),
        profile: "Neon".into(),
        name: None,
        copy_saves: false,
        install: None,
    };
    let report = game_import::run(&f.ctx, &inventory, &request, RunOptions::default(), &|_| {})
        .expect("the MO2 setup imports");
    let instance_id = report.instance_id.expect("an instance was made");

    assert_mod_roots(&f, &instance_id, "ArchivePack", "PluginPack");
}

fn write_store(store: &Path, pairs: &[(&str, String)]) {
    let mut db = DB::open(
        store,
        Options {
            create_if_missing: true,
            ..Options::default()
        },
    )
    .expect("the test store opens");
    for (key, value) in pairs {
        db.put(key.as_bytes(), value.as_bytes()).expect("a key");
    }
    db.flush().expect("the store flushes");
}

#[test]
fn a_vortex_setup_imports_mods_into_both_roots() {
    let f = Fixture::new(neon_courier::definition());
    let vortex = f._tmp.path().join("Vortex");
    let store = vortex.join("state.v2");
    std::fs::create_dir_all(&store).unwrap();
    let staging = vortex.join("neoncourier").join("mods");
    write(&staging.join("ArchivePack/archive.json"), b"{}");
    write(&staging.join("ArchivePack/pack.bin"), b"archive");
    write(&staging.join("PluginPack/plugin.json"), b"{}");
    write(&staging.join("PluginPack/plug.dll"), b"plugin");
    write(&staging.join("ArchivePack/shared.bin"), b"archive bytes");
    write(&staging.join("PluginPack/shared.bin"), b"plugin bytes");
    let game = &f.install.discovered.location;
    write(
        &game.join("archive/mods/vortex.deployment.json"),
        br#"{"files":[{"relPath":"shared.bin","target":"Archive Pack","source":"ArchivePack"}]}"#,
    );
    write(
        &game.join("bin/plugins/vortex.deployment.json"),
        br#"{"files":[{"relPath":"shared.bin","target":"Plugin Pack","source":"PluginPack"}]}"#,
    );
    std::fs::create_dir_all(vortex.join("neoncourier/profiles/p1")).unwrap();

    let mods = r#"{"neoncourier":{
  "mod-archive": {"id":"mod-archive","state":"installed","installationPath":"ArchivePack","attributes":{"name":"Archive Pack"}},
  "mod-plugin": {"id":"mod-plugin","state":"installed","installationPath":"PluginPack","attributes":{"name":"Plugin Pack"}}
}}"#;
    let profiles = r#"{"p1":{"id":"p1","gameId":"neoncourier","name":"Main","modState":{"mod-archive":{"enabled":true},"mod-plugin":{"enabled":true}},"features":{}}}"#;
    write_store(
        &store,
        &[
            ("persistent###mods", mods.to_string()),
            ("persistent###profiles", profiles.to_string()),
            (
                "settings###mods###installPath###neoncourier",
                staging.to_string_lossy().replace('\\', "/"),
            ),
            (
                "settings###gameMode###discovered###neoncourier",
                format!(
                    r#"{{"path":"{}"}}"#,
                    f.install
                        .discovered
                        .location
                        .to_string_lossy()
                        .replace('\\', "/")
                ),
            ),
        ],
    );

    let inventory = GameInventory {
        installs: vec![f.install.clone()],
        unsupported: vec![],
    };
    let request = game_import::vortex::VortexRequest {
        store,
        game: GameId::new(neon_courier::GAME_ID).unwrap(),
        profile: "Main".into(),
        name: None,
        install: None,
        add_folder: false,
        copy_saves: false,
        all_mods: false,
    };
    let report =
        game_import::vortex::run(&f.ctx, &inventory, &request, RunOptions::default(), &|_| {})
            .expect("the Vortex setup imports");
    let instance_id = report.instance_id.expect("an instance was made");

    assert!(
        report.plan.fidelity.is_empty(),
        "{:?}",
        report.plan.fidelity
    );
    assert_eq!(report.plan.manifest.matched, 2);
    assert_mod_roots(&f, &instance_id, "Archive Pack", "Plugin Pack");
}
