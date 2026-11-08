//! Plugin activation for games whose plugin list names what loads (MASTER_SPEC §26.6).
//!
//! Skyrim-family games load a plugin (`.esp`, `.esm`, `.esl`) only when the game's
//! `Plugins.txt` names it. A game definition declares that as a
//! [`LoadOrderDefinition`]; this module keeps the instance's copy of the list in step with the
//! plugins its content layers deploy. Ordering rules (masters, limits, sorting) belong to
//! [`crate::game_load_order`], which reads and writes the list through this module.
//!
//! The list is an instance's per-user file (`<instance dir>/<user_file>`), swapped in for a
//! launch session by [`crate::game_user_files`]. Agora records which plugins it managed, and
//! which the user locked, in `<instance dir>/plugin_list_state.json`, so a layer that is removed
//! or disabled takes its lines with it while a line the user wrote themselves stays.
//!
//! Every failure is an error: an unreadable or unparsable file is never read as "empty".

use std::path::{Path, PathBuf};

use agora_game_api::{
    fold_path_case, glob_match, GameDefinition, GameId, LoadOrderDefinition, LoadOrderFormat,
    StoreId, UserFileMapping, UserFileStrategy,
};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_deploy::{DeploymentPlan, FileSource};
use crate::lock_manager::LockResource;

const STATE_FILE: &str = "plugin_list_state.json";

#[derive(Debug, thiserror::Error)]
pub enum PluginListError {
    #[error("game '{0}' does not keep a plugin list")]
    NoRule(GameId),
    #[error("game '{game}' declares no load order named '{list}'")]
    UnknownList { game: String, list: String },
    #[error("load order '{list}' could not be read: {reason}")]
    Format { list: String, reason: String },
    #[error("instance '{0}' not found")]
    InstanceNotFound(String),
    #[error("'{0}' is not in the instance's plugin list")]
    NotInList(String),
    #[error("this game's plugin list has no inactive state (it has no active prefix), so '{0}' cannot be disabled")]
    NoInactiveState(String),
    #[error("instance '{0}' is running; close the game before changing its plugin list")]
    InstanceRunning(String),
    #[error("plugin list state file {path} cannot be read: {reason}")]
    CorruptState { path: PathBuf, reason: String },
    #[error("plugin list {path} cannot be read: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    UserFiles(#[from] crate::game_user_files::UserFilesError),
    #[error("lock error: {0}")]
    Lock(#[from] crate::error::LauncherError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// What changed in an instance's plugin list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSyncReport {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub warnings: Vec<String>,
}

impl PluginSyncReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.warnings.is_empty()
    }
}

/// One plugin line of an instance's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub active: bool,
    /// Whether Agora added this plugin because a content layer deploys it.
    pub managed: bool,
    /// Whether the user locked this plugin's place in the order (MASTER_SPEC §26.6). Sorting
    /// and moving never move a locked plugin.
    #[serde(default)]
    pub locked: bool,
}

/// An instance's plugin list, in load order among the listed plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginList {
    /// The list's id within the game.
    #[serde(default)]
    pub list_id: String,
    /// The list's name as a person reads it.
    #[serde(default)]
    pub list_name: String,
    pub path: PathBuf,
    /// False when the instance has no copy of the list yet (it has never been deployed).
    pub exists: bool,
    pub entries: Vec<PluginEntry>,
}

/// One list's managed and locked names. A file written before locks existed has only `managed`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ManagedState {
    managed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    locked: Vec<String>,
}

/// `plugin_list_state.json`: one entry per list, so two lists never share managed or locked names.
/// A file written before several lists (or before locks) has the names at the top; they are read
/// as the first list's and moved under its id on the next write.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    lists: std::collections::BTreeMap<String, ManagedState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    managed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    locked: Vec<String>,
}

// ---------------------------------------------------------------------------
// The plugin list file
// ---------------------------------------------------------------------------

const CRLF: &[u8] = b"\r\n";
const LF: &[u8] = b"\n";

/// A plugin list held as raw lines, so lines Agora does not understand (comments, other
/// encodings) are written back byte for byte.
struct ListFile {
    lines: Vec<Vec<u8>>,
    eol: &'static [u8],
}

impl ListFile {
    fn parse(bytes: &[u8]) -> Self {
        let eol = if bytes.windows(2).any(|w| w == CRLF) || !bytes.contains(&b'\n') {
            CRLF
        } else {
            LF
        };
        let mut lines: Vec<Vec<u8>> = bytes
            .split(|b| *b == b'\n')
            .map(|l| l.strip_suffix(b"\r").unwrap_or(l).to_vec())
            .collect();
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        Self { lines, eol }
    }

    fn from_header(rule: &LoadOrderDefinition) -> Self {
        Self {
            lines: rule
                .format
                .header()
                .iter()
                .map(|l| l.as_bytes().to_vec())
                .collect(),
            eol: CRLF,
        }
    }

    fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for line in &self.lines {
            out.extend_from_slice(line);
            out.extend_from_slice(self.eol);
        }
        out
    }
}

/// A line that names a plugin: its state and the name as written.
fn plugin_line<'a>(line: &'a [u8], rule: &LoadOrderDefinition) -> Option<(bool, &'a [u8])> {
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed.starts_with(b"#") {
        return None;
    }
    let prefix = rule.format.active_prefix().as_bytes();
    let (active, name) = if prefix.is_empty() {
        (true, trimmed)
    } else if let Some(rest) = trimmed.strip_prefix(prefix) {
        (true, rest.trim_ascii())
    } else {
        (false, trimmed)
    };
    if name.is_empty() {
        None
    } else {
        Some((active, name))
    }
}

fn key(name: &[u8]) -> Vec<u8> {
    // Plugin lists are bytes; a non-ASCII name is normally UTF-8 from the file system.
    // Invalid UTF-8 keeps the old ASCII-only fold rather than inventing characters.
    match std::str::from_utf8(name) {
        Ok(text) => fold_path_case(text).into_bytes(),
        Err(_) => name.to_ascii_lowercase(),
    }
}

