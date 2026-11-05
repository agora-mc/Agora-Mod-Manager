use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{add_content, plan, DeployError, DeployMode};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::{create, GameInstanceRecord};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId, InstallKind,
    LaunchRecipe, LaunchValue, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier,
};
use tempfile::TempDir;

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn register_test_game_with_features(def: GameDefinition) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    let pkg_def = PackageDefinition {
        id: format!("test.{}", def.id),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        games: vec![def],
        frameworks: vec![],
        tools: vec![],
    };
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(pkg_def)),
        )
        .expect("register test package");
    Arc::new(builder.build())
}

fn make_test_definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: None,
        session: Default::default(),
        id: GameId::new("test-game").unwrap(),
        name: "Test Game".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "12345".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        native_code_patterns: vec![],
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![LaunchValue::Literal {
                value: "-test".into(),
            }],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        log_paths: vec![],
        crash_paths: vec![],
        user_files: vec![],
        save_paths: vec![],
        linked_archive_patterns: vec!["Data/*.bsa".into()],
        declared_writes: vec!["writeable_base.txt".into()],
        excluded_paths: vec![],
        plugin_list: None,
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        launch_alternatives: Vec::new(),
        content_layout: None,
        copy_patterns: Vec::new(),
    }
}

fn setup_fake_install(dir: &Path) {
    std::fs::write(dir.join("Game.exe"), b"fake game binary").unwrap();
    std::fs::write(dir.join("base_file.txt"), b"initial base file").unwrap();
    std::fs::write(dir.join("writeable_base.txt"), b"initial writeable base").unwrap();
    let data_dir = dir.join("Data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("Skyrim.bsa"), b"BSA DATA").unwrap();
}

fn make_test_install(install_dir: &Path, readable: bool, relocatable: bool) -> IdentifiedInstall {
    let store_id = StoreId::new("steam").unwrap();
    let runtime = RuntimeIdentity {
        game: GameId::new("test-game").unwrap(),
        store: store_id.clone(),
        version: "1.0.0".into(),
        build: None,
    };
    let detector = agora_core::game_discovery::volume::VolumeDetector::new();
    let volume = detector.get_volume_info(install_dir);
    let install_id = InstallId::new("steam:12345").unwrap();
    let discovered = DiscoveredInstall {
        store: store_id,
        product: "12345".into(),
        name: "Test Game".into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: install_dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: runtime.build.clone(),
        executables: vec!["Game.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: readable,
            accepts_new_files: true,
            relocatable,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id,
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

fn create_test_context(tmp: &TempDir, def: &GameDefinition) -> CoreContext {
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let registry = register_test_game_with_features(def.clone());
    ctx.with_games(registry)
}

fn create_pinned_instance(
    ctx: &CoreContext,
    install: &IdentifiedInstall,
    def: &GameDefinition,
    name: &str,
) -> GameInstanceRecord {
    create(ctx, install, def, name, None, BaseMode::Linked, &|_| {}).expect("create instance")
}

fn add_content_folder(ctx: &CoreContext, name: &str, files: &[(&str, &[u8])]) -> String {
    let mod_dir = tempfile::tempdir().unwrap();
    for (rel, content) in files {
        let p = mod_dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
    let outcome = agora_core::content_store::add_folder(ctx, mod_dir.path(), Some(name)).unwrap();
    outcome.item().item_id.clone()
}

// ---------------------------------------------------------------------------
// Review tests for the content store core (final multi-game review). Each test proves a fixed
// bug; run them with
//   cargo nextest run -p agora-core --test review_store_core
// ---------------------------------------------------------------------------

use agora_core::content_store as cs;
use agora_core::lock_manager::LockResource;

/// A PNG signature plus filler: `sniff_image` accepts it, which is all an icon needs here.
fn png_bytes(tag: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[tag; 32]);
    bytes
}

fn add_one_file(ctx: &CoreContext, name: &str, bytes: &[u8]) -> String {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join(format!("{name}.dll"));
    std::fs::write(&path, bytes).unwrap();
    cs::add_file(ctx, &path, Some(name), None)
        .unwrap()
        .item()
        .item_id
        .clone()
}

#[test]
fn removing_one_item_deletes_another_items_saved_icon() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().to_path_buf());

    let keep = add_one_file(&ctx, "Keeper", b"keeper bytes");
    let other = add_one_file(&ctx, "Other", b"other bytes");

    let saved = agora_core::library::save_icon(&ctx, &keep, &png_bytes(7)).unwrap();
    assert!(saved, "a PNG is saved as the icon");
    assert!(
        agora_core::library::read_icon(&ctx, &keep)
            .unwrap()
            .is_some(),
        "the icon reads before any removal"
    );

    // Removing a different, unused item must not touch Keeper's icon.
    cs::remove_item(&ctx, &other).unwrap();

    let icon = agora_core::library::read_icon(&ctx, &keep);
    assert!(
        matches!(icon, Ok(Some(_))),
        "Keeper's icon object was swept by removing another item: {icon:?}"
    );
}

#[test]
fn storing_an_icon_object_waits_for_the_content_store_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().to_path_buf());

    // Someone else (a remove_item sweep, an add) holds the store lock.
    let _held = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "test-holder")
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let ctx2 = ctx.clone();
    let worker = std::thread::spawn(move || {
        let r = cs::store_object_bytes(&ctx2, &png_bytes(9));
        let _ = tx.send(r.is_ok());
    });
    let finished_while_locked = rx.recv_timeout(std::time::Duration::from_secs(3)).is_ok();
    drop(_held);
    let _ = worker.join();
    assert!(
        !finished_while_locked,
        "store_object_bytes completed while another operation held the content-store lock"
    );
}

