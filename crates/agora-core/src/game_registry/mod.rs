//! Game registry and install identification (MASTER_SPEC §26.3, §26.11).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use agora_game_api::{
    Condition, FrameworkId, GameDefinition, GameHost, GameId, GameInstall, GamePackage, GamePath,
    InstallId, InstallKind, LoadOrderFormat, RelPath, RuntimeIdentity, StoreId, ToolId,
    UserFileStrategy, GAME_API_VERSION,
};
use serde::{Deserialize, Serialize};

pub mod inherit;
pub mod scripted;
pub use inherit::{PackageInput, ResolvedPackage};

use crate::app_paths::AppPaths;
use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use crate::game_discovery::{DiscoveredInstall, DiscoveryReport};
use crate::game_hooks::{CatalogEvent, CompiledServices, InstanceBackend, InstanceBackends};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PackageSource {
    Compiled { crate_name: String },
    Plugin { plugin_id: String },
}

/// A pure declarative package defined from deserialized JSON data.
#[derive(Debug, Clone)]
pub struct DeclarativePackage(pub agora_game_api::PackageDefinition);

impl GamePackage for DeclarativePackage {
    fn definition(&self) -> &agora_game_api::PackageDefinition {
        &self.0
    }

    fn recognize_download(
        &self,
        game: &GameId,
        _file_name: &str,
        entries: &[String],
    ) -> Option<agora_game_api::DownloadRecognition> {
        let definition = self
            .0
            .games
            .iter()
            .find(|definition| &definition.id == game)?;
        let layout = definition.content_layout.as_ref()?;
        crate::download_watch::recognize_with_layout(entries, layout)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GameRegistryError {
    #[error("package {package_id} api_range {api_range} does not match GAME_API_VERSION {current_version}")]
    IncompatibleApiRange {
        package_id: String,
        api_range: semver::VersionReq,
        current_version: semver::Version,
    },
    #[error("package {package_id} defines no games")]
    NoGamesDefined { package_id: String },
    #[error("game id {game_id} is already registered")]
    GameAlreadyRegistered { game_id: GameId },
    #[error("game id {game_id} is held by an installed official plugin that is blocked")]
    GameReservedByBlockedPlugin { game_id: GameId },
    #[error(
        "store product ({store}, {product}) is held by an installed official plugin that is blocked"
    )]
    StoreProductReservedByBlockedPlugin { store: StoreId, product: String },
    #[error("store product ({store}, {product}) is already claimed by game {claimed_by}")]
    StoreProductAlreadyClaimed {
        store: StoreId,
        product: String,
        claimed_by: GameId,
    },
    #[error("framework {framework_id} names undefined game {game_id}")]
    UndefinedGameInFramework {
        framework_id: FrameworkId,
        game_id: GameId,
    },
    #[error("tool {tool_id} names undefined game {game_id}")]
    UndefinedGameInTool { tool_id: ToolId, game_id: GameId },
    #[error("game {game_id} plugin_list.user_file '{user_file}' names no user_files mapping")]
    PluginListUserFileNotFound { game_id: GameId, user_file: RelPath },
    #[error("game {game_id} save_location.ini '{user_file}' names no user_files mapping for one of its stores")]
    SaveLocationUserFileNotFound { game_id: GameId, user_file: RelPath },
    #[error("game {game_id} has an invalid save_location: {reason}")]
    InvalidSaveLocation { game_id: GameId, reason: String },
    #[error("game {game_id} has an invalid plugin_list: {reason}")]
    InvalidPluginList { game_id: GameId, reason: String },
    #[error("game {game_id} has an invalid load order: {reason}")]
    InvalidLoadOrder { game_id: GameId, reason: String },
    #[error("game {game_id} has an invalid launch alternative: {reason}")]
    InvalidLaunchAlternative { game_id: GameId, reason: String },
}

/// Whether a condition is a `present` with nothing to look for, which would silently never match.
fn condition_names_no_path(condition: &Condition) -> bool {
    match condition {
        Condition::Present(path) => path.as_str().is_empty(),
        _ => false,
    }
}

