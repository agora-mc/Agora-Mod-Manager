//! The two importer cases from the Neon Courier second-game review fixture.

#![cfg(windows)]

use std::path::Path;
use std::sync::Arc;

use agora_core::ctx::CoreContext;
use agora_core::game_deploy::{self, DeployMode};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_import::{self, Mo2Request, RunOptions};
use agora_core::game_registry::{
    GameInventory, GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    ContentLayout, ContentMatch, ContentRule, DeploymentStrategy, GameDefinition, GameId,
    GamePackage, GamePath, InstallId, InstallKind, LaunchRecipe, PackageDefinition, RelPath,
    RuntimeIdentity, StoreId, StoreIdentifier, VersionSource,
};
use rusty_leveldb::{Options, DB};
use tempfile::TempDir;

const GAME_ID: &str = "neon-courier";
const MO2_NAME: &str = "Neon Courier";
const VORTEX_ID: &str = "neoncourier";

fn store() -> StoreId {
    StoreId::steam()
}

fn rule(marker: &str, mount: &str) -> ContentRule {
    ContentRule {
        kind: marker.trim_end_matches(".json").to_string(),
        matching: ContentMatch {
            marker: Some(marker.into()),
            all_files: None,
        },
        mount: RelPath::new(mount).unwrap(),
        when: None,
    }
}

/// Neon Courier as a package would declare it. Two roots are reached through content rules; the
/// layout names only one, which is what the importers read.
fn definition() -> GameDefinition {
    GameDefinition {
        mo2_game_name: Some(MO2_NAME.into()),
        import_ids: [("vortex".to_string(), VORTEX_ID.to_string())]
            .into_iter()
            .collect(),
        session: Default::default(),
        id: GameId::new(GAME_ID).unwrap(),
        extends: None,
        name: MO2_NAME.into(),
        stores: vec![StoreIdentifier {
            store: store(),
            product: "990001".into(),
        }],
        version_sources: vec![VersionSource::Executable {
            path: RelPath::new("bin/NeonCourier.exe").unwrap(),
        }],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![
            rule("plugin.json", "bin/plugins/{folder}"),
            rule("archive.json", "archive/mods/{folder}"),
        ],
        content_layout: Some(ContentLayout {
            data_path: RelPath::new("archive/mods").unwrap(),
            data_markers: vec![],
            root_markers: vec![],
            thunderstore_bepinex: false,
        }),
        framework_ids: vec![],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("bin/NeonCourier.exe").unwrap(),
            },
            arguments: vec![],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        user_files: vec![],
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        copy_patterns: vec![],
        excluded_paths: vec![],
        load_orders: vec![],
        runtime_files: vec![],
        save_location: vec![],
        ini_checks: vec![],
        launch_alternatives: vec![],
    }
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

/// Register a Neon Courier definition as a compiled test package, the way the desktop registers
/// shipped ones.
fn registry(def: GameDefinition) -> Arc<GameRegistry> {
    let package = PackageDefinition {
        id: format!("test.{GAME_ID}"),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def],
        frameworks: vec![],
        tools: vec![],
    };
    let mut builder = GameRegistry::builder();
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "second-game-test".into(),
            },
            Arc::new(TestPackage(package)),
        )
        .expect("the Neon Courier test package registers");
    Arc::new(builder.build())
}

struct Fixture {
    _tmp: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    install: IdentifiedInstall,
}

impl Fixture {
    fn new(def: GameDefinition) -> Self {
        let tmp = TempDir::new().unwrap();

        let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let ctx = ctx.with_games(registry(def.clone()));

        let install_dir = tmp.path().join("install");
        std::fs::create_dir_all(install_dir.join("bin")).unwrap();
        std::fs::write(install_dir.join("bin/NeonCourier.exe"), b"neon courier").unwrap();
        std::fs::create_dir_all(install_dir.join("archive/mods")).unwrap();
        std::fs::create_dir_all(install_dir.join("bin/plugins")).unwrap();

        let install = install_of(&install_dir);
        Self {
            _tmp: tmp,
            ctx,
            def,
            install,
        }
    }
}

fn install_of(dir: &Path) -> IdentifiedInstall {
    let runtime = RuntimeIdentity {
        game: GameId::new(GAME_ID).unwrap(),
        store: StoreId::steam(),
        version: "1.0.0".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: StoreId::steam(),
        product: "990001".into(),
        name: MO2_NAME.into(),
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
    let f = Fixture::new(definition());
    let setup = f._tmp.path().join("mo2");
    write(
        &setup.join("ModOrganizer.ini"),
        format!(
            "[General]\r\ngameName={}\r\nselected_profile=@ByteArray(Neon)\r\ngamePath={}\r\n",
            MO2_NAME,
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
    let f = Fixture::new(definition());
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
        game: GameId::new(GAME_ID).unwrap(),
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
