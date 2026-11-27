//! Snapshots and restore for generic game instances (MASTER_SPEC §23, §26).
//!
//! The state is the instance's own: manifest, plugin lists, INI copies, loadout
//! store and load-after rules. Mod bytes stay in the content store, and a
//! snapshot that names an item keeps it from being removed.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use agora_core::backup;
use agora_core::catalog_install::CatalogTransport;
use agora_core::content_store::{self, ContentSource};
use agora_core::ctx::CoreContext;
use agora_core::error::{LauncherError, LauncherResult};
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_core::game_snapshot;
use agora_core::github_release::{GitHubRelease, GitHubReleaseAsset};
use agora_core::lkg::{self, LaunchOutcome};
use agora_core::registry::{DownloadSource, GameCatalogItem};
use agora_core::snapshot::{self, SnapshotOrigin};
use agora_game_api::crash::SessionVerdict;
use agora_game_api::{
    BaseReference, ContentLayout, GameDefinition, GameId, GamePackage, InstallId, Layer, LayerId,
    LayerSource, LayerStack, LoadOrderDefinition, LoadOrderFormat, LoadOrderItems, LoadOrderRules,
    PackageDefinition, RelPath, RuntimeIdentity, StoreId,
};
use async_trait::async_trait;
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

/// A Minecraft game, registered so a Minecraft snapshot has the declaration core now reads.
fn minecraft_definition() -> GameDefinition {
    let mut def = skyrim_definition();
    def.id = GameId::minecraft();
    def.name = "Minecraft: Java Edition".into();
    def.stores = Vec::new();
    def.content_rules = Vec::new();
    def.load_orders = Vec::new();
    def.user_files = Vec::new();
    def.save_paths = Vec::new();
    def.save_location = Vec::new();
    def.ini_checks = Vec::new();
    def.declared_writes = Vec::new();
    def.snapshot = Some(
        agora_game_api::SnapshotScope::declared(MINECRAFT, None)
            .expect("the fixture's Minecraft scope is usable"),
    );
    def
}

fn registry(def: &GameDefinition) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    for def in [def.clone(), minecraft_definition()] {
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
    }
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
        let ctx = CoreContext::for_testing(data.path().join("app_data")).with_games(registry(&def));
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
            "INSERT INTO instances (instance_id, game, name, created_at, last_launched_at)
             VALUES (?1, 'skyrim-se', 'Test', datetime('now'), NULL)",
            rusqlite::params![harness.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO game_instance_bases (instance_id, base_json) VALUES (?1, ?2)",
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
    let ctx = CoreContext::for_testing(root.join("app_data")).with_games(registry(def));
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
    // A pinned base so the fresh machine can be deploy-planned after import.
    let base_id = "base-imported-test";
    let runtime = h.manifest().runtime_identity.unwrap();
    let mut manifest = h.manifest();
    manifest.base = BaseReference::Pinned {
        id: base_id.to_string(),
        runtime: runtime.clone(),
        mode: agora_game_api::BaseMode::Linked,
    };
    h.write_manifest(&manifest);
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

    // The base itself is machine-local, like any game install; record an empty
    // one so the deploy plan has somewhere to read it from.
    let base_dir = ctx.paths.bases_dir().join(base_id);
    std::fs::create_dir_all(&base_dir).unwrap();
    // The install the base was built from is machine-local and separate from the base folder.
    let source_dir = empty.path().join("install");
    std::fs::create_dir_all(&source_dir).unwrap();
    let base_manifest = agora_core::game_base::BaseManifest {
        manifest_version: 1,
        base_id: base_id.to_string(),
        runtime,
        mode: agora_game_api::BaseMode::Linked,
        source_install: InstallId::new("steam:489830").unwrap(),
        source_product: None,
        source_location: source_dir,
        location: base_dir,
        created_unix_ms: 0,
        files: Vec::new(),
        skipped: Vec::new(),
        incomplete_frameworks: Vec::new(),
    };
    let base_path = ctx.paths.base_manifest_path(base_id);
    std::fs::create_dir_all(base_path.parent().unwrap()).unwrap();
    std::fs::write(
        &base_path,
        serde_json::to_vec_pretty(&base_manifest).unwrap(),
    )
    .unwrap();

    let imported = backup::import_instance_backup_blocking(&ctx, id, &artifact).unwrap();
    assert_eq!(imported.id, snap.id);

    // Import made a usable instance: it is registered, listed, opens, and can
    // be planned.
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_some());
    let listed = agora_core::game_instance::list(&ctx).unwrap();
    assert!(listed.iter().any(|record| record.instance_id == id));
    let opened = agora_core::game_instance::get_manifest(&ctx, id).unwrap();
    assert_eq!(opened.instance_id, id);
    let plan =
        agora_core::game_deploy::plan(&ctx, id, &h.def, agora_core::game_deploy::DeployMode::Links)
            .expect("an imported instance must be deploy-plannable");
    assert!(plan
        .files
        .iter()
        .any(|file| file.path.as_str() == "Data/Test.esp"));

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

    // The snapshot came from the source instance, but restoring it keeps the
    // imported instance's identity: it still lists and opens under its new id
    // and name.
    let after = agora_core::game_instance::get_manifest(&ctx, id).unwrap();
    assert_eq!(after.instance_id, id);
    assert_eq!(after.name, "Test");
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_some());
    assert!(agora_core::game_instance::list(&ctx)
        .unwrap()
        .iter()
        .any(|record| record.instance_id == id && record.name == "Test"));
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

