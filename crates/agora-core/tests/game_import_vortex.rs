//! Importing a Vortex setup (MASTER_SPEC §26.10): an invented LevelDB store and staging folder
//! built in each test. The real store is never read here (or in the product): it holds the user's
//! account data.

#![cfg(windows)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use agora_core::ctx::CoreContext;
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_import::vortex::{self, VortexRequest};
use agora_core::game_import::ImportError;
use agora_core::game_registry::{
    GameInventory, GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_game_api::{
    ContentLayout, GameDefinition, GameId, GamePackage, InstallId, InstallKind,
    LoadOrderDefinition, LoadOrderFormat, LoadOrderItems, LoadOrderRules, PackageDefinition,
    RelPath, RuntimeIdentity, StoreId,
};
use rusty_leveldb::{Options, DB};
use tempfile::TempDir;

const GAME: &str = "skyrim-se";
const VORTEX_GAME: &str = "vortextestgame";

static USER_DATA: Mutex<()> = Mutex::new(());

struct Fixture {
    _lock: MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user_data: TempDir,
    ctx: CoreContext,
    inventory: GameInventory,
    store: PathBuf,
    staging: PathBuf,
}

impl Drop for Fixture {
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

fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn skyrim_like() -> GameDefinition {
    let mut def = common::skyrim_definition();
    def.mo2_game_name = Some("Skyrim Special Edition".into());
    def.content_layout = Some(ContentLayout {
        data_path: RelPath::new("Data").unwrap(),
        data_markers: vec![],
        root_markers: vec![],
        thunderstore_bepinex: false,
    });
    def.load_orders = vec![LoadOrderDefinition {
        id: "plugins".into(),
        name: "Plugins".into(),
        file: RelPath::new("user/Plugins.txt").unwrap(),
        destination: Default::default(),
        items: LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esm".into(), "*.esl".into(), "*.esp".into()],
            folders: false,
        },
        format: LoadOrderFormat::Lines {
            active_prefix: "*".into(),
            header: vec![
                "# This file is used by Skyrim to keep track of your downloaded content.".into(),
                "# Please do not modify this file.".into(),
            ],
        },
        rules: LoadOrderRules {
            kind: "none".into(),
            entry: None,
            implicit: vec!["Skyrim.esm".into(), "Update.esm".into()],
            implicit_list_file: None,
            sorter: None,
            overlaps: None,
        },
        intro: None,
        item_heading: None,
    }];
    def
}

fn install_of(dir: &Path) -> IdentifiedInstall {
    let store_id = StoreId::steam();
    let runtime = RuntimeIdentity {
        game: GameId::new(GAME).unwrap(),
        store: store_id.clone(),
        version: "1.6.1170".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: store_id,
        product: "489830".into(),
        name: "Skyrim Special Edition".into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: None,
        executables: vec!["Game.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("steam:489830").unwrap(),
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
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
        db.put(key.as_bytes(), value.as_bytes())
            .expect("a key is written");
    }
    db.flush().expect("the test store flushes");
}

fn build_setup(tmp: &Path, game_dir: &Path) -> (PathBuf, PathBuf) {
    write(&game_dir.join("Data/Skyrim.esm"), b"MASTER");
    std::fs::copy(r"C:\Windows\System32\cmd.exe", game_dir.join("Game.exe")).unwrap();

    let store = tmp.join("Vortex").join("state.v2");
    std::fs::create_dir_all(&store).unwrap();
    let staging = tmp.join("Vortex").join(VORTEX_GAME).join("mods");
    let profile = tmp
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p1");

    write(&staging.join("Alpha Folder/Alpha.esp"), b"ALPHA PLUGIN");
    write(&staging.join("Alpha Folder/shared.txt"), b"from alpha");
    write(&staging.join("Beta Folder/Beta.esp"), b"BETA PLUGIN");
    write(&staging.join("Beta Folder/shared.txt"), b"from beta");
    write(&staging.join("Gamma Folder/Gamma.esp"), b"GAMMA PLUGIN");
    write(&staging.join("Gamma Folder/shared.txt"), b"from gamma");

    write(
        &profile.join("plugins.txt"),
        b"# Automatically generated by Vortex\r\n*Alpha.esp\r\n*Beta.esp\r\n",
    );
    write(
        &profile.join("loadorder.txt"),
        b"# Automatically generated by Vortex\r\nAlpha.esp\r\nBeta.esp\r\nGamma.esp\r\n",
    );

    // The manifest says Beta supplied the shared file; the rules alone would say Alpha.
    write(
        &game_dir.join("Data/vortex.deployment.json"),
        br#"{
  "version": 1,
  "instance": "test-instance",
  "files": [
    { "relPath": "Alpha.esp", "source": "Alpha Folder", "time": 1 },
    { "relPath": "Beta.esp", "source": "Beta Folder", "time": 1 },
    { "relPath": "Gamma.esp", "source": "Gamma Folder", "time": 1 },
    { "relPath": "shared.txt", "source": "Beta Folder", "time": 1 }
  ]
}"#,
    );

    let mods = format!(
        r#"{{"{VORTEX_GAME}":{{
  "mod-alpha": {{"id":"mod-alpha","state":"installed","installationPath":"Alpha Folder","attributes":{{"name":"Alpha Mod","version":"1.0","modId":101,"fileId":11}}}},
  "mod-beta": {{"id":"mod-beta","state":"installed","installationPath":"Beta Folder","attributes":{{"name":"Beta Mod","version":"2.0","modId":102,"fileId":12}},"rules":[{{"type":"after","reference":{{"id":"mod-gamma"}}}}]}},
  "mod-gamma": {{"id":"mod-gamma","state":"installed","installationPath":"Gamma Folder","attributes":{{"name":"Gamma Mod","version":"3.0","modId":103,"fileId":13}},"rules":[{{"type":"after","reference":{{"id":"mod-alpha"}}}}]}}
}}}}"#
    );
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":false}}}}}}}}"#
    );
    write_store(
        &store,
        &[
            ("persistent###mods", mods),
            ("persistent###profiles", profiles),
            (
                &format!("settings###mods###installPath###{VORTEX_GAME}"),
                "\"{USERDATA}/{GAME}/mods\"".to_string(),
            ),
            (
                &format!("settings###gameMode###discovered###{VORTEX_GAME}"),
                format!(
                    r#"{{"path":"{}"}}"#,
                    game_dir.to_string_lossy().replace('\\', "/")
                ),
            ),
        ],
    );
    (store, staging)
}

