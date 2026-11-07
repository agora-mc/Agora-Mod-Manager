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
use agora_plugin_api::host::{GameReads, ScriptReadError};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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

/// Bytes a single write may add.
pub const DEFAULT_MAX_WRITE_BYTES: u64 = 4 * 1024 * 1024;
/// Bytes one hook or one plugin call may write in total.
pub const DEFAULT_MAX_TOTAL_WRITE_BYTES: u64 = 16 * 1024 * 1024;
/// Files one hook or one plugin call may create in its generated layer.
pub const DEFAULT_MAX_WRITE_FILES: u32 = 256;

/// The budgets one read scope enforces.
#[derive(Debug, Clone, Copy)]
pub struct GameReadLimits {
    pub max_read_bytes: u64,
    pub max_total_read_bytes: u64,
    pub max_lines: u32,
    pub max_line_len: u32,
}

impl Default for GameReadLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_total_read_bytes: DEFAULT_MAX_TOTAL_READ_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_line_len: DEFAULT_MAX_LINE_LEN,
        }
    }
}

/// The budgets one write scope enforces.
#[derive(Debug, Clone, Copy)]
pub struct GameWriteLimits {
    pub max_write_bytes: u64,
    pub max_total_write_bytes: u64,
    pub max_files: u32,
}

impl Default for GameWriteLimits {
    fn default() -> Self {
        Self {
            max_write_bytes: DEFAULT_MAX_WRITE_BYTES,
            max_total_write_bytes: DEFAULT_MAX_TOTAL_WRITE_BYTES,
            max_files: DEFAULT_MAX_WRITE_FILES,
        }
    }
}

fn refusal(code: &str, message: impl Into<String>) -> ScriptReadError {
    ScriptReadError::new(code, message)
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
    RelPath::new(path).map_err(|_| refusal("outside_view", "that is not a game-relative path"))
}

/// A write may not name a Windows device. Reuses the store's list, which
/// already covers `COM0`/`LPT0`, the superscript forms and trailing spaces
/// (Microsoft, "Naming Files, Paths, and Namespaces").
fn is_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part);
    crate::content_store::is_windows_device_name(stem)
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

/// An instance's merged view, highest precedence root first.
///
/// The roots are physical directories core chose; the script only ever names a
/// game path. No reply, error or log line contains one of them.
pub struct GameViewReads {
    roots: Vec<PathBuf>,
    limits: GameReadLimits,
    total_read: AtomicU64,
}

impl GameViewReads {
    pub fn new(roots: Vec<PathBuf>, limits: GameReadLimits) -> Self {
        Self {
            roots,
            limits,
            total_read: AtomicU64::new(0),
        }
    }