// -- fetching missing content ---------------------------------------------------------------

struct FakeCatalogTransport {
    files: HashMap<String, Vec<u8>>,
    fetches: Mutex<usize>,
}

impl FakeCatalogTransport {
    fn with_archive(bytes: Vec<u8>) -> Self {
        let mut files = HashMap::new();
        files.insert("https://example.invalid/fake-v1.zip".to_string(), bytes);
        Self {
            files,
            fetches: Mutex::new(0),
        }
    }

    fn fetches(&self) -> usize {
        *self.fetches.lock().unwrap()
    }
}

#[async_trait]
impl CatalogTransport for FakeCatalogTransport {
    async fn releases_page(
        &self,
        _repo: &str,
        page: u32,
    ) -> LauncherResult<(Vec<GitHubRelease>, u32)> {
        if page > 1 {
            return Ok((Vec::new(), 1));
        }
        Ok((
            vec![GitHubRelease {
                tag_name: "v1".into(),
                published_at: Some("2026-11-01T00:00:00Z".into()),
                draft: false,
                prerelease: false,
                assets: vec![GitHubReleaseAsset {
                    name: "fake.zip".into(),
                    browser_download_url: "https://example.invalid/fake-v1.zip".into(),
                    size: None,
                    digest: None,
                }],
            }],
            1,
        ))
    }

    async fn fetch(&self, url: &str, _pinned_host: Option<&str>) -> LauncherResult<Vec<u8>> {
        *self.fetches.lock().unwrap() += 1;
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| LauncherError::Generic {
                code: "TEST_NO_FILE".into(),
                message: format!("no fake file for {url}"),
            })
    }
}

fn zip_archive_bytes() -> Vec<u8> {
    zip_archive_bytes_named("Data/Test.esp", b"plugin bytes")
}

