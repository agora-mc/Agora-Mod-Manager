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
    RelPath, RuntimeIdentity, StoreId, VersionSource,
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
    def.import_ids.insert("vortex".into(), VORTEX_GAME.into());
    def.version_sources = vec![VersionSource::Executable {
        path: RelPath::new("Game.exe").unwrap(),
    }];
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
    // The profile keeps its own game settings and saves, as the real setup does.
    write(
        &profile.join("Skyrim.ini"),
        b"[General]\r\nsLocalTest=1\r\n",
    );
    write(
        &profile.join("SkyrimPrefs.ini"),
        b"[Display]\r\niSize H=1080\r\n",
    );
    write(
        &profile.join("SkyrimCustom.ini"),
        b"[Custom]\r\nsCustom=2\r\n",
    );
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":false}}}},"features":{{"local_game_settings":true,"local_saves":true}}}}}}"#
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
    // Vortex keeps a profile's local saves under the game's shared save folder, in a folder named
    // after the profile id.
    let rule = def
        .save_location
        .iter()
        .find(|rule| rule.applies_to_store(&StoreId::steam()))
        .expect("the test game has a save-location rule");
    let saves = agora_core::game_saves::shared_folder(rule)
        .unwrap()
        .join("p1");
    write(&saves.join("Save1.ess"), b"SAVE ONE");

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
        add_folder: false,
        copy_saves: false,
        all_mods: false,
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
    // The package still names this Vortex id, so the game is supported; the folder is what failed.
    assert!(game.supported);
    assert!(!game.needs_folder_add);
    assert!(game
        .reason
        .as_deref()
        .unwrap_or_default()
        .contains("does not hold"));
}

#[test]
fn the_manifest_decides_the_layer_order_and_disagreements_are_reported() {
    let f = fixture();
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert_eq!(plan.mods.len(), 2);
    assert_eq!(plan.mods_enabled, 2);
    assert_eq!(plan.mods_disabled, 0);
    assert_eq!(plan.mods_left_out, vec!["Gamma Mod"]);
    assert!(plan.manifest.found);
    assert_eq!(plan.manifest.matched, 3);

    let place = |id: &str| plan.order.iter().position(|m| m == id).unwrap();
    // Beta supplied the shared file, so it is above Alpha even though the rules put Alpha first.
    assert!(place("mod-beta") < place("mod-alpha"));
    assert!(!plan
        .warnings
        .iter()
        .any(|warning| warning.contains("cannot reproduce")));

    let shared = plan
        .disagreements
        .iter()
        .find(|d| d.file == "Data/shared.txt")
        .expect("the shared file is a disagreement");
    assert_eq!(shared.manifest_supplied, "Beta Mod");
    assert_eq!(shared.rules_supply, "Alpha Mod");

    assert_eq!(plan.plugins.active, 2);
    assert_eq!(plan.plugins.lines.len(), 3);
}

#[test]
fn crossed_manifest_winners_name_the_file_the_import_cannot_reproduce() {
    let f = fixture();
    write(&f.staging.join("Alpha Folder/other.txt"), b"alpha");
    write(&f.staging.join("Beta Folder/other.txt"), b"beta");
    write(
        &f._tmp.path().join("game/Data/vortex.deployment.json"),
        br#"{"files":[
            {"relPath":"shared.txt","source":"Beta Folder"},
            {"relPath":"other.txt","source":"Alpha Folder"}
        ]}"#,
    );
    let report = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect("the conflicting profile still imports");
    assert!(report
        .plan
        .warnings
        .iter()
        .any(|w| w.contains("contradictory winners")));
    assert!(report
        .plan
        .warnings
        .iter()
        .any(|w| w.contains("cannot reproduce") && w.contains("shared.txt")));
    assert!(report
        .next_steps
        .iter()
        .any(|w| w.contains("cannot reproduce") && w.contains("shared.txt")));
}

#[test]
fn disabled_mods_are_left_out_unless_all_mods_is_given() {
    let f = fixture();
    let default = request(&f, None);
    let plan = vortex::plan(&f.ctx, &f.inventory, &default).expect("the plan is made");
    assert_eq!(plan.mods.len(), 2);
    assert_eq!(plan.mods_left_out, vec!["Gamma Mod"]);

    let mut all = default;
    all.all_mods = true;
    let plan = vortex::plan(&f.ctx, &f.inventory, &all).expect("the plan is made");
    assert_eq!(plan.mods.len(), 3);
    assert_eq!(plan.mods_enabled, 2);
    assert_eq!(plan.mods_disabled, 1);
    assert!(plan.mods_left_out.is_empty());
    assert!(plan.order.iter().any(|id| id == "mod-gamma"));

    let report = vortex::run(&f.ctx, &f.inventory, &all, Default::default(), &|_| {})
        .expect("the import runs");
    assert_eq!(report.stored_mods, 3);
    assert_eq!(report.disabled_layers, 1);
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
    assert_eq!(report.stored_mods, 2);
    assert_eq!(report.content_layers, 2);
    assert_eq!(report.disabled_layers, 0);
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
fn a_profile_id_that_is_a_path_is_refused_before_a_read() {
    let f = fixture();
    // The id names the profile's folder under Vortex's user data. A path here would make every
    // read look outside it.
    let profiles = format!(
        r#"{{"p1":{{"id":"../../escape","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}}}},"features":{{}}}}}}"#
    );
    write_store(&f.store, &[("persistent###profiles", profiles)]);

    let report = vortex::scan(&f.ctx, &f.inventory, &f.store).expect("the store scans");
    let game = &report.games[0];
    assert!(game.profiles.is_empty(), "{:?}", game.profiles);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("not a folder name")),
        "{:?}",
        report.warnings
    );
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