    /// The physical file or directory `rel` names, if any layer has it.
    ///
    /// A reparse point in the *chosen* root is an error, not a fall-through:
    /// quietly reading a lower layer would hide the fact that a link was put
    /// in the way.
    fn physical(&self, rel: &str) -> Result<Option<PathBuf>, ScriptReadError> {
        for root in &self.roots {
            reject_reparse_chain(root, rel)?;
            let candidate = rel
                .split('/')
                .fold(root.clone(), |path, part| path.join(part));
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => return Ok(Some(candidate)),
                Err(_) => continue,
            }
        }
        Ok(None)
    }

    fn spend(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total_read.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(refusal(
                    "budget_spent",
                    format!(
                        "this call has read its whole {} byte budget",
                        self.limits.max_total_read_bytes
                    ),
                ));
            }
            match self.total_read.compare_exchange(
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

impl GameReads for GameViewReads {
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
        self.spend(data.len() as u64)?;
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
        let size = std::fs::metadata(&physical)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if size > self.limits.max_total_read_bytes {
            return Err(refusal(
                "too_large",
                format!(
                    "that file is {size} bytes; one call may read {}",
                    self.limits.max_total_read_bytes
                ),
            ));
        }
        let data = std::fs::read(&physical)
            .map_err(|_| refusal("unreadable", "that path could not be read"))?;
        let max_lines = max_lines.min(self.limits.max_lines);
        let max_len = self.limits.max_line_len as usize;
        let text = String::from_utf8_lossy(&data);

        let raw: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            let mut parts: Vec<&str> = text.split('\n').collect();
            if parts.last() == Some(&"") {
                parts.pop();
            }
            parts
        };

        let mut lines = Vec::new();
        let mut cut = false;
        let mut index = 0u64;
        let mut more = false;
        for line in raw {
            if index < from_line {
                index += 1;
                continue;
            }
            if lines.len() >= max_lines as usize {
                more = true;
                break;
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (line, was_cut) = match line.char_indices().nth(max_len) {
                Some((byte, _)) => (&line[..byte], true),
                None => (line, false),
            };
            cut |= was_cut;
            lines.push(line.to_string());
            index += 1;
        }
        let bytes: u64 = lines.iter().map(|line| line.len() as u64).sum();
        self.spend(bytes)?;
        Ok(serde_json::json!({
            "lines": lines,
            "nextLine": more.then_some(index),
            "cut": cut,
        }))
    }

    fn list(&self, path: &str) -> Result<serde_json::Value, ScriptReadError> {
        let rel = parse_game_path(path)?;
        let rel_str = rel.as_str().to_string();
        // The merged view can hold the same directory in several layers. The
        // highest layer wins for a name, exactly as a read does.
        let mut entries: BTreeMap<String, (bool, u64)> = BTreeMap::new();
        for root in &self.roots {
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
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if is_reparse_point(&meta) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                entries.entry(name).or_insert((meta.is_dir(), meta.len()));
            }
        }
        let list: Vec<serde_json::Value> = entries
            .into_iter()
            .map(
                |(name, (dir, size))| serde_json::json!({ "name": name, "dir": dir, "size": size }),
            )
            .collect();
        Ok(serde_json::Value::Array(list))
    }
}

/// The directories that make up one instance's merged view, highest precedence
/// first. The plugin's own generated layer is included, so a script can read
/// back what it wrote; a game path never names a physical directory.
pub fn instance_view_roots(
    ctx: &crate::ctx::Ctx,
    instance_id: &str,
) -> Result<Vec<PathBuf>, String> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|_| "that is not an instance Agora knows".to_string())?;
    if !instance_dir.exists() {
        return Err("that instance does not exist".to_string());
    }
    let mut roots = vec![
        instance_dir.join("overrides"),
        instance_dir.join("writable"),
    ];
    let generated = instance_dir.join("plugin-generated");
    if let Ok(read_dir) = std::fs::read_dir(&generated) {
        for entry in read_dir.flatten() {
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                roots.push(entry.path());
            }
        }
    }
    if let Ok(Some(game)) = crate::game_deploy::deployment_dir(ctx, instance_id) {
        roots.push(game);
    }
    // The instance folder itself is not a view root: it holds Agora's own
    // manifest and import records, which may name a real path. Only the
    // folders the game actually sees are included.
    roots.retain(|root| root.exists());
    Ok(roots)
}

/// A `game:read` scope for one instance.
pub fn instance_reads(ctx: &crate::ctx::Ctx, instance_id: &str) -> Result<GameViewReads, String> {
    Ok(GameViewReads::new(
        instance_view_roots(ctx, instance_id)?,
        GameReadLimits::default(),
    ))
}

/// A `game:write` scope for one plugin's own generated layer in one instance.
pub fn instance_write_scope(
    ctx: &crate::ctx::Ctx,
    plugin_id: &agora_plugin_api::manifest::PluginId,
    instance_id: &str,
) -> Result<GameWriteScope, String> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|_| "that is not an instance Agora knows".to_string())?;
    if !instance_dir.exists() {
        return Err("that instance does not exist".to_string());
    }
    Ok(GameWriteScope::new(
        instance_dir
            .join("plugin-generated")
            .join(plugin_id.as_str()),
        GameWriteLimits::default(),
    ))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// One plugin's generated layer for one instance.
///
/// The layer root is chosen by core (`<instance>/plugin-generated/<plugin>`),
/// never by a script. A write joins a relative path below it, so it cannot
/// reach the game, a save or another layer.
pub struct GameWriteScope {
    layer_root: PathBuf,
    limits: GameWriteLimits,
    total_written: AtomicU64,
    files_written: AtomicU32,
}

