//! Importing a Mod Organizer 2 setup (MASTER_SPEC §26.10): a synthetic setup built in each test, the
//! hostile names an MO2 file can hold, the layer and plugin orders the import sets, the saves and INI
//! copies, refusals, interruption and re-run, and the proof that the setup itself is never written.
//!
//! The real 214,592-file setup is read by the `#[ignore]` tests in `crates/agora/tests`, never here.

#![cfg(windows)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use agora_core::content_store::{self, ContentSource, Mo2Provenance};
use agora_core::ctx::CoreContext;
use agora_core::game_deploy::{self, DeployMode, FileSource};
use agora_core::game_discovery::{DiscoveredInstall, DiscoveryReport, InstallCapabilities};
use agora_core::game_import::{self, ImportError, Mo2Request, RunOptions};
use agora_core::game_instance;
use agora_core::game_launch::SystemLauncher;
use agora_core::game_plugins;
use agora_core::game_registry::{
    GameInventory, GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::game_saves;
use agora_game_api::{
    ContentLayout, GameDefinition, GameId, GamePackage, GamePath, InputFingerprint, InstallId,
    InstallKind, LaunchRecipe, LayerSource, LoadOrderDefinition, PackageDefinition, RelPath,
    RuntimeIdentity, StoreId, ToolDefinition, ToolId, ToolOutput,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const GAME: &str = "skyrim-se";
const NEMESIS_PATTERNS: [&str; 7] = [
    "Data/meshes/actors/character/**",
    "Data/meshes/animationdatasinglefile.txt",
    "Data/meshes/animationsetdatasinglefile.txt",
    "Data/Nemesis_Engine/**",
    "Data/scripts/FNIS.pex",
    "Data/scripts/FNIS_aa.pex",
    "Data/scripts/Nemesis_AA_Core.pex",
];

/// The user-data folders are redirected by `AGORA_TEST_USER_DATA_ROOT`, a process-wide setting.
static USER_DATA: Mutex<()> = Mutex::new(());

struct Fixture {
    _lock: MutexGuard<'static, ()>,
    _tmp: TempDir,
    _user_data: TempDir,
    ctx: CoreContext,
    def: GameDefinition,
    inventory: GameInventory,
    /// The MO2 setup folder: `ModOrganizer.ini`, `mods`, `overwrite`, `profiles`.
    setup: PathBuf,
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

/// Skyrim SE as the shipped package declares it, for the parts an import reads: the MO2 name, the
/// plugin list, the content layout, the Nemesis tool with its output patterns, and the user files.
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
        items: agora_game_api::LoadOrderItems {
            folder: RelPath::new("Data").unwrap(),
            patterns: vec!["*.esm".into(), "*.esl".into(), "*.esp".into()],
            folders: false,
        },
        format: agora_game_api::LoadOrderFormat::Lines {
            active_prefix: "*".into(),
            header: vec![
                "# This file is used by Skyrim to keep track of your downloaded content.".into(),
                "# Please do not modify this file.".into(),
            ],
        },
        rules: agora_game_api::LoadOrderRules {
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
    def.tool_ids = vec![ToolId::new("nemesis").unwrap()];
    def.launch = Some(LaunchRecipe {
        executable: GamePath::Runtime {
            path: RelPath::new("Game.exe").unwrap(),
        },
        arguments: vec![],
        environment: Default::default(),
        working_directory: GamePath::Runtime {
            path: RelPath::default(),
        },
    });
    def
}

fn nemesis() -> ToolDefinition {
    ToolDefinition {
        output: ToolOutput::GeneratedLayer,
        output_arg: None,
        output_patterns: NEMESIS_PATTERNS.iter().map(|p| p.to_string()).collect(),
        id: ToolId::new("nemesis").unwrap(),
        game: GameId::new(GAME).unwrap(),
        name: "Nemesis".into(),
        launch: LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        },
        relevant_settings: vec![],
        after_tools: vec![],
        conflicts_with: vec![],
        uses_install_path: false,
        required_outputs: Vec::new(),
        failure_markers: Vec::new(),
        required_magic: Vec::new(),
        run_only: false,
        executable_names: Vec::new(),
        inputs: Vec::new(),
        scope: Default::default(),
        input_files: Vec::new(),
        before_launch: None,
    }
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

/// The MO2 `gamePath` value as MO2 writes it: backslashes doubled inside `@ByteArray(...)`.
fn game_path_value(path: &Path) -> String {
    format!(
        "@ByteArray({})",
        path.to_string_lossy().replace('\\', "\\\\")
    )
}

fn build_setup(setup: &Path, game_path: &Path) {
    write(
        &setup.join("ModOrganizer.ini"),
        format!(
            "[General]\r\ngameName=Skyrim Special Edition\r\nselected_profile=@ByteArray(Test Profile)\r\ngamePath={}\r\n[Settings]\r\nprofiles_directory=%BASE_DIR%/profiles\r\n",
            game_path_value(game_path)
        )
        .as_bytes(),
    );

    // Alpha: enabled, highest-priority mod with provenance; it has a plugin and a shared file.
    write(&setup.join("mods/Alpha/Alpha.esp"), b"ALPHA PLUGIN");
    write(&setup.join("mods/Alpha/shared.txt"), b"from alpha");
    write(
        &setup.join("mods/Alpha/meta.ini"),
        b"[General]\r\ngameName=skyrimspecialedition\r\nmodid=42\r\nversion=1.2\r\ninstallationFile=D:/downloads/Alpha-42.7z\r\nrepository=Nexus\r\n",
    );
    // Beta: disabled, no meta.ini at all.
    write(&setup.join("mods/Beta/Beta.esp"), b"BETA PLUGIN");
    write(&setup.join("mods/Beta/beta.txt"), b"beta only");
    // Gamma: enabled, lowest real mod; its meta.ini is binary garbage.
    write(&setup.join("mods/Gamma/Gamma.esp"), b"GAMMA PLUGIN");
    write(&setup.join("mods/Gamma/shared.txt"), b"from gamma");
    write(
        &setup.join("mods/Gamma/meta.ini"),
        &[0u8, 159, 146, 150, 0, 1, 2],
    );

    // A folder that a hostile name would reach: next to `mods`, so `..\outside` finds it.
    write(&setup.join("outside/secret.txt"), b"never imported");

    // The overwrite folder: Nemesis-like output, and files no tool claims.
    write(
        &setup.join("overwrite/meshes/actors/character/behaviors/idle.hkx"),
        b"HKX",
    );
    write(
        &setup.join("overwrite/Nemesis_Engine/cache/behavior_path"),
        b"CACHE",
    );
    write(&setup.join("overwrite/scripts/FNIS.pex"), b"PEX");
    write(&setup.join("overwrite/meshes/armor/helm.nif"), b"NIF");
    write(&setup.join("overwrite/scripts/custom.pex"), b"CUSTOM");
    write(&setup.join("overwrite/FNIS.esp"), b"FNIS ESP");
    write(&setup.join("overwrite/SKSE/plugin.txt"), b"SKSE");

    // The profile.
    let profile = setup.join("profiles/Test Profile");
    write(
        &profile.join("modlist.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\n\
+Alpha\r\n\
+--- Textures_separator\r\n\
-Beta\r\n\
+Gamma\r\n\
*DLC: Dawnguard\r\n\
+..\\outside\r\n\
+C:\\Windows\r\n\
+bad/name\r\n\
+Missing\r\n",
    );
    write(
        &profile.join("plugins.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\n*Alpha.esp\r\n*Gamma.esp\r\n*DLCX.esm\r\n",
    );
    write(
        &profile.join("loadorder.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\nSkyrim.esm\r\nUpdate.esm\r\nAlpha.esp\r\nBeta.esp\r\nGamma.esp\r\nDLCX.esm\r\n",
    );
    write(
        &profile.join("lockedorder.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\nGamma.esp|2\r\n",
    );
    write(
        &profile.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=true\r\nAutomaticArchiveInvalidation=true\r\n",
    );
    write(
        &profile.join("Skyrim.ini"),
        b"[General]\r\nsLocalTest=1\r\n",
    );
    write(
        &profile.join("skyrimcustom.ini"),
        b"[Custom]\r\nsCustom=2\r\n",
    );
    write(&profile.join("saves/Save1.ess"), b"SAVE ONE");
    write(&profile.join("saves/Save1.skse"), b"SKSE CO-SAVE");
}

fn fixture() -> Fixture {
    let lock = USER_DATA.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let user_data = TempDir::new().unwrap();
    std::env::set_var("AGORA_TEST_USER_DATA_ROOT", user_data.path());

    let def = skyrim_like();
    let tool = nemesis();
    let pkg = PackageDefinition {
        id: format!("test.{GAME}"),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![def.clone()],
        frameworks: vec![],
        tools: vec![tool],
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
    write(&game_dir.join("Data/Skyrim.esm"), b"MASTER");
    std::fs::copy(r"C:\Windows\System32\cmd.exe", game_dir.join("Game.exe")).unwrap();
    let setup = tmp.path().join("setup");
    build_setup(&setup, &game_dir);

    let inventory = GameInventory {
        installs: vec![install_of(&game_dir)],
        unsupported: vec![],
    };
    Fixture {
        _lock: lock,
        _tmp: tmp,
        _user_data: user_data,
        ctx,
        def,
        inventory,
        setup,
    }
}

fn request(f: &Fixture, name: Option<&str>, copy_saves: bool) -> Mo2Request {
    Mo2Request {
        ini_path: f.setup.join("ModOrganizer.ini"),
        profile: "Test Profile".into(),
        name: name.map(str::to_string),
        copy_saves,
        install: None,
    }
}

/// Every file under `root` with its SHA-256, keyed by relative path.
fn tree_hashes(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let bytes = std::fs::read(&path).unwrap();
                out.insert(rel, format!("{:x}", Sha256::digest(&bytes)));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The store's items. An unreadable record fails the test: the import must not damage the store.
fn stored_items(ctx: &CoreContext) -> Vec<content_store::ContentItem> {
    content_store::list_items(ctx)
        .unwrap()
        .into_iter()
        .map(|entry| entry.into_item().expect("every stored item reads"))
        .collect()
}

fn item_names(ctx: &CoreContext) -> BTreeMap<String, String> {
    stored_items(ctx)
        .into_iter()
        .map(|i| (i.item_id.clone(), i.name.clone()))
        .collect()
}

fn item_id_named(ctx: &CoreContext, name: &str) -> String {
    item_names(ctx)
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(id, _)| id)
        .unwrap_or_else(|| panic!("no content item named {name}"))
}

// ---------------------------------------------------------------------------
// Hostile names: the probe, written first
// ---------------------------------------------------------------------------

/// A modlist that names a path outside `mods`, a drive, a separator, and a folder that is not there.
/// Each is reported and none is read: the `outside` folder's file never reaches the content store.
#[test]
fn progress_names_each_mod_and_counts_bytes_up_to_the_total_then_the_unmeasured_stages() {
    use game_import::ImportPhase;
    let f = fixture();
    let seen = std::cell::RefCell::new(Vec::new());
    game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|progress| seen.borrow_mut().push(progress),
    )
    .unwrap();
    let seen = seen.into_inner();
    let storing: Vec<_> = seen
        .iter()
        .filter(|p| p.phase == ImportPhase::Storing)
        .collect();
    assert_eq!(
        storing.len(),
        storing[0].mods_total * 2,
        "one before and one after each mod"
    );
    assert_eq!(storing[0].mods_done, 0);
    assert_eq!(storing[0].bytes_done, 0);
    assert!(
        storing[0].current_mod.is_some(),
        "the first report names the mod being stored"
    );
    assert!(storing
        .windows(2)
        .all(|w| w[0].bytes_done <= w[1].bytes_done));
    let last = storing.last().unwrap();
    assert_eq!(last.mods_done, last.mods_total);
    assert_eq!(last.bytes_done, last.bytes_total, "the bar reaches its end");
    assert_eq!(last.current_mod, None);

    // The stages after the mods come in order, and carry no mod.
    let rest: Vec<ImportPhase> = seen
        .iter()
        .filter(|p| p.phase != ImportPhase::Storing && p.phase != ImportPhase::Overwrite)
        .map(|p| p.phase)
        .collect();
    assert_eq!(
        rest,
        [
            ImportPhase::Instance,
            ImportPhase::Layers,
            ImportPhase::Finishing
        ]
    );
}

#[test]
fn hostile_modlist_names_are_reported_and_never_read() {
    let f = fixture();
    let report = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();

    let mods: Vec<&str> = report.mods.iter().map(|m| m.folder.as_str()).collect();
    assert_eq!(
        mods,
        vec!["Alpha", "Beta", "Gamma"],
        "only real folders are planned, in modlist order"
    );
    let problems: Vec<&str> = report.problems.iter().map(|p| p.entry.as_str()).collect();
    for hostile in ["..\\outside", "C:\\Windows", "bad/name", "Missing"] {
        assert!(
            problems.contains(&hostile),
            "{hostile} should be reported, got {problems:?}"
        );
    }
    assert!(
        report.problems.iter().all(|p| p.line.is_some()),
        "every problem names its modlist line"
    );

    // Nothing in the plan came from outside mods: the secret file is not counted anywhere.
    assert!(report
        .mods
        .iter()
        .all(|m| m.path.starts_with(f.setup.join("mods"))));

    // Running it stores the real mods and never the secret.
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    assert!(!run.interrupted);
    let stored: Vec<String> = stored_items(&f.ctx)
        .iter()
        .flat_map(|i| i.files.iter().map(|file| file.path.as_str().to_string()))
        .collect();
    assert!(
        stored.iter().all(|p| !p.contains("secret")),
        "the file outside mods was read: {stored:?}"
    );
}

/// A `meta.ini` that is binary garbage is recorded as unreadable, and the mod still imports.
#[test]
fn binary_meta_ini_is_recorded_and_the_mod_still_imports() {
    let f = fixture();
    let report = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();
    let gamma = report.mods.iter().find(|m| m.folder == "Gamma").unwrap();
    assert!(matches!(gamma.provenance, Mo2Provenance::Unreadable { .. }));
    let beta = report.mods.iter().find(|m| m.folder == "Beta").unwrap();
    assert_eq!(beta.provenance, Mo2Provenance::Absent);
}

// ---------------------------------------------------------------------------
// The full import
// ---------------------------------------------------------------------------

#[test]
fn import_stores_bytes_orders_layers_and_leaves_the_setup_alone() {
    let f = fixture();
    let before = tree_hashes(&f.setup);

    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    assert!(!run.interrupted);
    assert_eq!(run.plan.mods_enabled, 2);
    assert_eq!(run.plan.mods_disabled, 1);
    assert_eq!(run.plan.separators, 1);
    assert_eq!(run.plan.unmanaged, vec!["DLC: Dawnguard".to_string()]);

    // The setup is untouched: every file hashes the same before and after.
    assert_eq!(before, tree_hashes(&f.setup), "the setup was written to");

    let instance_id = run.instance_id.clone().expect("an instance was made");
    let manifest = game_instance::get_manifest(&f.ctx, &instance_id).unwrap();
    let rec = manifest
        .imported_from
        .clone()
        .expect("the import is recorded");
    assert_eq!(rec.profile, "Test Profile");
    assert_eq!(rec.kind, "mo2");

    // Layers: Gamma lowest, then Beta (disabled), then Alpha, then the overwrite layer, then Nemesis.
    let names = item_names(&f.ctx);
    let order: Vec<String> = manifest
        .layers
        .layers()
        .iter()
        .map(|l| match &l.source {
            LayerSource::Content { content } => names.get(content).cloned().unwrap_or_default(),
            LayerSource::Generated { tool, .. } => format!("generated:{tool}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        order,
        vec![
            "Gamma",
            "Beta",
            "Alpha",
            "MO2 overwrite",
            "generated:nemesis"
        ]
    );
    let beta_layer = manifest
        .layers
        .layers()
        .iter()
        .find(|l| matches!(&l.source, LayerSource::Content { content } if names.get(content).map(String::as_str) == Some("Beta")))
        .unwrap();
    assert!(!beta_layer.enabled, "a disabled mod is a disabled layer");

    // Two mods with the same file: Alpha is higher, so its shared.txt is the one deployed.
    let plan = game_deploy::plan(&f.ctx, &instance_id, &f.def, DeployMode::Links).unwrap();
    let shared = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == "Data/shared.txt")
        .expect("shared.txt is deployed");
    match &shared.source {
        FileSource::Content { item_id, .. } => {
            assert_eq!(
                item_id,
                &item_id_named(&f.ctx, "Alpha"),
                "the higher mod wins"
            );
        }
        other => panic!("shared.txt came from {other:?}"),
    }

    // The Nemesis output is a generated layer whose inputs are unknown; the other overwrite files
    // are in the overwrite layer.
    let generated = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == "Data/meshes/actors/character/behaviors/idle.hkx")
        .expect("the Nemesis behaviour is deployed");
    assert!(matches!(generated.source, FileSource::Generated { .. }));
    let (gen_id, inputs) = match &manifest
        .layers
        .layers()
        .iter()
        .find(|l| matches!(l.source, LayerSource::Generated { .. }))
        .unwrap()
        .source
    {
        LayerSource::Generated {
            generation, inputs, ..
        } => (generation.clone(), inputs.clone()),
        _ => unreachable!(),
    };
    assert_eq!(gen_id, "1");
    assert_eq!(inputs, InputFingerprint::Unknown);
    let helm = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == "Data/meshes/armor/helm.nif")
        .expect("the unclaimed mesh is deployed");
    assert!(matches!(&helm.source, FileSource::Content { .. }));
    let fnis_esp = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == "Data/FNIS.esp")
        .unwrap();
    assert!(
        matches!(&fnis_esp.source, FileSource::Content { .. }),
        "FNIS.esp stays in overwrite"
    );

    // Plugins: MO2's order and activation, its lock, and Skyrim's own masters left out.
    let listed = game_plugins::list(&f.ctx, &instance_id, &f.def).unwrap();
    let entries: Vec<(&str, bool, bool, bool)> = listed
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.active, e.managed, e.locked))
        .collect();
    assert_eq!(
        entries,
        vec![
            ("Alpha.esp", true, true, false),
            ("Beta.esp", false, false, false),
            ("Gamma.esp", true, true, true),
            ("DLCX.esm", true, false, false),
        ]
    );
    assert_eq!(run.plan.plugins.locked, vec!["Gamma.esp".to_string()]);

    // Local INIs: the profile's copies are the instance's copies.
    let skyrim_ini = std::fs::read(
        f.ctx
            .paths
            .instance_dir(&instance_id)
            .unwrap()
            .join("user/Skyrim.ini"),
    )
    .unwrap();
    // The copy keeps the profile's line, and the saves choice adds its own save folder.
    let skyrim_ini = String::from_utf8(skyrim_ini).unwrap();
    assert!(skyrim_ini.contains("sLocalTest=1"), "{skyrim_ini}");
    assert!(
        skyrim_ini.contains("SLocalSavePath=Saves\\Agora\\"),
        "{skyrim_ini}"
    );
    assert_eq!(run.inis_copied.len(), 2);

    // Saves: the choice is own, and without --copy-saves nothing reached Documents.
    assert_eq!(manifest.saves, game_instance::SavesChoice::Own);
    let rule = game_saves::rule_for(&f.def, &StoreId::steam())
        .unwrap()
        .clone();
    let own = game_saves::own_folder(&rule, &instance_id).unwrap();
    assert!(
        !own.exists(),
        "saves were written to Documents without the flag"
    );
    assert_eq!(run.saves_copied, 0);
    assert!(run.next_steps.iter().any(|s| s.contains("--copy-saves")));

    // Provenance: Alpha's claim is recorded, labelled as unverified; Gamma's garbage is noted.
    let alpha = content_store::get_item(&f.ctx, &item_id_named(&f.ctx, "Alpha")).unwrap();
    let claimed = alpha.sources.iter().find_map(|s| match s {
        ContentSource::Mo2Import { provenance, .. } => Some(provenance.clone()),
        _ => None,
    });
    match claimed {
        Some(Mo2Provenance::Claimed { modid, note, .. }) => {
            assert_eq!(modid, Some(42));
            assert_eq!(note, "source claimed, not verified");
        }
        other => panic!("Alpha's provenance was {other:?}"),
    }
}

#[test]
fn a_second_import_is_refused_unless_it_is_named() {
    let f = fixture();
    game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();

    let refused = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false));
    match refused {
        Err(ImportError::AlreadyImported { instance }) => assert!(!instance.is_empty()),
        other => panic!("expected a refusal, got {:?}", other.map(|p| p.profile)),
    }

    let named = game_import::plan(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Second copy"), false),
    );
    assert!(named.is_ok(), "a new name allows the import");
}

#[test]
fn an_interrupted_import_finishes_on_rerun() {
    let f = fixture();
    let first = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions {
            stop_after_mods: Some(1),
        },
        &|_| {},
    )
    .unwrap();
    assert!(first.interrupted);
    assert!(
        game_instance::list(&f.ctx).unwrap().is_empty(),
        "no instance before every item is stored"
    );
    let stored_after_first = content_store::list_items(&f.ctx).unwrap().len();
    assert_eq!(stored_after_first, 1);

    let second = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    assert!(!second.interrupted);
    assert_eq!(game_instance::list(&f.ctx).unwrap().len(), 1);
    // Alpha was stored by the first run and is reused, not stored twice.
    let items = stored_items(&f.ctx);
    let mut ids: Vec<&String> = items.iter().map(|i| &i.item_id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), items.len());
}