#[test]
fn a_fresh_data_dir_says_the_folder_needs_adding_then_adds_it() {
    let f = fixture();
    let mut empty = f.inventory.clone();
    empty.installs.clear();
    empty.unsupported.clear();

    // Scan: mapped through import_ids, a real install Agora has not added.
    let scan = vortex::scan(&f.ctx, &empty, &f.store).expect("the store scans");
    let game = &scan.games[0];
    assert!(game.supported, "{game:?}");
    assert!(game.needs_folder_add, "{game:?}");
    assert!(game.install.is_none());
    assert!(game.reason.is_none());
    assert!(game
        .detection
        .as_deref()
        .unwrap_or_default()
        .contains("has not added"));

    // Import without --add-folder says exactly what to do, and writes nothing.
    let request = request(&f, None);
    let error = vortex::plan(&f.ctx, &empty, &request).expect_err("the folder is not added");
    let text = error.to_string();
    assert!(text.contains("games folder add"), "{text}");
    assert!(text.contains("--add-folder"), "{text}");
    assert!(
        agora_core::game_discovery::custom_folders::list_folders(&f.ctx.paths)
            .unwrap()
            .is_empty()
    );

    // A dry run with --add-folder plans the folder addition but writes nothing.
    let mut with_add = request.clone();
    with_add.add_folder = true;
    let plan = vortex::plan(&f.ctx, &empty, &with_add).expect("the plan is made");
    assert!(plan.would_add_folder);
    assert_eq!(plan.mods.len(), 2);
    assert_eq!(plan.mods_left_out, vec!["Gamma Mod"]);
    assert!(plan.local_settings);
    assert_eq!(plan.inis.len(), 3);
    assert!(plan.local_saves);
    assert!(!plan.saves.local);
    assert!(plan.saves.note.contains("custom folder"));
    assert!(
        agora_core::game_discovery::custom_folders::list_folders(&f.ctx.paths)
            .unwrap()
            .is_empty()
    );
    assert!(agora_core::game_instance::list(&f.ctx).unwrap().is_empty());

    // A real run adds the folder first, then imports.
    let report = vortex::run(&f.ctx, &empty, &with_add, Default::default(), &|_| {})
        .expect("the import runs");
    assert_eq!(report.stored_mods, 2);
    assert!(report.instance_id.is_some());
    let folders = agora_core::game_discovery::custom_folders::list_folders(&f.ctx.paths).unwrap();
    let added = folder_identity(&f._tmp.path().join("game"));
    assert!(
        folders
            .iter()
            .any(|folder| folder_identity(Path::new(folder)) == added),
        "{folders:?}"
    );
}

/// The identity the code compares a game folder by: the real folder, shown without the `\\?\`
/// prefix and case-folded. A temp folder can arrive in its 8.3 short form, so the test compares
/// the way the code does rather than by spelling.
fn folder_identity(path: &Path) -> String {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    agora_core::game_discovery::custom_folders::folder_key(
        &agora_core::game_discovery::custom_folders::display_path(&real),
    )
}

/// The folder's 8.3 short name, the spelling CI's temp folder can arrive as.
/// `None` when the volume does not make one, or gives the same name back.
#[cfg(windows)]
fn short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buffer = vec![0u16; 260];
    let len = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
            wide.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if len == 0 || len as usize >= buffer.len() {
        return None;
    }
    buffer.truncate(len as usize);
    let short = PathBuf::from(String::from_utf16_lossy(&buffer));
    (short != path).then_some(short)
}

/// Vortex can name a folder with its 8.3 short form while the folder Agora added
/// is stored under its long form. They are one folder, so the add must not
/// happen twice and the planned install must still be there when the run looks
/// for it.
#[test]
fn a_folder_spelled_with_its_83_short_name_is_the_same_folder() {
    let f = fixture();
    let game_dir = f._tmp.path().join("game");
    let Some(short) = short_path(&game_dir) else {
        return;
    };
    write_store(
        &f.store,
        &[(
            &format!("settings###gameMode###discovered###{VORTEX_GAME}"),
            format!(
                r#"{{"path":"{}"}}"#,
                short.to_string_lossy().replace('\\', "/")
            ),
        )],
    );

    let mut empty = f.inventory.clone();
    empty.installs.clear();
    empty.unsupported.clear();

    let mut with_add = request(&f, Some("Short Path Import"));
    with_add.add_folder = true;
    let report = vortex::run(&f.ctx, &empty, &with_add, Default::default(), &|_| {})
        .expect("the short spelling is the same folder, so the import runs");
    assert_eq!(report.stored_mods, 2);
    assert!(report.instance_id.is_some());
    let folders = agora_core::game_discovery::custom_folders::list_folders(&f.ctx.paths).unwrap();
    assert_eq!(folders.len(), 1, "the folder is added once: {folders:?}");
}

#[test]
fn a_missing_manifest_falls_back_to_vortex_rules_and_says_where_it_looked() {
    let f = fixture();
    std::fs::remove_file(f._tmp.path().join("game/Data/vortex.deployment.json")).unwrap();
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert!(!plan.manifest.found);
    assert!(plan
        .manifest
        .searched
        .iter()
        .any(|path| path.ends_with("Data/vortex.deployment.json")
            || path.ends_with("Data\\vortex.deployment.json")));
    assert!(plan
        .warnings
        .iter()
        .any(|warning| warning.contains("rules alone") && warning.contains("looked in")));
    assert_eq!(plan.mods_left_out, vec!["Gamma Mod"]);
    let place = |id: &str| plan.order.iter().position(|m| m == id).unwrap();
    // The imported mods are Alpha and Beta; the rule that names the left-out Gamma cannot order
    // them, so Alpha stays before Beta.
    assert!(place("mod-alpha") < place("mod-beta"));
}

#[test]
fn the_profiles_inis_are_planned_and_saves_counted() {
    let f = fixture();
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert!(plan.local_settings);
    let names: Vec<&str> = plan.inis.iter().map(|ini| ini.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Skyrim.ini", "SkyrimPrefs.ini", "SkyrimCustom.ini"]
    );
    assert!(plan.local_saves);
    assert_eq!(plan.saves.files, 1);
}

#[test]
fn copy_saves_copies_the_profiles_saves_and_inis() {
    let f = fixture();
    let mut request = request(&f, None);
    request.copy_saves = true;
    let report = vortex::run(&f.ctx, &f.inventory, &request, Default::default(), &|_| {})
        .expect("the import runs");
    assert_eq!(report.inis_copied.len(), 3);
    assert_eq!(report.saves_copied, 1);
    let folder = report
        .saves_folder
        .as_ref()
        .expect("the instance has its own saves");
    assert!(folder.join("Save1.ess").is_file());
}

// ---------------------------------------------------------------------------
// A Cyberpunk-shaped game: its default deploy target is the game root, not `Data`, and a
// manifest entry's `target` is the subfolder Vortex placed the file in.
// ---------------------------------------------------------------------------

const CYBERPUNK: &str = "cyberpunk-2077";
const CYBERPUNK_VORTEX_ID: &str = "cyberpunk2077";

fn cyberpunk_like() -> GameDefinition {
    let mut def = common::skyrim_definition();
    def.mo2_game_name = None;
    def.id = GameId::new(CYBERPUNK).unwrap();
    def.name = "Cyberpunk 2077".into();
    def.import_ids
        .insert("vortex".into(), CYBERPUNK_VORTEX_ID.into());
    def.version_sources = vec![VersionSource::Executable {
        path: RelPath::new("bin/x64/Cyberpunk2077.exe").unwrap(),
    }];
    def.content_layout = Some(ContentLayout {
        data_path: RelPath::default(),
        data_markers: vec![],
        root_markers: vec![],
        thunderstore_bepinex: false,
    });
    def.load_orders = vec![];
    def.save_location = vec![];
    def.user_files = vec![];
    def.save_paths = vec![];
    def
}

