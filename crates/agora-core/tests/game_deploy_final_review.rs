//! Final review of the multi-game branch: proofs of bugs in deploy teardown and harvest.
//! Every test is `#[ignore]`d; run with `--ignored` to see it fail.
#![allow(unused_imports, dead_code)]

use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::game_base::{get_file_identity, BaseMode};
use agora_core::game_deploy::{
    add_content, deploy, deployment_dir, move_content, plan, remove_content, set_content_enabled,
    undeploy, DeployError, DeployMode, DeployOutcome, FileSource, Placement,
};
use agora_core::game_discovery::{DiscoveredInstall, DiscoveryReport, InstallCapabilities};
use agora_core::game_instance::{
    create, get_manifest, prepare_launch_with_discovery, GameInstanceRecord, InstanceError,
};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    BaseReference, DeploymentStrategy, GameDefinition, GameId, GamePackage, GamePath, InstallId,
    InstallKind, LaunchRecipe, LaunchValue, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
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

// ---------------------------------------------------------------- adversarial probes

fn setup(tmp: &TempDir) -> (CoreContext, GameDefinition, GameInstanceRecord) {
    let def = make_test_definition();
    let ctx = create_test_context(tmp, &def);
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    setup_fake_install(&install_dir);
    let install = make_test_install(&install_dir, true, true);
    let inst = create_pinned_instance(&ctx, &install, &def, "Probe");
    (ctx, def, inst)
}

fn record_path(ctx: &CoreContext, id: &str) -> std::path::PathBuf {
    deployment_dir(ctx, id)
        .unwrap()
        .unwrap()
        .parent()
        .unwrap()
        .join("deployment.json")
}

#[cfg(windows)]
#[test]
#[ignore = "proves bug: a teardown that stops half way (file in use) leaves a record whose next harvest reads the half-deleted farm as game deletions and deletes writable-layer files"]
fn interrupted_farm_removal_makes_the_next_harvest_delete_writable_files() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = TempDir::new().unwrap();
    let (ctx, def, inst) = setup(&tmp);
    let item = add_content_folder(&ctx, "M", &[("a.txt", b"a")]);
    add_content(&ctx, &inst.instance_id, &item, None, None).unwrap();
    deploy(&ctx, &inst.instance_id, &def, DeployMode::Links).unwrap();
    let game = deployment_dir(&ctx, &inst.instance_id).unwrap().unwrap();
    std::fs::write(game.join("new.txt"), b"the game wrote this save").unwrap();
    undeploy(&ctx, &inst.instance_id).unwrap();
    let inst_dir = ctx.paths.instance_dir(&inst.instance_id).unwrap();
    let kept = inst_dir.join("writable").join("new.txt");
    assert!(kept.exists(), "setup: harvest should have created the writable copy");

    deploy(&ctx, &inst.instance_id, &def, DeployMode::Links).unwrap();
    let game = deployment_dir(&ctx, &inst.instance_id).unwrap().unwrap();
    assert!(game.join("new.txt").exists());

    // An antivirus scan / indexer / Explorer holds a file open without delete sharing.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only
        .open(game.join("writeable_base.txt"))
        .unwrap();
    let first = undeploy(&ctx, &inst.instance_id);
    drop(held);
    assert!(first.is_err(), "setup: the first undeploy should fail on the held file");
    assert!(!game.join("new.txt").exists(), "setup: the farm should be half removed");

    // The user closes the scanner and undeploys / deploys again.
    let _ = undeploy(&ctx, &inst.instance_id);
    assert!(
        kept.exists(),
        "the game's save in the writable layer was deleted because a half-removed farm looked like the game deleted it"
    );
}

#[test]
#[ignore = "proves bug: deleting an instance leaves its deployed game folder (and any unharvested game writes) behind forever"]
fn deleting_an_instance_removes_its_deployed_farm() {
    let tmp = TempDir::new().unwrap();
    let (ctx, def, inst) = setup(&tmp);
    let item = add_content_folder(&ctx, "M", &[("a.txt", b"a")]);
    add_content(&ctx, &inst.instance_id, &item, None, None).unwrap();
    deploy(&ctx, &inst.instance_id, &def, DeployMode::Links).unwrap();
    let farm = deployment_dir(&ctx, &inst.instance_id).unwrap().unwrap();
    let deployment = farm.parent().unwrap().to_path_buf();
    assert!(deployment.join("deployment.json").exists());

    agora_core::game_instance::delete(&ctx, &inst.instance_id).unwrap();

    assert!(
        !deployment.exists(),
        "the deleted instance's deployment folder is still on disk: {}",
        deployment.display()
    );
}

#[test]
#[ignore = "proves bug: a deploy killed between renaming the new farm into place and writing its record leaves a farm no deploy can replace and no undeploy can clean"]
fn farm_without_a_record_blocks_every_later_deploy() {
    let tmp = TempDir::new().unwrap();
    let (ctx, def, inst) = setup(&tmp);
    let item = add_content_folder(&ctx, "M", &[("a.txt", b"a")]);
    add_content(&ctx, &inst.instance_id, &item, None, None).unwrap();
    deploy(&ctx, &inst.instance_id, &def, DeployMode::Links).unwrap();
    // The state a crash (or a full disk) leaves between `rename(staging, game)` and the record write.
    std::fs::remove_file(record_path(&ctx, &inst.instance_id)).unwrap();

    // A re-run must finish the job or put things back.
    let again = deploy(&ctx, &inst.instance_id, &def, DeployMode::Links);
    assert!(
        again.is_ok(),
        "deploy is stuck behind the record-less farm: {:?}",
        again.err()
    );
}
