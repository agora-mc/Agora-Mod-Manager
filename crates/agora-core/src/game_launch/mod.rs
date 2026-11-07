//! Generic game launch recipe resolution, launch preparation, process execution,
//! and process watching (MASTER_SPEC §26.3, §26.4, §26.13).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use agora_game_api::{
    check_runtime_files, describe_runtime_findings, Condition, ConditionFacts, FrameworkId,
    GameDefinition, GamePath, LaunchRecipe, LaunchValue, RelPath, RuntimeFileFinding,
    RuntimeFilePass, Truth, UserDataLocation,
};
use serde::{Deserialize, Serialize};

use crate::game_base::{verify_base, BaseManifest, BaseProblem, VerifyDepth};
use crate::game_load_order::{describe_findings, Finding};
use crate::process_identity::{self, ProcessIdentity};
use crate::session_ledger::ExitObservation;

mod vfs;
pub use vfs::{
    launch_under_vfs, locate_dll, locate_dll_x86, log_len, processes_ended_by_vfs,
    processes_ended_since, EndedProcess, VfsLaunch,
};

/// Host-resolved roots for recipe resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRoots {
    pub runtime: PathBuf,
    pub install: Option<PathBuf>,
    pub base: Option<PathBuf>,
}

/// A fully resolved launch configuration ready to be executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<String, OsString>,
    pub cwd: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("unsupported launch root: {0}")]
    UnsupportedRoot(String),
    #[error("program missing or not a file: {path}")]
    ProgramMissing { path: PathBuf },
    #[error(
        "the launch recipe's executable must be inside the game's runtime, base or install folder"
    )]
    ExecutableOutsideGame,
    #[error("game definition has no launch recipe")]
    NoRecipe,
    #[error("unsafe launch recipe: {message}")]
    UnsafeRecipe { message: String },
    #[error("base is damaged ({} problem(s))", problems.len())]
    BaseDamaged { problems: Vec<BaseProblem> },
    /// A framework in the game's files was built for another runtime version (MASTER_SPEC §26.6).
    #[error("frameworks do not match this game's version: {}", describe_runtime_findings(.findings))]
    RuntimeMismatch { findings: Vec<RuntimeFileFinding> },
    /// The plugin list breaks the game's load order rules (MASTER_SPEC §26.6): the game would crash
    /// or refuse to load. `agora games instance plugins sort <instance>` fixes a master's order.
    #[error(
        "the plugin load order would stop the game: {}. `agora games instance plugins sort <instance>` fixes master order",
        describe_findings(.findings)
    )]
    LoadOrderProblems { findings: Vec<Finding> },
    #[error("root '{0}' is not configured")]
    RootNotConfigured(String),
    #[error("user data location '{0:?}' could not be resolved")]
    UserDataNotFound(UserDataLocation),
    #[error("process capture failed: {0}")]
    ProcessCapture(String),
    /// The game was not started: the virtual file system could not be put under it.
    #[error("the virtual file system could not start: {reason}")]
    VfsUnavailable { reason: String },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A prepared launch from a base with warnings (if launched anyway).
#[derive(Debug, Clone)]
pub struct PreparedLaunch {
    pub resolved: ResolvedLaunch,
    pub warnings: Vec<BaseProblem>,
    pub deploy_outcome: Option<crate::game_deploy::DeployOutcome>,
    /// How the instance was deployed, when it was.
    pub deployment: Option<crate::game_deploy::DeployMode>,
    /// The user chose `deployment` (for the instance or for this launch), so it is never
    /// replaced by a fallback.
    pub deployment_chosen: bool,
    /// Set when the game is to run under the virtual file system.
    pub vfs: Option<VfsLaunch>,
    /// Something the user should be told about how this launch will run, such as a step down
    /// from the virtual file system.
    pub notice: Option<String>,
    /// The launch alternative that replaced the recipe's executable, if one did.
    pub alternative: Option<AppliedAlternative>,
    /// Framework findings that `launch_anyway` let through. Empty when the launch was clean.
    pub runtime_findings: Vec<RuntimeFileFinding>,
    /// Load order findings to show the user: the warnings always, and the refusing findings too
    /// when `launch_anyway` let them through. Empty when the load order is clean.
    pub load_order_findings: Vec<Finding>,
    /// Tool output that is stale or unknown (MASTER_SPEC §26.9). Warnings only, never a refusal.
    pub generated_findings: Vec<crate::game_tools::OutputFinding>,
}

impl PreparedLaunch {
    /// A launch with no deployment: the game runs from the folder the recipe names.
    pub fn undeployed(
        resolved: ResolvedLaunch,
        warnings: Vec<BaseProblem>,
        alternative: Option<AppliedAlternative>,
    ) -> Self {
        Self {
            resolved,
            warnings,
            deploy_outcome: None,
            deployment: None,
            deployment_chosen: false,
            vfs: None,
            notice: None,
            alternative,
            runtime_findings: Vec::new(),
            load_order_findings: Vec::new(),
            generated_findings: Vec::new(),
        }
    }
}