fn cyberpunk_install(dir: &Path) -> IdentifiedInstall {
    let store_id = StoreId::gog();
    let runtime = RuntimeIdentity {
        game: GameId::new(CYBERPUNK).unwrap(),
        store: store_id.clone(),
        version: "2.21".into(),
        build: None,
    };
    let volume = agora_core::game_discovery::volume::VolumeDetector::new().get_volume_info(dir);
    let discovered = DiscoveredInstall {
        store: store_id,
        product: "1423049311".into(),
        name: "Cyberpunk 2077".into(),
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: dir.to_path_buf(),
        store_version: Some(runtime.version.clone()),
        store_build: None,
        executables: vec!["bin/x64/Cyberpunk2077.exe".into()],
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume,
    };
    IdentifiedInstall {
        game: runtime.game.clone(),
        install_id: InstallId::new("gog:1423049311").unwrap(),
        discovered,
        add_ons: vec![],
        runtime: RuntimeResolution::Identified {
            runtime,
            source: "executable".into(),
        },
    }
}

struct CyberpunkFixture {
    _lock: MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user_data: TempDir,
    ctx: CoreContext,
    inventory: GameInventory,
    store: PathBuf,
}

fn cyberpunk_fixture() -> CyberpunkFixture {
    let lock = USER_DATA.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let user_data = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data.path());

    let def = cyberpunk_like();
    let pkg = PackageDefinition {
        id: format!("test.{CYBERPUNK}"),
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
    write(&game_dir.join("bin/x64/Cyberpunk2077.exe"), b"EXE");
    std::fs::copy(
        r"C:\Windows\System32\cmd.exe",
        game_dir.join("bin/x64/Cyberpunk2077.exe"),
    )
    .unwrap();
    let store = tmp.path().join("Vortex").join("state.v2");
    std::fs::create_dir_all(&store).unwrap();
    let staging = tmp
        .path()
        .join("Vortex")
        .join(CYBERPUNK_VORTEX_ID)
        .join("mods");
    write(
        &staging.join("Mod Folder/archive/pc/mod/foo.archive"),
        b"ARCHIVE",
    );
    // The default manifest sits at the game root; `target` says where the file went.
    write(
        &game_dir.join("vortex.deployment.json"),
        br#"{
  "version": 1,
  "instance": "test-instance",
  "files": [
    { "relPath": "foo.archive", "source": "Mod Folder", "target": "archive/pc/mod", "time": 1 }
  ]
}"#,
    );
    let mods = format!(
        r#"{{"{CYBERPUNK_VORTEX_ID}":{{"cpmod":{{"id":"cpmod","state":"installed","installationPath":"Mod Folder","attributes":{{"name":"Archive Mod","version":"1.0"}}}}}}}}"#
    );
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{CYBERPUNK_VORTEX_ID}","name":"Main","modState":{{"cpmod":{{"enabled":true}}}}}}}}"#
    );
    write_store(
        &store,
        &[
            ("persistent###mods", mods),
            ("persistent###profiles", profiles),
            (
                &format!("settings###mods###installPath###{CYBERPUNK_VORTEX_ID}"),
                "\"{USERDATA}/{GAME}/mods\"".to_string(),
            ),
            (
                &format!("settings###gameMode###discovered###{CYBERPUNK_VORTEX_ID}"),
                format!(
                    r#"{{"path":"{}"}}"#,
                    game_dir.to_string_lossy().replace('\\', "/")
                ),
            ),
        ],
    );
    let inventory = GameInventory {
        installs: vec![cyberpunk_install(&game_dir)],
        unsupported: vec![],
    };
    CyberpunkFixture {
        _lock: lock,
        _tmp: tmp,
        _user_data: user_data,
        ctx,
        inventory,
        store,
    }
}

#[test]
fn a_manifest_at_the_game_root_is_found_and_its_target_subfolder_is_honoured() {
    let f = cyberpunk_fixture();
    let scan = vortex::scan(&f.ctx, &f.inventory, &f.store).expect("the store scans");
    let game = &scan.games[0];
    assert!(game.supported, "{game:?}");
    assert_eq!(game.install.as_deref(), Some("gog:1423049311"));
    assert!(game.manifest_found, "{game:?}");
    assert_eq!(game.manifest_files, 1);
    assert!(!game.needs_folder_add);

    let request = VortexRequest {
        store: f.store.clone(),
        game: GameId::new(CYBERPUNK).unwrap(),
        profile: "Main".into(),
        name: None,
        install: None,
        add_folder: false,
        copy_saves: false,
        all_mods: false,
    };
    let plan = vortex::plan(&f.ctx, &f.inventory, &request).expect("the plan is made");
    assert!(plan.manifest.found);
    assert_eq!(plan.manifest.matched, 1);
    assert_eq!(plan.order, vec!["cpmod"]);
    assert!(plan.manifest.path.ends_with("vortex.deployment.json"));
}