#[test]
fn a_dry_run_stores_nothing() {
    let f = fixture();
    let plan = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();
    assert_eq!(plan.mods.len(), 3);
    assert!(content_store::list_items(&f.ctx).unwrap().is_empty());
    assert!(game_instance::list(&f.ctx).unwrap().is_empty());
}

#[test]
fn a_gamepath_with_no_install_stops_with_a_message() {
    let f = fixture();
    let elsewhere = f._tmp.path().join("not-installed");
    build_setup(&f.setup, &elsewhere);
    let result = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false));
    match result {
        Err(e @ ImportError::NoInstall { .. }) => {
            let text = e.to_string();
            assert!(text.contains("not-installed"), "{text}");
            assert!(
                text.contains("--install"),
                "the CLI is told how to choose: {text}"
            );
            let ImportError::NoInstall {
                candidates,
                game_id,
                ..
            } = e
            else {
                unreachable!()
            };
            assert_eq!(candidates, vec!["steam:489830".to_string()]);
            assert_eq!(game_id.as_str(), GAME);
        }
        other => panic!("expected NoInstall, got {:?}", other.map(|p| p.profile)),
    }
}

/// The owner's case: the setup names a Steam folder the game is no longer in, while Agora found
/// the game somewhere else. Naming that install imports onto it.
#[test]
fn a_gamepath_with_no_install_imports_onto_the_install_that_is_named() {
    let f = fixture();
    let elsewhere = f._tmp.path().join("not-installed");
    build_setup(&f.setup, &elsewhere);
    let mut req = request(&f, None, false);
    req.install = Some("steam:489830".into());
    let plan = game_import::plan(&f.ctx, &f.inventory, &req).unwrap();
    assert_eq!(plan.install_id, "steam:489830");
    assert_eq!(
        plan.install_location,
        f.inventory.installs[0].discovered.location
    );
    assert_eq!(plan.setup_game_path, elsewhere);
    assert_eq!(plan.mods.len(), 3);

    let report = game_import::run(&f.ctx, &f.inventory, &req, RunOptions::default(), &|_| {})
        .expect("the import runs onto the named install");
    assert!(report.instance_id.is_some());
}

