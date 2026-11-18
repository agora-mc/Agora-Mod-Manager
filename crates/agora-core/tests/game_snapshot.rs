//! Snapshots and restore for generic game instances (MASTER_SPEC §23, §26).
//!
//! The state is the instance's own: manifest, plugin lists, INI copies, loadout
//! store and load-after rules. Mod bytes stay in the content store, and a
//! snapshot that names an item keeps it from being removed.

use std::sync::{Arc, Mutex, MutexGuard};

use agora_core::backup;
use agora_core::content_store;
use agora_core::ctx::CoreContext;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_core::game_snapshot;
use agora_core::lkg::{self, LaunchOutcome};
use agora_core::snapshot::{self, SnapshotOrigin};
use agora_game_api::crash::SessionVerdict;
use agora_game_api::{
    BaseReference, ContentLayout, GameDefinition, GameId, GamePackage, InstallId, Layer, LayerId,
    LayerSource, LayerStack, LoadOrderDefinition, LoadOrderFormat, LoadOrderItems, LoadOrderRules,
    PackageDefinition, RelPath, RuntimeIdentity, StoreId,
};
use tempfile::TempDir;

mod common;

use common::skyrim_definition;

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The Minecraft scope the moved-out test uses; core no longer declares it.
const MINECRAFT: &[&str] = &["mods", "config", "saves", "instance_manifest.json"];

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

fn definition() -> GameDefinition {
    let mut def = skyrim_definition();
    def.content_layout = Some(ContentLayout {
        data_path: RelPath::new("Data").unwrap(),
        data_markers: Vec::new(),
        root_markers: Vec::new(),
        thunderstore_bepinex: false,
    });
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
    def
}

struct Harness {
    _lock: MutexGuard<'static, ()>,
    _data: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    id: String,
}