fn fixture() -> Fixture {
    let lock = USER_DATA.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let user_data = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data.path());

    let def = skyrim_like();
    let pkg = PackageDefinition {
        id: format!("test.{GAME}"),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks: vec![],
        tools: vec![],
    };
    let mut builder = GameRegistry::builder();
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(pkg)),
        )
        .expect("register the test package");
    let registry = Arc::new(builder.build());

    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry);

    let game_dir = tmp.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    let (store, staging) = build_setup(tmp.path(), &game_dir);

    let inventory = GameInventory {
        installs: vec![install_of(&game_dir)],
        unsupported: vec![],
    };
    Fixture {
        _lock: lock,
        _tmp: tmp,
        _user_data: user_data,
        ctx,
        inventory,
        store,
        staging,
    }
}

fn request(f: &Fixture, name: Option<&str>) -> VortexRequest {
    VortexRequest {
        store: f.store.clone(),
        game: GameId::new(GAME).unwrap(),
        profile: "Main".into(),
        name: name.map(str::to_string),
        install: None,
    }
}

#[test]
fn scan_lists_the_game_profile_and_manifest_without_writing() {
    let f = fixture();
    let report = vortex::scan(&f.ctx, &f.inventory, &f.store).expect("the store scans");
    assert_eq!(report.games.len(), 1);
    let game = &report.games[0];
    assert_eq!(game.vortex_game, VORTEX_GAME);
    assert!(game.supported, "{game:?}");
    assert_eq!(game.game.as_ref(), Some(&GameId::new(GAME).unwrap()));
    assert!(game.manifest_found);
    assert_eq!(game.mods, 3);
    assert_eq!(game.profiles.len(), 1);
    let profile = &game.profiles[0];
    assert_eq!(profile.name, "Main");
    assert_eq!(profile.mods_enabled, 2);
    assert_eq!(profile.mods_total, 3);
    assert!(profile.plugins_txt);
    assert!(profile.loadorder_txt);
    // Nothing was written beside the scratch copy the reader removes.
    assert!(f.store.join("CURRENT").is_file());
    assert!(f.staging.join("Alpha Folder/Alpha.esp").is_file());
}