/// Refuse a launch whose plugin list breaks the game's load order rules, unless `launch_anyway` is
/// set. Only the refusing findings refuse (a missing or inactive master, a master loop, too many
/// plugins); the others (a late master, an unreadable header, a plugin listed twice) come back as
/// warnings.
/// `findings` come from [`crate::game_load_order::check`] after the plugin list is synced.
pub fn refuse_load_order(
    findings: Vec<Finding>,
    launch_anyway: bool,
) -> Result<Vec<Finding>, LaunchError> {
    let refuses = findings.iter().any(Finding::refuses_launch);
    if refuses && !launch_anyway {
        Err(LaunchError::LoadOrderProblems { findings })
    } else {
        Ok(findings)
    }
}

/// Refuse a launch that has a framework built for another version, unless `launch_anyway` is set.
/// Any other finding (a rule that cannot be checked) never refuses: it comes back as a warning.
/// `findings` come from [`check_runtime_files`] over the files the game will see.
pub fn refuse_runtime_mismatch(
    findings: Vec<RuntimeFileFinding>,
    launch_anyway: bool,
) -> Result<Vec<RuntimeFileFinding>, LaunchError> {
    let refuses = findings.iter().any(RuntimeFileFinding::refuses_launch);
    if refuses && !launch_anyway {
        Err(LaunchError::RuntimeMismatch { findings })
    } else {
        Ok(findings)
    }
}

/// The runtime-file findings for a runtime at `version` over `paths` (the files the game will
/// see). One function for launch and for `games instance check`, so they cannot disagree.
pub fn runtime_findings<S: AsRef<str>>(
    definition: &GameDefinition,
    version: &str,
    paths: &[S],
) -> Vec<RuntimeFileFinding> {
    check_runtime_files(&definition.runtime_files, version, paths)
}

/// The framework rules `paths` satisfy for `version`. The same paths and rules the findings use,
/// so a rule is never reported as both passing and failing.
pub fn runtime_passes<S: AsRef<str>>(
    definition: &GameDefinition,
    version: &str,
    paths: &[S],
) -> Vec<RuntimeFilePass> {
    agora_game_api::runtime_file_passes(&definition.runtime_files, version, paths)
}

/// A launch alternative (a framework's loader) that was used instead of the recipe's own
/// executable, and the reason the game definition gives for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedAlternative {
    pub id: String,
    pub reason: String,
}

/// How a prepared launch is started. The real implementation is [`SystemLauncher`]; tests
/// substitute their own so no process has to be injected.
pub trait Launcher {
    /// Find `agora_vfs.dll`, or say why it cannot be used.
    fn locate_vfs_dll(&self) -> Result<PathBuf, String>;
    /// Start the game: under the virtual file system when `prepared.vfs` is set.
    fn launch(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError>;
    /// Start the game and keep the last part of its stdout and stderr, for a tool run. The
    /// default keeps no output, which is what a game launch wants.
    fn launch_capturing(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        self.launch(prepared)
    }
}

/// Starts real processes.
pub struct SystemLauncher;

impl Launcher for SystemLauncher {
    fn locate_vfs_dll(&self) -> Result<PathBuf, String> {
        locate_dll()
    }

    fn launch(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        launch(prepared)
    }

    fn launch_capturing(&self, prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
        launch_capturing(prepared)
    }
}

/// How much of a process's stdout and stderr is kept, per stream, from the end.
pub const CAPTURED_OUTPUT_LIMIT: usize = 64 * 1024;

/// The last part of a process's stdout and stderr.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedOutput {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}

impl CapturedOutput {
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

/// Reads a child's stdout and stderr while it runs, keeping only the last
/// [`CAPTURED_OUTPUT_LIMIT`] bytes of each so a chatty tool cannot grow the app's memory.
pub struct OutputCapture {
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

impl OutputCapture {
    /// Start reading a child's piped output. `None` when neither stream was piped.
    pub fn start(child: &mut std::process::Child) -> Option<Self> {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if stdout.is_none() && stderr.is_none() {
            return None;
        }
        let stdout_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        if let Some(out) = stdout {
            readers.push(read_into(out, Arc::clone(&stdout_buffer)));
        }
        if let Some(err) = stderr {
            readers.push(read_into(err, Arc::clone(&stderr_buffer)));
        }
        Some(Self {
            stdout: stdout_buffer,
            stderr: stderr_buffer,
            readers,
        })
    }

