//! The two traits that keep the script engine out of `agora-core`.
//!
//! `agora-core` owns plugin *policy*: what is installed, what is enabled, what
//! a plugin may ask for, and which core service answers each method. It does
//! not own the JavaScript engine, and does not depend on one — it holds an
//! `Arc<dyn ScriptHost>` that an adapter supplies, exactly the way it holds a
//! `dyn Clock` or a `dyn EventSink`.
//!
//! This is what makes "additional runtimes later" a real option rather than a
//! sentence in a plan. A companion process speaking another language
//! implements [`ScriptHost`] too; nothing above it changes.

use crate::error::PluginResult;
use crate::manifest::PluginId;
use crate::protocol::{HostRequest, HostResponse, LogLevel, PluginEventEnvelope};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What the script engine calls back into.
///
/// Implemented by `agora-core`. Calls arrive on the script host's own threads,
/// may block, and must not assume any particular caller.
pub trait HostBridge: Send + Sync {
    /// Serve one host call. Capability checks, argument validation and service
    /// dispatch all happen on this side; the script host does not get a vote.
    fn call(&self, plugin_id: &PluginId, request: HostRequest) -> HostResponse;

    /// Record a line in the plugin's own log.
    fn log(&self, plugin_id: &PluginId, level: LogLevel, message: &str);

    /// An event handler is about to run for this plugin. The core opens a fresh
    /// budget for it, so one handler's reads and writes never spend another's.
    fn begin_event(&self, _plugin_id: &PluginId) {}

    /// The event handler has finished, however it finished.
    fn end_event(&self, _plugin_id: &PluginId) {}
}

/// Everything the script host needs in order to start a plugin.
#[derive(Debug, Clone)]
pub struct ActivationRequest {
    pub plugin_id: PluginId,
    /// Directory the plugin's files live in. The script host must refuse any
    /// module specifier that resolves outside it.
    pub package_root: PathBuf,
    /// Agora's data root. The host may put a WebAssembly compile cache and
    /// short-lived staging folders here, and nowhere else.
    pub data_root: PathBuf,
    /// Package-relative path to the bundled entrypoint.
    pub entrypoint: String,
    /// Ceiling for this plugin's runtime, in bytes.
    pub memory_limit_bytes: usize,
    /// Stack ceiling for this plugin's runtime, in bytes.
    pub stack_limit_bytes: usize,
    /// Host API version string exposed to the plugin as `agora.apiVersion`.
    pub api_version: String,
    /// The plugin's current settings, so its `activate` can read them without
    /// a round-trip before it has done anything.
    pub settings: serde_json::Value,
}

impl ActivationRequest {
    /// Memory a plugin gets unless the host says otherwise.
    ///
    /// Generous enough for a dashboard that builds a table of every mod in
    /// every instance; far below the point where one misbehaving plugin
    /// threatens the launcher.
    pub const DEFAULT_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

    /// Stack a plugin gets. Deep enough for ordinary recursion, shallow
    /// enough that runaway recursion fails fast instead of taking the process.
    pub const DEFAULT_STACK_LIMIT: usize = 512 * 1024;
}

/// What a script may read during one call, and nothing else.
///
/// The crash doctor hands a parser one artifact and lets it pull bytes or lines
/// from that artifact and its children. A generic trait rather than a
/// crash-specific one because a WebAssembly host will implement it too: bytes
/// and JSON cross the boundary, never a file handle or a path.
pub trait ScriptReads: Send + Sync {
    /// At most `len` bytes from `offset`. A short reply at the end of the file
    /// is normal, not an error.
    fn read_bytes(&self, artifact: &str, offset: u64, len: u64)
        -> Result<Vec<u8>, ScriptReadError>;

    /// Lines decoded as text, as a [`serde_json::Value`] shaped like the crash
    /// contract's `LinesReply`: `lines`, an optional `next_line`, and `cut`.
    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError>;
}

/// How much one module run's mounts may stage.
///
/// This is separate from the `game:read` byte budget: staged bytes stay inside
/// the sandbox, and only the module's capped output comes back. A script may
/// lower these, never raise them (MASTER_SPEC §30.13).
#[derive(Debug, Clone, Copy)]
pub struct StageLimits {
    /// Total bytes staged across every mount in one run.
    pub max_bytes: u64,
    /// Total files staged across every mount in one run.
    pub max_files: u64,
}

impl Default for StageLimits {
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024 * 1024,
            max_files: 100_000,
        }
    }
}

/// Bytes and files one run's mounts have staged so far.
#[derive(Debug, Default)]
pub struct StageBudget {
    bytes: AtomicU64,
    files: AtomicU64,
}