fn render_line(rule: &LoadOrderDefinition, name: &[u8], active: bool) -> Vec<u8> {
    let mut line = Vec::new();
    if active {
        line.extend_from_slice(rule.format.active_prefix().as_bytes());
    }
    line.extend_from_slice(name);
    line
}

/// Bring `file` in step with the plugins `desired` (in the order new ones are appended) and
/// the plugins Agora `old_managed`. Returns what was added and removed.
fn apply_sync(
    file: &mut ListFile,
    rule: &LoadOrderDefinition,
    desired: &[String],
    old_managed: &[String],
) -> (Vec<String>, Vec<String>) {
    let desired_keys: std::collections::HashSet<Vec<u8>> =
        desired.iter().map(|n| key(n.as_bytes())).collect();
    let old_keys: std::collections::HashSet<Vec<u8>> =
        old_managed.iter().map(|n| key(n.as_bytes())).collect();

    // A line for a plugin Agora used to deploy and no longer does goes away.
    let mut removed = Vec::new();
    file.lines.retain(|line| {
        let Some((_, name)) = plugin_line(line, rule) else {
            return true;
        };
        let k = key(name);
        if old_keys.contains(&k) && !desired_keys.contains(&k) {
            removed.push(String::from_utf8_lossy(name).into_owned());
            false
        } else {
            true
        }
    });

    // A plugin entering the managed set for the first time is made active even if a line for
    // it exists: Skyrim itself appends plugins it finds in Data to the list, inactive, so an
    // inactive line Agora never managed is the game's, not the user's choice. Once managed, a
    // line's state is the user's and is kept.
    let mut added = Vec::new();
    for line in file.lines.iter_mut() {
        let Some((active, name)) = plugin_line(line, rule) else {
            continue;
        };
        let k = key(name);
        if !active && desired_keys.contains(&k) && !old_keys.contains(&k) {
            let name = name.to_vec();
            *line = render_line(rule, &name, true);
            added.push(String::from_utf8_lossy(&name).into_owned());
        }
    }

    // A deployed plugin the list does not name yet is appended, active.
    let mut present: std::collections::HashSet<Vec<u8>> = file
        .lines
        .iter()
        .filter_map(|l| plugin_line(l, rule).map(|(_, n)| key(n)))
        .collect();
    for name in desired {
        if present.insert(key(name.as_bytes())) {
            file.lines.push(render_line(rule, name.as_bytes(), true));
            added.push(name.clone());
        }
    }
    (added, removed)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn instance_dir(ctx: &Ctx, instance_id: &str) -> Result<PathBuf, PluginListError> {
    ctx.paths
        .instance_dir(instance_id)
        .map_err(|e| PluginListError::Other(e.to_string()))
}

/// Read a file that may not exist. Any failure other than "not found" is an error.
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, PluginListError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(PluginListError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// The instance folder holding the rendered files of `view` load orders. It is a generated
/// layer of the instance: the deployment plan injects it above content and tool output, so the
/// player's order wins without writing the game's real folder (MASTER_SPEC §30.12).
fn view_root(instance_dir: &Path) -> PathBuf {
    instance_dir.join("view-files")
}

fn view_path(instance_dir: &Path, rule: &LoadOrderDefinition) -> PathBuf {
    view_root(instance_dir).join(rule.file.as_str())
}

/// Write a view list's file when its bytes changed. Returns whether it was written.
fn write_view_file(
    instance_dir: &Path,
    rule: &LoadOrderDefinition,
    bytes: &[u8],
) -> Result<bool, PluginListError> {
    if rule.destination != agora_game_api::LoadOrderDestination::View {
        return Ok(false);
    }
    let path = view_path(instance_dir, rule);
    if read_optional(&path)?.as_deref() == Some(bytes) {
        return Ok(false);
    }
    write_atomic(&path, bytes)?;
    Ok(true)
}

/// Render a `view` list into the generated layer the plan deploys. `None` for an `instance` list,
/// which is swapped in through its user file as before. An unreadable list is an error, never an
/// empty file.
pub(crate) fn render_view_load_order(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
) -> Result<Option<(PathBuf, Vec<u8>)>, PluginListError> {
    if rule.destination != agora_game_api::LoadOrderDestination::View {
        return Ok(None);
    }
    let dir = instance_dir(ctx, instance_id)?;
    let source = dir.join(rule.file.as_str());
    let doc = match read_optional(&source)? {
        Some(bytes) => parse_doc(ctx, instance_id, definition, rule, bytes)?,
        None => blank_doc(rule),
    };
    let bytes = render_doc(ctx, instance_id, definition, rule, &doc)?;
    write_view_file(&dir, rule, &bytes)?;
    Ok(Some((view_path(&dir, rule), bytes)))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PluginListError> {
    let Some(parent) = path.parent() else {
        return Err(PluginListError::Other(format!(
            "{} has no parent folder",
            path.display()
        )));
    };
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!("{file_name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

fn read_state_file(instance_dir: &Path) -> Result<StateFile, PluginListError> {
    let path = instance_dir.join(STATE_FILE);
    match read_optional(&path)? {
        None => Ok(StateFile::default()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| PluginListError::CorruptState {
            path,
            reason: e.to_string(),
        }),
    }
}

/// One list's state. A file from before several lists keeps its names at the top; they are the
/// first list's, so they are handed to `list_id` and moved under it by the next write.
fn state_of(mut file: StateFile, list_id: &str) -> ManagedState {
    if let Some(state) = file.lists.remove(list_id) {
        return state;
    }
    if file.lists.is_empty() && (!file.managed.is_empty() || !file.locked.is_empty()) {
        return ManagedState {
            managed: std::mem::take(&mut file.managed),
            locked: std::mem::take(&mut file.locked),
        };
    }
    ManagedState::default()
}

fn write_state(instance_dir: &Path, state: &StateFile) -> Result<(), PluginListError> {
    write_atomic(
        &instance_dir.join(STATE_FILE),
        &serde_json::to_vec_pretty(state)?,
    )
}

fn store_state(
    instance_dir: &Path,
    list_id: &str,
    state: ManagedState,
) -> Result<(), PluginListError> {
    let mut file = read_state_file(instance_dir)?;
    file.lists.insert(list_id.to_string(), state);
    file.managed.clear();
    file.locked.clear();
    write_state(instance_dir, &file)
}

fn user_file_mapping<'a>(
    definition: &'a GameDefinition,
    rule: &LoadOrderDefinition,
    store: &StoreId,
) -> Option<&'a UserFileMapping> {
    definition.user_files.iter().find(|m| {
        m.strategy == UserFileStrategy::JournaledSwap
            && m.instance_path == rule.file
            && m.applies_to_store(store)
    })
}

/// A session left behind by a finished or crashed run still holds this game's real list.
/// Put it back before reading the real file or editing an instance copy, exactly as the next
/// swap would. A session that is still running is left alone.
fn recover_stale_session(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<(), PluginListError> {
    use crate::game_user_files::UserFilesError;
    if !ctx
        .paths
        .user_files_journal_path(definition.id.as_str(), store.as_str())
        .exists()
    {
        return Ok(());
    }
    // A session that is running (or still starting) is not a stale one: leave it alone, or a
    // plugin edit during a launch would put back the files the launch just swapped in.
    if let Some(session) = crate::game_user_files::status(ctx, &definition.id, store)? {
        if session.running {
            return Ok(());
        }
    }
    match crate::game_user_files::restore(ctx, definition, store) {
        Ok(_)
        | Err(UserFilesError::CannotRestoreRunning { .. })
        | Err(UserFilesError::NoSession { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// The contents a new instance copy starts from: the user's real list when it is safe to read
/// it, else the rule's header.
fn starting_contents(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    mapping: &UserFileMapping,
    rule: &LoadOrderDefinition,
    report: &mut PluginSyncReport,
) -> Result<ListDoc, PluginListError> {
    let journal = ctx
        .paths
        .user_files_journal_path(definition.id.as_str(), store.as_str());
    if journal.exists() {
        // A session is running: the real file holds that session's list, not the user's.
        report.warnings.push(
            "another session of this game is running, so the plugin list starts from the \
             game's header instead of your current list"
                .to_string(),
        );
        return Ok(blank_doc(rule));
    }
    let real = crate::game_user_files::resolve_user_file_source(&mapping.source)?;
    Ok(match read_optional(&real)? {
        Some(bytes) => parse_doc(ctx, instance_id, definition, rule, bytes)?,
        None => blank_doc(rule),
    })
}

fn same_names(a: &[String], b: &[String]) -> bool {
    let norm = |v: &[String]| {
        let mut k: Vec<Vec<u8>> = v.iter().map(|n| key(n.as_bytes())).collect();
        k.sort();
        k.dedup();
        k
    };
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------
// Which plugins a deployment provides
// ---------------------------------------------------------------------------

/// The plugins a deployment gets from content layers, in the order new ones are appended
/// to the list.
///
/// Only plugins directly inside `plugin_folder` that match one of the rule's patterns count,
/// and only when a content layer wins the path: plugins from the base, from the writable
/// layer, or that the base also has (the game's own masters) are never managed. Order is the
/// layer's position (lowest first), then the position of the first matching pattern (so a
/// definition lists its patterns in load priority: masters before plain plugins), then name.
pub fn desired_plugins<'a>(
    rule: &LoadOrderDefinition,
    content_layer_items: &[String],
    plan: &DeploymentPlan,
    base_paths: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let base_keys: std::collections::HashSet<String> =
        base_paths.into_iter().map(fold_path_case).collect();
    let folder = rule.items.folder.as_str().trim_matches('/');
    let patterns: Vec<String> = rule
        .items
        .patterns
        .iter()
        .map(|p| fold_path_case(p.trim()))
        .collect();

    let mut found: Vec<(usize, usize, String, String)> = Vec::new();
    for file in &plan.files {
        let FileSource::Content { item_id, .. } = &file.source else {
            continue;
        };
        let path = file.path.as_str();
        if rule.items.folders {
            // A list of folders orders the folder directly under `items.folder`, not each file.
            let prefix = format!("{}/", folder.trim_matches('/'));
            let Some(rest) = path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let Some(name) = rest.split('/').next().filter(|name| !name.is_empty()) else {
                continue;
            };
            let lower = fold_path_case(name);
            let Some(pattern_rank) = patterns
                .iter()
                .position(|p| !p.is_empty() && glob_match(p, &lower))
            else {
                continue;
            };
            let Some(layer_rank) = content_layer_items.iter().position(|i| i == item_id) else {
                continue;
            };
            found.push((layer_rank, pattern_rank, lower, name.to_string()));
            continue;
        }
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", path),
        };
        if fold_path_case(parent) != fold_path_case(folder) {
            continue;
        }
        let lower = fold_path_case(name);
        let Some(pattern_rank) = patterns
            .iter()
            .position(|p| !p.is_empty() && glob_match(p, &lower))
        else {
            continue;
        };
        if base_keys.contains(&fold_path_case(path)) {
            continue;
        }
        let Some(layer_rank) = content_layer_items.iter().position(|i| i == item_id) else {
            continue;
        };
        found.push((layer_rank, pattern_rank, lower, name.to_string()));
    }
    found.sort();
    found.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
    found.into_iter().map(|(_, _, _, name)| name).collect()
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// The plugins the game always loads itself: the rule's `implicit` names and the names its
/// `implicit_list_file` holds, as the deployment provides it. Their lines are the game's, so sync
/// never manages, activates or removes them.
fn implicit_keys(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    links: Option<&DeploymentPlan>,
) -> Result<std::collections::HashSet<Vec<u8>>, PluginListError> {
    let mut names: Vec<String> = rule.rules.implicit.clone();
    if rule.rules.implicit_list_file.is_some() {
        names.extend(
            crate::game_load_order::implicit_list_names_in(
                ctx,
                instance_id,
                definition,
                Some(&rule.id),
                links,
            )
            .map_err(|e| PluginListError::Other(e.to_string()))?,
        );
    }
    Ok(names.iter().map(|n| key(n.as_bytes())).collect())
}

/// A list's contents while it is being edited: raw lines for `lines`, parsed entries for `script`.
enum ListDoc {
    Lines(ListFile),
    Script(Vec<agora_game_api::LoadOrderLine>),
}

/// Read a list's bytes through the format it declares. A script format's hook failing is an
/// error: an unreadable file is never an empty list.
fn parse_doc(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    bytes: Vec<u8>,
) -> Result<ListDoc, PluginListError> {
    match rule.format {
        LoadOrderFormat::Lines { .. } => Ok(ListDoc::Lines(ListFile::parse(&bytes))),
        LoadOrderFormat::Script { .. } => {
            let lines = crate::game_load_order::call_parse_format(
                ctx,
                instance_id,
                definition,
                rule,
                bytes,
            )
            .map_err(|reason| PluginListError::Format {
                list: rule.id.clone(),
                reason,
            })?;
            Ok(ListDoc::Script(lines))
        }
    }
}

/// Render a list through the format it declares. The bytes go to the declared file only.
fn render_doc(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    doc: &ListDoc,
) -> Result<Vec<u8>, PluginListError> {
    match (doc, &rule.format) {
        (ListDoc::Lines(file), LoadOrderFormat::Lines { .. }) => Ok(file.render()),
        (ListDoc::Script(lines), LoadOrderFormat::Script { .. }) => {
            crate::game_load_order::call_render_format(
                ctx,
                instance_id,
                definition,
                rule,
                lines.clone(),
            )
            .map_err(|reason| PluginListError::Format {
                list: rule.id.clone(),
                reason,
            })
        }
        _ => Err(PluginListError::Other(format!(
            "load order '{}' changed format while it was being written",
            rule.id
        ))),
    }
}

/// A new list's contents: the format's header for `lines`, an empty script list otherwise.
fn blank_doc(rule: &LoadOrderDefinition) -> ListDoc {
    match rule.format {
        LoadOrderFormat::Lines { .. } => ListDoc::Lines(ListFile::from_header(rule)),
        LoadOrderFormat::Script { .. } => ListDoc::Script(Vec::new()),
    }
}

/// The entries and active state in a document, in its order.
fn doc_entries(doc: &ListDoc, rule: &LoadOrderDefinition) -> Vec<(String, bool)> {
    match doc {
        ListDoc::Lines(file) => file
            .lines
            .iter()
            .filter_map(|line| plugin_line(line, rule))
            .map(|(active, name)| (String::from_utf8_lossy(name).into_owned(), active))
            .collect(),
        ListDoc::Script(lines) => lines
            .iter()
            .map(|line| (line.name.clone(), line.active))
            .collect(),
    }
}

/// Bring a script list in step with the plugins a deployment provides, by the same rules the
/// `lines` format uses: remove what Agora managed and no longer deploys, activate a newly managed
/// line, and append a deployed plugin the list does not name.
fn apply_sync_script(
    lines: &mut Vec<agora_game_api::LoadOrderLine>,
    desired: &[String],
    old_managed: &[String],
) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;
    let desired_keys: HashSet<Vec<u8>> = desired.iter().map(|n| key(n.as_bytes())).collect();
    let old_keys: HashSet<Vec<u8>> = old_managed.iter().map(|n| key(n.as_bytes())).collect();

    let mut removed = Vec::new();
    lines.retain(|line| {
        let k = key(line.name.as_bytes());
        if old_keys.contains(&k) && !desired_keys.contains(&k) {
            removed.push(line.name.clone());
            false
        } else {
            true
        }
    });

    let mut added = Vec::new();
    for line in lines.iter_mut() {
        let k = key(line.name.as_bytes());
        if !line.active && desired_keys.contains(&k) && !old_keys.contains(&k) {
            line.active = true;
            added.push(line.name.clone());
        }
    }

    let mut present: HashSet<Vec<u8>> =
        lines.iter().map(|line| key(line.name.as_bytes())).collect();
    for name in desired {
        if present.insert(key(name.as_bytes())) {
            lines.push(agora_game_api::LoadOrderLine {
                name: name.clone(),
                active: true,
            });
            added.push(name.clone());
        }
    }
    (added, removed)
}

/// Bring an instance's plugin list in step with the plugins its deployment provides.
///
/// The caller holds the instance lock. `None` when the game keeps no plugin list.
pub fn sync_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    desired: &[String],
) -> Result<Option<PluginSyncReport>, PluginListError> {
    sync_locked_in_with_view(ctx, instance_id, definition, None, store, desired, None)
}

/// [`sync_locked`] for one list; `None` names the game's first list. An id the game does not
/// declare is an error, never the first list by accident.
pub(crate) fn sync_locked_in_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    store: &StoreId,
    desired: &[String],
    links: Option<&DeploymentPlan>,
) -> Result<Option<PluginSyncReport>, PluginListError> {
    let Some(rule) = rule_opt(definition, list_id)? else {
        return Ok(None);
    };
    let dir = instance_dir(ctx, instance_id)?;
    let copy_path = dir.join(rule.file.as_str());
    let old = state_of(read_state_file(&dir)?, &rule.id);
    let mut report = PluginSyncReport::default();

    let view = rule.destination == agora_game_api::LoadOrderDestination::View;
    let mapping = if view {
        None
    } else {
        let Some(mapping) = user_file_mapping(definition, rule, store) else {
            if !desired.is_empty() || !old.managed.is_empty() {
                report.warnings.push(format!(
                    "the plugin list is not swapped in for the '{store}' store, so its plugins                      will not be activated"
                ));
            }
            return Ok(Some(report));
        };
        Some(mapping)
    };

    let existing = read_optional(&copy_path)?;
    if existing.is_none() && desired.is_empty() && old.managed.is_empty() && !view {
        return Ok(Some(report));
    }
    if !view {
        recover_stale_session(ctx, definition, store)?;
    }
    // Recovery rewrites the instance copy from the session's file.
    let existing = read_optional(&copy_path)?;
    let have_copy = existing.is_some();
    let mut doc = match existing {
        Some(bytes) => parse_doc(ctx, instance_id, definition, rule, bytes)?,
        None => match mapping {
            Some(mapping) => starting_contents(
                ctx,
                instance_id,
                definition,
                store,
                mapping,
                rule,
                &mut report,
            )?,
            None => blank_doc(rule),
        },
    };

    // A plugin the game loads itself is never Agora's, whatever an earlier sync recorded for it.
    let implicit = implicit_keys(ctx, instance_id, definition, rule, links)?;
    let not_implicit = |n: &String| !implicit.contains(&key(n.as_bytes()));
    let wanted: Vec<String> = desired
        .iter()
        .filter(|n| not_implicit(n))
        .cloned()
        .collect();
    let managed_before: Vec<String> = old
        .managed
        .iter()
        .filter(|n| not_implicit(n))
        .cloned()
        .collect();

    let (added, removed) = match &mut doc {
        ListDoc::Lines(file) => apply_sync(file, rule, &wanted, &managed_before),
        ListDoc::Script(lines) => apply_sync_script(lines, &wanted, &managed_before),
    };
    let changed = !added.is_empty() || !removed.is_empty();
    // The list goes first: a state file that is behind only delays a removal, one that is
    // ahead would forget lines Agora still owns.
    if changed || (!have_copy && !wanted.is_empty()) {
        let rendered = render_doc(ctx, instance_id, definition, rule, &doc)?;
        write_atomic(&copy_path, &rendered)?;
        write_view_file(&dir, rule, &rendered)?;
    } else if view {
        // A hand-edited list whose view file was never written still reaches the plan.
        let rendered = render_doc(ctx, instance_id, definition, rule, &doc)?;
        write_view_file(&dir, rule, &rendered)?;
    }
    if !same_names(&old.managed, &wanted) {
        store_state(
            &dir,
            &rule.id,
            ManagedState {
                managed: wanted,
                locked: old.locked,
            },
        )?;
    }
    report.added = added;
    report.removed = removed;
    Ok(Some(report))
}

/// [`sync_locked`] for a launch of an instance that deploys no content: layers removed since
/// the last deploy still take their lines with them. Works over every list the game keeps.
pub fn sync_without_content(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<Option<PluginSyncReport>, PluginListError> {
    if definition.load_orders.is_empty() {
        return Ok(None);
    }
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "plugins-sync",
    )?;
    let mut all = PluginSyncReport::default();
    for rule in &definition.load_orders {
        if let Some(report) = sync_locked_in_with_view(
            ctx,
            instance_id,
            definition,
            Some(&rule.id),
            store,
            &[],
            None,
        )? {
            all.added.extend(report.added);
            all.removed.extend(report.removed);
            all.warnings.extend(report.warnings);
        }
    }
    Ok(Some(all))
}

// ---------------------------------------------------------------------------
// Inspect and toggle
// ---------------------------------------------------------------------------

/// One list, or `None` when the caller named no id and the game keeps none. Naming an id the
/// game does not declare is an error.
fn rule_opt<'a>(
    definition: &'a GameDefinition,
    list_id: Option<&str>,
) -> Result<Option<&'a LoadOrderDefinition>, PluginListError> {
    match list_id {
        None => Ok(definition.load_orders.first()),
        Some(id) => definition
            .load_orders
            .iter()
            .find(|rule| rule.id == id)
            .map(Some)
            .ok_or_else(|| PluginListError::UnknownList {
                game: definition.id.as_str().to_string(),
                list: id.to_string(),
            }),
    }
}

fn rule_of<'a>(
    definition: &'a GameDefinition,
    list_id: Option<&str>,
) -> Result<&'a LoadOrderDefinition, PluginListError> {
    rule_opt(definition, list_id)?.ok_or_else(|| PluginListError::NoRule(definition.id.clone()))
}

/// The instance's plugin list with each line's state and whether Agora manages it.
pub fn list(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<PluginList, PluginListError> {
    list_in(ctx, instance_id, definition, None)
}

/// [`list`] for one list; `None` names the game's first list.
pub fn list_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<PluginList, PluginListError> {
    let rule = rule_of(definition, list_id)?;
    crate::game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        crate::game_instance::InstanceError::NotFound(id) => PluginListError::InstanceNotFound(id),
        other => PluginListError::Other(other.to_string()),
    })?;
    let dir = instance_dir(ctx, instance_id)?;
    let path = dir.join(rule.file.as_str());
    let state = state_of(read_state_file(&dir)?, &rule.id);
    let managed: std::collections::HashSet<Vec<u8>> =
        state.managed.iter().map(|n| key(n.as_bytes())).collect();
    let locked: std::collections::HashSet<Vec<u8>> =
        state.locked.iter().map(|n| key(n.as_bytes())).collect();
    let Some(bytes) = read_optional(&path)? else {
        return Ok(PluginList {
            list_id: rule.id.clone(),
            list_name: rule.name.clone(),
            path,
            exists: false,
            entries: Vec::new(),
        });
    };
    let doc = parse_doc(ctx, instance_id, definition, rule, bytes)?;
    let entries = doc_entries(&doc, rule)
        .into_iter()
        .map(|(name, active)| PluginEntry {
            managed: managed.contains(&key(name.as_bytes())),
            locked: locked.contains(&key(name.as_bytes())),
            name,
            active,
        })
        .collect();
    Ok(PluginList {
        list_id: rule.id.clone(),
        list_name: rule.name.clone(),
        path,
        exists: true,
        entries,
    })
}

