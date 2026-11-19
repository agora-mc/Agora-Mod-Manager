//! "Neon Courier", an invented game shaped to be as unlike Skyrim as a test can make it: no
//! plugin list and no load order, no per-user INI files Agora edits, two mod roots
//! (`archive/mods/` and `bin/plugins/`), saves that are folders under a documents path, and a
//! different executable. It is registered only inside tests, never shipped or cataloged.
//!
//! Tests use it to prove which generic paths still assume Skyrim. The base definition is the true
//! shape; [`with_save_rule`] adds an INI-backed save rule only so a test can reach the save survey
//! and show what its `.ess` filter does to folder saves.

use std::sync::Arc;

use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_game_api::{
    ContentLayout, ContentMatch, ContentRule, DeploymentStrategy, GameDefinition, GameId,
    GamePackage, GamePath, LaunchRecipe, PackageDefinition, RelPath, SaveFolderRule,
    SaveFolderSpec, SaveLocationRule, SaveRecognition, StoreId, StoreIdentifier, UserDataLocation,
    UserFileMapping, UserFileStrategy, VersionSource,
};

pub const GAME_ID: &str = "neon-courier";
pub const MO2_NAME: &str = "Neon Courier";
pub const VORTEX_ID: &str = "neoncourier";
pub const GAME_DIR: &str = "Neon Courier";
pub const SAVES_DIR: &str = "Neon Courier/Saves";

fn store() -> StoreId {
    StoreId::steam()
}

fn documents(path: &str) -> GamePath {
    GamePath::UserData {
        location: UserDataLocation::Documents,
        path: RelPath::new(path).unwrap(),
    }
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
pub fn definition() -> GameDefinition {
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
        save_paths: vec![SaveFolderSpec::Rule(SaveFolderRule {
            path: documents(SAVES_DIR),
            recognition: Some(SaveRecognition::Folders),
            own_value: Some("Agora/{instance}/characters".into()),
            relative_to: None,
        })],
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

/// The same game with an INI-backed save rule, so a test can reach the survey and the own/shared
/// save paths at all. Real Neon Courier has no such INI; that is the finding.
pub fn with_save_rule() -> GameDefinition {
    let mut def = definition();
    def.user_files = vec![UserFileMapping::new(
        documents("Neon Courier/settings.ini"),
        RelPath::new("user/settings.ini").unwrap(),
        UserFileStrategy::JournaledSwap,
    )
    .with_stores(vec![store()])];
    def.save_location = vec![SaveLocationRule {
        ini: RelPath::new("user/settings.ini").unwrap(),
        section: "Saves".into(),
        key: "Path".into(),
        own_value: r"Saves\Agora\{instance}\".into(),
        shared_dir: documents(SAVES_DIR),
        relative_to: documents("Neon Courier"),
        recognition: Some(agora_game_api::SaveRecognition::Folders),
        stores: vec![store()],
    }];
    def
}

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

/// Register a Neon Courier definition as a compiled test package, the way the desktop registers
/// shipped ones.
pub fn registry(def: GameDefinition) -> Arc<GameRegistry> {
    try_registry(def).expect("the Neon Courier test package registers")
}

pub fn try_registry(
    def: GameDefinition,
) -> Result<Arc<GameRegistry>, agora_core::game_registry::GameRegistryError> {
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
    builder.add(
        PackageSource::Compiled {
            crate_name: "second-game-test".into(),
        },
        Arc::new(TestPackage(package)),
    )?;
    Ok(Arc::new(builder.build()))
}