/// The real official Skyrim package, loaded from `packages/official` through the same JSON and
/// inheritance path the registry uses, then a whole scan → refuse → dry-run → run sequence.
#[test]
fn the_real_skyrim_package_maps_and_imports_a_not_yet_added_folder() {
    use agora_core::game_discovery::custom_folders;
    use agora_core::game_registry::inherit::{self, PackageInput};
    use agora_core::game_registry::PackageSource;
    use serde_json::Value;

    struct UserDataGuard;
    impl Drop for UserDataGuard {
        fn drop(&mut self) {
            std::env::remove_var("AGORA_TEST_USER_DATA_ROOT");
        }
    }

    struct RealPackage(PackageDefinition);
    impl GamePackage for RealPackage {
        fn definition(&self) -> &PackageDefinition {
            &self.0
        }
    }

    let _lock = USER_DATA.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let user_data = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data.path());
    let _guard = UserDataGuard;

    let packages = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/official");
    let read = |relative: &str| -> Value {
        let path = packages.join(relative);
        serde_json::from_str(&std::fs::read_to_string(&path).expect("the package file is read"))
            .expect("the package file is JSON")
    };
    let input = |plugin: &str, relative: &str| PackageInput {
        source: PackageSource::Plugin {
            plugin_id: plugin.to_string(),
        },
        blocked: false,
        label: format!("Plugin {plugin}, game package {relative}"),
        json: read(relative),
    };
    let (resolved, warnings) = inherit::resolve(vec![
        input(
            "agora.creation-engine",
            "creation-engine/games/package.json",
        ),
        input("agora.skyrim-se", "skyrim-se/games/package.json"),
    ]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut builder = GameRegistry::builder();
    for package in resolved {
        if package.definition.games.is_empty() {
            continue;
        }
        builder
            .add(package.source, Arc::new(RealPackage(package.definition)))
            .expect("the package registers");
    }
    let registry = Arc::new(builder.build());
    let definition = registry
        .game(&GameId::new("skyrim-se").unwrap())
        .expect("the real Skyrim definition is registered");
    assert_eq!(
        definition.import_ids.get("vortex").map(String::as_str),
        Some("skyrimse")
    );

    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let ctx = ctx.with_games(registry);

    // An invented folder that passes the real package's executable detection.
    let game_dir = tmp.path().join("Skyrim Special Edition");
    std::fs::create_dir_all(&game_dir).unwrap();
    std::fs::copy(
        r"C:\Windows\System32\cmd.exe",
        game_dir.join("SkyrimSE.exe"),
    )
    .unwrap();
    write(&game_dir.join("Data/Skyrim.esm"), b"MASTER");

    let store = tmp.path().join("Vortex").join("state.v2");
    std::fs::create_dir_all(&store).unwrap();
    let staging = tmp.path().join("Vortex").join("skyrimse").join("mods");
    write(&staging.join("Alpha Mod/readme.txt"), b"MOD");
    let profile = tmp
        .path()
        .join("Vortex")
        .join("skyrimse")
        .join("profiles")
        .join("p1");
    write(
        &profile.join("Skyrim.ini"),
        b"[General]\r\nsLocalTest=1\r\n",
    );
    write(
        &profile.join("SkyrimPrefs.ini"),
        b"[Display]\r\niSize H=1080\r\n",
    );
    write(
        &profile.join("SkyrimCustom.ini"),
        b"[Custom]\r\nsCustom=2\r\n",
    );
    let mods = r#"{"skyrimse":{"mod-alpha":{"id":"mod-alpha","state":"installed","installationPath":"Alpha Mod","attributes":{"name":"Alpha Mod","version":"1.0","modId":101}}}}"#;
    let profiles = r#"{"p1":{"id":"p1","gameId":"skyrimse","name":"Test Profile","modState":{"mod-alpha":{"enabled":true}},"features":{"local_game_settings":true}}}"#;
    write_store(
        &store,
        &[
            ("persistent###mods", mods.to_string()),
            ("persistent###profiles", profiles.to_string()),
            (
                "settings###mods###installPath###skyrimse",
                "\"{USERDATA}/{GAME}/mods\"".to_string(),
            ),
            (
                "settings###gameMode###discovered###skyrimse",
                format!(
                    r#"{{"path":"{}"}}"#,
                    game_dir.to_string_lossy().replace('\\', "/")
                ),
            ),
        ],
    );
    let empty = GameInventory {
        installs: Vec::new(),
        unsupported: Vec::new(),
    };

    // The scan maps the Vortex id through the real package and says the folder is not added.
    let scan = vortex::scan(&ctx, &empty, &store).expect("the store scans");
    let game = &scan.games[0];
    assert!(game.supported, "{game:?}");
    assert_eq!(game.game.as_ref(), Some(&GameId::new("skyrim-se").unwrap()));
    assert!(game.needs_folder_add, "{game:?}");
    assert!(game
        .detection
        .as_deref()
        .unwrap_or_default()
        .contains("has not added"));

    let request = VortexRequest {
        store: store.clone(),
        // The owner's command passes the Vortex id the scan prints.
        game: GameId::new("skyrimse").unwrap(),
        profile: "Test Profile".into(),
        name: None,
        install: None,
        add_folder: false,
        copy_saves: false,
        all_mods: false,
    };
    let error = vortex::plan(&ctx, &empty, &request).expect_err("the folder is not added");
    assert!(error.to_string().contains("--add-folder"), "{error}");
    assert!(error.to_string().contains("games folder add"), "{error}");
    assert!(custom_folders::list_folders(&ctx.paths).unwrap().is_empty());

    let mut with_add = request.clone();
    with_add.add_folder = true;
    let plan = vortex::plan(&ctx, &empty, &with_add).expect("the dry run plans");
    assert!(plan.would_add_folder);
    assert_eq!(plan.game.as_str(), "skyrim-se");
    assert_eq!(plan.mods.len(), 1);
    assert_eq!(plan.inis.len(), 3);
    assert!(custom_folders::list_folders(&ctx.paths).unwrap().is_empty());
    assert!(agora_core::game_instance::list(&ctx).unwrap().is_empty());

    let report = vortex::run(&ctx, &empty, &with_add, Default::default(), &|_| {})
        .expect("the real run imports");
    assert!(report.instance_id.is_some());
    assert_eq!(report.stored_mods, 1);
    let folders = custom_folders::list_folders(&ctx.paths).unwrap();
    let added = folder_identity(&game_dir);
    assert!(
        folders
            .iter()
            .any(|folder| folder_identity(Path::new(folder)) == added),
        "{folders:?}"
    );
}

#[test]
fn a_star_format_list_can_have_no_active_plugins() {
    let f = fixture();
    let profile = f
        ._tmp
        .path()
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p1");
    write(
        &profile.join("plugins.txt"),
        b"# Automatically generated by Vortex\r\nAlpha.esp\r\nBeta.esp\r\n",
    );
    let report = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect("the profile imports with all plugins off");
    let plan = report.plan;
    assert_eq!(plan.plugins.active, 0);
    assert!(report.plugins_written.is_some());
    let line = |name: &str| {
        plan.plugins
            .lines
            .iter()
            .find(|line| line.name == name)
            .expect("the plugin is listed")
    };
    assert!(!line("Alpha.esp").active);
    assert!(!line("Beta.esp").active);
    assert!(!line("Gamma.esp").active);
}