/// The store an instance runs under, when it can be told.
pub fn instance_store(ctx: &Ctx, instance_id: &str) -> Result<Option<StoreId>, PluginListError> {
    let manifest = crate::game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        crate::game_instance::InstanceError::NotFound(id) => PluginListError::InstanceNotFound(id),
        other => PluginListError::Other(other.to_string()),
    })?;
    match &manifest.base {
        agora_game_api::BaseReference::Pinned { id, .. } => {
            let path = ctx.paths.base_manifest_path(id);
            let text =
                std::fs::read_to_string(&path).map_err(|source| PluginListError::Unreadable {
                    path: path.clone(),
                    source,
                })?;
            let base: crate::game_base::BaseManifest = serde_json::from_str(&text)?;
            Ok(Some(base.runtime.store))
        }
        agora_game_api::BaseReference::Unpinned { .. } => {
            Ok(manifest.runtime_identity.map(|r| r.store))
        }
    }
}

/// Activate or deactivate one plugin in an instance's list. Returns whether the line changed.
pub fn set_active(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    name: &str,
    active: bool,
) -> Result<bool, PluginListError> {
    set_active_in(ctx, instance_id, definition, None, name, active)
}

/// [`set_active`] for one list; `None` names the game's first list.
pub fn set_active_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    name: &str,
    active: bool,
) -> Result<bool, PluginListError> {
    let rule = rule_of(definition, list_id)?;
    if !active
        && matches!(
            &rule.format,
            LoadOrderFormat::Lines { active_prefix, .. } if active_prefix.is_empty()
        )
    {
        return Err(PluginListError::NoInactiveState(name.to_string()));
    }
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "plugins-toggle",
    )?;
    ensure_copy_editable(ctx, instance_id, definition)?;

    let dir = instance_dir(ctx, instance_id)?;
    let path = dir.join(rule.file.as_str());
    let Some(bytes) = read_optional(&path)? else {
        return Err(PluginListError::NotInList(name.to_string()));
    };
    let mut doc = parse_doc(ctx, instance_id, definition, rule, bytes)?;
    let wanted = key(name.trim().as_bytes());
    let mut found = false;
    let mut changed = false;
    match &mut doc {
        ListDoc::Lines(file) => {
            for line in &mut file.lines {
                let Some((is_active, line_name)) = plugin_line(line, rule) else {
                    continue;
                };
                if key(line_name) != wanted {
                    continue;
                }
                found = true;
                if is_active != active {
                    *line = render_line(rule, line_name, active);
                    changed = true;
                }
            }
        }
        ListDoc::Script(lines) => {
            for line in lines.iter_mut() {
                if key(line.name.as_bytes()) != wanted {
                    continue;
                }
                found = true;
                if line.active != active {
                    line.active = active;
                    changed = true;
                }
            }
        }
    }
    if !found {
        return Err(PluginListError::NotInList(name.to_string()));
    }
    if changed {
        let rendered = render_doc(ctx, instance_id, definition, rule, &doc)?;
        write_atomic(&path, &rendered)?;
        write_view_file(&dir, rule, &rendered)?;
    }
    Ok(changed)
}