impl StageBudget {
    /// Spend one file of `bytes`. Staging happens on one thread per run, so a
    /// load-and-add is enough; the atomics only exist so the method can take
    /// `&self` through the shared view trait.
    pub fn spend(&self, limits: &StageLimits, bytes: u64) -> Result<(), ScriptReadError> {
        let files = self.files.load(Ordering::SeqCst);
        if files >= limits.max_files {
            return Err(ScriptReadError::new(
                "mount_too_large",
                format!(
                    "the run's mounts would stage more than {} files",
                    limits.max_files
                ),
            ));
        }
        let total = self.bytes.load(Ordering::SeqCst);
        if total.saturating_add(bytes) > limits.max_bytes {
            return Err(ScriptReadError::new(
                "mount_too_large",
                format!(
                    "the run's mounts would stage more than {} bytes",
                    limits.max_bytes
                ),
            ));
        }
        self.bytes.fetch_add(bytes, Ordering::SeqCst);
        self.files.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Longest a stored blob's key may be.
pub const MAX_BLOB_KEY_LEN: usize = 128;

/// Whether `key` is a plain blob name: no path, no Windows device name, and
/// nothing that could name a stream or a folder. The same rule is used by the
/// store's writes and by a wasm `stored` mount, so a key that cannot be saved
/// can never be mounted either.
pub fn validate_blob_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("a blob key cannot be empty".into());
    }
    if key.len() > MAX_BLOB_KEY_LEN {
        return Err(format!(
            "a blob key may be at most {MAX_BLOB_KEY_LEN} bytes; `{}` is {}",
            key.chars().take(24).collect::<String>(),
            key.len()
        ));
    }
    if key == "." || key == ".." {
        return Err(format!("`{key}` is not a blob name"));
    }
    if let Some(bad) = key.chars().find(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err(format!(
            "a blob key cannot contain `{bad:?}`; it must be a plain name, like `notes.txt`"
        ));
    }
    if key.ends_with('.') || key.ends_with(' ') {
        return Err("a blob key cannot end with a dot or a space".into());
    }
    let stem = key.split('.').next().unwrap_or(key).trim_end();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=9).contains(&n))
        || upper
            .strip_prefix("LPT")
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n));
    if reserved {
        return Err(format!("`{key}` is a reserved device name"));
    }
    Ok(())
}

/// The stored blobs a plugin may stage into a module's mount, when the call
/// has one. The default refuses, so a scope without a blob store cannot be
/// reached by accident.
pub trait StoredBlobs: Send + Sync {
    /// The plugin whose blobs these are, for logs.
    fn plugin(&self) -> Option<&str> {
        None
    }

    /// Put one stored blob at `destination` (a file path inside a mount's
    /// staging folder), spending the run's mount allowance. A missing key is
    /// an error naming it, never an empty file.
    fn stage(
        &self,
        key: &str,
        destination: &Path,
        limits: &StageLimits,
        budget: &StageBudget,
    ) -> Result<u64, ScriptReadError>;
}

/// Read an instance's merged view by game path, and nothing else.
///
/// The literals a script passes are game paths, never filesystem paths: a
/// reply, an error and a log line may name the path the script asked for, and
/// no host path is ever revealed. The scope is one instance and one call
/// (MASTER_SPEC §30.12).
pub trait GameReads: Send + Sync {
    /// The instance this scope is bound to, when it is bound to one. A hook's
    /// `game.*` calls use it to refuse a different instance id.
    fn instance(&self) -> Option<&str> {
        None
    }

    /// At most `len` bytes from `offset`, or an error that says why.
    fn read_bytes(&self, path: &str, offset: u64, len: u64) -> Result<Vec<u8>, ScriptReadError>;

    /// Lines as text, shaped like the crash contract's `LinesReply`.
    fn read_lines(
        &self,
        path: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError>;

    /// The direct children of a game path. Directories are marked so a script
    /// can tell them apart without guessing from an extension.
    fn list(&self, path: &str) -> Result<serde_json::Value, ScriptReadError>;

    /// Stage the files under `folder` that match `patterns` into
    /// `destination`, preserving their paths below `folder`, and return how
    /// many were staged.
    ///
    /// This exists only for the trusted WebAssembly host, which needs real
    /// bytes to preopen read-only for a module. It resolves every source with
    /// the same merged-view rules as [`GameReads::read_bytes`] and never
    /// returns a source path to a script. Each staged file spends `budget`
    /// (the run's mount allowance), not the script's read budget. The default
    /// refuses, so a scope that has no merged view cannot stage one by
    /// accident.
    fn stage_view(
        &self,
        folder: &str,
        patterns: &[String],
        destination: &Path,
        limits: &StageLimits,
        budget: &StageBudget,
    ) -> Result<u64, ScriptReadError> {
        let _ = (folder, patterns, destination, limits, budget);
        Err(ScriptReadError::new(
            "unsupported",
            "this read scope cannot stage files for a module",
        ))
    }
}

/// Why a scoped read was refused. `code` is a stable token the script can
/// branch on; `message` is for the author and the plugin log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptReadError {
    pub code: String,
    pub message: String,
}