pub struct GameRegistry {
    packages: Vec<(PackageSource, Arc<dyn GamePackage>)>,
    games: BTreeMap<GameId, (usize, GameDefinition)>,
    store_products: HashMap<(StoreId, String), GameId>,
    /// In registration order, which is the order catalogs load and providers list.
    services: Vec<Arc<dyn CompiledServices>>,
}

#[derive(Default)]
pub struct GameRegistryBuilder {
    packages: Vec<(PackageSource, Arc<dyn GamePackage>)>,
    games: BTreeMap<GameId, (usize, GameDefinition)>,
    store_products: HashMap<(StoreId, String), GameId>,
    /// Game ids and store products an installed-but-blocked official plugin
    /// still holds. They are not loaded, but nobody else may claim them: the
    /// id belongs to that install until it is removed.
    reserved_games: BTreeSet<GameId>,
    reserved_store_products: BTreeSet<(StoreId, String)>,
    services: Vec<Arc<dyn CompiledServices>>,
}

impl GameRegistryBuilder {
    /// Keep a blocked official plugin's game ids out of other packages' reach.
    ///
    /// The package is not registered (its files did not verify), but the ids it
    /// declares stay reserved while the plugin is installed, so a community
    /// package cannot take over a game by shadowing a blocked official one.
    pub fn reserve_blocked(&mut self, package: &agora_game_api::PackageDefinition) {
        for game in &package.games {
            self.reserved_games.insert(game.id.clone());
            for store in &game.stores {
                self.reserved_store_products
                    .insert((store.store.clone(), store.product.clone()));
            }
        }
    }

    /// Register a compiled package together with the services it still
    /// provides through core's own types. Validated exactly like [`Self::add`];
    /// a refused package attaches no services.
    pub fn add_compiled(
        &mut self,
        crate_name: &str,
        package: Arc<dyn GamePackage>,
        services: Arc<dyn CompiledServices>,
    ) -> Result<(), GameRegistryError> {
        self.add(
            PackageSource::Compiled {
                crate_name: crate_name.to_string(),
            },
            package,
        )?;
        self.services.push(services);
        Ok(())
    }