#[test]
fn an_install_of_another_game_is_refused() {
    let mut f = fixture();
    let mut other = install_of(&f._tmp.path().join("game"));
    other.game = GameId::new("other-game").unwrap();
    other.install_id = InstallId::new("steam:377160").unwrap();
    f.inventory.installs.push(other);
    let mut req = request(&f, None, false);
    req.install = Some("steam:377160".into());
    match game_import::plan(&f.ctx, &f.inventory, &req) {
        Err(ImportError::InstallOfAnotherGame { install, .. }) => {
            assert_eq!(install, "steam:377160")
        }
        other => panic!(
            "expected InstallOfAnotherGame, got {:?}",
            other.map(|p| p.profile)
        ),
    }
}

#[test]
fn an_install_id_agora_did_not_find_is_refused() {
    let f = fixture();
    let mut req = request(&f, None, false);
    req.install = Some("gog:nope".into());
    match game_import::plan(&f.ctx, &f.inventory, &req) {
        Err(ImportError::NoSuchInstall(id)) => assert_eq!(id, "gog:nope"),
        other => panic!("expected NoSuchInstall, got {:?}", other.map(|p| p.profile)),
    }
}

#[test]
fn no_installs_at_all_still_stops_with_a_message() {
    let mut f = fixture();
    f.inventory.installs.clear();
    match game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)) {
        Err(e @ ImportError::NoInstall { .. }) => {
            let text = e.to_string();
            assert!(text.contains("Install the game"), "{text}");
            let ImportError::NoInstall { candidates, .. } = e else {
                unreachable!()
            };
            assert!(candidates.is_empty());
        }
        other => panic!("expected NoInstall, got {:?}", other.map(|p| p.profile)),
    }
}

#[test]
fn saves_are_copied_only_with_the_flag_and_never_moved() {
    let f = fixture();
    // First import: no flag, so the saves stay where they are.
    let first = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let rule = game_saves::rule_for(&f.def, &StoreId::steam())
        .unwrap()
        .clone();
    let own_first = game_saves::own_folder(&rule, first.instance_id.as_deref().unwrap()).unwrap();
    assert!(!own_first.exists());

    // Second import, named, with the flag: the files are copied into the instance's own folder.
    let second = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("With saves"), true),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    assert_eq!(second.saves_copied, 2);
    let own = game_saves::own_folder(&rule, second.instance_id.as_deref().unwrap()).unwrap();
    assert_eq!(std::fs::read(own.join("Save1.ess")).unwrap(), b"SAVE ONE");
    assert_eq!(
        std::fs::read(f.setup.join("profiles/Test Profile/saves/Save1.ess")).unwrap(),
        b"SAVE ONE",
        "the profile's saves are copied, not moved"
    );
}

/// Two mods with identical bytes share one content item: the second is reported as a duplicate and
/// adds no layer of its own.
#[test]
fn identical_mods_share_one_layer() {
    let f = fixture();
    let gamma = f.setup.join("mods/Gamma");
    let delta = f.setup.join("mods/Delta");
    for name in ["Gamma.esp", "shared.txt", "meta.ini"] {
        write(&delta.join(name), &std::fs::read(gamma.join(name)).unwrap());
    }
    std::fs::write(
        f.setup.join("profiles/Test Profile/modlist.txt"),
        "+Delta\r\n+Gamma\r\n",
    )
    .unwrap();
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    assert_eq!(run.duplicates.len(), 1, "{:?}", run.duplicates);
    assert_eq!(
        run.content_layers, 2,
        "one mod layer and the overwrite layer"
    );
    let manifest =
        game_instance::get_manifest(&f.ctx, run.instance_id.as_deref().unwrap()).unwrap();
    assert_eq!(
        manifest.layers.layers().len(),
        3,
        "mod, overwrite, and Nemesis"
    );
}

/// A top-level `meta.ini` is MO2's metadata: it is left out of the stored content, so it never deploys
/// as `Data\meta.ini`, but its claims are still read. A `meta.ini` deeper in a mod is content and stays.
#[test]
fn top_level_meta_ini_is_provenance_not_content() {
    let f = fixture();
    write(
        &f.setup.join("mods/Alpha/sub/meta.ini"),
        b"[General]\r\nmodid=7\r\n",
    );
    // Beta has no meta.ini: give it one in a different case, which is still top-level metadata.
    write(
        &f.setup.join("mods/Beta/Meta.INI"),
        b"[General]\r\nmodid=9\r\n",
    );

    let plan = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();
    // Alpha's and Gamma's top-level files, and Beta's differently-cased one.
    assert_eq!(plan.meta_ini_excluded, 3);
    let beta = plan.mods.iter().find(|m| m.folder == "Beta").unwrap();
    assert!(beta.meta_ini_excluded);
    assert!(matches!(
        beta.provenance,
        Mo2Provenance::Claimed { modid: Some(9), .. }
    ));

    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let alpha = content_store::get_item(&f.ctx, &item_id_named(&f.ctx, "Alpha")).unwrap();
    let paths: Vec<&str> = alpha.files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"sub/meta.ini"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.eq_ignore_ascii_case("meta.ini")),
        "{paths:?}"
    );
    let claimed = alpha.sources.iter().find_map(|s| match s {
        ContentSource::Mo2Import { provenance, .. } => Some(provenance.clone()),
        _ => None,
    });
    assert!(
        matches!(
            claimed,
            Some(Mo2Provenance::Claimed {
                modid: Some(42),
                ..
            })
        ),
        "Alpha's top-level meta.ini is still read as provenance: {claimed:?}"
    );

    let instance_id = run.instance_id.clone().unwrap();
    let deployed = game_deploy::plan(&f.ctx, &instance_id, &f.def, DeployMode::Links).unwrap();
    assert!(
        !deployed
            .files
            .iter()
            .any(|p| p.path.as_str().eq_ignore_ascii_case("Data/meta.ini")),
        "a top-level meta.ini was deployed"
    );
    assert!(deployed
        .files
        .iter()
        .any(|p| p.path.as_str() == "Data/sub/meta.ini"));
}

