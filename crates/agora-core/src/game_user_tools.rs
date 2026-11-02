//! The user's own tools for an instance (MASTER_SPEC §26.9).
//!
//! Besides the tools a game package declares, a player may keep a list of programs that ship inside
//! their mods: an executable, a name and optional arguments. The list lives in
//! `<instance>/user_tools.json` as `{ "version": 1, "tools": [UserTool...] }`.
//!
//! A missing file is an empty list. A present file that cannot be read, is malformed, or names
//! another version is an error: the screen says so rather than showing "no tools", and adding a tool
//! refuses to overwrite it.
//!
//! `find_candidates` looks through the instance's enabled mods for `.exe` files and recognises the
//! tools the game declares. It suggests; nothing is added until the person confirms.

use std::collections::BTreeMap;
use std::path::Path;

use agora_game_api::{fold_path_case, GameDefinition, GamePath, ToolDefinition};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_deploy::{self, DeployMode, FileSource};
use crate::game_instance::{self, GameInstanceManifest};
use crate::lock_manager::LockResource;

const USER_TOOLS_FILE: &str = "user_tools.json";
const USER_TOOLS_VERSION: u32 = 1;
/// The longest a tool's name may be, in characters.
pub const NAME_MAX_CHARS: usize = 80;

/// Executables that are installers rather than tools a player runs from Agora. Everything else is
/// offered; there is no other exclusion.
pub const EXCLUDED_EXE_NAMES: &[&str] = &["uninstall.exe", "unins000.exe"];