/// Refuse to edit an instance's copy of the list while its game runs, after putting back any
/// session a finished run left behind. The caller holds the instance lock.
fn ensure_copy_editable(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<(), PluginListError> {
    if let Some(store) = instance_store(ctx, instance_id)? {
        recover_stale_session(ctx, definition, &store)?;
        // A running session of this instance rewrites its copy of the list when it ends.
        if let Some(status) = crate::game_user_files::status(ctx, &definition.id, &store)? {
            if status.running && status.instance == instance_id {
                return Err(PluginListError::InstanceRunning(instance_id.to_string()));
            }
        }
    }
    Ok(())
}

/// Write an instance's plugin list from an import (MASTER_SPEC §26.10). `lines` are the plugins in
/// load order with whether each is active; `managed` are the ones a content layer deploys, so a
/// later sync keeps their states rather than activating them; `locked` are the names the import
/// locked. The list and its state are replaced. Refused while the game runs. The caller holds the
/// instance lock.
pub fn import_list(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    lines: &[(String, bool)],
    managed: &[String],
    locked: &[String],
) -> Result<(), PluginListError> {
    import_list_in(ctx, instance_id, definition, None, lines, managed, locked)
}

/// [`import_list`] for one list; `None` names the game's first list.
pub fn import_list_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    lines: &[(String, bool)],
    managed: &[String],
    locked: &[String],
) -> Result<(), PluginListError> {
    let rule = rule_of(definition, list_id)?;
    if matches!(
        &rule.format,
        LoadOrderFormat::Lines { active_prefix, .. } if active_prefix.is_empty()
    ) {
        if let Some((name, _)) = lines.iter().find(|(_, active)| !active) {
            return Err(PluginListError::NoInactiveState(name.clone()));
        }
    }
    ensure_copy_editable(ctx, instance_id, definition)?;
    let dir = instance_dir(ctx, instance_id)?;
    let path = dir.join(rule.file.as_str());
    let doc = match &rule.format {
        LoadOrderFormat::Lines { .. } => {
            let mut file = ListFile::from_header(rule);
            for (name, active) in lines {
                file.lines.push(render_line(rule, name.as_bytes(), *active));
            }
            ListDoc::Lines(file)
        }
        LoadOrderFormat::Script { .. } => ListDoc::Script(
            lines
                .iter()
                .map(|(name, active)| agora_game_api::LoadOrderLine {
                    name: name.clone(),
                    active: *active,
                })
                .collect(),
        ),
    };
    let rendered = render_doc(ctx, instance_id, definition, rule, &doc)?;
    write_atomic(&path, &rendered)?;
    write_view_file(&dir, rule, &rendered)?;
    store_state(
        &dir,
        &rule.id,
        ManagedState {
            managed: managed.to_vec(),
            locked: locked.to_vec(),
        },
    )
}