#[test]
fn a_plan_names_the_sources_it_would_use() {
    let f = fixture();
    let plan = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();
    assert_eq!(plan.overwrite.generated.len(), 1);
    assert_eq!(plan.overwrite.generated[0].tool, "nemesis");
    assert_eq!(plan.overwrite.generated[0].files, 3);
    assert_eq!(plan.overwrite.rest_files, 4);
    assert_eq!(plan.saves.files, 2);
    assert!(plan.plugins.kept);
}

#[test]
fn the_setup_is_refused_when_it_is_not_there() {
    let f = fixture();
    let mut req = request(&f, None, false);
    req.ini_path = f.setup.join("nope/ModOrganizer.ini");
    assert!(matches!(
        game_import::plan(&f.ctx, &f.inventory, &req),
        Err(ImportError::NoSetup(_))
    ));
}

#[test]
fn a_chosen_setup_is_summarised_and_a_missing_one_is_refused() {
    let f = fixture();
    let ini = f.setup.join("ModOrganizer.ini");
    let found = game_import::inspect_setup(&ini).expect("the setup the user chose is readable");
    assert_eq!(found.ini_path, ini);
    assert_eq!(found.game_name.as_deref(), Some("Skyrim Special Edition"));
    assert_eq!(found.profiles, vec!["Test Profile".to_string()]);
    assert_eq!(
        found.mod_count,
        Some(3),
        "Alpha, Beta and Gamma are the setup's mod folders"
    );
    assert_eq!(found.found_by, "chosen by you");

    // A chosen path that is not there is refused, never summarised as an empty setup.
    let missing = f.setup.join("nope/ModOrganizer.ini");
    assert!(matches!(
        game_import::inspect_setup(&missing),
        Err(ImportError::NoSetup(_))
    ));
}

#[test]
fn an_unknown_game_is_refused() {
    let f = fixture();
    let text = std::fs::read_to_string(f.setup.join("ModOrganizer.ini")).unwrap();
    std::fs::write(
        f.setup.join("ModOrganizer.ini"),
        text.replace("gameName=Skyrim Special Edition", "gameName=Rimworld"),
    )
    .unwrap();
    assert!(matches!(
        game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)),
        Err(ImportError::UnknownGame(_))
    ));
}

#[test]
fn a_profile_that_is_not_there_is_refused() {
    let f = fixture();
    let mut req = request(&f, None, false);
    req.profile = "Nope".into();
    assert!(matches!(
        game_import::plan(&f.ctx, &f.inventory, &req),
        Err(ImportError::NoProfile { .. })
    ));
}

// ---------------------------------------------------------------------------
// Plugin activation after an import: a deploy or a launch must keep MO2's state
// ---------------------------------------------------------------------------

/// The active and managed flags of one plugin line in the instance's list.
fn plugin_state(f: &Fixture, instance_id: &str, name: &str) -> (bool, bool) {
    game_plugins::list(&f.ctx, instance_id, &f.def)
        .unwrap()
        .entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name))
        .map(|e| (e.active, e.managed))
        .unwrap_or_else(|| panic!("{name} is not in the plugin list"))
}

/// Replace the profile's `plugins.txt`, which the import reads for each line's activation.
fn set_profile_plugins(f: &Fixture, text: &str) {
    write(
        &f.setup.join("profiles/Test Profile/plugins.txt"),
        text.as_bytes(),
    );
}

/// The plugins a deploy reports it activated.
fn activated_by_deploy(outcome: &game_deploy::DeployOutcome) -> Vec<String> {
    outcome
        .plugins()
        .map(|r| r.added.clone())
        .unwrap_or_default()
}

/// Launch preparation for an instance, as a launch would do it. It does not start the game.
fn prepare_launch(f: &Fixture, instance_id: &str) {
    let report = DiscoveryReport {
        installs: vec![f.inventory.installs[0].discovered.clone()],
        warnings: vec![],
    };
    game_instance::prepare_launch_with(
        &f.ctx,
        instance_id,
        &f.def,
        game_instance::LaunchOptions {
            on_vfs_failure: game_instance::VfsFailure::FallBack,
            ..Default::default()
        },
        &|| report.clone(),
        &SystemLauncher,
    )
    .expect("launch preparation");
}

/// The MO2 overwrite layer deploys `FNIS.esp`. MO2 lists it inactive, and the import, a deploy and
/// a launch preparation must all leave it inactive.
#[test]
fn an_inactive_overwrite_plugin_stays_inactive_through_import_deploy_and_launch() {
    let f = fixture();
    set_profile_plugins(
        &f,
        "# This file was automatically generated by Mod Organizer.\r\n*Alpha.esp\r\n*Gamma.esp\r\n*DLCX.esm\r\nFNIS.esp\r\n",
    );
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let id = run.instance_id.clone().expect("an instance was made");
    assert_eq!(plugin_state(&f, &id, "FNIS.esp"), (false, true));

    let outcome = game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    // The check is only meaningful if the deploy really counts FNIS.esp as a deployed plugin.
    let plan = game_deploy::plan(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    let fnis = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == "Data/FNIS.esp")
        .expect("FNIS.esp is deployed");
    assert!(matches!(fnis.source, FileSource::Content { .. }));
    assert!(
        !activated_by_deploy(&outcome)
            .iter()
            .any(|n| n == "FNIS.esp"),
        "a deploy activated FNIS.esp"
    );
    assert_eq!(plugin_state(&f, &id, "FNIS.esp"), (false, true));

    prepare_launch(&f, &id);
    assert_eq!(plugin_state(&f, &id, "FNIS.esp"), (false, true));
}

/// An enabled mod's plugin that MO2 lists inactive stays inactive through a deploy and a launch.
#[test]
fn an_inactive_plugin_in_an_enabled_mod_stays_inactive_through_deploy_and_launch() {
    let f = fixture();
    set_profile_plugins(
        &f,
        "# This file was automatically generated by Mod Organizer.\r\nAlpha.esp\r\n*Gamma.esp\r\n*DLCX.esm\r\n",
    );
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let id = run.instance_id.clone().expect("an instance was made");
    assert_eq!(plugin_state(&f, &id, "Alpha.esp"), (false, true));

    let outcome = game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    assert!(!activated_by_deploy(&outcome)
        .iter()
        .any(|n| n == "Alpha.esp"));
    assert_eq!(plugin_state(&f, &id, "Alpha.esp"), (false, true));

    prepare_launch(&f, &id);
    assert_eq!(plugin_state(&f, &id, "Alpha.esp"), (false, true));
}

/// A disabled layer's plugin stays inactive and unmanaged. Enabling the layer then activates it,
/// the way a newly added mod's plugin is activated.
#[test]
fn a_disabled_layers_plugin_stays_inactive_until_its_layer_is_enabled() {
    let f = fixture();
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let id = run.instance_id.clone().expect("an instance was made");
    assert_eq!(plugin_state(&f, &id, "Beta.esp"), (false, false));

    game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    assert_eq!(plugin_state(&f, &id, "Beta.esp"), (false, false));
    prepare_launch(&f, &id);
    assert_eq!(plugin_state(&f, &id, "Beta.esp"), (false, false));

    let beta = item_id_named(&f.ctx, "Beta");
    game_deploy::set_content_enabled(&f.ctx, &id, &beta, true).unwrap();
    let outcome = game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    assert_eq!(activated_by_deploy(&outcome), vec!["Beta.esp".to_string()]);
    assert_eq!(plugin_state(&f, &id, "Beta.esp"), (true, true));
}

/// An overwrite plugin the game's install already has is the game's own file, so the import does
/// not manage it: a deploy must not remove its line, which the user's MO2 state sets.
#[test]
fn an_overwrite_plugin_the_install_already_has_is_not_managed() {
    let f = fixture();
    let game = f.inventory.installs[0].discovered.location.clone();
    write(&game.join("Data/Base.esp"), b"BASE PLUGIN");
    write(&f.setup.join("overwrite/Base.esp"), b"OVERWRITE COPY");
    set_profile_plugins(
        &f,
        "# This file was automatically generated by Mod Organizer.\r\n*Alpha.esp\r\n*Gamma.esp\r\n*DLCX.esm\r\nBase.esp\r\n",
    );
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let id = run.instance_id.clone().expect("an instance was made");
    assert_eq!(plugin_state(&f, &id, "Base.esp"), (false, false));

    game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    assert_eq!(plugin_state(&f, &id, "Base.esp"), (false, false));
}

/// An enabled mod ships a plugin the game's own data folder also has, and MO2 lists it active. The
/// import must not manage it, so a deploy and a launch keep its line, still active.
#[test]
fn an_enabled_mods_plugin_the_install_already_has_keeps_its_active_line() {
    let f = fixture();
    let game = f.inventory.installs[0].discovered.location.clone();
    write(&game.join("Data/Shared.esp"), b"BASE PLUGIN");
    write(&f.setup.join("mods/Alpha/Shared.esp"), b"ALPHA COPY");
    set_profile_plugins(
        &f,
        "# This file was automatically generated by Mod Organizer.\r\n*Alpha.esp\r\n*Gamma.esp\r\n*DLCX.esm\r\n*Shared.esp\r\n",
    );
    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();
    let id = run.instance_id.clone().expect("an instance was made");
    assert_eq!(plugin_state(&f, &id, "Shared.esp"), (true, false));

    let outcome = game_deploy::deploy(&f.ctx, &id, &f.def, DeployMode::Links).unwrap();
    assert!(!activated_by_deploy(&outcome)
        .iter()
        .any(|n| n == "Shared.esp"));
    assert_eq!(plugin_state(&f, &id, "Shared.esp"), (true, false));

    prepare_launch(&f, &id);
    assert_eq!(plugin_state(&f, &id, "Shared.esp"), (true, false));
}

