//! Tools and the generated layers they write (MASTER_SPEC §26.9).
//!
//! A game's package declares tools: programs that read the installed mods and write files the game
//! needs, such as Nemesis's behaviour files. Core runs a tool under the instance's virtual file
//! system, so everything it writes lands in a staging folder, never in a mod or in the game's own
//! writable layer. A run that exits 0 and passes its output checks is promoted: its folder becomes
//! the tool's next generation, and the instance's generated layer for the tool points at it. Any
//! other run is discarded and the current generation stays in effect.
//!
//! On disk, a tool's output lives in `<instance>/generated/<tool>/`:
//! - `<n>/` is a generation: exactly what one successful run wrote, with its deletions kept as
//!   whiteout markers in the layer's manifest entry;
//! - `staging-<run>/` exists only while a run is going;
//! - `failed-<run>/` is a discarded run, the newest few kept for diagnostics;
//! - `state.json` names the previous generation, so rollback and diff can reach it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use agora_game_api::{
    fold_path_case, glob_match, BaseReference, BeforeLaunchMode, DeploymentStrategy,
    GameDefinition, GamePath, InputFingerprint, Layer, LayerId, LayerSource, LayerStack, RelPath,
    StoreId, ToolDefinition, ToolId, ToolInputKind, ToolOutput, ToolScope,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::event_sink::CancellationToken;
use crate::game_base::BaseManifest;
use crate::game_deploy::{
    compare_farm, deploy_locked, deploy_locked_with_copy, discard_deployment, farm_file_differs,
    read_deployment_record, DeployMode, DeploymentRecord, FarmChange, Placement,
};
use crate::game_ini;
use crate::game_instance::GameInstanceManifest;
use crate::game_launch::{
    self, LaunchError, LaunchRoots, LaunchedGame, Launcher, PreparedLaunch, VfsLaunch,
};
use crate::game_load_order;
use crate::game_tool_copy::{self, CopyKind, SwapCosts};
use crate::game_tool_swap::{self, SwapError, SwapJournal};
use crate::game_user_files::{self, UserFilesError};
use crate::game_user_tools::UserTool;
use crate::install_watch::{InstallChanges, InstallWatch};
use crate::lock_manager::LockResource;

/// Failed runs kept in `failed-<run>` folders, the newest first (MASTER_SPEC §26.9).
pub const FAILED_RUNS_KEPT: usize = 3;
/// Written paths a run reports in full; the rest are counted.
pub const REPORTED_PATHS: usize = 20;
/// The largest file a failure marker is searched in: a bigger file is not searched (MASTER_SPEC §26.9).
pub const MARKER_SEARCH_LIMIT: u64 = 16 * 1024 * 1024;

pub(crate) const WHITEOUT_DIR: &str = ".agvfs-wh";
const WHITEOUT_SUFFIX: &str = ".wh";
const STATE_FILE: &str = "state.json";
/// The generation a would-be output is planned under. It names no folder: the staging folder is used.
const WOULD_BE_GENERATION: &str = "would-be";
const VFS_CONFIG: &str = "tool-config.json";
const POLL: Duration = Duration::from_millis(250);
/// How long no process may run from a tool's folder before its run counts as over.
const QUIET: Duration = Duration::from_secs(1);
/// The consecutive discarded runs after which a tool's failure offers something (MASTER_SPEC §26.9).
const OFFER_AFTER_DISCARDS: u32 = 2;
/// The consecutive discarded runs of a tool, in its folder. A missing or unreadable file counts as none.
const FAILURES_FILE: &str = "failures.json";

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("instance '{0}' not found")]
    InstanceNotFound(String),
    #[error("instance '{0}' runs from its install folder, so it has no deployed folder for a tool to run in")]
    Unpinned(String),
    #[error("game '{game}' declares no tool '{tool}'")]
    UnknownTool { game: String, tool: String },
    #[error("tool '{0}' has no output in this instance")]
    NoOutput(String),
    #[error("tool '{0}' has no earlier generation to compare with or roll back to")]
    NoPrevious(String),
    /// A tool asks for a value the player has not filled in (MASTER_SPEC §30.12).
    #[error("{tool} needs '{label}' ({input}) filled in before it can run")]
    MissingInput {
        tool: String,
        input: String,
        label: String,
    },
    /// A stored value is not one of the input's choices, or is not the kind it declares.
    #[error("{tool} input '{input}' is not valid: {reason}")]
    InvalidInput {
        tool: String,
        input: String,
        reason: String,
    },
    /// An item-scoped tool needs the content item it should run for.
    #[error("{tool} runs once per content item, but no item was chosen")]
    ItemScopeWithoutItem { tool: String },
    /// A mod-output tool's folder already holds files. Nothing ran: the user has to choose between
    /// moving it aside and cancelling (MASTER_SPEC §26.9).
    #[error("{tool} would write into '{folder}', but that folder already has files. Move it aside to '{aside}' and run again, or cancel.")]
    OutputFolderNotEmpty {
        tool: String,
        folder: String,
        aside: String,
    },
    #[error("the game is running from instance '{0}'; close it before running a tool")]
    SessionRunning(String),
    /// The virtual file system could not start, so nothing ran (MASTER_SPEC §26.9). `next` is the tier
    /// the run can be captured by instead: `Links` for a tool that uses its own folder, `Swap` for one
    /// that works on the real install. Nothing switches to it without the user's answer. The refusal is
    /// a failure of the tool too: it counts toward the offers (MASTER_SPEC §26.9, decision 3), and `offers`
    /// carries the ones the count has reached.
    #[error("{}", vfs_unavailable_message(.reason, .next))]
    VfsUnavailable {
        reason: String,
        next: Option<CaptureMode>,
        offers: Vec<ToolOffer>,
    },
    /// A run captured from links found a linked file changed. A link is the game's copy of a mod's
    /// own file and is never written in place, so the run is discarded and the paths are named.
    #[error("{tool} changed linked file(s) in place: {}. A linked file is never written, so the run was discarded", paths.join(", "))]
    LinkedFileChanged {
        tool: String,
        paths: Vec<String>,
        failed_folder: Option<PathBuf>,
    },
    /// The tool works on the real install's `Data` folder, and this instance is deployed as linked or
    /// copied files, so `auto` does not use the virtual file system for it, and link capture would miss
    /// its writes. Nothing was deployed. The CLI asks, and runs it with `--capture swap` (MASTER_SPEC §26.9).
    #[error(
        "{tool} works on the real install's Data folder, which link capture would miss, and this instance is not deployed under the virtual file system. It runs by swap: run it with --capture swap"
    )]
    InstallPathNeedsSwap { tool: String, instance_id: String },
    /// The full-copy swap was asked for, and the caller did not say the user had been shown its costs
    /// and accepted them. Nothing ran and nothing was touched.
    #[error("the full-copy swap was not confirmed: its costs have to be shown to the user and accepted before it runs")]
    SwapNotConfirmed,
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Swap(#[from] SwapError),
    #[error(transparent)]
    Deploy(#[from] crate::game_deploy::DeployError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    UserFiles(#[from] UserFilesError),
    #[error(transparent)]
    Content(#[from] crate::content_store::ContentError),
    #[error(transparent)]
    Instance(#[from] crate::game_instance::InstanceError),
    #[error(transparent)]
    Layer(#[from] agora_game_api::LayerError),
    #[error(transparent)]
    Id(#[from] agora_game_api::IdError),
    #[error("lock error: {0}")]
    Lock(#[from] crate::error::LauncherError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// Whether a tool's output is what the instance's inputs would build now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStatus {
    /// The inputs the output was built from are the inputs the instance has now.
    Current,
    /// An input changed since the output was built.
    Stale,
    /// The output was imported, so nobody recorded what it was built from.
    Unknown,
    /// Another tool the game declares as conflicting also has enabled output. Both are listed.
    Conflict,
}

impl OutputStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputStatus::Current => "current",
            OutputStatus::Stale => "stale",
            OutputStatus::Unknown => "unknown",
            OutputStatus::Conflict => "conflict",
        }
    }
}

/// A warning about a tool's output, shown at launch and by `games instance check`. Never a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFinding {
    pub tool: String,
    pub generation: String,
    pub status: OutputStatus,
    pub message: String,
}

/// One tool in an instance, as `games instance tools list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolState {
    pub tool: String,
    pub name: String,
    /// The game's package still declares the tool. An output whose tool is gone is listed anyway.
    pub declared: bool,
    /// Whether the tool writes a generated layer or a whole mod (MASTER_SPEC §26.9).
    #[serde(default)]
    pub output: ToolOutput,
    pub current: Option<String>,
    pub previous: Option<String>,
    /// `None` when the instance has no output for the tool.
    pub status: Option<OutputStatus>,
    /// Where the instance's deployment provides the tool's executable. `None` when no enabled mod
    /// (nor the game) has it: the tool is not installed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<crate::game_user_tools::FoundTool>,
    /// The tool has nothing to build; a player simply runs it (see `ToolDefinition::run_only`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run_only: bool,
}

impl ToolState {
    /// Whether the instance's Tools list shows the tool: it is installed here, or it has output
    /// here that the person may want to roll back or remove. A tool the game declares but nobody
    /// installed is not listed.
    pub fn is_present(&self) -> bool {
        self.found.is_some() || self.current.is_some()
    }
}

/// How a tool run captures what it writes (MASTER_SPEC §26.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Under the virtual file system. A run whose virtual file system cannot start is refused.
    Vfs,
    /// From a linked deployment: the tool runs in the game's folder, and what it changed is
    /// compared with the deployment's record.
    Links,
    /// The instance's own deployment decides. A virtual instance runs every tool under the virtual file
    /// system, and a run whose virtual file system cannot start is refused with the next tier, which
    /// the user is asked about. A linked or copied instance captures a tool without `uses_install_path`
    /// from links directly, and refuses a tool that works on the real install (it needs `Swap`).
    Auto,
    /// The real install's `Data` folder is swapped for a link to the farm for the length of the run,
    /// and put back afterwards (MASTER_SPEC §26.9). Asked for by name; never chosen by `Auto`.
    Swap,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Vfs => "vfs",
            CaptureMode::Links => "links",
            CaptureMode::Auto => "auto",
            CaptureMode::Swap => "swap",
        }
    }

    /// Parse a user-facing capture name (`vfs`, `links`, `auto`, `swap`).
    pub fn parse(s: &str) -> Option<CaptureMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vfs" => Some(CaptureMode::Vfs),
            "links" => Some(CaptureMode::Links),
            "auto" => Some(CaptureMode::Auto),
            "swap" => Some(CaptureMode::Swap),
            _ => None,
        }
    }
}

/// What captured a run's writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    Vfs,
    Links,
    Swap,
}

/// Something a tool run offers once a tool keeps failing (MASTER_SPEC §26.9, decision 3). An offer is
/// only ever shown and asked about: nothing switches without the user's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "offer", rename_all = "snake_case")]
pub enum ToolOffer {
    /// The full-copy swap, with what it costs (`game_tool_copy::swap_costs`).
    CopySwap { costs: SwapCosts },
    /// Deploy the instance as linked files: the virtual file system is no longer used for launches or
    /// tools. Offered only for a failure of the virtual file system's injection, and never for a tool
    /// that works on the real install, which the switch cannot help.
    SwitchToLinks,
}

/// What one run wrote, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub tool: String,
    pub name: String,
    /// The run's id, which names its `failed-<run>` folder when it was discarded.
    pub run: String,
    /// What captured the run's writes.
    pub capture: CaptureMethod,
    /// Why the run was captured from links: the virtual file system's failure, or a request for
    /// links. `None` for a run captured under the virtual file system.
    pub capture_reason: Option<String>,
    pub promoted: bool,
    /// The generation the output is now: the new one when promoted, else the one that was in effect.
    pub current: Option<String>,
    /// The generation kept for rollback.
    pub previous: Option<String>,
    /// Every file the run wrote, relative to the game folder, `/`-separated.
    pub written: Vec<String>,
    /// Lower files the run deleted, which the output hides.
    pub deleted: Vec<String>,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    /// Where a discarded run was kept.
    pub failed_folder: Option<PathBuf>,
    /// Why the run was discarded, when it was: the exit code, or the output check it failed and the
    /// file or text that failed it (MASTER_SPEC §26.9). `None` for a promoted run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The last of what the tool wrote to stdout and stderr, kept so a failed run can say why.
    /// `CAPTURED_OUTPUT_LIMIT` bytes at most per stream.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout_tail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr_tail: String,
    /// The content layer whose files were copied for this run, because it provides the tool's
    /// executable: the tool may then rewrite its own files (MASTER_SPEC §26.9). `None` when no
    /// content layer was copied, as under the virtual file system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_layer: Option<String>,
    /// What changed under the real install folder during the run, outside what Agora captured: a
    /// report, never a refusal (MASTER_SPEC §26.9). Under a swap, `Data` is the link, so its writes
    /// are captured and not listed here.
    #[serde(default)]
    pub install_changes: InstallChanges,
    /// What a swap did with its copy of `Data`, for a run captured by swap (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<SwapRunReport>,
    /// What the run lets the user try next. Set when the tool has been discarded twice in a row, and
    /// empty otherwise (MASTER_SPEC §26.9, decision 3). Each offer is asked about, never applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offers: Vec<ToolOffer>,
    /// Files a virtual-file-system run opened for writing but did not change: the copies the VFS made
    /// are identical to what the game would read without the tool's output, so they were left out of
    /// the output and deleted from staging (MASTER_SPEC §26.9). Zero for any other capture.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged_dropped: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// What a swap did with its copy of the instance's `Data` (MASTER_SPEC §26.9): how much was copied in
/// before the tool ran, what the capture found, and how much was copied again afterwards to match the
/// plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRunReport {
    /// Whether the copy is kept between runs (same volume) or made for this run only.
    pub copy: CopyKind,
    /// Files and bytes written into the copy before the tool ran: all of `Data` for a new copy, only
    /// what changed for a kept one.
    pub copied_in_files: usize,
    pub copied_in_bytes: u64,
    /// Changed or new files the capture found in the copy.
    pub captured_files: usize,
    /// Files the tool deleted from the copy, captured as whiteouts.
    pub deleted_files: usize,
    /// Files written into the copy after the run to match the plan again.
    pub refreshed_files: usize,
    pub refreshed_bytes: u64,
}

/// The difference between a tool's previous and current generations, by file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub tool: String,
    pub previous: String,
    pub current: String,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

/// A generation's record, kept in `state.json` so rollback can bring it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Generation {
    generation: String,
    inputs: InputFingerprint,
    whiteouts: Vec<RelPath>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ToolRecord {
    #[serde(default)]
    previous: Option<Generation>,
}

/// Where a tool's generations live under an instance folder.
pub fn tool_dir(instance_dir: &Path, tool: &str) -> PathBuf {
    instance_dir.join("generated").join(tool)
}

/// The folder of one generation. A generation is a number, so nothing else can name a folder
/// outside the tool's own.
pub fn generation_dir(
    instance_dir: &Path,
    tool: &str,
    generation: &str,
) -> Result<PathBuf, String> {
    if generation.is_empty() || !generation.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "generation '{generation}' of tool '{tool}' is not a number"
        ));
    }
    Ok(tool_dir(instance_dir, tool).join(generation))
}

/// The tool the game's package declares under `tool_id`.
pub fn declared_tool(
    ctx: &Ctx,
    definition: &GameDefinition,
    tool_id: &ToolId,
) -> Result<ToolDefinition, ToolError> {
    ctx.games
        .tool(&definition.id, tool_id)
        .cloned()
        .ok_or_else(|| ToolError::UnknownTool {
            game: definition.id.to_string(),
            tool: tool_id.to_string(),
        })
}

fn load_manifest(ctx: &Ctx, instance_id: &str) -> Result<GameInstanceManifest, ToolError> {
    crate::game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        crate::game_instance::InstanceError::NotFound(id) => ToolError::InstanceNotFound(id),
        other => ToolError::Instance(other),
    })
}

fn load_base(ctx: &Ctx, base_id: &str) -> Result<BaseManifest, ToolError> {
    let path = ctx.paths.base_manifest_path(base_id);
    if !path.exists() {
        return Err(ToolError::Other(format!(
            "pinned base '{base_id}' not found"
        )));
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text)
        .map_err(|e| ToolError::Other(format!("base manifest is unreadable: {e}")))
}

/// The folder the game runs from when an instance is deployed: `<bases>/deployments/<id>/game`.
fn deployment_folder(base: &BaseManifest, instance_id: &str) -> Result<PathBuf, ToolError> {
    let bases_root = base
        .location
        .parent()
        .ok_or_else(|| ToolError::Other("base location has no parent".into()))?;
    Ok(bases_root
        .join("deployments")
        .join(instance_id)
        .join("game"))
}