    pub fn add(
        &mut self,
        source: PackageSource,
        package: Arc<dyn GamePackage>,
    ) -> Result<(), GameRegistryError> {
        let def = package.definition();

        // 1. API range check
        if !def.api_range.matches(&GAME_API_VERSION) {
            return Err(GameRegistryError::IncompatibleApiRange {
                package_id: def.id.clone(),
                api_range: def.api_range.clone(),
                current_version: GAME_API_VERSION,
            });
        }

        // 2. Defines no game
        if def.games.is_empty() {
            return Err(GameRegistryError::NoGamesDefined {
                package_id: def.id.clone(),
            });
        }

        // 3. Game IDs check (against reserved, existing and intra-package)
        let mut new_game_ids = BTreeSet::new();
        for game in &def.games {
            if self.reserved_games.contains(&game.id) {
                return Err(GameRegistryError::GameReservedByBlockedPlugin {
                    game_id: game.id.clone(),
                });
            }
            if self.games.contains_key(&game.id) || !new_game_ids.insert(game.id.clone()) {
                return Err(GameRegistryError::GameAlreadyRegistered {
                    game_id: game.id.clone(),
                });
            }
        }

        // 4. (store, product) claims check
        let mut new_store_claims: HashMap<(StoreId, String), GameId> = HashMap::new();
        for game in &def.games {
            for store_id in &game.stores {
                let key = (store_id.store.clone(), store_id.product.clone());
                if self.reserved_store_products.contains(&key) {
                    return Err(GameRegistryError::StoreProductReservedByBlockedPlugin {
                        store: store_id.store.clone(),
                        product: store_id.product.clone(),
                    });
                }
                if let Some(claimed_by) = self.store_products.get(&key) {
                    return Err(GameRegistryError::StoreProductAlreadyClaimed {
                        store: store_id.store.clone(),
                        product: store_id.product.clone(),
                        claimed_by: claimed_by.clone(),
                    });
                }
                if let Some(claimed_by) = new_store_claims.get(&key) {
                    return Err(GameRegistryError::StoreProductAlreadyClaimed {
                        store: store_id.store.clone(),
                        product: store_id.product.clone(),
                        claimed_by: claimed_by.clone(),
                    });
                }
                new_store_claims.insert(key, game.id.clone());
            }
        }

        // 5. Frameworks name defined games
        for framework in &def.frameworks {
            if !new_game_ids.contains(&framework.game) {
                return Err(GameRegistryError::UndefinedGameInFramework {
                    framework_id: framework.id.clone(),
                    game_id: framework.game.clone(),
                });
            }
        }

        // 6. Tools name defined games
        for tool in &def.tools {
            if !new_game_ids.contains(&tool.game) {
                return Err(GameRegistryError::UndefinedGameInTool {
                    tool_id: tool.id.clone(),
                    game_id: tool.game.clone(),
                });
            }
        }

        // 7. Each load order must be able to work, or it would silently do nothing.
        for game in &def.games {
            let mut load_order_ids = BTreeSet::new();
            for rule in &game.load_orders {
                if rule.id.trim().is_empty() {
                    return Err(GameRegistryError::InvalidLoadOrder {
                        game_id: game.id.clone(),
                        reason: "a load order has an empty id".to_string(),
                    });
                }
                if !load_order_ids.insert(rule.id.clone()) {
                    return Err(GameRegistryError::InvalidLoadOrder {
                        game_id: game.id.clone(),
                        reason: format!("load order '{}' is declared twice", rule.id),
                    });
                }
                if !matches!(
                    rule.rules.kind.as_str(),
                    "none" | "script" | "creation_engine"
                ) {
                    return Err(GameRegistryError::InvalidLoadOrder {
                        game_id: game.id.clone(),
                        reason: format!(
                            "load order '{}' has unknown rule kind '{}'",
                            rule.id, rule.rules.kind
                        ),
                    });
                }
                if let LoadOrderFormat::Lines { active_prefix, .. } = &rule.format {
                    if active_prefix.chars().any(char::is_whitespace) {
                        return Err(GameRegistryError::InvalidPluginList {
                            game_id: game.id.clone(),
                            reason: "active_prefix must not contain whitespace".to_string(),
                        });
                    }
                    let matches_user_file = game.user_files.iter().any(|uf| {
                        uf.instance_path == rule.file
                            && uf.strategy == UserFileStrategy::JournaledSwap
                    });
                    if !matches_user_file {
                        return Err(GameRegistryError::PluginListUserFileNotFound {
                            game_id: game.id.clone(),
                            user_file: rule.file.clone(),
                        });
                    }
                    if rule.items.patterns.iter().all(|p| p.trim().is_empty()) {
                        return Err(GameRegistryError::InvalidPluginList {
                            game_id: game.id.clone(),
                            reason: "patterns names no item file".to_string(),
                        });
                    }
                }
                if rule.rules.is_script()
                    && rule
                        .rules
                        .entry
                        .as_deref()
                        .is_none_or(|entry| entry.trim().is_empty())
                {
                    return Err(GameRegistryError::InvalidLoadOrder {
                        game_id: game.id.clone(),
                        reason: format!(
                            "load order '{}' has a script rule set with no entrypoint",
                            rule.id
                        ),
                    });
                }
                // A compiled package answers script hooks in Rust, and a plugin's manifest is not
                // visible here, so the entrypoints are checked when the script loads.
                for name in &rule.rules.implicit {
                    if name.trim().is_empty() || name.contains(['/', '\\']) {
                        return Err(GameRegistryError::InvalidPluginList {
                            game_id: game.id.clone(),
                            reason: format!("implicit plugin '{name}' is not a plugin file name"),
                        });
                    }
                }
            }
        }

        // 7b. A save location must be a setting the game's own per-user file holds, for every store
        // it names, or an instance's save choice would write a file the game never reads.
        for game in &def.games {
            for rule in &game.save_location {
                let stores: Vec<StoreId> = if rule.stores.is_empty() {
                    game.stores.iter().map(|s| s.store.clone()).collect()
                } else {
                    rule.stores.clone()
                };
                for store in &stores {
                    let held = game
                        .user_files
                        .iter()
                        .any(|uf| uf.instance_path == rule.ini && uf.applies_to_store(store));
                    if !held {
                        return Err(GameRegistryError::SaveLocationUserFileNotFound {
                            game_id: game.id.clone(),
                            user_file: rule.ini.clone(),
                        });
                    }
                }
                let own_is_usable = !rule.own_value.trim().is_empty()
                    && !rule.own_value.contains(['\r', '\n'])
                    && rule.own_value.contains("{instance}");
                if rule.section.trim().is_empty() || rule.key.trim().is_empty() || !own_is_usable {
                    return Err(GameRegistryError::InvalidSaveLocation {
                        game_id: game.id.clone(),
                        reason: "the section and key must be named, and own_value must be one \
                                 line that contains {instance}"
                            .to_string(),
                    });
                }
            }
        }

        // 8. Launch alternatives must be well-formed and start a program inside the game
        for game in &def.games {
            let mut seen = BTreeSet::new();
            for alt in &game.launch_alternatives {
                let reason = if alt.id.trim().is_empty() {
                    Some("an alternative has an empty id".to_string())
                } else if !seen.insert(alt.id.clone()) {
                    Some(format!("alternative '{}' is declared twice", alt.id))
                } else if condition_names_no_path(&alt.when) {
                    Some(format!(
                        "alternative '{}' has an empty present path",
                        alt.id
                    ))
                } else if !matches!(
                    alt.executable,
                    GamePath::Runtime { .. } | GamePath::Base { .. } | GamePath::Install { .. }
                ) {
                    Some(format!(
                        "alternative '{}' must start a program inside the game's runtime, base or install folder",
                        alt.id
                    ))
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return Err(GameRegistryError::InvalidLaunchAlternative {
                        game_id: game.id.clone(),
                        reason,
                    });
                }
            }
        }

        // Commit mutations only after all checks pass
        let pkg_idx = self.packages.len();
        for game in &def.games {
            self.games.insert(game.id.clone(), (pkg_idx, game.clone()));
        }
        for (key, game_id) in new_store_claims {
            self.store_products.insert(key, game_id);
        }
        self.packages.push((source, package));

        Ok(())
    }

