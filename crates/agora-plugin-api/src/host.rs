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
use std::path::PathBuf;
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
}

/// Everything the script host needs in order to start a plugin.
#[derive(Debug, Clone)]
pub struct ActivationRequest {
    pub plugin_id: PluginId,
    /// Directory the plugin's files live in. The script host must refuse any
    /// module specifier that resolves outside it.
    pub package_root: PathBuf,
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