/// The id a user tool runs under (MASTER_SPEC §26.9): `user-` plus the tool's own generated id. The
/// caller refuses a collision with a declared tool, so a stored id can never take a declared tool's
/// place.
pub fn user_tool_id(stored_id: &str) -> Result<ToolId, ToolError> {
    let valid = !stored_id.is_empty()
        && stored_id.len() <= 32
        && stored_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(ToolError::Invalid(format!(
            "a user tool's saved id '{stored_id}' is not a valid tool id"
        )));
    }
    ToolId::new(format!("user-{stored_id}")).map_err(ToolError::Id)
}

/// The definition a user tool runs under. A tool whose `known_tool_id` names a declared tool runs as
/// that tool instead; [`run_user_tool`] checks that first.
pub fn user_tool_definition(
    definition: &GameDefinition,
    user: &UserTool,
) -> Result<ToolDefinition, ToolError> {
    let id = user_tool_id(&user.id)?;
    if definition.tool_ids.iter().any(|declared| declared == &id) {
        return Err(ToolError::Invalid(format!(
            "user tool '{}' would run as '{id}', which the game already declares",
            user.name
        )));
    }
    let external = crate::game_user_tools::is_absolute_exe(&user.exe);
    let arguments = user
        .args
        .iter()
        .map(|arg| agora_game_api::LaunchValue::Literal { value: arg.clone() })
        .collect();
    let launch = if external {
        agora_game_api::LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::default(),
            },
            arguments,
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }
    } else {
        let rel = RelPath::new(user.exe.as_str()).map_err(|e| ToolError::Invalid(e.to_string()))?;
        let parent = user
            .exe
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        agora_game_api::LaunchRecipe {
            executable: GamePath::Runtime { path: rel },
            arguments,
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::new(parent).map_err(|e| ToolError::Invalid(e.to_string()))?,
            },
        }
    };
    Ok(ToolDefinition {
        output: if user.output_is_mod {
            ToolOutput::Mod {
                folder: user.output_folder.clone().unwrap_or_default(),
                user_configured: true,
            }
        } else {
            ToolOutput::GeneratedLayer
        },
        output_arg: None,
        output_patterns: Vec::new(),
        id,
        game: definition.id.clone(),
        name: user.name.clone(),
        launch,
        relevant_settings: Vec::new(),
        after_tools: Vec::new(),
        conflicts_with: Vec::new(),
        uses_install_path: false,
        required_outputs: Vec::new(),
        failure_markers: Vec::new(),
        required_magic: Vec::new(),
        run_only: user.is_run_only(),
        executable_names: Vec::new(),
        inputs: Vec::new(),
        scope: Default::default(),
        input_files: Vec::new(),
        before_launch: None,
    })
}

/// The program a tool run starts: the user's absolute executable when there is one, otherwise the
/// game-relative path its recipe names in the deployment.
fn tool_program_path(
    base: &BaseManifest,
    instance_id: &str,
    tool: &ToolDefinition,
    external: Option<&Path>,
) -> PathBuf {
    if let Some(program) = external {
        return program.to_path_buf();
    }
    match &tool.launch.executable {
        GamePath::Runtime { path } => deployment_folder(base, instance_id)
            .map(|root| root.join(path.as_str()))
            .unwrap_or_default(),
        GamePath::Install { path, .. } => base.source_location.join(path.as_str()),
        GamePath::Base { path, .. } => base.location.join(path.as_str()),
        _ => PathBuf::new(),
    }
}

/// The folder a mod-output tool writes into. A relative folder is next to its executable; an
/// absolute one is used as it is.
fn resolve_mod_folder(folder: &str, program: &Path) -> Result<PathBuf, ToolError> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err(ToolError::Invalid(
            "a mod-output tool needs an output folder".to_string(),
        ));
    }
    let as_path = Path::new(folder);
    if as_path.is_absolute() {
        return Ok(as_path.to_path_buf());
    }
    let parent = program.parent().ok_or_else(|| {
        ToolError::Invalid("the tool has no folder to put its output beside".to_string())
    })?;
    Ok(parent.join(folder))
}

/// The folder the user set in PGPatcher's own settings, recorded on the user tool that names
/// PGPatcher (PGPatcher reads `output.dir` from its config and takes no folder argument:
/// https://github.com/hakasapl/PGPatcher/blob/HEAD/PGLib/src/PGParams.cpp).
fn user_configured_folder(
    ctx: &Ctx,
    instance_id: &str,
    tool: &ToolDefinition,
) -> Result<String, ToolError> {
    let user = crate::game_user_tools::list(ctx, instance_id)
        .map_err(|e| ToolError::Invalid(e.to_string()))?
        .into_iter()
        .find(|candidate| {
            candidate.output_is_mod
                && candidate.output_folder.is_some()
                && candidate.known_tool_id.as_deref() == Some(tool.id.as_str())
        });
    match user.and_then(|candidate| candidate.output_folder) {
        Some(folder) if !folder.trim().is_empty() => Ok(folder),
        _ => Err(ToolError::Invalid(format!(
            "Set the output folder in {}'s own settings, then add {} in Your tools with that same folder.",
            tool.name, tool.name
        ))),
    }
}

/// Lexically normalise `.` and `..` without touching the disk, so a missing folder can never hide a
/// path that reaches the game. The path is absolute by the time it gets here.
fn lexical_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if out
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, Component::Normal(_)))
                {
                    out.pop();
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// The path with its longest existing prefix resolved through the real file system, so a junction or
/// symlink on the way is followed even when the final folder does not exist yet.
fn real_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = existing.file_name().map(|name| name.to_os_string()) else {
            return path.to_path_buf();
        };
        if !existing.pop() {
            return path.to_path_buf();
        }
        tail.push(name);
    }
}

/// A path folded the way the platform compares names, with the Windows verbatim prefix removed and
/// separators made uniform.
fn folded_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let text = text.trim_end_matches('/');
    agora_game_api::fold_path_case(text)
}

fn same_or_inside(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The one check for a user-chosen output folder (MASTER_SPEC §26.9). `folder` is compared by its
/// real path and must not equal, sit inside, or contain any of `forbidden` (the game, its Data
/// folder, the instance, the deployment, Agora's data and content store, and the tool's own folder).
fn validate_output_folder(
    folder: &Path,
    forbidden: &[PathBuf],
    tool_root: Option<&Path>,
) -> Result<(), ToolError> {
    let folded = folded_path(&real_path(&lexical_path(folder)));
    let refuse = |what: &str| {
        ToolError::Invalid(format!(
            "The output folder '{}' is inside {what}, or contains it. Use the folder you set in the tool's own settings, outside it.",
            folder.display()
        ))
    };
    for root in forbidden {
        let root = folded_path(root);
        if same_or_inside(&folded, &root) || same_or_inside(&root, &folded) {
            return Err(refuse("the game or this instance's deployment"));
        }
    }
    // A folder inside the tool's own folder is allowed (it does not move the executable), but the
    // tool's folder itself, or a folder that contains it, would move the tool and is refused.
    if let Some(root) = tool_root {
        let root = folded_path(root);
        if same_or_inside(&root, &folded) {
            return Err(refuse("the tool's own folder"));
        }
    }
    Ok(())
}

fn folder_is_empty(dir: &Path) -> Result<bool, ToolError> {
    let mut entries = std::fs::read_dir(dir)?;
    Ok(entries.next().is_none())
}

fn aside_folder(instance_dir: &Path, tool: &ToolId) -> PathBuf {
    instance_dir
        .join("tool-output-replaced")
        .join(tool.as_str())
}

fn next_aside_path(root: &Path) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ").to_string();
    let mut candidate = root.join(&stamp);
    let mut n = 2;
    while candidate.exists() {
        candidate = root.join(format!("{stamp}-{n}"));
        n += 1;
    }
    candidate
}

/// A mod-output tool's folder and the user's move-aside choice. The folder is validated when it is
/// chosen and again immediately before it is moved, and the move only happens after the run's
/// capture has started, so a refused run leaves everything where it was (MASTER_SPEC §26.9).
struct ConfiguredOutput {
    folder: PathBuf,
    forbidden: Vec<PathBuf>,
    tool_root: Option<PathBuf>,
    aside_root: PathBuf,
    needs_move: bool,
    tool: String,
}

impl ConfiguredOutput {
    fn new(
        folder: PathBuf,
        forbidden: Vec<PathBuf>,
        tool_root: Option<PathBuf>,
        instance_dir: &Path,
        tool: &ToolDefinition,
        move_aside: bool,
    ) -> Result<Self, ToolError> {
        let folder = lexical_path(&folder);
        validate_output_folder(&folder, &forbidden, tool_root.as_deref())?;
        let aside_root = aside_folder(instance_dir, &tool.id);
        let needs_move = match std::fs::symlink_metadata(&folder) {
            Ok(meta) if meta.is_dir() => !folder_is_empty(&folder)?,
            Ok(_) => {
                return Err(ToolError::Invalid(format!(
                    "'{}' is not a folder, so {} cannot write its output there",
                    folder.display(),
                    tool.name
                )))
            }
            Err(_) => false,
        };
        if needs_move {
            if aside_root.exists() && !aside_root.is_dir() {
                return Err(ToolError::Invalid(format!(
                    "'{}' is a file, so Agora cannot move the old output aside",
                    aside_root.display()
                )));
            }
            if !move_aside {
                return Err(ToolError::OutputFolderNotEmpty {
                    tool: tool.name.clone(),
                    folder: folder.display().to_string(),
                    aside: next_aside_path(&aside_root).display().to_string(),
                });
            }
        }
        Ok(Self {
            folder,
            forbidden,
            tool_root,
            aside_root,
            needs_move,
            tool: tool.name.clone(),
        })
    }

    /// Move a non-empty folder aside, after checking it once more. A refused move changes nothing.
    fn move_aside(&self) -> Result<(), ToolError> {
        if !self.needs_move || !self.folder.exists() {
            return Ok(());
        }
        validate_output_folder(&self.folder, &self.forbidden, self.tool_root.as_deref())?;
        if !self.folder.is_dir() {
            return Err(ToolError::Invalid(format!(
                "'{}' is not a folder, so {} cannot write its output there",
                self.folder.display(),
                self.tool
            )));
        }
        if folder_is_empty(&self.folder)? {
            return Ok(());
        }
        if self.aside_root.exists() && !self.aside_root.is_dir() {
            return Err(ToolError::Invalid(format!(
                "'{}' is a file, so Agora cannot move the old output aside",
                self.aside_root.display()
            )));
        }
        let target = next_aside_path(&self.aside_root);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&self.folder, &target).map_err(|error| {
            ToolError::Invalid(format!(
                "the old output could not be moved to '{}': {error}. Nothing was changed",
                target.display()
            ))
        })?;
        Ok(())
    }
}

/// Everything a user-chosen output folder must stay clear of.
fn output_forbidden_roots(
    ctx: &Ctx,
    instance_id: &str,
    base: &BaseManifest,
) -> Result<Vec<PathBuf>, ToolError> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let roots = vec![
        lexical_path(&base.source_location),
        lexical_path(&base.source_location.join("Data")),
        lexical_path(&base.location),
        lexical_path(&instance_dir),
        lexical_path(&deployment_folder(base, instance_id)?),
        lexical_path(ctx.paths.root()),
        lexical_path(&ctx.paths.content_root()),
    ];
    Ok(roots.into_iter().map(|root| real_path(&root)).collect())
}

/// A tool's absolute executable, when its recipe is not a game path. The file is checked again here,
/// so an edited list cannot point a run somewhere else.
fn external_program(program: Option<&Path>) -> Result<Option<PathBuf>, ToolError> {
    let Some(program) = program else {
        return Ok(None);
    };
    if !program.is_file() {
        return Err(ToolError::Invalid(format!(
            "'{}' is not an existing file",
            program.display()
        )));
    }
    let is_exe = program
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
    if !is_exe {
        return Err(ToolError::Invalid(format!(
            "'{}' is not an .exe file",
            program.display()
        )));
    }
    Ok(Some(program.to_path_buf()))
}

/// Resolve a tool's launch, using the user's absolute executable when there is one.
fn resolve_tool_launch(
    tool: &ToolDefinition,
    roots: &LaunchRoots,
    external: Option<&Path>,
) -> Result<crate::game_launch::ResolvedLaunch, LaunchError> {
    let Some(program) = external else {
        return game_launch::resolve_recipe(&tool.launch, roots);
    };
    if !program.is_file() {
        return Err(LaunchError::ProgramMissing {
            path: program.to_path_buf(),
        });
    }
    let mut args = Vec::with_capacity(tool.launch.arguments.len());
    for arg in &tool.launch.arguments {
        let agora_game_api::LaunchValue::Literal { value } = arg else {
            return Err(LaunchError::UnsupportedRoot(
                "an absolute user tool only takes literal arguments".to_string(),
            ));
        };
        args.push(std::ffi::OsString::from(value));
    }
    Ok(crate::game_launch::ResolvedLaunch {
        program: program.to_path_buf(),
        args,
        env: Default::default(),
        cwd: program
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| program.to_path_buf()),
    })
}

fn read_record(dir: &Path) -> Result<ToolRecord, ToolError> {
    let path = dir.join(STATE_FILE);
    if !path.exists() {
        return Ok(ToolRecord::default());
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&text)?)
}

fn write_record(dir: &Path, record: &ToolRecord) -> Result<(), ToolError> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{STATE_FILE}.{}.tmp", new_id_suffix()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, dir.join(STATE_FILE))?;
    Ok(())
}

/// The numbers of the generation folders in a tool's folder.
fn generation_numbers(dir: &Path) -> Result<Vec<u64>, ToolError> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u64>().ok())
            {
                out.push(n);
            }
        }
    }
    Ok(out)
}

fn new_id_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// A run id that sorts by start time, so the newest failed runs are the last names.
fn new_run_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms:013}-{}", new_id_suffix())
}

fn generated_position(layers: &[Layer], tool: &str) -> Option<usize> {
    layers.iter().position(|layer| {
        matches!(&layer.source, LayerSource::Generated { tool: t, .. } if t.as_str() == tool)
    })
}

fn generated_layer(
    tool: &ToolId,
    generation: &str,
    inputs: InputFingerprint,
    whiteouts: Vec<RelPath>,
) -> Result<Layer, ToolError> {
    Ok(Layer {
        id: LayerId::new(format!("generated:{}", tool.as_str()))?,
        enabled: true,
        mount_path: RelPath::default(),
        source_path: RelPath::default(),
        source: LayerSource::Generated {
            tool: tool.clone(),
            generation: generation.to_string(),
            inputs,
        },
        whiteouts,
        own_copy: false,
    })
}

/// Put the generated layers in their order: a tool's output sits above the tools it names in
/// `after_tools`, and otherwise tools follow their ids. Base and content stay below them, the
/// writable layer and any staging above (MASTER_SPEC §26.5, §26.9).
fn arrange(ctx: &Ctx, definition: &GameDefinition, layers: Vec<Layer>) -> Vec<Layer> {
    let (generated, others): (Vec<Layer>, Vec<Layer>) = layers
        .into_iter()
        .partition(|layer| matches!(layer.source, LayerSource::Generated { .. }));

    let mut pending: BTreeMap<String, (Layer, Vec<String>)> = BTreeMap::new();
    for layer in generated {
        let LayerSource::Generated { tool, .. } = &layer.source else {
            continue;
        };
        let after = ctx
            .games
            .tool(&definition.id, tool)
            .map(|t| t.after_tools.iter().map(|a| a.to_string()).collect())
            .unwrap_or_default();
        pending.insert(tool.to_string(), (layer, after));
    }

    let mut placed: Vec<Layer> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        // The lowest-named tool whose `after_tools` are all placed. A cycle has none, and then the
        // lowest name goes first, so the order is always defined.
        let ready = pending
            .iter()
            .find(|(name, (_, after))| {
                after
                    .iter()
                    .all(|dep| dep == *name || !pending.contains_key(dep))
            })
            .map(|(name, _)| name.clone());
        let next = ready
            .or_else(|| pending.keys().next().cloned())
            .unwrap_or_default();
        if let Some((layer, _)) = pending.remove(&next) {
            placed.push(layer);
        }
    }

    let mut out = others;
    let at = out
        .iter()
        .position(|layer| layer.source.rank() > 3)
        .unwrap_or(out.len());
    let tail = out.split_off(at);
    out.extend(placed);
    out.extend(tail);
    out
}

fn save_layers(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    manifest: &mut GameInstanceManifest,
    layers: Vec<Layer>,
) -> Result<(), ToolError> {
    manifest.layers = LayerStack::new(arrange(ctx, definition, layers))?;
    crate::game_deploy::write_instance_manifest_atomic(ctx, instance_id, manifest)?;
    Ok(())
}