#[test]
fn a_saves_folder_that_cannot_be_read_stops_the_import_instead_of_being_skipped() {
    let f = fixture();
    // `saves` is a file, so listing it fails with something other than "not there".
    let saves = f.setup.join("profiles/Test Profile/saves");
    std::fs::remove_dir_all(&saves).unwrap();
    write(&saves, b"not a folder");

    let planned = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, true));
    let Err(ImportError::Unreadable { path, reason }) = planned else {
        panic!("expected the plan to refuse an unreadable saves folder, got {planned:?}");
    };
    assert_eq!(path, saves);
    assert!(reason.contains("saves"), "{reason}");

    let run = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, true),
        RunOptions::default(),
        &|_| {},
    );
    assert!(
        matches!(run, Err(ImportError::Unreadable { .. })),
        "{run:?}"
    );
    assert!(
        game_instance::list(&f.ctx).unwrap().is_empty(),
        "nothing was made"
    );
}

#[test]
fn a_profile_with_no_saves_folder_has_no_saves_and_still_imports() {
    let f = fixture();
    std::fs::remove_dir_all(f.setup.join("profiles/Test Profile/saves")).unwrap();
    let plan = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, true)).unwrap();
    assert_eq!(plan.saves.files, 0);
}

/// An instance the import made, for the cleanup tests.
fn imported_instance(f: &Fixture) -> String {
    game_import::run(
        &f.ctx,
        &f.inventory,
        &request(f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap()
    .instance_id
    .unwrap()
}

#[test]
fn a_half_made_instance_is_removed_and_the_original_error_is_what_is_reported() {
    let f = fixture();
    let id = imported_instance(&f);
    let cause = ImportError::Other("the plugin list could not be written".into());

    let reported = game_import::abandon_half_made_instance(&f.ctx, &id, cause);

    assert!(
        matches!(&reported, ImportError::Other(m) if m == "the plugin list could not be written"),
        "{reported:?}"
    );
    assert!(game_instance::get(&f.ctx, &id).unwrap().is_none());
}

#[test]
fn a_half_made_instance_that_cannot_be_removed_is_reported_and_labelled_incomplete() {
    let f = fixture();
    let id = imported_instance(&f);
    let name_before = game_instance::get(&f.ctx, &id).unwrap().unwrap().name;
    // A trigger that refuses the row's deletion makes the removal fail with the row still listed.
    let conn = rusqlite::Connection::open(f.ctx.paths.local_state_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER keep_rows BEFORE DELETE ON game_instances \
         BEGIN SELECT RAISE(ABORT, 'row is pinned by the test'); END;",
    )
    .unwrap();
    drop(conn);

    let reported = game_import::abandon_half_made_instance(
        &f.ctx,
        &id,
        ImportError::Other("the plugin list could not be written".into()),
    );

    let ImportError::CleanupFailed {
        instance_id,
        listed,
        labelled,
        ..
    } = &reported
    else {
        panic!("a failed cleanup is reported, got {reported:?}");
    };
    assert_eq!(instance_id, &id);
    assert!(*listed);
    assert_eq!(labelled, &Ok(()));
    let message = reported.to_string();
    assert!(
        message.contains("the plugin list could not be written"),
        "{message}"
    );
    assert!(
        message.contains(&id) && message.contains("(incomplete import)"),
        "{message}"
    );

    // It stays, and says so in the list and in its manifest.
    let record = game_instance::get(&f.ctx, &id).unwrap().unwrap();
    assert_eq!(record.name, format!("{name_before} (incomplete import)"));
    assert_eq!(
        game_instance::get_manifest(&f.ctx, &id).unwrap().name,
        record.name
    );
}

#[test]
fn mo2_executables_become_the_instances_tools_except_the_games_own_and_mo2s() {
    let f = fixture();
    let root = f._tmp.path();
    let setup = f.setup.clone();
    let slash = |path: &Path| path.to_string_lossy().replace('\\', "/");
    write(&setup.join("mods/Alpha/tools/AlphaTool.exe"), b"MZ");
    write(&setup.join("mods/Beta/BetaTool.exe"), b"MZ");
    write(&setup.join("explorer++/Explorer++.exe"), b"MZ");
    write(&root.join("tools/SSEEdit/SSEEdit.exe"), b"MZ");
    let ini_path = setup.join("ModOrganizer.ini");
    let mut ini = std::fs::read_to_string(&ini_path).unwrap();
    ini.push_str(&format!(
        "[customExecutables]\r\nsize=7\r\n\
1\\title=Alpha Tool\r\n1\\binary={alpha}\r\n1\\arguments=-x \\\"a b\\\"\r\n\
2\\title=Beta Tool\r\n2\\binary={beta}\r\n\
3\\title=SKSE\r\n3\\binary={skse}\r\n\
4\\title=Explore VFS\r\n4\\binary={explorer}\r\n\
5\\title=SSEEdit\r\n5\\binary={sseedit}\r\n\
6\\title=Hidden\r\n6\\binary={sseedit}\r\n6\\hide=true\r\n\
7\\title=Escape\r\n7\\binary={escape}\r\n",
        alpha = slash(&setup.join("mods/Alpha/tools/AlphaTool.exe")),
        beta = slash(&setup.join("mods/Beta/BetaTool.exe")),
        skse = slash(&root.join("game/skse64_loader.exe")),
        explorer = slash(&setup.join("explorer++/Explorer++.exe")),
        sseedit = slash(&root.join("tools/SSEEdit/SSEEdit.exe")),
        escape = slash(&setup.join("mods/../outside/evil.exe")),
    ));
    std::fs::write(&ini_path, ini).unwrap();

    let report = game_import::run(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        RunOptions::default(),
        &|_| {},
    )
    .unwrap();

    assert_eq!(report.tools_added, vec!["Alpha Tool", "SSEEdit"]);
    let id = report.instance_id.clone().unwrap();
    let tools = agora_core::game_user_tools::list(&f.ctx, &id).unwrap();
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert_eq!(tools[0].exe, "Data/tools/AlphaTool.exe");
    assert_eq!(tools[0].args, vec!["-x".to_string(), "a b".to_string()]);
    assert_eq!(tools[1].exe, slash(&root.join("tools/SSEEdit/SSEEdit.exe")));
    let notes = report.next_steps.join("\n");
    assert!(notes.contains("Beta Tool"), "{notes}");
    assert!(notes.contains("Escape"), "{notes}");
    assert!(!notes.contains("SKSE"), "{notes}");
    assert!(!notes.contains("Explore VFS"), "{notes}");
    assert!(!notes.contains("Hidden"), "{notes}");
}

/// A second MO2 profile, "Alt", enables Alpha and Beta and leaves Gamma off.
fn add_second_mo2_profile(f: &Fixture) {
    let profile = f.setup.join("profiles/Alt");
    write(
        &profile.join("modlist.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\n+Alpha\r\n+Beta\r\n-Gamma\r\n",
    );
    write(
        &profile.join("plugins.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\n*Alpha.esp\r\n*Beta.esp\r\n",
    );
    write(
        &profile.join("loadorder.txt"),
        b"# This file was automatically generated by Mod Organizer.\r\nSkyrim.esm\r\nUpdate.esm\r\nAlpha.esp\r\nBeta.esp\r\nGamma.esp\r\n",
    );
}

#[test]
fn two_profiles_import_merged_with_a_loadout_for_each() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    add_second_mo2_profile(&f);
    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("MO2 Merge"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("the merged MO2 import runs");
    assert_eq!(report.instances.len(), 1, "{report:?}");
    assert_eq!(report.loadouts, profiles);
    let instance = &report.instances[0];
    let manifest = agora_core::game_instance::get_manifest(&f.ctx, instance).unwrap();
    let names: Vec<String> = manifest
        .layers
        .layers()
        .iter()
        .filter_map(|layer| match &layer.source {
            agora_game_api::LayerSource::Content { content } => Some(
                agora_core::content_store::get_item(&f.ctx, content)
                    .unwrap()
                    .name,
            ),
            _ => None,
        })
        .collect();
    for mod_name in ["Alpha", "Beta", "Gamma"] {
        assert!(
            names.iter().any(|name| name == mod_name),
            "{mod_name} is in the union: {names:?}"
        );
    }
    let dir = f.ctx.paths.instance_dir(instance).unwrap();
    let loadouts = agora_core::loadout::list_profiles(&dir).unwrap();
    let enabled = |profile_name: &str| -> std::collections::BTreeMap<String, bool> {
        let profile = loadouts
            .iter()
            .find(|profile| profile.name == profile_name)
            .unwrap_or_else(|| panic!("no loadout '{profile_name}'"));
        profile
            .game
            .as_ref()
            .unwrap()
            .content
            .iter()
            .map(|item| {
                (
                    agora_core::content_store::get_item(
                        &f.ctx,
                        item.item.strip_prefix("content:").unwrap_or(&item.item),
                    )
                    .unwrap()
                    .name,
                    item.enabled,
                )
            })
            .collect()
    };
    let main = enabled("Test Profile");
    assert!(main["Alpha"] && main["Gamma"] && !main["Beta"], "{main:?}");
    let alt = enabled("Alt");
    assert!(alt["Alpha"] && alt["Beta"] && !alt["Gamma"], "{alt:?}");
}

fn planned_file_bytes(f: &Fixture, instance: &str, game_path: &str) -> Vec<u8> {
    let plan = game_deploy::plan(&f.ctx, instance, &f.def, DeployMode::Links).unwrap();
    let entry = plan
        .files
        .iter()
        .find(|p| p.path.as_str() == game_path)
        .unwrap_or_else(|| panic!("{game_path} is not in the deploy plan"));
    match &entry.source {
        FileSource::Content { sha256, .. } => {
            content_store::read_object_bytes(&f.ctx, sha256).unwrap()
        }
        FileSource::Base { path, .. }
        | FileSource::Generated { path, .. }
        | FileSource::Writable { path }
        | FileSource::Override { path, .. } => std::fs::read(path).unwrap(),
    }
}

#[test]
fn f1h_merged_profiles_preserve_opposite_mod_priorities() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    // "Reverse" enables the exact same two mods as "Test Profile" (Alpha and Gamma),
    // but puts Gamma ABOVE Alpha so Gamma's shared.txt must win.
    let reverse = f.setup.join("profiles/Reverse");
    write(
        &reverse.join("modlist.txt"),
        b"# Mod Organizer\r\n+Gamma\r\n-Beta\r\n+Alpha\r\n",
    );
    write(
        &reverse.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*Gamma.esp\r\n",
    );
    write(
        &reverse.join("loadorder.txt"),
        b"# Mod Organizer\r\nSkyrim.esm\r\nUpdate.esm\r\nAlpha.esp\r\nGamma.esp\r\n",
    );

    let profiles = vec!["Test Profile".to_string(), "Reverse".to_string()];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Opposite Order"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import runs");
    let instance = &report.instances[0];

    assert_eq!(
        planned_file_bytes(&f, instance, "Data/shared.txt"),
        b"from alpha",
        "Test Profile puts Alpha above Gamma"
    );

    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Reverse")
        .expect("switching to Reverse succeeds");
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/shared.txt"),
        b"from gamma",
        "Reverse puts Gamma above Alpha"
    );

    agora_core::game_loadout::apply_game_profile_with_undo(
        &f.ctx,
        instance,
        &f.def,
        "Test Profile",
    )
    .expect("switching back to Test Profile succeeds");
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/shared.txt"),
        b"from alpha",
        "switching back restores Alpha winning over Gamma"
    );
}