    /// Wait briefly for the readers, then return what was kept. A reader still open because
    /// something outside the run holds the pipe is left to end on its own.
    pub fn finish(self) -> CapturedOutput {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        for reader in self.readers {
            while !reader.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        CapturedOutput {
            stdout: take_text(&self.stdout),
            stderr: take_text(&self.stderr),
        }
    }
}

fn read_into(
    mut reader: impl Read + Send + 'static,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let Ok(mut kept) = buffer.lock() else {
                        break;
                    };
                    kept.extend_from_slice(&chunk[..read]);
                    if kept.len() > CAPTURED_OUTPUT_LIMIT {
                        let excess = kept.len() - CAPTURED_OUTPUT_LIMIT;
                        kept.drain(..excess);
                    }
                }
            }
        }
    })
}

fn take_text(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
    let Ok(bytes) = buffer.lock() else {
        return String::new();
    };
    String::from_utf8_lossy(&bytes).trim_end().to_string()
}

/// A spawned game process and its captured OS identity.
pub struct LaunchedGame {
    pub child: std::process::Child,
    pub identity: ProcessIdentity,
    pub program: PathBuf,
    /// The child's output, when the launch was asked to keep it.
    pub output: Option<OutputCapture>,
}

impl LaunchedGame {
    pub fn pid(&self) -> u32 {
        self.identity.pid
    }

    /// Finish keeping the output, if this launch kept any.
    pub fn finish_output(&mut self) -> CapturedOutput {
        self.output
            .take()
            .map(OutputCapture::finish)
            .unwrap_or_default()
    }
}

/// A running process whose executable lies inside a target directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunningGameProcess {
    pub pid: u32,
    pub exe: PathBuf,
}

/// Exit and process monitoring summary for a game launch session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExitReport {
    pub processes: Vec<RunningGameProcess>,
    pub relaunched_outside: bool,
    /// How the game's own process ended, or why that could not be seen. Never a guess.
    #[serde(default)]
    pub exit: ExitObservation,
    /// How long after the watch began the game's process was seen to end.
    #[serde(default)]
    pub game_runtime_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Path and recipe resolution
// ---------------------------------------------------------------------------

/// Join a `/`-separated relative path one component at a time, so the result
/// uses the platform's separator throughout.
fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn resolve_game_path(path: &GamePath, roots: &LaunchRoots) -> Result<PathBuf, LaunchError> {
    match path {
        GamePath::Runtime { path } => {
            let rel = path.as_str();
            if rel.is_empty() {
                Ok(roots.runtime.clone())
            } else {
                Ok(join_rel(&roots.runtime, rel))
            }
        }
        GamePath::Base { path, .. } => {
            let Some(base_root) = &roots.base else {
                return Err(LaunchError::RootNotConfigured("base".to_string()));
            };
            let rel = path.as_str();
            if rel.is_empty() {
                Ok(base_root.clone())
            } else {
                Ok(join_rel(base_root, rel))
            }
        }
        GamePath::Install { path, .. } => {
            let Some(install_root) = &roots.install else {
                return Err(LaunchError::RootNotConfigured("install".to_string()));
            };
            let rel = path.as_str();
            if rel.is_empty() {
                Ok(install_root.clone())
            } else {
                Ok(join_rel(install_root, rel))
            }
        }
        GamePath::UserData { location, path } => {
            let Some(base) = crate::game_user_files::user_data_root(location) else {
                return Err(LaunchError::UserDataNotFound(location.clone()));
            };
            let rel = path.as_str();
            if rel.is_empty() {
                Ok(base)
            } else {
                Ok(join_rel(&base, rel))
            }
        }
        GamePath::Instance { .. } => Err(LaunchError::UnsupportedRoot("instance".to_string())),
        GamePath::Layer { .. } => Err(LaunchError::UnsupportedRoot("layer".to_string())),
        GamePath::RuntimeComponent { .. } => Err(LaunchError::UnsupportedRoot(
            "runtime_component".to_string(),
        )),
        GamePath::Artifact { .. } => Err(LaunchError::UnsupportedRoot("artifact".to_string())),
    }
}

/// The instance facts a launch recipe's conditions can ask about, from the roots it resolves
/// against. A framework version or the game version is not visible here, so those stay unknown.
struct LaunchRootsFacts<'a> {
    roots: &'a LaunchRoots,
}

impl ConditionFacts for LaunchRootsFacts<'_> {
    fn present(&self, path: &RelPath) -> Option<bool> {
        let roots = [
            Some(&self.roots.runtime),
            self.roots.install.as_ref(),
            self.roots.base.as_ref(),
        ];
        let mut any = false;
        for root in roots.into_iter().flatten() {
            any = true;
            if join_rel(root, path.as_str()).is_file() {
                return Some(true);
            }
        }
        any.then_some(false)
    }

    fn framework(&self, _framework: &FrameworkId) -> Option<Option<String>> {
        None
    }

    fn game_version(&self) -> Option<String> {
        None
    }
}

