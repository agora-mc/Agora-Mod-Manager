//! Importing a Vortex setup (MASTER_SPEC §26.10).
//!
//! Vortex keeps its state in a LevelDB store (`state.v2`) and writes a deployment manifest into
//! each game's mod folder. This module reads a **copy** of that store (see [`vortex_store`]) and
//! gives the importer the same pieces the MO2 import uses: mod folders copied bytes-first into the
//! content store, per-profile enabled state, the plugin list, and a layer order.
//!
//! Format rules and where they come from (GPL-3; read for the format, never copied):
//! - `persistent.mods` is `{[gameId]: {[modId]: IMod}}` and `persistent.profiles` is
//!   `{[profileId]: IProfile}` (`src_renderer_src_types_IState.ts:473-489`).
//! - An `IMod` has `id`, `state`, `installationPath` (its folder in the staging folder) and
//!   `attributes` (`src_renderer_src_extensions_mod_management_types_IMod.ts:135-172`); the Nexus
//!   ids are `attributes.modId` / `attributes.fileId` and the version is `attributes.version`
//!   (`:51-98`).
//! - An `IProfile` has `id`, `gameId`, `name` and `modState: {[modId]: {enabled}}`
//!   (`src_renderer_src_extensions_profile_management_types_IProfile.ts:1-18`).
//! - The staging folder is `settings.mods.installPath[gameId]`, a template whose default is
//!   `{USERDATA}/{GAME}/mods` (`src_renderer_src_extensions_mod_management_util_getInstallPath.ts:9-27`
//!   and `..._resolvePath.ts:11-31`).
//! - The deployment manifest is `vortex.deployment.json` in the deploy target, listing every
//!   deployed file with its `source` (the staging folder that supplied it)
//!   (`..._util_activationStore.ts:37-46`, `src_renderer_src_extensions_mod_management_types_IDeploymentManifest.ts:3-12`).
//! - Per-profile `plugins.txt` / `loadorder.txt` are written by the gamebryo plugin persistor
//!   (`..._util_PluginPersistor.ts:557-627`).
//!
//! Where the source leaves a field unclear this module handles its absence and says so in the
//! plan's warnings; it never invents a value.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use agora_game_api::{fold_path_case, BaseReference, GameDefinition, GameId, RelPath, StoreId};

use super::vortex_store::{self, VortexStore};
use super::{
    abandon_half_made_instance, find_import_of_kind, mo2, ImportError, ImportPhase, ImportProgress,
    IniPlan, PluginLinePlan, PluginPlan, Problem, RunOptions, SavesPlan,
};
use crate::content_store::{self, ContentSource, VortexProvenance};
use crate::ctx::Ctx;
use crate::game_base::{BaseMode, BuildOptions, BuildOutcome};
use crate::game_deploy;
use crate::game_discovery::{
    volume::VolumeDetector, DiscoveredInstall, DiscoveryReport, InstallCapabilities, InstallKind,
};
use crate::game_ini;
use crate::game_instance::{self, GameInstanceManifest, SavesChoice};
use crate::game_load_order;
use crate::game_plugins;
use crate::game_registry::{GameInventory, IdentifiedInstall, RuntimeResolution};
use crate::lock_manager::LockResource;

/// Where Vortex's own files were read from, and what the state says. This is the parsed store,
/// before anything is matched to an Agora install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexState {
    /// The store the copy was made from.
    pub source: PathBuf,
    pub games: Vec<VortexGame>,
    pub warnings: Vec<String>,
}

/// One game in Vortex's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexGame {
    /// Vortex's game id, e.g. an id such as `skyrimse`. Agora never interprets it; it is matched to
    /// an install by the game folder Vortex discovered.
    pub id: String,
    /// The game folder Vortex discovered (`settings.gameMode.discovered[id].path`), when it is
    /// there. Absent when Vortex never discovered the game.
    pub discovered_path: Option<PathBuf>,
    /// The staging folder, resolved from `settings.mods.installPath[id]`, or why it cannot be.
    pub staging: Result<PathBuf, String>,
    pub mods: Vec<VortexMod>,
    pub profiles: Vec<VortexProfile>,
}

/// One game the store has mods for, as a scan lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VortexProfile {
    pub id: String,
    pub name: String,
    /// Whether each mod is enabled in this profile. A mod the profile does not mention is absent
    /// here and is imported disabled.
    pub enabled: BTreeMap<String, bool>,
    /// `features.local_game_settings`: the profile keeps its own copies of the game's INI files
    /// (`extensions/local-gamesettings/src/index.ts:290`, feature id at `:174`).
    pub local_settings: bool,
    /// `features.local_saves`: the profile keeps its own saves
    /// (`extensions/gamebryo-savegame-management/src/index.ts:645`).
    pub local_saves: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexMod {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub nexus_mod_id: Option<u64>,
    pub nexus_file_id: Option<u64>,
    /// `installationPath`: the mod's folder in the staging folder. Usually its id.
    pub installation_folder: String,
    /// `state` as Vortex stores it (`installed`, `downloading`, ...). Absent is unknown.
    pub state: Option<String>,
    pub rules: Vec<VortexRule>,
}

/// One ordering rule of a mod. Only `before` and `after` order mods; the rest are kept so the plan
/// can say what was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexRule {
    /// `before`, `after`, `requires`, `recommends`, `provides`, `conflicts`, ...
    pub kind: String,
    /// The referenced local mod id (`reference.id`), when the rule names one.
    pub target_mod_id: Option<String>,
    /// The referenced Nexus mod id (`reference.repo.modId`), when the rule names one.
    pub target_nexus_mod_id: Option<u64>,
    pub description: Option<String>,
}

impl VortexRule {
    fn is_order(&self) -> bool {
        matches!(self.kind.as_str(), "before" | "after")
    }
}

/// Read the state from a store folder. This is the one entry point both the scan and the import
/// use, so neither ever reads the live store.
pub fn read_state(store_dir: &Path) -> Result<VortexState, ImportError> {
    let store = vortex_store::read_store(store_dir)?;
    parse_state(&store)
}