#[test]
fn file_folder_conflict_is_found_when_a_sibling_sorts_between_them() {
    let tmp = TempDir::new().unwrap();
    let def = make_test_definition();
    let ctx = create_test_context(&tmp, &def);

    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    setup_fake_install(&install_dir);
    let install = make_test_install(&install_dir, true, true);
    let inst = create_pinned_instance(&ctx, &install, &def, "SortGap");

    let file_layer = add_content_folder(&ctx, "FileLayer", &[("Data/x", b"file content")]);
    let sibling = add_content_folder(&ctx, "Sibling", &[("Data/x-extra.esp", b"sibling")]);
    let folder_layer = add_content_folder(&ctx, "FolderLayer", &[("Data/x/y", b"child content")]);
    add_content(&ctx, &inst.instance_id, &file_layer, None, None).unwrap();
    add_content(&ctx, &inst.instance_id, &sibling, None, None).unwrap();
    add_content(&ctx, &inst.instance_id, &folder_layer, None, None).unwrap();

    let result = plan(&ctx, &inst.instance_id, &def, DeployMode::Links);
    assert!(
        matches!(result, Err(DeployError::FileFolderConflict { .. })),
        "'Data/x' is a file in one layer and a folder in another: {:?}",
        result.as_ref().map(|p| p.files.len())
    );
}

/// A Minecraft instance manifest recording the store fields the real installs write. `item_id`
/// is `None` for an entry that recorded only the object hash.
fn write_minecraft_instance(ctx: &CoreContext, id: &str, hash: &str, item_id: Option<&str>) {
    let dir = ctx.paths.instance_dir(id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let entry = serde_json::json!({
        "filename": "stored.jar",
        "source": "manual",
        "version": "1.0.0",
        "sha256": hash,
        "content_item_id": item_id,
        "object_sha256": hash,
        "installed_at": "2026-01-01T00:00:00Z"
    });
    let manifest = serde_json::json!({
        "manifest_version": 2,
        "instance_id": id,
        "name": "Hash Only",
        "minecraft_version": "1.20.1",
        "loader": "fabric",
        "loader_version": "0.15.11",
        "mods": [entry],
        "resourcepacks": [],
        "shaders": [],
        "datapacks": [],
        "worlds": []
    });
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn removing_a_damaged_item_fails_closed_when_an_instance_may_use_its_objects() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = CoreContext::for_testing(tmp.path().to_path_buf());

    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("damaged.jar");
    std::fs::write(&path, b"damaged item bytes").unwrap();
    let outcome = cs::add_file(&ctx, &path, Some("Damaged"), None).unwrap();
    let item_id = outcome.item().item_id.clone();
    let hash = outcome.item().files[0].sha256.clone();
    let object_path = ctx.paths.content_object_path(&hash);
    assert!(object_path.exists());

    // The instance records only the object hash, never the item id.
    write_minecraft_instance(&ctx, "mc-hash-only", &hash, None);

    // Damage the item's own record: its file hashes cannot be read any more.
    std::fs::write(ctx.paths.content_item_path(&item_id), b"{ not an item").unwrap();

    let result = cs::remove_item(&ctx, &item_id);
    assert!(
        matches!(result, Err(cs::ContentError::UnreadableManifest { .. })),
        "a damaged record whose objects may be in use must not be removed: {result:?}"
    );
    assert!(
        ctx.paths.content_item_path(&item_id).exists(),
        "the damaged record is kept"
    );
    assert!(
        object_path.exists(),
        "the object a hash-only instance may use is kept"
    );
}

#[test]
fn windows_device_names_cover_zero_superscripts_and_a_space_before_the_extension() {
    for name in [
        "CON", "con", "PRN", "AUX", "NUL", "COM0", "LPT0", "COM1", "LPT9", "COM¹", "COM²", "COM³",
        "LPT¹", "LPT²", "LPT³", "aux ",
    ] {
        assert!(
            cs::is_windows_device_name(name),
            "{name:?} is a reserved device name"
        );
    }
    assert!(
        !cs::is_windows_device_name("COM10"),
        "COM10 is not reserved"
    );
    assert!(!cs::is_windows_device_name("auxiliary"));

    for path in ["aux .txt", "COM¹.txt", "LPT0.log", "con .esp"] {
        let error = cs::validate_entry_path(path).unwrap_err().to_string();
        assert!(
            error.contains("device name"),
            "{path:?} must be refused as a device name: {error}"
        );
    }
}