#[test]
fn a_profiles_userlist_becomes_load_after_rules_and_refuses_bad_ones() {
    use agora_core::game_load_order;

    let f = fixture();
    let profile = f
        ._tmp
        .path()
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p1");
    write(
        &profile.join("userlist.yaml"),
        b"plugins:\n  - name: 'Alpha.esp'\n    after:\n      - 'Beta.esp'\n  - name: 'Beta.esp'\n    after: ['Alpha.esp']\n  - name: 'Missing.esp'\n    after:\n      - 'Alpha.esp'\n  - name: 'Gamma.esp'\n    after:\n      - 'Gamma.esp'\n  - name: 'Beta.esp'\n    req:\n      - 'Alpha.esp'\n    inc:\n      - 'Gamma.esp'\n",
    );
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert_eq!(plan.userlist.rules.len(), 1);
    assert_eq!(plan.userlist.rules[0].item, "Alpha.esp");
    assert_eq!(plan.userlist.rules[0].after, "Beta.esp");
    assert_eq!(plan.userlist.refused.len(), 3);
    assert!(plan
        .userlist
        .refused
        .iter()
        .any(|rule| rule.reason.contains("cycle")));
    assert!(plan
        .userlist
        .refused
        .iter()
        .any(|rule| rule.reason.contains("itself")));
    assert!(plan
        .userlist
        .refused
        .iter()
        .any(|rule| rule.reason.contains("not in the profile")));
    assert_eq!(plan.userlist.not_imported.len(), 2);

    let report = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect("the import runs");
    assert_eq!(report.rules_added, 1);
    let instance = report.instance_id.as_deref().expect("an instance was made");
    let rules = game_load_order::load_after_rules(&f.ctx, instance, "plugins").expect("rules read");
    assert!(rules
        .iter()
        .any(|rule| rule.item == "Alpha.esp" && rule.after == "Beta.esp"));
}

/// A second profile, "Alt", enables every mod; "Main" leaves Gamma off. Both keep
/// the same staging folder, as Vortex really does.
fn add_second_profile(f: &Fixture) {
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":false}}}},"features":{{"local_game_settings":true,"local_saves":true}}}},"p2":{{"id":"p2","gameId":"{VORTEX_GAME}","name":"Alt","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":true}}}},"features":{{}}}}}}"#
    );
    write_store(&f.store, &[("persistent###profiles", profiles)]);
    let alt = f
        ._tmp
        .path()
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p2");
    write(
        &alt.join("plugins.txt"),
        b"# Alt\r\n*Alpha.esp\r\n*Beta.esp\r\n*Gamma.esp\r\n",
    );
}

fn chosen_profiles() -> Vec<String> {
    vec!["Main".to_string(), "Alt".to_string()]
}

fn enabled_count(ctx: &CoreContext, instance: &str, profile: &str) -> usize {
    let dir = ctx.paths.instance_dir(instance).unwrap();
    let profiles = agora_core::loadout::list_profiles(&dir).unwrap();
    profiles
        .iter()
        .find(|p| p.name == profile)
        .and_then(|p| p.game.as_ref())
        .map(|game| game.content.iter().filter(|item| item.enabled).count())
        .unwrap_or_else(|| panic!("no loadout '{profile}' on {instance}"))
}

#[test]
fn a_merged_import_makes_one_instance_and_a_loadout_per_profile() {
    let f = fixture();
    add_second_profile(&f);
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Merged Setup")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merge runs");

    assert_eq!(report.instances.len(), 1, "{report:?}");
    assert_eq!(report.loadouts, chosen_profiles());
    let instance = &report.instances[0];
    let manifest = agora_core::game_instance::get_manifest(&f.ctx, instance).unwrap();
    let content: Vec<_> = manifest
        .layers
        .layers()
        .iter()
        .filter(|layer| matches!(layer.source, agora_game_api::LayerSource::Content { .. }))
        .collect();
    assert_eq!(content.len(), 3, "the union holds every mod: {content:?}");
    // The first profile is applied, so the instance as it stands has Main's two.
    assert_eq!(content.iter().filter(|layer| layer.enabled).count(), 2);
    assert_eq!(enabled_count(&f.ctx, instance, "Main"), 2);
    assert_eq!(enabled_count(&f.ctx, instance, "Alt"), 3);
}

#[test]
fn a_separate_import_makes_one_instance_per_profile_with_only_its_mods() {
    let f = fixture();
    add_second_profile(&f);
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Sep Setup")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Separate,
        Default::default(),
        &|_| {},
    )
    .expect("the separate import runs");
    assert_eq!(report.instances.len(), 2, "{report:?}");
    assert!(report.loadouts.is_empty());
    let mut counts: Vec<usize> = report
        .instances
        .iter()
        .map(|instance| {
            let manifest = agora_core::game_instance::get_manifest(&f.ctx, instance).unwrap();
            manifest
                .layers
                .layers()
                .iter()
                .filter(|layer| matches!(layer.source, agora_game_api::LayerSource::Content { .. }))
                .count()
        })
        .collect();
    counts.sort_unstable();
    assert_eq!(counts, vec![2, 3], "Main has two mods, Alt three");
}

#[test]
fn the_suggestion_groups_the_two_overlapping_profiles() {
    use agora_core::game_import::profiles;

    let f = fixture();
    add_second_profile(&f);
    let sets = vortex::profile_sets(&f.ctx, &f.inventory, &f.store, GAME).unwrap();
    assert_eq!(sets.profiles.len(), 2);
    let suggestion = profiles::suggest_grouping(&sets.profiles);
    assert_eq!(suggestion.groups.len(), 1, "{suggestion:?}");
    assert_eq!(
        suggestion.groups[0].profiles,
        vec!["Alt".to_string(), "Main".to_string()]
    );
    let pair = &suggestion.pairs[0];
    assert_eq!(pair.shared, 2);
    assert_eq!(pair.union, 3);
    assert!(pair.ratio > profiles::OVERLAP_THRESHOLD);

    // Two profiles with nothing in common stay apart.
    let apart = profiles::suggest_grouping(&[
        profiles::ProfileSet {
            name: "One".into(),
            enabled: ["a".to_string()].into_iter().collect(),
        },
        profiles::ProfileSet {
            name: "Two".into(),
            enabled: ["b".to_string()].into_iter().collect(),
        },
    ]);
    assert_eq!(apart.groups.len(), 2);
    assert_eq!(apart.pairs[0].shared, 0);
}

#[test]
fn an_import_that_cannot_fit_is_refused_after_stating_the_bytes() {
    use agora_core::game_import::profiles::Grouping;
    use agora_core::game_import::ImportError;

    let f = fixture();
    add_second_profile(&f);
    let request = request(&f, Some("Too Big"));
    let plan = vortex::plan_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        Grouping::Merged,
    )
    .expect("the plan is made");
    assert!(plan.bytes > 0);
    assert!(plan.fits, "the real disk has room");

    let hits_before = f.ctx.test_seams.volume_hits();
    f.ctx
        .test_seams
        .set_volume_seam(Some(agora_core::game_tool_swap::VolumeSeam {
            free_bytes: Some(1),
            same_volume: None,
        }));
    let error = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect_err("one byte cannot hold the merge");
    f.ctx.test_seams.set_volume_seam(None);
    assert!(
        f.ctx.test_seams.volume_hits() > hits_before,
        "the free-space stand-in was installed but never consulted"
    );
    assert!(
        matches!(error, ImportError::NotEnoughSpace { .. }),
        "{error}"
    );
    assert!(agora_core::game_instance::list(&f.ctx).unwrap().is_empty());
}

