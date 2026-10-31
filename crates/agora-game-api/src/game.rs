use crate::id::{FrameworkId, GameId, InstallId, LayerId, RelPath, StoreId, ToolId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    pub game: GameId,
    pub store: StoreId,
    /// Exact opaque version: Minecraft snapshots and four-part PE versions are
    /// not semver. Never compare these lexicographically as a version range.
    pub version: String,
    #[serde(default)]
    pub build: Option<String>,
}

impl RuntimeIdentity {
    pub fn minecraft(version: impl Into<String>) -> Self {
        Self {
            game: GameId::minecraft(),
            store: StoreId::mojang(),
            version: version.into(),
            build: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum VersionConstraint {
    Any,
    Exact(Vec<String>),
    /// Use only when the game's version scheme really is semver.
    Semver(semver::VersionReq),
}

/// The answer to "does this constraint match that runtime?".
///
/// Three-valued:
/// - `Supported`: explicitly verified compatible
/// - `Unsupported`: incompatible (store, version, or build mismatch)
/// - `Indeterminate`: cannot be determined (e.g. unparseable version against semver range, missing build)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Unsupported,
    Indeterminate,
}

impl Support {
    pub fn is_supported(&self) -> bool {
        matches!(self, Support::Supported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConstraint {
    pub game: GameId,
    /// Empty means any store/build. Multiple constraints are alternatives (OR);
    /// fields within a constraint are conjunctive (AND).
    pub stores: Vec<StoreId>,
    pub versions: VersionConstraint,
    pub builds: Vec<String>,
}

impl RuntimeConstraint {
    pub fn check(&self, runtime: &RuntimeIdentity) -> Support {
        if self.game != runtime.game {
            return Support::Unsupported;
        }
        if !self.stores.is_empty() && !self.stores.contains(&runtime.store) {
            return Support::Unsupported;
        }

        let mut indeterminate = false;

        match &self.versions {
            VersionConstraint::Any => {}
            VersionConstraint::Exact(versions) => {
                if !versions.contains(&runtime.version) {
                    return Support::Unsupported;
                }
            }
            VersionConstraint::Semver(requirement) => {
                match semver::Version::parse(&runtime.version) {
                    Ok(v) => {
                        if !requirement.matches(&v) {
                            return Support::Unsupported;
                        }
                    }
                    Err(_) => {
                        indeterminate = true;
                    }
                }
            }
        }

        if !self.builds.is_empty() {
            match &runtime.build {
                Some(b) => {
                    if !self.builds.contains(b) {
                        return Support::Unsupported;
                    }
                }
                None => {
                    indeterminate = true;
                }
            }
        }

        if indeterminate {
            Support::Indeterminate
        } else {
            Support::Supported
        }
    }

    pub fn matches(&self, runtime: &RuntimeIdentity) -> bool {
        self.check(runtime).is_supported()
    }
}

/// Check a list of alternative constraints: any one supporting the runtime is enough.
/// Empty constraints list means no constraints (supported).
pub fn check_any(constraints: &[RuntimeConstraint], runtime: &RuntimeIdentity) -> Support {
    if constraints.is_empty() {
        return Support::Supported;
    }
    let mut indeterminate = false;
    for constraint in constraints {
        match constraint.check(runtime) {
            Support::Supported => return Support::Supported,
            Support::Indeterminate => indeterminate = true,
            Support::Unsupported => {}
        }
    }
    if indeterminate {
        Support::Indeterminate
    } else {
        Support::Unsupported
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    BaseGame,
    AddOn,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallCapabilities {
    pub executables_readable: bool,
    pub accepts_new_files: bool,
    pub relocatable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub id: String,
    pub filesystem: String,
    pub supports_hardlinks: bool,
    pub supports_file_clones: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameInstall {
    pub id: InstallId,
    pub runtime: RuntimeIdentity,
    pub kind: InstallKind,
    /// Display/discovery data only; services take an InstallId and a relative
    /// path, never trust this absolute path as permission to access the disk.
    pub location: String,
    /// `None` when the volume could not be inspected. Unknown is never the
    /// same volume as another install, so it never qualifies for hardlinks.
    pub volume: Option<VolumeInfo>,
    pub capabilities: InstallCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreIdentifier {
    pub store: StoreId,
    pub product: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VersionSource {
    Executable { path: RelPath },
    StoreRecord,
    PackageBehaviour,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStrategy {
    Redirect,
    VirtualFileSystem,
}

/// Journaled swaps supplement Redirect; they are not an alternative way to
/// deploy the game's mods. Core holds a game/store resource lock for the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserFileStrategy {
    Redirect,
    VirtualFileSystem,
    JournaledSwap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFileMapping {
    pub source: GamePath,
    pub instance_path: RelPath,
    pub strategy: UserFileStrategy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stores: Vec<StoreId>,
}

impl UserFileMapping {
    pub fn new(source: GamePath, instance_path: RelPath, strategy: UserFileStrategy) -> Self {
        Self {
            source,
            instance_path,
            strategy,
            stores: Vec::new(),
        }
    }

    pub fn with_stores(mut self, stores: Vec<StoreId>) -> Self {
        self.stores = stores;
        self
    }

    pub fn applies_to_store(&self, store: &StoreId) -> bool {
        self.stores.is_empty() || self.stores.contains(store)
    }
}

/// Where a game keeps its saves, and the per-user setting that moves them into an instance's own
/// folder (MASTER_SPEC §26.5, *Saves are the user's choice per instance*).
///
/// The setting is a `key` under `section` of the per-user file `ini`. Its `own_value` names a folder
/// relative to `relative_to`; `{instance}` in it is the instance's save folder name. The shared
/// folder, `shared_dir`, is where the game keeps its saves when the setting is not there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveLocationRule {
    /// The per-user file (an `instance_path`, such as `user/Skyrim.ini`) holding the setting.
    pub ini: RelPath,
    pub section: String,
    pub key: String,
    /// The value that gives the instance its own saves, e.g. `Saves\Agora\{instance}\`.
    pub own_value: String,
    /// The shared save folder, for listing the saves there.
    pub shared_dir: GamePath,
    /// The folder `own_value` is relative to.
    pub relative_to: GamePath,
    /// The stores this rule applies to; empty means every store.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stores: Vec<StoreId>,
}

impl SaveLocationRule {
    pub fn applies_to_store(&self, store: &StoreId) -> bool {
        self.stores.is_empty() || self.stores.contains(store)
    }
}

/// Host-resolved roots. Every `path` is relative to its root and checked by core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "root", rename_all = "snake_case")]
pub enum GamePath {
    /// The calling instance's selected runtime, resolved by the host to its
    /// pinned base or unpinned install through the deployment's visible stack.
    /// Declarative definitions can name this root before an install is chosen.
    Runtime {
        #[serde(default)]
        path: RelPath,
    },
    Install {
        install: InstallId,
        path: RelPath,
    },
    Base {
        base: String,
        path: RelPath,
    },
    Instance {
        path: RelPath,
    },
    Layer {
        layer: LayerId,
        path: RelPath,
    },
    RuntimeComponent {
        component: String,
        path: RelPath,
    },
    /// A downloaded file, including a jar on a Minecraft launch classpath.
    /// The artifact must belong to the calling package's host scope.
    Artifact {
        artifact: String,
    },
    /// Known user folders are resolved by the host, including when a store
    /// install is unreadable or its saves/logs live outside the game folder.
    UserData {
        location: UserDataLocation,
        path: RelPath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserDataLocation {
    Documents,
    RoamingAppData,
    LocalAppData,
    Home,
}

/// Recipes refer to managed paths without inventing a string interpolation
/// language or asking a package for absolute host paths. Core resolves paths
/// and joins path lists with the platform separator (e.g. Java's classpath).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchValue {
    Literal {
        value: String,
    },
    Path {
        path: GamePath,
        prefix: String,
        suffix: String,
    },
    PathList {
        paths: Vec<GamePath>,
        prefix: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecipe {
    pub executable: GamePath,
    pub arguments: Vec<LaunchValue>,
    pub environment: BTreeMap<String, LaunchValue>,
    pub working_directory: GamePath,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentLayout {
    pub data_path: RelPath, // "Data" for Skyrim, "" when content goes in the game root
    pub data_markers: Vec<String>, // globs on top-level names that mean "this is data-folder content"
    pub root_markers: Vec<String>, // globs on top-level names that mean "this is game-root content"
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thunderstore_bepinex: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Suggestion {
    Place {
        source_path: RelPath,
        mount_path: RelPath,
        reason: String,
    },
    Installer {
        reason: String,
    },
    Unknown {
        top_level: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationRule {
    pub source_pattern: String,
    pub destination: RelPath,
    pub content_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameDefinition {
    pub id: GameId,
    pub name: String,
    pub stores: Vec<StoreIdentifier>,
    pub version_sources: Vec<VersionSource>,
    pub deployment: DeploymentStrategy,
    pub content_rules: Vec<ClassificationRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_layout: Option<ContentLayout>,
    pub native_code_patterns: Vec<RelPath>,
    pub framework_ids: Vec<FrameworkId>,
    pub tool_ids: Vec<ToolId>,
    /// Declarative launch recipe for this game, if any.
    ///
    /// `None` means the package's [`GamePackage::prepare_launch`](crate::GamePackage::prepare_launch)
    /// provides the recipe (Minecraft builds its command line from version metadata, so a declarative
    /// recipe would be fiction).
    #[serde(default)]
    pub launch: Option<LaunchRecipe>,
    pub log_paths: Vec<GamePath>,
    pub crash_paths: Vec<GamePath>,
    pub user_files: Vec<UserFileMapping>,
    pub save_paths: Vec<GamePath>,
    pub linked_archive_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared_writes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_list: Option<PluginListRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch_alternatives: Vec<LaunchAlternative>,
    /// Files a framework ships once per game version (MASTER_SPEC §26.6), checked before launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_files: Vec<RuntimeFileRule>,
    /// Where the game keeps its saves and how an instance gets its own (MASTER_SPEC §26.5). Empty
    /// when the game has no such setting; the instance then always shares the game's saves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub save_location: Vec<SaveLocationRule>,
    /// The name Mod Organizer 2 gives this game in its `gameName` setting, such as
    /// `Skyrim Special Edition`. An MO2 import maps a setup to the game through it (MASTER_SPEC
    /// §26.10). Absent when no MO2 setup can be matched to this game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mo2_game_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginListRule {
    pub user_file: RelPath,
    pub plugin_folder: RelPath,
    pub patterns: Vec<String>,
    pub active_prefix: String,
    pub header: Vec<String>,
    /// The load-order rules the game's plugin format obeys. `"creation_engine"` turns on the
    /// masters, light plugin and limit rules of MASTER_SPEC §26.6; any other value is refused
    /// at registration. Absent means the list only activates plugins, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantics: Option<String>,
    /// Plugins the game always loads first, in this order, when they are present in the plugin
    /// folder. They are not written into the instance's list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implicit: Vec<String>,
    /// A file (relative to the game's root) naming more always-loaded plugins, one per line, in
    /// the file's order. Each loads after `implicit`, and only when present in the plugin folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implicit_list_file: Option<RelPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchAlternative {
    pub id: String,
    pub when_present: RelPath,
    pub executable: GamePath,
    pub reason: String,
}

/// A file a framework ships once per game version (MASTER_SPEC §26.6). When the game's files hold
/// some of the family but not the one this runtime needs, the launch is refused with a
/// [`RuntimeFileFinding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFileRule {
    /// Stable identifier, e.g. `address-library`.
    pub id: String,
    /// Display name, e.g. `Address Library for SKSE Plugins`.
    pub name: String,
    /// Case-insensitive glob over deployed paths, `/`-separated, e.g. `Data/SKSE/Plugins/versionlib-*.bin`.
    pub family: String,
    /// The path this runtime needs. `{1}`..`{9}` are the dot-separated components of the runtime
    /// version, `{version}` the whole of it.
    pub expected: String,
    /// Version globs the rule applies to (e.g. `1.6.*`); empty means every version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// What to do, shown with the finding; `{version}` is substituted.
    pub repair: String,
}

/// The most family file names one finding lists.
pub const RUNTIME_FILE_FINDING_MAX_FOUND: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeFileProblem {
    /// Files of the family are present, but not the one this runtime needs.
    WrongVersion,
    /// The rule cannot be evaluated against this runtime. That is a bad definition, and it is
    /// shown as a finding rather than skipped.
    CannotCheck { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFileFinding {
    pub rule_id: String,
    pub rule_name: String,
    /// The path this runtime needs, with the version substituted. For a rule that cannot be
    /// checked, the template as written.
    pub expected: String,
    /// Up to [`RUNTIME_FILE_FINDING_MAX_FOUND`] family files found, by file name.
    pub found: Vec<String>,
    /// What to do, with `{version}` substituted.
    pub repair: String,
    pub problem: RuntimeFileProblem,
}

impl RuntimeFileFinding {
    /// Only a framework built for another version refuses a launch. A rule that cannot be checked
    /// is a definition bug: it is carried as a warning, never a refusal.
    pub fn refuses_launch(&self) -> bool {
        matches!(self.problem, RuntimeFileProblem::WrongVersion)
    }

    /// One line for a person: what is wrong, what was found, and the repair.
    pub fn summary(&self) -> String {
        match &self.problem {
            RuntimeFileProblem::WrongVersion => {
                let found = if self.found.is_empty() {
                    "no file of its family".to_string()
                } else if self.found.len() >= RUNTIME_FILE_FINDING_MAX_FOUND {
                    format!("{}, ...", self.found.join(", "))
                } else {
                    self.found.join(", ")
                };
                format!(
                    "{} ({}) needs {} but the game has {}. Repair: {}",
                    self.rule_name, self.rule_id, self.expected, found, self.repair
                )
            }
            RuntimeFileProblem::CannotCheck { reason } => {
                format!(
                    "{} ({}) cannot be checked: {reason}",
                    self.rule_name, self.rule_id
                )
            }
        }
    }
}

/// A framework rule the game's files satisfy for this runtime: the expected file is there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFilePass {
    pub rule_id: String,
    pub rule_name: String,
    /// The path this runtime needs, with the version substituted.
    pub expected: String,
}

/// Every finding's [`RuntimeFileFinding::summary`], joined for one message.
pub fn describe_runtime_findings(findings: &[RuntimeFileFinding]) -> String {
    findings
        .iter()
        .map(RuntimeFileFinding::summary)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Check `rules` against the runtime `version` and the paths the game will see.
///
/// Pure: `deployed_paths` are `/`-separated paths relative to the game root, and matching is
/// case-insensitive. A rule that does not apply to `version` gives nothing. A rule whose family is
/// absent gives nothing (the framework is not installed, or the game is vanilla), and so does a
/// rule whose expected path is present. A rule that cannot be evaluated gives a
/// [`RuntimeFileProblem::CannotCheck`] finding, never a wrong-version one.
pub fn check_runtime_files<S: AsRef<str>>(
    rules: &[RuntimeFileRule],
    version: &str,
    deployed_paths: &[S],
) -> Vec<RuntimeFileFinding> {
    let mut findings = Vec::new();
    for rule in rules {
        if !rule_applies(rule, version) {
            continue;
        }

        let expected = match expected_runtime_path(rule, version) {
            Ok(expected) => expected,
            Err(reason) => {
                findings.push(cannot_check(rule, &reason));
                continue;
            }
        };

        let mut family_files: Vec<&str> = deployed_paths
            .iter()
            .map(AsRef::as_ref)
            .filter(|path| runtime_path_matches(&rule.family, path))
            .collect();
        if family_files.is_empty() {
            continue;
        }
        if deployed_paths
            .iter()
            .any(|path| fold_path_case(path.as_ref()) == fold_path_case(&expected))
        {
            continue;
        }

        family_files.sort_by_key(|path| fold_path_case(path));
        family_files.dedup_by(|a, b| fold_path_case(a) == fold_path_case(b));
        let found = family_files
            .iter()
            .take(RUNTIME_FILE_FINDING_MAX_FOUND)
            .map(|path| path.rsplit('/').next().unwrap_or(path).to_string())
            .collect();
        findings.push(RuntimeFileFinding {
            rule_id: rule.id.clone(),
            rule_name: rule.name.clone(),
            expected,
            found,
            repair: rule.repair.replace("{version}", version),
            problem: RuntimeFileProblem::WrongVersion,
        });
    }
    findings
}

/// The rules `check_runtime_files` verified for `version` and `deployed_paths`: a rule that
/// applies and whose expected file the game will see. A rule that does not apply, whose expected
/// file is absent, or whose whole family is not installed, is never a pass.
pub fn runtime_file_passes<S: AsRef<str>>(
    rules: &[RuntimeFileRule],
    version: &str,
    deployed_paths: &[S],
) -> Vec<RuntimeFilePass> {
    let mut passes = Vec::new();
    for rule in rules {
        if !rule_applies(rule, version) {
            continue;
        }
        let Ok(expected) = expected_runtime_path(rule, version) else {
            continue;
        };
        if deployed_paths
            .iter()
            .any(|path| fold_path_case(path.as_ref()) == fold_path_case(&expected))
        {
            passes.push(RuntimeFilePass {
                rule_id: rule.id.clone(),
                rule_name: rule.name.clone(),
                expected,
            });
        }
    }
    passes
}

/// Whether `rule`'s `applies_to` patterns cover `version`.
fn rule_applies(rule: &RuntimeFileRule, version: &str) -> bool {
    rule.applies_to.is_empty()
        || rule
            .applies_to
            .iter()
            .any(|pattern| runtime_path_matches(pattern, version))
}

/// The path `rule` expects for `version`, or why the rule cannot say.
fn expected_runtime_path(rule: &RuntimeFileRule, version: &str) -> Result<String, String> {
    if rule.family.trim().is_empty() {
        return Err("its family is empty".to_string());
    }
    if rule.expected.trim().is_empty() {
        return Err("its expected path is empty".to_string());
    }
    expand_runtime_template(&rule.expected, version)
}

fn cannot_check(rule: &RuntimeFileRule, reason: &str) -> RuntimeFileFinding {
    RuntimeFileFinding {
        rule_id: rule.id.clone(),
        rule_name: rule.name.clone(),
        expected: rule.expected.clone(),
        found: Vec::new(),
        repair: rule.repair.clone(),
        problem: RuntimeFileProblem::CannotCheck {
            reason: reason.to_string(),
        },
    }
}

/// Case-insensitive match of a pattern against a path or a version. A pattern without `*` must be
/// equal to the text; one with `*` is matched by [`glob_match`].
fn runtime_path_matches(pattern: &str, text: &str) -> bool {
    let pattern = fold_path_case(pattern);
    let text = fold_path_case(text);
    if pattern.contains('*') {
        glob_match(&pattern, &text)
    } else {
        pattern == text
    }
}

/// Substitute `{1}`..`{9}` (the dot-separated components of `version`) and `{version}` into a
/// runtime file template. Any other placeholder, a missing component or an unbalanced brace is an
/// error that says what is wrong.
fn expand_runtime_template(template: &str, version: &str) -> Result<String, String> {
    if version.trim().is_empty() {
        return Err("the runtime has no version".to_string());
    }
    let components: Vec<&str> = version.split('.').collect();
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close_offset) = rest[open..].find('}') else {
            return Err(format!("'{template}' has a '{{' with no matching '}}'"));
        };
        let close = open + close_offset;
        out.push_str(&rest[..open]);
        let key = &rest[open + 1..close];
        if key == "version" {
            out.push_str(version);
        } else {
            let index = match key.parse::<usize>() {
                Ok(n) if (1..=9).contains(&n) => n,
                _ => {
                    return Err(format!(
                        "'{{{key}}}' is not a placeholder (use {{1}} to {{9}}, or {{version}})"
                    ))
                }
            };
            let Some(component) = components.get(index - 1) else {
                return Err(format!(
                    "runtime version {version} has no component {index}"
                ));
            };
            out.push_str(component);
        }
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("'{template}' has a '}}' with no matching '{{'"));
    }
    out.push_str(rest);
    Ok(out)
}

/// Default copy patterns applied to every game in Links mode (small text files at most 1 MiB).
pub const DEFAULT_COPY_PATTERNS: &[&str] = &[
    "**/*.ini",
    "**/*.cfg",
    "**/*.json",
    "**/*.toml",
    "**/*.xml",
    "**/*.yaml",
    "**/*.yml",
    "**/*.conf",
    "**/*.config",
    "**/*.properties",
];

impl GameDefinition {
    /// Whether native code is legitimate at `path` (relative to the install root).
    /// Runs on the normalized path, rejecting escapes such as `Data/SKSE/Plugins/../../x.dll`.
    pub fn allows_native_code_at(&self, path: &str) -> bool {
        let Ok(rel) = RelPath::new(path) else {
            return false;
        };
        let normalized = rel.as_str();
        self.native_code_patterns
            .iter()
            .any(|pattern| glob_match(pattern.as_str(), normalized))
    }

    /// Whether `path` (relative to the install root) matches a linked archive pattern.
    /// Runs on the normalized path, comparing ASCII case-insensitively.
    pub fn is_linked_archive(&self, path: &str) -> bool {
        let Ok(rel) = RelPath::new(path) else {
            return false;
        };
        let normalized = fold_path_case(rel.as_str());
        self.linked_archive_patterns
            .iter()
            .any(|pattern| glob_match(&fold_path_case(pattern), &normalized))
    }

    /// Whether `path` (relative to the runtime root) matches a declared write pattern.
    /// Runs on the normalized path, comparing ASCII case-insensitively.
    pub fn is_declared_write(&self, path: &str) -> bool {
        let Ok(rel) = RelPath::new(path) else {
            return false;
        };
        let normalized = fold_path_case(rel.as_str());
        self.declared_writes
            .iter()
            .any(|pattern| glob_match(&fold_path_case(pattern), &normalized))
    }

    /// Whether `path` (relative to the runtime root) matches either core default copy patterns
    /// or the game definition's copy patterns. Runs on normalized path, case-insensitively.
    pub fn is_copy_pattern(&self, path: &str) -> bool {
        let Ok(rel) = RelPath::new(path) else {
            return false;
        };
        let normalized = fold_path_case(rel.as_str());
        DEFAULT_COPY_PATTERNS
            .iter()
            .any(|pattern| glob_match(&fold_path_case(pattern), &normalized))
            || self
                .copy_patterns
                .iter()
                .any(|pattern| glob_match(&fold_path_case(pattern), &normalized))
    }

    /// Whether `path` (relative to the install root) matches an excluded path pattern.
    /// Runs on the normalized path, comparing ASCII case-insensitively.
    pub fn is_excluded(&self, path: &str) -> bool {
        let Ok(rel) = RelPath::new(path) else {
            return false;
        };
        let normalized = fold_path_case(rel.as_str());
        self.excluded_paths
            .iter()
            .any(|pattern| glob_match(&fold_path_case(pattern), &normalized))
    }
}

pub fn suggest_placement(files: &[RelPath], layout: &ContentLayout) -> Suggestion {
    let mut current_source_components: Vec<String> = Vec::new();

    for level in 0..=8 {
        let mut files_at_level = BTreeSet::new();
        let mut folders_at_level = BTreeSet::new();

        for f in files {
            let comps: Vec<&str> = f.as_str().split('/').filter(|s| !s.is_empty()).collect();
            if comps.len() <= current_source_components.len() {
                continue;
            }
            let mut prefix_match = true;
            for (sc, fc) in current_source_components.iter().zip(comps.iter()) {
                if fold_path_case(sc) != fold_path_case(fc) {
                    prefix_match = false;
                    break;
                }
            }
            if !prefix_match {
                continue;
            }

            let rem = &comps[current_source_components.len()..];
            if rem.len() == 1 {
                files_at_level.insert(rem[0].to_string());
            } else if rem.len() > 1 {
                folders_at_level.insert(rem[0].to_string());
            }
        }

        let mut top_level_names: Vec<String> = files_at_level
            .iter()
            .cloned()
            .chain(folders_at_level.iter().cloned())
            .collect();
        top_level_names.sort();
        top_level_names.dedup();

        if top_level_names.is_empty() {
            return Suggestion::Unknown { top_level: vec![] };
        }

        let unwrapped_prefix = if current_source_components.is_empty() {
            String::new()
        } else {
            format!("unwrapped `{}/`; ", current_source_components.join("/"))
        };

        let current_source_rel =
            RelPath::new(current_source_components.join("/")).unwrap_or_default();

        // 1. A fomod folder at that level (case-insensitive): Installer
        if folders_at_level
            .iter()
            .any(|f| fold_path_case(f) == fold_path_case("fomod"))
        {
            return Suggestion::Installer {
                reason: format!("{unwrapped_prefix}archive contains a FOMOD installer (`fomod/`)"),
            };
        }

        // 2. Else, if any top-level name equals data_path (when data_path is not empty) or matches a root marker: Place { mount_path: "" }
        let mut root_match: Option<String> = None;
        for name in &top_level_names {
            if !layout.data_path.as_str().is_empty()
                && fold_path_case(name) == fold_path_case(layout.data_path.as_str())
            {
                root_match = Some(name.clone());
                break;
            }
            if layout
                .root_markers
                .iter()
                .any(|m| glob_match(&fold_path_case(m), &fold_path_case(name)))
            {
                root_match = Some(name.clone());
                break;
            }
        }

        if let Some(m) = root_match {
            let reason = if !layout.data_path.as_str().is_empty()
                && fold_path_case(&m) == fold_path_case(layout.data_path.as_str())
            {
                format!("{unwrapped_prefix}`{m}` matches data folder, so it goes in game root")
            } else {
                format!("{unwrapped_prefix}`{m}` is game-root content, so it goes in game root")
            };
            return Suggestion::Place {
                source_path: current_source_rel,
                mount_path: RelPath::default(),
                reason,
            };
        }

        // 3. Else, if any matches a data marker: Place { mount_path: data_path }
        let mut data_match: Option<String> = None;
        for name in &top_level_names {
            if layout
                .data_markers
                .iter()
                .any(|m| glob_match(&fold_path_case(m), &fold_path_case(name)))
            {
                data_match = Some(name.clone());
                break;
            }
        }

        if let Some(m) = data_match {
            let reason = if layout.data_path.as_str().is_empty() {
                format!("{unwrapped_prefix}`{m}` is data-folder content, so it goes in game root")
            } else {
                format!(
                    "{unwrapped_prefix}`{m}` is data-folder content, so it goes in `{}`",
                    layout.data_path.as_str()
                )
            };
            return Suggestion::Place {
                source_path: current_source_rel,
                mount_path: layout.data_path.clone(),
                reason,
            };
        }

        // 4. Else, if the level holds exactly one folder and no files, descend into it and repeat (at most 8 levels)
        if files_at_level.is_empty() && folders_at_level.len() == 1 && level < 8 {
            let only_folder = folders_at_level.into_iter().next().unwrap();
            current_source_components.push(only_folder);
            continue;
        }

        // 5. Else Unknown
        return Suggestion::Unknown {
            top_level: top_level_names,
        };
    }

    Suggestion::Unknown { top_level: vec![] }
}

/// Fold a path the way this platform compares names: a per-character simple uppercase, so a
/// character whose uppercase is more than one character (`ß`) keeps its own form. Windows paths
/// compare case-insensitively; elsewhere this is the identity.
pub fn fold_path_case(path: &str) -> String {
    #[cfg(not(windows))]
    {
        return path.to_string();
    }
    #[cfg(windows)]
    {
        let mut folded = String::with_capacity(path.len());
        for ch in path.chars() {
            let mut upper = ch.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => folded.push(single),
                _ => folded.push(ch),
            }
        }
        folded
    }
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    CompiledGlob::new(pattern).matches(path)
}

/// Characters whose simple uppercase NTFS does **not** treat as the same name, measured on
/// this machine's `D:` volume on 2026-10-31 by creating `x<c>` and asking whether `x<UPPER(c)>`
/// opens the same file. 225 of 1171 measured characters did not fold. Sorted by the first
/// character; `fold_path_case` leaves them alone.
///
/// The measurement is a committed, ignored-by-default test in
/// `crates/agora-game-api/tests/case_fold_ntfs.rs`. NTFS builds this table when a volume is
/// formatted, so another volume may differ; the list is the one measured here.
const NTFS_UNFOLDED: &[(char, char)] = &[
    ('\u{b5}', '\u{39c}'),    // U+00B5 -> U+039C
    ('\u{131}', 'I'),         // U+0131 -> U+0049
    ('\u{17f}', 'S'),         // U+017F -> U+0053
    ('\u{19b}', '\u{a7dc}'),  // U+019B -> U+A7DC
    ('\u{1c5}', '\u{1c4}'),   // U+01C5 -> U+01C4
    ('\u{1c8}', '\u{1c7}'),   // U+01C8 -> U+01C7
    ('\u{1cb}', '\u{1ca}'),   // U+01CB -> U+01CA
    ('\u{1f2}', '\u{1f1}'),   // U+01F2 -> U+01F1
    ('\u{23f}', '\u{2c7e}'),  // U+023F -> U+2C7E
    ('\u{240}', '\u{2c7f}'),  // U+0240 -> U+2C7F
    ('\u{252}', '\u{2c70}'),  // U+0252 -> U+2C70
    ('\u{25c}', '\u{a7ab}'),  // U+025C -> U+A7AB
    ('\u{261}', '\u{a7ac}'),  // U+0261 -> U+A7AC
    ('\u{264}', '\u{a7cb}'),  // U+0264 -> U+A7CB
    ('\u{265}', '\u{a78d}'),  // U+0265 -> U+A78D
    ('\u{266}', '\u{a7aa}'),  // U+0266 -> U+A7AA
    ('\u{26a}', '\u{a7ae}'),  // U+026A -> U+A7AE
    ('\u{26c}', '\u{a7ad}'),  // U+026C -> U+A7AD
    ('\u{282}', '\u{a7c5}'),  // U+0282 -> U+A7C5
    ('\u{287}', '\u{a7b1}'),  // U+0287 -> U+A7B1
    ('\u{29d}', '\u{a7b2}'),  // U+029D -> U+A7B2
    ('\u{29e}', '\u{a7b0}'),  // U+029E -> U+A7B0
    ('\u{345}', '\u{399}'),   // U+0345 -> U+0399
    ('\u{3c2}', '\u{3a3}'),   // U+03C2 -> U+03A3
    ('\u{3d0}', '\u{392}'),   // U+03D0 -> U+0392
    ('\u{3d1}', '\u{398}'),   // U+03D1 -> U+0398
    ('\u{3d5}', '\u{3a6}'),   // U+03D5 -> U+03A6
    ('\u{3d6}', '\u{3a0}'),   // U+03D6 -> U+03A0
    ('\u{3f0}', '\u{39a}'),   // U+03F0 -> U+039A
    ('\u{3f1}', '\u{3a1}'),   // U+03F1 -> U+03A1
    ('\u{3f3}', '\u{37f}'),   // U+03F3 -> U+037F
    ('\u{3f5}', '\u{395}'),   // U+03F5 -> U+0395
    ('\u{525}', '\u{524}'),   // U+0525 -> U+0524
    ('\u{527}', '\u{526}'),   // U+0527 -> U+0526
    ('\u{529}', '\u{528}'),   // U+0529 -> U+0528
    ('\u{52b}', '\u{52a}'),   // U+052B -> U+052A
    ('\u{52d}', '\u{52c}'),   // U+052D -> U+052C
    ('\u{52f}', '\u{52e}'),   // U+052F -> U+052E
    ('\u{10d0}', '\u{1c90}'), // U+10D0 -> U+1C90
    ('\u{10d1}', '\u{1c91}'), // U+10D1 -> U+1C91
    ('\u{10d2}', '\u{1c92}'), // U+10D2 -> U+1C92
    ('\u{10d3}', '\u{1c93}'), // U+10D3 -> U+1C93
    ('\u{10d4}', '\u{1c94}'), // U+10D4 -> U+1C94
    ('\u{10d5}', '\u{1c95}'), // U+10D5 -> U+1C95
    ('\u{10d6}', '\u{1c96}'), // U+10D6 -> U+1C96
    ('\u{10d7}', '\u{1c97}'), // U+10D7 -> U+1C97
    ('\u{10d8}', '\u{1c98}'), // U+10D8 -> U+1C98
    ('\u{10d9}', '\u{1c99}'), // U+10D9 -> U+1C99
    ('\u{10da}', '\u{1c9a}'), // U+10DA -> U+1C9A
    ('\u{10db}', '\u{1c9b}'), // U+10DB -> U+1C9B
    ('\u{10dc}', '\u{1c9c}'), // U+10DC -> U+1C9C
    ('\u{10dd}', '\u{1c9d}'), // U+10DD -> U+1C9D
    ('\u{10de}', '\u{1c9e}'), // U+10DE -> U+1C9E
    ('\u{10df}', '\u{1c9f}'), // U+10DF -> U+1C9F
    ('\u{10e0}', '\u{1ca0}'), // U+10E0 -> U+1CA0
    ('\u{10e1}', '\u{1ca1}'), // U+10E1 -> U+1CA1
    ('\u{10e2}', '\u{1ca2}'), // U+10E2 -> U+1CA2
    ('\u{10e3}', '\u{1ca3}'), // U+10E3 -> U+1CA3
    ('\u{10e4}', '\u{1ca4}'), // U+10E4 -> U+1CA4
    ('\u{10e5}', '\u{1ca5}'), // U+10E5 -> U+1CA5
    ('\u{10e6}', '\u{1ca6}'), // U+10E6 -> U+1CA6
    ('\u{10e7}', '\u{1ca7}'), // U+10E7 -> U+1CA7
    ('\u{10e8}', '\u{1ca8}'), // U+10E8 -> U+1CA8
    ('\u{10e9}', '\u{1ca9}'), // U+10E9 -> U+1CA9
    ('\u{10ea}', '\u{1caa}'), // U+10EA -> U+1CAA
    ('\u{10eb}', '\u{1cab}'), // U+10EB -> U+1CAB
    ('\u{10ec}', '\u{1cac}'), // U+10EC -> U+1CAC
    ('\u{10ed}', '\u{1cad}'), // U+10ED -> U+1CAD
    ('\u{10ee}', '\u{1cae}'), // U+10EE -> U+1CAE
    ('\u{10ef}', '\u{1caf}'), // U+10EF -> U+1CAF
    ('\u{10f0}', '\u{1cb0}'), // U+10F0 -> U+1CB0
    ('\u{10f1}', '\u{1cb1}'), // U+10F1 -> U+1CB1
    ('\u{10f2}', '\u{1cb2}'), // U+10F2 -> U+1CB2
    ('\u{10f3}', '\u{1cb3}'), // U+10F3 -> U+1CB3
    ('\u{10f4}', '\u{1cb4}'), // U+10F4 -> U+1CB4
    ('\u{10f5}', '\u{1cb5}'), // U+10F5 -> U+1CB5
    ('\u{10f6}', '\u{1cb6}'), // U+10F6 -> U+1CB6
    ('\u{10f7}', '\u{1cb7}'), // U+10F7 -> U+1CB7
    ('\u{10f8}', '\u{1cb8}'), // U+10F8 -> U+1CB8
    ('\u{10f9}', '\u{1cb9}'), // U+10F9 -> U+1CB9
    ('\u{10fa}', '\u{1cba}'), // U+10FA -> U+1CBA
    ('\u{10fd}', '\u{1cbd}'), // U+10FD -> U+1CBD
    ('\u{10fe}', '\u{1cbe}'), // U+10FE -> U+1CBE
    ('\u{10ff}', '\u{1cbf}'), // U+10FF -> U+1CBF
    ('\u{13f8}', '\u{13f0}'), // U+13F8 -> U+13F0
    ('\u{13f9}', '\u{13f1}'), // U+13F9 -> U+13F1
    ('\u{13fa}', '\u{13f2}'), // U+13FA -> U+13F2
    ('\u{13fb}', '\u{13f3}'), // U+13FB -> U+13F3
    ('\u{13fc}', '\u{13f4}'), // U+13FC -> U+13F4
    ('\u{13fd}', '\u{13f5}'), // U+13FD -> U+13F5
    ('\u{1c80}', '\u{412}'),  // U+1C80 -> U+0412
    ('\u{1c81}', '\u{414}'),  // U+1C81 -> U+0414
    ('\u{1c82}', '\u{41e}'),  // U+1C82 -> U+041E
    ('\u{1c83}', '\u{421}'),  // U+1C83 -> U+0421
    ('\u{1c84}', '\u{422}'),  // U+1C84 -> U+0422
    ('\u{1c85}', '\u{422}'),  // U+1C85 -> U+0422
    ('\u{1c86}', '\u{42a}'),  // U+1C86 -> U+042A
    ('\u{1c87}', '\u{462}'),  // U+1C87 -> U+0462
    ('\u{1c88}', '\u{a64a}'), // U+1C88 -> U+A64A
    ('\u{1c8a}', '\u{1c89}'), // U+1C8A -> U+1C89
    ('\u{1d8e}', '\u{a7c6}'), // U+1D8E -> U+A7C6
    ('\u{1e9b}', '\u{1e60}'), // U+1E9B -> U+1E60
    ('\u{1fbe}', '\u{399}'),  // U+1FBE -> U+0399
    ('\u{2c5f}', '\u{2c2f}'), // U+2C5F -> U+2C2F
    ('\u{2cec}', '\u{2ceb}'), // U+2CEC -> U+2CEB
    ('\u{2cee}', '\u{2ced}'), // U+2CEE -> U+2CED
    ('\u{2cf3}', '\u{2cf2}'), // U+2CF3 -> U+2CF2
    ('\u{2d27}', '\u{10c7}'), // U+2D27 -> U+10C7
    ('\u{2d2d}', '\u{10cd}'), // U+2D2D -> U+10CD
    ('\u{a661}', '\u{a660}'), // U+A661 -> U+A660
    ('\u{a699}', '\u{a698}'), // U+A699 -> U+A698
    ('\u{a69b}', '\u{a69a}'), // U+A69B -> U+A69A
    ('\u{a791}', '\u{a790}'), // U+A791 -> U+A790
    ('\u{a793}', '\u{a792}'), // U+A793 -> U+A792
    ('\u{a794}', '\u{a7c4}'), // U+A794 -> U+A7C4
    ('\u{a797}', '\u{a796}'), // U+A797 -> U+A796
    ('\u{a799}', '\u{a798}'), // U+A799 -> U+A798
    ('\u{a79b}', '\u{a79a}'), // U+A79B -> U+A79A
    ('\u{a79d}', '\u{a79c}'), // U+A79D -> U+A79C
    ('\u{a79f}', '\u{a79e}'), // U+A79F -> U+A79E
    ('\u{a7a1}', '\u{a7a0}'), // U+A7A1 -> U+A7A0
    ('\u{a7a3}', '\u{a7a2}'), // U+A7A3 -> U+A7A2
    ('\u{a7a5}', '\u{a7a4}'), // U+A7A5 -> U+A7A4
    ('\u{a7a7}', '\u{a7a6}'), // U+A7A7 -> U+A7A6
    ('\u{a7a9}', '\u{a7a8}'), // U+A7A9 -> U+A7A8
    ('\u{a7b5}', '\u{a7b4}'), // U+A7B5 -> U+A7B4
    ('\u{a7b7}', '\u{a7b6}'), // U+A7B7 -> U+A7B6
    ('\u{a7b9}', '\u{a7b8}'), // U+A7B9 -> U+A7B8
    ('\u{a7bb}', '\u{a7ba}'), // U+A7BB -> U+A7BA
    ('\u{a7bd}', '\u{a7bc}'), // U+A7BD -> U+A7BC
    ('\u{a7bf}', '\u{a7be}'), // U+A7BF -> U+A7BE
    ('\u{a7c1}', '\u{a7c0}'), // U+A7C1 -> U+A7C0
    ('\u{a7c3}', '\u{a7c2}'), // U+A7C3 -> U+A7C2
    ('\u{a7c8}', '\u{a7c7}'), // U+A7C8 -> U+A7C7
    ('\u{a7ca}', '\u{a7c9}'), // U+A7CA -> U+A7C9
    ('\u{a7cd}', '\u{a7cc}'), // U+A7CD -> U+A7CC
    ('\u{a7cf}', '\u{a7ce}'), // U+A7CF -> U+A7CE
    ('\u{a7d1}', '\u{a7d0}'), // U+A7D1 -> U+A7D0
    ('\u{a7d3}', '\u{a7d2}'), // U+A7D3 -> U+A7D2
    ('\u{a7d5}', '\u{a7d4}'), // U+A7D5 -> U+A7D4
    ('\u{a7d7}', '\u{a7d6}'), // U+A7D7 -> U+A7D6
    ('\u{a7d9}', '\u{a7d8}'), // U+A7D9 -> U+A7D8
    ('\u{a7db}', '\u{a7da}'), // U+A7DB -> U+A7DA
    ('\u{a7f6}', '\u{a7f5}'), // U+A7F6 -> U+A7F5
    ('\u{ab53}', '\u{a7b3}'), // U+AB53 -> U+A7B3
    ('\u{ab70}', '\u{13a0}'), // U+AB70 -> U+13A0
    ('\u{ab71}', '\u{13a1}'), // U+AB71 -> U+13A1
    ('\u{ab72}', '\u{13a2}'), // U+AB72 -> U+13A2
    ('\u{ab73}', '\u{13a3}'), // U+AB73 -> U+13A3
    ('\u{ab74}', '\u{13a4}'), // U+AB74 -> U+13A4
    ('\u{ab75}', '\u{13a5}'), // U+AB75 -> U+13A5
    ('\u{ab76}', '\u{13a6}'), // U+AB76 -> U+13A6
    ('\u{ab77}', '\u{13a7}'), // U+AB77 -> U+13A7
    ('\u{ab78}', '\u{13a8}'), // U+AB78 -> U+13A8
    ('\u{ab79}', '\u{13a9}'), // U+AB79 -> U+13A9
    ('\u{ab7a}', '\u{13aa}'), // U+AB7A -> U+13AA
    ('\u{ab7b}', '\u{13ab}'), // U+AB7B -> U+13AB
    ('\u{ab7c}', '\u{13ac}'), // U+AB7C -> U+13AC
    ('\u{ab7d}', '\u{13ad}'), // U+AB7D -> U+13AD
    ('\u{ab7e}', '\u{13ae}'), // U+AB7E -> U+13AE
    ('\u{ab7f}', '\u{13af}'), // U+AB7F -> U+13AF
    ('\u{ab80}', '\u{13b0}'), // U+AB80 -> U+13B0
    ('\u{ab81}', '\u{13b1}'), // U+AB81 -> U+13B1
    ('\u{ab82}', '\u{13b2}'), // U+AB82 -> U+13B2
    ('\u{ab83}', '\u{13b3}'), // U+AB83 -> U+13B3
    ('\u{ab84}', '\u{13b4}'), // U+AB84 -> U+13B4
    ('\u{ab85}', '\u{13b5}'), // U+AB85 -> U+13B5
    ('\u{ab86}', '\u{13b6}'), // U+AB86 -> U+13B6
    ('\u{ab87}', '\u{13b7}'), // U+AB87 -> U+13B7
    ('\u{ab88}', '\u{13b8}'), // U+AB88 -> U+13B8
    ('\u{ab89}', '\u{13b9}'), // U+AB89 -> U+13B9
    ('\u{ab8a}', '\u{13ba}'), // U+AB8A -> U+13BA
    ('\u{ab8b}', '\u{13bb}'), // U+AB8B -> U+13BB
    ('\u{ab8c}', '\u{13bc}'), // U+AB8C -> U+13BC
    ('\u{ab8d}', '\u{13bd}'), // U+AB8D -> U+13BD
    ('\u{ab8e}', '\u{13be}'), // U+AB8E -> U+13BE
    ('\u{ab8f}', '\u{13bf}'), // U+AB8F -> U+13BF
    ('\u{ab90}', '\u{13c0}'), // U+AB90 -> U+13C0
    ('\u{ab91}', '\u{13c1}'), // U+AB91 -> U+13C1
    ('\u{ab92}', '\u{13c2}'), // U+AB92 -> U+13C2
    ('\u{ab93}', '\u{13c3}'), // U+AB93 -> U+13C3
    ('\u{ab94}', '\u{13c4}'), // U+AB94 -> U+13C4
    ('\u{ab95}', '\u{13c5}'), // U+AB95 -> U+13C5
    ('\u{ab96}', '\u{13c6}'), // U+AB96 -> U+13C6
    ('\u{ab97}', '\u{13c7}'), // U+AB97 -> U+13C7
    ('\u{ab98}', '\u{13c8}'), // U+AB98 -> U+13C8
    ('\u{ab99}', '\u{13c9}'), // U+AB99 -> U+13C9
    ('\u{ab9a}', '\u{13ca}'), // U+AB9A -> U+13CA
    ('\u{ab9b}', '\u{13cb}'), // U+AB9B -> U+13CB
    ('\u{ab9c}', '\u{13cc}'), // U+AB9C -> U+13CC
    ('\u{ab9d}', '\u{13cd}'), // U+AB9D -> U+13CD
    ('\u{ab9e}', '\u{13ce}'), // U+AB9E -> U+13CE
    ('\u{ab9f}', '\u{13cf}'), // U+AB9F -> U+13CF
    ('\u{aba0}', '\u{13d0}'), // U+ABA0 -> U+13D0
    ('\u{aba1}', '\u{13d1}'), // U+ABA1 -> U+13D1
    ('\u{aba2}', '\u{13d2}'), // U+ABA2 -> U+13D2
    ('\u{aba3}', '\u{13d3}'), // U+ABA3 -> U+13D3
    ('\u{aba4}', '\u{13d4}'), // U+ABA4 -> U+13D4
    ('\u{aba5}', '\u{13d5}'), // U+ABA5 -> U+13D5
    ('\u{aba6}', '\u{13d6}'), // U+ABA6 -> U+13D6
    ('\u{aba7}', '\u{13d7}'), // U+ABA7 -> U+13D7
    ('\u{aba8}', '\u{13d8}'), // U+ABA8 -> U+13D8
    ('\u{aba9}', '\u{13d9}'), // U+ABA9 -> U+13D9
    ('\u{abaa}', '\u{13da}'), // U+ABAA -> U+13DA
    ('\u{abab}', '\u{13db}'), // U+ABAB -> U+13DB
    ('\u{abac}', '\u{13dc}'), // U+ABAC -> U+13DC
    ('\u{abad}', '\u{13dd}'), // U+ABAD -> U+13DD
    ('\u{abae}', '\u{13de}'), // U+ABAE -> U+13DE
    ('\u{abaf}', '\u{13df}'), // U+ABAF -> U+13DF
    ('\u{abb0}', '\u{13e0}'), // U+ABB0 -> U+13E0
    ('\u{abb1}', '\u{13e1}'), // U+ABB1 -> U+13E1
    ('\u{abb2}', '\u{13e2}'), // U+ABB2 -> U+13E2
    ('\u{abb3}', '\u{13e3}'), // U+ABB3 -> U+13E3
    ('\u{abb4}', '\u{13e4}'), // U+ABB4 -> U+13E4
    ('\u{abb5}', '\u{13e5}'), // U+ABB5 -> U+13E5
    ('\u{abb6}', '\u{13e6}'), // U+ABB6 -> U+13E6
    ('\u{abb7}', '\u{13e7}'), // U+ABB7 -> U+13E7
    ('\u{abb8}', '\u{13e8}'), // U+ABB8 -> U+13E8
    ('\u{abb9}', '\u{13e9}'), // U+ABB9 -> U+13E9
    ('\u{abba}', '\u{13ea}'), // U+ABBA -> U+13EA
    ('\u{abbb}', '\u{13eb}'), // U+ABBB -> U+13EB
    ('\u{abbc}', '\u{13ec}'), // U+ABBC -> U+13EC
    ('\u{abbd}', '\u{13ed}'), // U+ABBD -> U+13ED
    ('\u{abbe}', '\u{13ee}'), // U+ABBE -> U+13EE
    ('\u{abbf}', '\u{13ef}'), // U+ABBF -> U+13EF
];

/// The simple (one-to-one) uppercase mapping, or the character itself when its full uppercase is
/// more than one code point.
fn simple_upper(ch: char) -> char {
    let mut upper = ch.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

/// The simple (one-to-one) lowercase mapping, or the character itself when its full lowercase is
/// more than one code point.
fn simple_lower(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

/// A game path with the case Windows' NTFS uses to compare names: per character, the simple
/// uppercase mapping then its simple lowercase, so the result is a lowercase key.
///
/// NTFS keeps a per-volume upcase table and compares names through it. That is Unicode's *simple*
/// uppercase mapping, so `ß` stays `ß` (it is not full case folding to `SS`) and names are not
/// normalised (`é` precomposed and `e` plus a combining accent are different names). The table is
/// volume-specific: [`NTFS_UNFOLDED`] records the characters this machine's NTFS did not fold, and
/// the mapping here follows them. Characters whose mapping is more than one code point have no
/// simple mapping and are left as they are.
///
/// ASCII paths are lowercased exactly as before, so comparisons, records and stored keys keep
/// matching.
pub fn fold_path_case(path: &str) -> String {
    let mut folded = String::with_capacity(path.len());
    for ch in path.chars() {
        if NTFS_UNFOLDED
            .binary_search_by_key(&ch, |&(character, _)| character)
            .is_ok()
        {
            folded.push(ch);
        } else {
            folded.push(simple_lower(simple_upper(ch)));
        }
    }
    folded
}

/// A pattern for [`glob_match`], parsed once for matching many paths: for every `pattern` and `path`,
/// `CompiledGlob::new(pattern).matches(path)` is `glob_match(pattern, path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledGlob {
    /// The pattern as a literal prefix, when it has no `*`: it matches the prefix, or a path under it.
    literal: Option<String>,
    /// The pattern's `/`-separated parts, when it has a `*`.
    parts: Vec<String>,
}

impl CompiledGlob {
    pub fn new(pattern: &str) -> Self {
        if !pattern.contains('*') && !pattern.is_empty() {
            return Self {
                literal: Some(pattern.trim_end_matches('/').to_string()),
                parts: Vec::new(),
            };
        }
        Self {
            literal: None,
            parts: pattern.split('/').map(str::to_string).collect(),
        }
    }

    pub fn matches(&self, path: &str) -> bool {
        if let Some(prefix) = &self.literal {
            return path == prefix
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('/'));
        }
        let path_parts: Vec<&str> = path.split('/').collect();
        match_parts(&self.parts, &path_parts)
    }
}

fn match_parts<P: AsRef<str>>(pattern: &[P], path: &[&str]) -> bool {
    match (pattern.first().map(AsRef::as_ref), path.first()) {
        (None, None) => true,
        (Some("**"), _) => {
            match_parts(&pattern[1..], path)
                || (!path.is_empty() && match_parts(pattern, &path[1..]))
        }
        (Some(pat), Some(p)) => {
            if wildcard_match(pat, p) {
                match_parts(&pattern[1..], &path[1..])
            } else {
                false
            }
        }
        (Some(_), None) => pattern.iter().all(|p| p.as_ref() == "**"),
        (None, Some(_)) => false,
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    // The pieces between the `*`s, read in order without collecting them: this runs for every path
    // and every copy pattern when a plan is made, so an allocation here is paid 200k times.
    let mut pieces = pattern.split('*');
    let Some(first) = pieces.next() else {
        return true;
    };
    if !text.starts_with(first) {
        return false;
    }
    let mut cur = &text[first.len()..];
    while let Some(part) = pieces.next() {
        if pieces.clone().next().is_none() {
            // The last piece anchors the end of the text.
            return cur.ends_with(part);
        }
        if let Some(idx) = cur.find(part) {
            cur = &cur[idx + part.len()..];
        } else {
            return false;
        }
    }
    // A pattern with a `*` has at least two pieces, so the loop returns above.
    false
}

// ---------------------------------------------------------------------------
// Thunderstore Package Support (BepInEx)
// ---------------------------------------------------------------------------

/// The fields of a Thunderstore `manifest.json` that placement needs. Parsing it is the caller's
/// job (core, with a parser that bounds nesting); other fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThunderstoreManifest {
    pub name: String,
    pub version_number: String,
    pub dependencies: Vec<String>,
}

/// Thunderstore's rule for a namespace or package name: ASCII letters, digits and underscores.
/// Anything else (a `/`, a `..`, an empty name) never becomes part of a destination path.
pub fn is_thunderstore_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ThunderstoreMappingError {
    #[error("not a Thunderstore package")]
    NotPackage,
    #[error("destination path collision: {0}")]
    Collision(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

pub fn extract_thunderstore_package_id(
    file_stem_or_name: &str,
    manifest_name: &str,
    version: &str,
) -> String {
    let mut stem = file_stem_or_name;
    if let Some(s) = stem.strip_suffix(".zip") {
        stem = s;
    }
    let version_suffix = format!("-{version}");
    let base = if let Some(prefix) = stem.strip_suffix(&version_suffix) {
        prefix
    } else {
        stem
    };

    let name_suffix = format!("-{manifest_name}");
    if let Some(ns) = base.strip_suffix(&name_suffix) {
        if is_thunderstore_name(ns) {
            return format!("{ns}-{manifest_name}");
        }
    }
    manifest_name.to_string()
}

pub fn map_thunderstore_bepinex(
    paths: &[RelPath],
    pkg: &str,
) -> Result<Vec<(RelPath, RelPath)>, ThunderstoreMappingError> {
    let has_manifest = paths
        .iter()
        .any(|p| fold_path_case(p.as_str()) == fold_path_case("manifest.json"));
    if !has_manifest {
        return Err(ThunderstoreMappingError::NotPackage);
    }
    // The id becomes a folder name: `Namespace-Name` or `Name`, in Thunderstore's characters only.
    if !pkg.split('-').all(is_thunderstore_name) || pkg.split('-').count() > 2 {
        return Err(ThunderstoreMappingError::InvalidPath(format!(
            "'{pkg}' is not a Thunderstore package id"
        )));
    }

    let is_preloader = |parts: &[&str]| {
        parts.len() == 3
            && fold_path_case(parts[0]) == fold_path_case("BepInEx")
            && fold_path_case(parts[1]) == fold_path_case("core")
            && fold_path_case(parts[2]) == fold_path_case("BepInEx.Preloader.dll")
    };
    // A BepInEx pack: the folder holding `BepInEx/core/BepInEx.Preloader.dll`, which is usually a
    // top-level folder (`BepInExPack_Valheim/`) and sometimes the package itself (`Some("")`).
    let mut pack_folder: Option<String> = None;
    for p in paths {
        let parts: Vec<&str> = p.as_str().split('/').filter(|s| !s.is_empty()).collect();
        if is_preloader(&parts) {
            pack_folder = Some(String::new());
            break;
        }
        if parts.len() == 4 && is_preloader(&parts[1..]) {
            pack_folder = Some(parts[0].to_string());
            break;
        }
    }

    let mut mappings = Vec::new();

    if pack_folder.as_deref() == Some("") {
        // The package is the pack: everything but Thunderstore's own top-level files goes to the
        // game root.
        for p in paths {
            let parts: Vec<&str> = p.as_str().split('/').filter(|s| !s.is_empty()).collect();
            let thunderstore_file = parts.len() == 1
                && ["manifest.json", "icon.png", "readme.md", "changelog.md"]
                    .contains(&fold_path_case(parts[0]).as_str());
            if !parts.is_empty() && !thunderstore_file {
                mappings.push((p.clone(), p.clone()));
            }
        }
    } else if let Some(folder) = pack_folder {
        for p in paths {
            let parts: Vec<&str> = p.as_str().split('/').filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                continue;
            }
            if fold_path_case(parts[0]) == fold_path_case(&folder) {
                let rel = parts[1..].join("/");
                if !rel.is_empty() {
                    let dest = RelPath::new(&rel)
                        .map_err(|e| ThunderstoreMappingError::InvalidPath(e.to_string()))?;
                    mappings.push((p.clone(), dest));
                }
            }
        }
    } else {
        for p in paths {
            let parts: Vec<&str> = p.as_str().split('/').filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                continue;
            }
            let top = fold_path_case(parts[0]);
            let dest_str = match top.as_str() {
                "plugins" => {
                    let sub = parts[1..].join("/");
                    if sub.is_empty() {
                        format!("BepInEx/plugins/{pkg}")
                    } else {
                        format!("BepInEx/plugins/{pkg}/{sub}")
                    }
                }
                "patchers" => {
                    let sub = parts[1..].join("/");
                    if sub.is_empty() {
                        format!("BepInEx/patchers/{pkg}")
                    } else {
                        format!("BepInEx/patchers/{pkg}/{sub}")
                    }
                }
                "monomod" => {
                    let sub = parts[1..].join("/");
                    if sub.is_empty() {
                        format!("BepInEx/monomod/{pkg}")
                    } else {
                        format!("BepInEx/monomod/{pkg}/{sub}")
                    }
                }
                "config" => {
                    let sub = parts[1..].join("/");
                    if sub.is_empty() {
                        "BepInEx/config".to_string()
                    } else {
                        format!("BepInEx/config/{sub}")
                    }
                }
                "core" => {
                    let sub = parts[1..].join("/");
                    if sub.is_empty() {
                        "BepInEx/core".to_string()
                    } else {
                        format!("BepInEx/core/{sub}")
                    }
                }
                _ => {
                    format!("BepInEx/plugins/{pkg}/{}", p.as_str())
                }
            };
            let dest = RelPath::new(&dest_str)
                .map_err(|e| ThunderstoreMappingError::InvalidPath(e.to_string()))?;
            mappings.push((p.clone(), dest));
        }
    }

    if mappings.is_empty() {
        return Err(ThunderstoreMappingError::NotPackage);
    }

    let mut seen_lower = std::collections::HashSet::with_capacity(mappings.len());
    for (_, dest) in &mappings {
        let lower = fold_path_case(dest.as_str());
        if !seen_lower.insert(lower) {
            return Err(ThunderstoreMappingError::Collision(format!(
                "equals another destination path case-insensitively: '{dest}'"
            )));
        }
    }

    for (_, dest) in &mappings {
        let parts: Vec<&str> = dest.as_str().split('/').collect();
        for i in 1..parts.len() {
            let prefix = fold_path_case(&parts[..i].join("/"));
            if seen_lower.contains(&prefix) {
                return Err(ThunderstoreMappingError::Collision(format!(
                    "prefix folder '{prefix}' matches another file"
                )));
            }
        }
    }

    Ok(mappings)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameworkDefinition {
    pub id: FrameworkId,
    pub game: GameId,
    pub name: String,
    pub version: String,
    /// Alternatives, not a semver range over an assumed game version scheme.
    pub supported_runtimes: Vec<RuntimeConstraint>,
    pub required_frameworks: Vec<FrameworkId>,
    pub content: Vec<String>,
    pub launch: Option<LaunchRecipe>,
    /// How an instance is checked for this framework and its version (MASTER_SPEC §26.8). Absent
    /// means core cannot tell whether the framework is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<FrameworkDetection>,
}

/// How core finds a framework in an instance: a file at the game root whose presence means the
/// framework is installed, and optionally where its version is read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameworkDetection {
    /// A path relative to the game root, matched case-insensitively, e.g. `skse64_loader.exe`.
    pub marker: RelPath,
    /// Where the version is read from. Absent means presence is all core knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<FrameworkVersionSource>,
}

/// Where a framework's version is read from, given its marker file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameworkVersionSource {
    /// The marker's PE `FileVersion` resource. Its leading zero component is dropped, so the
    /// `0.2.2.6` that SKSE's loader carries reads as `2.2.6`, the version its manifests use.
    PeFileVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFramework {
    pub id: FrameworkId,
    pub version: String,
    /// Alternatives (OR); empty means compatibility evidence was not recorded,
    /// not that every runtime is known compatible (legacy Minecraft migration).
    pub supported_runtimes: Vec<RuntimeConstraint>,
    pub layers: Vec<LayerId>,
}

/// What a tool's successful run produces (MASTER_SPEC §26.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolOutput {
    /// A generated layer: everything the run wrote, kept as the tool's next generation.
    #[default]
    GeneratedLayer,
    /// A whole mod the tool wrote into a folder. A successful run imports that folder as a content
    /// item and points the tool's layer at it. A relative folder is next to the tool's executable;
    /// an absolute one is used as it is.
    Mod {
        folder: String,
        /// The folder is set in the tool's own settings, not by Agora (PGPatcher). A user tool
        /// supplies it, and Agora refuses it when it is inside the game or the deployment.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_configured: bool,
    },
}

impl ToolOutput {
    pub fn is_generated_layer(&self) -> bool {
        matches!(self, ToolOutput::GeneratedLayer)
    }
}

/// A program the game's package declares that reads the installed mods and writes files the game
/// needs (MASTER_SPEC §26.9). Core runs it under the instance's virtual file system and keeps what
/// it wrote as a generated layer.
///
/// The tool reads whatever the instance's stack shows it, so there is no list of input layers. Its
/// output is always a generated layer named by its id, so there is no output layer either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub game: GameId,
    /// What the tool is called to a person, e.g. "Nemesis".
    pub name: String,
    pub launch: LaunchRecipe,
    /// What the tool produces (MASTER_SPEC §26.9). Defaults to a generated layer, so a tool that
    /// writes files for the game keeps the behaviour it had before this field existed.
    #[serde(default, skip_serializing_if = "ToolOutput::is_generated_layer")]
    pub output: ToolOutput,
    /// How a mod-output tool is told where to write, when it takes an argument (DynDOLOD, TexGen:
    /// `-o:"{output}"`). `{output}` is replaced with a folder Agora owns for the run, so no folder
    /// outside Agora's control is touched. Absent for a tool that reads its output folder elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_arg: Option<String>,
    /// INI settings, as `file:section:key` (e.g. `user/Skyrim.ini:General:SLocalSavePath`), whose
    /// values the tool's output depends on. A change makes the output stale.
    #[serde(default)]
    pub relevant_settings: Vec<String>,
    /// Tools whose generated output sits below this tool's: this tool's files win over theirs.
    /// Generated output precedence is independent of plugin load order.
    #[serde(default)]
    pub after_tools: Vec<ToolId>,
    /// Tools whose output would contend with this tool's: two behaviour engines writing the same
    /// behaviour files. Not a refusal; the instance shows a warning when both have enabled output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts_with: Vec<ToolId>,
    /// The tool reads the install path from the game (its registry entry or its own config) and
    /// works on the real install's `Data` folder, so the virtual file system and link capture would
    /// both miss its writes. Such a tool runs by swap (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uses_install_path: bool,
    /// Files the tool writes, as game-relative, case-insensitive glob patterns such as
    /// `Data/meshes/actors/character/**`. A Mod Organizer 2 import uses them to decide which files
    /// in an `overwrite` folder are this tool's output (MASTER_SPEC §26.10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_patterns: Vec<String>,
    /// Game-relative, case-insensitive glob patterns that must each match at least one file in the
    /// output a run would produce: the previous generation, plus the run's writes, minus its
    /// deletions. A run that exits 0 but misses one is discarded (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_outputs: Vec<String>,
    /// Text that marks a failed run even when it exits 0. A run that wrote the file and left the text
    /// in it is discarded (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_markers: Vec<FailureMarker>,
    /// Files the tool writes that must begin with given bytes, such as a packed Havok `.hkx`. A run that
    /// writes one without them is discarded, as for a missing required output (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_magic: Vec<RequiredMagic>,
}

/// A file a tool writes to report a failure, and the text that says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureMarker {
    /// The file, relative to the runtime root (e.g. `Data/Nemesis_Engine/PatchLog.txt`).
    pub file: RelPath,
    /// The text whose presence in that file means the run failed.
    pub contains: String,
}

/// The first bytes a file a tool writes must have (MASTER_SPEC §26.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredMagic {
    /// The file, relative to the runtime root (e.g. `Data/meshes/actors/character/behaviors/0_master.hkx`).
    pub file: RelPath,
    /// The bytes it must start with, as hex pairs with no separators (e.g. `57e0e05710c0c010`).
    pub magic: String,
}

/// This is plugin/module order, distinct from the order of content layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoadOrder {
    pub entries: Vec<LoadOrderEntry>,
    pub rules: Vec<LoadOrderRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOrderEntry {
    pub id: String,
    pub enabled: bool,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadOrderRule {
    Before { first: String, second: String },
    Requires { item: String, master: String },
    Conflicts { first: String, second: String },
}

#[cfg(test)]
mod runtime_file_tests {
    use super::*;

    /// The three rules Skyrim SE declares in `agora-game-creation/data/package.json`.
    fn skyrim_rules() -> Vec<RuntimeFileRule> {
        vec![
            RuntimeFileRule {
                id: "skse".into(),
                name: "Skyrim Script Extender (SKSE)".into(),
                family: "skse64_*.dll".into(),
                expected: "skse64_{1}_{2}_{3}.dll".into(),
                applies_to: vec![],
                repair: "Install the SKSE build for Skyrim {version} from skse.silverlock.org"
                    .into(),
            },
            RuntimeFileRule {
                id: "address-library".into(),
                name: "Address Library for SKSE Plugins".into(),
                family: "Data/SKSE/Plugins/version*-*.bin".into(),
                expected: "Data/SKSE/Plugins/versionlib-{1}-{2}-{3}-{4}.bin".into(),
                applies_to: vec!["1.6.*".into()],
                repair: "Install the Address Library build for Skyrim {version}".into(),
            },
            RuntimeFileRule {
                id: "address-library-se".into(),
                name: "Address Library for SKSE Plugins".into(),
                family: "Data/SKSE/Plugins/version*-*.bin".into(),
                expected: "Data/SKSE/Plugins/version-{1}-{2}-{3}-{4}.bin".into(),
                applies_to: vec!["1.5.*".into()],
                repair: "Install the Address Library build for Skyrim {version}".into(),
            },
        ]
    }

    /// Paths of the working Skyrim SE 1.6.1170.0 instance: the SKSE and Address Library files.
    fn skyrim_1_6_1170_files() -> Vec<&'static str> {
        vec![
            "SkyrimSE.exe",
            "skse64_1_6_1170.dll",
            "skse64_loader.exe",
            "Data/Skyrim.esm",
            "Data/SKSE/Plugins/versionlib-1-6-1170-0.bin",
            "Data/SKSE/Plugins/versionlib-1-6-1170-0-1.bin",
        ]
    }

    fn ids(findings: &[RuntimeFileFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule_id.as_str()).collect()
    }

    #[test]
    fn the_matching_runtime_gives_no_findings() {
        let findings = check_runtime_files(&skyrim_rules(), "1.6.1170.0", &skyrim_1_6_1170_files());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_newer_runtime_names_skse_and_the_address_library_it_lacks() {
        let findings = check_runtime_files(&skyrim_rules(), "1.6.1179.0", &skyrim_1_6_1170_files());
        assert_eq!(ids(&findings), vec!["skse", "address-library"]);

        let skse = &findings[0];
        assert_eq!(skse.expected, "skse64_1_6_1179.dll");
        assert_eq!(skse.found, vec!["skse64_1_6_1170.dll"]);
        assert_eq!(skse.problem, RuntimeFileProblem::WrongVersion);
        assert_eq!(
            skse.repair,
            "Install the SKSE build for Skyrim 1.6.1179.0 from skse.silverlock.org"
        );

        let library = &findings[1];
        assert_eq!(
            library.expected,
            "Data/SKSE/Plugins/versionlib-1-6-1179-0.bin"
        );
        // Sorted by path: "-0-1.bin" sorts before "-0.bin" because '-' is before '.'.
        assert_eq!(
            library.found,
            vec!["versionlib-1-6-1170-0-1.bin", "versionlib-1-6-1170-0.bin"]
        );
    }

    #[test]
    fn the_older_runtime_names_skse_and_the_1_5_address_library() {
        let findings = check_runtime_files(&skyrim_rules(), "1.5.97.0", &skyrim_1_6_1170_files());
        assert_eq!(ids(&findings), vec!["skse", "address-library-se"]);
        assert_eq!(
            findings[1].expected,
            "Data/SKSE/Plugins/version-1-5-97-0.bin"
        );
    }

    #[test]
    fn matching_ignores_case() {
        let files = [
            "SKSE64_1_6_1170.DLL",
            "data/skse/plugins/VERSIONLIB-1-6-1170-0.BIN",
        ];
        assert!(check_runtime_files(&skyrim_rules(), "1.6.1170.0", &files).is_empty());

        let findings = check_runtime_files(&skyrim_rules(), "1.6.1179.0", &files);
        assert_eq!(ids(&findings), vec!["skse", "address-library"]);
        assert_eq!(findings[0].found, vec!["SKSE64_1_6_1170.DLL"]);
    }

    #[test]
    fn no_family_files_gives_no_findings_for_a_vanilla_game() {
        let files = ["SkyrimSE.exe", "Data/Skyrim.esm"];
        assert!(check_runtime_files(&skyrim_rules(), "1.6.1179.0", &files).is_empty());
        let none: [&str; 0] = [];
        assert!(check_runtime_files(&skyrim_rules(), "1.6.1179.0", &none).is_empty());
    }

    #[test]
    fn a_three_part_version_cannot_fill_component_four() {
        let findings = check_runtime_files(&skyrim_rules(), "1.6.1170", &skyrim_1_6_1170_files());
        assert_eq!(findings.len(), 1, "{findings:?}");
        let finding = &findings[0];
        assert_eq!(finding.rule_id, "address-library");
        assert_eq!(
            finding.expected,
            "Data/SKSE/Plugins/versionlib-{1}-{2}-{3}-{4}.bin"
        );
        assert!(finding.found.is_empty());
        match &finding.problem {
            RuntimeFileProblem::CannotCheck { reason } => {
                assert!(reason.contains("component 4"), "{reason}")
            }
            other => panic!("expected CannotCheck, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_family_or_expected_path_is_reported_as_unchecked() {
        let mut rules = skyrim_rules();
        rules[0].family = "  ".into();
        rules[1].expected = String::new();
        let findings = check_runtime_files(&rules, "1.6.1170.0", &skyrim_1_6_1170_files());
        assert_eq!(ids(&findings), vec!["skse", "address-library"]);
        assert!(findings
            .iter()
            .all(|f| matches!(f.problem, RuntimeFileProblem::CannotCheck { .. })));
    }

    #[test]
    fn a_rule_whose_applies_to_excludes_the_version_is_silent() {
        // The game has only the 1.5 library. The 1.5 rule does not apply to a 1.6 runtime, so it
        // says nothing; the 1.6 rule shares the family, finds the 1.5 file and names it.
        let files = ["Data/SKSE/Plugins/version-1-5-97-0.bin"];
        let findings = check_runtime_files(&skyrim_rules(), "1.6.1170.0", &files);
        assert_eq!(ids(&findings), vec!["address-library"]);
        assert_eq!(findings[0].found, vec!["version-1-5-97-0.bin"]);

        // A broken rule that does not apply to this version is not reported either.
        let mut rules = skyrim_rules();
        rules[1].family = String::new();
        rules[1].applies_to = vec!["1.5.*".into()];
        let findings = check_runtime_files(&rules, "1.6.1170.0", &skyrim_1_6_1170_files());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn unknown_placeholders_and_unbalanced_braces_are_reported() {
        let mut rules = skyrim_rules();
        rules[0].expected = "skse64_{0}_{2}.dll".into();
        rules[2].expected = "Data/{version".into();
        let files = [
            "skse64_1_6_1170.dll",
            "Data/SKSE/Plugins/version-1-5-97-0.bin",
        ];
        let findings = check_runtime_files(&rules, "1.5.97.0", &files);
        assert_eq!(ids(&findings), vec!["skse", "address-library-se"]);
        for finding in &findings {
            assert!(matches!(
                finding.problem,
                RuntimeFileProblem::CannotCheck { .. }
            ));
        }
    }

    #[test]
    fn summary_names_the_framework_what_was_found_and_the_repair() {
        let findings = check_runtime_files(&skyrim_rules(), "1.6.1179.0", &skyrim_1_6_1170_files());
        let text = describe_runtime_findings(&findings);
        assert!(text.contains("Skyrim Script Extender (SKSE)"), "{text}");
        assert!(text.contains("skse64_1_6_1170.dll"), "{text}");
        assert!(
            text.contains("Repair: Install the SKSE build for Skyrim 1.6.1179.0"),
            "{text}"
        );
    }

    #[test]
    fn a_finding_lists_at_most_five_family_files() {
        let names: Vec<String> = (0..8)
            .map(|i| format!("Data/SKSE/Plugins/version-{i}.bin"))
            .collect();
        let rules = vec![RuntimeFileRule {
            id: "lib".into(),
            name: "Library".into(),
            family: "Data/SKSE/Plugins/version*.bin".into(),
            expected: "Data/SKSE/Plugins/versionlib-{1}-{2}-{3}-{4}.bin".into(),
            applies_to: vec![],
            repair: "Install it".into(),
        }];
        let findings = check_runtime_files(&rules, "1.6.1179.0", &names);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].found.len(), RUNTIME_FILE_FINDING_MAX_FOUND);
        assert!(findings[0].summary().contains(", ..."));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_matching_keeps_store_build_and_opaque_versions() {
        let mut runtime = RuntimeIdentity {
            game: GameId::new("skyrim").unwrap(),
            store: StoreId::new("steam").unwrap(),
            version: "1.6.1170.0".into(),
            build: Some("12345".into()),
        };
        let constraint = RuntimeConstraint {
            game: GameId::new("skyrim").unwrap(),
            stores: vec![StoreId::new("steam").unwrap()],
            versions: VersionConstraint::Exact(vec!["1.6.1170.0".into()]),
            builds: vec!["12345".into()],
        };
        assert_eq!(constraint.check(&runtime), Support::Supported);
        assert!(constraint.matches(&runtime));

        runtime.store = StoreId::new("gog").unwrap();
        assert_eq!(constraint.check(&runtime), Support::Unsupported);
        assert!(!constraint.matches(&runtime));

        runtime.store = StoreId::new("steam").unwrap();
        runtime.build = None;
        assert_eq!(constraint.check(&runtime), Support::Indeterminate);
        assert!(!constraint.matches(&runtime));

        runtime.build = Some("12345".into());
        runtime.version = "24w14a".into();
        assert_eq!(constraint.check(&runtime), Support::Unsupported);
        assert!(!constraint.matches(&runtime));
    }

    #[test]
    fn semver_is_explicit_and_never_coerces_four_part_versions() {
        let constraint = RuntimeConstraint {
            game: GameId::new("valheim").unwrap(),
            stores: vec![],
            builds: vec![],
            versions: VersionConstraint::Semver(">=0.218.0, <0.219.0".parse().unwrap()),
        };
        let mut runtime = RuntimeIdentity {
            game: GameId::new("valheim").unwrap(),
            store: StoreId::new("community-store").unwrap(),
            version: "0.218.15".into(),
            build: None,
        };
        assert_eq!(constraint.check(&runtime), Support::Supported);

        runtime.version = "0.218.15.0".into();
        // Four-part version cannot parse as semver -> Indeterminate
        assert_eq!(constraint.check(&runtime), Support::Indeterminate);
    }

    #[test]
    fn native_code_checks_reject_path_traversal() {
        let def = GameDefinition {
            mo2_game_name: None,
            id: GameId::new("skyrim").unwrap(),
            name: "Skyrim".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![
                RelPath::new("Data/SKSE/Plugins/*.dll").unwrap(),
                RelPath::new("mods/**/*.dll").unwrap(),
            ],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: Some(LaunchRecipe {
                executable: GamePath::Runtime {
                    path: RelPath::new("SkyrimSE.exe").unwrap(),
                },
                arguments: vec![],
                environment: BTreeMap::new(),
                working_directory: GamePath::Runtime {
                    path: RelPath::default(),
                },
            }),
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            excluded_paths: vec![],
            plugin_list: None,
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        };

        // Normal valid paths
        assert!(def.allows_native_code_at("Data/SKSE/Plugins/plugin.dll"));
        assert!(def.allows_native_code_at("Data\\SKSE\\Plugins\\plugin.dll"));
        assert!(def.allows_native_code_at("mods/sub/nested/mod.dll"));

        // Path traversal attempts must NOT be accepted!
        assert!(!def.allows_native_code_at("Data/SKSE/Plugins/../../x.dll"));
        assert!(!def.allows_native_code_at("Data\\SKSE\\Plugins\\..\\..\\x.dll"));
        assert!(!def.allows_native_code_at("../mods/evil.dll"));
        assert!(!def.allows_native_code_at("C:/evil.dll"));
        assert!(!def.allows_native_code_at("Data/foo.dll"));
    }

    #[test]
    fn linked_archive_matching_is_case_insensitive() {
        let def = GameDefinition {
            mo2_game_name: None,
            id: GameId::new("skyrim-se").unwrap(),
            name: "Skyrim SE".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![
                "Data/*.bsa".into(),
                "Data/*.esm".into(),
                "Data/*.esl".into(),
                "Data/*.bik".into(),
            ],
            declared_writes: vec![],
            excluded_paths: vec![],
            plugin_list: None,
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        };

        // Matches exact and mixed cases
        assert!(def.is_linked_archive("Data/Skyrim - Textures.bsa"));
        assert!(def.is_linked_archive("data/skyrim - textures.BSA"));
        assert!(def.is_linked_archive("DATA\\b.ESM"));
        assert!(def.is_linked_archive("data/c.esl"));
        assert!(def.is_linked_archive("Data/video.bik"));
        assert!(def.is_linked_archive("DATA/VIDEO.BIK"));

        // Non-matches
        assert!(!def.is_linked_archive("SkyrimSE.exe"));
        assert!(!def.is_linked_archive("Data/Sub/d.txt"));
        assert!(!def.is_linked_archive("Data/plugin.dll"));
        // Path traversal rejected
        assert!(!def.is_linked_archive("Data/../../evil.bsa"));
    }

    #[test]
    fn declared_write_matching_is_case_insensitive() {
        let def = GameDefinition {
            mo2_game_name: None,
            id: GameId::new("skyrim-se").unwrap(),
            name: "Skyrim SE".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec!["d3dx9_42.log".into(), "logs/*.log".into()],
            excluded_paths: vec![],
            plugin_list: None,
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        };

        assert!(def.is_declared_write("d3dx9_42.log"));
        assert!(def.is_declared_write("D3DX9_42.LOG"));
        assert!(def.is_declared_write("D3dx9_42.log"));
        assert!(def.is_declared_write("logs/render.log"));
        assert!(def.is_declared_write("LOGS\\RENDER.LOG"));

        assert!(!def.is_declared_write("SkyrimSE.exe"));
        assert!(!def.is_declared_write("Data/d3dx9_42.log"));
        assert!(!def.is_declared_write("../d3dx9_42.log"));
    }

    #[test]
    fn excluded_paths_matching_is_case_insensitive() {
        let def = GameDefinition {
            mo2_game_name: None,
            id: GameId::new("skyrim-se").unwrap(),
            name: "Skyrim SE".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            excluded_paths: vec!["Data/SSEEdit Backups/**".into()],
            plugin_list: None,
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: Vec::new(),
        };

        assert!(def.is_excluded("Data/SSEEdit Backups/x.esm.backup"));
        assert!(def.is_excluded("data/sseedit backups/x.esm.backup"));
        assert!(def.is_excluded("DATA\\SSEEDIT BACKUPS\\SUB\\Y.ESM.BACKUP"));
        assert!(!def.is_excluded("Data/Skyrim.esm"));
        assert!(!def.is_excluded("SkyrimSE.exe"));
    }

    #[test]
    fn copy_patterns_match_defaults_and_custom_case_insensitively() {
        let def = GameDefinition {
            mo2_game_name: None,
            id: GameId::new("valheim").unwrap(),
            name: "Valheim".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            excluded_paths: vec![],
            plugin_list: None,
            runtime_files: Vec::new(),
            save_location: Vec::new(),
            launch_alternatives: Vec::new(),
            copy_patterns: vec!["**/*.dat".into()],
        };

        // Core defaults
        assert!(def.is_copy_pattern("BepInEx/config/BepInEx.cfg"));
        assert!(def.is_copy_pattern("settings.ini"));
        assert!(def.is_copy_pattern("SETTINGS.INI"));
        assert!(def.is_copy_pattern("Mod\\config.json"));
        assert!(def.is_copy_pattern("mod.toml"));
        assert!(def.is_copy_pattern("sub/file.xml"));
        assert!(def.is_copy_pattern("a/b/c.yaml"));
        assert!(def.is_copy_pattern("c.yml"));
        assert!(def.is_copy_pattern("foo.conf"));
        assert!(def.is_copy_pattern("bar.config"));
        assert!(def.is_copy_pattern("server.properties"));

        // Custom game copy_patterns
        assert!(def.is_copy_pattern("data/save.dat"));
        assert!(def.is_copy_pattern("DATA/SAVE.DAT"));

        // Non-matches
        assert!(!def.is_copy_pattern("plugin.dll"));
        assert!(!def.is_copy_pattern("Data/mod.esp"));
    }

    #[test]
    fn test_suggest_placement_skyrim_and_cyberpunk() {
        let skyrim = ContentLayout {
            data_path: RelPath::new("Data").unwrap(),
            data_markers: vec![
                "*.esp".into(),
                "*.esm".into(),
                "*.esl".into(),
                "*.bsa".into(),
                "textures".into(),
                "meshes".into(),
                "scripts".into(),
                "interface".into(),
                "sound".into(),
                "music".into(),
                "skse".into(),
                "strings".into(),
                "video".into(),
                "materials".into(),
                "lodsettings".into(),
                "seq".into(),
                "grass".into(),
                "shadersfx".into(),
                "facegen".into(),
                "lod".into(),
                "terrain".into(),
                "dyndolod".into(),
                "nemesis_engine".into(),
                "calientetools".into(),
                "tools".into(),
                "source".into(),
                "platform".into(),
            ],
            root_markers: vec![
                "*.exe".into(),
                "*.dll".into(),
                "enbseries".into(),
                "enb*.ini".into(),
                "reshade-shaders".into(),
            ],
            thunderstore_bepinex: false,
        };

        // 1. Loose MyMod.esp + textures/a.dds -> Data
        let files = vec![
            RelPath::new("MyMod.esp").unwrap(),
            RelPath::new("textures/a.dds").unwrap(),
        ];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                reason,
            } => {
                assert_eq!(source_path.as_str(), "");
                assert_eq!(mount_path.as_str(), "Data");
                assert!(reason.contains("Data"));
            }
            other => panic!("expected Place, got {other:?}"),
        }

        // 2. Data/MyMod.esp -> root
        let files = vec![RelPath::new("Data/MyMod.esp").unwrap()];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                ..
            } => {
                assert_eq!(source_path.as_str(), "");
                assert_eq!(mount_path.as_str(), "");
            }
            other => panic!("expected Place at root, got {other:?}"),
        }

        // 3. MyMod v1.2/textures/a.dds -> Data from MyMod v1.2
        let files = vec![RelPath::new("MyMod v1.2/textures/a.dds").unwrap()];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                reason,
            } => {
                assert_eq!(source_path.as_str(), "MyMod v1.2");
                assert_eq!(mount_path.as_str(), "Data");
                assert_eq!(
                    reason,
                    "unwrapped `MyMod v1.2/`; `textures` is data-folder content, so it goes in `Data`"
                );
            }
            other => panic!("expected Place, got {other:?}"),
        }

        // 4. skse64_2_02_06/skse64_loader.exe + skse64_2_02_06/Data/Scripts/x.pex -> root from skse64_2_02_06
        let files = vec![
            RelPath::new("skse64_2_02_06/skse64_loader.exe").unwrap(),
            RelPath::new("skse64_2_02_06/Data/Scripts/x.pex").unwrap(),
        ];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                ..
            } => {
                assert_eq!(source_path.as_str(), "skse64_2_02_06");
                assert_eq!(mount_path.as_str(), "");
            }
            other => panic!("expected Place at root, got {other:?}"),
        }

        // 5. two nested wrappers
        let files = vec![RelPath::new("Wrap1/Wrap2/textures/a.dds").unwrap()];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                ..
            } => {
                assert_eq!(source_path.as_str(), "Wrap1/Wrap2");
                assert_eq!(mount_path.as_str(), "Data");
            }
            other => panic!("expected Place, got {other:?}"),
        }

        // 6. TEXTURES/a.dds (case)
        let files = vec![RelPath::new("TEXTURES/a.dds").unwrap()];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Place {
                source_path,
                mount_path,
                ..
            } => {
                assert_eq!(source_path.as_str(), "");
                assert_eq!(mount_path.as_str(), "Data");
            }
            other => panic!("expected Place, got {other:?}"),
        }

        // 7. fomod/ModuleConfig.xml -> Installer
        let files = vec![RelPath::new("fomod/ModuleConfig.xml").unwrap()];
        assert!(matches!(
            suggest_placement(&files, &skyrim),
            Suggestion::Installer { .. }
        ));

        // 8. readme.txt alone -> Unknown
        let files = vec![RelPath::new("readme.txt").unwrap()];
        match suggest_placement(&files, &skyrim) {
            Suggestion::Unknown { top_level } => {
                assert_eq!(top_level, vec!["readme.txt"]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }

        // 9. nine nested wrappers -> Unknown
        let files = vec![RelPath::new("w1/w2/w3/w4/w5/w6/w7/w8/w9/textures/a.dds").unwrap()];
        assert!(matches!(
            suggest_placement(&files, &skyrim),
            Suggestion::Unknown { .. }
        ));

        // Cyberpunk: archive/pc/mod/x.archive -> root
        let cyberpunk = ContentLayout {
            data_path: RelPath::default(),
            data_markers: vec![],
            root_markers: vec![
                "archive".into(),
                "bin".into(),
                "r6".into(),
                "red4ext".into(),
                "engine".into(),
                "mods".into(),
            ],
            thunderstore_bepinex: false,
        };
        let files = vec![RelPath::new("archive/pc/mod/x.archive").unwrap()];
        match suggest_placement(&files, &cyberpunk) {
            Suggestion::Place {
                source_path,
                mount_path,
                ..
            } => {
                assert_eq!(source_path.as_str(), "");
                assert_eq!(mount_path.as_str(), "");
            }
            other => panic!("expected Place at root, got {other:?}"),
        }
    }
    #[test]
    fn path_case_folds_like_windows_simple_lowercase() {
        assert_eq!(fold_path_case("Data/Ü/x"), "data/ü/x");
        assert_eq!(fold_path_case("ü"), "ü");

        // Simple uppercase then simple lowercase, not full case folding: ß has no one-to-one
        // uppercase mapping, so it stays itself.
        assert_eq!(fold_path_case("ß"), "ß");
        assert_ne!(fold_path_case("ß"), fold_path_case("SS"));
        assert_eq!(fold_path_case("ẞ"), "ß");

        // No normalisation: precomposed and decomposed accents are different names.
        assert_ne!(fold_path_case("é"), fold_path_case("e\u{301}"));

        // Measured NTFS exceptions: dotless ı and long ſ do not fold to i and s.
        assert_eq!(fold_path_case("ı"), "ı");
        assert_eq!(fold_path_case("ſ"), "ſ");
        assert_ne!(fold_path_case("ı"), fold_path_case("i"));
        assert_ne!(fold_path_case("ſ"), fold_path_case("s"));
        assert_eq!(fold_path_case("I"), "i");
        assert_eq!(fold_path_case("i"), "i");
        assert_eq!(fold_path_case("İ"), "İ");
        assert_ne!(fold_path_case("İ"), fold_path_case("i"));
    }

    #[test]
    fn ascii_paths_fold_as_they_did_when_ascii_lowercased() {
        let paths = [
            "Data",
            "DATA",
            "data/Meshes/Thing.NIF",
            "Data/Meshes/thing.nif",
            "ReadMe.txt",
        ];
        for left in paths {
            for right in paths {
                assert_eq!(
                    fold_path_case(left) == fold_path_case(right),
                    left.eq_ignore_ascii_case(right),
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    #[test]
    fn empty_and_multibyte_paths_fold_without_panicking() {
        assert_eq!(fold_path_case(""), "");
        assert_eq!(fold_path_case("日本語/файл/😀"), "日本語/файл/😀");
        for path in ["", "é", "ß", "ı", "日本語", "😀", "a/日本語/b"] {
            let _ = fold_path_case(path);
        }
    }
}