#[test]
fn f1a_merged_profile_without_own_settings_uses_shared_not_profile_1() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    // "Shared" has no LocalSettings and no plugins.txt of its own.
    let shared_prof = f.setup.join("profiles/Shared");
    write(
        &shared_prof.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+Beta\r\n-Gamma\r\n",
    );
    write(
        &shared_prof.join("settings.ini"),
        b"[General]\r\nLocalSaves=false\r\nLocalSettings=false\r\n",
    );

    // "OwnPlugins" has shared INIs (LocalSettings=false) but its own plugins.txt (*Beta.esp only).
    let own_plugins = f.setup.join("profiles/OwnPlugins");
    write(
        &own_plugins.join("modlist.txt"),
        b"# Mod Organizer\r\n+Beta\r\n",
    );
    write(
        &own_plugins.join("settings.ini"),
        b"[General]\r\nLocalSaves=false\r\nLocalSettings=false\r\n",
    );
    write(
        &own_plugins.join("plugins.txt"),
        b"# Mod Organizer\r\n*Beta.esp\r\n",
    );
    write(
        &own_plugins.join("loadorder.txt"),
        b"# Mod Organizer\r\nSkyrim.esm\r\nUpdate.esm\r\nBeta.esp\r\n",
    );

    let profiles = vec![
        "Test Profile".to_string(),
        "Shared".to_string(),
        "OwnPlugins".to_string(),
    ];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Shared Settings Merge"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import runs");
    let instance = &report.instances[0];
    let dir = f.ctx.paths.instance_dir(instance).unwrap();

    let loadouts = agora_core::loadout::list_profiles(&dir).unwrap();
    let get_loadout = |name: &str| {
        loadouts
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("missing loadout '{name}'"))
            .game
            .clone()
            .expect("game loadout")
    };

    // Shared profile: every INI and plugin list must be explicitly absent (None).
    let shared_state = get_loadout("Shared");
    assert!(
        !shared_state.inis.is_empty() && shared_state.inis.iter().all(|i| i.contents.is_none()),
        "Shared must not inherit Profile 1 INIs: {:?}",
        shared_state.inis
    );
    assert!(
        !shared_state.plugin_lists.is_empty()
            && shared_state
                .plugin_lists
                .iter()
                .all(|l| l.contents.is_none()),
        "Shared has no plugins.txt of its own, so its plugin list must be None: {:?}",
        shared_state.plugin_lists
    );

    // Switching to Shared removes Profile 1's private INI copies and plugin list.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Shared")
        .expect("apply Shared");
    assert!(
        !dir.join("user/Skyrim.ini").exists(),
        "Profile 1 Skyrim.ini must not remain under Shared"
    );
    assert!(
        !dir.join("user/SkyrimCustom.ini").exists(),
        "Profile 1 SkyrimCustom.ini must not remain under Shared"
    );
    assert!(
        !dir.join("user/Plugins.txt").exists(),
        "Profile 1 Plugins.txt must not remain under Shared"
    );

    // Switching to OwnPlugins keeps INIs shared, while writing OwnPlugins' own Plugins.txt.
    agora_core::game_loadout::apply_game_profile_with_undo(
        &f.ctx,
        instance,
        &f.def,
        "OwnPlugins",
    )
    .expect("apply OwnPlugins");
    assert!(!dir.join("user/Skyrim.ini").exists());
    assert!(!dir.join("user/SkyrimCustom.ini").exists());
    let plugins_text = std::fs::read_to_string(dir.join("user/Plugins.txt"))
        .expect("OwnPlugins writes its own Plugins.txt");
    assert!(
        plugins_text.contains("*Beta.esp") && !plugins_text.contains("*Alpha.esp"),
        "OwnPlugins Plugins.txt should have only *Beta.esp, got:\n{plugins_text}"
    );
}

#[test]
fn f1b_merged_local_saves_get_distinct_scoped_folders_and_copy_each_profiles_saves() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    add_second_mo2_profile(&f);
    let alt = f.setup.join("profiles/Alt");
    write(
        &alt.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=false\r\n",
    );
    write(&alt.join("saves/AltSave.ess"), b"ALT SAVE");

    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Scoped Saves Merge"), true),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import with copy_saves runs");
    let instance = &report.instances[0];
    let rule = game_saves::rule_for(&f.def, &StoreId::steam()).expect("save rule");

    let unscoped = game_saves::own_folder(rule, instance).unwrap();
    let main_folder = game_saves::own_folder_scoped(rule, instance, Some("Test Profile")).unwrap();
    let alt_folder = game_saves::own_folder_scoped(rule, instance, Some("Alt")).unwrap();
    assert_ne!(main_folder, alt_folder);
    assert!(
        !unscoped.join("Save1.ess").exists(),
        "profile 1 saves must go to its scoped folder, not the unscoped own folder"
    );
    assert_eq!(
        std::fs::read(main_folder.join("Save1.ess")).expect("Test Profile save copied"),
        b"SAVE ONE"
    );
    assert!(!main_folder.join("AltSave.ess").exists());
    assert_eq!(
        std::fs::read(alt_folder.join("AltSave.ess")).expect("Alt save copied"),
        b"ALT SAVE"
    );
    assert!(!alt_folder.join("Save1.ess").exists());

    // Active profile (Test Profile) points at Test Profile's scoped folder.
    assert_eq!(
        game_saves::in_use_folder(&f.ctx, instance, &f.def).unwrap(),
        main_folder
    );
    let main_status = game_saves::status(&f.ctx, instance, &f.def).unwrap();
    assert_eq!(
        main_status.setting_value.as_deref(),
        Some(game_saves::own_value_scoped(rule, instance, Some("Test Profile")).as_str())
    );

    // Switching to Alt points at Alt's scoped folder.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Alt")
        .expect("apply Alt");
    assert_eq!(
        game_saves::in_use_folder(&f.ctx, instance, &f.def).unwrap(),
        alt_folder
    );
    let alt_status = game_saves::status(&f.ctx, instance, &f.def).unwrap();
    assert_eq!(
        alt_status.setting_value.as_deref(),
        Some(game_saves::own_value_scoped(rule, instance, Some("Alt")).as_str())
    );
}

#[test]
fn f1c_overwrite_layer_stays_enabled_and_wins_in_every_merged_loadout() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    add_second_mo2_profile(&f);
    write(&f.setup.join("mods/Alpha/override_check.txt"), b"from alpha");
    write(&f.setup.join("mods/Beta/override_check.txt"), b"from beta");
    write(&f.setup.join("overwrite/override_check.txt"), b"from overwrite");

    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Overwrite Merge"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import runs");
    let instance = &report.instances[0];

    // Profile 1 (applied initially): unclaimed overwrite mesh is deployed, and overwrite wins conflicts.
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/meshes/armor/helm.nif"),
        b"NIF",
        "MO2 overwrite mesh must stay enabled under Test Profile"
    );
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/override_check.txt"),
        b"from overwrite",
        "MO2 overwrite must outrank mods under Test Profile"
    );

    // Switching to Profile 2: overwrite still stays enabled and still wins conflicts.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Alt")
        .expect("apply Alt");
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/meshes/armor/helm.nif"),
        b"NIF",
        "MO2 overwrite mesh must stay enabled under Alt"
    );
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/override_check.txt"),
        b"from overwrite",
        "MO2 overwrite must outrank mods under Alt"
    );
}