    pub fn build(self) -> GameRegistry {
        GameRegistry {
            packages: self.packages,
            games: self.games,
            store_products: self.store_products,
            services: self.services,
        }
    }
}

impl GameRegistry {
    pub fn builder() -> GameRegistryBuilder {
        GameRegistryBuilder::default()
    }

    pub fn empty() -> Self {
        Self {
            packages: Vec::new(),
            games: BTreeMap::new(),
            store_products: HashMap::new(),
            services: Vec::new(),
        }
    }

    /// Whether any game is registered. Core runs without one, but an adapter
    /// that forgot to register would lose every game's catalogs and providers
    /// without an error, so startup says so.
    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }

    /// Run every package's startup recovery. Returns warnings.
    pub fn recover_at_startup(&self, paths: &AppPaths) -> Vec<String> {
        self.services
            .iter()
            .flat_map(|services| services.recover_at_startup(paths))
            .collect()
    }

    /// Load every package's catalogs. All packages run even if one fails; the
    /// first error is returned after the others have had their turn.
    pub fn load_catalogs(
        &self,
        ctx: &Ctx,
        registry: Option<&rusqlite::Connection>,
        event: CatalogEvent,
    ) -> LauncherResult<Vec<String>> {
        let mut warnings = Vec::new();
        let mut first_error = None;
        for services in &self.services {
            match services.load_catalogs(ctx, registry, event) {
                Ok(mut more) => warnings.append(&mut more),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(warnings),
        }
    }

    /// Every compiled package's content providers, in registration order.
    pub fn builtin_providers(&self, ctx: &Ctx) -> Vec<Arc<dyn crate::providers::ContentProvider>> {
        self.services
            .iter()
            .flat_map(|services| services.providers(ctx))
            .collect()
    }

    /// Every package's instances behind one backend, or an error naming the
    /// missing setup when no package provides any.
    pub fn instance_backend(&self) -> LauncherResult<Arc<dyn InstanceBackend>> {
        let mut backends: Vec<_> = self
            .services
            .iter()
            .filter_map(|services| services.instances())
            .collect();
        // Generic game instances exist for every registered package, so
        // `instances.*` and `content.*` work for them through the same SDK
        // calls Minecraft's package uses (MASTER_SPEC §30.12). A registry with
        // no packages still says so: there is nothing to enumerate.
        if !self.packages.is_empty() {
            backends.push(Arc::new(crate::game_hooks::GenericInstanceBackend));
        }
        match backends.len() {
            0 => Err(LauncherError::Generic {
                code: "ERR_NO_GAME_PACKAGE".into(),
                message: "No game package is registered to handle instances.".into(),
            }),
            1 => Ok(backends.into_iter().next().expect("one backend")),
            _ => Ok(Arc::new(InstanceBackends(backends))),
        }
    }

    /// Iterator over registered game definitions, ordered by game id.
    pub fn games(&self) -> impl Iterator<Item = &GameDefinition> {
        self.games.values().map(|(_, def)| def)
    }

    pub fn game(&self, id: &GameId) -> Option<&GameDefinition> {
        self.games.get(id).map(|(_, def)| def)
    }

    pub fn package_for(&self, id: &GameId) -> Option<&Arc<dyn GamePackage>> {
        self.games.get(id).map(|(idx, _)| &self.packages[*idx].1)
    }

    /// The framework `framework_id` of `game`, as the game's package declares it (MASTER_SPEC §26.6).
    pub fn framework(
        &self,
        game: &GameId,
        framework_id: &FrameworkId,
    ) -> Option<&agora_game_api::FrameworkDefinition> {
        self.package_for(game)?
            .definition()
            .frameworks
            .iter()
            .find(|framework| framework.game == *game && framework.id == *framework_id)
    }

    /// The tool `tool_id` of `game`, as the game's package declares it (MASTER_SPEC §26.9).
    pub fn tool(&self, game: &GameId, tool_id: &ToolId) -> Option<&agora_game_api::ToolDefinition> {
        self.package_for(game)?
            .definition()
            .tools
            .iter()
            .find(|tool| tool.game == *game && tool.id == *tool_id)
    }

    pub fn source_for(&self, id: &GameId) -> Option<&PackageSource> {
        self.games.get(id).map(|(idx, _)| &self.packages[*idx].0)
    }

    pub fn game_for_store_product(&self, store: &StoreId, product: &str) -> Option<&GameId> {
        self.store_products
            .get(&(store.clone(), product.to_string()))
    }

    /// The games whose executable (a version source a game declares) is inside `folder`. A folder
    /// the user added is identified by this: a game is in it when its executable is there.
    pub fn games_with_executable_in(&self, folder: &Path) -> Vec<GameId> {
        self.games()
            .filter(|game| {
                game.version_sources.iter().any(|source| {
                    matches!(
                        source,
                        agora_game_api::VersionSource::Executable { path }
                            if folder.join(path.as_str()).is_file()
                    )
                })
            })
            .map(|game| game.id.clone())
            .collect()
    }

    /// The one game a folder holds. Two games matching the same folder is no answer at all, so
    /// the folder stays unidentified rather than guessing.
    pub fn game_for_folder(&self, folder: &Path) -> Option<GameId> {
        match self.games_with_executable_in(folder).as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime matching and install identification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameInventory {
    pub installs: Vec<IdentifiedInstall>,
    pub unsupported: Vec<DiscoveredInstall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentifiedInstall {
    pub game: GameId,
    pub install_id: InstallId,
    pub discovered: DiscoveredInstall,
    pub add_ons: Vec<DiscoveredInstall>,
    pub runtime: RuntimeResolution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RuntimeResolution {
    Identified {
        runtime: RuntimeIdentity,
        source: String, // "executable" | "store_record"
    },
    Unidentified {
        reasons: Vec<String>,
    },
}

impl IdentifiedInstall {
    pub fn game_install(&self) -> Option<GameInstall> {
        match &self.runtime {
            RuntimeResolution::Identified { runtime, .. } => Some(GameInstall {
                id: self.install_id.clone(),
                runtime: runtime.clone(),
                kind: self.discovered.kind.clone(),
                location: self.discovered.location.to_string_lossy().to_string(),
                volume: self.discovered.volume.clone(),
                capabilities: self.discovered.capabilities.clone(),
            }),
            RuntimeResolution::Unidentified { .. } => None,
        }
    }
}

/// `{store}:{product}`, stable across runs, because bases and instances refer
/// to installs by it. A product that is not already a valid id part is
/// lowercased and sanitised, then suffixed with a hash of the original, so two
/// products that sanitise alike (`A.B` and `a-b`) still get different ids.
pub fn make_install_id(store: &StoreId, product: &str) -> InstallId {
    let mut part = String::new();
    for c in product.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            part.push(c);
        } else if !part.ends_with('-') {
            part.push('-');
        }
    }
    let part = part.trim_matches(|c| c == '-' || c == '_');
    let budget = agora_game_api::MAX_ID_LEN.saturating_sub(store.as_str().len() + 1);
    let id = if !part.is_empty() && part == product && part.len() <= budget {
        part.to_string()
    } else {
        let suffix = format!("{:08x}", fnv1a_32(product));
        let keep = budget.saturating_sub(suffix.len() + 1);
        let prefix = part[..part.len().min(keep)].trim_end_matches(['-', '_']);
        if prefix.is_empty() {
            suffix
        } else {
            format!("{prefix}-{suffix}")
        }
    };
    InstallId::new(format!("{}:{id}", store.as_str()))
        .expect("store ids are short and the product part is sanitised")
}

pub fn fnv1a_32(s: &str) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for byte in s.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// What is installed on this machine: the stores' installs and the folders the user added by hand
/// (MASTER_SPEC §26.3), identified against the registry. Every adapter asks this one function, so a
/// custom-folder game appears in the Installs list, the instance and base dialogs, import and the
/// CLI alike.
pub fn installed_games(ctx: &Ctx) -> GameInventory {
    let report = crate::game_discovery::discover_with_folders(&ctx.paths);
    identify_installs(
        &ctx.games,
        &report,
        &crate::game_discovery::file_version::read_file_version,
    )
}

pub fn identify_installs(
    registry: &GameRegistry,
    report: &DiscoveryReport,
    read_version: &dyn Fn(&Path) -> Option<String>,
) -> GameInventory {
    let mut installs = Vec::new();
    let mut unsupported = Vec::new();

    let base_games: Vec<&DiscoveredInstall> = report
        .installs
        .iter()
        .filter(|i| i.kind == InstallKind::BaseGame)
        .collect();

    for bg in base_games {
        // A folder the user added has no store product, so it is matched by its executable.
        let claimed = if bg.store == agora_game_api::StoreId::direct() {
            registry.game_for_folder(&bg.location)
        } else {
            registry
                .game_for_store_product(&bg.store, &bg.product)
                .cloned()
        };
        if let Some(game_id) = claimed {
            let game_def = registry
                .game(&game_id)
                .expect("game definition must exist for claimed product");

            let add_ons: Vec<DiscoveredInstall> = report
                .installs
                .iter()
                .filter(|i| {
                    i.kind == InstallKind::AddOn
                        && i.store == bg.store
                        && i.parent_product.as_deref() == Some(&bg.product)
                })
                .cloned()
                .collect();

            let install_id = make_install_id(&bg.store, &bg.product);
            let runtime =
                resolve_runtime(game_def, bg, read_version, registry.package_for(&game_id));

            installs.push(IdentifiedInstall {
                game: game_id,
                install_id,
                discovered: bg.clone(),
                add_ons,
                runtime,
            });
        } else {
            unsupported.push(bg.clone());
        }
    }

    GameInventory {
        installs,
        unsupported,
    }
}

/// A game host that offers nothing. `detect_runtime` is called before a
/// launch host exists; a package that needs one there is told so.
pub(crate) struct NoGameHost;

/// A game host that offers nothing, for hook call sites that have no launch
/// host yet. Scripted hooks use `game:read` through the plugin bridge instead.
pub(crate) fn null_game_host() -> &'static dyn GameHost {
    &NoGameHost
}

fn no_host<T>() -> agora_game_api::GameFuture<'static, T> {
    Box::pin(async {
        Err(agora_game_api::GameError {
            code: "ERR_NO_GAME_HOST".into(),
            message: "no game host is available while identifying an install".into(),
        })
    })
}