/// Parse a store copy. A store without `persistent.mods` is an error: it is not an empty setup,
/// because there is nothing to import and we cannot say the setup has no mods.
pub fn parse_state(store: &VortexStore) -> Result<VortexState, ImportError> {
    let mods_value = store
        .get_path(&["persistent", "mods"])
        .ok_or(ImportError::VortexNoMods)?;
    let mods_table = mods_value.as_object().ok_or(ImportError::VortexNoMods)?;
    let profiles_value = store.get_path(&["persistent", "profiles"]);
    let profiles_table = profiles_value.as_ref().and_then(Value::as_object);
    let install_paths = store
        .get_path(&["settings", "mods", "installPath"])
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let discovered = store
        .get_path(&["settings", "gameMode", "discovered"])
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let user_data = store
        .source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok();

    let mut warnings = store.warnings.clone();
    let mut games = Vec::new();
    for (game_id, game_value) in mods_table {
        let Some(game_mods) = game_value.as_object() else {
            warnings.push(format!(
                "the mods Vortex lists for {game_id} are not an object; that game was skipped"
            ));
            continue;
        };
        if game_mods.is_empty() {
            warnings.push(format!(
                "Vortex lists no mods for {game_id}; its profiles are still listed"
            ));
        }
        let mut mods = Vec::new();
        for (mod_id, mod_value) in game_mods {
            let Some(mod_object) = mod_value.as_object() else {
                warnings.push(format!(
                    "mod '{mod_id}' in {game_id} is not an object; it was skipped"
                ));
                continue;
            };
            match parse_mod(mod_id, mod_object) {
                Some(parsed) => mods.push(parsed),
                None => warnings.push(format!(
                    "mod '{mod_id}' in {game_id} has no installationPath; it was skipped"
                )),
            }
        }
        mods.sort_by(|a, b| {
            fold_path_case(&a.name)
                .cmp(&fold_path_case(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut profiles = Vec::new();
        if let Some(table) = profiles_table {
            for (profile_id, profile_value) in table {
                let Some(profile) = profile_value.as_object() else {
                    continue;
                };
                let profile_game = get_str(profile, "gameId").unwrap_or_default();
                if !profile_game.eq_ignore_ascii_case(game_id) {
                    continue;
                }
                let id = get_str(profile, "id").unwrap_or_else(|| profile_id.clone());
                let name = get_str(profile, "name").unwrap_or_else(|| id.clone());
                // The id names the profile's folder under Vortex's user data, so it is a folder
                // name and never a path. An id that could escape that folder is refused before
                // any read is joined to it.
                if let Err(reason) = mo2::check_mod_folder_name(&id) {
                    warnings.push(format!(
                        "profile '{name}' in {game_id} has an id that is not a folder name ({reason}); it was skipped"
                    ));
                    continue;
                }
                let mut enabled = BTreeMap::new();
                if let Some(mod_state) = profile.get("modState").and_then(Value::as_object) {
                    for (mod_id, entry) in mod_state {
                        let on = entry
                            .as_object()
                            .and_then(|e| e.get("enabled"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        enabled.insert(mod_id.clone(), on);
                    }
                }
                let features = profile.get("features").and_then(Value::as_object);
                let local_settings = feature_on(features, "local_game_settings");
                let local_saves = feature_on(features, "local_saves");
                profiles.push(VortexProfile {
                    id,
                    name,
                    enabled,
                    local_settings,
                    local_saves,
                });
            }
        }
        profiles.sort_by(|a, b| {
            fold_path_case(&a.name)
                .cmp(&fold_path_case(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        if profiles.is_empty() {
            warnings.push(format!(
                "Vortex lists no profile for {game_id}; its mods cannot be imported until one exists"
            ));
        }

        let pattern = get_ci(&install_paths, game_id)
            .and_then(Value::as_str)
            .map(str::to_string);
        let staging =
            resolve_install_path(pattern.as_deref(), &user_data, game_id, username.as_deref());
        if let Err(reason) = &staging {
            warnings.push(format!(
                "the staging folder for {game_id} cannot be read from the store: {reason}"
            ));
        }

        let discovered_path = get_ci(&discovered, game_id)
            .and_then(Value::as_object)
            .and_then(|entry| get_str(entry, "path"))
            .map(PathBuf::from);

        games.push(VortexGame {
            id: game_id.clone(),
            discovered_path,
            staging,
            mods,
            profiles,
        });
    }
    games.sort_by(|a, b| a.id.cmp(&b.id));
    if games.is_empty() {
        // `persistent.mods` exists but names no game: say so, rather than pretending it is empty.
        return Err(ImportError::VortexNoMods);
    }

    Ok(VortexState {
        source: store.source.clone(),
        games,
        warnings,
    })
}

fn parse_mod(mod_id: &str, object: &Map<String, Value>) -> Option<VortexMod> {
    let installation_folder = get_str(object, "installationPath")?.trim().to_string();
    if installation_folder.is_empty() {
        return None;
    }
    let attributes = object.get("attributes").and_then(Value::as_object);
    let name = attributes
        .and_then(|a| {
            get_str(a, "name")
                .or_else(|| get_str(a, "modName"))
                .or_else(|| get_str(a, "customFileName"))
        })
        .unwrap_or_else(|| mod_id.to_string());
    let version =
        attributes.and_then(|a| get_str(a, "version").or_else(|| get_str(a, "modVersion")));
    let nexus_mod_id = attributes.and_then(|a| get_u64(a, "modId"));
    let nexus_file_id = attributes.and_then(|a| get_u64(a, "fileId"));
    let state = get_str(object, "state");
    let mut rules = Vec::new();
    if let Some(list) = object.get("rules").and_then(Value::as_array) {
        for rule in list {
            let Some(rule) = rule.as_object() else {
                continue;
            };
            let reference = rule.get("reference").and_then(Value::as_object);
            let repo = reference
                .and_then(|r| r.get("repo"))
                .and_then(Value::as_object);
            rules.push(VortexRule {
                kind: get_str(rule, "type").unwrap_or_default(),
                target_mod_id: reference.and_then(|r| get_str(r, "id")),
                target_nexus_mod_id: repo.and_then(|r| get_u64(r, "modId")),
                description: reference
                    .and_then(|r| get_str(r, "description"))
                    .or_else(|| reference.and_then(|r| get_str(r, "tag"))),
            });
        }
    }
    Some(VortexMod {
        id: mod_id.to_string(),
        name,
        version,
        nexus_mod_id,
        nexus_file_id,
        installation_folder,
        state,
        rules,
    })
}

/// A profile feature is on only when it is the boolean `true`; any other value is off, as Vortex
/// reads it (`getSafe(profile, ["features", id], false)`).
fn feature_on(features: Option<&Map<String, Value>>, id: &str) -> bool {
    features
        .and_then(|f| get_ci(f, id))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn get_ci<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    object.get(key).or_else(|| {
        object
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    })
}

fn get_str(object: &Map<String, Value>, key: &str) -> Option<String> {
    get_ci(object, key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

fn get_u64(object: &Map<String, Value>, key: &str) -> Option<u64> {
    let value = get_ci(object, key)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Resolve `settings.mods.installPath[gameId]` with Vortex's placeholders. The default is
/// `{USERDATA}/{GAME}/mods` (`..._getInstallPath.ts:9-11`), `{USERDATA}` is the folder holding
/// `state.v2`, `{GAME}` is the game id and `{base}` is `{USERDATA}/{GAME}` (`..._resolvePath.ts:11-31`).
/// A rootless path is relative to the user-data folder (`..._getInstallPath.ts:33-39`).
pub fn resolve_install_path(
    pattern: Option<&str>,
    user_data: &Path,
    game_id: &str,
    username: Option<&str>,
) -> Result<PathBuf, String> {
    let base = user_data.join(game_id);
    let template = pattern
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or("{USERDATA}/{GAME}/mods");
    let user_data_text = user_data.to_string_lossy().to_string();
    let base_text = base.to_string_lossy().to_string();
    let username = username.unwrap_or("");
    let mut text = resolve_template(
        template,
        &[
            ("userdata", user_data_text.as_str()),
            ("game", game_id),
            ("username", username),
            ("base", base_text.as_str()),
        ],
    )?;
    text = text.replace(['/', '\\'], std::path::MAIN_SEPARATOR_STR);
    let path = PathBuf::from(&text);
    Ok(if path.is_absolute() {
        path
    } else {
        user_data.join(path)
    })
}

fn resolve_template(template: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close_rel) = rest[open..].find('}') else {
            return Err(format!(
                "the path '{template}' has an unclosed '{{'; it was not read"
            ));
        };
        let close = open + close_rel;
        let token = &rest[open + 1..close];
        match vars
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(token))
        {
            Some((_, value)) => out.push_str(value),
            None => {
                return Err(format!(
                "the path '{template}' uses the placeholder {{{token}}}, which Agora does not know"
            ))
            }
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// List what an import would bring: every game Vortex has mods for, its profiles, and whether the
/// game folder Vortex discovered is one Agora supports. Reads the store only; writes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VortexScan {
    pub store: PathBuf,
    pub games: Vec<VortexScanGame>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VortexScanGame {
    /// Vortex's own game id.
    pub vortex_game: String,
    /// The Agora game whose package names this Vortex id in `import_ids.vortex`.
    pub game: Option<GameId>,
    pub game_name: Option<String>,
    /// Whether an Agora package maps this Vortex game. A supported game whose folder Agora has
    /// not added yet is still supported; `needs_folder_add` says so.
    pub supported: bool,
    /// Why it cannot be imported yet, in the user's words.
    pub reason: Option<String>,
    pub discovered_path: Option<PathBuf>,
    /// The Agora install id, when the game folder is already one Agora lists.
    pub install: Option<String>,
    /// The folder is a real install of the game, but Agora has not added it.
    pub needs_folder_add: bool,
    /// What checking the folder found: a real install, or why it is not one.
    pub detection: Option<String>,
    pub staging: Option<PathBuf>,
    pub staging_error: Option<String>,
    /// Whether the staging folder has Vortex's `__vortex_staging_folder` marker.
    pub staging_marker: bool,
    pub mods: usize,
    pub profiles: Vec<VortexScanProfile>,
    /// A `vortex.deployment*.json` was found in the game's deploy folder(s).
    pub manifest_found: bool,
    pub manifest_files: usize,
    /// Every place the manifest was looked for, so "not found" says where.
    pub manifest_searched: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VortexScanProfile {
    pub id: String,
    pub name: String,
    pub mods_enabled: usize,
    pub mods_total: usize,
    /// The ids of the mods this profile enables, so a grouping can compare profiles.
    #[serde(default)]
    pub enabled_mod_ids: Vec<String>,
    pub plugins_txt: bool,
    pub loadorder_txt: bool,
    pub local_settings: bool,
    pub local_saves: bool,
}

/// Scan the store: which games and profiles an import could bring, and which games Agora cannot
/// import. A game is matched to an Agora game only through the folder Vortex discovered, so no
/// game id mapping is guessed.
pub fn scan(
    ctx: &Ctx,
    inventory: &GameInventory,
    store_dir: &Path,
) -> Result<VortexScan, ImportError> {
    let state = read_state(store_dir)?;
    let mut games = Vec::new();
    let mut warnings = state.warnings.clone();
    for game in &state.games {
        let definition = definition_for_vortex(ctx, &game.id);
        let mut reason = None;
        let mut install_id = None;
        let mut needs_folder_add = false;
        let mut detection = None;
        let mut manifest = ManifestSearch::default();
        let mut staging_marker = false;

        if definition.is_none() {
            reason = Some(format!(
                "no installed game package names Vortex's '{}' id. Install the game's Agora plugin first: `agora registry sync`, then `agora plugin official list` and `agora plugin official install <plugin-id>`; if it is already installed, update it with `agora plugin official update <plugin-id>`",
                game.id
            ));
        } else if let Some(def) = &definition {
            match &game.discovered_path {
                None => {
                    reason = Some(format!(
                        "Vortex never discovered a game folder for {}, so Agora cannot check it",
                        game.id
                    ));
                }
                Some(path) => match folder_state(ctx, inventory, def, path) {
                    FolderState::Installed(install) => {
                        install_id = Some(install.install_id.as_str().to_string());
                    }
                    FolderState::NeedsAdd { why } => {
                        needs_folder_add = true;
                        detection = Some(why);
                    }
                    FolderState::NotThisGame { why } => {
                        reason = Some(why);
                        detection = Some("not an install Agora can import".to_string());
                    }
                },
            }
        }
        if let Some(path) = &game.discovered_path {
            manifest = find_manifests(definition, path);
        }
        if let Some(damaged) = &manifest.damaged {
            warnings.push(format!(
                "a deployment manifest could not be read: {damaged}"
            ));
        }
        if let Ok(staging) = &game.staging {
            staging_marker = staging.join(VORTEX_STAGING_MARKER).is_file();
            if !staging_marker && staging.is_dir() {
                warnings.push(format!(
                    "the staging folder {} has no {} marker; it may not be the folder Vortex uses",
                    staging.display(),
                    VORTEX_STAGING_MARKER
                ));
            }
        }

        let mut profiles = Vec::new();
        for profile in &game.profiles {
            let enabled = game
                .mods
                .iter()
                .filter(|m| profile.enabled.get(&m.id).copied().unwrap_or(false))
                .count();
            let (plugins_txt, loadorder_txt) = plugin_files_found(&state, game, profile);
            let enabled_mod_ids: Vec<String> = game
                .mods
                .iter()
                .filter(|m| profile.enabled.get(&m.id).copied().unwrap_or(false))
                .map(|m| m.id.clone())
                .collect();
            profiles.push(VortexScanProfile {
                id: profile.id.clone(),
                name: profile.name.clone(),
                mods_enabled: enabled,
                mods_total: game.mods.len(),
                enabled_mod_ids,
                plugins_txt,
                loadorder_txt,
                local_settings: profile.local_settings,
                local_saves: profile.local_saves,
            });
        }
        if game.staging.is_err() {
            warnings.push(format!(
                "the staging folder for {} could not be resolved from the store",
                game.id
            ));
        }
        games.push(VortexScanGame {
            vortex_game: game.id.clone(),
            game: definition.as_ref().map(|def| def.id.clone()),
            game_name: definition.as_ref().map(|def| def.name.clone()),
            supported: definition.is_some(),
            reason,
            discovered_path: game.discovered_path.clone(),
            install: install_id,
            needs_folder_add,
            detection,
            staging: game.staging.clone().ok(),
            staging_error: game.staging.clone().err(),
            staging_marker,
            mods: game.mods.len(),
            profiles,
            manifest_found: manifest.found,
            manifest_files: manifest.files,
            manifest_searched: manifest.searched,
        });
    }
    Ok(VortexScan {
        store: state.source,
        games,
        warnings,
    })
}

/// The Agora game whose package names this Vortex id, if any. The mapping lives in the game package
/// (`import_ids.vortex`), so core never carries a table of game ids.
fn definition_for_vortex<'a>(ctx: &'a Ctx, vortex_game: &str) -> Option<&'a GameDefinition> {
    ctx.games.games().find(|definition| {
        definition.import_ids.iter().any(|(manager, id)| {
            manager.eq_ignore_ascii_case("vortex") && id.eq_ignore_ascii_case(vortex_game)
        })
    })
}

/// The Vortex game and the Agora definition it maps to, by either id. `wanted` may be the Agora
/// game id (`skyrim-se`) or the Vortex game id (`skyrimse`), because the scan lists the latter and
/// users type what they see. This is the one resolution the scan and the import share.
fn resolve_game<'a>(
    ctx: &'a Ctx,
    state: &'a VortexState,
    wanted: &str,
) -> Option<(&'a VortexGame, &'a GameDefinition)> {
    state.games.iter().find_map(|game| {
        let definition = definition_for_vortex(ctx, &game.id)?;
        let matches = definition.id.as_str().eq_ignore_ascii_case(wanted)
            || definition
                .import_ids
                .values()
                .any(|id| id.eq_ignore_ascii_case(wanted));
        matches.then_some((game, definition))
    })
}

/// An install identified from the folder as if it had been added, without writing the folder list.
/// A dry run uses it to show the plan the real import would make.
fn speculative_install(
    ctx: &Ctx,
    definition: &GameDefinition,
    path: &Path,
) -> Option<IdentifiedInstall> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let discovered = DiscoveredInstall {
        store: StoreId::direct(),
        product: normalise_path(path),
        name,
        kind: InstallKind::BaseGame,
        parent_product: None,
        location: path.to_path_buf(),
        store_version: None,
        store_build: None,
        executables: Vec::new(),
        capabilities: InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
        volume: VolumeDetector::new().get_volume_info(path),
    };
    let report = DiscoveryReport {
        installs: vec![discovered],
        warnings: Vec::new(),
    };
    let inventory = crate::game_registry::identify_installs(
        &ctx.games,
        &report,
        &crate::game_discovery::file_version::read_file_version,
    );
    inventory
        .installs
        .into_iter()
        .find(|install| install.game == definition.id)
}

enum FolderState<'a> {
    Installed(&'a IdentifiedInstall),
    NeedsAdd { why: String },
    NotThisGame { why: String },
}

/// The one sentence the scan and the import both use when a real install is not in Agora yet.
fn needs_add_message(definition: &GameDefinition, path: &Path) -> String {
    format!(
        "{} is a real {} install, but Agora has not added it; run `agora games folder add \"{}\"`, or import with --add-folder",
        path.display(),
        definition.name,
        path.display()
    )
}

/// What the folder Vortex discovered is for this game. The check is the package's own: its
/// executable must be in the folder (`GameRegistry::games_with_executable_in`).
fn folder_state<'a>(
    ctx: &Ctx,
    inventory: &'a GameInventory,
    definition: &GameDefinition,
    path: &Path,
) -> FolderState<'a> {
    if !path.is_dir() {
        return FolderState::NotThisGame {
            why: format!(
                "Vortex's folder for {} is {}, which is not there",
                definition.name,
                path.display()
            ),
        };
    }
    let in_folder = ctx.games.games_with_executable_in(path);
    if !in_folder.iter().any(|id| id == &definition.id) {
        let names: Vec<String> = in_folder
            .iter()
            .filter_map(|id| ctx.games.game(id).map(|game| game.name.clone()))
            .collect();
        let executable = definition
            .version_sources
            .iter()
            .find_map(|source| match source {
                agora_game_api::VersionSource::Executable { path } => Some(path.as_str()),
                _ => None,
            })
            .unwrap_or("its executable");
        return FolderState::NotThisGame {
            why: if names.is_empty() {
                format!(
                    "{} does not hold {} ({}), so Agora cannot import it as {}",
                    path.display(),
                    executable,
                    definition.name,
                    definition.name
                )
            } else {
                format!(
                    "{} holds {}, not {}; Vortex's folder and the game do not match",
                    path.display(),
                    names.join(", "),
                    definition.name
                )
            },
        };
    }
    if let Some(install) = inventory.installs.iter().find(|install| {
        install.game == definition.id
            && normalise_path(&install.discovered.location) == normalise_path(path)
    }) {
        return FolderState::Installed(install);
    }
    FolderState::NeedsAdd {
        why: needs_add_message(definition, path),
    }
}

/// Add Vortex's game folder to Agora when the request asks for it, then read the install list
/// again. A dry run never calls this (`plan` only says it would add the folder).
fn add_folder_for_request(
    ctx: &Ctx,
    inventory: &GameInventory,
    request: &VortexRequest,
) -> Result<GameInventory, ImportError> {
    if !request.add_folder {
        return Ok(inventory.clone());
    }
    let state = read_state(&request.store)?;
    let (game, definition) = resolve_game(ctx, &state, request.game.as_str())
        .ok_or_else(|| ImportError::NoVortexGame(request.game.as_str().to_string()))?;
    let Some(path) = &game.discovered_path else {
        return Err(ImportError::VortexFolderMissing {
            game: definition.name.clone(),
        });
    };
    if inventory.installs.iter().any(|install| {
        install.game == definition.id
            && normalise_path(&install.discovered.location) == normalise_path(path)
    }) {
        return Ok(inventory.clone());
    }
    match crate::game_discovery::custom_folders::add_folder(
        &ctx.paths,
        &ctx.games,
        &path.to_string_lossy(),
    ) {
        Ok(_) => {}
        Err(error) => {
            let already = matches!(
                &error,
                crate::error::LauncherError::Generic { code, .. }
                    if code == crate::game_discovery::custom_folders::ERR_FOLDER_DUPLICATE
                        || code == crate::game_discovery::custom_folders::ERR_FOLDER_INSIDE_INSTALL
            );
            if !already {
                return Err(ImportError::FolderAdd(error.to_string()));
            }
        }
    }
    Ok(crate::game_registry::installed_games(ctx))
}

/// The folder a game's content mounts at, from its content layout. Vortex writes the default
/// manifest into the deploy target (`..._util_activationStore.ts:37-46`): the game root when the
/// layout has no data path (Cyberpunk), and `Data` for the Creation Engine games.
fn data_mount(def: &GameDefinition) -> String {
    def.content_layout
        .as_ref()
        .map(|layout| layout.data_path.as_str().to_string())
        .unwrap_or_else(|| "Data".to_string())
}

/// The `IDeployedFile.target` subfolder: where a file was placed under the deploy target
/// (`deployedFilePath.ts` joins `target`, then `relPath`).
fn join_target(target: Option<&str>, rel_path: &str) -> String {
    match target.map(str::trim).filter(|t| !t.is_empty()) {
        Some(target) => format!(
            "{}/{}",
            target.replace('\\', "/").trim_end_matches('/'),
            rel_path
        ),
        None => rel_path.to_string(),
    }
}

/// The folders Vortex may have deployed into for this game: the game root, and its data mount.
fn manifest_folders(def: Option<&GameDefinition>, install: &Path) -> Vec<PathBuf> {
    let mut roots = vec![install.to_path_buf()];
    if let Some(def) = def {
        let mount = data_mount(def);
        if !mount.is_empty() {
            roots.push(install.join(&mount));
        }
    }
    roots
}

/// The candidate manifest files in one folder: the default `vortex.deployment.json` and any
/// `vortex.deployment.<mod type>.json` (`..._util_activationStore.ts:37-46`).
fn manifest_files_in(folder: &Path) -> Vec<PathBuf> {
    // The default is always a candidate, found or not, so "not found" can say where it looked.
    let mut files = vec![folder.join("vortex.deployment.json")];
    if let Ok(entries) = std::fs::read_dir(folder) {
        let mut typed: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            let lower = name.to_ascii_lowercase();
                            // `vortex.deployment.<type>.json`, not the default itself (which also
                            // starts and ends the same way).
                            lower != "vortex.deployment.json"
                                && lower.starts_with("vortex.deployment.")
                                && lower.ends_with(".json")
                        })
            })
            .collect();
        typed.sort();
        files.extend(typed);
    }
    files
}

#[derive(Debug, Default)]
struct ManifestSearch {
    found: bool,
    files: usize,
    entries: Vec<ManifestEntry>,
    searched: Vec<PathBuf>,
    damaged: Option<String>,
}

/// Read every deployment manifest the game's deploy folders hold. A missing one is not an error
/// (`found: false`), and `searched` says where we looked.
fn find_manifests(def: Option<&GameDefinition>, install: &Path) -> ManifestSearch {
    let mut search = ManifestSearch::default();
    for folder in manifest_folders(def, install) {
        let candidates = manifest_files_in(&folder);
        for path in candidates {
            search.searched.push(path.clone());
            if !path.is_file() {
                continue;
            }
            search.found = true;
            let read = read_manifest(&path);
            search.files += read.files;
            if let Some(damaged) = read.damaged {
                if search.damaged.is_none() {
                    search.damaged = Some(format!("{}: {damaged}", path.display()));
                }
            }
            search.entries.extend(read.entries);
        }
    }
    search.searched.sort();
    search.searched.dedup();
    search
}

const VORTEX_STAGING_MARKER: &str = "__vortex_staging_folder";

fn normalise_path(path: &Path) -> String {
    // Vortex, a game store and a user can all spell one folder differently: an
    // 8.3 short name, another case, a trailing separator, `/` for `\`, or the
    // `\?\` prefix. Resolve the real folder first, so two spellings of one
    // folder compare equal; a folder that is gone keeps its stored spelling.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let shown = crate::game_discovery::custom_folders::display_path(&resolved);
    fold_path_case(shown.replace('/', "\\").trim_end_matches('\\'))
}

/// Where Vortex keeps a profile's plugin files. The source names the files but not their folder;
/// `PluginPersistor` writes them under the profile's own folder (`PluginPersistor.ts:557-627`), and
/// Vortex's per-profile data lives under `<userData>/<game>/profiles/<profileId>`. Absent files are
/// reported, never treated as an empty plugin list.
fn plugin_files(store_source: &Path, game_id: &str, profile_id: &str) -> (PathBuf, PathBuf) {
    let dir = profile_folder(store_source, game_id, profile_id);
    (dir.join("plugins.txt"), dir.join("loadorder.txt"))
}

/// A profile's own folder: `<userData>/<gameId>/profiles/<profileId>/`
/// (`extensions/local-gamesettings/src/util/gameSupport.ts:83-85`). It holds the plugin files,
/// empty userlist, and the INIs a profile with `local_game_settings` keeps.
fn profile_folder(store_source: &Path, game_id: &str, profile_id: &str) -> PathBuf {
    let user_data = store_source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    user_data.join(game_id).join("profiles").join(profile_id)
}

/// A profile's own INI copies, as the game defines its per-user INI files. Vortex names the files
/// by their base name in the profile folder (`copyGameSettings` at
/// `extensions/local-gamesettings/src/index.ts:17-19`), and only keeps them when the profile has
/// `local_game_settings` (`:290`).
fn plan_inis(
    def: &GameDefinition,
    store: &StoreId,
    profile_dir: &Path,
    local: bool,
    warnings: &mut Vec<String>,
) -> Vec<IniPlan> {
    if !local {
        return Vec::new();
    }
    if !profile_dir.is_dir() {
        warnings.push(format!(
            "the profile keeps its own game settings, but its folder is not there: {}",
            profile_dir.display()
        ));
        return Vec::new();
    }
    // A custom folder has no store, so no store-specific mapping applies. The profile's files are
    // named by their base name; use the first mapping for each instance path, which is the same
    // file the game reads.
    let mut mappings: Vec<&agora_game_api::UserFileMapping> = def
        .user_files
        .iter()
        .filter(|m| m.applies_to_store(store))
        .collect();
    if mappings.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for mapping in &def.user_files {
            if seen.insert(mapping.instance_path.as_str().to_ascii_lowercase()) {
                mappings.push(mapping);
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for mapping in mappings {
        let instance_path = mapping.instance_path.as_str();
        if !instance_path.to_ascii_lowercase().ends_with(".ini") {
            continue;
        }
        if !seen.insert(instance_path.to_ascii_lowercase()) {
            continue;
        }
        let name = instance_path.rsplit('/').next().unwrap_or(instance_path);
        let from = profile_dir.join(name);
        if from.is_file() {
            out.push(IniPlan {
                name: name.to_string(),
                instance_path: instance_path.to_string(),
                from,
            });
        } else {
            warnings.push(format!(
                "the profile's own {name} was not in {}; that INI is not copied",
                profile_dir.display()
            ));
        }
    }
    out
}

/// One entry in the profile's own save folder, as the folder listed it.
struct SaveEntry {
    path: PathBuf,
    metadata: std::io::Result<std::fs::Metadata>,
}

/// A readable save file's size; a folder or a link counts no file. A size that cannot be read is
/// reported, never silently counted as 0.
fn sized_save(entry: SaveEntry, warnings: &mut Vec<String>) -> Option<u64> {
    match entry.metadata {
        Ok(meta) if meta.is_file() => Some(meta.len()),
        Ok(_) => None,
        Err(error) => {
            warnings.push(format!(
                "a save file's size could not be read ({}): {error}",
                entry.path.display()
            ));
            None
        }
    }
}

/// A profile's own saves. Vortex keeps them under the game's shared save folder, in a folder named
/// after the profile id (`extensions/gamebryo-savegame-management/src/util/profileSavePath.ts:8-20`),
/// not in the profile folder. Only the count is read here; the files are copied with --copy-saves.
fn plan_saves(
    def: &GameDefinition,
    store: &StoreId,
    profile_id: &str,
    local: bool,
    warnings: &mut Vec<String>,
) -> Result<SavesPlan, ImportError> {
    if !local {
        return Ok(SavesPlan {
            local: false,
            files: 0,
            bytes: 0,
            from: None,
            to: None,
            note: "the profile shares the game's saves".into(),
        });
    }
    let mut direct = false;
    let rule = def
        .save_location
        .iter()
        .find(|rule| rule.applies_to_store(store))
        .or_else(|| {
            // A custom folder has no store. The first rule still names the Documents save
            // folder, so the source can be counted; the instance cannot be told to use it
            // because its store is unknown, so its saves stay shared.
            direct = store.as_str() == "direct";
            direct.then(|| def.save_location.first()).flatten()
        });
    let Some(rule) = rule else {
        return Ok(SavesPlan {
            local: true,
            files: 0,
            bytes: 0,
            from: None,
            to: None,
            note: "the game has no save-folder setting for this store, so the saves stay shared"
                .into(),
        });
    };
    let base = crate::game_saves::shared_folder(rule).map_err(|error| ImportError::Unreadable {
        path: PathBuf::new(),
        reason: format!("the game's save folder cannot be resolved: {error}"),
    })?;
    let from = base.join(profile_id);
    let mut files = 0usize;
    let mut bytes = 0u64;
    match std::fs::read_dir(&from) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => SaveEntry {
                        path: entry.path(),
                        metadata: entry.metadata(),
                    },
                    Err(error) => {
                        warnings.push(format!(
                            "an entry in the profile's own save folder could not be read: {error}"
                        ));
                        continue;
                    }
                };
                if let Some(size) = sized_save(entry, warnings) {
                    files += 1;
                    bytes += size;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ImportError::Unreadable {
                path: from,
                reason: format!("the profile's own saves cannot be read: {error}"),
            })
        }
    }
    Ok(SavesPlan {
        local: !direct,
        files,
        bytes,
        from: Some(from),
        to: None,
        note: if direct {
            "the install is a custom folder, so Agora cannot tell which store's Documents folder it uses; the saves stay shared"
                .to_string()
        } else {
            String::new()
        },
    })
}

fn plugin_files_found(
    state: &VortexState,
    game: &VortexGame,
    profile: &VortexProfile,
) -> (bool, bool) {
    let (plugins, loadorder) = plugin_files(&state.source, &game.id, &profile.id);
    (plugins.is_file(), loadorder.is_file())
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// What an import was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VortexRequest {
    /// The store's folder (`state.v2`), never the live one.
    pub store: PathBuf,
    /// The Agora game to import into, as `agora games list` names it.
    pub game: GameId,
    /// The Vortex profile, by name.
    pub profile: String,
    /// The new instance's name. Required to import the same profile a second time.
    pub name: Option<String>,
    /// The install to import onto, by install id. `None` uses the install at the game folder
    /// Vortex discovered.
    pub install: Option<String>,
    /// Add Vortex's game folder to Agora (the `games folder add` path) before importing.
    pub add_folder: bool,
    /// Copy the profile's own saves into the instance. INIs are copied with their profile.
    pub copy_saves: bool,
    /// Bring the profile's disabled mods too, turned off. Off by default: a game's staging folder
    /// is shared by every profile.
    pub all_mods: bool,
}

/// A mod folder the profile lists, and what reading it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedMod {
    pub mod_id: String,
    pub name: String,
    pub version: Option<String>,
    pub nexus_mod_id: Option<u64>,
    pub nexus_file_id: Option<u64>,
    /// The folder's name in the staging folder (`installationPath`).
    pub folder: String,
    pub enabled: bool,
    pub files: usize,
    pub bytes: u64,
    pub provenance: VortexProvenance,
    /// Plugins at the mod's root, which the game loads from `Data`.
    pub plugin_files: Vec<String>,
    /// The ordering rules this mod states. Only `before` and `after` are used.
    #[serde(skip)]
    pub rules: Vec<VortexRule>,
    /// The files as they were collected, reused by the run so it stores exactly what was planned.
    #[serde(skip)]
    pub paths: Vec<(RelPath, PathBuf)>,
    /// Folded relative paths, for comparing with the deployment manifest.
    #[serde(skip)]
    pub file_set: BTreeSet<String>,
}

/// What the deployment manifest says, as far as the import can use it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestPlan {
    /// The first place a manifest was found, or the first place looked when none was.
    pub path: PathBuf,
    pub found: bool,
    /// Entries the manifest lists.
    pub files: usize,
    /// Entries whose `source` is one of the mods being imported.
    pub matched: usize,
    /// Why the manifest cannot be used, when it exists but cannot be read.
    pub damaged: Option<String>,
    /// Every file looked for, so a missing manifest says where.
    pub searched: Vec<PathBuf>,
}

/// One file the manifest and Vortex's ordering rules name different winners for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderDisagreement {
    /// The file, relative to the game's mod folder.
    pub file: String,
    /// The mod the manifest says supplied it.
    pub manifest_supplied: String,
    /// The mod the rules alone would put on top.
    pub rules_supply: String,
}

