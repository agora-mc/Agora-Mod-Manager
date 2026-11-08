//! The security boundary between plugin scripts and a game instance
//! (MASTER_SPEC §30.12).
//!
//! Two things live here:
//!
//! - [`GameViewReads`], the `game:read` scope: a merged view addressed by game
//!   path, with a byte budget, refusing `..`, absolute paths and every reparse
//!   point (a junction included). A script never sees a real path.
//! - [`GameWriteScope`], the `game:write` scope: one plugin's own generated
//!   layer for one instance, with byte and file budgets, atomic writes and
//!   `RelPath` checks.
//!
//! Launch recipes returned by a `prepare_launch` hook are checked by
//! [`validate_launch_recipe`] before core runs anything.

use agora_game_api::{GamePath, LaunchRecipe, LaunchValue, RelPath, UserDataLocation};
use agora_plugin_api::host::{path_matches_any, GameReads, ScriptReadError};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead as _, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Byte ceiling a script asks for in one `game.readBytes` call unless it says otherwise.
pub const DEFAULT_MAX_READ_BYTES: u64 = 1024 * 1024;
/// Bytes one hook or one plugin call may read in total.
pub const DEFAULT_MAX_TOTAL_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Lines one `game.readLines` call may return.
pub const DEFAULT_MAX_LINES: u32 = 10_000;
/// Longest line returned, in characters.
pub const DEFAULT_MAX_LINE_LEN: u32 = 8 * 1024;
/// Entries one `game.list` call may return before it says the list was cut.
pub const DEFAULT_MAX_LIST_ENTRIES: u32 = 4_096;

/// Bytes a single write may add.
pub const DEFAULT_MAX_WRITE_BYTES: u64 = 4 * 1024 * 1024;
/// Bytes one hook or one plugin call may write in total.
pub const DEFAULT_MAX_TOTAL_WRITE_BYTES: u64 = 16 * 1024 * 1024;
/// Files one hook or one plugin call may create in its generated layer.
pub const DEFAULT_MAX_WRITE_FILES: u32 = 256;
/// Total bytes a plugin's generated layer for one instance may hold. A
/// successful write that would pass this is refused, so repeated calls cannot
/// fill the disk one file at a time.
pub const DEFAULT_MAX_LAYER_BYTES: u64 = 64 * 1024 * 1024;

/// The budgets one read scope enforces.
#[derive(Debug, Clone, Copy)]
pub struct GameReadLimits {
    pub max_read_bytes: u64,
    pub max_total_read_bytes: u64,
    pub max_lines: u32,
    pub max_line_len: u32,
    pub max_list_entries: u32,
}

impl Default for GameReadLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_total_read_bytes: DEFAULT_MAX_TOTAL_READ_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_line_len: DEFAULT_MAX_LINE_LEN,
            max_list_entries: DEFAULT_MAX_LIST_ENTRIES,
        }
    }
}

/// The budgets one write scope enforces.
#[derive(Debug, Clone, Copy)]
pub struct GameWriteLimits {
    pub max_write_bytes: u64,
    pub max_total_write_bytes: u64,
    pub max_files: u32,
    pub max_layer_bytes: u64,
}

impl Default for GameWriteLimits {
    fn default() -> Self {
        Self {
            max_write_bytes: DEFAULT_MAX_WRITE_BYTES,
            max_total_write_bytes: DEFAULT_MAX_TOTAL_WRITE_BYTES,
            max_files: DEFAULT_MAX_WRITE_FILES,
            max_layer_bytes: DEFAULT_MAX_LAYER_BYTES,
        }
    }
}