impl Harness {
    fn new() -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = TempDir::new().unwrap();
        let def = definition();
        let ctx = CoreContext::for_testing(data.path().join("app_data"))
            .with_games(registry(&def))
            .with_minecraft_snapshot_entries(MINECRAFT);
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let id = "skyrim-test".to_string();
        let harness = Self {
            _lock: lock,
            _data: data,
            ctx,
            def,
            id,
        };
        harness.write_manifest(&GameInstanceManifest::new(
            GameId::new("skyrim-se").unwrap(),
            &harness.id,
            "Test",
            Some(RuntimeIdentity {
                game: GameId::new("skyrim-se").unwrap(),
                store: StoreId::steam(),
                version: "1.6.1170.0".into(),
                build: None,
            }),
            BaseReference::Unpinned {
                install: InstallId::new("steam:489830").unwrap(),
                reason: "test".into(),
            },
        ));
        let conn =
            agora_core::db::local_state_connection(&harness.ctx.paths.local_state_db()).unwrap();
        conn.execute(
            "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, last_launched_at)
             VALUES (?1, 'skyrim-se', 'Test', ?2, datetime('now'), NULL)",
            rusqlite::params![
                harness.id,
                serde_json::to_string(&harness.manifest().base).unwrap()
            ],
        )
        .unwrap();
        harness
    }

    fn dir(&self) -> std::path::PathBuf {
        self.ctx.paths.instance_dir(&self.id).unwrap()
    }

    fn manifest(&self) -> GameInstanceManifest {
        let text = std::fs::read_to_string(self.dir().join("instance_manifest.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn write_manifest(&self, manifest: &GameInstanceManifest) {
        let dir = self.dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    fn write_user_file(&self, name: &str, bytes: &[u8]) {
        let path = self.dir().join("user").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn read_user_file(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir().join("user").join(name)).unwrap()
    }

    fn add_item(&self) -> String {
        let folder = self.dir().join("source-item");
        std::fs::create_dir_all(folder.join("Data")).unwrap();
        std::fs::write(folder.join("Data").join("Test.esp"), b"plugin bytes").unwrap();
        content_store::add_folder(&self.ctx, &folder, Some("Test item"))
            .unwrap()
            .item()
            .item_id
            .clone()
    }

    fn add_layer(&self, item_id: &str) {
        let mut manifest = self.manifest();
        let layer = Layer {
            id: LayerId::new("content:test").unwrap(),
            enabled: true,
            mount_path: RelPath::default(),
            source_path: RelPath::default(),
            source: LayerSource::Content {
                content: item_id.to_string(),
            },
            whiteouts: Vec::new(),
            own_copy: false,
        };
        manifest.layers = LayerStack::new(vec![layer]).unwrap();
        self.write_manifest(&manifest);
    }

    /// Drop every content layer from the live instance without touching the
    /// content store: the snapshot is then the only thing naming the item.
    fn clear_layers(&self) {
        let mut manifest = self.manifest();
        manifest.layers = LayerStack::default();
        self.write_manifest(&manifest);
    }

    fn set_rules(&self, rules: &[agora_core::game_load_order::LoadAfterRule]) {
        agora_core::game_load_order::replace_all_load_after_rules(
            &self.ctx,
            &self.id,
            &[("plugins".to_string(), rules.to_vec())],
        )
        .unwrap();
    }
}

#[test]
fn snapshot_then_restore_round_trips_state_including_rules() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    h.write_user_file("Plugins.txt", b"*A.esp\n*B.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=1\n");
    h.set_rules(&[agora_core::game_load_order::LoadAfterRule {
        item: "B.esp".into(),
        after: "A.esp".into(),
    }]);

    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    // Change everything the snapshot covers.
    let mut manifest = h.manifest();
    let mut layers = manifest.layers.layers().to_vec();
    layers[0].enabled = false;
    manifest.layers = LayerStack::new(layers).unwrap();
    manifest.saves = SavesChoice::Own;
    h.write_manifest(&manifest);
    h.write_user_file("Plugins.txt", b"*C.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=2\n");
    h.set_rules(&[]);

    let outcome = game_snapshot::restore(&h.ctx, &h.id, &h.def, &snap.id, false).unwrap();

    let restored = h.manifest();
    assert!(restored.layers.layers()[0].enabled);
    assert_eq!(restored.saves, SavesChoice::Shared);
    assert_eq!(h.read_user_file("Plugins.txt"), b"*A.esp\n*B.esp\n");
    assert_eq!(h.read_user_file("Skyrim.ini"), b"[General]\nValue=1\n");
    let rules = agora_core::game_load_order::load_after_rules(&h.ctx, &h.id, "plugins").unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].item, "B.esp");
    assert_eq!(rules[0].after, "A.esp");

    // The restore took an undo snapshot that puts the changed state back.
    assert!(!outcome.undo_snapshot_id.is_empty());
    let undo = snapshot::list_snapshots(&h.dir())
        .unwrap()
        .into_iter()
        .find(|s| s.id == outcome.undo_snapshot_id)
        .expect("undo snapshot was not created");
    assert!(undo
        .label
        .as_deref()
        .is_some_and(|label| label.starts_with("pre-restore-")));
    game_snapshot::restore(&h.ctx, &h.id, &h.def, &undo.id, false).unwrap();
    assert!(!h.manifest().layers.layers()[0].enabled);
    assert_eq!(h.manifest().saves, SavesChoice::Own);
    assert_eq!(h.read_user_file("Plugins.txt"), b"*C.esp\n");
    assert!(
        agora_core::game_load_order::load_after_rules(&h.ctx, &h.id, "plugins")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn restore_is_refused_while_running_and_allowed_with_the_override() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n");
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();
    h.write_user_file("Plugins.txt", b"*B.esp\n");

    let generation = agora_core::instance_runtime::publish_starting(&h.ctx.paths, &h.id).unwrap();
    let error = game_snapshot::restore(&h.ctx, &h.id, &h.def, &snap.id, false)
        .expect_err("a running game refuses the restore");
    assert!(error.to_string().contains("running"), "{error}");
    assert_eq!(
        error.code(),
        agora_core::game_snapshot::ERR_SNAPSHOT_NOT_IDLE
    );
    assert_eq!(h.read_user_file("Plugins.txt"), b"*B.esp\n");

    let outcome =
        game_snapshot::restore(&h.ctx, &h.id, &h.def, &snap.id, true).expect("override restores");
    assert!(outcome.forced_while_not_idle);
    assert_eq!(h.read_user_file("Plugins.txt"), b"*A.esp\n");
    agora_core::instance_runtime::clear(&h.ctx.paths, &h.id, Some(&generation)).unwrap();
}