/// A LOOT userlist `after` entry, as a load-after rule Agora will store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedRule {
    pub item: String,
    pub after: String,
}

/// A userlist rule the import will not add, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedRule {
    pub item: String,
    pub after: String,
    pub reason: String,
}

/// The profile's `userlist.yaml`, read for its `after` entries. `req` and `inc` have no Agora
/// equivalent and are named in `not_imported`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UserlistPlan {
    pub path: Option<PathBuf>,
    pub rules: Vec<PlannedRule>,
    pub refused: Vec<RefusedRule>,
    pub not_imported: Vec<String>,
}

/// Everything an import would do, read from the store and checked. Nothing is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VortexPlan {
    pub store: PathBuf,
    /// Vortex's own game id.
    pub vortex_game: String,
    pub game: GameId,
    pub game_name: String,
    pub profile: String,
    pub profile_id: String,
    pub instance_name: String,
    pub install_id: String,
    pub install_location: PathBuf,
    /// The store id of the install Agora imports onto.
    pub store_id: StoreId,
    /// The folder is a real install but Agora has not added it. A real import adds it first; a
    /// plan (`--dry-run`) only says it would.
    pub would_add_folder: bool,
    pub staging: PathBuf,
    pub mods: Vec<PlannedMod>,
    pub mods_enabled: usize,
    pub mods_disabled: usize,
    /// Disabled mods left out because `--all-mods` was not given, by name.
    pub mods_left_out: Vec<String>,
    pub files: usize,
    pub bytes: u64,
    pub manifest: ManifestPlan,
    /// The mod ids, highest layer first. The manifest decides; Vortex's rules break ties.
    pub order: Vec<String>,
    pub disagreements: Vec<OrderDisagreement>,
    pub plugins: PluginPlan,
    /// The profile's LOOT userlist `after` entries, and what was not imported.
    pub userlist: UserlistPlan,
    /// The profile keeps its own copies of the game's INI files.
    pub local_settings: bool,
    pub inis: Vec<IniPlan>,
    /// The profile keeps its own saves.
    pub local_saves: bool,
    pub saves: SavesPlan,
    pub problems: Vec<Problem>,
    pub warnings: Vec<String>,
}