/// Lock or unlock a listed plugin's place in the order. Returns whether the state changed.
/// Only the state file is written, so a running game is not a reason to refuse.
pub fn set_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    name: &str,
    locked: bool,
) -> Result<bool, PluginListError> {
    set_locked_in(ctx, instance_id, definition, None, name, locked)
}

/// [`set_locked`] for one list; `None` names the game's first list.
pub fn set_locked_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    name: &str,
    locked: bool,
) -> Result<bool, PluginListError> {
    let rule = rule_of(definition, list_id)?;
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "plugins-lock",
    )?;
    let wanted = key(name.trim().as_bytes());
    let listed = list_in(ctx, instance_id, definition, Some(&rule.id))?;
    let Some(entry) = listed
        .entries
        .iter()
        .find(|e| key(e.name.as_bytes()) == wanted)
    else {
        return Err(PluginListError::NotInList(name.to_string()));
    };
    let canonical = entry.name.clone();
    let dir = instance_dir(ctx, instance_id)?;
    let mut state = state_of(read_state_file(&dir)?, &rule.id);
    let is_locked = state.locked.iter().any(|n| key(n.as_bytes()) == wanted);
    if is_locked == locked {
        return Ok(false);
    }
    if locked {
        state.locked.push(canonical);
    } else {
        state.locked.retain(|n| key(n.as_bytes()) != wanted);
    }
    store_state(&dir, &rule.id, state)?;
    Ok(true)
}