#[test]
fn a_snapshot_that_names_an_item_keeps_it_from_being_removed() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();
    h.clear_layers();

    let error =
        content_store::remove_item(&h.ctx, &item).expect_err("a snapshot still needs the item");
    let message = format!("{error}");
    assert!(message.contains(&item), "{message}");
    assert!(message.contains("snapshot"), "{message}");
    assert!(
        message.contains("agora snapshots delete"),
        "names the delete command: {message}"
    );
    assert!(h.ctx.paths.content_item_path(&item).is_file());
}

#[test]
fn an_unreadable_snapshot_blocks_removal_and_is_listed() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();
    h.clear_layers();

    let snapshots = h.dir().join(".agora_snapshots");
    // Remove the readable snapshot that also names the item, so the unreadable
    // one is the only thing standing between the item and removal.
    for snapshot in snapshot::list_snapshots(&h.dir()).unwrap() {
        if !snapshot.is_unreadable() {
            snapshot::delete_snapshot(&h.dir(), &snapshot.id).unwrap();
        }
    }
    std::fs::create_dir_all(&snapshots).unwrap();
    std::fs::write(snapshots.join("broken.json"), b"{not json").unwrap();

    let listed = snapshot::list_snapshots(&h.dir()).unwrap();
    let broken = listed
        .iter()
        .find(|snapshot| snapshot.id == "broken")
        .expect("a damaged snapshot must be listed");
    assert!(broken.is_unreadable());
    assert!(broken
        .unreadable
        .as_deref()
        .is_some_and(|why| !why.is_empty()));

    let error = content_store::remove_item(&h.ctx, &item)
        .expect_err("an unreadable snapshot refuses removal");
    let content_store::ContentError::UnreadableSnapshots {
        message,
        snapshots: snapshot_refs,
        ..
    } = error
    else {
        panic!("expected the unreadable-snapshot refusal, got {error:?}");
    };
    assert!(message.contains("broken"), "{message}");
    assert!(
        message.contains("agora snapshots delete"),
        "names the delete command: {message}"
    );
    let broken_ref = snapshot_refs
        .iter()
        .find(|snapshot| snapshot.snapshot_id == "broken")
        .expect("the refusal names the snapshot");
    assert_eq!(broken_ref.instance_id, h.id);
    assert!(broken_ref.command.contains("agora snapshots delete"));
    assert!(h.ctx.paths.content_item_path(&item).is_file());

    // The explicit override proceeds and leaves the unreadable snapshot alone.
    content_store::remove_item_with_options(
        &h.ctx,
        &item,
        content_store::RemoveOptions {
            ignore_unreadable_snapshots: true,
        },
    )
    .expect("the explicit override removes the item");
    assert!(!h.ctx.paths.content_item_path(&item).is_file());
    assert!(snapshots.join("broken.json").exists());
}

#[test]
fn a_missing_content_object_refuses_the_restore_by_name() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    let parsed: content_store::ContentItem = serde_json::from_str(
        &std::fs::read_to_string(h.ctx.paths.content_item_path(&item)).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(h.ctx.paths.content_object_path(&parsed.files[0].sha256)).unwrap();

    let error = game_snapshot::restore(&h.ctx, &h.id, &h.def, &snap.id, false)
        .expect_err("a missing object refuses the restore");
    assert!(error.to_string().contains(&item), "{error}");
}