/// Make files an import found in a Mod Organizer 2 `overwrite` folder into a tool's output
/// (MASTER_SPEC §26.10). They become generation 1 of the tool, copied whole into its folder, and
/// the layer's inputs are unknown: nothing records what they were built from, so the launch check
/// offers a rebuild rather than calling them current. `files` pairs each game-relative path
/// (`Data/...`) with the file it is copied from. Returns how many files were copied.
pub fn import_output(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    files: &[(String, PathBuf)],
) -> Result<usize, ToolError> {
    declared_tool(ctx, definition, tool_id)?;
    if files.is_empty() {
        return Err(ToolError::Invalid(format!(
            "no files to import as the output of '{tool_id}'"
        )));
    }
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "tool-import",
    )?;
    let mut manifest = load_manifest(ctx, instance_id)?;
    if generated_position(manifest.layers.layers(), tool_id.as_str()).is_some() {
        return Err(ToolError::Invalid(format!(
            "tool '{tool_id}' already has output in this instance"
        )));
    }
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let folder =
        generation_dir(&instance_dir, tool_id.as_str(), "1").map_err(ToolError::Invalid)?;
    if folder.exists() {
        return Err(ToolError::Invalid(format!(
            "a folder for the first output of '{tool_id}' already exists; remove it first"
        )));
    }
    for (rel, source) in files {
        let dest = crate::game_ini::join_rel(&folder, rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, &dest)?;
    }
    let layer = generated_layer(tool_id, "1", InputFingerprint::Unknown, Vec::new())?;
    let mut layers = manifest.layers.layers().to_vec();
    layers.push(layer);
    save_layers(ctx, instance_id, definition, &mut manifest, layers)?;
    Ok(files.len())
}

/// Split `file:section:key` into its three parts.
fn split_setting(setting: &str) -> Result<(&str, &str, &str), ToolError> {
    match setting.split(':').collect::<Vec<_>>().as_slice() {
        [file, section, key] if !file.is_empty() && !section.is_empty() && !key.is_empty() => {
            Ok((file, section, key))
        }
        _ => Err(ToolError::Invalid(format!(
            "relevant setting '{setting}' must be file:section:key"
        ))),
    }
}

const TOOL_INPUTS_FILE: &str = "tool_inputs.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ToolInputsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tools: BTreeMap<String, BTreeMap<String, String>>,
}

/// A tool's stored input values for an instance, keyed by tool then input id.
pub fn read_tool_inputs(
    ctx: &Ctx,
    instance_id: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, ToolError> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let path = dir.join(TOOL_INPUTS_FILE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let file: ToolInputsFile = serde_json::from_str(&text)?;
    Ok(file.tools)
}

fn write_tool_inputs(
    ctx: &Ctx,
    instance_id: &str,
    tools: BTreeMap<String, BTreeMap<String, String>>,
) -> Result<(), ToolError> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(TOOL_INPUTS_FILE);
    let tmp = dir.join(format!("{TOOL_INPUTS_FILE}.{}.tmp", new_id_suffix()));
    let file = ToolInputsFile { version: 1, tools };
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Store one input value for a tool in an instance. The value is checked against the declared
/// input's kind and choices here, so a bad value is refused at the door (MASTER_SPEC \u00a730.12).
pub fn set_tool_input(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    input_id: &str,
    value: &str,
) -> Result<(), ToolError> {
    let tool = declared_tool(ctx, definition, tool_id)?;
    let input = tool
        .inputs
        .iter()
        .find(|input| input.id == input_id)
        .ok_or_else(|| ToolError::InvalidInput {
            tool: tool_id.to_string(),
            input: input_id.to_string(),
            reason: "the tool declares no such input".into(),
        })?;
    validate_input_value(&tool.id, input, value)?;
    let mut inputs = read_tool_inputs(ctx, instance_id)?;
    inputs
        .entry(tool_id.to_string())
        .or_default()
        .insert(input_id.to_string(), value.to_string());
    write_tool_inputs(ctx, instance_id, inputs)
}

/// Remove a stored value, so the input's default (if any) applies again.
pub fn clear_tool_input(
    ctx: &Ctx,
    instance_id: &str,
    tool_id: &ToolId,
    input_id: &str,
) -> Result<(), ToolError> {
    let mut inputs = read_tool_inputs(ctx, instance_id)?;
    if let Some(tool) = inputs.get_mut(tool_id.as_str()) {
        tool.remove(input_id);
        if tool.is_empty() {
            inputs.remove(tool_id.as_str());
        }
    }
    write_tool_inputs(ctx, instance_id, inputs)
}

fn validate_input_value(
    tool: &ToolId,
    input: &agora_game_api::ToolInput,
    value: &str,
) -> Result<(), ToolError> {
    match input.kind {
        ToolInputKind::Choice => {
            if !input.choices.iter().any(|choice| choice == value) {
                return Err(ToolError::InvalidInput {
                    tool: tool.to_string(),
                    input: input.id.clone(),
                    reason: format!("'{value}' is not one of {}", input.choices.join(", ")),
                });
            }
        }
        ToolInputKind::Flag => {
            parse_flag(value).ok_or_else(|| ToolError::InvalidInput {
                tool: tool.to_string(),
                input: input.id.clone(),
                reason: format!("'{value}' is not a yes or no"),
            })?;
        }
        ToolInputKind::File | ToolInputKind::Folder | ToolInputKind::Text => {}
    }
    Ok(())
}

fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// The arguments a tool's inputs add to its recipe, with `{item}` replaced by `item_path` for an
/// item-scoped tool (MASTER_SPEC \u00a730.12). A required input with no value refuses the run; a value
/// outside a choice list refuses it too.
pub fn tool_input_arguments(
    tool: &ToolDefinition,
    stored: &BTreeMap<String, String>,
    item_path: Option<&str>,
) -> Result<Vec<std::ffi::OsString>, ToolError> {
    if tool.scope == ToolScope::Item && item_path.is_none() {
        return Err(ToolError::ItemScopeWithoutItem {
            tool: tool.id.to_string(),
        });
    }
    let substitute = |value: &str| match item_path {
        Some(item) => value.replace("{item}", item),
        None => value.to_string(),
    };
    let mut args = Vec::new();
    for input in &tool.inputs {
        let value = stored
            .get(&input.id)
            .cloned()
            .or_else(|| input.default.clone())
            .filter(|value| !value.is_empty());
        let Some(value) = value else {
            if input.required {
                return Err(ToolError::MissingInput {
                    tool: tool.id.to_string(),
                    input: input.id.clone(),
                    label: input.label.clone(),
                });
            }
            continue;
        };
        if input.kind == ToolInputKind::Choice
            && !input.choices.iter().any(|choice| choice == &value)
        {
            return Err(ToolError::InvalidInput {
                tool: tool.id.to_string(),
                input: input.id.clone(),
                reason: format!("'{value}' is not one of {}", input.choices.join(", ")),
            });
        }
        let Some(arg) = &input.arg else {
            continue;
        };
        let arg = substitute(arg);
        if input.kind == ToolInputKind::Flag {
            if parse_flag(&value).ok_or_else(|| ToolError::InvalidInput {
                tool: tool.id.to_string(),
                input: input.id.clone(),
                reason: format!("'{value}' is not a yes or no"),
            })? {
                args.push(std::ffi::OsString::from(arg));
            }
        } else {
            args.push(std::ffi::OsString::from(arg));
            args.push(std::ffi::OsString::from(substitute(&value)));
        }
    }
    Ok(args)
}

/// The item-scoped tool's path in the instance view: the mount path of the content layer the item
/// fills. `None` when the item is not in the instance.
pub fn item_view_path(
    ctx: &Ctx,
    instance_id: &str,
    item_id: &str,
) -> Result<Option<String>, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    for layer in manifest.layers.layers() {
        if let LayerSource::Content { content } = &layer.source {
            if content == item_id {
                return Ok(Some(layer.mount_path.as_str().to_string()));
            }
        }
    }
    Ok(None)
}

/// A tool a launch should run first because it has never run here, or its output is stale
/// (MASTER_SPEC \u00a730.12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeforeLaunchNeed {
    pub tool: String,
    pub name: String,
    pub mode: BeforeLaunchMode,
    /// Why the tool is needed, such as "it has never run here".
    pub reason: String,
}

/// The tools that must or may run before this instance launches. `when` is judged against
/// `facts`; unknown means the tool is not needed, but the launch log is told.
pub fn before_launch_needs(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    facts: &dyn agora_game_api::ConditionFacts,
    notes: &mut Vec<String>,
) -> Result<Vec<BeforeLaunchNeed>, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let mut needs = Vec::new();
    let Some(package) = ctx.games.package_for(&definition.id) else {
        return Ok(needs);
    };
    for tool in &package.definition().tools {
        let Some(before) = &tool.before_launch else {
            continue;
        };
        if tool.game != definition.id {
            continue;
        }
        if let Some(when) = &before.when {
            match when.evaluate(facts) {
                agora_game_api::Truth::True => {}
                agora_game_api::Truth::False => continue,
                agora_game_api::Truth::Unknown => {
                    notes.push(format!(
                        "Could not tell whether {} holds, so {} was not run before launch.",
                        when.describe(),
                        tool.name
                    ));
                    continue;
                }
            }
        }
        let (reason, needed) = before_launch_status(ctx, instance_id, definition, tool, &manifest);
        if needed {
            needs.push(BeforeLaunchNeed {
                tool: tool.id.to_string(),
                name: tool.name.clone(),
                mode: before.mode,
                reason,
            });
        }
    }
    Ok(needs)
}

fn before_launch_status(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    manifest: &GameInstanceManifest,
) -> (String, bool) {
    let mut recorded: Option<InputFingerprint> = None;
    for layer in manifest.layers.layers() {
        if let LayerSource::Generated {
            tool: layer_tool,
            inputs: layer_inputs,
            ..
        } = &layer.source
        {
            if layer.enabled && layer_tool == &tool.id {
                recorded = Some(layer_inputs.clone());
            }
        }
    }
    let Some(recorded) = recorded else {
        return ("it has never run here".to_string(), true);
    };
    let InputFingerprint::Known(recorded) = recorded else {
        return (
            "Agora cannot tell whether its output is current".to_string(),
            true,
        );
    };
    match input_fingerprint(ctx, instance_id, definition, tool) {
        Ok(now) if now == recorded => ("its output is current".to_string(), false),
        Ok(_) => ("its output is out of date".to_string(), true),
        Err(_) => (
            "Agora cannot tell whether its output is current".to_string(),
            true,
        ),
    }
}

fn view_path(layer: &Layer, file: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !layer.mount_path.as_str().is_empty() {
        parts.push(layer.mount_path.as_str());
    }
    if !layer.source_path.as_str().is_empty() {
        parts.push(layer.source_path.as_str());
    }
    parts.push(file);
    parts.join("/")
}

/// Whether a content layer provides at least one file matching the tool's `input_files` globs.
fn layer_matches_input_files(
    ctx: &Ctx,
    layer: &Layer,
    content: &str,
    patterns: &[String],
) -> Result<bool, ToolError> {
    let item = crate::content_store::get_item(ctx, content)?;
    Ok(item.files.iter().any(|file| {
        let path = view_path(layer, file.path.as_str());
        patterns
            .iter()
            .any(|pattern| glob_match(&fold_path_case(pattern), &fold_path_case(&path)))
    }))
}

/// The digest of everything a tool's output depends on: the enabled content layers in order, the
/// active plugin load order, and the values of the tool's relevant settings (MASTER_SPEC §26.9).
pub fn input_fingerprint(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
) -> Result<String, ToolError> {
    input_fingerprint_with_view(ctx, instance_id, definition, tool, None)
}

/// The fingerprint a run uses: the whole instance's, or for an item-scoped tool only the item it
/// was given (MASTER_SPEC §30.12).
pub(crate) fn input_fingerprint_scoped(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    scope_item: Option<&str>,
) -> Result<String, ToolError> {
    match scope_item {
        Some(item) => input_fingerprint_for_item(ctx, instance_id, tool, item),
        None => input_fingerprint(ctx, instance_id, definition, tool),
    }
}

/// The digest of one content item's files, for a tool that runs once per item. Changes to any
/// other content item leave it alone.
pub fn input_fingerprint_for_item(
    ctx: &Ctx,
    instance_id: &str,
    tool: &ToolDefinition,
    item_id: &str,
) -> Result<String, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let mut hasher = Sha256::new();
    let mut found = false;
    for layer in manifest.layers.layers() {
        if let (true, LayerSource::Content { content }) = (layer.enabled, &layer.source) {
            if content != item_id {
                continue;
            }
            found = true;
            hasher.update(
                format!(
                    "content\t{content}\t{}\t{}\n",
                    layer.mount_path.as_str(),
                    layer.source_path.as_str()
                )
                .as_bytes(),
            );
            let item = crate::content_store::get_item(ctx, content)?;
            for file in &item.files {
                let path = view_path(layer, file.path.as_str());
                if tool.input_files.is_empty()
                    || tool
                        .input_files
                        .iter()
                        .any(|pattern| glob_match(&fold_path_case(pattern), &fold_path_case(&path)))
                {
                    hasher.update(
                        format!("file\t{}\t{}\n", file.path.as_str(), file.sha256).as_bytes(),
                    );
                }
            }
        }
    }
    if !found {
        return Err(ToolError::InvalidInput {
            tool: tool.id.to_string(),
            input: "item".into(),
            reason: format!("'{item_id}' is not content of this instance"),
        });
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// [`input_fingerprint`], reading `links` (the instance's Links plan) for the load order when the
/// caller has it. `None` plans it.
pub(crate) fn input_fingerprint_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    links: Option<&crate::game_deploy::DeploymentPlan>,
) -> Result<String, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let mut hasher = Sha256::new();
    for layer in manifest.layers.layers() {
        if let (true, LayerSource::Content { content }) = (layer.enabled, &layer.source) {
            if !tool.input_files.is_empty()
                && !layer_matches_input_files(ctx, layer, content, &tool.input_files)?
            {
                continue;
            }
            hasher.update(
                format!(
                    "content\t{content}\t{}\t{}\n",
                    layer.mount_path.as_str(),
                    layer.source_path.as_str()
                )
                .as_bytes(),
            );
            if !tool.input_files.is_empty() {
                let item = crate::content_store::get_item(ctx, content)?;
                for file in &item.files {
                    let path = view_path(layer, file.path.as_str());
                    if tool
                        .input_files
                        .iter()
                        .any(|pattern| glob_match(&fold_path_case(pattern), &fold_path_case(&path)))
                    {
                        hasher.update(
                            format!("file\t{}\t{}\n", file.path.as_str(), file.sha256).as_bytes(),
                        );
                    }
                }
            }
        }
    }
    if !definition.load_orders.is_empty() {
        let order = game_load_order::order_with_view(ctx, instance_id, definition, links)
            .map_err(|e| ToolError::Other(format!("cannot read the load order: {e}")))?;
        for entry in order.entries.iter().filter(|entry| entry.active) {
            hasher.update(format!("plugin\t{}\n", entry.name).as_bytes());
        }
    }
    for setting in &tool.relevant_settings {
        let (file, section, key) = split_setting(setting)?;
        let read = game_ini::read(ctx, instance_id, definition, file)
            .map_err(|e| ToolError::Other(format!("cannot read '{setting}': {e}")))?;
        let value = match read.document.get(section, key) {
            Some(value) => format!("set\t{value}"),
            None => "unset".to_string(),
        };
        hasher.update(format!("setting\t{setting}\t{value}\n").as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Whether a generated layer's output matches the inputs now. Any failure to read the inputs is
/// `Unknown`, never `Current`.
fn layer_status(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolId,
    inputs: &InputFingerprint,
    links: Option<&crate::game_deploy::DeploymentPlan>,
) -> OutputStatus {
    let InputFingerprint::Known(recorded) = inputs else {
        return OutputStatus::Unknown;
    };
    let Some(declared) = ctx.games.tool(&definition.id, tool).cloned() else {
        return OutputStatus::Unknown;
    };
    match input_fingerprint_with_view(ctx, instance_id, definition, &declared, links) {
        Ok(now) if now == *recorded => OutputStatus::Current,
        Ok(_) => OutputStatus::Stale,
        Err(_) => OutputStatus::Unknown,
    }
}

fn rebuild_command(instance_id: &str, tool: &str) -> String {
    format!("agora games instance tools run {instance_id} {tool}")
}

/// The tools of an instance: every tool the game declares, and every tool with output here. The
/// instance's Links plan is made once for the whole list, however many outputs it has.
pub fn list(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<ToolState>, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let links = list_links(ctx, instance_id, definition, &manifest)?;
    list_with_manifest(ctx, instance_id, definition, &manifest, links.as_ref())
}

/// [`list`] and [`output_findings_with_view`] from one Links plan: what `games_tools_list` needs,
/// with no second planning pass.
pub fn list_and_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<(Vec<ToolState>, Vec<OutputFinding>), ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let links = list_links(ctx, instance_id, definition, &manifest)?;
    let states = list_with_manifest(ctx, instance_id, definition, &manifest, links.as_ref())?;
    let findings = output_findings_with_view(ctx, instance_id, definition, links.as_ref())?;
    Ok((states, findings))
}

/// The Links plan a list's staleness checks read, when the game's rules read one. A generated
/// layer per tool used to plan its own, so N outputs planned N + 1 times.
fn list_links(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    manifest: &GameInstanceManifest,
) -> Result<Option<crate::game_deploy::DeploymentPlan>, ToolError> {
    if definition.load_orders.is_empty() || !matches!(manifest.base, BaseReference::Pinned { .. }) {
        return Ok(None);
    }
    game_load_order::plan_for_order(ctx, instance_id, definition)
        .map_err(|e| ToolError::Other(format!("cannot plan the instance's links: {e}")))
}

fn list_with_manifest(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    manifest: &GameInstanceManifest,
    links: Option<&crate::game_deploy::DeploymentPlan>,
) -> Result<Vec<ToolState>, ToolError> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    // Where the deployment provides each declared tool. The staleness checks' Links plan is used
    // when there is one, so the list still plans once.
    let planned;
    let detect_in = match links {
        Some(plan) => Some(plan),
        None if matches!(manifest.base, BaseReference::Pinned { .. }) => {
            planned = crate::game_deploy::plan(
                ctx,
                instance_id,
                definition,
                crate::game_deploy::DeployMode::Links,
            )
            .map_err(|e| ToolError::Other(format!("cannot plan the instance's links: {e}")))?;
            Some(&planned)
        }
        None => None,
    };
    let mut found = match detect_in {
        Some(plan) => crate::game_user_tools::detect_known_tools(ctx, definition, manifest, plan)
            .map_err(|e| ToolError::Other(e.to_string()))?,
        None => BTreeMap::new(),
    };
    let mut states: BTreeMap<String, ToolState> = BTreeMap::new();
    for tool_id in &definition.tool_ids {
        let declared_tool = ctx.games.tool(&definition.id, tool_id);
        let name = declared_tool
            .map(|t| t.name.clone())
            .unwrap_or_else(|| tool_id.to_string());
        states.insert(
            tool_id.to_string(),
            ToolState {
                tool: tool_id.to_string(),
                name,
                declared: true,
                output: declared_tool
                    .map(|t| t.output.clone())
                    .unwrap_or(ToolOutput::GeneratedLayer),
                current: None,
                previous: None,
                status: None,
                found: found.remove(tool_id.as_str()),
                run_only: declared_tool.is_some_and(|t| t.run_only),
            },
        );
    }
    for layer in manifest.layers.layers() {
        let LayerSource::Generated {
            tool,
            generation,
            inputs,
        } = &layer.source
        else {
            continue;
        };
        let declared = ctx.games.tool(&definition.id, tool);
        let name = declared
            .map(|t| t.name.clone())
            .unwrap_or_else(|| tool.to_string());
        let previous = read_record(&tool_dir(&instance_dir, tool.as_str()))?
            .previous
            .map(|g| g.generation);
        let found = states
            .get_mut(tool.as_str())
            .and_then(|state| state.found.take());
        states.insert(
            tool.to_string(),
            ToolState {
                tool: tool.to_string(),
                name,
                declared: declared.is_some(),
                output: declared
                    .map(|t| t.output.clone())
                    .unwrap_or(ToolOutput::GeneratedLayer),
                current: Some(generation.clone()),
                previous,
                status: Some(layer_status(
                    ctx,
                    instance_id,
                    definition,
                    tool,
                    inputs,
                    links,
                )),
                found,
                run_only: declared.is_some_and(|t| t.run_only),
            },
        );
    }
    Ok(states.into_values().collect())
}