#[derive(Debug, thiserror::Error)]
pub enum UserToolError {
    #[error("instance '{0}' not found")]
    InstanceNotFound(String),
    #[error("the game of instance '{0}' is not known here")]
    GameNotFound(String),
    #[error("the user tools file '{path}' cannot be read: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("the user tools file '{path}' is not valid: {reason}")]
    Malformed { path: String, reason: String },
    #[error(
        "the user tools file '{path}' has version {found}, which this Agora does not understand"
    )]
    UnsupportedVersion { path: String, found: u32 },
    #[error("the tool '{0}' is not in this instance's list")]
    NotFound(String),
    #[error("the tool '{exe}' is already in this instance's list")]
    Duplicate { exe: String },
    #[error("{0}")]
    InvalidExe(String),
    #[error("{0}")]
    InvalidName(String),
    #[error(transparent)]
    Deploy(#[from] game_deploy::DeployError),
    #[error(transparent)]
    Content(#[from] crate::content_store::ContentError),
    #[error(transparent)]
    Instance(#[from] game_instance::InstanceError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// One tool the user added to an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTool {
    /// Generated, stable for the tool's life in the list.
    pub id: String,
    pub name: String,
    /// Game-relative path, with forward slashes.
    pub exe: String,
    pub args: Vec<String>,
    /// The game-declared tool this executable matches, when it is one.
    #[serde(default)]
    pub known_tool_id: Option<String>,
    /// The tool writes a whole mod into a folder instead of files for the game (MASTER_SPEC §26.9).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub output_is_mod: bool,
    /// The folder the tool writes its mod into. A relative folder is next to the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_folder: Option<String>,
}

/// What `add` is given: the user chose the name, path and arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUserTool {
    pub name: String,
    pub exe: String,
    pub args: Vec<String>,
    pub output_is_mod: bool,
    pub output_folder: Option<String>,
}

/// One executable `find_candidates` found, for the person to confirm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolCandidate {
    /// Game-relative path, with forward slashes.
    pub exe: String,
    /// The mod that wins this path. Empty for a game-defined tool not shipped by an enabled mod.
    pub mod_id: String,
    pub mod_name: String,
    pub known_tool_id: Option<String>,
    pub suggested_name: String,
    pub already_added: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct UserToolsFile {
    version: u32,
    tools: Vec<UserTool>,
}

/// The instance's list. A missing file is an empty list; a present file that cannot be read,
/// parsed or understood is an error.
pub fn list(ctx: &Ctx, instance_id: &str) -> Result<Vec<UserTool>, UserToolError> {
    let instance_dir = instance_dir(ctx, instance_id)?;
    get_manifest(ctx, instance_id)?;
    read_file(&instance_dir.join(USER_TOOLS_FILE))
}

/// Add a tool to the instance's list.
///
/// `exe` must be a path `find_candidates` returns for this instance right now, a known tool's
/// executable (relative, not traversing, no device name, not from a disabled mod), or an absolute
/// path to an existing `.exe` the user chose outside the game folder (see [`is_absolute_exe`]).
/// The same executable twice is refused.
pub fn add(ctx: &Ctx, instance_id: &str, new: NewUserTool) -> Result<UserTool, UserToolError> {
    // The list is an instance file, so it is edited under the instance lock like every other one.
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "user-tool-add",
        )
        .map_err(|e| UserToolError::Other(e.to_string()))?;
    let definition = definition_of(ctx, instance_id)?;
    let instance_dir = instance_dir(ctx, instance_id)?;
    let path = instance_dir.join(USER_TOOLS_FILE);
    let mut tools = read_file(&path)?;

    let exe = normalise_any_exe(&new.exe)?;
    let candidates = find_candidates(ctx, instance_id, &definition)?;
    let matched = candidates
        .iter()
        .find(|candidate| fold_path_case(&candidate.exe) == fold_path_case(&exe));
    if matched.is_none() && !is_absolute_path(&exe) {
        return Err(UserToolError::InvalidExe(format!(
            "'{exe}' is not an executable this instance's deployment provides from an enabled mod or the game's files"
        )));
    }
    let canonical = matched.map(|m| m.exe.clone()).unwrap_or(exe);
    if tools
        .iter()
        .any(|tool| fold_path_case(&tool.exe) == fold_path_case(&canonical))
    {
        return Err(UserToolError::Duplicate { exe: canonical });
    }

    let name = new.name.trim().to_string();
    if name.is_empty() {
        return Err(UserToolError::InvalidName(
            "a tool needs a name".to_string(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(UserToolError::InvalidName(
            "a tool's name cannot contain control characters such as a line break or a tab"
                .to_string(),
        ));
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(UserToolError::InvalidName(format!(
            "a tool's name can be at most {NAME_MAX_CHARS} characters"
        )));
    }

    let (output_is_mod, output_folder) = normalise_output(new.output_is_mod, new.output_folder)?;
    let known_tool_id = matched.and_then(|m| m.known_tool_id.clone());

    let tool = UserTool {
        id: new_user_tool_id(),
        name,
        exe: canonical,
        args: new.args,
        known_tool_id,
        output_is_mod,
        output_folder,
    };
    tools.push(tool.clone());
    write_file(
        &path,
        &UserToolsFile {
            version: USER_TOOLS_VERSION,
            tools,
        },
    )?;
    Ok(tool)
}

/// Remove a tool from the instance's list. Only the list is edited; no file is deleted.
pub fn remove(ctx: &Ctx, instance_id: &str, id: &str) -> Result<Vec<UserTool>, UserToolError> {
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "user-tool-remove",
        )
        .map_err(|e| UserToolError::Other(e.to_string()))?;
    let instance_dir = instance_dir(ctx, instance_id)?;
    let path = instance_dir.join(USER_TOOLS_FILE);
    let mut tools = read_file(&path)?;
    let before = tools.len();
    tools.retain(|tool| tool.id != id);
    if tools.len() == before {
        return Err(UserToolError::NotFound(id.to_string()));
    }
    write_file(
        &path,
        &UserToolsFile {
            version: USER_TOOLS_VERSION,
            tools: tools.clone(),
        },
    )?;
    Ok(tools)
}

/// Every `.exe` in the files of the instance's enabled mods, plus the game-declared tools whose
/// executable the deployment provides.
///
/// A path held by several enabled mods is one row, for the mod the deployment lets win it. A tool
/// the game declares is recognised by the file name of its executable (the last path segment),
/// compared case-insensitively. The two installer names in [`EXCLUDED_EXE_NAMES`] are left out.
pub fn find_candidates(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<ToolCandidate>, UserToolError> {
    let plan = game_deploy::plan(ctx, instance_id, definition, DeployMode::Links)?;
    let known = known_tools(ctx, definition);
    let mut added: Vec<String> = list(ctx, instance_id)?
        .into_iter()
        .map(|tool| fold_path_case(&tool.exe))
        .collect();
    added.sort();
    let is_added = |exe: &str| added.binary_search(&fold_path_case(exe)).is_ok();

    let mut rows: BTreeMap<String, ToolCandidate> = BTreeMap::new();
    for file in &plan.files {
        let FileSource::Content { item_id, .. } = &file.source else {
            continue;
        };
        let path = file.path.as_str();
        let Some(base) = file_name(path) else {
            continue;
        };
        if !fold_path_case(base).ends_with(".exe") {
            continue;
        }
        if is_excluded_exe(base) {
            continue;
        }
        let lower = fold_path_case(path);
        if rows.contains_key(&lower) {
            continue;
        }
        let item = crate::content_store::get_item(ctx, item_id)?;
        rows.insert(
            lower,
            candidate_row(path, base, item.item_id, item.name, &known, is_added(path)),
        );
    }

    // A known tool's executable is also a candidate when the deployment provides it from a source
    // that is not an enabled mod (for example the game's own files).
    for tool in &known {
        if rows
            .keys()
            .any(|key| fold_path_case(key) == fold_path_case(&tool.exe))
        {
            continue;
        }
        let provided = plan
            .files
            .iter()
            .any(|file| fold_path_case(file.path.as_str()) == fold_path_case(&tool.exe));
        if !provided {
            continue;
        }
        rows.insert(
            fold_path_case(&tool.exe),
            candidate_row(
                &tool.exe,
                file_name(&tool.exe).unwrap_or(tool.exe.as_str()),
                String::new(),
                String::new(),
                &known,
                is_added(&tool.exe),
            ),
        );
    }

    Ok(rows.into_values().collect())
}

struct KnownTool {
    id: String,
    name: String,
    /// The executable's game-relative path, with forward slashes.
    exe: String,
}

fn known_tools(ctx: &Ctx, definition: &GameDefinition) -> Vec<KnownTool> {
    definition
        .tool_ids
        .iter()
        .filter_map(|id| {
            let tool: ToolDefinition = ctx.games.tool(&definition.id, id)?.clone();
            let GamePath::Runtime { path } = &tool.launch.executable else {
                return None;
            };
            Some(KnownTool {
                id: id.to_string(),
                name: tool.name,
                exe: path.as_str().to_string(),
            })
        })
        .collect()
}

fn candidate_row(
    exe: &str,
    base: &str,
    mod_id: String,
    mod_name: String,
    known: &[KnownTool],
    already_added: bool,
) -> ToolCandidate {
    let base_folded = fold_path_case(base);
    let known_tool = known.iter().find(|tool| {
        fold_path_case(&tool.exe) == fold_path_case(exe)
            || file_name(&tool.exe).is_some_and(|name| fold_path_case(name) == base_folded)
    });
    ToolCandidate {
        exe: exe.to_string(),
        mod_id,
        mod_name,
        known_tool_id: known_tool.map(|tool| tool.id.clone()),
        suggested_name: known_tool
            .map(|tool| tool.name.clone())
            .unwrap_or_else(|| exe_stem(base)),
        already_added,
    }
}

/// The file's stem, without its `.exe`, for a name suggestion.
fn exe_stem(base: &str) -> String {
    if fold_path_case(base).ends_with(".exe") {
        // ".exe" is four ASCII bytes in the original too, so this is a char boundary.
        base.get(..base.len() - 4).unwrap_or(base).to_string()
    } else {
        base.to_string()
    }
}

fn is_excluded_exe(base: &str) -> bool {
    EXCLUDED_EXE_NAMES
        .iter()
        .any(|name| fold_path_case(base) == fold_path_case(name))
}

fn file_name(path: &str) -> Option<&str> {
    path.rsplit('/').find(|segment| !segment.is_empty())
}

fn instance_dir(ctx: &Ctx, instance_id: &str) -> Result<std::path::PathBuf, UserToolError> {
    ctx.paths
        .instance_dir(instance_id)
        .map_err(|e| UserToolError::Other(e.to_string()))
}

fn get_manifest(ctx: &Ctx, instance_id: &str) -> Result<GameInstanceManifest, UserToolError> {
    game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        game_instance::InstanceError::NotFound(id) => UserToolError::InstanceNotFound(id),
        other => UserToolError::Instance(other),
    })
}

fn definition_of(ctx: &Ctx, instance_id: &str) -> Result<GameDefinition, UserToolError> {
    let manifest = get_manifest(ctx, instance_id)?;
    ctx.games
        .game(&manifest.game)
        .cloned()
        .ok_or_else(|| UserToolError::GameNotFound(instance_id.to_string()))
}

fn read_file(path: &Path) -> Result<Vec<UserTool>, UserToolError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).map_err(|error| UserToolError::Unreadable {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let file: UserToolsFile =
        serde_json::from_str(&text).map_err(|error| UserToolError::Malformed {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
    if file.version != USER_TOOLS_VERSION {
        return Err(UserToolError::UnsupportedVersion {
            path: path.display().to_string(),
            found: file.version,
        });
    }
    Ok(file.tools)
}

fn write_file(path: &Path, file: &UserToolsFile) -> Result<(), UserToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| UserToolError::Other("the user tools file has no folder".to_string()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{USER_TOOLS_FILE}.{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut bytes = serde_json::to_vec_pretty(file)?;
    bytes.push(b'\n');
    if let Err(error) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn new_user_tool_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Normalise a path the way the store writes it, and refuse anything that cannot safely name an
/// executable inside the game folder.
fn normalise_exe(raw: &str) -> Result<String, UserToolError> {
    if raw.trim().is_empty() {
        return Err(UserToolError::InvalidExe(
            "a tool needs an executable".to_string(),
        ));
    }
    let path = raw.replace('\\', "/");
    if path.trim().is_empty() {
        return Err(UserToolError::InvalidExe(
            "a tool needs an executable".to_string(),
        ));
    }
    if path.starts_with('/')
        || path.contains(':')
        || path.starts_with("//")
        || Path::new(&path).is_absolute()
    {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' is not a path inside the game folder"
        )));
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' has an empty path segment"
        )));
    }
    for segment in &segments {
        if *segment == "." || *segment == ".." {
            return Err(UserToolError::InvalidExe(format!(
                "'{raw}' cannot leave the game folder"
            )));
        }
        if segment.ends_with('.') || segment.ends_with(' ') {
            return Err(UserToolError::InvalidExe(format!(
                "'{raw}' has a segment ending in a dot or a space"
            )));
        }
        let stem = segment
            .split_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(segment);
        if is_device_name(stem) {
            return Err(UserToolError::InvalidExe(format!("'{raw}' names a device")));
        }
    }
    let base = segments.last().copied().unwrap_or_default();
    if !fold_path_case(base).ends_with(".exe") || fold_path_case(base) == ".exe" {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' is not an .exe file"
        )));
    }
    Ok(path)
}