fn loadout(f: &Fixture, instance: &str, name: &str) -> agora_core::loadout::LoadoutProfile {
    let dir = f.ctx.paths.instance_dir(instance).unwrap();
    agora_core::loadout::list_profiles(&dir)
        .expect("the loadouts list")
        .into_iter()
        .find(|profile| profile.name == name)
        .unwrap_or_else(|| panic!("no loadout '{name}' on {instance}"))
}

fn content_layer_names(f: &Fixture, instance: &str) -> Vec<String> {
    let manifest = agora_core::game_instance::get_manifest(&f.ctx, instance).unwrap();
    manifest
        .layers
        .layers()
        .iter()
        .filter_map(|layer| match &layer.source {
            agora_game_api::LayerSource::Content { content } => {
                agora_core::content_store::get_item(&f.ctx, content)
                    .ok()
                    .map(|item| item.name)
            }
            _ => None,
        })
        .collect()
}

/// The manifest says Alpha supplied the shared file, so Alpha is above Beta even though the rules
/// alone put Beta above Alpha. Alt adds Gamma (which the rules put between them); the merge must
/// keep the single-import order, not append Gamma last.
#[test]
fn a_merged_import_layers_later_profiles_in_vortex_order() {
    let f = fixture();
    add_second_profile(&f);
    write(
        &f._tmp.path().join("game/Data/vortex.deployment.json"),
        br#"{
  "version": 1,
  "instance": "test-instance",
  "files": [
    { "relPath": "Alpha.esp", "source": "Alpha Folder", "time": 1 },
    { "relPath": "Beta.esp", "source": "Beta Folder", "time": 1 },
    { "relPath": "Gamma.esp", "source": "Gamma Folder", "time": 1 },
    { "relPath": "shared.txt", "source": "Alpha Folder", "time": 1 }
  ]
}"#,
    );
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Ordered Merge")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merge runs");
    let instance = &report.instances[0];
    let main = loadout(&f, instance, "Main").game.unwrap();
    let alt = loadout(&f, instance, "Alt").game.unwrap();
    assert_eq!(main.layer_order.len(), 3);
    assert_eq!(alt.layer_order.len(), 3);
    assert_ne!(main.layer_order, alt.layer_order);
    // Main puts the disabled Gamma layer below its enabled layers.
    assert_eq!(
        content_layer_names(&f, instance),
        vec!["Gamma Mod", "Beta Mod", "Alpha Mod"],
        "{:?}",
        report.plan.plans[0].order
    );
    agora_core::loadout::game::apply_game_profile_with_undo(
        &f.ctx,
        instance,
        &skyrim_like(),
        "Alt",
        false,
    )
    .unwrap();
    assert_eq!(
        content_layer_names(&f, instance),
        vec!["Beta Mod", "Gamma Mod", "Alpha Mod"],
    );
}

#[test]
fn separate_import_reports_the_completed_profile_and_the_next_profiles_error() {
    use agora_core::game_import::{profiles::Grouping, ImportPhase};

    let f = fixture();
    add_second_profile(&f);
    let mut request = request(&f, Some("Separate Results"));
    request.copy_saves = true;
    let gamma = f.staging.join("Gamma Folder/Gamma.esp");
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        Grouping::Separate,
        Default::default(),
        &|progress| {
            if progress.phase == ImportPhase::Storing
                && progress.current_mod.as_deref() == Some("Gamma Mod")
            {
                std::fs::remove_file(&gamma).unwrap();
            }
        },
    )
    .expect("a failure during a profile returns a batch report");
    assert_eq!(report.instances.len(), 1);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["profiles"][0]["instance_id"], report.instances[0]);
    let written = json["profiles"][0]["external_files_written"]
        .as_array()
        .unwrap();
    assert_eq!(written.len(), 1);
    assert_eq!(
        std::fs::read(written[0].as_str().unwrap()).unwrap(),
        b"SAVE ONE"
    );
    assert_eq!(json["profiles"][1]["profile"], "Alt");
    assert!(json["profiles"][1]["error"]
        .as_str()
        .unwrap()
        .contains("Gamma Mod"));
}

#[test]
fn preflight_counts_the_shared_mods_once_and_copied_saves_once_per_profile() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    add_second_profile(&f);
    let mut request = request(&f, None);
    request.copy_saves = true;
    request.all_mods = true;
    for grouping in [Grouping::Separate, Grouping::Merged] {
        let plan =
            vortex::plan_profiles(&f.ctx, &f.inventory, &request, &chosen_profiles(), grouping)
                .unwrap();
        let ini_bytes: u64 = plan.plans[0]
            .inis
            .iter()
            .map(|ini| std::fs::metadata(&ini.from).unwrap().len())
            .sum();
        assert_eq!(
            plan.bytes,
            plan.plans[0].bytes + ini_bytes + b"SAVE ONE".len() as u64
        );
    }
    let plan = vortex::plan_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &["Main".into()],
        Grouping::Merged,
    )
    .unwrap();
    let ini_bytes: u64 = plan.plans[0]
        .inis
        .iter()
        .map(|ini| std::fs::metadata(&ini.from).unwrap().len())
        .sum();
    assert_eq!(
        plan.bytes,
        plan.plans[0].bytes + ini_bytes + b"SAVE ONE".len() as u64
    );
    let hits_before = f.ctx.test_seams.volume_hits();
    f.ctx
        .test_seams
        .set_volume_seam(Some(agora_core::game_tool_swap::VolumeSeam {
            free_bytes: Some(plan.bytes - 1),
            same_volume: None,
        }));
    let error = vortex::run(&f.ctx, &f.inventory, &request, Default::default(), &|_| {})
        .expect_err("the single-profile import also checks the requested saves before copying");
    f.ctx.test_seams.set_volume_seam(None);
    assert!(
        f.ctx.test_seams.volume_hits() > hits_before,
        "the free-space stand-in was installed but never consulted"
    );
    assert!(matches!(error, ImportError::NotEnoughSpace { needed, .. } if needed == plan.bytes));
    assert!(agora_core::game_instance::list(&f.ctx).unwrap().is_empty());
}