/// The warnings a launch or check carries about the instance's enabled generated layers: each one
/// that is stale or unknown. A missing folder is not a warning; it refuses the deployment.
pub fn output_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<OutputFinding>, ToolError> {
    output_findings_with_view(ctx, instance_id, definition, None)
}

/// [`output_findings`], reading `links` (the instance's Links plan) for the load order when the
/// caller has it. `None` plans it.
pub(crate) fn output_findings_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    links: Option<&crate::game_deploy::DeploymentPlan>,
) -> Result<Vec<OutputFinding>, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let mut findings = Vec::new();
    let mut enabled: Vec<(ToolId, String)> = Vec::new();
    for layer in manifest.layers.layers() {
        let LayerSource::Generated {
            tool,
            generation,
            inputs,
        } = &layer.source
        else {
            continue;
        };
        if !layer.enabled {
            continue;
        }
        enabled.push((tool.clone(), generation.clone()));
        let status = layer_status(ctx, instance_id, definition, tool, inputs, links);
        if status == OutputStatus::Current {
            continue;
        }
        let name = ctx
            .games
            .tool(&definition.id, tool)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| tool.to_string());
        let command = rebuild_command(instance_id, tool.as_str());
        let message = match status {
            OutputStatus::Stale => format!(
                "{name} output is out of date: rebuild with `{command}`"
            ),
            _ => format!(
                "{name} output has unknown inputs (it was imported, so Agora cannot tell whether it is current): rebuild with `{command}`"
            ),
        };
        findings.push(OutputFinding {
            tool: tool.to_string(),
            generation: generation.clone(),
            status,
            message,
        });
    }

    // Two enabled outputs from tools that declare each other's conflict (MASTER_SPEC §26.9): a warning on
    // both. A declaration on either side is enough.
    let declares = |a: &ToolId, b: &ToolId| {
        ctx.games
            .tool(&definition.id, a)
            .is_some_and(|t| t.conflicts_with.contains(b))
    };
    for (i, (first, first_gen)) in enabled.iter().enumerate() {
        for (second, second_gen) in enabled.iter().skip(i + 1) {
            if !(declares(first, second) || declares(second, first)) {
                continue;
            }
            let name_of = |id: &ToolId| {
                ctx.games
                    .tool(&definition.id, id)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| id.to_string())
            };
            let (a, b) = (name_of(first), name_of(second));
            findings.push(OutputFinding {
                tool: first.to_string(),
                generation: first_gen.clone(),
                status: OutputStatus::Conflict,
                message: format!(
                    "{a} and {b} both have enabled output, and both write the same behaviour files: enable one of them"
                ),
            });
            findings.push(OutputFinding {
                tool: second.to_string(),
                generation: second_gen.clone(),
                status: OutputStatus::Conflict,
                message: format!(
                    "{b} and {a} both have enabled output, and both write the same behaviour files: enable one of them"
                ),
            });
        }
    }
    Ok(findings)
}