#[test]
fn content_changes_take_automatic_snapshots() {
    let h = Harness::new();
    let item = h.add_item();

    agora_core::game_deploy::add_content(&h.ctx, &h.id, &item, None, None).unwrap();
    let after_add = snapshot::list_snapshots(&h.dir()).unwrap();
    assert!(
        after_add
            .iter()
            .any(|snapshot| snapshot.label.as_deref() == Some("before-content-add")),
        "adding content must take a recovery point"
    );

    agora_core::game_deploy::remove_content(&h.ctx, &h.id, &item).unwrap();
    let after_remove = snapshot::list_snapshots(&h.dir()).unwrap();
    assert!(
        after_remove
            .iter()
            .any(|snapshot| snapshot.label.as_deref() == Some("before-content-remove")),
        "removing content must take a recovery point"
    );

    // Restore the pre-remove point: the item is back on the instance even
    // though it never left the content store.
    let before_remove = after_remove
        .into_iter()
        .find(|snapshot| snapshot.label.as_deref() == Some("before-content-remove"))
        .unwrap();
    game_snapshot::restore(&h.ctx, &h.id, &h.def, &before_remove.id, false).unwrap();
    assert!(h.manifest().layers.layers().iter().any(
        |layer| matches!(&layer.source, LayerSource::Content { content } if content == &item)
    ));
}

#[test]
fn a_minecraft_shaped_snapshot_still_restores() {
    let h = Harness::new();
    let dir = h.dir();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    std::fs::write(dir.join("mods").join("a.jar"), b"original").unwrap();
    std::fs::create_dir_all(dir.join("saves").join("world")).unwrap();
    std::fs::write(dir.join("saves").join("world").join("level.dat"), b"world").unwrap();

    let snap = snapshot::create_snapshot(&dir, Some("old-style"), MINECRAFT).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(
                dir.join(".agora_snapshots")
                    .join(format!("{}.json", snap.id))
            )
            .unwrap()
        )
        .unwrap()["schema_version"],
        4,
        "today's Minecraft snapshots are schema 4"
    );
    std::fs::write(dir.join("mods").join("a.jar"), b"changed").unwrap();
    let outcome = snapshot::restore_snapshot(&dir, &snap.id, MINECRAFT).unwrap();
    assert_eq!(
        std::fs::read(dir.join("mods").join("a.jar")).unwrap(),
        b"original"
    );
    assert!(outcome.restored_roots.iter().any(|root| root == "mods"));
}

// -- last-known-good ---------------------------------------------------------------------------

#[test]
fn a_successful_session_promotes_and_a_crash_or_short_run_does_not() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n");

    let point = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    assert!(point.created, "the first launch writes a recovery point");
    game_snapshot::record_launch_point(&h.ctx, &h.id, Some("s-1"), &point).unwrap();
    let outcome = game_snapshot::finish_launch_session(
        &h.ctx,
        &h.id,
        "s-1",
        Some(SessionVerdict::Normal),
        120_000,
    )
    .unwrap();
    assert_eq!(outcome, LaunchOutcome::Success);
    let state = lkg::read_lkg_state(&h.dir()).unwrap();
    assert_eq!(
        state.current_lkg_snapshot_id.as_deref(),
        Some(point.snapshot_id.as_str())
    );

    // A crash never replaces the setup that worked.
    h.write_user_file("Plugins.txt", b"*B.esp\n");
    let crash_point = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    game_snapshot::record_launch_point(&h.ctx, &h.id, Some("s-2"), &crash_point).unwrap();
    let outcome = game_snapshot::finish_launch_session(
        &h.ctx,
        &h.id,
        "s-2",
        Some(SessionVerdict::Crashed {
            how: agora_game_api::crash::MatchedBy::ExitStatus,
        }),
        120_000,
    )
    .unwrap();
    assert_eq!(outcome, LaunchOutcome::Crash);
    assert_eq!(
        lkg::read_lkg_state(&h.dir())
            .unwrap()
            .current_lkg_snapshot_id,
        Some(point.snapshot_id.clone())
    );

    // A clean exit before the minimum runtime is abandoned, not a success.
    let short_point = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    game_snapshot::record_launch_point(&h.ctx, &h.id, Some("s-3"), &short_point).unwrap();
    let outcome = game_snapshot::finish_launch_session(
        &h.ctx,
        &h.id,
        "s-3",
        Some(SessionVerdict::Normal),
        2_000,
    )
    .unwrap();
    assert_eq!(outcome, LaunchOutcome::Abandoned);
    assert_eq!(
        lkg::read_lkg_state(&h.dir())
            .unwrap()
            .current_lkg_snapshot_id,
        Some(point.snapshot_id)
    );
}