#[test]
fn separate_import_reports_saves_written_before_a_profile_failed() {
    use agora_core::game_import::{profiles::Grouping, ImportPhase};

    let f = fixture();
    add_second_profile(&f);
    let def = skyrim_like();
    let rule = agora_core::game_saves::rule_for(&def, &StoreId::steam()).unwrap();
    let last = agora_core::game_saves::shared_folder(rule)
        .unwrap()
        .join("p1/Z-last.ess");
    write(&last, b"will disappear");
    let mut request = request(&f, Some("Save Journal"));
    request.copy_saves = true;
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        Grouping::Separate,
        Default::default(),
        &|progress| {
            if progress.phase == ImportPhase::Finishing && last.is_file() {
                std::fs::remove_file(&last).unwrap();
            }
        },
    )
    .unwrap();
    let main = &report.profiles[0];
    assert!(main.instance_id.is_none());
    assert!(main.error.as_ref().unwrap().contains("Z-last.ess"));
    assert_eq!(main.external_files_written.len(), 1);
    assert_eq!(
        std::fs::read(&main.external_files_written[0]).unwrap(),
        b"SAVE ONE"
    );
    assert_eq!(
        report.profiles[1].instance_id.as_ref(),
        report.instances.first()
    );
}

#[test]
fn a_merged_profile_without_its_own_settings_uses_the_shared_ones() {
    let f = fixture();
    add_second_profile(&f);
    // Alt keeps no INIs of its own (its profile has no local_game_settings) and no plugins list.
    let alt = f
        ._tmp
        .path()
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p2");
    std::fs::remove_file(alt.join("plugins.txt")).unwrap();

    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Shared Settings")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merge runs");
    let instance = &report.instances[0];

    let main = loadout(&f, instance, "Main").game.expect("a game loadout");
    assert_eq!(main.inis.len(), 4);
    assert!(
        main.inis.iter().all(|ini| ini.contents.is_some()),
        "Main keeps its own INIs: {:?}",
        main.inis
    );
    assert!(
        main.plugin_lists.iter().all(|list| list.contents.is_some()),
        "Main keeps its own plugin list"
    );

    let alt = loadout(&f, instance, "Alt").game.expect("a game loadout");
    assert_eq!(alt.inis.len(), main.inis.len());
    assert!(
        alt.inis.iter().all(|ini| ini.contents.is_none()),
        "Alt has no INIs of its own, so it uses the game's shared ones: {:?}",
        alt.inis
    );
    assert!(
        alt.plugin_lists.iter().all(|list| list.contents.is_none()),
        "Alt has no plugins.txt of its own, so it uses the game's shared list: {:?}",
        alt.plugin_lists
    );
}

/// Two mods with the same file name and bytes are one content item. Each profile still enables
/// its own mod, so neither loadout may switch the shared layer off.
#[test]
fn two_mods_with_identical_bytes_stay_enabled_in_each_profiles_loadout() {
    let f = fixture();
    write(
        &f.staging.join("Delta Folder/Shared.esp"),
        b"THE SAME PLUGIN",
    );
    write(
        &f.staging.join("Echo Folder/Shared.esp"),
        b"THE SAME PLUGIN",
    );
    let mods = format!(
        r#"{{"{VORTEX_GAME}":{{
  "mod-alpha": {{"id":"mod-alpha","state":"installed","installationPath":"Alpha Folder","attributes":{{"name":"Alpha Mod"}}}},
  "mod-beta": {{"id":"mod-beta","state":"installed","installationPath":"Beta Folder","attributes":{{"name":"Beta Mod"}}}},
  "mod-delta": {{"id":"mod-delta","state":"installed","installationPath":"Delta Folder","attributes":{{"name":"Delta Mod"}}}},
  "mod-echo": {{"id":"mod-echo","state":"installed","installationPath":"Echo Folder","attributes":{{"name":"Echo Mod"}}}}
}}}}"#
    );
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-delta":{{"enabled":true}}}},"features":{{}}}},"p2":{{"id":"p2","gameId":"{VORTEX_GAME}","name":"Alt","modState":{{"mod-beta":{{"enabled":true}},"mod-echo":{{"enabled":true}}}},"features":{{}}}}}}"#
    );
    write_store(
        &f.store,
        &[
            ("persistent###mods", mods),
            ("persistent###profiles", profiles),
        ],
    );

    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Same Bytes")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merge runs");
    let instance = &report.instances[0];
    let content = content_layer_names(&f, instance);
    assert_eq!(content.len(), 3, "{content:?}");
    assert_eq!(
        enabled_count(&f.ctx, instance, "Main"),
        2,
        "Main enables Alpha and Delta's shared layer"
    );
    assert_eq!(
        enabled_count(&f.ctx, instance, "Alt"),
        2,
        "Alt enables Beta and Echo's shared layer"
    );
}

#[test]
fn merged_copy_saves_copies_each_profiles_saves_and_says_which() {
    let f = fixture();
    add_second_profile(&f);
    let def = skyrim_like();
    let rule = agora_core::game_saves::rule_for(&def, &StoreId::steam()).expect("a save rule");
    let alt_saves = agora_core::game_saves::shared_folder(rule)
        .expect("the shared save folder")
        .join("p2");
    write(&alt_saves.join("AltSave.ess"), b"ALT SAVE");
    // The same name as Main's save, with different bytes: the two setups must not collide.
    write(&alt_saves.join("Save1.ess"), b"ALT SAVE WITH THE SAME NAME");
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":false}}}},"features":{{"local_game_settings":true,"local_saves":true}}}},"p2":{{"id":"p2","gameId":"{VORTEX_GAME}","name":"Alt","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}},"mod-gamma":{{"enabled":true}}}},"features":{{"local_saves":true}}}}}}"#
    );
    write_store(&f.store, &[("persistent###profiles", profiles)]);

    let mut request = request(&f, Some("Merged Saves"));
    request.copy_saves = true;
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merge runs");
    let instance = &report.instances[0];

    let main = report
        .saves
        .iter()
        .find(|save| save.profile == "Main")
        .expect("Main's saves are reported");
    assert_eq!(main.copied, 1);
    let alt = report
        .saves
        .iter()
        .find(|save| save.profile == "Alt")
        .expect("Alt's saves are reported");
    assert_eq!(alt.copied, 2, "{alt:?}");

    let main_folder = agora_core::game_saves::own_folder_scoped(rule, instance, Some("Main"))
        .expect("Main's own save folder");
    let alt_folder = agora_core::game_saves::own_folder_scoped(rule, instance, Some("Alt"))
        .expect("Alt's own save folder");
    assert_ne!(main_folder, alt_folder, "each loadout has its own folder");
    let main_loadout = loadout(&f, instance, "Main").game.expect("Main's loadout");
    assert_eq!(
        main_loadout.save_folder.as_deref(),
        Some("Main"),
        "a merged profile never adopts the instance's single folder"
    );
    let alt_loadout = loadout(&f, instance, "Alt").game.expect("Alt's loadout");
    assert_eq!(alt_loadout.save_folder.as_deref(), Some("Alt"));
    assert_eq!(
        std::fs::read(main_folder.join("Save1.ess")).unwrap(),
        b"SAVE ONE",
        "Main's same-named save is its own"
    );
    assert_eq!(
        std::fs::read(alt_folder.join("Save1.ess")).unwrap(),
        b"ALT SAVE WITH THE SAME NAME",
        "Alt's same-named save was not overwritten by Main's"
    );
    assert!(
        alt_folder.join("AltSave.ess").is_file(),
        "Alt's other save was copied too"
    );
}