#[test]
fn f1d_byte_identical_alias_folders_stay_enabled_in_each_profiles_loadout() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    write(
        &f.setup.join("mods/DeltaAlias/AliasShared.esp"),
        b"IDENTICAL BYTES",
    );
    write(
        &f.setup.join("mods/EchoAlias/AliasShared.esp"),
        b"IDENTICAL BYTES",
    );
    // Profile 1 enables DeltaAlias (and disables EchoAlias).
    write(
        &f.setup.join("profiles/Test Profile/modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+DeltaAlias\r\n-EchoAlias\r\n-Beta\r\n+Gamma\r\n",
    );
    // Profile 2 enables EchoAlias (and disables DeltaAlias).
    let alt = f.setup.join("profiles/Alt");
    write(
        &alt.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n-DeltaAlias\r\n+EchoAlias\r\n+Beta\r\n-Gamma\r\n",
    );
    write(
        &alt.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*Beta.esp\r\n*AliasShared.esp\r\n",
    );

    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];
    let report = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Alias Merge"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import runs");
    let instance = &report.instances[0];

    // Under Test Profile (DeltaAlias enabled): AliasShared.esp must be deployed.
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/AliasShared.esp"),
        b"IDENTICAL BYTES",
        "DeltaAlias must keep the shared content item enabled under Test Profile"
    );

    // Under Alt (EchoAlias enabled): AliasShared.esp must STILL be deployed.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Alt")
        .expect("apply Alt");
    assert_eq!(
        planned_file_bytes(&f, instance, "Data/AliasShared.esp"),
        b"IDENTICAL BYTES",
        "EchoAlias must keep the shared content item enabled under Alt"
    );
}

#[test]
fn f1e_late_merge_failure_cleans_up_and_retry_succeeds() {
    use agora_core::game_import::{profiles::Grouping, ImportPhase};

    let f = fixture();
    // Create a mod folder enabled only in Alt, plus Alt profile.
    let only_in_alt = f.setup.join("mods/OnlyInAlt");
    write(&only_in_alt.join("OnlyInAlt.esp"), b"ALT ONLY");
    let alt = f.setup.join("profiles/Alt");
    write(
        &alt.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+OnlyInAlt\r\n",
    );
    write(
        &alt.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*OnlyInAlt.esp\r\n",
    );

    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];
    // Once profile 1 reaches Finishing (after planning succeeded and the instance was created),
    // remove OnlyInAlt so adding profile 2's mod fails after profile 1 has committed.
    let removed = std::cell::Cell::new(false);
    let err = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|p| {
            if p.phase == ImportPhase::Finishing && !removed.replace(true) {
                std::fs::remove_dir_all(&only_in_alt).unwrap();
            }
        },
    )
    .expect_err("missing second-profile mod must fail the merge");
    assert!(
        matches!(err, ImportError::Store { .. }),
        "expected store failure, got {err:?}"
    );
    assert!(
        game_instance::list(&f.ctx).unwrap().is_empty(),
        "a failed merge must not leave a partial instance listed"
    );

    // Restore the missing folder and retry with the default name (name: None):
    // must succeed rather than returning AlreadyImported.
    write(&only_in_alt.join("OnlyInAlt.esp"), b"ALT ONLY");
    let retry = agora_core::game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, None, false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("retry after fixing the source folder must succeed, not return AlreadyImported");
    assert_eq!(retry.instances.len(), 1);
    assert_eq!(retry.loadouts, profiles);
}

struct VolumeSeamGuard;

impl VolumeSeamGuard {
    fn set(free_bytes: u64) -> Self {
        agora_core::game_tool_swap::set_volume_seam(Some(agora_core::game_tool_swap::VolumeSeam {
            same_volume: None,
            free_bytes: Some(free_bytes),
        }));
        Self
    }
}

impl Drop for VolumeSeamGuard {
    fn drop(&mut self) {
        agora_core::game_tool_swap::set_volume_seam(None);
    }
}

#[test]
fn f1f_space_preflight_matches_exact_copies_in_merged_and_separate_modes() {
    use agora_core::game_import::profiles::Grouping;

    let f = fixture();
    // Add a large mod that is disabled in BOTH profiles.
    write(&f.setup.join("mods/HugeDisabled/big.bin"), &vec![b'X'; 8192]);
    write(
        &f.setup.join("profiles/Test Profile/modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n-Beta\r\n+Gamma\r\n-HugeDisabled\r\n",
    );
    let alt = f.setup.join("profiles/Alt");
    write(
        &alt.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+Beta\r\n-Gamma\r\n-HugeDisabled\r\n",
    );
    write(
        &alt.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*Beta.esp\r\n",
    );
    write(
        &alt.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=false\r\n",
    );
    write(&alt.join("saves/AltSave.ess"), &vec![b'S'; 512]);

    let profiles = vec!["Test Profile".to_string(), "Alt".to_string()];

    let p_main = game_import::plan(&f.ctx, &f.inventory, &request(&f, None, false)).unwrap();
    let p_alt = game_import::plan(
        &f.ctx,
        &f.inventory,
        &Mo2Request {
            profile: "Alt".into(),
            ..request(&f, None, false)
        },
    )
    .unwrap();

    let mod_bytes = |name: &str| {
        p_main
            .mods
            .iter()
            .find(|m| m.folder == name)
            .unwrap()
            .bytes
    };
    let enabled_union = mod_bytes("Alpha") + mod_bytes("Beta") + mod_bytes("Gamma");
    let all_union = enabled_union + mod_bytes("HugeDisabled");
    let ow_total = p_main.overwrite.bytes;
    let ow_rest = p_main.overwrite.rest_bytes;
    let ow_gen = ow_total - ow_rest;
    let total_saves = p_main.saves.bytes + p_alt.saves.bytes;
    assert!(ow_rest > 0 && ow_gen > 0 && total_saves > 0);

    // 1. Merged without copy_saves: enabled union + overwrite once (no HugeDisabled, no saves).
    let merged_no_saves = game_import::mo2_plan_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("M1"), false),
        &profiles,
        Grouping::Merged,
    )
    .unwrap();
    assert_eq!(
        merged_no_saves.bytes,
        enabled_union + ow_total,
        "merged (no saves) must count enabled union + overwrite once, not HugeDisabled"
    );

    // 2. Merged with copy_saves: adds both profiles' local saves.
    let merged_with_saves = game_import::mo2_plan_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("M2"), true),
        &profiles,
        Grouping::Merged,
    )
    .unwrap();
    assert_eq!(
        merged_with_saves.bytes,
        enabled_union + ow_total + total_saves,
        "merged (copy_saves) must include both profiles' local saves"
    );

    // 3. Separate without copy_saves: union of all listed mods (incl. HugeDisabled, no double
    //    count of Alpha/Beta/Gamma) + overwrite rest once + generated per instance.
    let sep_no_saves = game_import::mo2_plan_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("S1"), false),
        &profiles,
        Grouping::Separate,
    )
    .unwrap();
    assert_eq!(
        sep_no_saves.bytes,
        all_union + ow_rest + ow_gen * 2,
        "separate (no saves) must count all listed mods once + overwrite rest once + generated per instance"
    );

    // 4. Separate with copy_saves: adds both profiles' local saves.
    let sep_with_saves = game_import::mo2_plan_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("S2"), true),
        &profiles,
        Grouping::Separate,
    )
    .unwrap();
    assert_eq!(
        sep_with_saves.bytes,
        all_union + ow_rest + ow_gen * 2 + total_saves,
        "separate (copy_saves) must include both profiles' local saves"
    );

    // Executing Merged must NOT copy HugeDisabled into the content store.
    game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("M-Run"), false),
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import runs");
    let stored_names: Vec<String> = stored_items(&f.ctx).into_iter().map(|i| i.name).collect();
    assert!(
        !stored_names.iter().any(|n| n == "HugeDisabled"),
        "merged execution must not store mods disabled in every chosen profile: {stored_names:?}"
    );
}

#[test]
fn f1f_single_profile_run_checks_free_space_before_copying() {
    let f = fixture();
    let req = request(&f, Some("Tight Disk"), true);
    let planned = game_import::plan(&f.ctx, &f.inventory, &req).unwrap();
    let needed = planned.bytes + planned.saves.bytes;
    assert!(needed > 1);

    {
        let _guard = VolumeSeamGuard::set(needed - 1);
        let err = game_import::run(&f.ctx, &f.inventory, &req, RunOptions::default(), &|_| {})
            .expect_err("single-profile run must refuse when disk space is short by 1 byte");
        assert!(
            matches!(err, ImportError::NotEnoughSpace { needed: n, free } if n == needed && free == needed - 1),
            "expected NotEnoughSpace {{ needed: {needed}, free: {} }}, got {err:?}",
            needed - 1
        );
    }
    assert!(
        stored_items(&f.ctx).is_empty(),
        "nothing may be stored when preflight refuses"
    );
    assert!(
        game_instance::list(&f.ctx).unwrap().is_empty(),
        "no instance may be created when preflight refuses"
    );

    {
        let _guard = VolumeSeamGuard::set(needed);
        game_import::run(&f.ctx, &f.inventory, &req, RunOptions::default(), &|_| {})
            .expect("single-profile run succeeds when exact bytes fit");
    }
}