impl ScriptReadError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Whether `path` matches one of `patterns`, using the glob rules from
/// MASTER_SPEC §30.12: `*` stays inside one folder, `**` crosses folders, `?`
/// matches one non-separator character, and matching ignores case. A pattern
/// with no `/` matches a file name at any depth; one with a `/` matches the
/// whole relative path.
pub fn path_matches_any(patterns: &[String], path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    patterns.iter().any(|pattern| {
        if pattern.contains('/') {
            glob_matches(pattern, path)
        } else {
            glob_matches(pattern, name)
        }
    })
}

/// Match one glob against one path, case-insensitively.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let path: Vec<char> = path.to_lowercase().chars().collect();
    let mut table = vec![vec![false; path.len() + 1]; pattern.len() + 1];
    table[0][0] = true;
    for i in 1..=pattern.len() {
        if pattern[i - 1] == '*' {
            // A leading `*` can match an empty prefix; `**` behaves the same
            // here because the row below is filled from the left.
            table[i][0] = table[i - 1][0];
        }
    }
    for i in 1..=pattern.len() {
        for j in 1..=path.len() {
            table[i][j] = match pattern[i - 1] {
                '*' => {
                    let double = i >= 2 && pattern[i - 2] == '*';
                    let crosses = double || path[j - 1] != '/';
                    table[i - 1][j] || (crosses && table[i][j - 1])
                }
                '?' => path[j - 1] != '/' && table[i - 1][j - 1],
                other => other == path[j - 1] && table[i - 1][j - 1],
            };
        }
    }
    table[pattern.len()][path.len()]
}

/// A runtime that can load and run plugin code.
///
/// Implementations must isolate plugins from each other: a plugin that throws,
/// loops, or exhausts its memory must not affect another plugin, and must not
/// affect the launcher at all.
pub trait ScriptHost: Send + Sync {
    /// A short name for this runtime, for logs and the plugin manager
    /// ("quickjs 0.13"). Users report bugs against it.
    fn describe(&self) -> String;

    /// Load the entrypoint module and run the plugin's `activate` export, if
    /// it has one. Returns once activation has finished or failed.
    fn activate(&self, request: ActivationRequest, bridge: Arc<dyn HostBridge>)
        -> PluginResult<()>;

    /// Run the plugin's `deactivate` export, if it has one, then drop the
    /// runtime. Must be safe to call on a plugin that is not active.
    fn deactivate(&self, plugin_id: &PluginId) -> PluginResult<()>;

    /// Call an exported function and return its (possibly awaited) result.
    ///
    /// `timeout` is a hard ceiling: when it expires the implementation
    /// interrupts the plugin and returns [`crate::PluginErrorCode::Timeout`].
    fn invoke(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> PluginResult<serde_json::Value>;

    /// Like [`ScriptHost::invoke`], but the call sees `reads` and nothing else.
    ///
    /// The scope lasts exactly as long as this invocation. A script cannot use
    /// it to reach another artifact, and it cannot keep it after the export
    /// returns. Same isolation and timeout rules as `invoke`.
    fn invoke_with_reads(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
        reads: Arc<dyn ScriptReads>,
    ) -> PluginResult<serde_json::Value>;

    /// Like [`ScriptHost::invoke`], but the call sees one instance's merged
    /// view through `reads` (MASTER_SPEC §30.12). The scope lasts exactly as
    /// long as this invocation and cannot be kept afterwards.
    fn invoke_with_game_reads(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
        reads: Arc<dyn GameReads>,
    ) -> PluginResult<serde_json::Value>;

    /// Like [`ScriptHost::invoke`], with the call's merged view and its stored
    /// blobs in scope when given. The default ignores the scopes, so a host
    /// without a blob store still serves the rest of the API.
    fn invoke_with_scopes(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
        game_reads: Option<Arc<dyn GameReads>>,
        _blobs: Option<Arc<dyn StoredBlobs>>,
    ) -> PluginResult<serde_json::Value> {
        match game_reads {
            Some(reads) => self.invoke_with_game_reads(plugin_id, export, args, timeout, reads),
            None => self.invoke(plugin_id, export, args, timeout),
        }
    }

    /// Deliver an event to a plugin's subscribers.
    ///
    /// Best-effort and non-blocking from the caller's point of view: a slow
    /// subscriber must never hold up the operation that emitted the event.
    fn deliver_event(
        &self,
        plugin_id: &PluginId,
        envelope: &PluginEventEnvelope,
    ) -> PluginResult<()>;

    /// Interrupt whatever the plugin is doing right now.
    ///
    /// Used for user cancellation and for shutdown. A plugin that ignores
    /// cancellation cannot: the interrupt is enforced by the runtime, not
    /// requested of the script.
    fn cancel(&self, plugin_id: &PluginId);

    fn is_active(&self, plugin_id: &PluginId) -> bool;

    fn active_plugins(&self) -> Vec<PluginId>;
}