impl agora_game_api::GameHost for NoGameHost {
    fn download(
        &self,
        _: agora_game_api::DownloadRequest,
    ) -> agora_game_api::GameFuture<'_, agora_game_api::DownloadedArtifact> {
        no_host()
    }
    fn read_install_file(
        &self,
        _: agora_game_api::InstallReadRequest,
    ) -> agora_game_api::GameFuture<'_, Vec<u8>> {
        no_host()
    }
    fn read_artifact(
        &self,
        _: agora_game_api::ArtifactReadRequest,
    ) -> agora_game_api::GameFuture<'_, Vec<u8>> {
        no_host()
    }
    fn extract_archive(
        &self,
        _: agora_game_api::ArchiveRequest,
    ) -> agora_game_api::GameFuture<'_, agora_game_api::ExtractedArchive> {
        no_host()
    }
    fn run_fomod(
        &self,
        _: agora_game_api::FomodRequest,
    ) -> agora_game_api::GameFuture<'_, agora_game_api::FomodResult> {
        no_host()
    }
    fn sort_load_order(
        &self,
        _: agora_game_api::SortLoadOrderRequest,
    ) -> agora_game_api::GameFuture<'_, agora_game_api::LoadOrder> {
        no_host()
    }
    fn run_tool(
        &self,
        _: agora_game_api::ToolRunRequest,
    ) -> agora_game_api::GameFuture<'_, agora_game_api::ToolRunResult> {
        no_host()
    }
    fn emit_event(&self, _: agora_game_api::GameEvent) -> agora_game_api::GameResult<()> {
        Ok(())
    }
}