/// Plan an import: read the store copy, match the game to an install, read the mods and the
/// deployment manifest, and say what the layer order and plugin list would be. Reads only.
pub fn plan(
    ctx: &Ctx,
    inventory: &GameInventory,
    request: &VortexRequest,
) -> Result<VortexPlan, ImportError> {
    let state = read_state(&request.store)?;
    let (game, definition, install, would_add) =
        match_for_request(ctx, inventory, &state, request)?;

    let profile = game
        .profiles
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(&request.profile) || p.id == request.profile)
        .ok_or_else(|| ImportError::NoVortexProfile {
            profile: request.profile.clone(),
            game: definition.name.clone(),
        })?;

    let staging = match &game.staging {
        Ok(path) => path.clone(),
        Err(reason) => {
            return Err(ImportError::BadStagingPath {
                game: definition.name.clone(),
                reason: reason.clone(),
            })
        }
    };
    if !staging.is_dir() {
        return Err(ImportError::NoStaging {
            game: definition.name.clone(),
            path: staging,
        });
    }

    let mut warnings = state.warnings.clone();
    if would_add {
        warnings.push(format!(
            "{} is not in Agora yet; this import will add it first. A dry run writes nothing.",
            install.discovered.location.display()
        ));
    }
    let mut problems = Vec::new();
    let mods = plan_mods(game, profile, &staging, request.all_mods, &mut problems);
    let enabled = mods.iter().filter(|m| m.enabled).count();
    // A game's staging folder is shared by every profile. Unless asked, only this profile's
    // enabled mods are imported; the disabled ones are named for --verbose and counted.
    let mods_left_out: Vec<String> = if request.all_mods {
        Vec::new()
    } else {
        game.mods
            .iter()
            .filter(|entry| !profile.enabled.get(&entry.id).copied().unwrap_or(false))
            .map(|entry| entry.name.clone())
            .collect()
    };

    let manifest_search = find_manifests(Some(&definition), &install.discovered.location);
    let mut manifest = ManifestPlan {
        path: manifest_search
            .searched
            .first()
            .cloned()
            .unwrap_or_else(|| install.discovered.location.join("vortex.deployment.json")),
        found: manifest_search.found,
        files: manifest_search.files,
        matched: 0,
        damaged: manifest_search.damaged.clone(),
        searched: manifest_search.searched.clone(),
    };
    if let Some(reason) = &manifest.damaged {
        warnings.push(format!(
            "a deployment manifest could not be used, so the layer order may come from Vortex's rules alone: {reason}"
        ));
    }
    if !manifest.found {
        let looked: Vec<String> = manifest
            .searched
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        warnings.push(format!(
            "no deployment manifest was found (looked in {}); the layer order comes from Vortex's rules alone. A purged Vortex deployment is the usual reason: deploy once in Vortex before importing if you want every file's supplier known",
            looked.join(", ")
        ));
    }

    let folded_sets: HashMap<String, BTreeSet<String>> = mods
        .iter()
        .map(|m| (m.mod_id.clone(), m.file_set.clone()))
        .collect();
    let (manifest_winners, owners, unmatched) =
        match_manifest(&folder_ids_of(game), &manifest_search.entries, &folded_sets);
    manifest.matched = manifest_winners.len();
    if unmatched > 0 {
        warnings.push(format!(
            "{unmatched} file(s) in the deployment manifest are not in any imported mod (base-game files or a mod type this import does not read); they are not part of the order"
        ));
    }

    let (order, rule_rank, mut cycle) = order_mods(&mods, &manifest_winners, &owners);
    if !cycle.is_empty() {
        cycle.sort();
        warnings.push(format!(
            "the deployment manifest names contradictory winners for some files, so Agora placed {} mod(s) by rule: {}",
            cycle.len(),
            cycle.join(", ")
        ));
    }
    let disagreements = disagreements(&mods, &manifest_winners, &owners, &rule_rank, &folded_sets);

    let mount = data_mount(&definition);
    let base_data = if mount.is_empty() {
        install.discovered.location.clone()
    } else {
        install.discovered.location.join(&mount)
    };
    let plugins = plan_plugins(
        &definition,
        &state,
        game,
        profile,
        &mods,
        &base_data,
        &mut warnings,
    );
    let profile_dir = profile_folder(&state.source, &game.id, &profile.id);
    let userlist = plan_userlist(&profile_dir, &plugins, &mut warnings);
    let inis = plan_inis(
        &definition,
        &install.discovered.store,
        &profile_dir,
        profile.local_settings,
        &mut warnings,
    );
    let saves = plan_saves(
        &definition,
        &install.discovered.store,
        &profile.id,
        profile.local_saves,
        &mut warnings,
    )?;
    if profile.local_saves && !saves.local && !saves.note.is_empty() {
        warnings.push(saves.note.clone());
    }

    if request.name.is_none() {
        if let Some(instance) = find_import_of_kind(ctx, "vortex", &request.store, &profile.name)? {
            return Err(ImportError::AlreadyImported { instance });
        }
    }

    let files = mods.iter().map(|m| m.files).sum();
    let bytes: u64 = mods.iter().map(|m| m.bytes).sum();
    // Validate the space before anything (including adding a game folder) is written.
    content_store::ensure_room_for_declared(&ctx.paths.content_root(), bytes)?;
    Ok(VortexPlan {
        store: state.source.clone(),
        vortex_game: game.id.clone(),
        game: definition.id.clone(),
        game_name: definition.name.clone(),
        profile: profile.name.clone(),
        profile_id: profile.id.clone(),
        instance_name: request
            .name
            .clone()
            .unwrap_or_else(|| format!("{} (Vortex import)", profile.name)),
        install_id: install.install_id.as_str().to_string(),
        install_location: install.discovered.location.clone(),
        store_id: install.discovered.store.clone(),
        would_add_folder: would_add,
        staging,
        mods_enabled: enabled,
        mods_disabled: mods.len() - enabled,
        mods_left_out,
        mods,
        files,
        bytes,
        manifest,
        order,
        disagreements,
        plugins,
        userlist,
        local_settings: profile.local_settings,
        inis,
        local_saves: profile.local_saves,
        saves,
        problems,
        warnings,
    })
}

fn match_for_request<'a>(
    ctx: &'a Ctx,
    inventory: &GameInventory,
    state: &'a VortexState,
    request: &VortexRequest,
) -> Result<(&'a VortexGame, GameDefinition, IdentifiedInstall, bool), ImportError> {
    let (game, definition) = resolve_game(ctx, state, request.game.as_str())
        .ok_or_else(|| ImportError::NoVortexGame(request.game.as_str().to_string()))?;

    let mut would_add = false;
    let install = match request.install.as_deref() {
        Some(wanted) => {
            let found = inventory
                .installs
                .iter()
                .find(|i| i.install_id.as_str() == wanted)
                .ok_or_else(|| ImportError::NoSuchInstall(wanted.to_string()))?;
            if found.game != definition.id {
                let install_game = ctx
                    .games
                    .game(&found.game)
                    .map(|g| g.name.clone())
                    .unwrap_or_else(|| found.game.as_str().to_string());
                return Err(ImportError::InstallOfAnotherGame {
                    install: wanted.to_string(),
                    install_game,
                    game: definition.name.clone(),
                });
            }
            found.clone()
        }
        None => {
            let path =
                game.discovered_path
                    .as_ref()
                    .ok_or_else(|| ImportError::VortexFolderMissing {
                        game: definition.name.clone(),
                    })?;
            match folder_state(ctx, inventory, definition, path) {
                FolderState::Installed(install) => install.clone(),
                FolderState::NeedsAdd { why } => {
                    if !request.add_folder {
                        return Err(ImportError::VortexFolderNotAdded { reason: why });
                    }
                    would_add = true;
                    speculative_install(ctx, definition, path).ok_or_else(|| {
                        ImportError::VortexFolderNotGame {
                            game: definition.name.clone(),
                            path: path.clone(),
                            reason: format!(
                                "{} looks like a {} install but its version could not be read",
                                path.display(),
                                definition.name
                            ),
                        }
                    })?
                }
                FolderState::NotThisGame { why } => {
                    return Err(ImportError::VortexFolderNotGame {
                        game: definition.name.clone(),
                        path: path.clone(),
                        reason: why,
                    })
                }
            }
        }
    };
    if let RuntimeResolution::Unidentified { reasons } = &install.runtime {
        return Err(ImportError::Unidentified {
            path: install.discovered.location.clone(),
            reasons: reasons.join("; "),
        });
    }
    Ok((game, definition.clone(), install, would_add))
}

fn plan_mods(
    game: &VortexGame,
    profile: &VortexProfile,
    staging: &Path,
    all_mods: bool,
    problems: &mut Vec<Problem>,
) -> Vec<PlannedMod> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in &game.mods {
        let enabled = profile.enabled.get(&entry.id).copied().unwrap_or(false);
        if !enabled && !all_mods {
            continue;
        }
        if let Err(reason) = mo2::check_mod_folder_name(&entry.installation_folder) {
            problems.push(Problem {
                line: None,
                entry: entry.name.clone(),
                reason: format!("its installation folder is not a folder name: {reason}; not read"),
            });
            continue;
        }
        if !seen.insert(fold_path_case(&entry.installation_folder)) {
            problems.push(Problem {
                line: None,
                entry: entry.name.clone(),
                reason: "two mods name the same installation folder; this one is skipped".into(),
            });
            continue;
        }
        let path = staging.join(&entry.installation_folder);
        match std::fs::symlink_metadata(&path) {
            Err(_) => {
                problems.push(Problem {
                    line: None,
                    entry: entry.name.clone(),
                    reason: "its folder is missing from the staging folder; skipped".into(),
                });
                continue;
            }
            Ok(meta) => {
                if !meta.is_dir() || content_store::is_reparse_point_or_symlink(&meta) {
                    problems.push(Problem {
                        line: None,
                        entry: entry.name.clone(),
                        reason: "not a plain folder (a file, a link or a junction); skipped".into(),
                    });
                    continue;
                }
            }
        }
        let paths = match content_store::collect_folder_files(&path) {
            Ok(files) => files,
            Err(error) => {
                problems.push(Problem {
                    line: None,
                    entry: entry.name.clone(),
                    reason: format!("cannot be read: {error}; skipped"),
                });
                continue;
            }
        };
        if paths.is_empty() {
            problems.push(Problem {
                line: None,
                entry: entry.name.clone(),
                reason: "the folder holds no files; skipped".into(),
            });
            continue;
        }
        let sized = match size_mod_files(&paths) {
            Ok(sized) => sized,
            Err((file, error)) => {
                problems.push(Problem {
                    line: None,
                    entry: entry.name.clone(),
                    reason: format!(
                        "the size of {file} cannot be read ({error}); the space it needs is unknown, so the mod was not read"
                    ),
                });
                continue;
            }
        };
        out.push(PlannedMod {
            mod_id: entry.id.clone(),
            name: entry.name.clone(),
            version: entry.version.clone(),
            nexus_mod_id: entry.nexus_mod_id,
            nexus_file_id: entry.nexus_file_id,
            folder: entry.installation_folder.clone(),
            enabled,
            files: paths.len(),
            bytes: sized.bytes,
            provenance: provenance(entry),
            plugin_files: sized.plugin_files,
            rules: entry.rules.clone(),
            paths,
            file_set: sized.file_set,
        });
    }
    out
}

/// What reading a mod folder's files found.
#[derive(Debug)]
struct SizedModFiles {
    bytes: u64,
    plugin_files: Vec<String>,
    file_set: BTreeSet<String>,
}

/// A mod folder's total size, its plugin files at the top level, and every file it holds. A file
/// whose size cannot be read is an error: counting it as 0 would understate what the import needs.
fn size_mod_files(paths: &[(RelPath, PathBuf)]) -> Result<SizedModFiles, (String, std::io::Error)> {
    let mut bytes = 0u64;
    let mut plugin_files = Vec::new();
    let mut file_set = BTreeSet::new();
    for (rel, abs) in paths {
        let meta = std::fs::metadata(abs).map_err(|error| (rel.as_str().to_string(), error))?;
        bytes += meta.len();
        file_set.insert(fold_path_case(rel.as_str()));
        let text = rel.as_str();
        if !text.contains('/') && mo2::is_plugin_file(text) {
            plugin_files.push(text.to_string());
        }
    }
    Ok(SizedModFiles {
        bytes,
        plugin_files,
        file_set,
    })
}

fn provenance(entry: &VortexMod) -> VortexProvenance {
    if entry.nexus_mod_id.is_none()
        && entry.nexus_file_id.is_none()
        && entry.version.is_none()
        && entry.state.is_none()
    {
        return VortexProvenance::Absent;
    }
    VortexProvenance::Claimed {
        modid: entry.nexus_mod_id,
        fileid: entry.nexus_file_id,
        version: entry.version.clone(),
        name: Some(entry.name.clone()),
        note: "source claimed by Vortex, not verified".into(),
    }
}

#[derive(Debug, Default)]
struct ManifestRead {
    entries: Vec<ManifestEntry>,
    files: usize,
    damaged: Option<String>,
}

#[derive(Debug, Clone)]
struct ManifestEntry {
    rel_path: String,
    source: Option<String>,
    /// The mod's subfolder under the deploy target, when mods are not merged
    /// (`IDeploymentMethod.ts:59-64`).
    target: Option<String>,
}

impl ManifestEntry {
    /// The paths this entry could name inside a mod: the deployed path (`target/relPath`) and the
    /// plain `relPath` (some mods already carry the target folder).
    fn candidates(&self) -> Vec<String> {
        let rel = self.rel_path.trim_matches('/').to_string();
        let with_target = join_target(self.target.as_deref(), &rel);
        let mut out = vec![fold_path_case(&with_target)];
        let plain = fold_path_case(&rel);
        if !out.contains(&plain) {
            out.push(plain);
        }
        out
    }
}