fn zip_archive_bytes_named(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        writer.start_file(name, options).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

#[test]
fn fixture_archives_do_not_carry_the_wall_clock() {
    let bytes = zip_archive_bytes();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let entry = archive.by_index(0).unwrap();
    assert_eq!(
        entry.last_modified(),
        Some(zip::DateTime::default()),
        "a fixture whose bytes change with the clock makes a backup's recorded SHA-256 disagree with a later fetch of the same archive"
    );
}

fn fake_catalog_item(archive_sha256: &str) -> GameCatalogItem {
    GameCatalogItem {
        id: "fake-mod".into(),
        game: "skyrim-se".into(),
        name: "Fake mod".into(),
        author: None,
        content_type: "mod".into(),
        download_strategy: "github_release".into(),
        source_identifier: "example-author/fake-mod".into(),
        sha256: Some(archive_sha256.to_string()),
        download_sources: vec![DownloadSource {
            strategy: "github_release".into(),
            identifier: "example-author/fake-mod".into(),
            pins: Vec::new(),
        }],
        game_compatibility: Vec::new(),
        provides_framework: None,
        description: None,
        license_id: None,
        page_url: None,
        icon_url: None,
        status: "active".into(),
        date_added: None,
    }
}

fn catalog_backup(h: &Harness) -> (TempDir, String, String) {
    let bytes = zip_archive_bytes();
    let archive_sha = agora_core::download::sha256_hex(&bytes);
    let archive_path = h.dir().join("fake-mod.zip");
    std::fs::write(&archive_path, &bytes).unwrap();
    let item = content_store::add_archive_with_source(
        &h.ctx,
        &archive_path,
        Some("Fake mod"),
        |sha256, _path| ContentSource::Catalog {
            item_id: "fake-mod".into(),
            game: "skyrim-se".into(),
            release: Some("v1".into()),
            asset: "fake.zip".into(),
            sha256: sha256.to_string(),
            verified: true,
            added_at_unix_ms: 0,
        },
    )
    .unwrap()
    .item()
    .item_id
    .clone();
    h.add_layer(&item);
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();
    let export = TempDir::new().unwrap();
    backup::export_game_snapshot(
        &h.dir(),
        &snap.id,
        export.path(),
        &h.ctx.paths.content_root(),
        false,
    )
    .unwrap();
    (export, archive_sha, item)
}

#[tokio::test]
async fn a_backup_without_bytes_fetches_the_recorded_source_and_applies() {
    let h = Harness::new();
    let (export, archive_sha, content_item) = catalog_backup(&h);
    let artifact = export.path().join(format!(
        "agora-backup-{}-{}.zip",
        h.id,
        snapshot::list_snapshots(&h.dir()).unwrap()[0].id
    ));
    let empty = TempDir::new().unwrap();
    let ctx = fresh_context(&h.def, empty.path());
    let id = "fetched";
    let item = fake_catalog_item(&archive_sha);
    let transport = FakeCatalogTransport::with_archive(zip_archive_bytes());

    let preview = backup::preview_backup_import(&ctx, id, &artifact).unwrap();
    assert_eq!(preview.missing_items, vec!["Fake mod".to_string()]);
    assert_eq!(preview.missing_objects, 1);
    assert!(preview.missing_bytes > 0);

    backup::import_instance_backup_with(&ctx, id, &artifact, true, &transport, |item_id| {
        if item_id == "fake-mod" {
            Ok(Some(item.clone()))
        } else {
            Ok(None)
        }
    })
    .await
    .unwrap();

    assert_eq!(transport.fetches(), 1);
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_some());
    let parsed: content_store::ContentItem = serde_json::from_str(
        &std::fs::read_to_string(ctx.paths.content_item_path(&content_item)).unwrap(),
    )
    .unwrap();
    for file in &parsed.files {
        assert!(ctx.paths.content_object_path(&file.sha256).is_file());
    }
    assert_eq!(
        snapshot::list_snapshots(&ctx.paths.instance_dir(id).unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_backup_fetch_that_mismatches_applies_nothing_and_names_the_item() {
    let h = Harness::new();
    let (export, archive_sha, _content_item) = catalog_backup(&h);
    let artifact = export.path().join(format!(
        "agora-backup-{}-{}.zip",
        h.id,
        snapshot::list_snapshots(&h.dir()).unwrap()[0].id
    ));
    let empty = TempDir::new().unwrap();
    let ctx = fresh_context(&h.def, empty.path());
    let id = "mismatched";
    let item = fake_catalog_item(&archive_sha);
    let mut wrong = zip_archive_bytes();
    wrong[0] ^= 0xFF;
    let transport = FakeCatalogTransport::with_archive(wrong);

    let error =
        backup::import_instance_backup_with(&ctx, id, &artifact, true, &transport, |item_id| {
            if item_id == "fake-mod" {
                Ok(Some(item.clone()))
            } else {
                Ok(None)
            }
        })
        .await
        .unwrap_err();

    assert!(format!("{error}").contains("Fake mod"), "{error}");
    assert!(format!("{error}").contains("does not match"), "{error}");
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_none());
    let dir = ctx.paths.instance_dir(id).unwrap();
    assert!(!dir.join(".agora_snapshots").exists());
}

#[tokio::test]
async fn a_backup_fetch_is_refused_when_the_network_is_off() {
    let h = Harness::new();
    let (export, archive_sha, _content_item) = catalog_backup(&h);
    let artifact = export.path().join(format!(
        "agora-backup-{}-{}.zip",
        h.id,
        snapshot::list_snapshots(&h.dir()).unwrap()[0].id
    ));
    let empty = TempDir::new().unwrap();
    // The refusal is this context's own (MASTER_SPEC 32.9), not a process-wide slot another test
    // could loosen.
    let ctx = fresh_context(&h.def, empty.path())
        .with_network_gate(Arc::new(agora_core::network_gate::DenyAll));
    let id = "network-off";
    let item = fake_catalog_item(&archive_sha);
    let transport = FakeCatalogTransport::with_archive(zip_archive_bytes());

    let result =
        backup::import_instance_backup_with(&ctx, id, &artifact, true, &transport, |item_id| {
            if item_id == "fake-mod" {
                Ok(Some(item.clone()))
            } else {
                Ok(None)
            }
        })
        .await;

    let error = result.unwrap_err();
    assert!(
        format!("{error}").to_lowercase().contains("network"),
        "{error}"
    );
    assert_eq!(
        transport.fetches(),
        0,
        "no request is made when the gate refuses"
    );
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_none());
}

#[tokio::test]
async fn a_partial_fetch_leaves_nothing_applied_and_keeps_fetched_objects() {
    let h = Harness::new();
    let bytes_one = zip_archive_bytes();
    let sha_one = agora_core::download::sha256_hex(&bytes_one);
    let path_one = h.dir().join("one.zip");
    std::fs::write(&path_one, &bytes_one).unwrap();
    let item_one = content_store::add_archive_with_source(
        &h.ctx,
        &path_one,
        Some("Fetchable mod"),
        |sha256, _path| ContentSource::Catalog {
            item_id: "fake-mod".into(),
            game: "skyrim-se".into(),
            release: Some("v1".into()),
            asset: "fake.zip".into(),
            sha256: sha256.to_string(),
            verified: true,
            added_at_unix_ms: 0,
        },
    )
    .unwrap()
    .item()
    .item_id
    .clone();

    let bytes_two = zip_archive_bytes_named("Data/Other.esp", b"other bytes");
    let path_two = h.dir().join("two.zip");
    std::fs::write(&path_two, &bytes_two).unwrap();
    let item_two = content_store::add_archive_with_source(
        &h.ctx,
        &path_two,
        Some("Missing mod"),
        |sha256, _path| ContentSource::Catalog {
            item_id: "missing-mod".into(),
            game: "skyrim-se".into(),
            release: Some("v1".into()),
            asset: "missing.zip".into(),
            sha256: sha256.to_string(),
            verified: true,
            added_at_unix_ms: 0,
        },
    )
    .unwrap()
    .item()
    .item_id
    .clone();

    let mut manifest = h.manifest();
    manifest.layers = LayerStack::new(vec![
        Layer {
            id: LayerId::new("content:one").unwrap(),
            enabled: true,
            mount_path: RelPath::default(),
            source_path: RelPath::default(),
            source: LayerSource::Content {
                content: item_one.clone(),
            },
            whiteouts: Vec::new(),
            own_copy: false,
        },
        Layer {
            id: LayerId::new("content:two").unwrap(),
            enabled: true,
            mount_path: RelPath::default(),
            source_path: RelPath::default(),
            source: LayerSource::Content {
                content: item_two.clone(),
            },
            whiteouts: Vec::new(),
            own_copy: false,
        },
    ])
    .unwrap();
    h.write_manifest(&manifest);
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

    let empty = TempDir::new().unwrap();
    let ctx = fresh_context(&h.def, empty.path());
    let id = "partial";
    let fetchable = fake_catalog_item(&sha_one);
    let transport = FakeCatalogTransport::with_archive(bytes_one);

    let error = backup::import_instance_backup_with(
        &ctx,
        id,
        &artifact,
        true,
        &transport,
        move |item_id| {
            if item_id == "fake-mod" {
                Ok(Some(fetchable.clone()))
            } else {
                Ok(None)
            }
        },
    )
    .await
    .unwrap_err();

    assert!(format!("{error}").contains("Missing mod"), "{error}");
    // The item that could be fetched stays in the store for a retry.
    let parsed: content_store::ContentItem = serde_json::from_str(
        &std::fs::read_to_string(ctx.paths.content_item_path(&item_one)).unwrap(),
    )
    .unwrap();
    for file in &parsed.files {
        assert!(ctx.paths.content_object_path(&file.sha256).is_file());
    }
    // Nothing was applied to an instance.
    assert!(agora_core::game_instance::get(&ctx, id).unwrap().is_none());
    assert!(!ctx
        .paths
        .instance_dir(id)
        .unwrap()
        .join(".agora_snapshots")
        .exists());
}

// -- declared snapshot folders (MASTER_SPEC §32.7) -----------------------------------------------

/// A definition that declares folders of its own, beside the per-file state it names.
fn definition_declaring(folders: &[&str]) -> GameDefinition {
    let mut def = definition();
    def.snapshot = Some(
        agora_game_api::SnapshotScope::declared(folders, None).expect("the folders are usable"),
    );
    def
}

/// A definition that declares which of its folders the mandatory pre-launch snapshot captures.
fn definition_declaring_prelaunch(tracked: &[&str], prelaunch: &[&str]) -> GameDefinition {
    let mut def = definition();
    def.snapshot = Some(
        agora_game_api::SnapshotScope::declared(tracked, Some(prelaunch))
            .expect("the folders are usable"),
    );
    def
}

/// The documented example: `tracked: [characters, config]`, `prelaunch: [config]`.
fn prelaunch_entries(dir: &std::path::Path, snapshot_id: &str) -> Vec<String> {
    snapshot::snapshot_file_entries(dir, snapshot_id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.relative_path)
        .collect()
}

#[test]
fn launch_captures_only_the_declared_prelaunch_folders() {
    let h = Harness::new();
    let def = definition_declaring_prelaunch(&["characters", "config"], &["config"]);
    std::fs::create_dir_all(h.dir().join("characters")).unwrap();
    std::fs::write(h.dir().join("characters/hero.sav"), b"level 1").unwrap();
    std::fs::create_dir_all(h.dir().join("config")).unwrap();
    std::fs::write(h.dir().join("config/a.toml"), b"config v1").unwrap();

    let point = game_snapshot::prepare_launch(&h.ctx, &h.id, &def).expect("the launch point");
    let captured = prelaunch_entries(&h.dir(), &point.snapshot_id);

    assert!(
        captured.iter().any(|path| path == "config/a.toml"),
        "a pre-launch folder is captured: {captured:?}"
    );
    assert!(
        !captured.iter().any(|path| path.starts_with("characters")),
        "a folder the package did not list for pre-launch is not captured: {captured:?}"
    );
    // The state a launch point exists for is still captured.
    assert!(
        captured.iter().any(|path| path == "user/Plugins.txt"),
        "the declared state files are still captured: {captured:?}"
    );
}

#[test]
fn a_tracked_folder_that_cannot_be_read_does_not_block_launch() {
    let h = Harness::new();
    let def = definition_declaring_prelaunch(&["characters", "config"], &["config"]);
    // A file where the package declares a folder: capturing that folder refuses.
    std::fs::write(h.dir().join("characters"), b"not a folder").unwrap();

    let point = game_snapshot::prepare_launch(&h.ctx, &h.id, &def)
        .expect("a folder this launch does not capture cannot block it");
    let captured = prelaunch_entries(&h.dir(), &point.snapshot_id);
    assert!(
        !captured.iter().any(|path| path.starts_with("characters")),
        "{captured:?}"
    );
}

#[test]
fn a_declared_snapshot_folder_is_captured_and_restored() {
    let h = Harness::new();
    let def = definition_declaring(&["characters"]);
    let extras = h.dir().join("characters");
    std::fs::create_dir_all(&extras).unwrap();
    std::fs::write(extras.join("hero.sav"), b"level 1").unwrap();

    let taken = game_snapshot::create(
        &h.ctx,
        &h.id,
        &def,
        Some("with characters"),
        SnapshotOrigin::User,
    )
    .unwrap();
    let captured: Vec<String> = snapshot::snapshot_file_entries(&h.dir(), &taken.id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.relative_path)
        .collect();
    assert!(
        captured.iter().any(|path| path == "characters/hero.sav"),
        "the declared folder's file is in the snapshot: {captured:?}"
    );

    std::fs::write(extras.join("hero.sav"), b"level 9").unwrap();
    game_snapshot::restore(&h.ctx, &h.id, &def, &taken.id, false).unwrap();
    assert_eq!(
        std::fs::read(extras.join("hero.sav")).unwrap(),
        b"level 1",
        "the declared folder is state a restore puts back"
    );
}

#[test]
fn a_declared_folder_that_is_missing_captures_nothing_rather_than_failing() {
    let h = Harness::new();
    let def = definition_declaring(&["not-written-yet"]);
    let taken = game_snapshot::create(&h.ctx, &h.id, &def, None, SnapshotOrigin::User)
        .expect("a folder the game has not written is nothing to capture");
    let captured: Vec<String> = snapshot::snapshot_file_entries(&h.dir(), &taken.id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.relative_path)
        .collect();
    assert!(
        !captured
            .iter()
            .any(|path| path.starts_with("not-written-yet")),
        "{captured:?}"
    );
}

#[test]
fn a_declared_folder_that_is_a_file_refuses_the_capture() {
    let h = Harness::new();
    let def = definition_declaring(&["characters"]);
    // The instance has a file where the package declares a folder.
    std::fs::write(h.dir().join("characters"), b"not a folder").unwrap();

    let refused = game_snapshot::create(&h.ctx, &h.id, &def, None, SnapshotOrigin::User)
        .expect_err("a file standing in for a declared folder is cannot-tell");
    assert!(
        refused.to_string().contains("is a file, not a folder"),
        "{refused}"
    );
}

#[test]
fn a_restore_reports_a_folder_the_game_no_longer_declares() {
    let h = Harness::new();
    let declaring = definition_declaring(&["characters"]);
    let extras = h.dir().join("characters");
    std::fs::create_dir_all(&extras).unwrap();
    std::fs::write(extras.join("hero.sav"), b"level 1").unwrap();
    let taken =
        game_snapshot::create(&h.ctx, &h.id, &declaring, None, SnapshotOrigin::User).unwrap();
    std::fs::write(extras.join("hero.sav"), b"level 9").unwrap();

    // The package changed and no longer declares the folder; the snapshot still owns it.
    let outcome =
        game_snapshot::restore(&h.ctx, &h.id, &h.def, &taken.id, false).expect("restored anyway");
    assert_eq!(std::fs::read(extras.join("hero.sav")).unwrap(), b"level 1");
    let noticed = outcome
        .outcome
        .notices
        .iter()
        .find(|notice| notice.contains("characters"))
        .expect("the restore reports the folder it no longer tracks");
    assert!(
        noticed.contains("no longer tracked") && noticed.contains("skyrim-se"),
        "{noticed}"
    );
}

#[test]
fn the_undo_of_a_restore_that_wrote_a_retired_folder_puts_it_back() {
    let h = Harness::new();
    let declaring = definition_declaring(&["characters"]);
    let extras = h.dir().join("characters");
    std::fs::create_dir_all(&extras).unwrap();
    std::fs::write(extras.join("hero.sav"), b"level 1").unwrap();
    let taken =
        game_snapshot::create(&h.ctx, &h.id, &declaring, None, SnapshotOrigin::User).unwrap();

    // The game writes, and the package stops declaring the folder.
    std::fs::write(extras.join("hero.sav"), b"level 9").unwrap();
    std::fs::write(extras.join("new.sav"), b"created since").unwrap();

    let outcome =
        game_snapshot::restore(&h.ctx, &h.id, &h.def, &taken.id, false).expect("restored anyway");
    assert_eq!(std::fs::read(extras.join("hero.sav")).unwrap(), b"level 1");
    assert_eq!(
        std::fs::read(extras.join("new.sav")).unwrap(),
        b"created since",
        "a file created since the snapshot stays"
    );

    // Undoing the restore puts back what the restore replaced, in the folder the game dropped.
    game_snapshot::restore(&h.ctx, &h.id, &h.def, &outcome.undo_snapshot_id, false)
        .expect("the undo point holds the retired folder too");
    assert_eq!(
        std::fs::read(extras.join("hero.sav")).unwrap(),
        b"level 9",
        "the undo snapshot covers everything the restore touched"
    );
    assert_eq!(
        std::fs::read(extras.join("new.sav")).unwrap(),
        b"created since"
    );
}

/// Undo removes a file the restore created.
///
/// A data restore writes the files its manifest lists, so undoing it has to record the ones that
/// were not there: a file the restore created, left behind by the undo, is state the user cannot
/// get rid of (MASTER_SPEC §32.7).
#[test]
fn the_undo_of_a_restore_removes_a_file_the_restore_created() {
    let h = Harness::new();
    let def = definition_declaring(&["characters"]);
    let extras = h.dir().join("characters");
    std::fs::create_dir_all(&extras).unwrap();
    std::fs::write(extras.join("hero.sav"), b"level 1").unwrap();
    let taken = game_snapshot::create(&h.ctx, &h.id, &def, None, SnapshotOrigin::User).unwrap();

    std::fs::remove_file(extras.join("hero.sav")).unwrap();
    let outcome = game_snapshot::restore(&h.ctx, &h.id, &def, &taken.id, false)
        .expect("the snapshot owns a folder it declared");
    assert_eq!(
        std::fs::read(extras.join("hero.sav")).unwrap(),
        b"level 1",
        "the restore puts the captured file back"
    );

    game_snapshot::restore(&h.ctx, &h.id, &def, &outcome.undo_snapshot_id, false)
        .expect("the undo point reads back");
    assert!(
        !extras.join("hero.sav").exists(),
        "the undo removes the file the restore created"
    );
}

/// A mapped file is not a dropped folder.
///
/// The snapshot records what it captured: the file `user/Plugins.txt`, because that is what the
/// game declares there, not the folder `user` that happens to hold it. So a restore of it says
/// nothing about `user` — the false notice a root-shaped scope produced (MASTER_SPEC §32.7).
#[test]
fn a_mapped_file_is_not_reported_as_a_folder_the_game_dropped() {
    let h = Harness::new();
    // `h.def` declares the load order file `user/Plugins.txt` and no folder holding it.
    std::fs::create_dir_all(h.dir().join("user")).unwrap();
    std::fs::write(h.dir().join("user/Plugins.txt"), b"*A.esm").unwrap();
    let taken = game_snapshot::create(&h.ctx, &h.id, &h.def, None, SnapshotOrigin::User)
        .expect("the declared state files are captured");

    let outcome =
        game_snapshot::restore(&h.ctx, &h.id, &h.def, &taken.id, false).expect("restored");

    assert!(
        !outcome
            .outcome
            .notices
            .iter()
            .any(|notice| notice.contains("'user'")),
        "the file's folder is not reported as dropped: {:?}",
        outcome.outcome.notices
    );
}

/// The undo point covers the recorded scope, not the folder a mapped file lives in.
///
/// The reviewers' reproduction: map only `user/Plugins.txt`, keep `user/notes.txt` beside it,
/// snapshot, restore, edit `notes.txt`, undo — the edit has to survive, because the restore never
/// touched `notes.txt` (MASTER_SPEC §32.7).
#[test]
fn the_undo_of_a_restore_leaves_a_file_beside_a_mapped_one_alone() {
    let h = Harness::new();
    std::fs::create_dir_all(h.dir().join("user")).unwrap();
    std::fs::write(h.dir().join("user/Plugins.txt"), b"*A.esm").unwrap();
    std::fs::write(h.dir().join("user/notes.txt"), b"the user's notes").unwrap();
    let taken = game_snapshot::create(&h.ctx, &h.id, &h.def, None, SnapshotOrigin::User).unwrap();

    let outcome =
        game_snapshot::restore(&h.ctx, &h.id, &h.def, &taken.id, false).expect("restored");
    // The game writes to a file the snapshot never recorded, after the restore took its undo point.
    std::fs::write(h.dir().join("user/notes.txt"), b"edited after the restore").unwrap();

    game_snapshot::restore(&h.ctx, &h.id, &h.def, &outcome.undo_snapshot_id, false)
        .expect("the undo point reads back");

    assert_eq!(
        std::fs::read(h.dir().join("user/notes.txt")).unwrap(),
        b"edited after the restore",
        "the undo covers the recorded scope, so a file it never captured is left as it is"
    );
    assert_eq!(
        std::fs::read(h.dir().join("user/Plugins.txt")).unwrap(),
        b"*A.esm",
        "and the file the snapshot did record comes back"
    );
}

/// A snapshot records the mod setup, not which game files the instance plays on: that is the live
/// row's business. A snapshot taken while the instance was pinned to one base must not move a
/// restored instance onto it, or the manifest and the row would disagree about the base an
/// instance is pinned to.
#[test]
fn a_restore_keeps_the_live_rows_base_in_the_manifest() {
    let h = Harness::new();
    let runtime = h.manifest().runtime_identity.unwrap();
    let pinned = |id: &str| BaseReference::Pinned {
        id: id.into(),
        runtime: runtime.clone(),
        mode: agora_game_api::BaseMode::Linked,
    };

    // The snapshot is taken while the instance is pinned to the old base.
    let mut manifest = h.manifest();
    manifest.base = pinned("base-old");
    h.write_manifest(&manifest);
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    // The row and the live manifest now name the base it was rebuilt onto.
    let mut manifest = h.manifest();
    manifest.base = pinned("base-new");
    h.write_manifest(&manifest);
    let conn = agora_core::db::local_state_connection(&h.ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "UPDATE game_instance_bases SET base_json = ?1 WHERE instance_id = ?2",
        rusqlite::params![serde_json::to_string(&pinned("base-new")).unwrap(), h.id],
    )
    .unwrap();
    drop(conn);

    game_snapshot::restore(&h.ctx, &h.id, &h.def, &snap.id, false).unwrap();

    let base = h.manifest().base;
    assert!(
        matches!(&base, BaseReference::Pinned { id, .. } if id == "base-new"),
        "the restore put the snapshot's base back into the manifest, against the row: {base:?}"
    );
    let row = agora_core::game_instance::get(&h.ctx, &h.id)
        .unwrap()
        .expect("the row")
        .base;
    assert!(
        matches!(&row, BaseReference::Pinned { id, .. } if id == "base-new"),
        "the row still names the base it was pinned to: {row:?}"
    );
}

/// The manifest a restore writes must name the row's base from the moment it lands. A write
/// followed by a correction leaves the snapshot's base on the disk in between, and a deployment or
/// a base-removal scan that reads then acts on a base the instance is not pinned to
/// (MASTER_SPEC §32.8). The seam holds the restore at exactly that moment, before anything looks at
/// the manifest again.
#[test]
fn a_restore_never_puts_the_snapshots_base_on_the_disk() {
    let h = Harness::new();
    let runtime = h.manifest().runtime_identity.unwrap();
    let pinned = |id: &str| BaseReference::Pinned {
        id: id.into(),
        runtime: runtime.clone(),
        mode: agora_game_api::BaseMode::Linked,
    };

    // The snapshot is taken while the instance is pinned to the old base.
    let mut manifest = h.manifest();
    manifest.base = pinned("base-old");
    h.write_manifest(&manifest);
    let snap =
        game_snapshot::create(&h.ctx, &h.id, &h.def, Some("base"), SnapshotOrigin::User).unwrap();

    // The row and the live manifest now name the base it was rebuilt onto.
    let mut manifest = h.manifest();
    manifest.base = pinned("base-new");
    h.write_manifest(&manifest);
    let conn = agora_core::db::local_state_connection(&h.ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "UPDATE game_instance_bases SET base_json = ?1 WHERE instance_id = ?2",
        rusqlite::params![serde_json::to_string(&pinned("base-new")).unwrap(), h.id],
    )
    .unwrap();
    drop(conn);

    h.ctx
        .test_seams
        .set_restore_pause_after_manifest_for(Some(&h.id));
    let restore_ctx = h.ctx.clone();
    let restore_id = h.id.clone();
    let restore_def = h.def.clone();
    let restore_snapshot = snap.id.clone();
    let restore = std::thread::spawn(move || {
        game_snapshot::restore(
            &restore_ctx,
            &restore_id,
            &restore_def,
            &restore_snapshot,
            false,
        )
    });
    let mut reached = false;
    for _ in 0..600 {
        if h.ctx.test_seams.restore_pause_after_manifest_reached() {
            reached = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(reached, "the restore never wrote its manifest");

    // The moment the manifest is on the disk, before anything corrects it.
    let on_disk = h.manifest().base;
    h.ctx.test_seams.set_restore_pause_after_manifest_for(None);
    restore
        .join()
        .unwrap()
        .expect("the restore finishes once the seam is released");

    assert!(
        matches!(&on_disk, BaseReference::Pinned { id, .. } if id == "base-new"),
        "the restored manifest named the snapshot's base while the row named the live one: {on_disk:?}"
    );
    let after = h.manifest().base;
    assert!(
        matches!(&after, BaseReference::Pinned { id, .. } if id == "base-new"),
        "and it still names it afterwards: {after:?}"
    );
}