/// Whether a recipe or the game's launch alternatives ask something the roots cannot answer: a
/// framework's version or the game's version. A `present` check is answered by the roots, so it
/// does not force a read of the whole instance view.
pub fn needs_instance_facts(
    definition: &GameDefinition,
    recipe: &LaunchRecipe,
    plain: bool,
) -> bool {
    fn asks_about_versions(condition: &Condition) -> bool {
        match condition {
            Condition::Framework { .. } | Condition::Version(_) => true,
            Condition::All(conditions) | Condition::Any(conditions) => {
                conditions.iter().any(asks_about_versions)
            }
            Condition::Not(condition) => asks_about_versions(condition),
            Condition::Present(_) => false,
        }
    }
    let value_asks = |value: &LaunchValue| match value {
        LaunchValue::Conditional { when, .. } => asks_about_versions(when),
        _ => false,
    };
    recipe.arguments.iter().any(value_asks)
        || recipe.environment.values().any(value_asks)
        || (!plain
            && definition
                .launch_alternatives
                .iter()
                .any(|alternative| asks_about_versions(&alternative.when)))
}

/// Whether a condition applies. Only a known true applies; an unknown answer leaves the thing out,
/// as §30.12 says, and `notes` records in plain words that it could not tell.
fn applies(when: &Condition, facts: &dyn ConditionFacts, notes: &mut Vec<String>) -> bool {
    match when.evaluate(facts) {
        Truth::True => true,
        Truth::False => false,
        Truth::Unknown => {
            notes.push(format!(
                "Could not tell whether {} holds, so it was left out: {}.",
                when.describe(),
                "the instance's files or version could not be read"
            ));
            false
        }
    }
}

fn resolve_launch_value(
    val: &LaunchValue,
    roots: &LaunchRoots,
    facts: &dyn ConditionFacts,
    notes: &mut Vec<String>,
) -> Result<OsString, LaunchError> {
    match val {
        LaunchValue::Literal { value } => Ok(OsString::from(value)),
        LaunchValue::Conditional { value, when } => {
            if !applies(when, facts, notes) {
                return Ok(OsString::new());
            }
            resolve_launch_value(value, roots, facts, notes)
        }
        LaunchValue::Path {
            path,
            prefix,
            suffix,
        } => {
            let resolved = resolve_game_path(path, roots)?;
            let mut s = OsString::from(prefix);
            s.push(resolved.as_os_str());
            s.push(suffix);
            Ok(s)
        }
        LaunchValue::PathList { paths, prefix } => {
            #[cfg(windows)]
            const SEP: &str = ";";
            #[cfg(not(windows))]
            const SEP: &str = ":";

            let mut s = OsString::from(prefix);
            for (i, p) in paths.iter().enumerate() {
                let resolved = resolve_game_path(p, roots)?;
                if i > 0 {
                    s.push(SEP);
                }
                s.push(resolved.as_os_str());
            }
            Ok(s)
        }
    }
}

/// Resolve a declarative launch recipe against available roots, with launch alternatives conditions
/// asking only what the roots can tell (`present`).
pub fn resolve_recipe(
    recipe: &LaunchRecipe,
    roots: &LaunchRoots,
) -> Result<ResolvedLaunch, LaunchError> {
    Ok(resolve_recipe_with_facts(recipe, roots, &LaunchRootsFacts { roots })?.0)
}

/// Resolve a declarative launch recipe against `facts` (the instance's merged view, frameworks and
/// version). Returns the resolved launch and plain-word notes about conditions that could not be
/// judged, which the caller writes to the launch log (MASTER_SPEC §30.12).
pub fn resolve_recipe_with_facts(
    recipe: &LaunchRecipe,
    roots: &LaunchRoots,
    facts: &dyn ConditionFacts,
) -> Result<(ResolvedLaunch, Vec<String>), LaunchError> {
    // A game's executable lives in the game's own folders. A definition comes
    // from a package (possibly a community plugin holding `game:define`), and
    // consenting to "define a game" is not consenting to run any program in
    // the user's Documents or AppData.
    if !matches!(
        recipe.executable,
        GamePath::Runtime { .. } | GamePath::Base { .. } | GamePath::Install { .. }
    ) {
        return Err(LaunchError::ExecutableOutsideGame);
    }
    let program = resolve_game_path(&recipe.executable, roots)?;
    if !program.is_file() {
        return Err(LaunchError::ProgramMissing { path: program });
    }

    let cwd = resolve_game_path(&recipe.working_directory, roots)?;
    let mut notes = Vec::new();

    let mut args = Vec::with_capacity(recipe.arguments.len());
    for arg in &recipe.arguments {
        if let LaunchValue::Conditional { value, when } = arg {
            if !applies(when, facts, &mut notes) {
                continue;
            }
            args.push(resolve_launch_value(value, roots, facts, &mut notes)?);
        } else {
            args.push(resolve_launch_value(arg, roots, facts, &mut notes)?);
        }
    }

    let mut env = BTreeMap::new();
    for (k, v) in &recipe.environment {
        if let LaunchValue::Conditional { value, when } = v {
            if !applies(when, facts, &mut notes) {
                continue;
            }
            env.insert(
                k.clone(),
                resolve_launch_value(value, roots, facts, &mut notes)?,
            );
        } else {
            env.insert(
                k.clone(),
                resolve_launch_value(v, roots, facts, &mut notes)?,
            );
        }
    }

    Ok((
        ResolvedLaunch {
            program,
            args,
            env,
            cwd,
        },
        notes,
    ))
}