/// Whether a user-files session is running for this game and store. A tool does not run then.
fn refuse_during_session(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<(), ToolError> {
    match game_user_files::status(ctx, &definition.id, store)? {
        Some(session) if session.running => Err(ToolError::SessionRunning(session.instance)),
        _ => Ok(()),
    }
}

/// Restore the game's user files once a run is over, if a session was swapped in for it.
fn release_session(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<(), ToolError> {
    if game_user_files::status(ctx, &definition.id, store)?.is_none() {
        return Ok(());
    }
    match game_user_files::restore(ctx, definition, store) {
        Ok(_) | Err(UserFilesError::NoSession { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

struct TreeExit {
    code: Option<i32>,
    cancelled: bool,
    output: crate::game_launch::CapturedOutput,
}

impl TreeExit {
    /// Why a run was discarded because the tool did not exit cleanly, with the last of what it
    /// said so the player is not left with only the code.
    fn failed_with_code(&self, code: Option<i32>) -> String {
        let base = match code {
            Some(code) => format!("the tool exited with code {code}"),
            None => "the tool exited with no exit code".to_string(),
        };
        self.with_output(base)
    }

    /// Add the last of the tool's output to a failure reason, when it said anything.
    fn with_output(&self, base: String) -> String {
        let mut said = String::new();
        for (name, text) in [
            ("stdout", &self.output.stdout),
            ("stderr", &self.output.stderr),
        ] {
            let tail = failure_tail(text);
            if !tail.is_empty() {
                if !said.is_empty() {
                    said.push('\n');
                }
                said.push_str(&format!("{name}:\n{tail}"));
            }
        }
        if said.is_empty() {
            base
        } else {
            format!("{base}. It said:\n{said}")
        }
    }
}

/// The last of a stream that belongs in a failure message: a few lines, not the whole capture.
fn failure_tail(text: &str) -> &str {
    const CHARS: usize = 2000;
    let start = text
        .char_indices()
        .rev()
        .nth(CHARS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    text[start..].trim()
}

/// Wait for the tool and every process it started from `dir` to end. A cancel ends them all.
fn wait_for_tree(dir: &Path, launched: &mut LaunchedGame, cancel: &CancellationToken) -> TreeExit {
    let mut code = None;
    let mut child_done = false;
    let mut cancelled = false;
    let mut quiet_since = Instant::now();
    loop {
        if !cancelled && cancel.is_cancelled() {
            cancelled = true;
            let _ = launched.child.kill();
            kill_from(dir);
        }
        if !child_done {
            if let Ok(Some(status)) = launched.child.try_wait() {
                child_done = true;
                code = status.code();
            }
        }
        let running = game_launch::processes_running_from(dir);
        if !running.is_empty() || !child_done {
            quiet_since = Instant::now();
        }
        if child_done && running.is_empty() && quiet_since.elapsed() >= QUIET {
            break;
        }
        std::thread::sleep(POLL);
    }
    let output = launched.finish_output();
    TreeExit {
        code,
        cancelled,
        output,
    }
}

fn kill_from(dir: &Path) {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes();
    for process in sys.processes().values() {
        if let Some(exe) = process.exe() {
            if game_launch::is_subpath(exe, dir) {
                process.kill();
            }
        }
    }
}

/// Every file under `root`, `/`-separated and relative to it, skipping the VFS's markers.
fn written_files(root: &Path) -> Result<Vec<String>, ToolError> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<String>) -> Result<(), ToolError> {
        let cur = if rel.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        for entry in std::fs::read_dir(&cur)? {
            let entry = entry?;
            let name = entry.file_name();
            if rel.as_os_str().is_empty() && name.to_string_lossy() == WHITEOUT_DIR {
                continue;
            }
            let child = rel.join(&name);
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                walk(root, &child, out)?;
            } else if meta.is_file() {
                out.push(child.to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if root.is_dir() {
        walk(root, Path::new(""), &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// The paths a run deleted, from the VFS's markers `<staging>\.agvfs-wh\<path>.wh`.
fn read_markers(staging: &Path) -> Result<Vec<RelPath>, ToolError> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), ToolError> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                walk(&entry.path(), &child, out)?;
            } else if meta.is_file() {
                if let Some(path) = child.strip_suffix(WHITEOUT_SUFFIX) {
                    if !path.is_empty() {
                        out.push(path.to_string());
                    }
                }
            }
        }
        Ok(())
    }
    let dir = staging.join(WHITEOUT_DIR);
    let mut names = Vec::new();
    if dir.is_dir() {
        walk(&dir, "", &mut names)?;
    }
    names.sort();
    names
        .into_iter()
        .map(|n| RelPath::new(n).map_err(|e| ToolError::Other(format!("invalid deletion: {e}"))))
        .collect()
}

/// Make a finished run's folder the tool's next generation. The previous current generation is
/// kept for rollback, and the one before it is deleted.
fn promote(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    fingerprint: &str,
) -> Result<(String, Option<String>, Vec<RelPath>), ToolError> {
    let deletions = read_markers(staging)?;
    let markers = staging.join(WHITEOUT_DIR);
    if markers.exists() {
        std::fs::remove_dir_all(&markers)?;
    }

    let mut manifest = load_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    let position = generated_position(&layers, tool.id.as_str());
    let current = position.and_then(|i| match &layers[i].source {
        LayerSource::Generated {
            generation, inputs, ..
        } => Some(Generation {
            generation: generation.clone(),
            inputs: inputs.clone(),
            whiteouts: layers[i].whiteouts.clone(),
        }),
        _ => None,
    });

    let mut record = read_record(dir)?;
    let mut numbers = generation_numbers(dir)?;
    numbers.extend(
        current
            .iter()
            .filter_map(|g| g.generation.parse::<u64>().ok()),
    );
    numbers.extend(
        record
            .previous
            .iter()
            .filter_map(|g| g.generation.parse::<u64>().ok()),
    );
    let next = numbers.into_iter().max().unwrap_or(0) + 1;
    let next_name = next.to_string();

    std::fs::rename(staging, dir.join(&next_name))?;

    record.previous = current.clone();
    write_record(dir, &record)?;

    let inputs = InputFingerprint::Known(fingerprint.to_string());
    match position {
        Some(i) => {
            layers[i].source = LayerSource::Generated {
                tool: tool.id.clone(),
                generation: next_name.clone(),
                inputs,
            };
            layers[i].whiteouts = deletions.clone();
        }
        None => layers.push(generated_layer(
            &tool.id,
            &next_name,
            inputs,
            deletions.clone(),
        )?),
    }
    save_layers(ctx, instance_id, definition, &mut manifest, layers)?;

    // Only the new current and the previous generation stay on disk.
    let keep: BTreeSet<String> = std::iter::once(next_name.clone())
        .chain(current.as_ref().map(|g| g.generation.clone()))
        .collect();
    for n in generation_numbers(dir)? {
        if !keep.contains(&n.to_string()) {
            std::fs::remove_dir_all(dir.join(n.to_string()))?;
        }
    }

    Ok((next_name, current.map(|g| g.generation), deletions))
}

/// Keep a discarded run's folder as `failed-<run>`, and the newest few of those. The tool's last
/// output goes beside the run's files, so the player can see why it failed.
fn discard(dir: &Path, staging: &Path, run: &str, exit: &TreeExit) -> Result<PathBuf, ToolError> {
    let failed = dir.join(format!("failed-{run}"));
    std::fs::rename(staging, &failed)?;
    if !exit.output.stdout.is_empty() {
        std::fs::write(
            failed.join("tool-stdout.txt"),
            exit.output.stdout.as_bytes(),
        )?;
    }
    if !exit.output.stderr.is_empty() {
        std::fs::write(
            failed.join("tool-stderr.txt"),
            exit.output.stderr.as_bytes(),
        )?;
    }
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("failed-") && entry.file_type()?.is_dir() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    let excess = names.len().saturating_sub(FAILED_RUNS_KEPT);
    for name in names.iter().take(excess) {
        std::fs::remove_dir_all(dir.join(name))?;
    }
    Ok(failed)
}

/// The consecutive discarded runs of a tool in an instance, kept in the tool's folder (MASTER_SPEC §26.9,
/// decision 3). `not_swap` is whether any of them was captured other than by swap, and `injected` whether
/// one of them had the virtual file system end a program it started. The count is read leniently: a missing or corrupt
/// file is a count of nothing, so it never blocks a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FailureCount {
    #[serde(default)]
    discarded: u32,
    /// Whether any of the discarded runs was captured other than by swap.
    #[serde(default)]
    not_swap: bool,
    /// Whether any of the discarded runs had the virtual file system end a program it started.
    #[serde(default)]
    injected: bool,
}

fn read_failures(dir: &Path) -> FailureCount {
    std::fs::read(dir.join(FAILURES_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Save the count. Best effort: a count that cannot be saved only means an offer comes later.
fn write_failures(dir: &Path, count: FailureCount) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&count) {
        let _ = std::fs::write(dir.join(FAILURES_FILE), bytes);
    }
}

/// Count one more discarded run of a tool, and return the count with it.
fn note_discard(dir: &Path, swap: bool, injected: bool) -> FailureCount {
    let mut count = read_failures(dir);
    count.discarded = count.discarded.saturating_add(1);
    count.not_swap |= !swap;
    count.injected |= injected;
    write_failures(dir, count);
    count
}

/// Settle the offers of a run that came back (MASTER_SPEC §26.9, decision 3). A promoted run clears the
/// count. A discarded run adds to it, and once the tool has been discarded [`OFFER_AFTER_DISCARDS`] times
/// in a row it offers the full-copy swap, when a failing run was not a swap, and switching the instance to
/// links, when a failure involved injection and the tool does not work on the real install. Nothing is
/// switched here. If the swap's costs cannot be read, the swap is not offered, and the run still stands.
fn with_offers(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    mut outcome: RunOutcome,
    injected: bool,
) -> RunOutcome {
    let Ok(instance_dir) = ctx.paths.instance_dir(instance_id) else {
        return outcome;
    };
    let dir = tool_dir(&instance_dir, tool_id.as_str());
    if outcome.promoted {
        let _ = std::fs::remove_file(dir.join(FAILURES_FILE));
        return outcome;
    }
    let count = note_discard(&dir, outcome.capture == CaptureMethod::Swap, injected);
    outcome
        .offers
        .extend(offers_after(ctx, instance_id, definition, tool, count));
    outcome
}

/// What a tool discarded `count` times in a row is offered (MASTER_SPEC §26.9, decision 3): nothing before
/// [`OFFER_AFTER_DISCARDS`]; then the full-copy swap when a failing run was not a swap; and switching the
/// instance to links when a failure involved injection and the tool does not work on the real install.
fn offers_after(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    count: FailureCount,
) -> Vec<ToolOffer> {
    let mut offers = Vec::new();
    if count.discarded < OFFER_AFTER_DISCARDS {
        return offers;
    }
    if count.not_swap {
        if let Ok(costs) = game_tool_copy::swap_costs(ctx, instance_id, definition) {
            offers.push(ToolOffer::CopySwap { costs });
        }
    }
    if count.injected && !tool.uses_install_path {
        offers.push(ToolOffer::SwitchToLinks);
    }
    offers
}

/// A run the virtual file system refused to start: nothing ran, but it is a failure of the tool all the
/// same. It counts as a discarded run that involved injection, so a tool whose virtual file system keeps
/// failing is offered switching to links on the second refusal (MASTER_SPEC §26.9, decision 3). The count
/// is best effort: if the instance folder is unknown, the refusal carries no offers.
fn refused_vfs(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    reason: String,
    next: CaptureMode,
) -> ToolError {
    let offers = match ctx.paths.instance_dir(instance_id) {
        Ok(instance_dir) => {
            let dir = tool_dir(&instance_dir, tool_id.as_str());
            let count = note_discard(&dir, false, true);
            offers_after(ctx, instance_id, definition, tool, count)
        }
        Err(_) => Vec::new(),
    };
    ToolError::VfsUnavailable {
        reason,
        next: Some(next),
        offers,
    }
}

/// Run a tool in an instance and capture what it writes (MASTER_SPEC §26.9): under the virtual file
/// system, or from a linked deployment, as `capture` asks. Then promote the output if the tool exited
/// 0 and discard it otherwise.
///
/// Under `Auto` a virtual instance whose virtual file system cannot start is refused with
/// [`ToolError::VfsUnavailable`] and its next tier, and nothing runs: the caller asks the user before it
/// captures from that tier. A linked or copied instance is captured from links directly. Under `Vfs` the
/// start failure is refused the same way. A run that comes back discarded twice in a row carries offers
/// (MASTER_SPEC §26.9, decision 3).
///
/// The instance lock is held for the whole run, and the game's user files are swapped in for it, as
/// for a launch, so the tool and the game never run at once.
///
/// `CaptureMode::Swap` has costs (see [`game_tool_copy::swap_costs`]), so it is refused with
/// [`ToolError::SwapNotConfirmed`] unless `swap_costs_acknowledged` is true. The caller passes true
/// only after it showed the user those costs and the user accepted them; the flag is not read for
/// any other capture.
#[allow(clippy::too_many_arguments)]
pub fn run(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    capture: CaptureMode,
    swap_costs_acknowledged: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
) -> Result<RunOutcome, ToolError> {
    let tool = as_installed(
        ctx,
        instance_id,
        definition,
        declared_tool(ctx, definition, tool_id)?,
    );
    run_inner(
        ctx,
        instance_id,
        definition,
        &tool,
        tool_id,
        capture,
        swap_costs_acknowledged,
        launcher,
        cancel,
        None,
        None,
        false,
        None,
    )
}

/// Run an item-scoped tool for one content item (MASTER_SPEC §30.12). The item's path in the view
/// is passed wherever the recipe says `{item}`, and the tool's input fingerprint is narrowed to
/// that item.
#[allow(clippy::too_many_arguments)]
pub fn run_for_item(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    item_id: &str,
    capture: CaptureMode,
    swap_costs_acknowledged: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
) -> Result<RunOutcome, ToolError> {
    let tool = as_installed(
        ctx,
        instance_id,
        definition,
        declared_tool(ctx, definition, tool_id)?,
    );
    if tool.scope != ToolScope::Item {
        return Err(ToolError::InvalidInput {
            tool: tool_id.to_string(),
            input: "scope".into(),
            reason: "this tool runs once for the instance, not per item".into(),
        });
    }
    if item_view_path(ctx, instance_id, item_id)?.is_none() {
        return Err(ToolError::InvalidInput {
            tool: tool_id.to_string(),
            input: "item".into(),
            reason: format!("'{item_id}' is not content of this instance"),
        });
    }
    run_inner(
        ctx,
        instance_id,
        definition,
        &tool,
        tool_id,
        capture,
        swap_costs_acknowledged,
        launcher,
        cancel,
        None,
        None,
        false,
        Some(item_id),
    )
}

/// A declared tool as it runs in this instance: from where the deployment provides its executable
/// (MASTER_SPEC §26.9). A mod may ship the tool in another folder than the package declares; it runs
/// from there, and so does its working folder when the package's is the executable's own folder.
/// When the deployment cannot be read here, the declared recipe is kept and the run reports what
/// is wrong with it.
fn as_installed(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    mut tool: ToolDefinition,
) -> ToolDefinition {
    let Ok(found) = crate::game_user_tools::installed_known_tools(ctx, instance_id, definition)
    else {
        return tool;
    };
    let Some(found) = found.get(tool.id.as_str()) else {
        return tool;
    };
    if found.at_declared_path {
        return tool;
    }
    let Ok(exe) = RelPath::new(found.exe.as_str()) else {
        return tool;
    };
    let parent_of = |path: &str| {
        path.rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default()
    };
    if let GamePath::Runtime { path: declared } = &tool.launch.executable {
        let declared_parent = parent_of(declared.as_str());
        if let GamePath::Runtime { path: working } = &tool.launch.working_directory {
            if fold_path_case(working.as_str()) == fold_path_case(&declared_parent) {
                if let Ok(folder) = RelPath::new(parent_of(found.exe.as_str())) {
                    tool.launch.working_directory = GamePath::Runtime { path: folder };
                }
            }
        }
    }
    tool.launch.executable = GamePath::Runtime { path: exe };
    tool
}

/// Run a tool by the tier the user chose after the virtual file system refused it (MASTER_SPEC §26.9): link
/// capture or the full-copy swap. `refusal` is the refusal's reason, and the outcome's `capture_reason` names
/// it, so the record says what failed and which tier ran instead. Only `Links` and `Swap` are accepted.
/// `swap_costs_acknowledged` is as for [`run`]: a `Swap` is refused without it.
#[allow(clippy::too_many_arguments)]
pub fn run_after_vfs_refusal(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    next: CaptureMode,
    swap_costs_acknowledged: bool,
    refusal: &str,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
) -> Result<RunOutcome, ToolError> {
    let tool = as_installed(
        ctx,
        instance_id,
        definition,
        declared_tool(ctx, definition, tool_id)?,
    );
    match next {
        CaptureMode::Links | CaptureMode::Swap => run_inner(
            ctx,
            instance_id,
            definition,
            &tool,
            tool_id,
            next,
            swap_costs_acknowledged,
            launcher,
            cancel,
            Some(refusal),
            None,
            false,
            None,
        ),
        CaptureMode::Vfs | CaptureMode::Auto => Err(ToolError::Other(
            "only link capture or the full-copy swap can follow a refused virtual file system run"
                .into(),
        )),
    }
}

/// The user's tool in an instance's list.
fn find_user_tool(ctx: &Ctx, instance_id: &str, id: &str) -> Result<UserTool, ToolError> {
    crate::game_user_tools::list(ctx, instance_id)
        .map_err(|e| ToolError::Invalid(e.to_string()))?
        .into_iter()
        .find(|tool| tool.id == id)
        .ok_or_else(|| ToolError::Invalid(format!("the instance has no user tool '{id}'")))
}

/// A user tool's run: a tool the user added runs through the same pipeline as a declared tool, under
/// the same capture (MASTER_SPEC §26.9). A user tool whose `known_tool_id` names a declared tool runs
/// as that tool. A tool that wrote a whole mod has its output imported after a successful run.
#[allow(clippy::too_many_arguments)]
pub fn run_user_tool(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    user_tool_id: &str,
    capture: CaptureMode,
    swap_costs_acknowledged: bool,
    move_aside: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
) -> Result<RunOutcome, ToolError> {
    run_user_inner(
        ctx,
        instance_id,
        definition,
        user_tool_id,
        capture,
        swap_costs_acknowledged,
        move_aside,
        launcher,
        cancel,
        None,
    )
}

/// [`run_user_tool`] by the tier the user chose after the virtual file system refused it.
#[allow(clippy::too_many_arguments)]
pub fn run_user_tool_after_vfs_refusal(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    user_tool_id: &str,
    next: CaptureMode,
    swap_costs_acknowledged: bool,
    refusal: &str,
    move_aside: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
) -> Result<RunOutcome, ToolError> {
    match next {
        CaptureMode::Links | CaptureMode::Swap => run_user_inner(
            ctx,
            instance_id,
            definition,
            user_tool_id,
            next,
            swap_costs_acknowledged,
            move_aside,
            launcher,
            cancel,
            Some(refusal),
        ),
        CaptureMode::Vfs | CaptureMode::Auto => Err(ToolError::Other(
            "only link capture or the full-copy swap can follow a refused virtual file system run"
                .into(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_user_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    user_tool_id: &str,
    capture: CaptureMode,
    swap_costs_acknowledged: bool,
    move_aside: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
    fallback: Option<&str>,
) -> Result<RunOutcome, ToolError> {
    let user = find_user_tool(ctx, instance_id, user_tool_id)?;
    if let Some(known) = user.known_tool_id.as_deref() {
        if let Ok(id) = ToolId::new(known) {
            if let Some(declared) = ctx.games.tool(&definition.id, &id).cloned() {
                let declared = as_installed(ctx, instance_id, definition, declared);
                return run_inner(
                    ctx,
                    instance_id,
                    definition,
                    &declared,
                    &id,
                    capture,
                    swap_costs_acknowledged,
                    launcher,
                    cancel,
                    fallback,
                    None,
                    move_aside,
                    None,
                );
            }
        }
    }
    let tool = user_tool_definition(definition, &user)?;
    let external = if crate::game_user_tools::is_absolute_exe(&user.exe) {
        Some(PathBuf::from(&user.exe))
    } else {
        None
    };
    run_inner(
        ctx,
        instance_id,
        definition,
        &tool,
        &tool.id,
        capture,
        swap_costs_acknowledged,
        launcher,
        cancel,
        fallback,
        external.as_deref(),
        move_aside,
        None,
    )
}

/// The reason a run captured by a tier the user chose after a refusal gives (MASTER_SPEC §26.9).
fn fell_back_reason(refusal: &str, tier: &str) -> String {
    format!(
        "the virtual file system could not start ({}), and {tier} was chosen",
        refusal.trim_end_matches('.')
    )
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    capture: CaptureMode,
    swap_costs_acknowledged: bool,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
    fallback: Option<&str>,
    external: Option<&Path>,
    move_aside: bool,
    scope_item: Option<&str>,
) -> Result<RunOutcome, ToolError> {
    if capture == CaptureMode::Swap && !swap_costs_acknowledged {
        return Err(ToolError::SwapNotConfirmed);
    }
    let record = crate::game_instance::get(ctx, instance_id)?
        .ok_or_else(|| ToolError::InstanceNotFound(instance_id.to_string()))?;
    let base_id = match &record.base {
        BaseReference::Pinned { id, .. } => id.clone(),
        BaseReference::Unpinned { .. } => {
            return Err(ToolError::Unpinned(instance_id.to_string()));
        }
    };
    let base = load_base(ctx, &base_id)?;
    let store = base.runtime.store.clone();
    refuse_during_session(ctx, definition, &store)?;

    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let external = external_program(external)?;
    let mut effective = tool.clone();
    let (output_dir, configured) = match &tool.output {
        ToolOutput::GeneratedLayer => (None, None),
        ToolOutput::Mod {
            folder,
            user_configured,
        } => {
            if let Some(template) = &tool.output_arg {
                // DynDOLOD and TexGen take the folder as an argument (`-o:"[path]"`,
                // https://dyndolod.info/Help/Command-Line-Argument). Agora gives each run its own
                // folder under the instance, so another manager's folder is never touched.
                let dir = instance_dir
                    .join("tool-output")
                    .join(tool.id.as_str())
                    .join(format!("run-{}", new_run_id()));
                let rendered = template.replace("{output}", &dir.to_string_lossy());
                for arg in &mut effective.launch.arguments {
                    if let agora_game_api::LaunchValue::Literal { value } = arg {
                        if value.contains("{output}") {
                            *value = value.replace("{output}", &dir.to_string_lossy());
                        }
                    }
                }
                effective
                    .launch
                    .arguments
                    .push(agora_game_api::LaunchValue::Literal { value: rendered });
                (Some(dir), None)
            } else {
                let program = tool_program_path(&base, instance_id, tool, external.as_deref());
                let raw_folder = if *user_configured && folder.trim().is_empty() {
                    user_configured_folder(ctx, instance_id, tool)?
                } else {
                    folder.clone()
                };
                let dir = resolve_mod_folder(&raw_folder, &program)?;
                let forbidden = output_forbidden_roots(ctx, instance_id, &base)?;
                let tool_root = program.parent().map(real_path);
                let configured = ConfiguredOutput::new(
                    dir,
                    forbidden,
                    tool_root,
                    &instance_dir,
                    tool,
                    move_aside,
                )?;
                (Some(configured.folder.clone()), Some(configured))
            }
        }
    };

    // The player's stored input values (MASTER_SPEC §30.12). A required one with no value refuses
    // the run before anything is deployed. `{item}` is the chosen item's path in the view.
    let item_path = match scope_item {
        Some(item) => item_view_path(ctx, instance_id, item)?,
        None => None,
    };
    if effective.scope == ToolScope::Item && item_path.is_none() {
        return Err(ToolError::ItemScopeWithoutItem {
            tool: tool_id.to_string(),
        });
    }
    let stored = read_tool_inputs(ctx, instance_id)?
        .get(effective.id.as_str())
        .cloned()
        .unwrap_or_default();
    for argument in tool_input_arguments(&effective, &stored, item_path.as_deref())? {
        effective
            .launch
            .arguments
            .push(agora_game_api::LaunchValue::Literal {
                value: argument.to_string_lossy().into_owned(),
            });
    }
    if let Some(item) = item_path.as_deref() {
        for argument in &mut effective.launch.arguments {
            if let agora_game_api::LaunchValue::Literal { value } = argument {
                if value.contains("{item}") {
                    *value = value.replace("{item}", item);
                }
            }
        }
    }

    let tool = &effective;

    // Decided before anything is deployed (MASTER_SPEC §26.9, decision 1). Under `auto` a virtual instance
    // runs every tool under the virtual file system; a linked or copied instance runs a tool that uses its
    // own folder by link capture directly, and a tool that works on the real install needs the swap offer.
    let preferred = preferred_mode(&load_manifest(ctx, instance_id)?, definition);
    let route = match capture {
        CaptureMode::Links => Route::Links(Some(match fallback {
            Some(refusal) => fell_back_reason(refusal, "link capture"),
            None => "linked capture was asked for".to_string(),
        })),
        CaptureMode::Swap => Route::Swap(Some(match fallback {
            Some(refusal) => fell_back_reason(refusal, "the full-copy swap"),
            None => "the tool works on the real install's Data folder, and swap was asked for"
                .to_string(),
        })),
        CaptureMode::Auto if preferred != DeployMode::Virtual => {
            if tool.uses_install_path {
                return Err(ToolError::InstallPathNeedsSwap {
                    tool: tool_id.to_string(),
                    instance_id: instance_id.to_string(),
                });
            }
            Route::Links(Some(format!(
                "the instance is set to {}, so the tool is captured from links without trying the virtual file system",
                preferred.plain_name()
            )))
        }
        CaptureMode::Vfs | CaptureMode::Auto => match launcher.locate_vfs_dll() {
            Ok(dll) => Route::Vfs(dll),
            Err(reason) => {
                let next = next_capture(tool);
                return Err(refused_vfs(
                    ctx,
                    instance_id,
                    definition,
                    tool,
                    tool_id,
                    reason,
                    next,
                ));
            }
        },
    };

    let _lock = ctx
        .lock_manager
        .acquire(LockResource::Instance(instance_id.to_string()), "tool-run")?;

    // An interrupted swap for this game is put back before anything else runs (MASTER_SPEC §26.9).
    game_tool_swap::recover_game(ctx, definition)?;

    match route {
        Route::Swap(reason) => {
            let outcome = run_by_swap(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                &base,
                launcher,
                cancel,
                reason,
                external.as_deref(),
                output_dir.as_deref(),
                configured.as_ref(),
                scope_item,
            )?;
            Ok(with_offers(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                outcome,
                false,
            ))
        }
        Route::Vfs(dll) => {
            let (outcome, injected) = match run_under_vfs(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                &base,
                dll,
                launcher,
                cancel,
                external.as_deref(),
                output_dir.as_deref(),
                configured.as_ref(),
                scope_item,
            ) {
                Ok(found) => found,
                // The program would not start under the VFS, after the instance was deployed: nothing ran,
                // and the refusal is counted like one from the locate step.
                Err(ToolError::VfsUnavailable { reason, next, .. }) => {
                    let next = next.unwrap_or_else(|| next_capture(tool));
                    return Err(refused_vfs(
                        ctx,
                        instance_id,
                        definition,
                        tool,
                        tool_id,
                        reason,
                        next,
                    ));
                }
                Err(e) => return Err(e),
            };
            Ok(with_offers(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                outcome,
                injected,
            ))
        }
        Route::Links(reason) => {
            let outcome = run_from_links(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                &base,
                launcher,
                cancel,
                reason,
                external.as_deref(),
                output_dir.as_deref(),
                configured.as_ref(),
                scope_item,
            )?;
            Ok(with_offers(
                ctx,
                instance_id,
                definition,
                tool,
                tool_id,
                outcome,
                false,
            ))
        }
    }
}

/// Where a tool run goes: under the virtual file system (with its DLL), or from links with the reason.
enum Route {
    Vfs(PathBuf),
    Links(Option<String>),
    Swap(Option<String>),
}

/// The deployment an instance is set to, or the game's default when the user has not chosen one.
fn preferred_mode(manifest: &GameInstanceManifest, definition: &GameDefinition) -> DeployMode {
    manifest.deployment.unwrap_or(match definition.deployment {
        DeploymentStrategy::VirtualFileSystem | DeploymentStrategy::Redirect => DeployMode::Virtual,
    })
}

/// The tier a run is captured by when the virtual file system cannot start: link capture for a tool that
/// uses its own folder, the full-copy swap for one that works on the real install (MASTER_SPEC §26.9).
fn next_capture(tool: &ToolDefinition) -> CaptureMode {
    if tool.uses_install_path {
        CaptureMode::Swap
    } else {
        CaptureMode::Links
    }
}

/// What a refused virtual file system run says, and what the next tier means for it.
fn vfs_unavailable_message(reason: &str, next: &Option<CaptureMode>) -> String {
    let what = format!("the virtual file system could not start: {reason}. Nothing was run.");
    match next {
        Some(CaptureMode::Swap) => format!(
            "{what} A swap would run the tool on a copy of the instance's Data, swapped in for the run: it is slower, and it needs free disk space."
        ),
        Some(CaptureMode::Links) => format!(
            "{what} Link capture would run the tool in the deployment's folder, and capture what it writes from the linked files."
        ),
        _ => what,
    }
}

/// A run under the virtual file system: the instance is deployed in the VFS, the tool runs with its
/// staging folder as the writable layer, and its output is settled. The caller holds the lock. Also
/// returns whether the VFS ended a program the run started, which is an injection failure (MASTER_SPEC
/// §26.9, decision 3).
#[allow(clippy::too_many_arguments)]
fn run_under_vfs(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    base: &BaseManifest,
    dll: PathBuf,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
    external: Option<&Path>,
    output_dir: Option<&Path>,
    configured: Option<&ConfiguredOutput>,
    scope_item: Option<&str>,
) -> Result<(RunOutcome, bool), ToolError> {
    let store = base.runtime.store.clone();
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let game_dir = deployment_folder(base, instance_id)?;
    let writable = instance_dir.join("writable");

    deploy_locked(ctx, instance_id, definition, DeployMode::Virtual)?;
    let fingerprint = input_fingerprint_scoped(ctx, instance_id, definition, tool, scope_item)?;

    let roots = LaunchRoots {
        runtime: game_dir.clone(),
        install: Some(base.source_location.clone()),
        base: Some(base.location.clone()),
    };
    let resolved = resolve_tool_launch(tool, &roots, external)?;

    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let run = new_run_id();
    let staging = dir.join(format!("staging-{run}"));
    std::fs::create_dir_all(&staging)?;

    let mut prepared = PreparedLaunch::undeployed(resolved, Vec::new(), None);
    prepared.deployment = Some(DeployMode::Virtual);
    // The game folder is mounted; its lowers are the instance's writable layer over the farm, so
    // the tool sees what the game wrote but every write lands in this run's staging folder. The
    // real install folder is an alias, so a tool that reads or writes it there (Nemesis, BodySlide)
    // sees the same mod files at its own path, and its writes land in the same staging folder.
    let log = instance_dir.join("logs").join("vfs.log");
    prepared.vfs = Some(VfsLaunch {
        dll_x86: Some(game_launch::locate_dll_x86(&dll)),
        dll,
        mount: game_dir.clone(),
        upper: staging.clone(),
        lowers: vec![writable, game_dir.clone()],
        aliases: vec![base.source_location.clone()],
        config_path: instance_dir.join("vfs").join(VFS_CONFIG),
        log: log.clone(),
    });

    if let Err(e) = game_user_files::swap_in(ctx, instance_id, definition, &store, &game_dir) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e.into());
    }
    if let Some(configured) = configured {
        if let Err(e) = configured.move_aside() {
            release_session(ctx, definition, &store)?;
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    }

    // What the VFS's log held before the launch, so what it says about this run is read after it.
    let log_from = game_launch::log_len(&log);
    // The real install is watched from just before the tool starts until it exits (MASTER_SPEC §26.9).
    let watch = InstallWatch::start(&base.source_location);
    let mut launched = match launcher.launch_capturing(&prepared) {
        Ok(launched) => launched,
        Err(e) => {
            release_session(ctx, definition, &store)?;
            let _ = std::fs::remove_dir_all(&staging);
            return Err(match e {
                LaunchError::VfsUnavailable { reason } => ToolError::VfsUnavailable {
                    reason,
                    next: Some(next_capture(tool)),
                    // Counted by the caller, which sees the refusal (see `run_inner`).
                    offers: Vec::new(),
                },
                other => ToolError::Launch(other),
            });
        }
    };
    let _ = game_user_files::record_process(ctx, &definition.id, &store, launched.identity.clone());

    let exit = wait_for_tree(&game_dir, &mut launched, cancel);
    let installed = watch.finish();
    let injected = !game_launch::processes_ended_since(&log, log_from).is_empty();
    // The copies the VFS made of files the tool only opened for writing are not output, so they go before
    // the output is settled and checked (MASTER_SPEC §26.9).
    let settled = drop_unchanged_outputs(ctx, instance_id, definition, tool, &dir, &staging, &run)
        .and_then(|dropped| {
            settle_run(
                ctx,
                instance_id,
                definition,
                tool,
                &dir,
                &staging,
                &run,
                &fingerprint,
                &exit,
                output_dir,
            )
            .map(|settled| (settled, dropped))
        });
    // The game's files go back whatever became of the output, and a failure to settle is reported
    // only after that.
    let released = release_session(ctx, definition, &store);
    let (settled, unchanged_dropped) = settled?;
    released?;

    let mut out = outcome(tool_id, tool, run, &exit, settled, CaptureMethod::Vfs, None);
    out.install_changes = installed;
    out.unchanged_dropped = unchanged_dropped;
    Ok((out, injected))
}

/// The content layer that provides a tool's executable, when one does: the layer whose file wins the
/// executable's path among the layers that hold it (MASTER_SPEC §26.9). That layer is copied for the
/// tool's run, so the tool can rewrite its own files. `None` when the executable is not a content
/// layer's file (a base file, or the instance's own output) or the recipe names no runtime path.
fn executable_layer(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
) -> Result<Option<String>, ToolError> {
    let GamePath::Runtime { path } = &tool.launch.executable else {
        return Ok(None);
    };
    let planned = crate::game_deploy::plan(ctx, instance_id, definition, DeployMode::Links)?;
    let winner = planned
        .files
        .iter()
        .find(|file| fold_path_case(file.path.as_str()) == fold_path_case(path.as_str()));
    Ok(winner.and_then(|file| match file.source {
        crate::game_deploy::FileSource::Content { .. } => Some(file.layer_id.clone()),
        _ => None,
    }))
}

/// A run captured from a linked deployment (MASTER_SPEC §26.9). The instance is deployed in Links
/// mode, which records each file's placement, size and source: that record is the "before". The tool
/// runs in the game's folder, and what it changed is compared with the record. New files and changed
/// copies become the output; deletions become whiteouts; a changed link is an error and the run is
/// discarded. Then the output settles as a VFS run's does, and the farm is taken away, so the tool's
/// writes never become the game's writable layer. The caller holds the lock.
#[allow(clippy::too_many_arguments)]
fn run_from_links(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    base: &BaseManifest,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
    reason: Option<String>,
    external: Option<&Path>,
    output_dir: Option<&Path>,
    configured: Option<&ConfiguredOutput>,
    scope_item: Option<&str>,
) -> Result<RunOutcome, ToolError> {
    let store = base.runtime.store.clone();
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let game_dir = deployment_folder(base, instance_id)?;
    let deployment_dir = game_dir
        .parent()
        .ok_or_else(|| ToolError::Other("the deployment has no folder".into()))?
        .to_path_buf();

    let copied = executable_layer(ctx, instance_id, definition, tool)?;
    deploy_locked_with_copy(
        ctx,
        instance_id,
        definition,
        DeployMode::Links,
        copied.as_deref(),
    )?;
    let before = read_deployment_record(&deployment_dir)?;
    let fingerprint = input_fingerprint_scoped(ctx, instance_id, definition, tool, scope_item)?;

    let roots = LaunchRoots {
        runtime: game_dir.clone(),
        install: Some(base.source_location.clone()),
        base: Some(base.location.clone()),
    };
    let resolved = resolve_tool_launch(tool, &roots, external)?;

    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let run = new_run_id();
    let staging = dir.join(format!("staging-{run}"));

    // No VFS: the tool's working folder is the game's folder, so its writes land in the farm.
    let mut prepared = PreparedLaunch::undeployed(resolved, Vec::new(), None);
    prepared.deployment = Some(DeployMode::Links);

    game_user_files::swap_in(ctx, instance_id, definition, &store, &game_dir)?;
    if let Some(configured) = configured {
        if let Err(e) = configured.move_aside() {
            let _ = discard_deployment(&deployment_dir, instance_id);
            release_session(ctx, definition, &store)?;
            return Err(e);
        }
    }

    // The real install is watched from just before the tool starts until it exits (MASTER_SPEC §26.9).
    let watch = InstallWatch::start(&base.source_location);
    let mut launched = match launcher.launch_capturing(&prepared) {
        Ok(launched) => launched,
        Err(e) => {
            release_session(ctx, definition, &store)?;
            return Err(ToolError::Launch(e));
        }
    };
    let _ = game_user_files::record_process(ctx, &definition.id, &store, launched.identity.clone());

    let exit = wait_for_tree(&game_dir, &mut launched, cancel);
    let installed = watch.finish();
    let settled = capture_from_links(
        ctx,
        instance_id,
        definition,
        tool,
        &dir,
        &staging,
        &run,
        &fingerprint,
        &game_dir,
        &before,
        &exit,
        output_dir,
    );
    // The farm holds whatever the tool wrote. It goes whatever became of the output, and without a
    // harvest: the writes were captured above, and the next launch deploys the promoted output.
    let forgotten = discard_deployment(&deployment_dir, instance_id);
    let released = release_session(ctx, definition, &store);
    let settled = settled?;
    forgotten?;
    released?;

    let mut out = outcome(
        tool_id,
        tool,
        run,
        &exit,
        settled,
        CaptureMethod::Links,
        reason,
    );
    out.copied_layer = copied;
    out.install_changes = installed;
    Ok(out)
}

/// A run that works on the real install's `Data` folder (MASTER_SPEC §26.9, slice 4e). The instance's
/// `Data` is copied as real, writable files (the copy is kept between runs on the same volume, or made
/// for this run on the install's volume). The real `Data` is renamed aside, the copy is renamed into its
/// place, and the tool runs from the install folder, so it sees the real path. When it exits the copy
/// is renamed back and the real `Data` with it. The copy's changes are captured from the copy against
/// its record, the way link capture does, then the copy is brought back in line with the plan. The
/// instance's normal farm is not touched. The caller holds the lock.
#[allow(clippy::too_many_arguments)]
fn run_by_swap(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    tool_id: &ToolId,
    base: &BaseManifest,
    launcher: &dyn Launcher,
    cancel: &CancellationToken,
    reason: Option<String>,
    external: Option<&Path>,
    output_dir: Option<&Path>,
    configured: Option<&ConfiguredOutput>,
    scope_item: Option<&str>,
) -> Result<RunOutcome, ToolError> {
    let store = base.runtime.store.clone();
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let run = new_run_id();

    // Decided from trusted inputs before anything is created or moved.
    let copy = game_tool_copy::copy_plan(ctx, instance_id, definition, &run)?;
    let install = copy.location.install.clone();
    let real_data = install.join("Data");
    let aside = install.join(format!("{}{run}", game_tool_swap::ASIDE_PREFIX));
    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let staging = dir.join(format!("staging-{run}"));

    game_tool_swap::preflight(&install, &real_data, &aside, &copy.space_need())?;
    let fingerprint = input_fingerprint_scoped(ctx, instance_id, definition, tool, scope_item)?;

    // The user files go in before the real Data moves, so the swap journal is the only swap in progress
    // for this game when the recovery check runs.
    game_user_files::swap_in(ctx, instance_id, definition, &store, &install)?;

    let journal = SwapJournal {
        version: game_tool_swap::JOURNAL_VERSION,
        game: definition.id.to_string(),
        store: store.as_str().to_string(),
        instance_id: instance_id.to_string(),
        tool: tool_id.to_string(),
        run: run.clone(),
        started_unix_ms: now_unix_ms(),
        install_dir: install.clone(),
        real_data: real_data.clone(),
        aside: aside.clone(),
        copy_kind: copy.kind,
        copy_dir: copy.copy_dir.clone(),
        swap_dir: copy.location.swap_dir.clone(),
    };
    // The journal is written, and made durable, before the copy or the real Data is touched.
    if let Err(e) = game_tool_swap::write_journal(ctx, &journal) {
        let _ = release_session(ctx, definition, &store);
        return Err(e.into());
    }

    let (before, prepared) = match game_tool_copy::prepare(ctx, &journal, &copy.files) {
        Ok(prepared) => prepared,
        Err(e) => return Err(abandon_swap(ctx, definition, &store, &journal, e.into())),
    };
    if let Err(e) = game_tool_swap::engage(&journal) {
        return Err(abandon_swap(ctx, definition, &store, &journal, e.into()));
    }
    if let Some(configured) = configured {
        if let Err(e) = configured.move_aside() {
            return Err(put_back_after(ctx, definition, &store, &journal, e));
        }
    }

    // The tool runs from the install folder, with the copy in place of `Data`: a program the recipe starts
    // from `Data` runs the copy's file, and a program from the install root runs the real file. The recipe
    // is resolved now, because resolving it checks that the program is there.
    let roots = LaunchRoots {
        runtime: install.clone(),
        install: Some(install.clone()),
        base: Some(base.location.clone()),
    };
    let resolved = match resolve_tool_launch(tool, &roots, external) {
        Ok(resolved) => resolved,
        Err(e) => {
            return Err(put_back_after(
                ctx,
                definition,
                &store,
                &journal,
                ToolError::Launch(e),
            ))
        }
    };

    let mut prepared_launch = PreparedLaunch::undeployed(resolved, Vec::new(), None);
    prepared_launch.deployment = Some(DeployMode::Copies);

    let watch = InstallWatch::start(&install);
    let mut launched = match launcher.launch_capturing(&prepared_launch) {
        Ok(launched) => launched,
        Err(e) => {
            let _ = watch.finish();
            return Err(put_back_after(
                ctx,
                definition,
                &store,
                &journal,
                ToolError::Launch(e),
            ));
        }
    };
    let _ = game_user_files::record_process(ctx, &definition.id, &store, launched.identity.clone());

    let exit = wait_for_tree(&install, &mut launched, cancel);
    let installed = watch.finish();

    // Put the real Data back before capturing: the copy is moved home, then the aside comes back. If that
    // fails, the copy and the journal are kept for the next check.
    if let Err(e) = game_tool_swap::disengage(&journal) {
        let _ = release_session(ctx, definition, &store);
        return Err(e.into());
    }

    let captured = capture_from_copy(
        ctx,
        instance_id,
        definition,
        tool,
        &dir,
        &staging,
        &run,
        &fingerprint,
        &journal.copy_dir,
        &before,
        &exit,
        output_dir,
    );
    let finished = game_tool_copy::finish_copy(ctx, definition, &journal);
    let released = release_session(ctx, definition, &store);
    let refreshed = finished?;
    game_tool_swap::remove_journal(ctx, &journal)?;
    released?;
    let (settled, counts) = captured?;

    let mut out = outcome(
        tool_id,
        tool,
        run,
        &exit,
        settled,
        CaptureMethod::Swap,
        reason,
    );
    out.install_changes = outside_data(installed);
    out.swap = Some(SwapRunReport {
        copy: copy.kind,
        copied_in_files: prepared.files,
        copied_in_bytes: prepared.bytes,
        captured_files: counts.changed,
        deleted_files: counts.missing,
        refreshed_files: refreshed.files,
        refreshed_bytes: refreshed.bytes,
    });
    Ok(out)
}

/// What a swap's capture found in the copy: the changed or new files written to the staging folder, and
/// the files the tool deleted.
#[derive(Debug, Clone, Copy, Default)]
struct CopyCounts {
    changed: usize,
    missing: usize,
}

/// Step 9 of a swap: compares the copy with the record it had before the run, by the change detection
/// link capture uses (size; then time; then the bytes against the source), and writes what changed into
/// the staging folder under `Data/`. A deletion becomes a whiteout. The copy's own marker is never
/// output. Then the run is settled as any other.
#[allow(clippy::too_many_arguments)]
fn capture_from_copy(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    run: &str,
    fingerprint: &str,
    copy_dir: &Path,
    before: &game_tool_copy::CopyRecord,
    exit: &TreeExit,
    output_dir: Option<&Path>,
) -> Result<(Settled, CopyCounts), ToolError> {
    std::fs::create_dir_all(staging)?;
    let deployed = game_tool_copy::as_deployment_record(before);
    let diff = compare_farm(copy_dir, &deployed, &mut |file, recorded| {
        farm_file_differs(ctx, file, recorded)
    })?;

    let mut counts = CopyCounts::default();
    for change in diff.changes {
        let file = match change {
            FarmChange::Changed { file, .. } | FarmChange::Added(file) => file,
        };
        if file.rel_path.as_str() == game_tool_copy::MARKER_FILE {
            continue;
        }
        let rel = format!("Data/{}", file.rel_path.as_str());
        let dest = crate::game_ini::join_rel(staging, &rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&file.abs_path, &dest)?;
        counts.changed += 1;
    }
    for recorded in &diff.missing {
        let rel = RelPath::new(format!("Data/{}", recorded.path.as_str()))
            .map_err(|e| ToolError::Other(format!("invalid deletion: {e}")))?;
        write_whiteout(staging, &rel)?;
        counts.missing += 1;
    }

    let settled = settle_run(
        ctx,
        instance_id,
        definition,
        tool,
        dir,
        staging,
        run,
        fingerprint,
        exit,
        output_dir,
    )?;
    Ok((settled, counts))
}

/// The changes under the real install folder that are not under `Data`: a swap's `Data` writes are the
/// copy's, and are captured, so they are not listed here.
fn outside_data(changes: InstallChanges) -> InstallChanges {
    let outside = |paths: Vec<String>| -> Vec<String> {
        paths
            .into_iter()
            .filter(|p| {
                let folded = fold_path_case(p);
                !(folded == "data" || folded.starts_with("data/"))
            })
            .collect()
    };
    InstallChanges {
        created: outside(changes.created),
        changed: outside(changes.changed),
        deleted: outside(changes.deleted),
        may_be_incomplete: changes.may_be_incomplete,
        unavailable: changes.unavailable,
    }
}

/// A swap that did not get as far as the tool: the real Data is put back if it moved, the copy is
/// discarded, and the journal goes last. If the real Data cannot be put back, the copy and the journal
/// are kept for the next check and that error is returned instead.
fn abandon_swap(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
    journal: &SwapJournal,
    error: ToolError,
) -> ToolError {
    if let Err(e) = game_tool_swap::restore_and_discard(journal) {
        return e.into();
    }
    let _ = game_tool_swap::remove_journal(ctx, journal);
    let _ = release_session(ctx, definition, store);
    error
}

/// A swap whose tool did not start: the copy moves home, the kept copy is brought to the plan (or the
/// per-run copy deleted), and the journal goes. Returns `error`, or the error of the step that failed.
fn put_back_after(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
    journal: &SwapJournal,
    error: ToolError,
) -> ToolError {
    if let Err(e) = game_tool_swap::disengage(journal) {
        let _ = release_session(ctx, definition, store);
        return e.into();
    }
    if let Err(e) = game_tool_copy::finish_copy(ctx, definition, journal) {
        let _ = release_session(ctx, definition, store);
        return e.into();
    }
    if let Err(e) = game_tool_swap::remove_journal(ctx, journal) {
        let _ = release_session(ctx, definition, store);
        return e.into();
    }
    let _ = release_session(ctx, definition, store);
    error
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn outcome(
    tool_id: &ToolId,
    tool: &ToolDefinition,
    run: String,
    exit: &TreeExit,
    settled: Settled,
    capture: CaptureMethod,
    capture_reason: Option<String>,
) -> RunOutcome {
    RunOutcome {
        tool: tool_id.to_string(),
        name: tool.name.clone(),
        run,
        capture,
        capture_reason,
        promoted: settled.promoted,
        current: settled.current,
        previous: settled.previous,
        written: settled.written,
        deleted: settled
            .deleted
            .iter()
            .map(|d| d.as_str().to_string())
            .collect(),
        exit_code: exit.code,
        cancelled: exit.cancelled,
        failed_folder: settled.failed_folder,
        failure: settled.failure,
        stdout_tail: exit.output.stdout.clone(),
        stderr_tail: exit.output.stderr.clone(),
        copied_layer: None,
        install_changes: InstallChanges::default(),
        swap: None,
        offers: Vec::new(),
        unchanged_dropped: 0,
    }
}

/// Write the staging folder of a link-captured run from the difference between the farm and its
/// record, then settle it. A changed link is refused: the run is discarded and the paths named.
#[allow(clippy::too_many_arguments)]
fn capture_from_links(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    run: &str,
    fingerprint: &str,
    game_dir: &Path,
    before: &DeploymentRecord,
    exit: &TreeExit,
    output_dir: Option<&Path>,
) -> Result<Settled, ToolError> {
    std::fs::create_dir_all(staging)?;
    // A copy is changed when its bytes differ, not when its time moves: a tool that rewrites the same
    // bytes changed nothing. A link is the content object itself, so a write through it shows in its
    // size and time, and the copy in the source cannot be compared with it.
    let diff = compare_farm(game_dir, before, &mut |file, recorded| {
        farm_file_differs(ctx, file, recorded)
    })?;

    let linked: Vec<String> = diff
        .changes
        .iter()
        .filter_map(|change| match change {
            FarmChange::Changed { recorded, file } if recorded.placement == Placement::Link => {
                Some(file.rel_path.as_str().to_string())
            }
            _ => None,
        })
        .collect();
    if !linked.is_empty() {
        let failed = discard(dir, staging, run, exit)?;
        // A refused run is a discarded run too, though it returns no outcome to carry an offer.
        note_discard(dir, false, false);
        return Err(ToolError::LinkedFileChanged {
            tool: tool.id.to_string(),
            paths: linked,
            failed_folder: Some(failed),
        });
    }

    for change in diff.changes {
        let file = match change {
            FarmChange::Changed { file, .. } | FarmChange::Added(file) => file,
        };
        let dest = crate::game_ini::join_rel(staging, file.rel_path.as_str());
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&file.abs_path, &dest)?;
    }
    for recorded in &diff.missing {
        write_whiteout(staging, &recorded.path)?;
    }

    settle_run(
        ctx,
        instance_id,
        definition,
        tool,
        dir,
        staging,
        run,
        fingerprint,
        exit,
        output_dir,
    )
}

/// The VFS's deletion marker for `rel` in a staging folder: `<staging>\.agvfs-wh\<rel>.wh`, the same
/// marker the VFS writes when a tool deletes a file.
pub(crate) fn write_whiteout(staging: &Path, rel: &RelPath) -> Result<(), ToolError> {
    let mut marker = staging
        .join(WHITEOUT_DIR)
        .join(rel.as_str())
        .into_os_string();
    marker.push(WHITEOUT_SUFFIX);
    let marker = PathBuf::from(marker);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&marker, b"")?;
    Ok(())
}

/// A mod-output tool's earlier outputs (MASTER_SPEC §26.9). They stay in the content store, so
/// pointing the tool's layer back at one is a layer change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputRecord {
    pub item_id: String,
    pub name: String,
    pub added_at_unix_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ToolOutputsFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tools: BTreeMap<String, Vec<ToolOutputRecord>>,
}

const TOOL_OUTPUTS_FILE: &str = "tool-outputs.json";

fn read_outputs(instance_dir: &Path) -> Result<ToolOutputsFile, ToolError> {
    let path = instance_dir.join(TOOL_OUTPUTS_FILE);
    if !path.exists() {
        return Ok(ToolOutputsFile::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|e| {
        ToolError::Other(format!(
            "'{}' is not a valid record of tool outputs: {e}",
            path.display()
        ))
    })
}

fn write_outputs(instance_dir: &Path, file: &ToolOutputsFile) -> Result<(), ToolError> {
    std::fs::create_dir_all(instance_dir)?;
    let path = instance_dir.join(TOOL_OUTPUTS_FILE);
    let tmp = instance_dir.join(format!("{TOOL_OUTPUTS_FILE}.{}.tmp", new_id_suffix()));
    let mut bytes = serde_json::to_vec_pretty(file)?;
    bytes.push(b'\n');
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// The layer a mod-output tool's result sits in, named after the tool so a later run can point it at
/// a new item without adding another layer.
fn output_layer_id(tool_id: &ToolId) -> Result<LayerId, ToolError> {
    let digest = Sha256::digest(tool_id.as_str().as_bytes());
    let hex = format!("{digest:x}");
    LayerId::new(format!("tool-output:{}", &hex[..16])).map_err(ToolError::Id)
}

/// Point a mod-output tool's layer at `item_id`: the item from this run, or an earlier output the
/// user chose. A new layer goes at the end of the content (mod) order.
fn point_output_layer(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    item_id: &str,
) -> Result<(), ToolError> {
    let mut manifest = load_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    let id = output_layer_id(tool_id)?;
    match layers.iter().position(|layer| layer.id == id) {
        Some(i) => {
            layers[i].source = LayerSource::Content {
                content: item_id.to_string(),
            };
            layers[i].enabled = true;
        }
        None => {
            let layer = Layer {
                id,
                enabled: true,
                mount_path: RelPath::default(),
                source_path: RelPath::default(),
                source: LayerSource::Content {
                    content: item_id.to_string(),
                },
                whiteouts: Vec::new(),
                own_copy: false,
            };
            let at = layers
                .iter()
                .position(|layer| layer.source.rank() > 2)
                .unwrap_or(layers.len());
            layers.insert(at, layer);
        }
    }
    save_layers(ctx, instance_id, definition, &mut manifest, layers)
}

fn current_mod_output(
    ctx: &Ctx,
    instance_id: &str,
    tool_id: &ToolId,
) -> Result<Option<String>, ToolError> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    Ok(read_outputs(&instance_dir)?
        .tools
        .get(tool_id.as_str())
        .and_then(|items| items.last())
        .map(|item| item.item_id.clone()))
}

/// Import a mod-output tool's folder as a content item and point the tool's layer at it. The item is
/// named after the tool and the local UTC time of the run.
fn import_mod_output(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    folder: &Path,
) -> Result<ToolOutputRecord, ToolError> {
    let now = chrono::Utc::now();
    let name = format!("{} Output ({})", tool.name, now.format("%Y-%m-%d %H:%M"));
    let outcome = crate::content_store::add_folder(ctx, folder, Some(&name))?;
    let item = outcome.item();
    let record = ToolOutputRecord {
        item_id: item.item_id.clone(),
        name,
        added_at_unix_ms: now.timestamp_millis(),
    };
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let mut file = read_outputs(&instance_dir)?;
    file.version = 1;
    let items = file.tools.entry(tool.id.to_string()).or_default();
    if items.last().map(|last| &last.item_id) != Some(&record.item_id) {
        items.push(record.clone());
    }
    write_outputs(&instance_dir, &file)?;
    point_output_layer(ctx, instance_id, definition, &tool.id, &record.item_id)?;
    Ok(record)
}

/// A mod-output tool's earlier outputs, newest last.
pub fn list_tool_outputs(
    ctx: &Ctx,
    instance_id: &str,
    tool_id: &ToolId,
) -> Result<Vec<ToolOutputRecord>, ToolError> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    Ok(read_outputs(&instance_dir)?
        .tools
        .get(tool_id.as_str())
        .cloned()
        .unwrap_or_default())
}

/// Point a tool's layer back at an earlier output. The item must be one this tool produced; nothing
/// else can be made a tool's output through this door.
pub fn use_tool_output(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
    item_id: &str,
) -> Result<Vec<ToolOutputRecord>, ToolError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "tool-output-use",
    )?;
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let file = read_outputs(&instance_dir)?;
    let items = file
        .tools
        .get(tool_id.as_str())
        .cloned()
        .unwrap_or_default();
    if !items.iter().any(|item| item.item_id == item_id) {
        return Err(ToolError::Invalid(format!(
            "'{item_id}' is not an earlier output of tool '{tool_id}'"
        )));
    }
    crate::content_store::get_item(ctx, item_id)?;
    point_output_layer(ctx, instance_id, definition, tool_id, item_id)?;
    Ok(items)
}

/// Settle a run of a tool whose output is a whole mod: the folder is the output, not the run's
/// staging folder. Nothing is imported when the run failed, was cancelled, or left the folder empty.
#[allow(clippy::too_many_arguments)]
fn settle_mod_output(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    run: &str,
    folder: &Path,
    exit: &TreeExit,
) -> Result<Settled, ToolError> {
    let written = written_files(staging)?;
    let failure = if exit.cancelled {
        Some(exit.with_output("the run was cancelled".to_string()))
    } else if exit.code != Some(0) {
        Some(exit.failed_with_code(exit.code))
    } else {
        match crate::content_store::collect_folder_files(folder) {
            Ok(files) if !files.is_empty() => None,
            Ok(_) => Some(exit.with_output(format!(
                "the tool exited with code 0 but its output folder '{}' has no files, so nothing was imported",
                folder.display()
            ))),
            Err(e) => Some(exit.with_output(format!(
                "the tool exited with code 0 but its output folder '{}' could not be read: {e}",
                folder.display()
            ))),
        }
    };
    let current = current_mod_output(ctx, instance_id, &tool.id)?;
    if let Some(failure) = failure {
        let failed = discard(dir, staging, run, exit)?;
        if tool.output_arg.is_some() {
            let _ = std::fs::remove_dir_all(folder);
        }
        return Ok(Settled {
            written,
            promoted: false,
            current,
            previous: None,
            deleted: Vec::new(),
            failed_folder: Some(failed),
            failure: Some(failure),
        });
    }
    let record = import_mod_output(ctx, instance_id, definition, tool, folder)?;
    let _ = std::fs::remove_dir_all(staging);
    if tool.output_arg.is_some() {
        let _ = std::fs::remove_dir_all(folder);
    }
    Ok(Settled {
        written: Vec::new(),
        promoted: true,
        current: Some(record.item_id.clone()),
        previous: current,
        deleted: Vec::new(),
        failed_folder: None,
        failure: None,
    })
}

/// What became of a finished run's output.
struct Settled {
    written: Vec<String>,
    promoted: bool,
    current: Option<String>,
    previous: Option<String>,
    deleted: Vec<RelPath>,
    failed_folder: Option<PathBuf>,
    failure: Option<String>,
}

/// Promote a run that exited 0 and passed its output checks, or discard any other run, and say what
/// the output is now. A run that exits 0 is still discarded when a required output is missing or a
/// failure marker is in a file it wrote (MASTER_SPEC §26.9).
#[allow(clippy::too_many_arguments)]
fn settle_run(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    run: &str,
    fingerprint: &str,
    exit: &TreeExit,
    output_dir: Option<&Path>,
) -> Result<Settled, ToolError> {
    if let Some(folder) = output_dir {
        return settle_mod_output(
            ctx,
            instance_id,
            definition,
            tool,
            dir,
            staging,
            run,
            folder,
            exit,
        );
    }
    let written = written_files(staging)?;
    let failure = if exit.cancelled {
        Some(exit.with_output("the run was cancelled".to_string()))
    } else if exit.code != Some(0) {
        Some(exit.failed_with_code(exit.code))
    } else {
        output_check(ctx, instance_id, definition, tool, staging, &written)?
            .map(|failure| exit.with_output(failure))
    };
    let promoted = failure.is_none();
    if promoted && tool.run_only && written.is_empty() && read_markers(staging)?.is_empty() {
        // A run-only tool that wrote nothing simply ran: there is no new generation, and the
        // output the instance had (if any) stays as it was.
        std::fs::remove_dir_all(staging)?;
        return Ok(Settled {
            written,
            promoted,
            current: read_current_generation(ctx, instance_id, &tool.id)?,
            previous: read_record(dir)?.previous.map(|g| g.generation),
            deleted: Vec::new(),
            failed_folder: None,
            failure: None,
        });
    }
    if promoted {
        let (current, previous, deleted) = promote(
            ctx,
            instance_id,
            definition,
            tool,
            dir,
            staging,
            fingerprint,
        )?;
        return Ok(Settled {
            written,
            promoted,
            current: Some(current),
            previous,
            deleted,
            failed_folder: None,
            failure: None,
        });
    }
    let failed = discard(dir, staging, run, exit)?;
    Ok(Settled {
        written,
        promoted,
        current: read_current_generation(ctx, instance_id, &tool.id)?,
        previous: read_record(dir)?.previous.map(|g| g.generation),
        deleted: Vec::new(),
        failed_folder: Some(failed),
        failure,
    })
}

/// The instance's stack as the game would have it once `tool`'s output is promoted with `deletions` as its
/// whiteouts (MASTER_SPEC §26.9). The tool's generated layer points at the would-be output, and the layers
/// above it are left out, the writable layer among them: the game's own writes are not the tool's output.
fn would_be_manifest(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    deletions: Vec<RelPath>,
) -> Result<GameInstanceManifest, ToolError> {
    let mut manifest = load_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    match generated_position(&layers, tool.id.as_str()) {
        Some(i) => {
            layers[i].source = LayerSource::Generated {
                tool: tool.id.clone(),
                generation: WOULD_BE_GENERATION.to_string(),
                inputs: InputFingerprint::Unknown,
            };
            layers[i].whiteouts = deletions;
        }
        None => layers.push(generated_layer(
            &tool.id,
            WOULD_BE_GENERATION,
            InputFingerprint::Unknown,
            deletions,
        )?),
    }
    let layers = arrange(ctx, definition, layers);
    let end = generated_position(&layers, tool.id.as_str()).unwrap_or(layers.len());
    manifest.layers = LayerStack::new(layers.into_iter().take(end + 1).collect())?;
    Ok(manifest)
}

/// The files the game would read once `tool`'s run is promoted, case-folded and `/`-separated
/// (MASTER_SPEC §26.9). The stack is the instance's, with the tool's generated layer holding the run's
/// staging folder and the run's whiteouts. The layers above that layer are left out, the writable
/// layer among them: the game's own writes are not the tool's output. The planner does the work, so
/// what it hides or places is what the game would see.
///
/// The inner `Err` is a reason the output cannot be deployed at all (for example a file that is a
/// folder in another layer). That fails the run's checks; an outer `Err` is a failure to read the
/// instance.
fn would_be_files(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    staging: &Path,
) -> Result<Result<Vec<String>, String>, ToolError> {
    let deletions = read_markers(staging)?;
    let manifest = would_be_manifest(ctx, instance_id, definition, tool, deletions)?;

    let would_be = crate::game_deploy::WouldBe {
        manifest,
        tool: tool.id.as_str(),
        folder: staging.to_path_buf(),
    };
    match crate::game_deploy::plan_would_be(ctx, instance_id, definition, would_be) {
        Ok(plan) => Ok(Ok(plan
            .files
            .iter()
            .map(|file| fold_path_case(file.path.as_str()))
            .collect())),
        Err(e) => Ok(Err(format!(
            "the output the run would produce cannot be deployed: {e}"
        ))),
    }
}

/// Drops the files of a virtual-file-system run that are not output: those the game would already read
/// unchanged without the tool's output (MASTER_SPEC §26.9). A tool that opens a file for writing and
/// never changes it makes the VFS copy the file up into staging, and such a copy would shadow the mod's
/// own file from then on. Returns how many files were dropped. The run's whiteouts are left alone.
///
/// The baseline is the stack with this tool's output empty, planned as links with the writable layer
/// left out. A staged file the baseline does not hold is always kept. The tool's previous generation is
/// not in the baseline, so a file equal to it but not to the baseline is kept too. A file is equal when
/// its size and SHA-256 match: the content store's hash when the plan carries one, otherwise both files
/// are hashed. Dropped files are deleted, with the folders they leave empty.
pub fn drop_unchanged_outputs(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    dir: &Path,
    staging: &Path,
    run: &str,
) -> Result<usize, ToolError> {
    // The baseline's tool layer reads this folder, which is empty: the tool's output is absent.
    let empty = dir.join(format!("baseline-{run}"));
    std::fs::create_dir_all(&empty)?;
    let baseline = baseline_plan(ctx, instance_id, definition, tool, &empty);
    let _ = std::fs::remove_dir_all(&empty);
    let baseline = baseline?;

    let below: BTreeMap<String, &crate::game_deploy::PlannedFile> = baseline
        .files
        .iter()
        .map(|file| (fold_path_case(file.path.as_str()), file))
        .collect();
    let mut dropped = 0;
    for rel in written_files(staging)? {
        let Some(file) = below.get(&fold_path_case(&rel)) else {
            continue;
        };
        let path = rel
            .split('/')
            .fold(staging.to_path_buf(), |p, part| p.join(part));
        if !same_bytes_as_baseline(ctx, &path, file)? {
            continue;
        }
        std::fs::remove_file(&path)?;
        remove_emptied_folders(staging, &path);
        dropped += 1;
    }
    Ok(dropped)
}

/// The files the game would read with `tool`'s output empty, planned as links (MASTER_SPEC §26.9).
fn baseline_plan(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    empty: &Path,
) -> Result<crate::game_deploy::DeploymentPlan, ToolError> {
    let manifest = would_be_manifest(ctx, instance_id, definition, tool, Vec::new())?;
    let would_be = crate::game_deploy::WouldBe {
        manifest,
        tool: tool.id.as_str(),
        folder: empty.to_path_buf(),
    };
    Ok(crate::game_deploy::plan_would_be(
        ctx,
        instance_id,
        definition,
        would_be,
    )?)
}

/// Whether a staged file has the bytes of the baseline file it shares a path with. The sizes are compared
/// first, so a file of another size is never hashed. The staged file is always hashed here, never a hash
/// read from the staging folder.
fn same_bytes_as_baseline(
    ctx: &Ctx,
    staged: &Path,
    baseline: &crate::game_deploy::PlannedFile,
) -> Result<bool, ToolError> {
    if std::fs::metadata(staged)?.len() != baseline.size {
        return Ok(false);
    }
    let staged_hash = crate::helpers::hash_file_sha256(staged)?;
    let baseline_hash = match &baseline.source {
        crate::game_deploy::FileSource::Content { sha256, .. } => sha256.clone(),
        other => crate::helpers::hash_file_sha256(&crate::game_deploy::source_path(ctx, other))?,
    };
    Ok(staged_hash.eq_ignore_ascii_case(&baseline_hash))
}

/// Removes the folders above a dropped file that are now empty, stopping at `root`, which stays.
fn remove_emptied_folders(root: &Path, file: &Path) {
    let mut folder = file.parent();
    while let Some(current) = folder {
        if current == root || !current.starts_with(root) || std::fs::remove_dir(current).is_err() {
            break;
        }
        folder = current.parent();
    }
}

/// Hex pairs as bytes, or `None` when the text is empty, has an odd length, or has a non-hex digit. An
/// empty magic is refused rather than matching everything.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// The checks a run that exited 0 must pass (MASTER_SPEC §26.9): every required output is a file the
/// game would read once the run is promoted, a file the run wrote begins with its declared bytes, and no
/// failure marker is in a file the run wrote. Returns why the run failed, or `None` when it passed.
/// Nothing is planned when the tool declares no required outputs, and nothing is read when it declares
/// no markers or magic.
fn output_check(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool: &ToolDefinition,
    staging: &Path,
    written: &[String],
) -> Result<Option<String>, ToolError> {
    if !tool.required_outputs.is_empty() {
        let files = match would_be_files(ctx, instance_id, definition, tool, staging)? {
            Ok(files) => files,
            Err(reason) => return Ok(Some(reason)),
        };
        for pattern in &tool.required_outputs {
            let wanted = fold_path_case(pattern);
            if !files.iter().any(|key| glob_match(&wanted, key)) {
                return Ok(Some(format!(
                    "required output '{pattern}' is missing from the output the run would produce"
                )));
            }
        }
    }

    // A file the run wrote must begin with its declared bytes. A file the run did not write is the
    // output of an earlier run, which was checked when it was promoted, so it is not read here.
    for required in &tool.required_magic {
        let wanted = fold_path_case(required.file.as_str());
        let Some(rel) = written.iter().find(|rel| fold_path_case(rel) == wanted) else {
            continue;
        };
        let Some(expected) = decode_hex(&required.magic) else {
            return Ok(Some(format!(
                "the magic declared for {rel} is not valid hex, so the run cannot be checked and is discarded"
            )));
        };
        let path = rel
            .split('/')
            .fold(staging.to_path_buf(), |p, part| p.join(part));
        let mut head = vec![0u8; expected.len()];
        let mut file = std::fs::File::open(&path)?;
        let starts_right = match std::io::Read::read_exact(&mut file, &mut head) {
            Ok(()) => head == expected,
            // Shorter than its magic: it cannot start with it.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
            Err(e) => return Err(e.into()),
        };
        if !starts_right {
            return Ok(Some(format!(
                "{rel} does not start with the bytes its tool declares ({}), so it is not a valid file of its kind and the run is discarded",
                required.magic
            )));
        }
    }

    for marker in &tool.failure_markers {
        if marker.contains.is_empty() {
            continue;
        }
        let wanted = fold_path_case(marker.file.as_str());
        let Some(rel) = written.iter().find(|rel| fold_path_case(rel) == wanted) else {
            continue;
        };
        let path = rel
            .split('/')
            .fold(staging.to_path_buf(), |p, part| p.join(part));
        if std::fs::metadata(&path)?.len() > MARKER_SEARCH_LIMIT {
            continue;
        }
        let text = String::from_utf8_lossy(&std::fs::read(&path)?).into_owned();
        if text.contains(&marker.contains) {
            return Ok(Some(format!(
                "{rel} contains '{}', which marks a failed run",
                marker.contains
            )));
        }
    }
    Ok(None)
}

/// The generation a tool's layer points at now. Read through the manifest on demand: a discarded run
/// leaves the output as it was.
fn read_current_generation(
    ctx: &Ctx,
    instance_id: &str,
    tool: &ToolId,
) -> Result<Option<String>, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    Ok(manifest
        .layers
        .layers()
        .iter()
        .find_map(|layer| match &layer.source {
            LayerSource::Generated {
                tool: layer_tool,
                generation,
                ..
            } if layer_tool == tool => Some(generation.clone()),
            _ => None,
        }))
}

/// Swap a tool's current and previous generations, so the game reads the previous output on its
/// next deployment (MASTER_SPEC §26.9). Nothing is deleted.
pub fn rollback(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
) -> Result<ToolState, ToolError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "tool-rollback",
    )?;
    let mut manifest = load_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    let position = generated_position(&layers, tool_id.as_str())
        .ok_or_else(|| ToolError::NoOutput(tool_id.to_string()))?;
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let mut record = read_record(&dir)?;
    let previous = record
        .previous
        .clone()
        .ok_or_else(|| ToolError::NoPrevious(tool_id.to_string()))?;
    let previous_dir = generation_dir(&instance_dir, tool_id.as_str(), &previous.generation)
        .map_err(ToolError::Other)?;
    if !previous_dir.is_dir() {
        return Err(ToolError::Other(format!(
            "the previous output of tool '{tool_id}' is missing from {}",
            previous_dir.display()
        )));
    }

    let current = match &layers[position].source {
        LayerSource::Generated {
            generation, inputs, ..
        } => Generation {
            generation: generation.clone(),
            inputs: inputs.clone(),
            whiteouts: layers[position].whiteouts.clone(),
        },
        _ => return Err(ToolError::NoOutput(tool_id.to_string())),
    };
    layers[position].source = LayerSource::Generated {
        tool: tool_id.clone(),
        generation: previous.generation.clone(),
        inputs: previous.inputs.clone(),
    };
    layers[position].whiteouts = previous.whiteouts.clone();
    save_layers(ctx, instance_id, definition, &mut manifest, layers)?;
    record.previous = Some(current);
    write_record(&dir, &record)?;

    list(ctx, instance_id, definition)?
        .into_iter()
        .find(|state| state.tool == tool_id.as_str())
        .ok_or_else(|| ToolError::NoOutput(tool_id.to_string()))
}

/// Remove a tool's output from an instance: its layer goes, and so do its generations.
/// Returns the generation folders that were deleted.
pub fn remove(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    tool_id: &ToolId,
) -> Result<Vec<String>, ToolError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "tool-remove",
    )?;
    let mut manifest = load_manifest(ctx, instance_id)?;
    let mut layers = manifest.layers.layers().to_vec();
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let position = generated_position(&layers, tool_id.as_str());
    if position.is_none() && !dir.exists() {
        return Err(ToolError::NoOutput(tool_id.to_string()));
    }
    let removed = generation_numbers(&dir)?
        .into_iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>();
    if let Some(i) = position {
        layers.remove(i);
        save_layers(ctx, instance_id, definition, &mut manifest, layers)?;
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(removed)
}

/// Files added, changed and removed between a tool's previous and current generations.
pub fn diff(ctx: &Ctx, instance_id: &str, tool_id: &ToolId) -> Result<DiffReport, ToolError> {
    let manifest = load_manifest(ctx, instance_id)?;
    let current = manifest
        .layers
        .layers()
        .iter()
        .find_map(|layer| match &layer.source {
            LayerSource::Generated {
                tool, generation, ..
            } if tool == tool_id => Some(generation.clone()),
            _ => None,
        })
        .ok_or_else(|| ToolError::NoOutput(tool_id.to_string()))?;
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| ToolError::Other(e.to_string()))?;
    let dir = tool_dir(&instance_dir, tool_id.as_str());
    let previous = read_record(&dir)?
        .previous
        .map(|g| g.generation)
        .ok_or_else(|| ToolError::NoPrevious(tool_id.to_string()))?;

    let before =
        generation_dir(&instance_dir, tool_id.as_str(), &previous).map_err(ToolError::Other)?;
    let after =
        generation_dir(&instance_dir, tool_id.as_str(), &current).map_err(ToolError::Other)?;
    let before_files = files_by_key(&before)?;
    let after_files = files_by_key(&after)?;

    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (key, (rel, abs)) in &after_files {
        match before_files.get(key) {
            None => added.push(rel.clone()),
            Some((_, old_abs)) => {
                if std::fs::read(abs)? != std::fs::read(old_abs)? {
                    changed.push(rel.clone());
                }
            }
        }
    }
    for (key, (rel, _)) in &before_files {
        if !after_files.contains_key(key) {
            removed.push(rel.clone());
        }
    }
    Ok(DiffReport {
        tool: tool_id.to_string(),
        previous,
        current,
        added,
        changed,
        removed,
    })
}