fn refusal(code: &str, message: impl Into<String>) -> ScriptReadError {
    ScriptReadError::new(code, message)
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// Bytes one top-level plugin invocation or hook has read so far.
#[derive(Debug, Default)]
pub struct ReadBudget {
    total: AtomicU64,
}

impl ReadBudget {
    fn spend(&self, limits: &GameReadLimits, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > limits.max_total_read_bytes {
                return Err(refusal(
                    "budget_spent",
                    format!(
                        "this plugin call has read its whole {} byte budget",
                        limits.max_total_read_bytes
                    ),
                ));
            }
            match self.total.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

/// Bytes and files one top-level plugin invocation or hook has written so far.
#[derive(Debug, Default)]
pub struct WriteBudget {
    total: AtomicU64,
    files: AtomicU32,
}

impl WriteBudget {
    fn spend(&self, limits: &GameWriteLimits, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > limits.max_total_write_bytes {
                return Err(refusal(
                    "budget_spent",
                    format!(
                        "this plugin call has written its whole {} byte budget",
                        limits.max_total_write_bytes
                    ),
                ));
            }
            match self.total.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn claim_file(&self, limits: &GameWriteLimits) -> Result<(), ScriptReadError> {
        let mut current = self.files.load(Ordering::SeqCst);
        loop {
            if current >= limits.max_files {
                return Err(refusal(
                    "file_budget_spent",
                    format!(
                        "this plugin call has created its whole {} file budget",
                        limits.max_files
                    ),
                ));
            }
            match self.files.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        Ok(())
    }
}

/// The budgets shared by every `game.*` call in one top-level plugin invocation
/// or hook. A new invocation gets a new session; a call inside it reuses it.
#[derive(Clone, Debug, Default)]
pub struct BudgetSession {
    pub reads: Arc<ReadBudget>,
    pub writes: Arc<WriteBudget>,
}

/// Per-plugin budget sessions carried on the context. Dispatch uses the
/// session `PluginService` opened for the top-level call; a direct call with no
/// session (tests, a plugin acting outside an invocation) gets one that lasts
/// until the process stops, which is stricter rather than looser.
#[derive(Clone, Default)]
pub struct GameBudgets {
    sessions: Arc<Mutex<HashMap<String, BudgetSession>>>,
    /// How many event handlers are running per plugin. An event handler opens a
    /// fresh session when the first one starts and closes it when the last one
    /// ends, so one handler's `game.*` calls never share another handler's
    /// budget (MASTER_SPEC §30.12).
    handlers: Arc<Mutex<HashMap<String, u32>>>,
}

impl GameBudgets {
    /// Open a fresh session for one top-level call. Dropping the guard closes
    /// it, even when the call returned an error.
    pub fn begin(&self, plugin_id: &str) -> BudgetGuard {
        let session = BudgetSession::default();
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(plugin_id.to_string(), session);
        }
        BudgetGuard {
            budgets: self.clone(),
            plugin_id: plugin_id.to_string(),
        }
    }

    /// Start one event handler call. The first handler for a plugin opens a
    /// fresh budget session; nested handlers share it.
    pub fn start_handler(&self, plugin_id: &str) {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let depth = handlers.entry(plugin_id.to_string()).or_insert(0);
        *depth += 1;
        if *depth == 1 {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.insert(plugin_id.to_string(), BudgetSession::default());
            }
        }
    }

    /// End one event handler call, closing the session when the last one ends.
    pub fn end_handler(&self, plugin_id: &str) {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(depth) = handlers.get_mut(plugin_id) else {
            return;
        };
        *depth = depth.saturating_sub(1);
        if *depth == 0 {
            handlers.remove(plugin_id);
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(plugin_id);
            }
        }
    }

    /// The session for a plugin, creating one if the caller did not open one.
    pub fn session(&self, plugin_id: &str) -> BudgetSession {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.entry(plugin_id.to_string()).or_default().clone()
    }
}

/// Closes a budget session when a top-level call ends.
pub struct BudgetGuard {
    budgets: GameBudgets,
    plugin_id: String,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.budgets.sessions.lock() {
            sessions.remove(&self.plugin_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Path checks
// ---------------------------------------------------------------------------

/// Parse a game path the way a script wrote it.
///
/// `RelPath` already rejects absolute forms, drive letters, colons and `..`.
/// This adds the check that a component is not the empty string (which would
/// silently collapse `a//b`) and rejects `.` so a reply always names the path
/// the script used.
pub fn parse_game_path(path: &str) -> Result<RelPath, ScriptReadError> {
    if path.starts_with('/') || path.starts_with('\\') || path.contains(':') {
        return Err(refusal(
            "absolute_path",
            "a game path is relative to the instance, never absolute",
        ));
    }
    if path.split(['/', '\\']).any(|part| part == "..") {
        return Err(refusal("outside_view", "a game path may not contain `..`"));
    }
    // Characters that look like a separator or a dot to a reader but are not
    // interpreted by the filesystem are refused on both read and write. They
    // are not traversal by themselves; refusing them keeps the path the game
    // sees and the path the script thinks it wrote from drifting apart.
    if path.chars().any(|c| {
        matches!(
            c,
            '\u{FF0F}'
                | '\u{FF3C}'
                | '\u{FF0E}'
                | '\u{3002}'
                | '\u{FF61}'
                | '\u{2024}'
                | '\u{FE52}'
        )
    }) {
        return Err(refusal(
            "invalid_path",
            "that path contains a character that looks like a separator or a dot",
        ));
    }
    let rel = RelPath::new(path)
        .map_err(|_| refusal("outside_view", "that is not a game-relative path"))?;
    if rel
        .as_str()
        .split('/')
        .filter(|part| !part.is_empty())
        .any(is_device_name)
    {
        return Err(refusal(
            "device_name",
            "that path names a Windows device, so it is refused",
        ));
    }
    Ok(rel)
}

/// A write may not name a Windows device. Reuses the store's list, which
/// already covers `COM0`/`LPT0`, the superscript forms and trailing spaces
/// (Microsoft, "Naming Files, Paths, and Namespaces").
fn is_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part);
    let upper = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(upper.as_str(), "CONIN$" | "CONOUT$")
        || crate::content_store::is_windows_device_name(stem)
}

fn check_write_path(path: &str) -> Result<RelPath, ScriptReadError> {
    let rel = parse_game_path(path)?;
    if rel.is_empty() {
        return Err(refusal("invalid_path", "a write needs a file path"));
    }
    if rel.as_str().split('/').any(is_device_name) {
        return Err(refusal(
            "device_name",
            "that path names a Windows device, so it is refused",
        ));
    }
    Ok(rel)
}

/// Whether `meta` is a reparse point (a symlink or a Windows junction).
///
/// On Windows a junction is not reported by `is_symlink`, but it is a reparse
/// point. Microsoft documents `FILE_ATTRIBUTE_REPARSE_POINT` as the attribute
/// every reparse point carries:
/// <https://learn.microsoft.com/windows/win32/fileio/file-attribute-constants>
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Refuse if any component of `path` below `root` is a reparse point.
///
/// Checked component by component, so a junction *mid path* is caught as well
/// as one at the leaf. A path that does not exist yet is fine as long as the
/// components that do exist are ordinary directories.
fn reject_reparse_chain(root: &Path, rel: &str) -> Result<(), ScriptReadError> {
    let mut current = root.to_path_buf();
    for part in rel.split('/').filter(|part| !part.is_empty()) {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if is_reparse_point(&meta) => {
                return Err(refusal(
                    "reparse_point",
                    "a link in that path is not followed, so it is refused",
                ));
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A view made of trusted physical roots, highest precedence first.
///
/// The roots are physical directories core chose; the script only ever names a
/// game path. The root itself is trusted — a person may redirect a game folder
/// through a junction — and every component *below* it is checked. No reply,
/// error or log line contains one of them.
pub struct GameViewReads {
    roots: Vec<PathBuf>,
    /// When present, the exact merged view: one source per game path, already
    /// resolved in deploy precedence. Roots are then only a fallback for a
    /// view that has no deployment.
    entries: Option<HashMap<String, ViewEntry>>,
    limits: GameReadLimits,
    budget: Arc<ReadBudget>,
    instance: Option<String>,
}

/// One file in a merged view, with the trusted root it came from and its game
/// path. The trusted root is never shown to a script.
pub struct ViewEntry {
    pub source: PathBuf,
    pub trusted_root: PathBuf,
    pub game_path: String,
    /// Deploy precedence rank: base 1, content 2, generated 3, writable/override 4.
    pub rank: u8,
}

impl GameViewReads {
    pub fn new(roots: Vec<PathBuf>, limits: GameReadLimits) -> Self {
        Self::with_budget(roots, limits, Arc::new(ReadBudget::default()))
    }

    pub fn with_budget(
        roots: Vec<PathBuf>,
        limits: GameReadLimits,
        budget: Arc<ReadBudget>,
    ) -> Self {
        Self {
            roots,
            entries: None,
            limits,
            budget,
            instance: None,
        }
    }

    /// A view resolved from a deployment plan, highest precedence already
    /// applied. `entries` is keyed by folded game path.
    pub fn from_entries(
        entries: HashMap<String, ViewEntry>,
        limits: GameReadLimits,
        budget: Arc<ReadBudget>,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            roots: Vec::new(),
            entries: Some(entries),
            limits,
            budget,
            instance: Some(instance.into()),
        }
    }

    /// Bind this scope to one instance. A hook's `game.*` calls then refuse a
    /// different instance id.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// The physical file or directory `rel` names, if any layer has it.
    ///
    /// A reparse point in the *chosen* root is an error, not a fall-through:
    /// quietly reading a lower layer would hide the fact that a link was put
    /// in the way.
    fn physical(&self, rel: &str) -> Result<Option<PathBuf>, ScriptReadError> {
        if let Some(entries) = &self.entries {
            let Some(entry) = entries.get(&agora_game_api::fold_path_case(rel)) else {
                return Ok(None);
            };
            let from_root = entry
                .source
                .strip_prefix(&entry.trusted_root)
                .unwrap_or(&entry.source);
            reject_reparse_chain(
                &entry.trusted_root,
                &from_root.to_string_lossy().replace('\\', "/"),
            )?;
            return Ok(Some(entry.source.clone()));
        }
        for root in &self.roots {
            reject_reparse_chain(root, rel)?;
            let candidate = rel
                .split('/')
                .filter(|part| !part.is_empty())
                .fold(root.clone(), |path, part| path.join(part));
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => return Ok(Some(candidate)),
                Err(_) => continue,
            }
        }
        Ok(None)
    }
}

/// Read one line, bounded by `cap` bytes, without holding the rest of a large
/// line in memory. Returns the bytes consumed so the caller can charge them.
fn read_line_capped(
    reader: &mut impl std::io::BufRead,
    cap: usize,
) -> Result<(Vec<u8>, u64, bool), ScriptReadError> {
    let mut out = Vec::new();
    let mut consumed = 0u64;
    let mut cut = false;
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|_| refusal("unreadable", "that path could not be read"))?;
        if buffer.is_empty() {
            break;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|position| position + 1).unwrap_or(buffer.len());
        consumed += take as u64;
        if out.len() < cap {
            let room = cap - out.len();
            let keep = take.min(room);
            out.extend_from_slice(&buffer[..keep]);
            if keep < take {
                cut = true;
            }
        } else {
            cut = true;
        }
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    Ok((out, consumed, cut))
}

impl GameReads for GameViewReads {
    fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }

    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError> {
        if len > self.limits.max_read_bytes {
            return Err(refusal(
                "too_large",
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let rel = parse_game_path(path)?;
        let Some(physical) = self.physical(rel.as_str())? else {
            return Err(refusal("not_found", "that path is not in this instance"));
        };
        // Read only the window asked for: a script must not be able to make
        // the launcher hold a large file in memory to return 10 bytes of it.
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut file = std::fs::File::open(&physical)
            .map_err(|_| refusal("unreadable", "that path could not be read"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| refusal("unreadable", "that offset could not be read"))?;
        let mut data = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(len)
            .read_to_end(&mut data)
            .map_err(|_| refusal("unreadable", "that path could not be read"))?;
        self.budget.spend(&self.limits, data.len() as u64)?;
        Ok(data)
    }

    fn read_lines(
        &self,
        path: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let rel = parse_game_path(path)?;
        let Some(physical) = self.physical(rel.as_str())? else {
            return Err(refusal("not_found", "that path is not in this instance"));
        };
        let max_lines = max_lines.min(self.limits.max_lines);
        let max_len = self.limits.max_line_len as usize;
        let byte_cap = max_len.saturating_mul(4);
        let mut reader = std::io::BufReader::new(
            std::fs::File::open(&physical)
                .map_err(|_| refusal("unreadable", "that path could not be read"))?,
        );

        let mut consumed = 0u64;
        let mut index = 0u64;
        // Skipped lines are read, so they are charged: a page deep in a huge
        // file must not be cheaper than reading the file from the start.
        while index < from_line {
            let (_, read, _) = read_line_capped(&mut reader, 0)?;
            if read == 0 {
                break;
            }
            consumed += read;
            index += 1;
        }

        let mut lines = Vec::new();
        let mut cut = false;
        while lines.len() < max_lines as usize {
            let (bytes, read, was_cut) = read_line_capped(&mut reader, byte_cap)?;
            if read == 0 {
                break;
            }
            consumed += read;
            index += 1;
            cut |= was_cut;
            let text = String::from_utf8_lossy(&bytes);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let text = text.strip_suffix('\r').unwrap_or(text);
            let text = match text.char_indices().nth(max_len) {
                Some((byte, _)) => {
                    cut = true;
                    &text[..byte]
                }
                None => text,
            };
            lines.push(text.to_string());
        }
        let more = !reader
            .fill_buf()
            .map_err(|_| refusal("unreadable", "that path could not be read"))?
            .is_empty();
        self.budget.spend(&self.limits, consumed)?;
        Ok(serde_json::json!({
            "lines": lines,
            "nextLine": more.then_some(index),
            "cut": cut,
        }))
    }

    fn list(&self, path: &str) -> Result<serde_json::Value, ScriptReadError> {
        let rel = parse_game_path(path)?;
        let rel_str = rel.as_str().to_string();
        if let Some(view) = &self.entries {
            let prefix = if rel_str.is_empty() {
                String::new()
            } else {
                format!("{}/", agora_game_api::fold_path_case(&rel_str))
            };
            let mut found: BTreeMap<String, (bool, u64)> = BTreeMap::new();
            let mut cut = false;
            for (folded, entry) in view {
                let Some(rest) = folded.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                let name = rest.split('/').next().unwrap_or(rest).to_string();
                let is_dir = rest.contains('/');
                if found.len() >= self.limits.max_list_entries as usize {
                    cut = true;
                    break;
                }
                self.budget.spend(&self.limits, name.len() as u64 + 64)?;
                let size = if is_dir {
                    0
                } else {
                    std::fs::metadata(&entry.source)
                        .map(|meta| meta.len())
                        .unwrap_or(0)
                };
                found.entry(name).or_insert((is_dir, size));
            }
            let entries: Vec<serde_json::Value> = found
                .into_iter()
                .map(|(name, (dir, size))| serde_json::json!({ "name": name, "dir": dir, "size": size }))
                .collect();
            return Ok(serde_json::json!({ "entries": entries, "cut": cut }));
        }
        // The merged view can hold the same directory in several layers. The
        // highest layer wins for a name, exactly as a read does.
        let mut entries: BTreeMap<String, (bool, u64)> = BTreeMap::new();
        let mut cut = false;
        'roots: for root in &self.roots {
            reject_reparse_chain(root, &rel_str)?;
            let dir = rel_str
                .split('/')
                .filter(|part| !part.is_empty())
                .fold(root.clone(), |path, part| path.join(part));
            let Ok(read_dir) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read_dir.flatten() {
                // A reparse point inside the listed directory is not shown at
                // all: listing it would invite a script to read it next.
                let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                if is_reparse_point(&meta) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if entries.len() >= self.limits.max_list_entries as usize {
                    cut = true;
                    break 'roots;
                }
                self.budget.spend(&self.limits, name.len() as u64 + 64)?;
                entries.entry(name).or_insert((meta.is_dir(), meta.len()));
            }
        }
        let entries: Vec<serde_json::Value> = entries
            .into_iter()
            .map(
                |(name, (dir, size))| serde_json::json!({ "name": name, "dir": dir, "size": size }),
            )
            .collect();
        Ok(serde_json::json!({ "entries": entries, "cut": cut }))
    }

    fn stage_view(
        &self,
        folder: &str,
        patterns: &[String],
        destination: &Path,
    ) -> Result<u64, ScriptReadError> {
        let folder = parse_game_path(folder)?;
        let folder = folder.as_str().trim_matches('/');
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();

        if let Some(entries) = &self.entries {
            let folded_folder = agora_game_api::fold_path_case(folder);
            let prefix = if folded_folder.is_empty() {
                String::new()
            } else {
                format!("{folded_folder}/")
            };
            let mut exists = false;
            for (folded, entry) in entries {
                if folded == &folded_folder {
                    return Err(refusal(
                        "not_a_folder",
                        format!("`{folder}` is not a folder in this instance's view"),
                    ));
                }
                let Some(rest) = folded.strip_prefix(&prefix) else {
                    continue;
                };
                if rest.is_empty() {
                    continue;
                }
                exists = true;
                let Some(relative) = relative_below(&entry.game_path, folder) else {
                    continue;
                };
                // The chosen source is checked exactly as a read would check
                // it, so a link put in the way is refused rather than staged.
                let from_root = entry
                    .source
                    .strip_prefix(&entry.trusted_root)
                    .unwrap_or(&entry.source);
                reject_reparse_chain(
                    &entry.trusted_root,
                    &from_root.to_string_lossy().replace('\\', "/"),
                )?;
                candidates.push((relative, entry.source.clone()));
            }
            if !exists {
                return Err(refusal(
                    "not_found",
                    format!("`{folder}` does not exist in this instance's view"),
                ));
            }
        } else {
            let mut exists = false;
            for root in &self.roots {
                reject_reparse_chain(root, folder)?;
                let base = folder
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .fold(root.clone(), |path, part| path.join(part));
                match std::fs::symlink_metadata(&base) {
                    Ok(meta) if is_reparse_point(&meta) => {
                        return Err(refusal(
                            "reparse_point",
                            format!("`{folder}` passes through a link, which is not followed"),
                        ));
                    }
                    Ok(meta) if meta.is_dir() => {
                        exists = true;
                        walk_view(&base, folder, "", 0, &mut candidates)?;
                    }
                    Ok(_) => {
                        // A file in a higher layer shadows a folder below it;
                        // only the highest layer that has the name decides.
                        if !exists {
                            return Err(refusal(
                                "not_a_folder",
                                format!("`{folder}` is not a folder in this instance's view"),
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {
                        return Err(refusal(
                            "unreadable",
                            format!(
                                "{} could not be read in this instance's view",
                                view_subject(folder)
                            ),
                        ));
                    }
                }
            }
            if !exists {
                return Err(refusal(
                    "not_found",
                    format!("`{folder}` does not exist in this instance's view"),
                ));
            }
        }

        let mut staged = 0u64;
        for (relative, source) in candidates {
            // No patterns means the whole folder, which is the useful reading
            // of `{ folder: "Data" }` with nothing else said.
            if !patterns.is_empty() && !path_matches_any(patterns, &relative) {
                continue;
            }
            if relative
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            {
                continue;
            }
            let game_path = view_path(folder, &relative);
            // Check the chosen file again immediately before staging: the walk
            // and the staging pass are separate, and a link can appear between
            // them. The identity is checked again after the hardlink, so a
            // swap inside that window is caught too.
            let meta = std::fs::symlink_metadata(&source)
                .map_err(|_| refusal("unreadable", format!("`{game_path}` could not be read")))?;
            if is_reparse_point(&meta) || !meta.is_file() {
                return Err(refusal(
                    "reparse_point",
                    format!("`{game_path}` is not a regular file, so it is not staged"),
                ));
            }
            let expected = file_identity(&meta);
            // Staging hands the module the same bytes a read would return, so
            // it spends the same byte budget.
            self.budget.spend(&self.limits, meta.len())?;
            let dest = relative
                .split('/')
                .fold(destination.to_path_buf(), |path, part| path.join(part));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|_| {
                    refusal(
                        "staging_failed",
                        format!("could not create a folder for `{game_path}`"),
                    )
                })?;
            }
            stage_file(&source, &dest, expected, &game_path)?;
            staged += 1;
        }
        Ok(staged)
    }
}

/// A game path made from a folder and a path relative to it.
fn view_path(folder: &str, relative: &str) -> String {
    if folder.is_empty() {
        relative.to_string()
    } else {
        format!("{folder}/{relative}")
    }
}

/// What a script sees for a path, never a host path.
fn view_subject(path: &str) -> String {
    if path.is_empty() {
        "the instance view".to_string()
    } else {
        format!("`{path}`")
    }
}

/// The identity of a file, used to catch a swap between the walk and staging.
///
/// On Unix an inode number and its device; on Windows the creation and write
/// times plus the size, because the stable `MetadataExt` surface does not
/// expose a file index.
#[cfg(unix)]
type FileIdentity = (u64, u64, u64);
#[cfg(windows)]
type FileIdentity = (u64, u64, u64);
#[cfg(not(any(unix, windows)))]
type FileIdentity = ();

fn file_identity(meta: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino(), meta.len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (
            meta.creation_time(),
            meta.last_write_time(),
            meta.file_size(),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = meta;
    }
}

/// Link or copy one file into the staging folder, refusing a swapped source.
///
/// A hardlink shares the real file's data, so after linking the staged entry
/// must be the same ordinary file the walk saw. The copy fallback opens the
/// source and checks the opened handle's identity before copying, because a
/// copy follows a link.
fn stage_file(
    source: &Path,
    dest: &Path,
    expected: FileIdentity,
    game_path: &str,
) -> Result<(), ScriptReadError> {
    match std::fs::hard_link(source, dest) {
        Ok(()) => {
            let meta = std::fs::symlink_metadata(dest).map_err(|_| {
                refusal(
                    "staging_failed",
                    format!("`{game_path}` could not be checked after staging"),
                )
            })?;
            if is_reparse_point(&meta) || !meta.is_file() || file_identity(&meta) != expected {
                let _ = std::fs::remove_file(dest);
                return Err(refusal(
                    "staging_failed",
                    format!("`{game_path}` changed while it was staged"),
                ));
            }
            Ok(())
        }
        Err(_) => {
            let mut from = std::fs::File::open(source)
                .map_err(|_| refusal("unreadable", format!("`{game_path}` could not be read")))?;
            let opened = from
                .metadata()
                .map_err(|_| refusal("unreadable", format!("`{game_path}` could not be read")))?;
            if is_reparse_point(&opened) || !opened.is_file() || file_identity(&opened) != expected
            {
                return Err(refusal(
                    "staging_failed",
                    format!("`{game_path}` changed while it was staged"),
                ));
            }
            let mut to = std::fs::File::create(dest).map_err(|_| {
                refusal(
                    "staging_failed",
                    format!("`{game_path}` could not be staged"),
                )
            })?;
            std::io::copy(&mut from, &mut to).map_err(|_| {
                refusal(
                    "staging_failed",
                    format!("`{game_path}` could not be staged"),
                )
            })?;
            let meta = std::fs::symlink_metadata(dest).map_err(|_| {
                refusal(
                    "staging_failed",
                    format!("`{game_path}` could not be checked after staging"),
                )
            })?;
            if is_reparse_point(&meta) || !meta.is_file() {
                let _ = std::fs::remove_file(dest);
                return Err(refusal(
                    "staging_failed",
                    format!("`{game_path}` changed while it was staged"),
                ));
            }
            Ok(())
        }
    }
}

/// The part of `full` below `folder`, keeping the real spelling. `None` when
/// the path names the folder itself or sits outside it.
fn relative_below(full: &str, folder: &str) -> Option<String> {
    let folder_parts = folder.split('/').filter(|part| !part.is_empty()).count();
    let parts: Vec<&str> = full.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= folder_parts {
        return None;
    }
    Some(parts[folder_parts..].join("/"))
}

/// Collect the ordinary files below `dir`, skipping reparse points, with paths
/// relative to the staged folder.
///
/// `game_dir` is the path the script used for `dir`, so an error can name what
/// the script asked for without revealing a host path. Every `read_dir` and
/// metadata failure is an error rather than a silently smaller mount.
fn walk_view(
    dir: &Path,
    game_dir: &str,
    prefix: &str,
    depth: usize,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), ScriptReadError> {
    if depth > 64 {
        return Ok(());
    }
    let read_dir = std::fs::read_dir(dir).map_err(|_| {
        refusal(
            "unreadable",
            format!(
                "{} could not be read in this instance's view",
                view_subject(game_dir)
            ),
        )
    })?;
    for entry in read_dir {
        let entry = entry.map_err(|_| {
            refusal(
                "unreadable",
                format!(
                    "{} could not be read in this instance's view",
                    view_subject(game_dir)
                ),
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let game_path = view_path(game_dir, &name);
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|_| {
            refusal(
                "unreadable",
                format!("`{game_path}` could not be read in this instance's view"),
            )
        })?;
        if is_reparse_point(&meta) {
            continue;
        }
        if meta.is_dir() {
            walk_view(&entry.path(), &game_path, &relative, depth + 1, out)?;
        } else if meta.is_file() {
            out.push((relative, entry.path()));
        }
    }
    Ok(())
}

/// The physical root a base's files come from, when the instance has one.
fn base_root(ctx: &crate::ctx::Ctx, instance_id: &str) -> Option<PathBuf> {
    let manifest = crate::game_instance::get_manifest(ctx, instance_id).ok()?;
    let agora_game_api::BaseReference::Pinned { id, .. } = manifest.base else {
        return None;
    };
    let text = std::fs::read_to_string(ctx.paths.base_manifest_path(&id)).ok()?;
    let base: crate::game_base::BaseManifest = serde_json::from_str(&text).ok()?;
    Some(base.location)
}

/// The view roots for an instance with no plan: its own layers and, when it
/// has one, the deployed game folder.
fn fallback_roots(ctx: &crate::ctx::Ctx, instance_id: &str, instance_dir: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        instance_dir.join("overrides"),
        instance_dir.join("writable"),
    ];
    if let Ok(read_dir) = std::fs::read_dir(instance_dir.join("plugin-generated")) {
        for entry in read_dir.flatten() {
            if entry
                .file_type()
                .map(|kind| kind.is_dir() && !kind.is_symlink())
                .unwrap_or(false)
            {
                roots.push(entry.path());
            }
        }
    }
    if let Ok(Some(game)) = crate::game_deploy::deployment_dir(ctx, instance_id) {
        roots.push(game);
    }
    roots.retain(|root| root.exists());
    roots
}

/// A `game:read` scope for one instance over the same merged view deploy uses.
///
/// The plan applies base, content, generated, overrides and writable layers in
/// deploy precedence, so a script sees the same files whether or not the
/// instance has been deployed. The instance folder itself is not a view root:
/// it holds Agora's own manifest and import records, which may name a real path.
pub fn instance_reads(ctx: &crate::ctx::Ctx, instance_id: &str) -> Result<GameViewReads, String> {
    instance_reads_with_budget(ctx, instance_id, Arc::new(ReadBudget::default()))
}

/// [`instance_reads`] sharing a top-level call's budget.
pub fn instance_reads_with_budget(
    ctx: &crate::ctx::Ctx,
    instance_id: &str,
    budget: Arc<ReadBudget>,
) -> Result<GameViewReads, String> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|_| "that is not an instance Agora knows".to_string())?;
    if !instance_dir.exists() {
        return Err("that instance does not exist".to_string());
    }
    // A Minecraft instance (or a folder the player added) has no generic
    // `game_instances` record. It still has a view: overrides, the game's
    // writable layer, generated layers and a deployment, if one exists.
    let Some(record) = crate::game_instance::get(ctx, instance_id).map_err(|e| e.to_string())?
    else {
        return Ok(GameViewReads::with_budget(
            fallback_roots(ctx, instance_id, &instance_dir),
            GameReadLimits::default(),
            budget,
        )
        .with_instance(instance_id));
    };
    let Some(definition) = ctx.games.game(&record.game).cloned() else {
        return Ok(GameViewReads::with_budget(
            fallback_roots(ctx, instance_id, &instance_dir),
            GameReadLimits::default(),
            budget,
        )
        .with_instance(instance_id));
    };
    let plan = crate::game_deploy::plan_for_reads(
        ctx,
        instance_id,
        &definition,
        crate::game_deploy::DeployMode::Links,
    )
    .map_err(|error| error.to_string())?;
    let base = base_root(ctx, instance_id);
    let content_root = ctx.paths.content_objects_dir();
    let mut entries = HashMap::new();
    for file in plan.files {
        let source = crate::game_deploy::source_path(ctx, &file.source);
        let (trusted_root, rank) = match &file.source {
            crate::game_deploy::FileSource::Content { .. } => {
                if source.strip_prefix(&content_root).is_err() {
                    continue;
                }
                (content_root.clone(), 2)
            }
            crate::game_deploy::FileSource::Base { .. } => {
                let Some(root) = &base else { continue };
                if source.strip_prefix(root).is_err() {
                    continue;
                }
                (root.clone(), 1)
            }
            crate::game_deploy::FileSource::Generated { .. } => {
                if source.strip_prefix(&instance_dir).is_err() {
                    continue;
                }
                (instance_dir.clone(), 3)
            }
            crate::game_deploy::FileSource::Writable { .. }
            | crate::game_deploy::FileSource::Override { .. } => {
                if source.strip_prefix(&instance_dir).is_err() {
                    continue;
                }
                (instance_dir.clone(), 4)
            }
        };
        let game_path = file.path.as_str().to_string();
        entries.insert(
            agora_game_api::fold_path_case(&game_path),
            ViewEntry {
                source,
                trusted_root,
                game_path,
                rank,
            },
        );
    }

    // A script's own generated layer is a layer deploy does not know about
    // yet. It sits at generated rank: above content, below overrides and the
    // game's writable layer, exactly where a generated layer belongs.
    let generated = instance_dir.join("plugin-generated");
    if let Ok(read_dir) = std::fs::read_dir(&generated) {
        for layer in read_dir.flatten() {
            let Ok(kind) = layer.file_type() else {
                continue;
            };
            if !kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let root = layer.path();
            let mut stack = vec![(root.clone(), String::new())];
            while let Some((dir, prefix)) = stack.pop() {
                let Ok(read_dir) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in read_dir.flatten() {
                    let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                        continue;
                    };
                    if is_reparse_point(&meta) {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().to_string();
                    let rel = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    if meta.is_dir() {
                        stack.push((entry.path(), rel));
                        continue;
                    }
                    let folded = agora_game_api::fold_path_case(&rel);
                    if entries.get(&folded).is_some_and(|entry| entry.rank > 3) {
                        continue;
                    }
                    entries.insert(
                        folded,
                        ViewEntry {
                            source: entry.path(),
                            trusted_root: instance_dir.clone(),
                            game_path: rel.clone(),
                            rank: 3,
                        },
                    );
                }
            }
        }
    }
    Ok(GameViewReads::from_entries(
        entries,
        GameReadLimits::default(),
        budget,
        instance_id,
    ))
}

/// A `game:read` scope over one install, by game path. Used by
/// `detect_runtime`, which has an install rather than an instance.
pub fn install_reads(location: PathBuf, budget: Arc<ReadBudget>) -> Result<GameViewReads, String> {
    if !location.is_dir() {
        return Err("that game install is not a folder".to_string());
    }
    Ok(GameViewReads::with_budget(
        vec![location],
        GameReadLimits::default(),
        budget,
    ))
}

/// A `game:write` scope for one plugin's own generated layer in one instance.
pub fn instance_write_scope(
    ctx: &crate::ctx::Ctx,
    plugin_id: &agora_plugin_api::manifest::PluginId,
    instance_id: &str,
) -> Result<GameWriteScope, String> {
    instance_write_scope_with_budget(
        ctx,
        plugin_id,
        instance_id,
        Arc::new(WriteBudget::default()),
    )
}

/// [`instance_write_scope`] sharing a top-level call's budget.
pub fn instance_write_scope_with_budget(
    ctx: &crate::ctx::Ctx,
    plugin_id: &agora_plugin_api::manifest::PluginId,
    instance_id: &str,
    budget: Arc<WriteBudget>,
) -> Result<GameWriteScope, String> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|_| "that is not an instance Agora knows".to_string())?;
    if !instance_dir.exists() {
        return Err("that instance does not exist".to_string());
    }
    Ok(GameWriteScope::with_trusted_root(
        instance_dir
            .join("plugin-generated")
            .join(plugin_id.as_str()),
        instance_dir,
        GameWriteLimits::default(),
        budget,
    ))
}

/// One plugin's generated layer for one instance.
///
/// The layer root is chosen by core (`<instance>/plugin-generated/<plugin>`),
/// never by a script. A write joins a relative path below it, so it cannot
/// reach the game, a save or another layer.
pub struct GameWriteScope {
    layer_root: PathBuf,
    /// The instance root. It is trusted even when it is a junction; everything
    /// below it, including the generated layer folder, is checked.
    trusted_root: PathBuf,
    /// `layer_root` relative to `trusted_root`, with `/` separators.
    layer_rel: String,
    limits: GameWriteLimits,
    budget: Arc<WriteBudget>,
    /// Bytes already in the layer, from a scan at scope creation plus this
    /// call's writes. A repeated call cannot fill the disk one file at a time.
    layer_bytes: AtomicU64,
}

fn directory_size(root: &Path, stop_at: u64) -> u64 {
    let mut total = 0u64;
    let Ok(read_dir) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in read_dir.flatten() {
        if total >= stop_at {
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if is_reparse_point(&meta) {
            continue;
        }
        if meta.is_dir() {
            total = total.saturating_add(directory_size(&entry.path(), stop_at - total));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

impl GameWriteScope {
    pub fn new(layer_root: PathBuf, limits: GameWriteLimits) -> Self {
        let trusted_root = layer_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| layer_root.clone());
        Self::with_trusted_root(
            layer_root,
            trusted_root,
            limits,
            Arc::new(WriteBudget::default()),
        )
    }

    pub fn with_trusted_root(
        layer_root: PathBuf,
        trusted_root: PathBuf,
        limits: GameWriteLimits,
        budget: Arc<WriteBudget>,
    ) -> Self {
        let layer_rel = layer_root
            .strip_prefix(&trusted_root)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let layer_bytes = directory_size(&layer_root, limits.max_layer_bytes);
        Self {
            layer_root,
            trusted_root,
            layer_rel,
            limits,
            budget,
            layer_bytes: AtomicU64::new(layer_bytes),
        }
    }

    pub fn layer_root(&self) -> &Path {
        &self.layer_root
    }

    /// Write one file atomically (a temporary file in the same directory, then
    /// a rename) after checking every component and both budgets.
    pub fn write(&self, path: &str, bytes: &[u8]) -> Result<(), ScriptReadError> {
        let rel = check_write_path(path)?;
        if bytes.len() as u64 > self.limits.max_write_bytes {
            return Err(refusal(
                "too_large",
                format!(
                    "that write is {} bytes; one write may add {}",
                    bytes.len(),
                    self.limits.max_write_bytes
                ),
            ));
        }

        // Check the whole chain *before* creating anything. A junction at the
        // layer folder or anywhere below it is refused, and a refused write
        // never leaves a folder outside the layer behind.
        let full_rel = if self.layer_rel.is_empty() {
            rel.as_str().to_string()
        } else {
            format!("{}/{}", self.layer_rel, rel.as_str())
        };
        reject_reparse_chain(&self.trusted_root, &full_rel)?;

        let target = rel
            .as_str()
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(self.layer_root.clone(), |path, part| path.join(part));
        let Some(parent) = target.parent() else {
            return Err(refusal("invalid_path", "that write has no parent folder"));
        };
        let existing = std::fs::metadata(&target)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let projected = self
            .layer_bytes
            .load(Ordering::SeqCst)
            .saturating_add(bytes.len() as u64)
            .saturating_sub(existing);
        if projected > self.limits.max_layer_bytes {
            return Err(refusal(
                "layer_full",
                format!(
                    "this plugin's generated layer for the instance is full ({} bytes); a write \
                     that would make it {projected} bytes was refused",
                    self.limits.max_layer_bytes
                ),
            ));
        }

        // Budgets are claimed before doing any work, so two concurrent writes
        // cannot both slip under a total that only one of them fits.
        self.budget.spend(&self.limits, bytes.len() as u64)?;
        if target.exists() {
            // Replacing a file does not create another one.
        } else {
            self.budget.claim_file(&self.limits)?;
        }

        std::fs::create_dir_all(parent)
            .map_err(|_| refusal("unwritable", "the generated layer could not be created"))?;

        let unique = format!(
            ".agora-write-{}-{}",
            std::process::id(),
            self.layer_bytes.load(Ordering::SeqCst)
        );
        let temp = parent.join(unique);
        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temp);
            return Err(refusal(
                "unwritable",
                format!("that write could not be completed: {error}"),
            ));
        }
        // A rename over an existing file is atomic on Unix. On Windows it
        // fails when the target exists, so the old file is removed first;
        // the bytes are already safe in the temporary file.
        #[cfg(windows)]
        if target.exists() {
            std::fs::remove_file(&target)
                .map_err(|_| refusal("unwritable", "the old file could not be replaced"))?;
        }
        if std::fs::rename(&temp, &target).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(refusal("unwritable", "that write could not be completed"));
        }
        self.layer_bytes
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        self.layer_bytes.fetch_sub(existing, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Launch recipes from a script
// ---------------------------------------------------------------------------

/// Environment variables that make a loader import code, or redirect where it
/// looks for code. They are refused in a recipe a script returns.
///
/// Sources: glibc's `ld.so(8)` documents `LD_*`; Apple's `dyld(1)` documents
/// `DYLD_*`; the .NET host documentation documents `DOTNET_*`, `COR_*`,
/// `CORECLR_*` and profiler variables; Oracle's Java launcher documentation
/// documents `JAVA_TOOL_OPTIONS` and `_JAVA_OPTIONS`; Node documents
/// `NODE_OPTIONS`; the Python documentation documents `PYTHON*`; Vulkan and Qt
/// document `VK_*` and `QT_*`; Wine documents `WINEDLLOVERRIDES`. Microsoft's
/// "Dynamic-Link Library Search Order" documents `PATH`. Windows facts checked
/// against those pages. This is defence in depth, not a boundary: a game can
/// still read a path it is told about through a relative name.
pub const BLOCKED_LOADER_ENVIRONMENT: &[&str] = &[
    "LD_PRELOAD",
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "CLASSPATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5LIB",
    "RUBYLIB",
    "PATH",
];

/// Patterns beside the names above: a new loader variable in the same family
/// is refused without this list having to name it first.
pub const BLOCKED_LOADER_ENVIRONMENT_PATTERNS: &[&str] = &[
    "LD_*",
    "DYLD_*",
    "*_PRELOAD",
    "*_PLUGIN_PATH",
    "DOTNET_*",
    "COR_*",
    "CORECLR_*",
    "*PROFILER*",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "NODE_OPTIONS",
    "PYTHON*",
    "VK_*",
    "QT_*",
    "WINEDLLOVERRIDES",
];

fn wildcard_match(pattern: &str, text: &str) -> bool {
    // Iterative `*` matching: a star remembers the last place it could have
    // consumed more, so the common case stays linear.
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn blocked_environment_key(key: &str) -> Option<String> {
    let upper = key.to_ascii_uppercase();
    if BLOCKED_LOADER_ENVIRONMENT
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(key))
    {
        return Some(key.to_string());
    }
    BLOCKED_LOADER_ENVIRONMENT_PATTERNS
        .iter()
        .find(|pattern| wildcard_match(&pattern.to_ascii_uppercase(), &upper))
        .map(|pattern| pattern.to_string())
}

/// The physical roots a launch recipe may name.
///
/// Mirrors [`crate::game_launch::LaunchRoots`] without making this module
/// depend on the launch implementation. The caller supplies the roots it will
/// actually run with.
pub struct RecipeRoots<'a> {
    pub runtime: &'a Path,
    pub install: Option<&'a Path>,
    pub base: Option<&'a Path>,
}

impl RecipeRoots<'_> {
    fn root_for(&self, path: &GamePath) -> Option<&Path> {
        match path {
            GamePath::Runtime { .. } => Some(self.runtime),
            GamePath::Install { .. } => self.install,
            GamePath::Base { .. } => self.base,
            _ => None,
        }
    }

    fn relative(path: &GamePath) -> Option<&RelPath> {
        match path {
            GamePath::Runtime { path }
            | GamePath::Install { path, .. }
            | GamePath::Base { path, .. } => Some(path),
            _ => None,
        }
    }
}

fn check_recipe_path(path: &GamePath, roots: &RecipeRoots<'_>, what: &str) -> Result<(), String> {
    let Some(root) = roots.root_for(path) else {
        return Err(format!(
            "{what} must be inside the game's runtime, base or install folder"
        ));
    };
    let rel = RecipeRoots::relative(path)
        .expect("the root match above covers every path kind with a RelPath");
    reject_reparse_chain(root, rel.as_str())
        .map_err(|_| format!("{what} passes through a link, which is not followed"))?;
    let physical = rel
        .as_str()
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(root.to_path_buf(), |path, part| path.join(part));
    let exists = physical.exists();
    if what == "the executable" && !exists {
        return Err("the executable is not a file in this instance's view".to_string());
    }
    if what == "the working folder" && !physical.is_dir() {
        return Err("the working folder is not a folder in this instance's view".to_string());
    }
    Ok(())
}

/// Whether a literal names an absolute, UNC or parent-relative path.
///
/// This is defence in depth, not a boundary: a game can read anything it is
/// told to through a relative name once it runs. It stops a recipe from handing
/// a loader an obvious absolute path through an argument it does not inspect.
fn literal_names_outside(value: &str) -> bool {
    // A leading `/` is usually a POSIX path, but `cmd.exe` takes switches such
    // as `/c`; those are flags, not paths.
    let is_cmd_switch = |candidate: &str| {
        matches!(
            candidate.to_ascii_lowercase().as_str(),
            "/c" | "/s"
                | "/k"
                | "/d"
                | "/q"
                | "/a"
                | "/u"
                | "/v"
                | "/x"
                | "/y"
                | "/b"
                | "/i"
                | "/wait"
        )
    };
    let looks_absolute = |candidate: &str| {
        let candidate = candidate.trim();
        (candidate.starts_with('/') && !is_cmd_switch(candidate))
            || candidate.starts_with('\\')
            || (candidate.len() >= 2
                && candidate.as_bytes()[0].is_ascii_alphabetic()
                && candidate.as_bytes()[1] == b':')
            || candidate.split(['/', '\\']).any(|part| part == "..")
    };
    if looks_absolute(value) {
        return true;
    }
    // Flags often carry a path after `=` or `:` (`-javaagent:C:/x.jar`).
    value
        .split_once(['=', ':'])
        .is_some_and(|(_, rest)| looks_absolute(rest))
}

fn check_recipe_value(value: &LaunchValue, roots: &RecipeRoots<'_>) -> Result<(), String> {
    match value {
        LaunchValue::Literal { value } => {
            if literal_names_outside(value) {
                return Err(format!(
                    "the argument `{value}` names a path outside the instance's view"
                ));
            }
            Ok(())
        }
        LaunchValue::Conditional { value, .. } => check_recipe_value(value, roots),
        LaunchValue::Path { path, .. } => check_recipe_path(path, roots, "an argument path"),
        LaunchValue::PathList { paths, .. } => {
            for path in paths {
                check_recipe_path(path, roots, "an argument path")?;
            }
            Ok(())
        }
    }
}

/// Check a recipe a `prepare_launch` hook returned, before core runs it.
///
/// Everything it names must resolve inside the instance's view. The executable
/// is refused if it is outside the allowed roots, missing, or reached through a
/// reparse point. A loader environment key is refused and the player is told
/// which one and why.
///
/// `.bat` and `.cmd` executables are allowed: a batch file in the view carries
/// no more risk than an executable there, and core spawns processes without a
/// shell, so Rust escapes each argument instead of letting a batch file's shell
/// split or interpret it.
pub fn validate_launch_recipe(
    recipe: &LaunchRecipe,
    roots: &RecipeRoots<'_>,
) -> Result<(), String> {
    validate_launch_recipe_with(recipe, roots, true)
}

/// [`validate_launch_recipe`] with the argument check optional.
///
/// A user's own tool arguments, tool input values and `{item}` are the player's choices and are
/// never checked. Their recipe still gets the executable, working-folder and environment checks.
pub fn validate_launch_recipe_with(
    recipe: &LaunchRecipe,
    roots: &RecipeRoots<'_>,
    check_arguments: bool,
) -> Result<(), String> {
    check_recipe_path(&recipe.executable, roots, "the executable")?;
    check_recipe_path(&recipe.working_directory, roots, "the working folder")?;
    if check_arguments {
        for argument in &recipe.arguments {
            check_recipe_value(argument, roots)?;
        }
    }
    for (key, value) in &recipe.environment {
        if let Some(blocked) = blocked_environment_key(key) {
            return Err(format!(
                "the recipe sets `{key}` (matches `{blocked}`), which can load or redirect code; a \
                 plugin's launch recipe may not set it"
            ));
        }
        check_recipe_value(value, roots)?;
    }
    Ok(())
}
/// A user-data root a script may not redirect a recipe to.
///
/// Kept explicit so the list of roots a script can name stays visibly small.
pub fn user_data_root(location: &UserDataLocation) -> Option<PathBuf> {
    crate::game_user_files::user_data_root(location)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agora_plugin_api::host::GameReads;

    #[test]
    fn each_event_handler_call_gets_a_fresh_budget_session() {
        let budgets = GameBudgets::default();
        budgets.start_handler("acme.events");
        let first = budgets.session("acme.events").reads;
        budgets.end_handler("acme.events");

        budgets.start_handler("acme.events");
        let second = budgets.session("acme.events").reads;
        budgets.end_handler("acme.events");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "a second handler call must not spend the first one's budget"
        );

        // Nested calls for one handler share, and the session closes with the last.
        budgets.start_handler("acme.events");
        budgets.start_handler("acme.events");
        let shared = budgets.session("acme.events").reads;
        budgets.end_handler("acme.events");
        assert!(Arc::ptr_eq(&shared, &budgets.session("acme.events").reads));
        budgets.end_handler("acme.events");
        assert!(
            !Arc::ptr_eq(&shared, &budgets.session("acme.events").reads),
            "the handler's session must close when it ends"
        );
    }

    fn unique_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agora-script-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn reader(root: &Path) -> GameViewReads {
        GameViewReads::new(
            vec![root.to_path_buf()],
            GameReadLimits {
                max_total_read_bytes: 64,
                ..GameReadLimits::default()
            },
        )
    }

    #[test]
    fn a_read_with_dotdot_is_refused() {
        let root = unique_root("dotdot");
        write(&root.to_path_buf().join("a.txt"), b"a");
        let reads = reader(&root);
        let error = reads.read_bytes("../a.txt", 0, 10).unwrap_err();
        assert_eq!(error.code, "outside_view");
        let error = reads.list("a/../../").unwrap_err();
        assert_eq!(error.code, "outside_view");
    }

    #[test]
    fn an_absolute_read_is_refused() {
        let root = unique_root("absolute");
        let reads = reader(&root);
        for path in ["C:/Windows/win.ini", "/etc/passwd", "\\\\server\\share\\x"] {
            let error = reads.read_bytes(path, 0, 10).unwrap_err();
            assert_eq!(error.code, "absolute_path", "{path}");
        }
    }

    #[cfg(windows)]
    fn make_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "could not create a junction for the test");
    }

    #[test]
    #[cfg(windows)]
    fn a_junction_inside_the_view_is_not_followed() {
        let root = unique_root("junction-leaf");
        let outside = unique_root("junction-leaf-outside");
        write(&outside.join("marker.txt"), b"outside");
        let link = root.join("link");
        make_junction(&link, &outside);

        let reads = reader(&root);
        let error = reads.read_bytes("link/marker.txt", 0, 16).unwrap_err();
        assert_eq!(error.code, "reparse_point");
    }

    #[test]
    #[cfg(windows)]
    fn a_junction_mid_path_is_not_followed() {
        let root = unique_root("junction-mid");
        let outside = unique_root("junction-mid-outside");
        write(&outside.join("inner/marker.txt"), b"outside");
        make_junction(&root.join("link"), &outside);

        let reads = reader(&root);
        let error = reads
            .read_bytes("link/inner/marker.txt", 0, 16)
            .unwrap_err();
        assert_eq!(error.code, "reparse_point");
    }

    #[test]
    fn no_reply_contains_a_real_path() {
        let root = unique_root("nopath");
        write(&root.join("logs/latest.txt"), b"line one\nline two\n");
        let reads = GameViewReads::new(vec![root.clone()], GameReadLimits::default());

        let lines = reads.read_lines("logs/latest.txt", 0, 10).unwrap();
        let listed = reads.list("logs").unwrap();
        let not_found = reads.read_bytes("missing.txt", 0, 1).unwrap_err();
        let real = root.to_string_lossy().to_string();
        for text in [
            lines.to_string(),
            listed.to_string(),
            not_found.message.clone(),
        ] {
            assert!(
                !text.contains(&real),
                "a reply leaked the real root: {text}"
            );
        }
    }

    #[test]
    fn reads_past_the_budget_report_it() {
        let root = unique_root("budget");
        write(&root.join("big.txt"), &[b'x'; 100]);
        let reads = reader(&root);
        reads.read_bytes("big.txt", 0, 40).unwrap();
        let error = reads.read_bytes("big.txt", 40, 40).unwrap_err();
        assert_eq!(error.code, "budget_spent");
    }

    fn launch_recipe(
        executable: serde_json::Value,
        environment: serde_json::Value,
    ) -> LaunchRecipe {
        serde_json::from_value(serde_json::json!({
            "executable": executable,
            "arguments": [],
            "environment": environment,
            "working_directory": { "root": "runtime", "path": "" }
        }))
        .unwrap()
    }

    fn roots(root: &Path) -> RecipeRoots<'_> {
        RecipeRoots {
            runtime: root,
            install: None,
            base: None,
        }
    }

    #[test]
    fn a_recipe_outside_the_view_is_refused() {
        let root = unique_root("recipe-outside");
        let recipe = launch_recipe(
            serde_json::json!({ "root": "user_data", "location": "home", "path": "tool.exe" }),
            serde_json::json!({}),
        );
        let error = validate_launch_recipe(&recipe, &roots(&root)).unwrap_err();
        assert!(error.contains("must be inside"), "{error}");
    }

    #[test]
    fn a_recipe_with_dotdot_or_an_absolute_exe_is_refused() {
        for path in ["../evil.exe", "C:/Windows/evil.exe", "/usr/bin/evil"] {
            let parsed: Result<LaunchRecipe, _> = serde_json::from_value(serde_json::json!({
                "executable": { "root": "runtime", "path": path },
                "arguments": [],
                "environment": {},
                "working_directory": { "root": "runtime", "path": "" }
            }));
            assert!(parsed.is_err(), "{path} must not become a recipe");
        }
    }

    #[test]
    #[cfg(windows)]
    fn a_recipe_cannot_escape_through_a_junction() {
        let root = unique_root("recipe-junction");
        let outside = unique_root("recipe-junction-outside");
        write(&outside.join("tool.exe"), b"not really an executable");
        make_junction(&root.join("link"), &outside);
        let recipe = launch_recipe(
            serde_json::json!({ "root": "runtime", "path": "link/tool.exe" }),
            serde_json::json!({}),
        );
        let error = validate_launch_recipe(&recipe, &roots(&root)).unwrap_err();
        assert!(error.contains("link"), "{error}");
    }

    #[test]
    fn a_recipe_setting_a_loader_environment_key_is_refused() {
        let root = unique_root("recipe-env");
        write(&root.join("tool.exe"), b"x");
        for key in ["LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "PATH"] {
            let recipe = launch_recipe(
                serde_json::json!({ "root": "runtime", "path": "tool.exe" }),
                serde_json::json!({ key: "/tmp/evil.so" }),
            );
            let error = validate_launch_recipe(&recipe, &roots(&root)).unwrap_err();
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn another_instances_files_are_not_readable_without_the_capability() {
        let ctx_root = unique_root("other-instance");
        let ctx = crate::ctx::CoreContext::for_testing(ctx_root.clone());
        for (id, file) in [("one", "one.txt"), ("two", "two.txt")] {
            let dir = ctx.paths.instance_dir(id).unwrap().join("writable");
            std::fs::create_dir_all(&dir).unwrap();
            write(&dir.join(file), b"private");
        }
        let reads = instance_reads(&ctx, "two").unwrap();
        assert!(reads.read_bytes("two.txt", 0, 10).is_ok());
        let error = reads.read_bytes("one.txt", 0, 10).unwrap_err();
        assert_eq!(error.code, "not_found");
    }

    #[test]
    fn a_write_lands_only_in_the_plugins_own_layer() {
        let root = unique_root("write-own");
        let layer = root.join("plugin-generated/acme.plugin");
        let game = root.join("game");
        write(&game.join("keep.txt"), b"game");
        let scope = GameWriteScope::new(layer.clone(), GameWriteLimits::default());
        scope.write("mods/thing.txt", b"mine").unwrap();
        assert_eq!(
            std::fs::read(layer.join("mods/thing.txt")).unwrap(),
            b"mine"
        );
        assert_eq!(std::fs::read(game.join("keep.txt")).unwrap(), b"game");
    }

    #[test]
    fn a_write_cannot_escape_or_overwrite_another_layer() {
        let root = unique_root("write-escape");
        let layer = root.join("plugin-generated/acme.plugin");
        let scope = GameWriteScope::new(layer.clone(), GameWriteLimits::default());
        for path in [
            "../other.txt",
            "mods/../../other.txt",
            "C:/other.txt",
            "/other.txt",
        ] {
            assert!(scope.write(path, b"x").is_err(), "{path}");
        }
        assert!(!root.join("other.txt").exists());
    }

    #[test]
    fn a_write_over_a_device_name_is_refused() {
        let root = unique_root("write-device");
        let scope = GameWriteScope::new(root, GameWriteLimits::default());
        for path in ["CON", "mods/NUL.txt", "com1", "LPT9.log"] {
            let error = scope.write(path, b"x").unwrap_err();
            assert_eq!(error.code, "device_name", "{path}");
        }
    }

    #[test]
    fn a_write_past_the_budget_stops_and_reports() {
        let root = unique_root("write-budget");
        let scope = GameWriteScope::new(
            root,
            GameWriteLimits {
                max_write_bytes: 8,
                max_total_write_bytes: 10,
                max_files: 8,
                ..GameWriteLimits::default()
            },
        );
        scope.write("a.txt", b"12345678").unwrap();
        let error = scope.write("b.txt", b"123").unwrap_err();
        assert_eq!(error.code, "budget_spent");
        assert!(!scope.layer_root().join("b.txt").exists());
    }

    #[test]
    #[cfg(windows)]
    fn a_generated_layer_that_is_a_junction_is_refused() {
        let root = unique_root("layer-junction");
        let outside = unique_root("layer-junction-outside");
        let generated = root.join("plugin-generated");
        std::fs::create_dir_all(&generated).unwrap();
        make_junction(&generated.join("acme.plugin"), &outside);

        let scope = GameWriteScope::new(generated.join("acme.plugin"), GameWriteLimits::default());
        let error = scope.write("mods/thing.txt", b"mine").unwrap_err();
        assert_eq!(error.code, "reparse_point");
        assert!(!outside.join("mods/thing.txt").exists());
    }

    #[test]
    #[cfg(windows)]
    fn a_failed_write_through_a_junction_creates_no_folders_outside_the_layer() {
        let root = unique_root("write-junction-mid");
        let outside = unique_root("write-junction-mid-outside");
        let layer = root.join("plugin-generated").join("acme.plugin");
        std::fs::create_dir_all(&layer).unwrap();
        make_junction(&layer.join("link"), &outside);

        let scope = GameWriteScope::new(layer, GameWriteLimits::default());
        let error = scope.write("link/newdir/thing.txt", b"mine").unwrap_err();
        assert_eq!(error.code, "reparse_point");
        assert!(
            !outside.join("newdir").exists(),
            "a refused write must not create folders outside the layer"
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_redirected_game_root_works() {
        // A person may redirect the game/instance folder through a junction.
        // The root core was given is trusted; only components below it are
        // checked.
        let base = unique_root("root-redirected");
        let real = unique_root("root-redirected-real");
        write(&real.join("marker.txt"), b"redirected");
        make_junction(&base.join("game"), &real);

        let reads = GameViewReads::new(vec![base.join("game")], GameReadLimits::default());
        assert_eq!(
            reads.read_bytes("marker.txt", 0, 16).unwrap(),
            b"redirected"
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_junction_below_a_redirected_root_is_not_followed() {
        let base = unique_root("root-redirected-link");
        let real = unique_root("root-redirected-link-real");
        let outside = unique_root("root-redirected-link-outside");
        write(&outside.join("marker.txt"), b"outside");
        make_junction(&real.join("link"), &outside);
        make_junction(&base.join("game"), &real);

        let reads = GameViewReads::new(vec![base.join("game")], GameReadLimits::default());
        let error = reads.read_bytes("link/marker.txt", 0, 16).unwrap_err();
        assert_eq!(error.code, "reparse_point");
    }

    #[test]
    fn a_literal_argument_cannot_name_a_path_outside_the_view() {
        let root = unique_root("recipe-literal-arg");
        write(&root.join("tool.exe"), b"x");
        // A plain-string argument is a literal. Core never checks literals, so
        // this one can hand a runtime any file on the machine to load.
        let recipe: LaunchRecipe = serde_json::from_value(serde_json::json!({
            "executable": { "root": "runtime", "path": "tool.exe" },
            "arguments": ["-javaagent:C:/Users/someone/agent.jar"],
            "environment": {},
            "working_directory": { "root": "runtime", "path": "" }
        }))
        .unwrap();
        let error = validate_launch_recipe(&recipe, &roots(&root)).unwrap_err();
        assert!(error.contains("javaagent"), "{error}");
    }

    #[test]
    fn a_recipe_setting_a_code_loading_environment_key_is_refused() {
        let root = unique_root("recipe-env-code");
        write(&root.join("tool.exe"), b"x");
        for key in [
            "DOTNET_STARTUP_HOOKS",
            "COR_PROFILER",
            "COR_PROFILER_PATH",
            "NODE_OPTIONS",
            "VK_ICD_FILENAMES",
            "QT_PLUGIN_PATH",
        ] {
            let recipe = launch_recipe(
                serde_json::json!({ "root": "runtime", "path": "tool.exe" }),
                serde_json::json!({ key: "C:/Users/someone/evil.dll" }),
            );
            let error = validate_launch_recipe(&recipe, &roots(&root)).unwrap_err();
            assert!(error.contains(key), "{key}: {error}");
        }
    }
    #[test]
    fn install_reads_reads_a_version_file_by_game_path() {
        let root = unique_root("install-reads");
        write(&root.join("version.txt"), b"1.2.3");
        let reads = install_reads(root, Arc::new(ReadBudget::default())).unwrap();
        assert_eq!(reads.read_bytes("version.txt", 0, 5).unwrap(), b"1.2.3");
    }

    #[test]
    fn a_plugin_cannot_write_past_its_generated_layer_size() {
        let root = unique_root("layer-size-cap");
        let limits = GameWriteLimits {
            max_layer_bytes: 4,
            ..GameWriteLimits::default()
        };
        GameWriteScope::new(root.clone(), limits)
            .write("a.txt", b"1234")
            .unwrap();
        // A later call sees the bytes already there, so it cannot fill the
        // disk one file at a time.
        let error = GameWriteScope::new(root, limits)
            .write("b.txt", b"5")
            .unwrap_err();
        assert_eq!(error.code, "layer_full");
    }

    #[test]
    fn a_batch_recipe_is_allowed_and_an_argument_stays_one_argument() {
        let root = unique_root("batch-recipe");
        write(&root.join("tool.bat"), b"@echo off\n");
        let mut recipe = launch_recipe(
            serde_json::json!({ "root": "runtime", "path": "tool.bat" }),
            serde_json::json!({}),
        );
        recipe.arguments = vec![LaunchValue::Literal {
            value: "& calc.exe".into(),
        }];
        validate_launch_recipe(&recipe, &roots(&root)).unwrap();
        let launch_roots = crate::game_launch::LaunchRoots {
            runtime: root,
            install: None,
            base: None,
        };
        let resolved = crate::game_launch::resolve_recipe(&recipe, &launch_roots).unwrap();
        assert_eq!(resolved.args.len(), 1, "the argument was split by a shell");
        assert_eq!(resolved.args[0], std::ffi::OsString::from("& calc.exe"));
    }

    #[test]
    fn a_listing_that_is_cut_says_so() {
        let root = unique_root("list-cut");
        for index in 0..5 {
            write(&root.join(format!("f{index}.txt")), b"x");
        }
        let reads = GameViewReads::new(
            vec![root],
            GameReadLimits {
                max_list_entries: 2,
                ..GameReadLimits::default()
            },
        );
        let reply = reads.list("").unwrap();
        assert_eq!(reply["cut"], true);
        assert_eq!(reply["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn reading_a_deep_page_charges_the_lines_it_skipped() {
        let root = unique_root("deep-page");
        let mut text = String::new();
        for index in 0..100 {
            text.push_str(&format!("line {index:03}\n"));
        }
        write(&root.join("log.txt"), text.as_bytes());
        let reads = GameViewReads::new(
            vec![root],
            GameReadLimits {
                max_total_read_bytes: 50,
                ..GameReadLimits::default()
            },
        );
        let error = reads.read_lines("log.txt", 90, 1).unwrap_err();
        assert_eq!(error.code, "budget_spent");
    }

    #[test]
    fn device_names_and_fullwidth_separators_are_refused_on_read_and_write() {
        let root = unique_root("names");
        let reads = GameViewReads::new(vec![root.clone()], GameReadLimits::default());
        let scope = GameWriteScope::new(root, GameWriteLimits::default());

        for path in ["CON", "CONIN$", "COM1"] {
            assert_eq!(
                reads.read_bytes(path, 0, 1).unwrap_err().code,
                "device_name",
                "read {path}"
            );
            assert_eq!(
                scope.write(path, b"x").unwrap_err().code,
                "device_name",
                "write {path}"
            );
        }
        for path in ["a\u{FF0F}b", "a\u{FF0E}\u{FF0E}b", "a\u{FF3C}b"] {
            assert_eq!(
                reads.read_bytes(path, 0, 1).unwrap_err().code,
                "invalid_path",
                "read {path}"
            );
            assert_eq!(
                scope.write(path, b"x").unwrap_err().code,
                "invalid_path",
                "write {path}"
            );
        }
    }

    #[test]
    fn a_view_mount_stages_only_the_matching_files() {
        let root = unique_root("stage-view");
        write(&root.join("Data/a.esp"), b"a");
        write(&root.join("Data/b.esm"), b"b");
        write(&root.join("Data/nested/c.esp"), b"c");
        let reads = reader(&root);
        let destination = unique_root("stage-dest");

        let staged = reads
            .stage_view("Data", &["*.esp".to_string()], &destination)
            .expect("the view stages");

        assert_eq!(staged, 2);
        assert!(destination.join("a.esp").exists());
        assert!(destination.join("nested/c.esp").exists());
        assert!(!destination.join("b.esm").exists());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&destination).ok();
    }

    #[test]
    fn a_missing_view_folder_is_not_an_empty_mount() {
        let root = unique_root("stage-missing");
        let reads = reader(&root);
        let destination = unique_root("stage-missing-dest");

        // `walk_view` treats an unreadable or absent folder as "no files" and
        // returns success, and the wasm host turns that into a mount with
        // nothing in it. A script that asked for a view is then run against an
        // empty one instead of being told the source is gone.
        let missing = reads.stage_view("Data", &[], &destination);
        let not_a_folder = {
            write(&root.join("File"), b"x");
            reads.stage_view("File", &[], &destination)
        };

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&destination).ok();
        assert!(
            missing.is_err(),
            "a missing view folder staged an empty mount: {missing:?}"
        );
        assert!(
            not_a_folder.is_err(),
            "a file named as a view folder staged an empty mount: {not_a_folder:?}"
        );
    }

    #[test]
    fn a_view_folder_that_matches_nothing_is_still_a_mount() {
        let root = unique_root("stage-no-match");
        write(&root.join("Data/a.esp"), b"a");
        let reads = reader(&root);
        let destination = unique_root("stage-no-match-dest");

        let staged = reads
            .stage_view("Data", &["*.esm".to_string()], &destination)
            .expect("an existing folder is a mount even when no file matches");

        assert_eq!(staged, 0);
        // How the module tells: the guest mount exists and lists as empty, so
        // "nothing matched" is a mount with no entries, not a failed run.
        assert!(destination.is_dir());
        assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&destination).ok();
    }

    #[test]
    fn staging_spends_the_read_budget() {
        let root = unique_root("stage-budget");
        write(&root.join("Data/big.esp"), &[0u8; 100]);
        let reads = reader(&root);
        let destination = unique_root("stage-budget-dest");

        let error = reads
            .stage_view("Data", &[], &destination)
            .expect_err("staging past the read budget is refused");

        assert_eq!(error.code, "budget_spent", "{}", error.message);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&destination).ok();
    }

    #[test]
    fn a_file_swapped_after_the_walk_is_not_staged() {
        let root = unique_root("stage-swap");
        let original = root.join("original.esp");
        let swapped = root.join("swapped.esp");
        write(&original, b"a");
        write(&swapped, b"bbbb");
        let destination = unique_root("stage-swap-dest").join("staged.esp");
        let expected = file_identity(&std::fs::symlink_metadata(&original).unwrap());

        // The walk saw `original`, but staging is handed `swapped`: the
        // identity no longer matches, so nothing is left in the staging folder.
        let error = stage_file(&swapped, &destination, expected, "Data/original.esp")
            .expect_err("a swapped file is not staged");

        assert_eq!(error.code, "staging_failed", "{}", error.message);
        assert!(!destination.exists(), "the swapped file was removed");
        std::fs::remove_dir_all(&root).ok();
        if let Some(parent) = destination.parent() {
            std::fs::remove_dir_all(parent).ok();
        }
    }
}