/// Replace the recipe's executable with the first launch alternative of the game whose
/// `when_present` file exists in the runtime root (a framework's loader, for example).
///
/// Arguments, environment and working directory stay the recipe's, and the executable rule of
/// [`resolve_recipe`] still applies: the program must be a file inside the runtime, base or
/// install folder. An alternative that matches but cannot be started is an error, never a
/// quiet fall back to the plain executable. Callers skip this entirely for a plain launch.
pub fn apply_launch_alternative(
    definition: &GameDefinition,
    roots: &LaunchRoots,
    resolved: &mut ResolvedLaunch,
) -> Result<Option<AppliedAlternative>, LaunchError> {
    apply_launch_alternative_with_facts(
        definition,
        roots,
        resolved,
        &LaunchRootsFacts { roots },
        &mut Vec::new(),
    )
}

/// [`apply_launch_alternative`] with instance facts, and the notes about alternatives whose `when`
/// could not be judged appended to `notes`.
pub fn apply_launch_alternative_with_facts(
    definition: &GameDefinition,
    roots: &LaunchRoots,
    resolved: &mut ResolvedLaunch,
    facts: &dyn ConditionFacts,
    notes: &mut Vec<String>,
) -> Result<Option<AppliedAlternative>, LaunchError> {
    for alternative in &definition.launch_alternatives {
        if !applies(&alternative.when, facts, notes) {
            continue;
        }
        if !matches!(
            alternative.executable,
            GamePath::Runtime { .. } | GamePath::Base { .. } | GamePath::Install { .. }
        ) {
            return Err(LaunchError::ExecutableOutsideGame);
        }
        let program = resolve_game_path(&alternative.executable, roots)?;
        if !program.is_file() {
            return Err(LaunchError::ProgramMissing { path: program });
        }
        resolved.program = program;
        return Ok(Some(AppliedAlternative {
            id: alternative.id.clone(),
            reason: alternative.reason.clone(),
        }));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Base launch preparation
// ---------------------------------------------------------------------------

/// Prepare a launch from a pinned base, verifying base integrity and setting up store environment.
pub fn prepare_base_launch(
    manifest: &BaseManifest,
    definition: &GameDefinition,
    launch_anyway: bool,
) -> Result<PreparedLaunch, LaunchError> {
    prepare_base_launch_with(manifest, definition, launch_anyway, false)
}

/// Like [`prepare_base_launch`]; `plain` skips the game's launch alternatives and starts
/// the recipe's own executable.
pub fn prepare_base_launch_with(
    manifest: &BaseManifest,
    definition: &GameDefinition,
    launch_anyway: bool,
    plain: bool,
) -> Result<PreparedLaunch, LaunchError> {
    let roots = LaunchRoots {
        runtime: manifest.location.clone(),
        install: Some(manifest.source_location.clone()),
        base: Some(manifest.location.clone()),
    };
    let mut notes = Vec::new();
    let prepared = prepare_base_launch_with_facts(
        manifest,
        definition,
        launch_anyway,
        plain,
        &LaunchRootsFacts { roots: &roots },
        &mut notes,
    )?;
    Ok(prepared)
}

/// [`prepare_base_launch_with`] with the instance facts a conditional argument asks about, and the
/// notes about conditions that could not be judged appended to `notes`.
pub fn prepare_base_launch_with_facts(
    manifest: &BaseManifest,
    definition: &GameDefinition,
    launch_anyway: bool,
    plain: bool,
    facts: &dyn ConditionFacts,
    notes: &mut Vec<String>,
) -> Result<PreparedLaunch, LaunchError> {
    let Some(recipe) = &definition.launch else {
        return Err(LaunchError::NoRecipe);
    };

    let ver = verify_base(
        manifest,
        VerifyDepth::Quick,
        &|p| definition.is_declared_write(p),
        &|p| definition.is_excluded(p),
    );
    if !ver.problems.is_empty() && !launch_anyway {
        return Err(LaunchError::BaseDamaged {
            problems: ver.problems,
        });
    }
    let warnings = ver.problems;

    let base_paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let findings = refuse_runtime_mismatch(
        runtime_findings(definition, &manifest.runtime.version, &base_paths),
        launch_anyway,
    )?;

    let roots = LaunchRoots {
        runtime: manifest.location.clone(),
        install: Some(manifest.source_location.clone()),
        base: Some(manifest.location.clone()),
    };

    let (mut resolved, recipe_notes) = resolve_recipe_with_facts(recipe, &roots, facts)?;
    notes.extend(recipe_notes);
    let alternative = if plain {
        None
    } else {
        apply_launch_alternative_with_facts(definition, &roots, &mut resolved, facts, notes)?
    };

    // Store launch environment:
    // for runtime.store == "steam", set SteamAppId and SteamGameId to the store product.
    if manifest.runtime.store.as_str() == "steam" {
        let product = manifest.source_product.as_deref().or_else(|| {
            definition
                .stores
                .iter()
                .find(|s| s.store == manifest.runtime.store)
                .map(|s| s.product.as_str())
        });
        if let Some(prod) = product {
            resolved
                .env
                .insert("SteamAppId".to_string(), OsString::from(prod));
            resolved
                .env
                .insert("SteamGameId".to_string(), OsString::from(prod));
        }
    }

    let mut prepared = PreparedLaunch::undeployed(resolved, warnings, alternative);
    prepared.runtime_findings = findings;
    Ok(prepared)
}

// ---------------------------------------------------------------------------
// Spawning and process watching
// ---------------------------------------------------------------------------

/// Spawn the game process according to a prepared launch, under the virtual file system when the
/// preparation asked for it.
pub fn launch(prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
    spawn(prepared, false)
}

/// Like [`launch`], but keeps the last part of the process's stdout and stderr. Used for tool
/// runs, whose output is what tells the player why a tool failed.
pub fn launch_capturing(prepared: &PreparedLaunch) -> Result<LaunchedGame, LaunchError> {
    spawn(prepared, true)
}

fn spawn(prepared: &PreparedLaunch, capture: bool) -> Result<LaunchedGame, LaunchError> {
    if let Some(vfs) = &prepared.vfs {
        return launch_under_vfs(&prepared.resolved, vfs, capture);
    }
    let mut cmd = std::process::Command::new(&prepared.resolved.program);
    cmd.args(&prepared.resolved.args);
    cmd.current_dir(&prepared.resolved.cwd);
    cmd.envs(&prepared.resolved.env);
    if capture {
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
    } else {
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
    }

    // The game and everything it starts join a job object Agora made, so Stop can end a game
    // whose launcher has already exited and left only a child (`skse64_loader.exe` starting
    // `SkyrimSE.exe`). See `process_job`.
    let (mut child, job) = crate::process_job::spawn_in_job(&mut cmd).map_err(LaunchError::Io)?;
    let output = if capture {
        OutputCapture::start(&mut child)
    } else {
        None
    };
    let pid = child.id();
    let mut identity =
        process_identity::capture(pid).map_err(|e| LaunchError::ProcessCapture(format!("{e}")))?;
    identity.job = job;

    Ok(LaunchedGame {
        child,
        identity,
        program: prepared.resolved.program.clone(),
        output,
    })
}

fn clean_path_for_comparison(p: &Path) -> PathBuf {
    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let s = canon.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        canon
    }
}

/// Check whether `path` is inside `dir` (canonicalised, case-insensitive on Windows).
pub fn is_subpath(path: &Path, dir: &Path) -> bool {
    let norm_path = clean_path_for_comparison(path);
    let norm_dir = clean_path_for_comparison(dir);

    #[cfg(windows)]
    {
        let p_str = norm_path.to_string_lossy().replace('\\', "/");
        let p_str = agora_game_api::fold_path_case(&p_str);
        let d_str = norm_dir.to_string_lossy().replace('\\', "/");
        let mut d_str = agora_game_api::fold_path_case(&d_str);
        if !d_str.ends_with('/') {
            d_str.push('/');
        }
        p_str.starts_with(&d_str)
    }
    #[cfg(not(windows))]
    {
        norm_path.starts_with(&norm_dir)
    }
}

/// List all running processes whose executable path lies inside `dir`.
pub fn processes_running_from(dir: &Path) -> Vec<RunningGameProcess> {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes();
    let mut running = Vec::new();
    for (pid, process) in sys.processes() {
        if let Some(exe) = process.exe() {
            if is_subpath(exe, dir) {
                running.push(RunningGameProcess {
                    pid: pid.as_u32(),
                    exe: exe.to_path_buf(),
                });
            }
        }
    }
    running.sort_by_key(|p| p.pid);
    running
}

/// The image names whose exit decides a session: the game's declared `session.processes`, else
/// the recipe's own executable. A framework loader (`skse64_loader.exe`) replaces the recipe's
/// executable at launch and exits at once, so it is never the right thing to watch.
pub fn session_images(definition: &GameDefinition, launched_program: &Path) -> Vec<String> {
    if !definition.session.processes.is_empty() {
        return definition.session.processes.clone();
    }
    let recipe_exe = definition
        .launch
        .as_ref()
        .and_then(|recipe| match &recipe.executable {
            GamePath::Runtime { path }
            | GamePath::Install { path, .. }
            | GamePath::Base { path, .. }
            | GamePath::Instance { path }
            | GamePath::Layer { path, .. }
            | GamePath::RuntimeComponent { path, .. } => {
                path.as_str().rsplit('/').next().map(str::to_string)
            }
            _ => None,
        })
        .filter(|name| !name.is_empty());
    recipe_exe
        .or_else(|| {
            launched_program
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .into_iter()
        .collect()
}

fn image_matches(a: &str, b: &str) -> bool {
    agora_game_api::fold_path_case(a) == agora_game_api::fold_path_case(b)
}

/// A process of the game that was seen to end.
struct EndedGame {
    pid: u32,
    image: String,
    code: u32,
    at_ms: u64,
}

/// How the process Agora spawned ended.
struct DirectEnd {
    description: String,
}

/// Watches for the game's own process and keeps its exit code.
///
/// The process Agora spawns is not always the game: a framework loader starts the game and exits.
/// The game is found by image name among the processes of the launch's own job (Windows), held by
/// handle (which also protects against a recycled pid), and its exit code is read when it ends.
/// Where the spawned process itself is the game, its own status is the observation.
struct GameWatch {
    images: Vec<String>,
    direct_is_game: bool,
    direct: Option<DirectEnd>,
    held: Vec<crate::process_job::HeldProcess>,
    ended: Vec<EndedGame>,
    /// Pids already held or ended, never held again (a recycled pid is a stranger).
    seen_pids: Vec<u32>,
    started: std::time::Instant,
}

impl GameWatch {
    fn new(images: Vec<String>, launched: &LaunchedGame) -> Self {
        let program_name = launched
            .program
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let direct_is_game = images
            .iter()
            .any(|image| image_matches(image, program_name));
        Self {
            images,
            direct_is_game,
            direct: None,
            held: Vec::new(),
            ended: Vec::new(),
            seen_pids: vec![launched.pid()],
            started: std::time::Instant::now(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// The spawned process's end, once it has one.
    fn note_direct(
        &mut self,
        launched: &LaunchedGame,
        status: std::io::Result<std::process::ExitStatus>,
    ) {
        let (code, description) = match status {
            Ok(status) => match status.code() {
                // Windows hands back the exit code as an i32; the bits are the NTSTATUS.
                Some(code) => (Some(code as u32), format!("with exit code {}", code as u32)),
                None => (None, describe_signal(&status)),
            },
            Err(e) => (None, format!("and its status could not be read ({e})")),
        };
        if self.direct_is_game {
            if let Some(code) = code {
                let image = launched
                    .program
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.ended.push(EndedGame {
                    pid: launched.pid(),
                    image,
                    code,
                    at_ms: self.elapsed_ms(),
                });
            }
        }
        self.direct = Some(DirectEnd { description });
    }

    /// One poll: find new game processes in the job, and note the ones that have ended.
    fn poll_job(&mut self, job: Option<&str>) {
        if let Some(job) = job {
            for process in
                crate::process_job::hold_game_processes(job, &self.images, &self.seen_pids)
            {
                self.seen_pids.push(process.pid());
                self.held.push(process);
            }
        }
        let at_ms = self.elapsed_ms();
        let mut still = Vec::new();
        for process in std::mem::take(&mut self.held) {
            match process.exit_code() {
                Some(code) => self.ended.push(EndedGame {
                    pid: process.pid(),
                    image: process.image().to_string(),
                    code,
                    at_ms,
                }),
                None => still.push(process),
            }
        }
        self.held = still;
    }

    /// Whether a game process Agora holds is still running.
    fn game_running(&self) -> bool {
        !self.held.is_empty()
    }

    /// The observation: the game process that ended last, or why none could be read.
    fn finish(self, job: Option<&str>, relaunched_outside: bool) -> (ExitObservation, Option<u64>) {
        if let Some(last) = self.ended.iter().max_by_key(|ended| ended.at_ms) {
            return (
                ExitObservation::Code {
                    pid: last.pid,
                    image: last.image.clone(),
                    code: last.code,
                },
                Some(last.at_ms),
            );
        }
        let names = self.images.join(" or ");
        let loader = self
            .direct
            .as_ref()
            .map(|end| format!("the launched program ended {}", end.description))
            .unwrap_or_else(|| "the launched program's end was not seen".to_string());
        let mut reason = if self.direct_is_game {
            // The spawned process is the game, and it ended without a code.
            format!("the game ended without a readable exit code ({loader})")
        } else if job.is_some() {
            format!(
                "no process named {names} was seen running in the launch's job ({loader}), so the game's exit code could not be read"
            )
        } else {
            format!(
                "this launch has no job object, so the exit code of {names} could not be read ({loader})"
            )
        };
        if relaunched_outside {
            reason.push_str("; a process with the game's name also ran outside Agora's job (a store relaunch), and its exit is not observed");
        }
        (ExitObservation::unavailable(reason), None)
    }
}

#[cfg(unix)]
fn describe_signal(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(signal) => format!("by signal {signal}, which has no exit code"),
        None => "without an exit code".to_string(),
    }
}

#[cfg(not(unix))]
fn describe_signal(_status: &std::process::ExitStatus) -> String {
    "without an exit code".to_string()
}

/// Wait for a launched game to exit and ensure no processes are running from `dir` for `grace`.
/// The game is taken to be the executable that was launched; see [`wait_for_exit_watching`] for a
/// game whose launcher exits first.
pub fn wait_for_exit(
    dir: &Path,
    launched: &mut LaunchedGame,
    poll: Duration,
    grace: Duration,
) -> SessionExitReport {
    let images: Vec<String> = launched
        .program
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .into_iter()
        .collect();
    wait_for_exit_watching(dir, launched, &images, poll, grace)
}

/// [`wait_for_exit`], also keeping the exit code of the game's own process: the process named by
/// one of `game_images` that is the launched process itself or runs in its job. The session ends
/// only when the launched process is gone, nothing runs from `dir` and no game process Agora holds
/// is still running, all for `grace`.
pub fn wait_for_exit_watching(
    dir: &Path,
    launched: &mut LaunchedGame,
    game_images: &[String],
    poll: Duration,
    grace: Duration,
) -> SessionExitReport {
    let main_exe_name = launched
        .program
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let mut watch = GameWatch::new(game_images.to_vec(), launched);
    let job = launched.identity.job.clone();

    let mut seen_processes: Vec<RunningGameProcess> = Vec::new();
    if is_subpath(&launched.program, dir) {
        seen_processes.push(RunningGameProcess {
            pid: launched.pid(),
            exe: launched.program.clone(),
        });
    }

    let mut relaunched_outside = false;
    let mut child_exited = false;
    let mut last_activity = std::time::Instant::now();

    loop {
        // 1. Check child status
        if !child_exited {
            match launched.child.try_wait() {
                Ok(Some(status)) => {
                    child_exited = true;
                    watch.note_direct(launched, Ok(status));
                    last_activity = std::time::Instant::now();
                }
                Ok(None) => {}
                Err(e) => {
                    child_exited = true;
                    watch.note_direct(launched, Err(e));
                    last_activity = std::time::Instant::now();
                }
            }
        }

        // 1b. Hold the game's own process once it appears, and read the ones that have ended.
        watch.poll_job(job.as_deref());

        // 2. Poll processes on system
        let mut sys = sysinfo::System::new();
        sys.refresh_processes();

        let mut current_in_dir_count = 0;
        for (pid, process) in sys.processes() {
            if let Some(exe) = process.exe() {
                if is_subpath(exe, dir) {
                    current_in_dir_count += 1;
                    let entry = RunningGameProcess {
                        pid: pid.as_u32(),
                        exe: exe.to_path_buf(),
                    };
                    if !seen_processes.contains(&entry) {
                        seen_processes.push(entry);
                    }
                } else if !main_exe_name.is_empty()
                    // Only a process started by this launch counts: a copy of the
                    // game the user already had running elsewhere is not a relaunch.
                    && process.start_time() >= launched.identity.start_time
                {
                    if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
                        #[cfg(windows)]
                        let matches_name = agora_game_api::fold_path_case(name)
                            == agora_game_api::fold_path_case(&main_exe_name);
                        #[cfg(not(windows))]
                        let matches_name = name == main_exe_name;
                        if matches_name {
                            relaunched_outside = true;
                        }
                    }
                }
            }
        }

        if current_in_dir_count > 0 || !child_exited || watch.game_running() {
            last_activity = std::time::Instant::now();
        }

        // 3. Exit condition: child exited AND no processes in dir for >= grace
        if child_exited && current_in_dir_count == 0 && last_activity.elapsed() >= grace {
            break;
        }

        std::thread::sleep(poll);
    }

    seen_processes.sort_by_key(|p| p.pid);

    let (exit, game_runtime_ms) = watch.finish(job.as_deref(), relaunched_outside);
    SessionExitReport {
        processes: seen_processes,
        relaunched_outside,
        exit,
        game_runtime_ms,
    }
}