#[test]
fn an_unchanged_state_skips_the_prelaunch_snapshot() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n");

    let first = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    assert!(first.created);
    let count = snapshot::list_snapshots(&h.dir()).unwrap().len();

    let again = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    assert!(
        !again.created,
        "an unchanged state reuses the last snapshot"
    );
    assert_eq!(again.snapshot_id, first.snapshot_id);
    assert_eq!(snapshot::list_snapshots(&h.dir()).unwrap().len(), count);

    h.write_user_file("Skyrim.ini", b"[General]\nValue=1\n");
    let changed = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    assert!(changed.created, "a changed INI writes a new recovery point");
    assert_ne!(changed.snapshot_id, first.snapshot_id);
}

#[test]
fn changes_since_lkg_lists_a_reordered_plugin_and_a_changed_ini() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n*B.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=1\n");
    let point = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    game_snapshot::record_launch_point(&h.ctx, &h.id, Some("s-1"), &point).unwrap();
    game_snapshot::finish_launch_session(
        &h.ctx,
        &h.id,
        "s-1",
        Some(SessionVerdict::Normal),
        120_000,
    )
    .unwrap();

    h.write_user_file("Plugins.txt", b"*B.esp\n*A.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=2\n");

    let diff = game_snapshot::diff_since_lkg(&h.ctx, &h.id, &h.def).unwrap();
    let changed: Vec<&str> = diff
        .files
        .modified
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert!(
        changed.contains(&"user/Plugins.txt"),
        "the reordered plugin list is listed: {changed:?}"
    );
    assert!(
        changed.contains(&"user/Skyrim.ini"),
        "the changed INI is listed: {changed:?}"
    );
}

#[test]
fn restore_to_lkg_round_trips_files_and_rules() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n*B.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=1\n");
    h.set_rules(&[agora_core::game_load_order::LoadAfterRule {
        item: "B.esp".into(),
        after: "A.esp".into(),
    }]);
    let point = game_snapshot::prepare_launch(&h.ctx, &h.id, &h.def).unwrap();
    game_snapshot::record_launch_point(&h.ctx, &h.id, Some("s-1"), &point).unwrap();
    game_snapshot::finish_launch_session(
        &h.ctx,
        &h.id,
        "s-1",
        Some(SessionVerdict::Normal),
        120_000,
    )
    .unwrap();

    h.write_user_file("Plugins.txt", b"*C.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=2\n");
    h.set_rules(&[]);

    let outcome = game_snapshot::restore_to_lkg(&h.ctx, &h.id, &h.def, false).unwrap();
    assert!(!outcome.undo_snapshot_id.is_empty());
    assert_eq!(h.read_user_file("Plugins.txt"), b"*A.esp\n*B.esp\n");
    assert_eq!(h.read_user_file("Skyrim.ini"), b"[General]\nValue=1\n");
    let rules = agora_core::game_load_order::load_after_rules(&h.ctx, &h.id, "plugins").unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].item, "B.esp");
}

// -- backups ------------------------------------------------------------------------------------

fn fresh_context(def: &GameDefinition, root: &std::path::Path) -> CoreContext {
    let ctx = CoreContext::for_testing(root.join("app_data"))
        .with_games(registry(def))
        .with_minecraft_snapshot_entries(MINECRAFT);
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    ctx
}

fn snapshot_manifest_path(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    dir.join(".agora_snapshots").join(format!("{id}.json"))
}