/// Drive a hook future to completion. The hooks themselves only wait on the
/// plugin thread's synchronous bridge, so parking this thread is enough and no
/// Tokio runtime is created here.
pub(crate) fn block_on_hook<T>(future: impl std::future::Future<Output = T>) -> T {
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn resolve_runtime(
    game_def: &GameDefinition,
    discovered: &DiscoveredInstall,
    read_version: &dyn Fn(&Path) -> Option<String>,
    package: Option<&Arc<dyn GamePackage>>,
) -> RuntimeResolution {
    let mut reasons = Vec::new();

    for source in &game_def.version_sources {
        match source {
            agora_game_api::VersionSource::Executable { path } => {
                if !discovered.capabilities.executables_readable {
                    reasons.push("executables not readable".to_string());
                    continue;
                }
                let path_str = path.as_str();
                if path_str.is_empty()
                    || RelPath::new(path_str).is_err()
                    || path_str.split('/').any(|p| p == "..")
                    || path_str.split('\\').any(|p| p == "..")
                {
                    reasons.push(format!("executable path {path_str} escapes install"));
                    continue;
                }
                let relative = std::path::Path::new(path_str);
                if relative.is_absolute() {
                    reasons.push(format!("executable path {path_str} is absolute"));
                    continue;
                }
                let exe_path = discovered.location.join(relative);
                match read_version(&exe_path) {
                    Some(version) => {
                        return RuntimeResolution::Identified {
                            runtime: RuntimeIdentity {
                                game: game_def.id.clone(),
                                store: discovered.store.clone(),
                                version,
                                build: discovered.store_build.clone(),
                            },
                            source: "executable".to_string(),
                        };
                    }
                    None => {
                        reasons.push(format!("could not read executable version from {path_str}"));
                    }
                }
            }
            agora_game_api::VersionSource::StoreRecord => {
                let version = discovered
                    .store_version
                    .clone()
                    .or_else(|| discovered.store_build.clone());
                match version {
                    Some(version) => {
                        return RuntimeResolution::Identified {
                            runtime: RuntimeIdentity {
                                game: game_def.id.clone(),
                                store: discovered.store.clone(),
                                version,
                                build: discovered.store_build.clone(),
                            },
                            source: "store_record".to_string(),
                        };
                    }
                    None => {
                        reasons.push("store record missing version and build".to_string());
                    }
                }
            }
            agora_game_api::VersionSource::PackageBehaviour => {
                let Some(package) = package else {
                    reasons.push("the game's package is not registered".to_string());
                    continue;
                };
                let runtime = RuntimeIdentity {
                    game: game_def.id.clone(),
                    store: discovered.store.clone(),
                    version: discovered
                        .store_version
                        .clone()
                        .or_else(|| discovered.store_build.clone())
                        .unwrap_or_default(),
                    build: discovered.store_build.clone(),
                };
                let install = GameInstall {
                    id: make_install_id(&discovered.store, &discovered.product),
                    runtime: runtime.clone(),
                    kind: discovered.kind.clone(),
                    location: discovered.location.to_string_lossy().to_string(),
                    volume: discovered.volume.clone(),
                    capabilities: discovered.capabilities.clone(),
                };
                match block_on_hook(package.detect_runtime(&NoGameHost, install)) {
                    Ok(Some(identity)) => {
                        return RuntimeResolution::Identified {
                            runtime: identity,
                            source: "package_behaviour".to_string(),
                        };
                    }
                    Ok(None) => {
                        reasons.push("the package's detect_runtime found no version".to_string());
                    }
                    Err(error) => reasons.push(format!(
                        "the package's detect_runtime failed: {}",
                        error.message
                    )),
                }
            }
        }
    }

    RuntimeResolution::Unidentified { reasons }
}

/// A minimal package for tests that need a registered game but not its data.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use agora_game_api::{
        DeploymentStrategy, GameDefinition, GameId, GamePackage, PackageDefinition,
    };
    use std::sync::Arc;

    struct Package(PackageDefinition);

    impl GamePackage for Package {
        fn definition(&self) -> &PackageDefinition {
            &self.0
        }
    }

    /// A package defining one game, `game_id`, with no stores and no data.
    pub fn package(game_id: &str) -> Arc<dyn GamePackage> {
        Arc::new(Package(PackageDefinition {
            id: format!("test.{game_id}"),
            version: semver::Version::new(0, 1, 0),
            api_range: semver::VersionReq::parse(">=0.1, <0.2").expect("valid range"),
            parents: Vec::new(),
            game_templates: std::collections::BTreeMap::new(),
            behaviours: None,
            games: vec![GameDefinition {
                mo2_game_name: None,
                session: Default::default(),
                id: GameId::new(game_id).expect("valid game id"),
                extends: None,
                name: game_id.to_string(),
                stores: Vec::new(),
                version_sources: Vec::new(),
                deployment: DeploymentStrategy::Redirect,
                content_rules: Vec::new(),
                framework_ids: Vec::new(),
                tool_ids: Vec::new(),
                launch: None,
                log_paths: Vec::new(),
                crash_paths: Vec::new(),
                user_files: Vec::new(),
                save_paths: Vec::new(),
                linked_archive_patterns: Vec::new(),
                declared_writes: Vec::new(),
                excluded_paths: Vec::new(),
                content_layout: None,
                load_orders: Vec::new(),
                runtime_files: Vec::new(),
                save_location: Vec::new(),
                launch_alternatives: Vec::new(),
                copy_patterns: Vec::new(),
            }],
            frameworks: Vec::new(),
            tools: Vec::new(),
        }))
    }
}