/// Read `vortex.deployment.json`. A missing file is not an error (`found: false`); one that cannot
/// be read is recorded as damaged, so the order is never presented as checked.
fn read_manifest(path: &Path) -> ManifestRead {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ManifestRead::default()
        }
        Err(error) => {
            return ManifestRead {
                damaged: Some(error.to_string()),
                ..ManifestRead::default()
            }
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return ManifestRead {
                damaged: Some(format!("it is not JSON: {error}")),
                ..ManifestRead::default()
            }
        }
    };
    let Some(files) = value.get("files").and_then(Value::as_array) else {
        return ManifestRead {
            damaged: Some("it has no 'files' list".into()),
            ..ManifestRead::default()
        };
    };
    let mut entries = Vec::new();
    for file in files {
        let Some(file) = file.as_object() else {
            continue;
        };
        let Some(rel_path) = file.get("relPath").and_then(Value::as_str) else {
            continue;
        };
        entries.push(ManifestEntry {
            rel_path: rel_path.replace('\\', "/"),
            source: file
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string),
            target: file
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    ManifestRead {
        files: entries.len(),
        entries,
        damaged: None,
    }
}

/// A mod's installation folder and id, folded, both naming the mod. The manifest's `source` may
/// spell either one.
fn folder_ids(mods: &[PlannedMod]) -> HashMap<String, String> {
    let mut by_folder = HashMap::new();
    for entry in mods {
        by_folder.insert(fold_path_case(&entry.folder), entry.mod_id.clone());
        by_folder.insert(fold_path_case(&entry.mod_id), entry.mod_id.clone());
    }
    by_folder
}

fn folder_ids_of(game: &VortexGame) -> HashMap<String, String> {
    let mut by_folder = HashMap::new();
    for entry in &game.mods {
        by_folder.insert(fold_path_case(&entry.installation_folder), entry.id.clone());
        by_folder.insert(fold_path_case(&entry.id), entry.id.clone());
    }
    by_folder
}

/// What the manifest says supplied each file, and which imported mods hold that file. Keys are
/// folded. A manifest entry whose `source` names no imported mod is not counted.
fn match_manifest(
    by_folder: &HashMap<String, String>,
    entries: &[ManifestEntry],
    mod_sets: &HashMap<String, BTreeSet<String>>,
) -> (
    BTreeMap<String, String>,
    BTreeMap<String, Vec<String>>,
    usize,
) {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (mod_id, files) in mod_sets {
        for file in files {
            owners.entry(file.clone()).or_default().push(mod_id.clone());
        }
    }
    let mut winners = BTreeMap::new();
    let mut unmatched = 0usize;
    for entry in entries {
        let Some(source) = entry.source.as_deref() else {
            unmatched += 1;
            continue;
        };
        let Some(winner) = by_folder.get(&fold_path_case(source)) else {
            unmatched += 1;
            continue;
        };
        let Some(set) = mod_sets.get(winner) else {
            unmatched += 1;
            continue;
        };
        let Some(rel) = entry.candidates().into_iter().find(|rel| set.contains(rel)) else {
            unmatched += 1;
            continue;
        };
        // A file has one supplier, and the manifest is the deployed state.
        winners.insert(rel, winner.clone());
    }
    (winners, owners, unmatched)
}

/// Order the mods, highest layer first. Manifest constraints decide; Vortex's `before` / `after`
/// rules order mods the manifest does not order. Returns the order, each mod's rank in the
/// rules-only order (for the disagreement report), and any mods a manifest cycle left unordered.
fn order_mods(
    mods: &[PlannedMod],
    manifest_winners: &BTreeMap<String, String>,
    owners: &BTreeMap<String, Vec<String>>,
) -> (Vec<String>, HashMap<String, usize>, Vec<String>) {
    let ids: Vec<String> = mods.iter().map(|m| m.mod_id.clone()).collect();
    let by_nexus: HashMap<u64, String> = mods
        .iter()
        .filter_map(|m| m.nexus_mod_id.map(|id| (id, m.mod_id.clone())))
        .collect();
    let known: BTreeSet<String> = ids.iter().cloned().collect();

    let rule_edges = rule_edges(mods, &by_nexus, &known);
    let rules_order = topo(&ids, &rule_edges, &HashMap::new());
    let rule_rank: HashMap<String, usize> = rules_order
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect();

    let mut manifest_edges: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (rel, winner) in manifest_winners {
        let Some(list) = owners.get(rel) else {
            continue;
        };
        for other in list {
            if other != winner {
                manifest_edges
                    .entry(winner.clone())
                    .or_default()
                    .insert(other.clone());
            }
        }
    }
    let mut order = topo(&ids, &manifest_edges, &rule_rank);
    let mut cycle: Vec<String> = ids
        .iter()
        .filter(|id| !order.iter().any(|placed| placed == *id))
        .cloned()
        .collect();
    if !cycle.is_empty() {
        // A manifest cycle is contradictory; place the rest by rule rather than dropping them.
        let mut rest: Vec<String> = cycle.clone();
        rest.sort_by_key(|id| rule_rank.get(id).copied().unwrap_or(usize::MAX));
        order.extend(rest);
        cycle = ids
            .iter()
            .filter(|id| !order.iter().any(|placed| placed == *id))
            .cloned()
            .collect();
    }
    (order, rule_rank, cycle)
}

fn rule_edges(
    mods: &[PlannedMod],
    by_nexus: &HashMap<u64, String>,
    known: &BTreeSet<String>,
) -> HashMap<String, BTreeSet<String>> {
    let mut edges: HashMap<String, BTreeSet<String>> = HashMap::new();
    for entry in mods {
        for rule in &entry.rules {
            if !rule.is_order() {
                continue;
            }
            let target = rule
                .target_mod_id
                .as_ref()
                .filter(|id| known.contains(*id))
                .cloned()
                .or_else(|| {
                    rule.target_nexus_mod_id
                        .and_then(|id| by_nexus.get(&id).cloned())
                });
            let Some(target) = target else {
                continue;
            };
            let (before, after) = if rule.kind == "after" {
                (&target, &entry.mod_id)
            } else {
                (&entry.mod_id, &target)
            };
            if before != after {
                edges
                    .entry(before.clone())
                    .or_default()
                    .insert(after.clone());
            }
        }
    }
    edges
}

/// A topological order: every edge's source comes before its target. Ties go to the lowest `rank`
/// (then name), so a caller's order is kept wherever the constraints allow it.
fn topo(
    ids: &[String],
    edges: &HashMap<String, BTreeSet<String>>,
    rank: &HashMap<String, usize>,
) -> Vec<String> {
    let mut indegree: HashMap<String, usize> = ids.iter().map(|id| (id.clone(), 0)).collect();
    for targets in edges.values() {
        for target in targets {
            if let Some(count) = indegree.get_mut(target) {
                *count += 1;
            }
        }
    }
    let mut ready: Vec<String> = ids
        .iter()
        .filter(|id| indegree.get(*id).copied().unwrap_or(1) == 0)
        .cloned()
        .collect();
    let mut out = Vec::with_capacity(ids.len());
    while !ready.is_empty() {
        ready.sort_by_key(|id| {
            (
                rank.get(id).copied().unwrap_or(usize::MAX),
                fold_path_case(id),
            )
        });
        let next = ready.remove(0);
        if let Some(targets) = edges.get(&next) {
            for target in targets {
                if let Some(count) = indegree.get_mut(target) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.push(target.clone());
                    }
                }
            }
        }
        out.push(next);
    }
    out
}

fn disagreements(
    mods: &[PlannedMod],
    manifest_winners: &BTreeMap<String, String>,
    owners: &BTreeMap<String, Vec<String>>,
    rule_rank: &HashMap<String, usize>,
    mod_sets: &HashMap<String, BTreeSet<String>>,
) -> Vec<OrderDisagreement> {
    let names: HashMap<&str, &str> = mods
        .iter()
        .map(|m| (m.mod_id.as_str(), m.name.as_str()))
        .collect();
    let mut out = Vec::new();
    for (rel, winner) in manifest_winners {
        let Some(list) = owners.get(rel) else {
            continue;
        };
        if list.len() < 2 {
            continue;
        }
        let Some(rules_winner) = list.iter().min_by_key(|id| {
            (
                rule_rank.get(*id).copied().unwrap_or(usize::MAX),
                fold_path_case(id),
            )
        }) else {
            continue;
        };
        if rules_winner == winner {
            continue;
        }
        // Show the file as the mods spell it (one of them holds it).
        let display = mods
            .iter()
            .find(|m| mod_sets.get(&m.mod_id).is_some_and(|set| set.contains(rel)))
            .and_then(|m| {
                m.paths
                    .iter()
                    .find(|(path, _)| fold_path_case(path.as_str()) == *rel)
                    .map(|(path, _)| path.as_str().to_string())
            })
            .unwrap_or_else(|| rel.clone());
        out.push(OrderDisagreement {
            file: display,
            manifest_supplied: names
                .get(winner.as_str())
                .copied()
                .unwrap_or(winner)
                .to_string(),
            rules_supply: names
                .get(rules_winner.as_str())
                .copied()
                .unwrap_or(rules_winner)
                .to_string(),
        });
    }
    out
}

/// Parse a profile's `plugins.txt`. Vortex writes two shapes
/// (`..._util_PluginPersistor.ts`):
/// - the original shape lists **only active plugins**, with no prefix:
///   `toPluginListOriginal` filters to active/native and returns bare names (`:459-471`), and
///   `writeFiles` writes that list (`:600-607`);
/// - the Fallout 4 shape marks an active plugin with `*` and leaves a ghost bare (`:473-483`).
///
/// The original reader treats every entry as active (`initFromKeyList`: `keyEnabled = enable &&
/// (this.mPluginFormat === "original" || key[0] === "*")`, `:680`). A file with no `*` line is
/// therefore the original shape, where every line is active; a file with any `*` line is read the
/// MO2 way, where a bare line is inactive. The order always comes from `loadorder.txt`
/// (`writeFiles` at `:589-599`, read first for the original format at `:751-762`).
fn parse_vortex_plugins(text: &str) -> Vec<mo2::PluginLine> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if lines.iter().any(|line| line.starts_with('*')) {
        return mo2::parse_plugins(text);
    }
    lines
        .into_iter()
        .map(|line| mo2::PluginLine {
            name: line.to_string(),
            active: true,
        })
        .collect()
}