/// A relative path the deployment provides, or an absolute `.exe` the user chose (MASTER_SPEC
/// §26.9). An absolute path is only accepted when it names an existing `.exe` file and has no
/// control characters, so the run cannot be pointed at anything else.
fn normalise_any_exe(raw: &str) -> Result<String, UserToolError> {
    if raw.chars().any(char::is_control) {
        return Err(UserToolError::InvalidExe(
            "an executable cannot contain control characters".to_string(),
        ));
    }
    if !is_absolute_path(raw) {
        return normalise_exe(raw);
    }
    let path = raw.replace('\\', "/");
    let path = path.trim();
    if path.is_empty() {
        return Err(UserToolError::InvalidExe(
            "a tool needs an executable".to_string(),
        ));
    }
    let base = path.rsplit('/').find(|segment| !segment.is_empty());
    let Some(base) = base else {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' does not name a file"
        )));
    };
    if !base.to_ascii_lowercase().ends_with(".exe") || base.eq_ignore_ascii_case(".exe") {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' is not an .exe file"
        )));
    }
    if base.ends_with('.') || base.ends_with(' ') {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' has a file name ending in a dot or a space"
        )));
    }
    let stem = base.split_once('.').map(|(stem, _)| stem).unwrap_or(base);
    if is_device_name(stem) {
        return Err(UserToolError::InvalidExe(format!("'{raw}' names a device")));
    }
    let file = Path::new(&path);
    if !file.is_absolute() {
        return Err(UserToolError::InvalidExe(format!(
            "'{raw}' is not an absolute path"
        )));
    }
    match std::fs::metadata(file) {
        Ok(meta) if meta.is_file() => {}
        _ => {
            return Err(UserToolError::InvalidExe(format!(
                "'{raw}' is not an existing .exe file"
            )))
        }
    }
    Ok(path.to_string())
}

/// Whether a stored executable is an absolute path outside the game folder. Read when a tool runs,
/// so a saved list written by hand is judged by the same rule as `add`.
pub fn is_absolute_exe(exe: &str) -> bool {
    is_absolute_path(exe)
}

fn is_absolute_path(raw: &str) -> bool {
    let path = raw.replace('\\', "/");
    let path = path.trim();
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || path.starts_with('/')
}

/// The output choice a user tool made. A mod-output tool needs a folder; a relative folder is next
/// to the executable, so it is stored as given.
fn normalise_output(
    is_mod: bool,
    folder: Option<String>,
) -> Result<(bool, Option<String>), UserToolError> {
    if !is_mod {
        return Ok((false, None));
    }
    let folder = folder.unwrap_or_default().trim().replace('\\', "/");
    if folder.is_empty() {
        return Err(UserToolError::InvalidExe(
            "a tool whose output is a mod needs an output folder".to_string(),
        ));
    }
    if folder.chars().any(char::is_control) {
        return Err(UserToolError::InvalidExe(
            "an output folder cannot contain control characters".to_string(),
        ));
    }
    Ok((true, Some(folder)))
}

fn is_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let digits = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    matches!(
        digits,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
    )
}