/// A generation's files, keyed by case-folded path, with the path as written and its location.
fn files_by_key(root: &Path) -> Result<BTreeMap<String, (String, PathBuf)>, ToolError> {
    let mut out = BTreeMap::new();
    for rel in written_files(root)? {
        let abs = rel
            .split('/')
            .fold(root.to_path_buf(), |p, part| p.join(part));
        out.insert(fold_path_case(&rel), (rel, abs));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pinned_base_itself_is_a_forbidden_output_root() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().join("app_data"));
        let base = BaseManifest {
            manifest_version: 1,
            base_id: "elsewhere".into(),
            runtime: agora_game_api::RuntimeIdentity {
                game: agora_game_api::GameId::new("test-game").unwrap(),
                store: StoreId::new("steam").unwrap(),
                version: "1.0.0".into(),
                build: None,
            },
            mode: crate::game_base::BaseMode::Linked,
            source_install: agora_game_api::InstallId::new("steam:1").unwrap(),
            source_product: None,
            source_location: tmp.path().join("install"),
            location: tmp.path().join("other-root").join("elsewhere"),
            created_unix_ms: 0,
            files: Vec::new(),
            skipped: Vec::new(),
        };

        let roots = output_forbidden_roots(&ctx, "inst", &base).unwrap();

        assert!(
            roots.contains(&real_path(&base.location)),
            "the pinned base {} is not a forbidden output root: {roots:?}",
            base.location.display()
        );
    }
}