fn plan_plugins(
    def: &GameDefinition,
    state: &VortexState,
    game: &VortexGame,
    profile: &VortexProfile,
    mods: &[PlannedMod],
    base_data: &Path,
    warnings: &mut Vec<String>,
) -> PluginPlan {
    let Some(rule) = def.load_orders.first() else {
        return PluginPlan {
            kept: false,
            lines: Vec::new(),
            active: 0,
            locked: Vec::new(),
            implicit_skipped: Vec::new(),
            notes: vec!["the game keeps no plugin list".into()],
        };
    };
    let (plugins_path, loadorder_path) = plugin_files(&state.source, &game.id, &profile.id);
    let mut notes = Vec::new();
    let loadorder = match std::fs::read(&loadorder_path) {
        Ok(bytes) => mo2::parse_loadorder(&String::from_utf8_lossy(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            notes.push(format!(
                "the profile has no loadorder.txt (looked in {}); the order comes from plugins.txt",
                loadorder_path.display()
            ));
            Vec::new()
        }
        Err(error) => {
            warnings.push(format!(
                "the profile's loadorder.txt could not be read ({}): {error}",
                loadorder_path.display()
            ));
            Vec::new()
        }
    };
    let listed = match std::fs::read(&plugins_path) {
        Ok(bytes) => parse_vortex_plugins(&String::from_utf8_lossy(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warnings.push(format!(
                "the profile has no plugins.txt (looked in {}); no plugin is active",
                plugins_path.display()
            ));
            Vec::new()
        }
        Err(error) => {
            warnings.push(format!(
                "the profile's plugins.txt could not be read ({}): {error}",
                plugins_path.display()
            ));
            Vec::new()
        }
    };
    let active_names: BTreeSet<String> = listed
        .iter()
        .filter(|p| p.active)
        .map(|p| fold_path_case(&p.name))
        .collect();
    let implicit: BTreeSet<String> = rule
        .rules
        .implicit
        .iter()
        .map(|name| fold_path_case(name))
        .collect();

    let managed: Vec<String> = {
        let mut seen = BTreeSet::new();
        mods.iter()
            .filter(|m| m.enabled)
            .flat_map(|m| m.plugin_files.iter().cloned())
            .filter(|name| !base_data.join(name).is_file())
            .filter(|name| seen.insert(fold_path_case(name)))
            .collect()
    };
    let managed_keys: BTreeSet<String> = managed.iter().map(|n| fold_path_case(n)).collect();

    let mut lines = Vec::new();
    let mut implicit_skipped = Vec::new();
    let mut have = BTreeSet::new();
    let names = loadorder
        .iter()
        .cloned()
        .chain(listed.iter().map(|p| p.name.clone()));
    for name in names {
        let key = fold_path_case(&name);
        if implicit.contains(&key) {
            if !implicit_skipped
                .iter()
                .any(|n: &String| fold_path_case(n) == key)
            {
                implicit_skipped.push(name);
            }
            continue;
        }
        if !have.insert(key.clone()) {
            continue;
        }
        lines.push(PluginLinePlan {
            name,
            active: active_names.contains(&key),
            managed: managed_keys.contains(&key),
        });
    }
    for name in &managed {
        if have.insert(fold_path_case(name)) {
            notes.push(format!(
                "{name} is in an enabled mod but not in the profile's plugin lists; it is kept inactive"
            ));
            lines.push(PluginLinePlan {
                name: name.clone(),
                active: false,
                managed: true,
            });
        }
    }
    let active = lines.iter().filter(|line| line.active).count();
    PluginPlan {
        kept: true,
        lines,
        active,
        locked: Vec::new(),
        implicit_skipped,
        notes,
    }
}

// ---------------------------------------------------------------------------
// LOOT userlist
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct UserlistEntry {
    name: String,
    after: Vec<String>,
    req: Vec<String>,
    inc: Vec<String>,
}

/// Read the profile's `userlist.yaml` for its `after` entries. This is the small YAML LOOT writes
/// for a profile: a `plugins:` list of `- name:` entries with `after`/`req`/`inc` lists. Other
/// keys (`groups`, `globals`) are ignored. A quoted or bare value is accepted; an inline
/// `[a, b]` list is accepted too.
fn parse_userlist(text: &str) -> Vec<UserlistEntry> {
    let mut entries = Vec::new();
    let mut current: Option<UserlistEntry> = None;
    let mut key: Option<u8> = None; // 0 after, 1 req, 2 inc
    let mut in_plugins = false;
    for raw in text.lines() {
        let line = strip_yaml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line == "plugins:" {
            in_plugins = true;
            key = None;
            continue;
        }
        if !in_plugins {
            continue;
        }
        if line == "groups:" || line == "globals:" {
            in_plugins = false;
            key = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            let rest = rest.trim();
            if let Some(name) = rest.strip_prefix("name:") {
                if let Some(entry) = current.take() {
                    if !entry.name.is_empty() {
                        entries.push(entry);
                    }
                }
                current = Some(UserlistEntry {
                    name: unquote(name.trim()),
                    ..UserlistEntry::default()
                });
                key = None;
                continue;
            }
            if let Some(entry) = current.as_mut() {
                let value = unquote(rest);
                if value.is_empty() {
                    continue;
                }
                match key {
                    Some(0) => entry.after.push(value),
                    Some(1) => entry.req.push(value),
                    Some(2) => entry.inc.push(value),
                    _ => {}
                }
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name == "name" {
                if let Some(entry) = current.as_mut() {
                    entry.name = unquote(value);
                }
            } else if matches!(name, "after" | "req" | "inc") {
                let slot = match name {
                    "after" => 0,
                    "req" => 1,
                    _ => 2,
                };
                key = Some(slot);
                if value.starts_with('[') && value.ends_with(']') {
                    if let Some(entry) = current.as_mut() {
                        for item in inline_list(value) {
                            match slot {
                                0 => entry.after.push(item),
                                1 => entry.req.push(item),
                                _ => entry.inc.push(item),
                            }
                        }
                    }
                }
            } else {
                key = None;
            }
        }
    }
    if let Some(entry) = current {
        if !entry.name.is_empty() {
            entries.push(entry);
        }
    }
    entries
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (index, ch) in line.char_indices() {
        match ch {
            '\'' | '"' if quote == Some(ch) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            '#' if quote.is_none()
                && (index == 0
                    || line[..index]
                        .chars()
                        .last()
                        .is_some_and(char::is_whitespace)) =>
            {
                return &line[..index];
            }
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let first = value.chars().next().unwrap_or(' ');
        let last = value.chars().last().unwrap_or(' ');
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn inline_list(value: &str) -> Vec<String> {
    value
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|item| !item.is_empty())
        .collect()
}

fn reaches(graph: &HashMap<String, Vec<String>>, from: &str, to: &str) -> bool {
    let mut stack = vec![from.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Some(next) = graph.get(&node) {
            stack.extend(next.iter().cloned());
        }
    }
    false
}

/// Turn the profile's LOOT `after` entries into load-after rules Agora can store. A rule whose
/// plugin is not in the profile's list, a self rule and a rule that closes a cycle among the
/// accepted rules are refused with a reason. A master contradiction is checked by
/// [`crate::game_load_order::add_load_after_rule`] when the instance exists. `req` and `inc` have
/// no Agora equivalent and are named in `not_imported`.
fn plan_userlist(
    profile_dir: &Path,
    plugins: &PluginPlan,
    warnings: &mut Vec<String>,
) -> UserlistPlan {
    let path = profile_dir.join("userlist.yaml");
    if !path.is_file() {
        return UserlistPlan::default();
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            warnings.push(format!(
                "the profile's userlist.yaml could not be read ({}): {error}",
                path.display()
            ));
            return UserlistPlan {
                path: Some(path),
                ..UserlistPlan::default()
            };
        }
    };
    let known: BTreeSet<String> = plugins
        .lines
        .iter()
        .map(|line| fold_path_case(&line.name))
        .collect();
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut accepted: BTreeSet<(String, String)> = BTreeSet::new();
    let mut rules = Vec::new();
    let mut refused = Vec::new();
    let mut not_imported = Vec::new();
    for entry in parse_userlist(&text) {
        let item = entry.name.trim().to_string();
        if item.is_empty() {
            continue;
        }
        for after in entry.after {
            let after = after.trim().to_string();
            let item_key = fold_path_case(&item);
            let after_key = fold_path_case(&after);
            let reason = if !known.contains(&item_key) || !known.contains(&after_key) {
                Some(
                    "not in the profile's plugin list (it may come from a mod left out)"
                        .to_string(),
                )
            } else if item_key == after_key {
                Some("a plugin cannot load after itself".to_string())
            } else if accepted.contains(&(item_key.clone(), after_key.clone())) {
                None
            } else if reaches(&graph, &item_key, &after_key) {
                Some("adding it would make a cycle with the rules already accepted".to_string())
            } else {
                None
            };
            match reason {
                Some(reason) => refused.push(RefusedRule {
                    item: item.clone(),
                    after,
                    reason,
                }),
                None => {
                    if accepted.insert((item_key.clone(), after_key.clone())) {
                        graph.entry(after_key).or_default().push(item_key);
                        rules.push(PlannedRule {
                            item: item.clone(),
                            after,
                        });
                    }
                }
            }
        }
        for target in entry.req {
            not_imported.push(format!(
                "{item}: req {target} (Agora has no requirement rule; no equivalent)"
            ));
        }
        for target in entry.inc {
            not_imported.push(format!("{item}: inc {target} (Agora has no conflict rule)"));
        }
    }
    UserlistPlan {
        path: Some(path),
        rules,
        refused,
        not_imported,
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// What a run did, after its plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VortexRunReport {
    pub plan: VortexPlan,
    /// The run stopped before the instance was made (`stop_after_mods`). Run it again to finish.
    pub interrupted: bool,
    pub stored_mods: usize,
    pub instance_id: Option<String>,
    pub base_id: Option<String>,
    pub base_built: Option<bool>,
    pub content_layers: usize,
    pub disabled_layers: usize,
    pub duplicates: Vec<String>,
    pub plugins_written: Option<usize>,
    pub plugin_findings: Vec<String>,
    /// LOOT userlist `after` rules stored on the instance.
    pub rules_added: usize,
    /// Userlist rules refused at import time (masters, cycles), with the reason.
    pub rules_refused: Vec<String>,
    /// The profile's own INI files copied into the instance.
    pub inis_copied: Vec<String>,
    pub saves_copied: usize,
    pub saves_skipped_existing: usize,
    pub saves_folder: Option<PathBuf>,
    pub next_steps: Vec<String>,
}

/// Run an import: store the bytes, make the instance, and set it up from the profile. The plan is
/// made first, so a refusal happens before anything is written.
pub fn run(
    ctx: &Ctx,
    inventory: &GameInventory,
    request: &VortexRequest,
    options: RunOptions,
    progress: &dyn Fn(ImportProgress),
) -> Result<VortexRunReport, ImportError> {
    // Plan and validate before writing anything, including a game folder. Only then is a folder
    // with --add-folder added, and the plan is made again against the real install.
    let mut plan = plan(ctx, inventory, request)?;
    let inventory = if plan.would_add_folder {
        let added = add_folder_for_request(ctx, inventory, request)?;
        plan = self::plan(ctx, &added, request)?;
        added
    } else {
        inventory.clone()
    };
    let install = inventory
        .installs
        .iter()
        .find(|i| i.install_id.as_str() == plan.install_id)
        .ok_or_else(|| {
            ImportError::Other("the install vanished between planning and running".into())
        })?
        .clone();
    let def = ctx
        .games
        .game(&plan.game)
        .cloned()
        .ok_or_else(|| ImportError::Other("the game definition is missing".into()))?;
    let mut report = VortexRunReport {
        plan: plan.clone(),
        interrupted: false,
        stored_mods: 0,
        instance_id: None,
        base_id: None,
        base_built: None,
        content_layers: 0,
        disabled_layers: 0,
        duplicates: Vec::new(),
        plugins_written: None,
        plugin_findings: Vec::new(),
        rules_added: 0,
        rules_refused: Vec::new(),
        inis_copied: Vec::new(),
        saves_copied: 0,
        saves_skipped_existing: 0,
        saves_folder: None,
        next_steps: Vec::new(),
    };

    let mods_total = plan.mods.len();
    let mods_bytes: u64 = plan.mods.iter().map(|m| m.bytes).sum();
    let mut stored: Vec<(usize, String)> = Vec::new();
    let mut bytes_done = 0u64;
    for (index, planned) in plan.mods.iter().enumerate() {
        progress(ImportProgress {
            phase: ImportPhase::Storing,
            current_mod: Some(planned.name.clone()),
            mods_done: index,
            mods_total,
            bytes_done,
            bytes_total: mods_bytes,
        });
        let source = ContentSource::VortexImport {
            store: plan.store.to_string_lossy().to_string(),
            game: plan.vortex_game.clone(),
            mod_id: planned.mod_id.clone(),
            installation_folder: planned.folder.clone(),
            provenance: planned.provenance.clone(),
            added_at_unix_ms: content_store::now_unix_ms(),
        };
        let store_error = |error: crate::content_store::ContentError| ImportError::Store {
            mod_folder: planned.name.clone(),
            reason: error.to_string(),
        };
        let outcome =
            content_store::add_collected(ctx, &planned.name, planned.paths.clone(), source)
                .map_err(store_error)?;
        stored.push((index, outcome.item().item_id.clone()));
        bytes_done += planned.bytes;
        report.stored_mods += 1;
        progress(ImportProgress {
            phase: ImportPhase::Storing,
            current_mod: None,
            mods_done: index + 1,
            mods_total,
            bytes_done,
            bytes_total: mods_bytes,
        });
        if options
            .stop_after_mods
            .is_some_and(|n| report.stored_mods >= n)
            && index + 1 < mods_total
        {
            report.interrupted = true;
            return Ok(report);
        }
    }

    let done_progress = |phase: ImportPhase| ImportProgress {
        phase,
        current_mod: None,
        mods_done: mods_total,
        mods_total,
        bytes_done,
        bytes_total: mods_bytes,
    };
    progress(done_progress(ImportPhase::Instance));
    let outcome = game_instance::create_with_options(
        ctx,
        &install,
        &def,
        &plan.instance_name,
        None,
        BaseMode::Linked,
        BuildOptions::default(),
        &|_| {},
    )?;
    let instance_id = outcome.instance_id.clone();
    report.instance_id = Some(instance_id.clone());
    report.base_id = match &outcome.base {
        BaseReference::Pinned { id, .. } => Some(id.clone()),
        BaseReference::Unpinned { .. } => None,
    };
    report.base_built = outcome
        .build_outcome
        .as_ref()
        .map(|b| matches!(b, BuildOutcome::Built { .. }));

    let populated: Result<(), ImportError> = (|| {
        let mut manifest: GameInstanceManifest = game_instance::get_manifest(ctx, &instance_id)?;
        manifest.imported_from = Some(super::ImportRecord {
            kind: "vortex".into(),
            setup: plan.store.to_string_lossy().to_string(),
            profile: plan.profile.clone(),
            imported_at_unix_ms: content_store::now_unix_ms(),
        });
        game_deploy::write_instance_manifest_atomic(ctx, &instance_id, &manifest)?;

        progress(done_progress(ImportPhase::Layers));
        let mount = data_mount(&def);
        let mount_arg = if mount.is_empty() {
            None
        } else {
            Some(mount.as_str())
        };
        // `order` is highest first, so the last one is the lowest layer.
        for mod_id in plan.order.iter().rev() {
            let Some(planned) = plan.mods.iter().find(|m| &m.mod_id == mod_id) else {
                continue;
            };
            let Some((_, item_id)) = stored
                .iter()
                .find(|(index, _)| plan.mods.get(*index).is_some_and(|m| &m.mod_id == mod_id))
            else {
                continue;
            };
            let existing = game_instance::get_manifest(ctx, &instance_id)?
                .layers
                .layers()
                .iter()
                .any(|l| matches!(&l.source, agora_game_api::LayerSource::Content { content } if content == item_id));
            if existing {
                report.duplicates.push(format!(
                    "{} (same bytes as a layer already here)",
                    planned.name
                ));
                if planned.enabled {
                    game_deploy::set_content_enabled(ctx, &instance_id, item_id, true)?;
                }
                continue;
            }
            game_deploy::add_content(ctx, &instance_id, item_id, mount_arg, None)?;
            report.content_layers += 1;
            if !planned.enabled {
                game_deploy::set_content_enabled(ctx, &instance_id, item_id, false)?;
                report.disabled_layers += 1;
            }
        }

        progress(done_progress(ImportPhase::Finishing));
        if plan.plugins.kept {
            let lines: Vec<(String, bool)> = plan
                .plugins
                .lines
                .iter()
                .map(|line| (line.name.clone(), line.active))
                .collect();
            let managed: Vec<String> = plan
                .plugins
                .lines
                .iter()
                .filter(|line| line.managed)
                .map(|line| line.name.clone())
                .collect();
            let _lock = ctx.lock_manager.acquire(
                LockResource::Instance(instance_id.clone()),
                "vortex-import-plugins",
            )?;
            game_plugins::import_list(ctx, &instance_id, &def, &lines, &managed, &[])?;
            report.plugins_written = Some(lines.len());
            // The profile's LOOT `after` entries become Agora's load-after rules. The rule add
            // checks masters and cycles against the instance as well; a refused rule is reported
            // and the rest still apply.
            for rule in &plan.userlist.rules {
                match game_load_order::add_load_after_rule(
                    ctx,
                    &instance_id,
                    &def,
                    None,
                    &rule.item,
                    &rule.after,
                ) {
                    Ok(true) => report.rules_added += 1,
                    Ok(false) => {}
                    Err(error) => report
                        .rules_refused
                        .push(format!("{} after {}: {error}", rule.item, rule.after)),
                }
            }
            let findings = game_load_order::check(ctx, &instance_id, &def)?;
            if !findings.is_empty() {
                report.plugin_findings = game_load_order::describe_findings(&findings)
                    .split("; ")
                    .map(str::to_string)
                    .collect();
            }
        }

        // The profile's own INIs, when it keeps local game settings.
        for ini in &plan.inis {
            let bytes = std::fs::read(&ini.from)?;
            let dest = game_ini::join_rel(
                &ctx.paths
                    .instance_dir(&instance_id)
                    .map_err(|e| ImportError::Other(e.to_string()))?,
                &ini.instance_path,
            );
            game_ini::write_atomic(&dest, &bytes)?;
            report.inis_copied.push(ini.name.clone());
        }

        // The profile's own saves: the instance uses its own, and --copy-saves copies the files.
        if plan.saves.local {
            if let Some(rule) = crate::game_saves::rule_for(&def, &install.discovered.store) {
                crate::game_saves::set_choice_inner(
                    ctx,
                    &instance_id,
                    &def,
                    SavesChoice::Own,
                    request.copy_saves,
                    None,
                )?;
                let folder = crate::game_saves::own_folder(rule, &instance_id)?;
                report.saves_folder = Some(folder.clone());
                if request.copy_saves {
                    if let Some(from) = &plan.saves.from {
                        let (copied, skipped, notes) = copy_save_folder(from, &folder)?;
                        report.saves_copied = copied;
                        report.saves_skipped_existing = skipped;
                        report.next_steps.extend(notes);
                    }
                } else if plan.saves.files > 0 {
                    report.next_steps.push(format!(
                        "Agora did not copy {} save file(s) (they are under Documents, which needs --copy-saves). To copy them by hand, copy the files in {} into {}",
                        plan.saves.files,
                        plan.saves
                            .from
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        folder.display()
                    ));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = populated {
        return Err(abandon_half_made_instance(ctx, &instance_id, error));
    }

    report.next_steps.extend(
        plan.problems
            .iter()
            .map(|p| format!("{} was not imported: {}", p.entry, p.reason)),
    );
    report.next_steps.push(
        "Vortex's downloads were not copied; mods are stored as they are in the staging folder"
            .into(),
    );
    if !plan.disagreements.is_empty() {
        report.next_steps.push(format!(
            "A different mod supplies {} file(s) than Vortex's rules alone would pick; the deployment manifest was followed, and the plan lists each one",
            plan.disagreements.len()
        ));
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Several profiles
// ---------------------------------------------------------------------------

/// The profiles of one Vortex game, with what each enables, for a grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSets {
    pub game: GameId,
    pub game_name: String,
    pub profiles: Vec<super::profiles::ProfileSet>,
}

/// Read one Vortex game's profiles and the mods each enables. Reads a copy of
/// the store, like every other Vortex read.
pub fn profile_sets(
    ctx: &Ctx,
    inventory: &GameInventory,
    store: &Path,
    game: &str,
) -> Result<ProfileSets, ImportError> {
    let scan = scan(ctx, inventory, store)?;
    let found = scan
        .games
        .iter()
        .find(|g| g.vortex_game == game || g.game.as_ref().is_some_and(|id| id.as_str() == game))
        .ok_or_else(|| ImportError::NoVortexGame(game.to_string()))?;
    if let Some(reason) = &found.reason {
        return Err(ImportError::Other(reason.clone()));
    }
    let game = found
        .game
        .clone()
        .ok_or_else(|| ImportError::NoVortexGame(game.to_string()))?;
    Ok(ProfileSets {
        game,
        game_name: found.game_name.clone().unwrap_or_default(),
        profiles: found
            .profiles
            .iter()
            .map(|profile| super::profiles::ProfileSet {
                name: profile.name.clone(),
                enabled: profile.enabled_mod_ids.iter().cloned().collect(),
            })
            .collect(),
    })
}

/// What importing several profiles would do, read and checked. Nothing is written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MultiProfilePlan {
    pub grouping: super::profiles::Grouping,
    pub profiles: Vec<String>,
    pub game: GameId,
    pub game_name: String,
    /// One plan per profile, in the order asked for.
    pub plans: Vec<VortexPlan>,
    /// The bytes the choice brings in: the union for a merge, the sum for separate instances.
    pub bytes: u64,
    /// The free bytes on the content store's drive, when they can be read.
    pub free_bytes: Option<u64>,
    /// False only when the bytes cannot fit.
    pub fits: bool,
    pub suggestion: super::profiles::Suggestion,
    pub warnings: Vec<String>,
}

/// Plan several profiles at once, and say whether the bytes fit.
pub fn plan_profiles(
    ctx: &Ctx,
    inventory: &GameInventory,
    base: &VortexRequest,
    profiles: &[String],
    grouping: super::profiles::Grouping,
) -> Result<MultiProfilePlan, ImportError> {
    if profiles.is_empty() {
        return Err(ImportError::NoProfiles);
    }
    let sets = profile_sets(ctx, inventory, &base.store, base.game.as_str())?;
    let mut plans = Vec::with_capacity(profiles.len());
    let mut names = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let request = VortexRequest {
            profile: profile.clone(),
            ..base.clone()
        };
        plans.push(plan(ctx, inventory, &request)?);
        names.push(profile.clone());
    }

    let mut union: BTreeMap<String, u64> = BTreeMap::new();
    let mut separate_bytes = 0u64;
    for plan in &plans {
        for item in plan.mods.iter().filter(|m| m.enabled) {
            union.insert(item.mod_id.clone(), item.bytes);
            separate_bytes = separate_bytes.saturating_add(item.bytes);
        }
    }
    let bytes = match grouping {
        super::profiles::Grouping::Merged => union.values().sum(),
        super::profiles::Grouping::Separate => separate_bytes,
    };
    let free_bytes = crate::game_tool_swap::available_bytes(&ctx.paths.content_root());
    let fits = free_bytes.is_none_or(|free| bytes <= free);
    let suggestion = super::profiles::suggest_grouping(&sets.profiles);
    let mut warnings = Vec::new();
    if !fits {
        warnings.push(format!(
            "the choice needs {} but only {} is free on the content store's drive",
            crate::game_tool_swap::human_bytes(bytes),
            crate::game_tool_swap::human_bytes(free_bytes.unwrap_or(0))
        ));
    }
    if suggestion.groups.len() > 1 {
        warnings.push(suggestion.note.clone());
    }
    Ok(MultiProfilePlan {
        grouping,
        profiles: names,
        game: sets.game,
        game_name: sets.game_name,
        plans,
        bytes,
        free_bytes,
        fits,
        suggestion,
        warnings,
    })
}

/// What one profile's saves did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileSavesReport {
    pub profile: String,
    pub copied: usize,
    pub skipped_existing: usize,
    /// What happened when nothing was copied, or something could not be read.
    pub note: Option<String>,
}

/// What importing several profiles did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MultiProfileReport {
    pub plan: MultiProfilePlan,
    /// One instance id per import: one for a merge, one per profile for separate.
    pub instances: Vec<String>,
    /// The loadout profiles a merge created, by name.
    pub loadouts: Vec<String>,
    /// What each profile's own saves did, in profile order.
    pub saves: Vec<ProfileSavesReport>,
}

fn add_mod_to_merged(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    plan: &VortexPlan,
    item: &PlannedMod,
) -> Result<String, ImportError> {
    let source = ContentSource::VortexImport {
        store: plan.store.to_string_lossy().to_string(),
        game: plan.vortex_game.clone(),
        mod_id: item.mod_id.clone(),
        installation_folder: item.folder.clone(),
        provenance: item.provenance.clone(),
        added_at_unix_ms: content_store::now_unix_ms(),
    };
    let outcome = content_store::add_collected(ctx, &item.name, item.paths.clone(), source)
        .map_err(|e| ImportError::Store {
            mod_folder: item.name.clone(),
            reason: e.to_string(),
        })?;
    let item_id = outcome.item().item_id.clone();
    let manifest = game_instance::get_manifest(ctx, instance_id)?;
    let present = manifest.layers.layers().iter().any(|layer| {
        matches!(&layer.source, agora_game_api::LayerSource::Content { content } if content == &item_id)
    });
    if !present {
        let mount = data_mount(definition);
        let mount_arg = if mount.is_empty() {
            None
        } else {
            Some(mount.as_str())
        };
        game_deploy::add_content(ctx, instance_id, &item_id, mount_arg, None).map_err(|e| {
            ImportError::Other(format!(
                "'{}' could not be put on the merged instance: {e}",
                item.name
            ))
        })?;
    }
    Ok(item_id)
}

/// Copy the files directly inside `from` into `folder`, keeping a file already there. An entry
/// or a size that cannot be read is returned as a note, never silently skipped.
fn copy_save_folder(
    from: &Path,
    folder: &Path,
) -> Result<(usize, usize, Vec<String>), ImportError> {
    std::fs::create_dir_all(folder).map_err(|e| {
        ImportError::Other(format!(
            "the instance's save folder {} could not be made: {e}",
            folder.display()
        ))
    })?;
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut notes = Vec::new();
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        // The planner already counts an absent folder as no saves.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0, notes)),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                notes.push(format!(
                    "an entry in {} could not be read, so it was not copied: {error}",
                    from.display()
                ));
                continue;
            }
        };
        let is_file = match entry.metadata() {
            Ok(meta) => meta.is_file(),
            Err(error) => {
                notes.push(format!(
                    "{} could not be read, so it was not copied: {error}",
                    entry.path().display()
                ));
                continue;
            }
        };
        if !is_file {
            continue;
        }
        let dest = folder.join(entry.file_name());
        if dest.exists() {
            skipped += 1;
            continue;
        }
        std::fs::copy(entry.path(), &dest).map_err(|e| {
            ImportError::Other(format!(
                "{} could not be copied to {}: {e}",
                entry.path().display(),
                dest.display()
            ))
        })?;
        copied += 1;
    }
    Ok((copied, skipped, notes))
}

/// Every chosen profile's mods, in the order one import of all of them would put them: the
/// deployment manifest decides, and Vortex's rules break ties.
fn merged_mod_order(
    definition: &GameDefinition,
    plan: &MultiProfilePlan,
) -> Result<Vec<String>, ImportError> {
    // The first profile's mods are already on the instance, including the disabled ones an
    // `--all-mods` run stored; every later profile adds only what it enables.
    let mut union: BTreeMap<String, PlannedMod> = BTreeMap::new();
    for (index, profile_plan) in plan.plans.iter().enumerate() {
        for item in &profile_plan.mods {
            if index > 0 && !item.enabled {
                continue;
            }
            union
                .entry(item.mod_id.clone())
                .or_insert_with(|| item.clone());
        }
    }
    let mods: Vec<PlannedMod> = union.into_values().collect();
    let install = plan
        .plans
        .first()
        .map(|plan| plan.install_location.clone())
        .ok_or_else(|| ImportError::Other("no profile was planned".into()))?;
    let search = find_manifests(Some(definition), &install);
    let folded_sets: HashMap<String, BTreeSet<String>> = mods
        .iter()
        .map(|mod_| (mod_.mod_id.clone(), mod_.file_set.clone()))
        .collect();
    let (winners, owners, _) = match_manifest(&folder_ids(&mods), &search.entries, &folded_sets);
    let (order, _, _) = order_mods(&mods, &winners, &owners);
    Ok(order)
}

/// Put the instance's content layers in `order`'s order (highest first). The layers are added as
/// the union is stored; this gives them Vortex's layer order rather than the order they arrived.
fn order_merged_layers(
    ctx: &Ctx,
    instance_id: &str,
    order: &[String],
    mod_to_item: &HashMap<String, String>,
) -> Result<(), ImportError> {
    let mut item_rank: HashMap<String, usize> = HashMap::new();
    for (index, mod_id) in order.iter().enumerate() {
        if let Some(item) = mod_to_item.get(mod_id) {
            item_rank
                .entry(item.clone())
                .and_modify(|rank| *rank = (*rank).min(index))
                .or_insert(index);
        }
    }
    let mut manifest = game_instance::get_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    let positions: Vec<usize> = layers
        .iter()
        .enumerate()
        .filter(|(_, layer)| matches!(&layer.source, agora_game_api::LayerSource::Content { .. }))
        .map(|(index, _)| index)
        .collect();
    let rank_of = |layer: &agora_game_api::Layer| match &layer.source {
        agora_game_api::LayerSource::Content { content } => {
            item_rank.get(content).copied().unwrap_or(usize::MAX)
        }
        _ => usize::MAX,
    };
    let mut content: Vec<agora_game_api::Layer> = positions
        .iter()
        .map(|index| layers[*index].clone())
        .collect();
    // An order index of 0 is the highest layer; the layer list runs lowest first.
    content.sort_by_key(|layer| std::cmp::Reverse(rank_of(layer)));
    for (position, layer) in positions.into_iter().zip(content) {
        layers[position] = layer;
    }
    manifest.layers =
        agora_game_api::LayerStack::new(layers).map_err(|e| ImportError::Other(e.to_string()))?;
    game_deploy::write_instance_manifest_atomic(ctx, instance_id, &manifest)?;
    Ok(())
}

/// One profile's state on the merged instance. Its own INIs and plugin list are the profile's;
/// a profile that has neither uses the game's shared settings, never the first profile's copies.
fn profile_state_for_merge(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    plan: &VortexPlan,
    layer_mod: &HashMap<String, Vec<String>>,
) -> Result<crate::game_loadout::GameLoadout, ImportError> {
    let mut state =
        crate::game_loadout::capture(ctx, instance_id, definition).map_err(ImportError::Other)?;
    let enabled: BTreeSet<&str> = plan
        .mods
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.mod_id.as_str())
        .collect();
    for item in &mut state.content {
        item.enabled = layer_mod
            .get(item.item.as_str())
            .is_some_and(|mods| mods.iter().any(|mod_id| enabled.contains(mod_id.as_str())));
    }

    // A profile without its own plugins.txt keeps the game's shared list. A loadout entry with no
    // contents means the instance has no copy, so the game reads the shared file.
    let (plugins_path, _) = plugin_files(&plan.store, &plan.vortex_game, &plan.profile_id);
    let own_plugins = plugins_path.is_file();
    let first_rule = definition.load_orders.first();
    let plugin_contents = if own_plugins && plan.plugins.kept {
        first_rule.map(|rule| {
            let lines: Vec<(String, bool)> = plan
                .plugins
                .lines
                .iter()
                .map(|line| (line.name.clone(), line.active))
                .collect();
            game_plugins::list_bytes(rule, &lines)
        })
    } else {
        None
    };
    for list in &mut state.plugin_lists {
        list.contents = None;
    }
    if let Some(rule) = first_rule {
        if let Some(list) = state.plugin_lists.iter_mut().find(|l| l.list == rule.id) {
            list.contents = plugin_contents.clone();
        }
    }

    // Likewise, only this profile's own INI copies go in the loadout. Every other INI is shared.
    for ini in &mut state.inis {
        ini.contents = None;
    }
    for ini in &plan.inis {
        let contents = match std::fs::read(&ini.from) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Err(ImportError::Other(format!(
                    "the profile's INI {} could not be read: {e}",
                    ini.from.display()
                )))
            }
        };
        if let Some(existing) = state.inis.iter_mut().find(|i| i.file == ini.instance_path) {
            existing.contents = Some(contents);
        } else {
            state.inis.push(crate::game_loadout::GameLoadoutIni {
                file: ini.instance_path.clone(),
                contents: Some(contents),
            });
        }
    }
    // The game's plugin list is also one of its user files, so keep the INI-shaped entry in step
    // with the plugin list above rather than clearing it after writing the list.
    if let Some(rule) = first_rule {
        if let Some(ini) = state
            .inis
            .iter_mut()
            .find(|ini| ini.file == rule.file.as_str())
        {
            ini.contents = plugin_contents;
        }
    }

    // The profile's own load-after rules replace anything captured from the first profile.
    for rules in &mut state.rules {
        rules.rules = Vec::new();
    }
    if let Some(rule) = definition.load_orders.first() {
        let rules: Vec<crate::game_load_order::LoadAfterRule> = plan
            .userlist
            .rules
            .iter()
            .map(|rule| crate::game_load_order::LoadAfterRule {
                item: rule.item.clone(),
                after: rule.after.clone(),
            })
            .collect();
        if let Some(existing) = state.rules.iter_mut().find(|r| r.list == rule.id) {
            existing.rules = rules;
        } else {
            state.rules.push(crate::game_loadout::GameLoadoutRules {
                list: rule.id.clone(),
                rules,
            });
        }
    }
    state.saves = if plan.local_saves {
        SavesChoice::Own
    } else {
        SavesChoice::Shared
    };
    // A merged import has no earlier own folder to adopt: every own-saves profile gets its own.
    state.save_folder = plan.local_saves.then(|| plan.profile.clone());
    Ok(state)
}