#[test]
fn f1g_separate_mode_returns_per_profile_report_and_lists_copied_saves() {
    use agora_core::game_import::{profiles::Grouping, ImportPhase};

    let f = fixture();
    // Second profile enables a mod we delete once profile 1 reaches Finishing, so planning
    // succeeds for all three profiles, profile 1 creates its instance, profile 2 fails, and
    // profile 3 still completes.
    let doomed_mod = f.setup.join("mods/DoomedMod");
    write(&doomed_mod.join("Doomed.esp"), b"DOOMED");
    let fails_prof = f.setup.join("profiles/FailsMidRun");
    write(
        &fails_prof.join("modlist.txt"),
        b"# Mod Organizer\r\n+DoomedMod\r\n",
    );
    write(
        &fails_prof.join("plugins.txt"),
        b"# Mod Organizer\r\n*Doomed.esp\r\n",
    );

    let third = f.setup.join("profiles/Third");
    write(
        &third.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+Beta\r\n",
    );
    write(
        &third.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*Beta.esp\r\n",
    );
    write(
        &third.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=false\r\n",
    );
    write(&third.join("saves/ThirdSave.ess"), b"THIRD SAVE");

    let profiles = vec![
        "Test Profile".to_string(),
        "FailsMidRun".to_string(),
        "Third".to_string(),
    ];
    let removed = std::cell::Cell::new(false);
    let report = game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &request(&f, Some("Sep Batch"), true),
        &profiles,
        Grouping::Separate,
        Default::default(),
        &|p| {
            if p.phase == ImportPhase::Finishing && !removed.replace(true) {
                std::fs::remove_file(fails_prof.join("modlist.txt")).unwrap();
            }
        },
    )
    .expect("separate mode must return a per-profile report instead of failing the whole call");

    assert_eq!(
        report.instances.len(),
        2,
        "profiles 1 and 3 created instances: {report:?}"
    );
    assert_eq!(report.profiles.len(), 3, "{:?}", report.profiles);

    // Profile 1 succeeded and lists both copied save files written outside the instance.
    let p0 = &report.profiles[0];
    assert_eq!(p0.profile, "Test Profile");
    assert!(p0.instance_id.is_some());
    assert!(p0.error.is_none());
    assert_eq!(p0.external_files_written.len(), 2, "{p0:?}");
    assert!(
        p0.external_files_written.iter().all(|p| p.is_file()),
        "all reported copied save files must exist on disk: {:?}",
        p0.external_files_written
    );
    assert!(p0
        .external_files_written
        .iter()
        .any(|p| p.file_name().and_then(|n| n.to_str()) == Some("Save1.ess")));

    // Profile 2 failed and reports its error without hiding profiles 1 and 3.
    let p1 = &report.profiles[1];
    assert_eq!(p1.profile, "FailsMidRun");
    assert!(p1.instance_id.is_none());
    assert!(
        p1.error
            .as_deref()
            .is_some_and(|e| e.contains("modlist.txt")),
        "profile 2 must carry its error: {p1:?}"
    );

    // Profile 3 succeeded and lists its copied save file.
    let p2 = &report.profiles[2];
    assert_eq!(p2.profile, "Third");
    assert!(p2.instance_id.is_some());
    assert!(p2.error.is_none());
    assert_eq!(p2.external_files_written.len(), 1, "{p2:?}");
    assert!(p2.external_files_written[0].is_file());
    assert_eq!(
        p2.external_files_written[0]
            .file_name()
            .and_then(|n| n.to_str()),
        Some("ThirdSave.ess")
    );
}

#[test]
fn acceptance_merged_mo2_profiles_reproduce_winners_saves_and_effective_inis() {
    use agora_core::game_import::profiles::Grouping;
    use agora_core::game_ini::{self, IniSource};

    let f = fixture();
    // Seed the game's shared INI files in the fake Documents folder.
    let shared_docs = f
        ._user_data
        .path()
        .join("documents/My Games/Skyrim Special Edition");
    write(
        &shared_docs.join("Skyrim.ini"),
        b"[General]\r\nsProfileTag=shared_base\r\n",
    );
    write(
        &shared_docs.join("SkyrimCustom.ini"),
        b"[Custom]\r\nsCustomTag=shared_custom\r\n",
    );

    // Profile 1 ("Mage"): Alpha above Gamma, private INIs, local save MageHero.ess.
    let mage = f.setup.join("profiles/Mage");
    write(
        &mage.join("modlist.txt"),
        b"# Mod Organizer\r\n+Alpha\r\n+Gamma\r\n",
    );
    write(
        &mage.join("plugins.txt"),
        b"# Mod Organizer\r\n*Alpha.esp\r\n*Gamma.esp\r\n",
    );
    write(
        &mage.join("loadorder.txt"),
        b"# Mod Organizer\r\nSkyrim.esm\r\nUpdate.esm\r\nAlpha.esp\r\nGamma.esp\r\n",
    );
    write(
        &mage.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=true\r\n",
    );
    write(
        &mage.join("Skyrim.ini"),
        b"[General]\r\nsProfileTag=mage_private\r\n",
    );
    write(
        &mage.join("SkyrimCustom.ini"),
        b"[Custom]\r\nsCustomTag=mage_custom\r\n",
    );
    write(&mage.join("saves/MageHero.ess"), b"MAGE SAVE");

    // Profile 2 ("Warrior"): exact same enabled mods in OPPOSITE order (Gamma above Alpha),
    // shared INIs (LocalSettings=false), local save WarriorHero.ess.
    let warrior = f.setup.join("profiles/Warrior");
    write(
        &warrior.join("modlist.txt"),
        b"# Mod Organizer\r\n+Gamma\r\n+Alpha\r\n",
    );
    write(
        &warrior.join("plugins.txt"),
        b"# Mod Organizer\r\n*Gamma.esp\r\n*Alpha.esp\r\n",
    );
    write(
        &warrior.join("loadorder.txt"),
        b"# Mod Organizer\r\nSkyrim.esm\r\nUpdate.esm\r\nGamma.esp\r\nAlpha.esp\r\n",
    );
    write(
        &warrior.join("settings.ini"),
        b"[General]\r\nLocalSaves=true\r\nLocalSettings=false\r\n",
    );
    write(&warrior.join("saves/WarriorHero.ess"), b"WARRIOR SAVE");

    let profiles = vec!["Mage".to_string(), "Warrior".to_string()];
    let report = game_import::mo2_run_profiles(
        &f.ctx,
        &f.inventory,
        &Mo2Request {
            profile: "Mage".into(),
            ..request(&f, Some("Two Characters"), true)
        },
        &profiles,
        Grouping::Merged,
        Default::default(),
        &|_| {},
    )
    .expect("merged import succeeds");
    assert_eq!(report.instances.len(), 1);
    assert_eq!(report.loadouts, profiles);
    let instance = &report.instances[0];
    let rule = game_saves::rule_for(&f.def, &StoreId::steam()).expect("save rule");

    let mage_saves = game_saves::own_folder_scoped(rule, instance, Some("Mage")).unwrap();
    let warrior_saves = game_saves::own_folder_scoped(rule, instance, Some("Warrior")).unwrap();
    assert_ne!(mage_saves, warrior_saves);

    let check_mage = || {
        assert_eq!(
            planned_file_bytes(&f, instance, "Data/shared.txt"),
            b"from alpha",
            "Mage puts Alpha above Gamma"
        );
        assert_eq!(
            planned_file_bytes(&f, instance, "Data/meshes/armor/helm.nif"),
            b"NIF",
            "overwrite mesh stays active under Mage"
        );
        assert_eq!(
            game_saves::in_use_folder(&f.ctx, instance, &f.def).unwrap(),
            mage_saves
        );
        assert_eq!(
            std::fs::read(mage_saves.join("MageHero.ess")).unwrap(),
            b"MAGE SAVE"
        );
        assert!(!mage_saves.join("WarriorHero.ess").exists());

        let skyrim_ini = game_ini::read(&f.ctx, instance, &f.def, "user/Skyrim.ini").unwrap();
        assert_eq!(
            skyrim_ini.document.get("General", "sProfileTag").as_deref(),
            Some("mage_private")
        );
        assert_eq!(
            skyrim_ini
                .document
                .get("General", "SLocalSavePath")
                .as_deref(),
            Some(game_saves::own_value_scoped(rule, instance, Some("Mage")).as_str())
        );

        let custom_ini = game_ini::read(&f.ctx, instance, &f.def, "user/SkyrimCustom.ini").unwrap();
        assert_eq!(custom_ini.source, IniSource::Copy);
        assert_eq!(
            custom_ini.document.get("Custom", "sCustomTag").as_deref(),
            Some("mage_custom")
        );
    };

    let check_warrior = || {
        assert_eq!(
            planned_file_bytes(&f, instance, "Data/shared.txt"),
            b"from gamma",
            "Warrior puts Gamma above Alpha"
        );
        assert_eq!(
            planned_file_bytes(&f, instance, "Data/meshes/armor/helm.nif"),
            b"NIF",
            "overwrite mesh stays active under Warrior"
        );
        assert_eq!(
            game_saves::in_use_folder(&f.ctx, instance, &f.def).unwrap(),
            warrior_saves
        );
        assert_eq!(
            std::fs::read(warrior_saves.join("WarriorHero.ess")).unwrap(),
            b"WARRIOR SAVE"
        );
        assert!(!warrior_saves.join("MageHero.ess").exists());

        // Warrior has shared settings (seeded from shared Skyrim.ini for SLocalSavePath), never Mage's tag.
        let skyrim_ini = game_ini::read(&f.ctx, instance, &f.def, "user/Skyrim.ini").unwrap();
        assert_eq!(
            skyrim_ini.document.get("General", "sProfileTag").as_deref(),
            Some("shared_base"),
            "Warrior must inherit shared Skyrim.ini, never Mage's private Skyrim.ini"
        );
        assert_eq!(
            skyrim_ini
                .document
                .get("General", "SLocalSavePath")
                .as_deref(),
            Some(game_saves::own_value_scoped(rule, instance, Some("Warrior")).as_str())
        );

        let custom_ini = game_ini::read(&f.ctx, instance, &f.def, "user/SkyrimCustom.ini").unwrap();
        assert_eq!(custom_ini.source, IniSource::GameFile);
        assert_eq!(
            custom_ini.document.get("Custom", "sCustomTag").as_deref(),
            Some("shared_custom"),
            "Warrior must see the shared SkyrimCustom.ini, never Mage's private copy"
        );
    };

    // Initial state is Mage (profile 1).
    check_mage();

    // Switch to Warrior.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Warrior")
        .expect("switch to Warrior");
    check_warrior();

    // Switch back to Mage.
    agora_core::game_loadout::apply_game_profile_with_undo(&f.ctx, instance, &f.def, "Mage")
        .expect("switch back to Mage");
    check_mage();
}