impl GameWriteScope {
    pub fn new(layer_root: PathBuf, limits: GameWriteLimits) -> Self {
        Self {
            layer_root,
            limits,
            total_written: AtomicU64::new(0),
            files_written: AtomicU32::new(0),
        }
    }

    pub fn layer_root(&self) -> &Path {
        &self.layer_root
    }

    /// Write one file atomically (a temporary file in the same directory, then
    /// a rename) after checking both budgets.
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
        // Budget is claimed before doing any work, so two concurrent writes
        // cannot both slip under a total that only one of them fits.
        self.spend(bytes.len() as u64)?;

        let target = rel
            .as_str()
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(self.layer_root.clone(), |path, part| path.join(part));
        let Some(parent) = target.parent() else {
            return Err(refusal("invalid_path", "that write has no parent folder"));
        };
        std::fs::create_dir_all(parent)
            .map_err(|_| refusal("unwritable", "the generated layer could not be created"))?;
        reject_reparse_chain(&self.layer_root, rel.as_str())?;

        let existed = target.exists();
        if !existed {
            let mut current = self.files_written.load(Ordering::SeqCst);
            loop {
                if current >= self.limits.max_files {
                    return Err(refusal(
                        "file_budget_spent",
                        format!(
                            "this call has created its whole {} file budget",
                            self.limits.max_files
                        ),
                    ));
                }
                match self.files_written.compare_exchange(
                    current,
                    current + 1,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }

        let unique = format!(
            ".agora-write-{}-{}",
            std::process::id(),
            self.total_written.load(Ordering::SeqCst)
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
        Ok(())
    }

    fn spend(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total_written.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_write_bytes {
                return Err(refusal(
                    "budget_spent",
                    format!(
                        "this call has written its whole {} byte budget",
                        self.limits.max_total_write_bytes
                    ),
                ));
            }
            match self.total_written.compare_exchange(
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

// ---------------------------------------------------------------------------
// Launch recipes from a script
// ---------------------------------------------------------------------------

/// Environment variables that make a loader import code, or redirect where it
/// looks for code. They are refused in a recipe a script returns.
///
/// Sources: glibc's `ld.so(8)` documents `LD_PRELOAD`, `LD_AUDIT` and
/// `LD_LIBRARY_PATH`; Apple's `dyld(1)` documents `DYLD_INSERT_LIBRARIES`,
/// `DYLD_LIBRARY_PATH`, `DYLD_FRAMEWORK_PATH` and
/// `DYLD_FALLBACK_LIBRARY_PATH`; Oracle's Java launcher documentation
/// documents `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS` and `JDK_JAVA_OPTIONS`; the
/// Python documentation documents `PYTHONPATH` and `PYTHONHOME`; Microsoft's
/// "Dynamic-Link Library Search Order" documents `PATH`; Perl and Ruby document
/// `PERL5LIB` and `RUBYLIB`. Windows facts checked against those pages; the
/// list is deliberately conservative rather than exhaustive.
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
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5LIB",
    "RUBYLIB",
    "PATH",
];

fn blocked_environment_key(key: &str) -> Option<&'static str> {
    BLOCKED_LOADER_ENVIRONMENT
        .iter()
        .copied()
        .find(|blocked| blocked.eq_ignore_ascii_case(key))
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

fn check_recipe_value(value: &LaunchValue, roots: &RecipeRoots<'_>) -> Result<(), String> {
    match value {
        LaunchValue::Literal { .. } => Ok(()),
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
pub fn validate_launch_recipe(
    recipe: &LaunchRecipe,
    roots: &RecipeRoots<'_>,
) -> Result<(), String> {
    check_recipe_path(&recipe.executable, roots, "the executable")?;
    check_recipe_path(&recipe.working_directory, roots, "the working folder")?;
    for argument in &recipe.arguments {
        check_recipe_value(argument, roots)?;
    }
    for (key, value) in &recipe.environment {
        if let Some(blocked) = blocked_environment_key(key) {
            return Err(format!(
                "the recipe sets `{blocked}`, which can load or redirect code; a plugin's launch \
                 recipe may not set it"
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
            },
        );
        scope.write("a.txt", b"12345678").unwrap();
        let error = scope.write("b.txt", b"123").unwrap_err();
        assert_eq!(error.code, "budget_spent");
        assert!(!scope.layer_root().join("b.txt").exists());
    }
}