/// Run a several-profile import. A merge makes one instance holding the union,
/// creates a loadout per profile and applies the first; separate imports make
/// one instance per profile.
pub fn run_profiles(
    ctx: &Ctx,
    inventory: &GameInventory,
    base: &VortexRequest,
    profiles: &[String],
    grouping: super::profiles::Grouping,
    options: RunOptions,
    progress: &dyn Fn(ImportProgress),
) -> Result<MultiProfileReport, ImportError> {
    let plan = plan_profiles(ctx, inventory, base, profiles, grouping)?;
    if !plan.fits {
        return Err(ImportError::NotEnoughSpace {
            needed: plan.bytes,
            free: plan.free_bytes.unwrap_or(0),
        });
    }
    match grouping {
        super::profiles::Grouping::Separate => {
            let mut instances = Vec::new();
            let mut saves = Vec::new();
            for profile in profiles {
                let request = VortexRequest {
                    profile: profile.clone(),
                    name: Some(
                        base.name
                            .clone()
                            .map(|name| format!("{name} ({profile})"))
                            .unwrap_or_else(|| profile.clone()),
                    ),
                    ..base.clone()
                };
                let report = run(ctx, inventory, &request, options, progress)?;
                if let Some(instance) = report.instance_id.clone() {
                    instances.push(instance);
                }
                saves.push(ProfileSavesReport {
                    profile: report.plan.profile.clone(),
                    copied: report.saves_copied,
                    skipped_existing: report.saves_skipped_existing,
                    note: report
                        .saves_folder
                        .is_none()
                        .then(|| "the profile uses the game's shared saves".to_string()),
                });
            }
            Ok(MultiProfileReport {
                plan,
                instances,
                loadouts: Vec::new(),
                saves,
            })
        }
        super::profiles::Grouping::Merged => {
            let request = VortexRequest {
                profile: profiles[0].clone(),
                // The merged branch copies each profile's saves into its own loadout folder.
                copy_saves: false,
                ..base.clone()
            };
            let first_report = run(ctx, inventory, &request, options, progress)?;
            let instance_id = first_report
                .instance_id
                .clone()
                .ok_or_else(|| ImportError::Other("the base import made no instance".into()))?;

            let definition = ctx
                .games
                .game(&plan.game)
                .cloned()
                .ok_or_else(|| ImportError::Other("the game definition is missing".into()))?;

            let merged = (|| -> Result<(Vec<String>, Vec<ProfileSavesReport>), ImportError> {
                // Every mod the chosen profiles bring, not just the first's. A mod two profiles
                // enable with identical bytes is still one content item, but it belongs to both.
                let mut mod_to_item: HashMap<String, String> = HashMap::new();
                for (index, profile_plan) in plan.plans.iter().enumerate() {
                    for item in &profile_plan.mods {
                        if index > 0 && !item.enabled {
                            continue;
                        }
                        if mod_to_item.contains_key(&item.mod_id) {
                            continue;
                        }
                        let item_id =
                            add_mod_to_merged(ctx, &instance_id, &definition, profile_plan, item)?;
                        mod_to_item.insert(item.mod_id.clone(), item_id);
                    }
                }

                // Lay the union out in Vortex's order, not in the order it was stored.
                let order = merged_mod_order(&definition, &plan)?;
                order_merged_layers(ctx, &instance_id, &order, &mod_to_item)?;

                let mut item_mods: HashMap<String, Vec<String>> = HashMap::new();
                for (mod_id, item_id) in &mod_to_item {
                    item_mods
                        .entry(item_id.clone())
                        .or_default()
                        .push(mod_id.clone());
                }
                let manifest = game_instance::get_manifest(ctx, &instance_id)?;
                let mut layer_mod: HashMap<String, Vec<String>> = HashMap::new();
                for layer in manifest.layers.layers() {
                    if let agora_game_api::LayerSource::Content { content } = &layer.source {
                        layer_mod.insert(
                            layer.id.as_str().to_string(),
                            item_mods.get(content).cloned().unwrap_or_default(),
                        );
                    }
                }

                let mut loadouts = Vec::new();
                for (profile, profile_plan) in profiles.iter().zip(plan.plans.iter()) {
                    let state = profile_state_for_merge(
                        ctx,
                        &instance_id,
                        &definition,
                        profile_plan,
                        &layer_mod,
                    )?;
                    crate::game_loadout::save_game_profile(ctx, &instance_id, profile, state)
                        .map_err(ImportError::Other)?;
                    loadouts.push(profile.clone());
                }

                // The instance keeps the first profile's setup, including its save choice and
                // the loadout's own save folder.
                crate::game_loadout::apply_game_profile_with_undo(
                    ctx,
                    &instance_id,
                    &definition,
                    &profiles[0],
                    false,
                )
                .map_err(|e| ImportError::Other(e.to_string()))?;

                // Every own-saving profile copies into its own loadout's save folder, so two mod
                // setups never load each other's saves. A file already there is kept and
                // counted; an unreadable one is named.
                let mut saves = Vec::new();
                for (profile, profile_plan) in profiles.iter().zip(plan.plans.iter()) {
                    if !profile_plan.saves.local {
                        saves.push(ProfileSavesReport {
                            profile: profile.clone(),
                            copied: 0,
                            skipped_existing: 0,
                            note: Some("the profile uses the game's shared saves".to_string()),
                        });
                        continue;
                    }
                    let Some(rule) =
                        crate::game_saves::rule_for(&definition, &profile_plan.store_id)
                    else {
                        saves.push(ProfileSavesReport {
                            profile: profile.clone(),
                            copied: 0,
                            skipped_existing: 0,
                            note: Some(
                                "the game has no save-folder setting for this install, so its saves stay shared"
                                    .to_string(),
                            ),
                        });
                        continue;
                    };
                    let folder =
                        crate::game_saves::own_folder_scoped(rule, &instance_id, Some(profile))
                            .map_err(|e| ImportError::Other(e.to_string()))?;
                    if !base.copy_saves {
                        let note = if profile_plan.saves.files > 0 {
                            format!(
                                "Agora did not copy {} save file(s): they are under Documents, which needs --copy-saves. To copy them by hand, copy the files in {} into {}",
                                profile_plan.saves.files,
                                profile_plan
                                    .saves
                                    .from
                                    .as_ref()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_default(),
                                folder.display()
                            )
                        } else {
                            "the profile's own save folder is empty".to_string()
                        };
                        saves.push(ProfileSavesReport {
                            profile: profile.clone(),
                            copied: 0,
                            skipped_existing: 0,
                            note: Some(note),
                        });
                        continue;
                    }
                    let Some(from) = &profile_plan.saves.from else {
                        saves.push(ProfileSavesReport {
                            profile: profile.clone(),
                            copied: 0,
                            skipped_existing: 0,
                            note: Some("the profile's own save folder is empty".to_string()),
                        });
                        continue;
                    };
                    let (copied, skipped, notes) = copy_save_folder(from, &folder)?;
                    saves.push(ProfileSavesReport {
                        profile: profile.clone(),
                        copied,
                        skipped_existing: skipped,
                        note: (!notes.is_empty()).then(|| notes.join("; ")),
                    });
                }
                Ok((loadouts, saves))
            })();

            match merged {
                Ok((loadouts, saves)) => Ok(MultiProfileReport {
                    plan,
                    instances: vec![instance_id],
                    loadouts,
                    saves,
                }),
                Err(error) => Err(abandon_half_made_instance(ctx, &instance_id, error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_leveldb::{Options, DB};

    fn store_folder() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("agora-vortex-state-")
            .tempdir()
            .expect("a temp folder can be made");
        let store = tmp.path().join("Vortex").join("state.v2");
        std::fs::create_dir_all(&store).expect("the store folder can be made");
        (tmp, store)
    }

    fn write_store(store: &Path, pairs: &[(&str, &str)]) {
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

    #[test]
    fn a_store_without_persistent_mods_is_an_error() {
        let (_tmp, store) = store_folder();
        write_store(&store, &[("settings###mods###installPath", "{}")]);
        let error = read_state(&store).expect_err("no mods is not an empty setup");
        assert!(matches!(error, ImportError::VortexNoMods), "{error}");
    }

    #[test]
    fn a_save_file_size_that_cannot_be_read_is_reported_not_counted_as_zero() {
        let mut warnings = Vec::new();
        let entry = SaveEntry {
            path: PathBuf::from("Save1.ess"),
            metadata: Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the file is locked",
            )),
        };
        assert_eq!(sized_save(entry, &mut warnings), None);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("size could not be read"));
        assert!(warnings[0].contains("Save1.ess"));
    }

    #[test]
    fn a_file_whose_size_cannot_be_read_is_an_error_not_zero_bytes() {
        let tmp = tempfile::TempDir::new().expect("a temp folder can be made");
        let missing = tmp.path().join("gone.esp");
        let paths = vec![(RelPath::new("gone.esp").unwrap(), missing)];
        let (file, error) = size_mod_files(&paths).expect_err("a missing file cannot be sized");
        assert_eq!(file, "gone.esp");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn reads_games_profiles_mods_and_the_staging_folder() {
        let (tmp, store) = store_folder();
        let mods = r#"{"testgame":{"mod-a":{"id":"mod-a","state":"installed","installationPath":"Alpha Folder","attributes":{"name":"Alpha Mod","version":"1.2","modId":42,"fileId":7},"rules":[{"type":"after","reference":{"id":"mod-b"}}]}}}"#;
        let profiles = r#"{"p1":{"id":"p1","gameId":"testgame","name":"Main","modState":{"mod-a":{"enabled":true}}}}"#;
        write_store(
            &store,
            &[
                ("persistent###mods", mods),
                ("persistent###profiles", profiles),
                (
                    "settings###mods###installPath###testgame",
                    "\"{USERDATA}/{GAME}/mods\"",
                ),
                (
                    "settings###gameMode###discovered###testgame",
                    r#"{"path":"C:/Games/TestGame"}"#,
                ),
            ],
        );
        let state = read_state(&store).expect("the store reads");
        assert_eq!(state.games.len(), 1);
        let game = &state.games[0];
        assert_eq!(game.id, "testgame");
        assert_eq!(
            game.staging,
            Ok(tmp.path().join("Vortex").join("testgame").join("mods"))
        );
        assert_eq!(
            game.discovered_path,
            Some(PathBuf::from("C:/Games/TestGame"))
        );
        assert_eq!(game.mods.len(), 1);
        let entry = &game.mods[0];
        assert_eq!(entry.name, "Alpha Mod");
        assert_eq!(entry.version.as_deref(), Some("1.2"));
        assert_eq!(entry.nexus_mod_id, Some(42));
        assert_eq!(entry.nexus_file_id, Some(7));
        assert_eq!(entry.installation_folder, "Alpha Folder");
        assert_eq!(entry.rules.len(), 1);
        assert_eq!(entry.rules[0].kind, "after");
        assert_eq!(entry.rules[0].target_mod_id.as_deref(), Some("mod-b"));
        assert_eq!(game.profiles.len(), 1);
        assert_eq!(game.profiles[0].name, "Main");
        assert_eq!(game.profiles[0].enabled.get("mod-a"), Some(&true));
    }

    #[test]
    fn a_profile_that_does_not_mention_a_mod_leaves_it_off() {
        let (_tmp, store) = store_folder();
        write_store(
            &store,
            &[
                (
                    "persistent###mods",
                    r#"{"testgame":{"a":{"id":"a","installationPath":"a"},"b":{"id":"b","installationPath":"b"}}}"#,
                ),
                (
                    "persistent###profiles",
                    r#"{"p1":{"id":"p1","gameId":"testgame","name":"Main","modState":{"a":{"enabled":true}}}}"#,
                ),
            ],
        );
        let state = read_state(&store).expect("the store reads");
        let profile = &state.games[0].profiles[0];
        assert_eq!(profile.enabled.get("a"), Some(&true));
        assert_eq!(profile.enabled.get("b"), None);
    }

    #[test]
    fn an_unknown_placeholder_is_an_error_naming_it() {
        let (_tmp, store) = store_folder();
        write_store(
            &store,
            &[
                (
                    "persistent###mods",
                    r#"{"testgame":{"a":{"id":"a","installationPath":"a"}}}"#,
                ),
                (
                    "settings###mods###installPath###testgame",
                    "\"{USERDATA}/{NOPE}/mods\"",
                ),
            ],
        );
        let state = read_state(&store).expect("the store reads");
        let reason = state.games[0]
            .staging
            .as_ref()
            .expect_err("the placeholder cannot be resolved");
        assert!(reason.contains("NOPE"), "{reason}");
    }

    #[test]
    fn the_default_staging_folder_and_placeholders_match_vortex() {
        let user_data = std::env::temp_dir().join("agora-vortex-user-data");
        assert_eq!(
            resolve_install_path(None, &user_data, "testgame", None).unwrap(),
            user_data.join("testgame").join("mods")
        );
        assert_eq!(
            resolve_install_path(
                Some("{userdata}/{GAME}/staged"),
                &user_data,
                "testgame",
                Some("player")
            )
            .unwrap(),
            user_data.join("testgame").join("staged")
        );
        assert_eq!(
            resolve_install_path(Some("{base}/mods"), &user_data, "testgame", None).unwrap(),
            user_data.join("testgame").join("mods")
        );
    }

    #[test]
    fn with_no_definition_the_manifest_search_still_names_the_game_root() {
        let tmp = tempfile::Builder::new()
            .prefix("agora-vortex-manifest-")
            .tempdir()
            .expect("a temp folder can be made");
        let game = tmp.path();
        std::fs::write(
            game.join("vortex.deployment.json"),
            br#"{"version":1,"instance":"x","files":[]}"#,
        )
        .unwrap();
        let search = find_manifests(None, game);
        assert!(search.found);
        assert!(search
            .searched
            .iter()
            .any(|path| path == &game.join("vortex.deployment.json")));
    }
}