/// Rewrite the order of an instance's listed plugins. `order` names every plugin line the caller
/// reorders, in its new order, with each line's active state. A line keeps its own bytes; the
/// lines `order` does not name (comments, lines for always-loaded plugins) keep their places.
/// Returns whether the file changed. Refused while the game runs. The caller holds the instance
/// lock.
pub fn write_order_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    order: &[(String, bool)],
) -> Result<bool, PluginListError> {
    write_order_locked_in(ctx, instance_id, definition, None, order)
}

/// [`write_order_locked`] for one list; `None` names the game's first list.
pub fn write_order_locked_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    order: &[(String, bool)],
) -> Result<bool, PluginListError> {
    let rule = rule_of(definition, list_id)?;
    ensure_copy_editable(ctx, instance_id, definition)?;
    let dir = instance_dir(ctx, instance_id)?;
    let path = dir.join(rule.file.as_str());
    let Some(bytes) = read_optional(&path)? else {
        if order.is_empty() {
            return Ok(false);
        }
        return Err(PluginListError::Other(format!(
            "instance '{instance_id}' has no plugin list to reorder"
        )));
    };
    let mut doc = parse_doc(ctx, instance_id, definition, rule, bytes)?;
    let changed = match &mut doc {
        ListDoc::Lines(file) => {
            let before = file.lines.clone();
            // Names in the order are lossy strings from `list`, so both sides use the lossy key.
            let lossy_key = |raw: &[u8]| key(String::from_utf8_lossy(raw).as_bytes());
            let wanted: std::collections::HashSet<Vec<u8>> =
                order.iter().map(|(name, _)| key(name.as_bytes())).collect();
            let mut slots = Vec::new();
            let mut originals: std::collections::HashMap<
                Vec<u8>,
                std::collections::VecDeque<Vec<u8>>,
            > = std::collections::HashMap::new();
            for (index, line) in file.lines.iter().enumerate() {
                let Some((_, name)) = plugin_line(line, rule) else {
                    continue;
                };
                let k = lossy_key(name);
                originals
                    .entry(k.clone())
                    .or_default()
                    .push_back(name.to_vec());
                if wanted.contains(&k) {
                    slots.push(index);
                }
            }
            if slots.len() != order.len() {
                return Err(PluginListError::Other(format!(
                    "the plugin list of instance '{instance_id}' changed while it was being reordered; nothing was written"
                )));
            }
            for (slot, (name, active)) in slots.into_iter().zip(order) {
                let k = key(name.as_bytes());
                let original = originals
                    .get_mut(&k)
                    .and_then(std::collections::VecDeque::pop_front)
                    .ok_or_else(|| {
                        PluginListError::Other(format!(
                            "the plugin list of instance '{instance_id}' changed while it was being reordered; nothing was written"
                        ))
                    })?;
                file.lines[slot] = render_line(rule, &original, *active);
            }
            file.lines != before
        }
        ListDoc::Script(lines) => {
            let before = lines.clone();
            if lines.len() != order.len() {
                return Err(PluginListError::Other(format!(
                    "the plugin list of instance '{instance_id}' changed while it was being reordered; nothing was written"
                )));
            }
            let mut originals: std::collections::HashMap<
                Vec<u8>,
                std::collections::VecDeque<String>,
            > = std::collections::HashMap::new();
            for line in lines.iter() {
                originals
                    .entry(key(line.name.as_bytes()))
                    .or_default()
                    .push_back(line.name.clone());
            }
            let mut rebuilt = Vec::with_capacity(order.len());
            for (name, active) in order {
                let original = originals
                    .get_mut(&key(name.as_bytes()))
                    .and_then(std::collections::VecDeque::pop_front)
                    .ok_or_else(|| {
                        PluginListError::Other(format!(
                            "the plugin list of instance '{instance_id}' changed while it was being reordered; nothing was written"
                        ))
                    })?;
                rebuilt.push(agora_game_api::LoadOrderLine {
                    name: original,
                    active: *active,
                });
            }
            *lines = rebuilt;
            *lines != before
        }
    };
    if !changed {
        return Ok(false);
    }
    let rendered = render_doc(ctx, instance_id, definition, rule, &doc)?;
    write_atomic(&path, &rendered)?;
    write_view_file(&dir, rule, &rendered)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_game_api::RelPath;

    fn rule() -> LoadOrderDefinition {
        LoadOrderDefinition {
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
                header: vec!["# header".into()],
            },
            rules: agora_game_api::LoadOrderRules {
                kind: "none".into(),
                entry: None,
                implicit: Vec::new(),
                implicit_list_file: None,
                sorter: None,
            },
            intro: None,
            item_heading: None,
        }
    }

    fn lines(file: &ListFile) -> Vec<String> {
        file.lines
            .iter()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_keeps_crlf_and_drops_the_trailing_blank() {
        let f = ListFile::parse(b"# c\r\n*A.esp\r\nB.esp\r\n");
        assert_eq!(lines(&f), ["# c", "*A.esp", "B.esp"]);
        assert_eq!(f.render(), b"# c\r\n*A.esp\r\nB.esp\r\n");
    }

    #[test]
    fn parse_empty_input_is_an_empty_list_that_renders_to_nothing() {
        let f = ListFile::parse(b"");
        assert!(f.lines.is_empty());
        assert!(f.render().is_empty());
    }

    #[test]
    fn lf_files_stay_lf() {
        let f = ListFile::parse(b"*A.esp\n");
        assert_eq!(f.render(), b"*A.esp\n");
    }

    #[test]
    fn plugin_lines_tell_active_inactive_and_noise_apart() {
        let r = rule();
        assert_eq!(plugin_line(b"*A.esp", &r), Some((true, &b"A.esp"[..])));
        assert_eq!(plugin_line(b"A.esp", &r), Some((false, &b"A.esp"[..])));
        assert_eq!(
            plugin_line(b"  *  A.esp  ", &r),
            Some((true, &b"A.esp"[..]))
        );
        assert_eq!(plugin_line(b"# *A.esp", &r), None);
        assert_eq!(plugin_line(b"", &r), None);
        assert_eq!(plugin_line(b"   ", &r), None);
        assert_eq!(plugin_line(b"*", &r), None);
    }

    #[test]
    fn sync_appends_new_plugins_active_after_existing_lines() {
        let mut f = ListFile::parse(b"# c\r\n*Old.esp\r\n");
        let (added, removed) = apply_sync(&mut f, &rule(), &names(&["B.esm", "A.esp"]), &[]);
        assert_eq!(added, ["B.esm", "A.esp"]);
        assert!(removed.is_empty());
        assert_eq!(lines(&f), ["# c", "*Old.esp", "*B.esm", "*A.esp"]);
    }

    #[test]
    fn a_line_the_game_added_inactive_is_activated_when_agora_starts_managing_it() {
        // Skyrim appends plugins it finds in Data, inactive, before Agora has managed them.
        let r = rule();
        let mut f = ListFile::parse(b"# header\r\nCoolMod.esp\r\n");
        let (added, removed) = apply_sync(&mut f, &r, &["CoolMod.esp".to_string()], &[]);
        assert_eq!(added, vec!["CoolMod.esp".to_string()]);
        assert!(removed.is_empty());
        assert_eq!(f.render(), b"# header\r\n*CoolMod.esp\r\n".to_vec());
        // Once managed, the user's later choice to deactivate it is kept.
        let mut f = ListFile::parse(b"CoolMod.esp\r\n");
        let managed = ["CoolMod.esp".to_string()];
        let (added, _) = apply_sync(&mut f, &r, &managed, &managed);
        assert!(added.is_empty());
        assert_eq!(f.render(), b"CoolMod.esp\r\n".to_vec());
    }

    #[test]
    fn sync_keeps_an_inactive_line_inactive_and_in_place() {
        let mut f = ListFile::parse(b"*X.esp\r\nA.esp\r\n*Y.esp\r\n");
        let (added, _) = apply_sync(&mut f, &rule(), &names(&["A.esp"]), &names(&["A.esp"]));
        assert!(added.is_empty());
        assert_eq!(lines(&f), ["*X.esp", "A.esp", "*Y.esp"]);
    }

    #[test]
    fn sync_matches_names_case_insensitively_and_keeps_one_line() {
        let mut f = ListFile::parse(b"*a.ESP\r\n");
        let (added, removed) = apply_sync(&mut f, &rule(), &names(&["A.esp"]), &names(&["A.esp"]));
        assert!(added.is_empty() && removed.is_empty());
        assert_eq!(lines(&f), ["*a.ESP"]);
    }

    #[test]
    fn sync_matches_non_ascii_plugin_names_the_way_ntfs_does() {
        // `key` still lower-cases ASCII only, while the planner folds Unicode: `Ü.esp` and `ü.esp`
        // are one file on NTFS (checked 2026-10-31), but this writes a second line for it.
        let mut f = ListFile::parse("*ü.esp\r\n".as_bytes());
        let (added, removed) = apply_sync(&mut f, &rule(), &names(&["Ü.esp"]), &[]);
        assert!(added.is_empty() && removed.is_empty());
        assert_eq!(lines(&f), ["*ü.esp"]);
    }

    #[test]
    fn sync_removes_only_lines_agora_managed() {
        let mut f = ListFile::parse(b"*Gone.esp\r\n*Mine.esp\r\n# note\r\n*Kept.esp\r\n");
        let (added, removed) = apply_sync(
            &mut f,
            &rule(),
            &names(&["Kept.esp"]),
            &names(&["gone.ESP", "Kept.esp"]),
        );
        assert!(added.is_empty());
        assert_eq!(removed, ["Gone.esp"]);
        assert_eq!(lines(&f), ["*Mine.esp", "# note", "*Kept.esp"]);
    }

    #[test]
    fn sync_with_nothing_desired_and_nothing_managed_changes_nothing() {
        let mut f = ListFile::parse(b"*A.esp\r\n");
        let (added, removed) = apply_sync(&mut f, &rule(), &[], &[]);
        assert!(added.is_empty() && removed.is_empty());
        assert_eq!(lines(&f), ["*A.esp"]);
    }

    #[test]
    fn non_utf8_lines_survive_byte_for_byte() {
        let raw = b"*Caf\xe9.esp\r\n";
        let mut f = ListFile::parse(raw);
        apply_sync(&mut f, &rule(), &names(&["New.esp"]), &[]);
        assert!(f.render().starts_with(raw));
    }

    #[test]
    fn a_rule_without_a_prefix_treats_every_line_as_active() {
        let mut r = rule();
        if let agora_game_api::LoadOrderFormat::Lines { active_prefix, .. } = &mut r.format {
            *active_prefix = String::new();
        }
        assert_eq!(plugin_line(b"A.esp", &r), Some((true, &b"A.esp"[..])));
        let mut f = ListFile::parse(b"A.esp\r\n");
        apply_sync(&mut f, &r, &names(&["B.esp"]), &[]);
        assert_eq!(lines(&f), ["A.esp", "B.esp"]);
    }
}