#[test]
fn a_backup_without_bytes_imports_when_objects_exist_and_lists_missing_ones() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    h.write_user_file("Plugins.txt", b"*A.esp\n");
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    let export = TempDir::new().unwrap();
    let artifact = backup::export_game_snapshot(
        &h.dir(),
        &snap.id,
        export.path(),
        &h.ctx.paths.content_root(),
        false,
    )
    .unwrap();

    // Same data root: the content objects the backup names are still there.
    let fresh_dir = h.ctx.paths.instance_dir("fresh-copy").unwrap();
    let imported = backup::import_backup(&fresh_dir, &artifact).unwrap();
    assert_eq!(imported.id, snap.id);
    assert!(snapshot::list_snapshots(&fresh_dir)
        .unwrap()
        .iter()
        .any(|snapshot| snapshot.id == snap.id));
    assert!(h.ctx.paths.content_item_path(&item).is_file());

    // Remove the objects, then import into another instance: the whole import
    // refuses and names the item, writing no snapshot.
    let parsed: content_store::ContentItem = serde_json::from_str(
        &std::fs::read_to_string(h.ctx.paths.content_item_path(&item)).unwrap(),
    )
    .unwrap();
    for file in &parsed.files {
        let _ = std::fs::remove_file(h.ctx.paths.content_object_path(&file.sha256));
    }
    let missing_dir = h.ctx.paths.instance_dir("fresh-missing").unwrap();
    let error = backup::import_backup(&missing_dir, &artifact).unwrap_err();
    assert!(error.contains("needs content"), "{error}");
    assert!(
        error.contains("Test item"),
        "names the missing item: {error}"
    );
    assert!(
        !snapshot_manifest_path(&missing_dir, &snap.id).exists(),
        "a refused import writes no snapshot"
    );
}

#[test]
fn a_backup_with_bytes_round_trips_into_an_empty_data_folder() {
    let h = Harness::new();
    let item = h.add_item();
    h.add_layer(&item);
    h.write_user_file("Plugins.txt", b"*A.esp\n");
    h.write_user_file("Skyrim.ini", b"[General]\nValue=1\n");
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    let export = TempDir::new().unwrap();
    let artifact = backup::export_game_snapshot(
        &h.dir(),
        &snap.id,
        export.path(),
        &h.ctx.paths.content_root(),
        true,
    )
    .unwrap();

    let empty = TempDir::new().unwrap();
    let ctx = fresh_context(&h.def, empty.path());
    let id = "restored";
    let dir = ctx.paths.instance_dir(id).unwrap();
    let imported = backup::import_backup(&dir, &artifact).unwrap();
    assert_eq!(imported.id, snap.id);

    // Every content object the state names was written from the backup.
    let parsed: content_store::ContentItem =
        serde_json::from_str(&std::fs::read_to_string(ctx.paths.content_item_path(&item)).unwrap())
            .unwrap();
    for file in &parsed.files {
        assert!(
            ctx.paths.content_object_path(&file.sha256).is_file(),
            "content object {} was not restored",
            file.sha256
        );
    }

    // The state files themselves restore, including the manifest the empty
    // instance did not have.
    game_snapshot::restore(&ctx, id, &h.def, &imported.id, true).unwrap();
    assert_eq!(
        std::fs::read(dir.join("user").join("Plugins.txt")).unwrap(),
        b"*A.esp\n"
    );
    assert_eq!(
        std::fs::read(dir.join("user").join("Skyrim.ini")).unwrap(),
        b"[General]\nValue=1\n"
    );
    assert!(dir.join("instance_manifest.json").is_file());
}

#[test]
fn a_tampered_game_backup_is_refused() {
    let h = Harness::new();
    h.write_user_file("Plugins.txt", b"*A.esp\n");
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();
    let export = TempDir::new().unwrap();
    let artifact = backup::export_game_snapshot(
        &h.dir(),
        &snap.id,
        export.path(),
        &h.ctx.paths.content_root(),
        false,
    )
    .unwrap();

    let tampered_path = export.path().join("tampered.zip");
    {
        use std::io::{Read, Write};
        let file = std::fs::File::open(&artifact).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let out = std::fs::File::create(&tampered_path).unwrap();
        let mut writer = zip::ZipWriter::new(out);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().unwrap().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if name == "user/Plugins.txt" && !bytes.is_empty() {
                bytes[0] ^= 0xFF;
            }
            writer.start_file(&name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    let empty = TempDir::new().unwrap();
    let ctx = fresh_context(&h.def, empty.path());
    let dir = ctx.paths.instance_dir("victim").unwrap();
    let error = backup::import_backup(&dir, &tampered_path).unwrap_err();
    assert!(
        error.contains("hash mismatch") || error.contains("size mismatch"),
        "a tampered backup is refused: {error}"
    );
    assert!(!snapshot_manifest_path(&dir, &snap.id).exists());
}