#[test]
fn a_game_vortex_discovered_but_agora_does_not_support_is_listed_as_unsupported() {
    let f = fixture();
    // Point Vortex's discovery at a folder no Agora install is at.
    let elsewhere = f._tmp.path().join("not-a-game");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let mut db = DB::open(
        &f.store,
        Options {
            create_if_missing: true,
            ..Options::default()
        },
    )
    .unwrap();
    db.put(
        format!("settings###gameMode###discovered###{VORTEX_GAME}").as_bytes(),
        format!(
            r#"{{"path":"{}"}}"#,
            elsewhere.to_string_lossy().replace('\\', "/")
        )
        .as_bytes(),
    )
    .unwrap();
    db.flush().unwrap();
    drop(db);

    let report = vortex::scan(&f.ctx, &f.inventory, &f.store).expect("the store scans");
    let game = &report.games[0];
    assert!(!game.supported);
    assert!(game
        .reason
        .as_deref()
        .unwrap_or_default()
        .contains("not an install"));
}

#[test]
fn the_manifest_decides_the_layer_order_and_disagreements_are_reported() {
    let f = fixture();
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert_eq!(plan.mods.len(), 3);
    assert_eq!(plan.mods_enabled, 2);
    assert_eq!(plan.mods_disabled, 1);
    assert!(plan.manifest.found);
    assert_eq!(plan.manifest.matched, 4);

    let place = |id: &str| plan.order.iter().position(|m| m == id).unwrap();
    // Beta supplied the shared file, so it is above Alpha and Gamma even though the rules put
    // Gamma before both of them.
    assert!(place("mod-beta") < place("mod-alpha"));
    assert!(place("mod-beta") < place("mod-gamma"));

    let shared = plan
        .disagreements
        .iter()
        .find(|d| d.file == "shared.txt")
        .expect("the shared file is a disagreement");
    assert_eq!(shared.manifest_supplied, "Beta Mod");
    assert_eq!(shared.rules_supply, "Alpha Mod");

    assert_eq!(plan.plugins.active, 2);
    assert_eq!(plan.plugins.lines.len(), 3);
}

#[test]
fn run_makes_an_instance_and_a_second_import_is_refused() {
    let f = fixture();
    let report = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect("the import runs");
    assert_eq!(report.stored_mods, 3);
    assert_eq!(report.content_layers, 3);
    assert_eq!(report.disabled_layers, 1);
    assert_eq!(report.plugins_written, Some(3));
    assert!(report.instance_id.is_some());
    assert!(report.next_steps.iter().any(|s| s.contains("downloads")));

    let error = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect_err("the same profile is refused");
    assert!(
        matches!(error, ImportError::AlreadyImported { .. }),
        "{error}"
    );

    // The staging folder and the store are only read; the store still has its own files.
    assert!(f.staging.join("Beta Folder/shared.txt").is_file());
    assert!(f.store.join("CURRENT").is_file());
}

#[test]
fn a_profile_that_does_not_exist_is_named() {
    let f = fixture();
    let mut request = request(&f, None);
    request.profile = "Missing Profile".into();
    let error = vortex::plan(&f.ctx, &f.inventory, &request).expect_err("the profile is missing");
    match error {
        ImportError::NoVortexProfile { profile, .. } => assert_eq!(profile, "Missing Profile"),
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn a_locked_store_is_refused_with_close_vortex() {
    let f = fixture();
    let held = DB::open(
        &f.store,
        Options {
            create_if_missing: true,
            ..Options::default()
        },
    )
    .expect("the live store opens");
    let error = vortex::scan(&f.ctx, &f.inventory, &f.store).expect_err("the store is held");
    let text = error.to_string();
    assert!(text.contains("Vortex is running"), "{text}");
    assert!(text.contains("Close Vortex"), "{text}");
    drop(held);
}