#[test]
fn merged_copy_saves_a_profile_with_no_save_folder_yet_copies_nothing() {
    let f = fixture();
    add_second_profile(&f);
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}}}},"features":{{"local_game_settings":true,"local_saves":true}}}},"p2":{{"id":"p2","gameId":"{VORTEX_GAME}","name":"Alt","modState":{{"mod-gamma":{{"enabled":true}}}},"features":{{"local_saves":true}}}}}}"#
    );
    write_store(&f.store, &[("persistent###profiles", profiles)]);

    let mut request = request(&f, Some("No Alt Saves"));
    request.copy_saves = true;
    let report = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request,
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("a profile with no saves folder yet is not an error");
    let alt = report
        .saves
        .iter()
        .find(|save| save.profile == "Alt")
        .expect("Alt's saves are reported");
    assert_eq!(alt.copied, 0);
    assert_eq!(alt.skipped_existing, 0);
}

#[test]
fn a_failure_after_the_merge_started_abandons_the_half_made_instance() {
    use agora_core::game_import::ImportPhase;

    let f = fixture();
    add_second_profile(&f);
    // Alt keeps its own INI, planned before the run. It is removed while run() makes the first
    // profile's instance, so reading it during the merge fails after the instance exists.
    let profiles = format!(
        r#"{{"p1":{{"id":"p1","gameId":"{VORTEX_GAME}","name":"Main","modState":{{"mod-alpha":{{"enabled":true}},"mod-beta":{{"enabled":true}}}},"features":{{"local_game_settings":true,"local_saves":true}}}},"p2":{{"id":"p2","gameId":"{VORTEX_GAME}","name":"Alt","modState":{{"mod-gamma":{{"enabled":true}}}},"features":{{"local_game_settings":true}}}}}}"#
    );
    write_store(&f.store, &[("persistent###profiles", profiles)]);
    let alt_ini = f
        ._tmp
        .path()
        .join("Vortex")
        .join(VORTEX_GAME)
        .join("profiles")
        .join("p2")
        .join("Skyrim.ini");
    write(&alt_ini, b"[General]\r\nsAlt=1\r\n");

    let error = vortex::run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Broken Merge")),
        &chosen_profiles(),
        agora_core::game_import::profiles::Grouping::Merged,
        Default::default(),
        &|progress| {
            if progress.phase == ImportPhase::Instance {
                let _ = std::fs::remove_file(&alt_ini);
            }
        },
    )
    .expect_err("the missing profile INI fails the merge");
    assert!(error.to_string().contains("could not be read"), "{error}");
    assert!(
        agora_core::game_instance::list(&f.ctx).unwrap().is_empty(),
        "the half-made instance was left behind"
    );
}

// -- Vortex mod types (V3) --------------------------------------------------------------------

/// Add one extra mod to the store and its staging folder. `mod_type` is Vortex's raw `IMod.type`
/// id, or `None` when the field is absent.
fn add_typed_mod(f: &Fixture, id: &str, folder: &str, name: &str, mod_type: Option<&str>) {
    write(&f.staging.join(folder).join("Tool.ini"), b"[tool]\n");
    let mut db = DB::open(
        &f.store,
        Options {
            create_if_missing: true,
            ..Options::default()
        },
    )
    .unwrap();
    let raw = db.get(b"persistent###mods").unwrap();
    let mut mods: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let mut entry = serde_json::json!({
        "id": id,
        "state": "installed",
        "installationPath": folder,
        "attributes": { "name": name }
    });
    if let Some(kind) = mod_type {
        entry["type"] = serde_json::Value::String(kind.to_string());
    }
    mods[VORTEX_GAME][id] = entry;
    db.put(b"persistent###mods", &serde_json::to_vec(&mods).unwrap())
        .unwrap();

    let raw = db.get(b"persistent###profiles").unwrap();
    let mut profiles: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    profiles["p1"]["modState"][id] = serde_json::json!({ "enabled": true });
    db.put(
        b"persistent###profiles",
        &serde_json::to_vec(&profiles).unwrap(),
    )
    .unwrap();
    db.flush().unwrap();
}

fn mount_of_named_layer(f: &Fixture, instance: &str, name: &str) -> String {
    let manifest = agora_core::game_instance::get_manifest(&f.ctx, instance).unwrap();
    manifest
        .layers
        .layers()
        .iter()
        .find_map(|layer| match &layer.source {
            agora_game_api::LayerSource::Content { content } => {
                let item = agora_core::content_store::get_item(&f.ctx, content).ok()?;
                (item.name == name).then(|| layer.mount_path.as_str().to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no content layer named '{name}'"))
}

#[test]
fn a_root_type_mod_is_placed_at_the_game_root() {
    let f = fixture();
    add_typed_mod(&f, "mod-root", "Root Folder", "Root Tool", Some("root"));
    let report = vortex::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None),
        Default::default(),
        &|_| {},
    )
    .expect("the import runs");
    let instance = report.instance_id.expect("an instance was made");
    assert_eq!(
        mount_of_named_layer(&f, &instance, "Root Tool"),
        "",
        "a mod of Vortex's root type deploys to the game root, not the data folder"
    );
    assert_eq!(
        mount_of_named_layer(&f, &instance, "Alpha Mod"),
        "Data",
        "the ordinary mod keeps the data-folder placement"
    );
}

#[test]
fn an_unknown_mod_type_is_imported_with_a_warning_that_names_it() {
    let f = fixture();
    add_typed_mod(
        &f,
        "mod-odd",
        "Odd Folder",
        "Odd Tool",
        Some("community-tool"),
    );
    let plan = vortex::plan(&f.ctx, &f.inventory, &request(&f, None)).expect("the plan is made");
    assert!(
        plan.mods.iter().any(|item| item.name == "Odd Tool"),
        "the mod is still imported, not silently dropped"
    );
    let warning = plan
        .warnings
        .iter()
        .find(|warning| warning.contains("community-tool"))
        .expect("a warning names the unknown type");
    assert!(warning.contains("Odd Tool"), "{warning}");
    assert!(warning.contains("guess"), "{warning}");
}
