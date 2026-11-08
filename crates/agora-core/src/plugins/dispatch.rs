//! The method table: every operation a plugin can ask the launcher to perform.
//!
//! This is the entire attack surface, in one file, on purpose. A method that
//! is not here does not exist, and every method that is here names the
//! capability it requires in the same place it names its implementation —
//! so "which permission does this need?" is answered by reading one match arm
//! rather than by auditing a service.
//!
//! Three rules hold for every arm:
//!
//! 1. **Capability first.** Checked before arguments are even parsed, so a
//!    plugin without `content:write` learns it cannot write rather than
//!    learning that its argument was malformed.
//! 2. **Reuse, never reimplement.** Each arm calls the same `agora-core`
//!    service the GUI and the CLI call, so plugins inherit the locks, the
//!    validation, the operation state and the recovery behaviour for free. A
//!    plugin cannot reach a code path the user could not reach themselves.
//! 3. **No handles out.** Arguments and results are JSON. No `Ctx`, no
//!    `Connection`, no filesystem path a plugin could use to go around this.

use super::events::EventSubscriptions;
use super::store::{self, StorageKind};
use crate::ctx::Ctx;
use crate::error::LauncherError;
use agora_plugin_api::capability::{Capability, CapabilitySet};
use agora_plugin_api::contributions::SettingDefinition;
use agora_plugin_api::dto;
use agora_plugin_api::error::{PluginError, PluginErrorCode, PluginResult};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::PluginEvent;

/// Where a plugin's UI intents go.
///
/// A plugin cannot draw on the launcher directly; it can ask for its own view
/// to re-render, or raise a toast attributed to it. The adapter decides how,
/// and is free to ignore a plugin that asks too often.
pub trait PluginUiSink: Send + Sync {
    fn refresh(&self, plugin_id: &PluginId, view_id: Option<&str>);
    fn notify(&self, plugin_id: &PluginId, tone: dto::Tone, message: &str);
}

/// A sink that drops everything. Used by the CLI and by tests.
pub struct NoopUiSink;

impl PluginUiSink for NoopUiSink {
    fn refresh(&self, _plugin_id: &PluginId, _view_id: Option<&str>) {}
    fn notify(&self, _plugin_id: &PluginId, _tone: dto::Tone, _message: &str) {}
}

/// Everything one dispatch needs to know about the calling plugin.
pub struct DispatchContext<'a> {
    pub ctx: &'a Ctx,
    pub plugin_id: &'a PluginId,
    pub granted: &'a CapabilitySet,
    /// Hosts from the plugin's manifest, for `net.*`.
    pub declared_hosts: &'a [String],
    /// Declared settings, so a write can be validated against its schema.
    pub settings_schema: &'a [SettingDefinition],
    pub subscriptions: &'a EventSubscriptions,
    pub ui: &'a dyn PluginUiSink,
    /// The instance a hook is bound to. `None` for a normal plugin call.
    /// A bound call may not name a different instance.
    pub bound_instance: Option<&'a str>,
}

/// Every method this host API version serves.
///
/// Kept as data so the documentation, the CLI's `agora plugin methods`, and
/// the capability check all read from one list that cannot drift apart.
pub const METHODS: &[(&str, Option<Capability>, &str)] = &[
    (
        "instance.list",
        Some(Capability::InstanceRead),
        "List every instance.",
    ),
    (
        "instance.get",
        Some(Capability::InstanceRead),
        "Inspect one instance.",
    ),
    (
        "instance.rename",
        Some(Capability::InstanceWrite),
        "Rename an instance.",
    ),
    (
        "instance.setMemory",
        Some(Capability::InstanceWrite),
        "Set an instance's heap ceiling.",
    ),
    (
        "content.list",
        Some(Capability::ContentRead),
        "List installed content.",
    ),
    (
        "content.enable",
        Some(Capability::ContentWrite),
        "Enable an installed item.",
    ),
    (
        "content.disable",
        Some(Capability::ContentWrite),
        "Disable an installed item.",
    ),
    (
        "content.setUpdatePinned",
        Some(Capability::ContentWrite),
        "Pin or unpin an item against updates.",
    ),
    (
        "launch.state",
        Some(Capability::LaunchRead),
        "What an instance is doing now.",
    ),
    (
        "launch.history",
        Some(Capability::LaunchRead),
        "Past launches for an instance.",
    ),
    (
        "storage.get",
        None,
        "Read one of this plugin's stored values.",
    ),
    (
        "storage.set",
        None,
        "Write one of this plugin's stored values.",
    ),
    (
        "storage.all",
        None,
        "Read all of this plugin's stored values.",
    ),
    (
        "storage.remove",
        None,
        "Delete one of this plugin's stored values.",
    ),
    (
        "storage.putBytes",
        None,
        "Store bytes in this plugin's own blob store.",
    ),
    (
        "storage.getBytes",
        None,
        "Read bytes from this plugin's own blob store.",
    ),
    (
        "storage.deleteBytes",
        None,
        "Delete one of this plugin's stored blobs.",
    ),
    (
        "settings.all",
        None,
        "Read this plugin's declared settings.",
    ),
    ("events.subscribe", None, "Start receiving an event."),
    ("events.unsubscribe", None, "Stop receiving an event."),
    (
        "net.fetchJson",
        Some(Capability::Network),
        "GET JSON from a declared host.",
    ),
    ("ui.refresh", None, "Ask this plugin's view to re-render."),
    (
        "ui.notify",
        None,
        "Raise a toast attributed to this plugin.",
    ),
    (
        "game.readBytes",
        Some(Capability::GameRead),
        "Read bytes from a game instance's merged view by game path.",
    ),
    (
        "game.readLines",
        Some(Capability::GameRead),
        "Read lines from a game instance's merged view by game path.",
    ),
    (
        "game.list",
        Some(Capability::GameRead),
        "List the children of a game path in an instance.",
    ),
    (
        "game.writeBytes",
        Some(Capability::GameWrite),
        "Write bytes into this plugin's own generated layer.",
    ),
    (
        "game.writeText",
        Some(Capability::GameWrite),
        "Write text into this plugin's own generated layer.",
    ),
];

/// What calling a method requires.
///
/// Three outcomes rather than two, because "there is no such method" and "you
/// may not call that method" are different answers and a plugin author
/// debugging a typo should not be told they lack a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodRequirement {
    /// Not part of this host API version at all.
    Unknown,
    /// Exists and needs no capability — a plugin's own storage, its settings,
    /// its own UI.
    Open,
    /// Exists and needs this capability.
    Requires(Capability),
}

/// What `method` requires of its caller.
pub fn required_capability(method: &str) -> MethodRequirement {
    match METHODS.iter().find(|(name, _, _)| *name == method) {
        None => MethodRequirement::Unknown,
        Some((_, None, _)) => MethodRequirement::Open,
        Some((_, Some(capability), _)) => MethodRequirement::Requires(*capability),
    }
}

/// Serve one plugin request.
pub fn dispatch(
    cx: &DispatchContext<'_>,
    method: &str,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    // Rule 1: the capability gate runs before anything looks at `args`.
    match required_capability(method) {
        MethodRequirement::Unknown => return Err(PluginError::unknown_method(method)),
        MethodRequirement::Requires(capability) if !cx.granted.contains(capability) => {
            return Err(PluginError::capability_denied(capability, method))
        }
        MethodRequirement::Requires(_) | MethodRequirement::Open => {}
    }

    match method {
        "instance.list" => instance_list(cx),
        "instance.get" => instance_get(cx, args),
        "instance.rename" => instance_rename(cx, args),
        "instance.setMemory" => instance_set_memory(cx, args),
        "content.list" => content_list(cx, args),
        "content.enable" => content_set_enabled(cx, args, true),
        "content.disable" => content_set_enabled(cx, args, false),
        "content.setUpdatePinned" => content_set_pinned(cx, args),
        "launch.state" => launch_state(cx, args),
        "launch.history" => launch_history(cx, args),
        "storage.get" => storage_get(cx, args, StorageKind::Data),
        "storage.set" => storage_set(cx, args),
        "storage.all" => storage_all(cx, args, StorageKind::Data),
        "storage.remove" => storage_remove(cx, args),
        "storage.putBytes" => storage_put_bytes(cx, args),
        "storage.getBytes" => storage_get_bytes(cx, args),
        "storage.deleteBytes" => storage_delete_bytes(cx, args),
        "settings.all" => storage_all(cx, args, StorageKind::Setting),
        "events.subscribe" => events_subscribe(cx, args),
        "events.unsubscribe" => events_unsubscribe(cx, args),
        "net.fetchJson" => net_fetch_json(cx, args),
        "ui.refresh" => ui_refresh(cx, args),
        "ui.notify" => ui_notify(cx, args),
        "game.readBytes" => game_read_bytes(cx, args),
        "game.readLines" => game_read_lines(cx, args),
        "game.list" => game_list(cx, args),
        "game.writeBytes" => game_write_bytes(cx, args),
        "game.writeText" => game_write_text(cx, args),
        // Unreachable: `required_capability` already rejected unknown names.
        other => Err(PluginError::unknown_method(other)),
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn arg_str(args: &serde_json::Value, key: &str) -> PluginResult<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| PluginError::invalid_arguments(format!("`{key}` must be a string")))
}

fn arg_opt_str(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn arg_i64(args: &serde_json::Value, key: &str) -> PluginResult<i64> {
    args.get(key)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| PluginError::invalid_arguments(format!("`{key}` must be a whole number")))
}

fn arg_bool(args: &serde_json::Value, key: &str) -> PluginResult<bool> {
    args.get(key)
        .and_then(|value| value.as_bool())
        .ok_or_else(|| PluginError::invalid_arguments(format!("`{key}` must be true or false")))
}

/// Turn a core error into a plugin error without leaking internals.
///
/// The code is preserved because it is already a stable, user-facing string in
/// this codebase, and a plugin author debugging `ERR_INSTANCE_NOT_FOUND` is
/// better served than one debugging "operation failed".
fn operation_failed(error: LauncherError) -> PluginError {
    let message = error.to_string();
    PluginError::new(PluginErrorCode::OperationFailed, message)
}

fn json<T: serde::Serialize>(value: T) -> PluginResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|e| PluginError::internal(format!("could not serialise the result: {e}")))
}

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

fn to_summary(row: &crate::models::InstanceRow) -> dto::InstanceSummary {
    dto::InstanceSummary {
        id: row.instance_id.clone(),
        name: row.name.clone(),
        minecraft_version: row.minecraft_version.clone(),
        loader: row.loader.clone(),
        loader_version: row.loader_version.clone(),
        is_modpack: row.is_modpack,
        is_locked: row.is_locked,
        last_launched_at: row.last_launched_at.clone(),
        created_at: row.created_at.clone(),
    }
}

fn instance_list(cx: &DispatchContext<'_>) -> PluginResult<serde_json::Value> {
    let backend = cx.ctx.games.instance_backend().map_err(operation_failed)?;
    let rows = backend.list(cx.ctx).map_err(operation_failed)?;
    json(rows.iter().map(to_summary).collect::<Vec<_>>())
}

fn instance_get(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let backend = cx.ctx.games.instance_backend().map_err(operation_failed)?;
    let Some((row, manifest)) = backend
        .get(cx.ctx, &instance_id)
        .map_err(operation_failed)?
    else {
        return Ok(serde_json::Value::Null);
    };

    let mut counts = dto::ContentCounts::default();
    if let Some(manifest) = &manifest {
        counts.mods = manifest.mods.len() as u32;
        counts.resourcepacks = manifest.resourcepacks.len() as u32;
        counts.shaders = manifest.shaders.len() as u32;
        counts.datapacks = manifest.datapacks.len() as u32;
        counts.worlds = manifest.worlds.len() as u32;
        counts.disabled = manifest
            .mods
            .iter()
            .chain(manifest.resourcepacks.iter())
            .chain(manifest.shaders.iter())
            .chain(manifest.datapacks.iter())
            .chain(manifest.worlds.iter())
            .filter(|entry| !entry.enabled)
            .count() as u32;
    }

    json(dto::InstanceDetail {
        summary: to_summary(&row),
        jvm: dto::JvmSettings {
            memory_mb: row.jvm_memory_mb,
            memory_mode: row.jvm_memory_mode.clone(),
            gc: row.jvm_gc.clone(),
            custom_args: row.jvm_custom_args.clone(),
            always_pre_touch: row.jvm_always_pre_touch,
            // The path itself stays behind the boundary.
            has_java_override: row.java_path.is_some(),
        },
        launch_mode: row.launch_mode_override.clone(),
        pack_origin: manifest
            .as_ref()
            .and_then(|manifest| manifest.created_from_pack.clone()),
        content_counts: counts,
    })
}

fn instance_rename(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let name = arg_str(args, "name")?;
    cx.ctx
        .games
        .instance_backend()
        .and_then(|backend| backend.rename(cx.ctx, &instance_id, &name))
        .map_err(operation_failed)?;
    Ok(serde_json::json!({ "renamed": true }))
}

fn instance_set_memory(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let memory_mb = arg_i64(args, "memoryMb")?;
    let backend = cx.ctx.games.instance_backend().map_err(operation_failed)?;
    // Read-modify-write through the same service the settings page uses, so
    // the other JVM fields keep their values and core's own clamping applies.
    let Some((row, _)) = backend
        .get(cx.ctx, &instance_id)
        .map_err(operation_failed)?
    else {
        return Err(PluginError::new(
            PluginErrorCode::OperationFailed,
            format!("no instance `{instance_id}`"),
        ));
    };
    backend
        .update_jvm(
            cx.ctx,
            &instance_id,
            memory_mb,
            &row.jvm_gc,
            row.jvm_always_pre_touch,
            &row.jvm_custom_args,
            // A plugin setting an explicit ceiling means the user is no longer
            // on automatic sizing; saying so is more honest than leaving the
            // settings page claiming "auto" while showing a fixed number.
            "manual",
        )
        .map_err(operation_failed)?;
    Ok(serde_json::json!({ "memoryMb": memory_mb }))
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

fn read_content(
    cx: &DispatchContext<'_>,
    instance_id: &str,
    content_type: Option<&str>,
) -> PluginResult<Vec<dto::ContentEntry>> {
    let backend = cx.ctx.games.instance_backend().map_err(operation_failed)?;
    backend
        .content(cx.ctx, instance_id, content_type)
        .map_err(operation_failed)?
        .ok_or_else(|| {
            PluginError::new(
                PluginErrorCode::OperationFailed,
                format!("no instance `{instance_id}`"),
            )
        })
}

fn content_list(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let content_type = arg_opt_str(args, "contentType");
    let entries = read_content(cx, &instance_id, content_type.as_deref())?;
    json(entries)
}

/// Turn the key a plugin holds into the filename core's services take.
///
/// Done by looking the key up in the instance's real content list rather than
/// by splitting the string, which means a plugin cannot name a file that is
/// not actually installed — the mapping is a lookup, not a parse.
fn filename_for_key(
    cx: &DispatchContext<'_>,
    instance_id: &str,
    key: &str,
) -> PluginResult<String> {
    read_content(cx, instance_id, None)?
        .into_iter()
        .find(|row| row.key == key)
        .map(|row| row.filename)
        .ok_or_else(|| {
            PluginError::invalid_arguments(format!("`{key}` is not installed in `{instance_id}`"))
        })
}

fn content_set_enabled(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
    enabled: bool,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let key = arg_str(args, "key")?;
    let filename = filename_for_key(cx, &instance_id, &key)?;
    let service = crate::crash_service::CrashService::new(cx.ctx.clone());
    let result = if enabled {
        service.enable_mod(&instance_id, &filename)
    } else {
        service.disable_mod(&instance_id, &filename)
    };
    result.map_err(operation_failed)?;
    Ok(serde_json::json!({ "key": key, "enabled": enabled }))
}

fn content_set_pinned(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let key = arg_str(args, "key")?;
    let pinned = arg_bool(args, "pinned")?;
    let filename = filename_for_key(cx, &instance_id, &key)?;
    let changed = cx
        .ctx
        .games
        .instance_backend()
        .and_then(|backend| backend.set_update_pinned(cx.ctx, &instance_id, &filename, pinned))
        .map_err(operation_failed)?;
    Ok(serde_json::json!({ "key": key, "pinned": pinned, "changed": changed }))
}

// ---------------------------------------------------------------------------
// Launch
// ---------------------------------------------------------------------------

fn launch_state(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    let session = cx
        .ctx
        .process_session_manager
        .list()
        .into_iter()
        .find(|session| session.instance_id == instance_id);
    let state = match session {
        Some(session) => dto::LaunchState {
            instance_id,
            status: if session.attached {
                "running"
            } else {
                "exited"
            }
            .to_string(),
            started_at: chrono::DateTime::<chrono::Utc>::from(session.start_time)
                .to_rfc3339()
                .into(),
            pid: Some(session.pid),
        },
        None => dto::LaunchState {
            instance_id,
            status: "idle".to_string(),
            started_at: None,
            pid: None,
        },
    };
    json(state)
}

fn launch_history(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_str(args, "instanceId")?;
    // Bounded regardless of what the plugin asks for: a view that renders
    // every launch ever is a view nobody reads and a payload nobody wants.
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(25)
        .clamp(1, 200) as usize;
    let conn = crate::db::local_state_connection(&cx.ctx.paths.local_state_db())
        .map_err(|e| PluginError::new(PluginErrorCode::OperationFailed, e.to_string()))?;
    let records = crate::launch_history::list_history(&conn, &instance_id, limit)
        .map_err(|e| PluginError::new(PluginErrorCode::OperationFailed, e.to_string()))?;
    let entries: Vec<dto::LaunchHistoryEntry> = records
        .into_iter()
        .map(|record| dto::LaunchHistoryEntry {
            instance_id: record.instance_id,
            started_at: record.started_at,
            prep_ms: record.prep_ms,
            duration_ms: record.duration_ms,
            outcome: record.outcome.map(|outcome| outcome.as_str().to_string()),
            enabled_mod_count: record.enabled_mod_count,
            minecraft_version: record.minecraft_version,
            loader: record.loader,
            peak_memory_mb: record.peak_memory_mb,
        })
        .collect();
    json(entries)
}

// ---------------------------------------------------------------------------
// Storage and settings
// ---------------------------------------------------------------------------

fn storage_conn(cx: &DispatchContext<'_>) -> PluginResult<rusqlite::Connection> {
    crate::db::local_state_connection(&cx.ctx.paths.local_state_db())
        .map_err(|e| PluginError::new(PluginErrorCode::OperationFailed, e.to_string()))
}

fn storage_get(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
    kind: StorageKind,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let instance_id = arg_opt_str(args, "instanceId");
    let conn = storage_conn(cx)?;
    let value = store::storage_get(&conn, cx.plugin_id, kind, instance_id.as_deref(), &key)
        .map_err(operation_failed)?;
    Ok(value.unwrap_or(serde_json::Value::Null))
}

fn storage_all(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
    kind: StorageKind,
) -> PluginResult<serde_json::Value> {
    let instance_id = arg_opt_str(args, "instanceId");
    let conn = storage_conn(cx)?;
    let mut values = store::storage_all(&conn, cx.plugin_id, kind, instance_id.as_deref())
        .map_err(operation_failed)?;

    // A declared setting the user never touched still has a value: its
    // default. Filling it in here means a plugin never has to carry its own
    // copy of the defaults and never sees `undefined` for a setting it
    // declared.
    if kind == StorageKind::Setting {
        for definition in cx.settings_schema {
            values
                .entry(definition.key.clone())
                .or_insert_with(|| definition.schema.default_value());
        }
    }
    Ok(serde_json::Value::Object(values))
}

fn storage_set(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let instance_id = arg_opt_str(args, "instanceId");
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| PluginError::invalid_arguments("`value` is required"))?;
    let conn = storage_conn(cx)?;
    store::storage_set(
        &conn,
        cx.plugin_id,
        StorageKind::Data,
        instance_id.as_deref(),
        &key,
        &value,
    )
    .map_err(|e| match &e {
        LauncherError::Generic { code, message } if code == "ERR_PLUGIN_STORAGE_QUOTA" => {
            PluginError::new(PluginErrorCode::ResourceExhausted, message.clone())
        }
        LauncherError::Generic { code, message } if code == "ERR_PLUGIN_STORAGE_KEY" => {
            PluginError::invalid_arguments(message.clone())
        }
        _ => operation_failed(e),
    })?;
    Ok(serde_json::json!({ "stored": true }))
}

fn storage_remove(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let instance_id = arg_opt_str(args, "instanceId");
    let conn = storage_conn(cx)?;
    let removed = store::storage_remove(
        &conn,
        cx.plugin_id,
        StorageKind::Data,
        instance_id.as_deref(),
        &key,
    )
    .map_err(operation_failed)?;
    Ok(serde_json::json!({ "removed": removed }))
}

fn storage_blob_failed(error: LauncherError) -> PluginError {
    match &error {
        LauncherError::Generic { code, message } if code == "ERR_PLUGIN_STORAGE_QUOTA" => {
            PluginError::new(PluginErrorCode::ResourceExhausted, message.clone())
        }
        LauncherError::Generic { code, message } if code == "ERR_PLUGIN_STORAGE_KEY" => {
            PluginError::invalid_arguments(message.clone())
        }
        _ => operation_failed(error),
    }
}

fn arg_bytes(args: &serde_json::Value) -> PluginResult<Vec<u8>> {
    args.get("bytes")
        .and_then(|value| value.as_array())
        .ok_or_else(|| PluginError::invalid_arguments("`bytes` must be an array of byte values"))?
        .iter()
        .map(|value| value.as_u64())
        .collect::<Option<Vec<u64>>>()
        .ok_or_else(|| PluginError::invalid_arguments("every byte must be a whole number"))?
        .into_iter()
        .map(|value| {
            u8::try_from(value)
                .map_err(|_| PluginError::invalid_arguments("every byte must be between 0 and 255"))
        })
        .collect()
}

fn storage_put_bytes(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let bytes = arg_bytes(args)?;
    let written = crate::plugins::blobs::put(cx.ctx, cx.plugin_id, &key, &bytes)
        .map_err(storage_blob_failed)?;
    Ok(serde_json::json!({ "written": written }))
}

fn storage_get_bytes(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let bytes =
        crate::plugins::blobs::get(cx.ctx, cx.plugin_id, &key).map_err(storage_blob_failed)?;
    Ok(match bytes {
        Some(bytes) => serde_json::json!({ "bytes": bytes }),
        None => serde_json::Value::Null,
    })
}

fn storage_delete_bytes(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let key = arg_str(args, "key")?;
    let deleted =
        crate::plugins::blobs::delete(cx.ctx, cx.plugin_id, &key).map_err(storage_blob_failed)?;
    Ok(serde_json::json!({ "deleted": deleted }))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

fn events_subscribe(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let event = arg_str(args, "event")?;
    if !PluginEvent::NAMES.contains(&event.as_str()) {
        return Err(PluginError::invalid_arguments(format!(
            "`{event}` is not an event Agora emits; known events are {}",
            PluginEvent::NAMES.join(", ")
        )));
    }
    // Subscribing is reading. The capability that guards the *event* is what
    // decides, not the fact that `events.subscribe` itself needs none.
    let capability = capability_for_event(&event);
    if let Some(capability) = capability {
        if !cx.granted.contains(capability) {
            return Err(PluginError::capability_denied(
                capability,
                &format!("events.subscribe({event})"),
            ));
        }
    }
    cx.subscriptions.subscribe(cx.plugin_id, &event);
    Ok(serde_json::json!({ "subscribed": event }))
}

fn events_unsubscribe(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let event = arg_str(args, "event")?;
    cx.subscriptions.unsubscribe(cx.plugin_id, &event);
    Ok(serde_json::json!({ "unsubscribed": event }))
}

/// The capability needed to hear about an event, by name.
pub fn capability_for_event(event: &str) -> Option<Capability> {
    match event {
        "instance.created" | "instance.deleted" | "instance.renamed" | "registry.synced" => {
            Some(Capability::InstanceRead)
        }
        "content.installed" | "content.removed" | "content.enabled" | "content.disabled" => {
            Some(Capability::ContentRead)
        }
        "launch.started" | "launch.exited" => Some(Capability::LaunchRead),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// How much of a plugin's HTTP response is read before giving up.
const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;

/// Read a stream, stopping one byte past `limit`.
///
/// The check happens while reading, so an oversized body is refused before it
/// is held in memory rather than after.
fn read_capped(mut reader: impl std::io::Read, limit: usize) -> PluginResult<Vec<u8>> {
    use std::io::Read as _;
    let mut body = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| {
            PluginError::new(
                PluginErrorCode::OperationFailed,
                format!("could not read the response body: {e}"),
            )
        })?;
    if body.len() > limit {
        return Err(PluginError::new(
            PluginErrorCode::ResourceExhausted,
            format!("the response is larger than the {limit} byte limit"),
        ));
    }
    Ok(body)
}

fn net_fetch_json(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let url = arg_str(args, "url")?;
    // A plugin installed under reduced security mode keeps its wide reach
    // only while the mode stays on.
    let elevated = cx.declared_hosts.len()
        > agora_plugin_api::manifest::NetworkDeclaration::MAX_HOSTS
        || cx.declared_hosts.iter().any(|h| h == "*");
    if elevated && !crate::settings::reduced_security_enabled(cx.ctx) {
        return Err(PluginError::new(
            PluginErrorCode::NetworkDenied,
            "this plugin's network access needs Reduced security mode, which is off",
        ));
    }
    if cx.declared_hosts.is_empty() {
        return Err(PluginError::new(
            PluginErrorCode::NetworkDenied,
            "this plugin declared no hosts, so it can reach none",
        ));
    }
    // Goes through the same broker as every other outbound request: the
    // network gate (including Lockdown) runs first, then the URL is checked
    // against the plugin's declared hosts, then each redirect hop is
    // re-checked against the same list.
    let response = crate::http_client::blocking_checked_request_with_policy(
        &cx.ctx.http_clients,
        crate::http_client::ClientCategory::Plugin,
        &url,
        crate::http_client::HostPolicy::PluginDeclared(cx.declared_hosts),
    )
    .map_err(|e| {
        let message = e.to_string();
        let code = match &e {
            LauncherError::Generic { code, .. }
                if code == "ERR_NETWORK_LOCKDOWN"
                    || code == "ERR_NETWORK_ENDPOINT_DISABLED"
                    || code.starts_with("ERR_NETWORK_HOST") =>
            {
                PluginErrorCode::NetworkDenied
            }
            _ => PluginErrorCode::NetworkDenied,
        };
        PluginError::new(code, message)
    })?;

    let status = response.status().as_u16();
    // Read at most one byte past the limit. Reading the whole body first would
    // let a host that answers with a huge stream exhaust this process's memory
    // before the size check ever ran.
    let body = read_capped(response, MAX_FETCH_BYTES)?;
    let parsed: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        PluginError::new(
            PluginErrorCode::OperationFailed,
            format!("the response was not JSON: {e}"),
        )
    })?;
    Ok(serde_json::json!({ "status": status, "body": parsed }))
}

// ---------------------------------------------------------------------------
// Game instance view (MASTER_SPEC §30.12)
// ---------------------------------------------------------------------------

fn game_instance_id(cx: &DispatchContext<'_>, args: &serde_json::Value) -> PluginResult<String> {
    let instance_id = arg_str(args, "instanceId")?;
    if let Some(bound) = cx.bound_instance {
        if bound != instance_id {
            return Err(PluginError::new(
                PluginErrorCode::CapabilityDenied,
                format!(
                    "this hook is bound to instance `{bound}`; it may not name `{instance_id}`"
                ),
            ));
        }
    }
    Ok(instance_id)
}

fn game_budgets(cx: &DispatchContext<'_>) -> std::sync::Arc<crate::game_script::GameBudgets> {
    cx.ctx
        .extensions
        .get_or_insert_with(crate::game_script::GameBudgets::default)
}

fn game_reads(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<crate::game_script::GameViewReads> {
    let instance_id = game_instance_id(cx, args)?;
    let budget = game_budgets(cx).session(cx.plugin_id.as_str()).reads;
    crate::game_script::instance_reads_with_budget(cx.ctx, &instance_id, budget)
        .map_err(|message| PluginError::new(PluginErrorCode::OperationFailed, message))
}

fn game_read_error(error: agora_plugin_api::host::ScriptReadError) -> PluginError {
    PluginError::new(PluginErrorCode::OperationFailed, error.message).with_detail(error.code)
}

fn game_read_bytes(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    use agora_plugin_api::host::GameReads;
    let path = arg_str(args, "path")?;
    let offset = args
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let len = args
        .get("len")
        .and_then(|value| value.as_u64())
        .unwrap_or(crate::game_script::DEFAULT_MAX_READ_BYTES);
    let reads = game_reads(cx, args)?;
    let bytes = reads
        .read_bytes(&path, offset, len)
        .map_err(game_read_error)?;
    Ok(serde_json::json!({ "bytes": bytes }))
}

fn game_read_lines(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    use agora_plugin_api::host::GameReads;
    let path = arg_str(args, "path")?;
    let from_line = args
        .get("fromLine")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let max_lines = args
        .get("maxLines")
        .and_then(|value| value.as_u64())
        .unwrap_or(crate::game_script::DEFAULT_MAX_LINES as u64)
        .min(u32::MAX as u64) as u32;
    let reads = game_reads(cx, args)?;
    reads
        .read_lines(&path, from_line, max_lines)
        .map_err(game_read_error)
}

fn game_list(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    use agora_plugin_api::host::GameReads;
    let path = arg_opt_str(args, "path").unwrap_or_default();
    let reads = game_reads(cx, args)?;
    reads.list(&path).map_err(game_read_error)
}

fn game_write_scope(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<crate::game_script::GameWriteScope> {
    let instance_id = game_instance_id(cx, args)?;
    let budget = game_budgets(cx).session(cx.plugin_id.as_str()).writes;
    crate::game_script::instance_write_scope_with_budget(cx.ctx, cx.plugin_id, &instance_id, budget)
        .map_err(|message| PluginError::new(PluginErrorCode::OperationFailed, message))
}

fn game_write_bytes(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let path = arg_str(args, "path")?;
    let bytes: Vec<u8> = args
        .get("bytes")
        .and_then(|value| value.as_array())
        .ok_or_else(|| PluginError::invalid_arguments("`bytes` must be an array of byte values"))?
        .iter()
        .map(|value| value.as_u64())
        .collect::<Option<Vec<u64>>>()
        .ok_or_else(|| PluginError::invalid_arguments("every byte must be a whole number"))?
        .into_iter()
        .map(|value| {
            u8::try_from(value)
                .map_err(|_| PluginError::invalid_arguments("every byte must be between 0 and 255"))
        })
        .collect::<PluginResult<Vec<u8>>>()?;
    let scope = game_write_scope(cx, args)?;
    scope.write(&path, &bytes).map_err(game_read_error)?;
    Ok(serde_json::json!({ "written": bytes.len() }))
}

fn game_write_text(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let path = arg_str(args, "path")?;
    let text = arg_str(args, "text")?;
    let scope = game_write_scope(cx, args)?;
    scope
        .write(&path, text.as_bytes())
        .map_err(game_read_error)?;
    Ok(serde_json::json!({ "written": text.len() }))
}

// ---------------------------------------------------------------------------
// UI intents
// ---------------------------------------------------------------------------

fn ui_refresh(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    cx.ui
        .refresh(cx.plugin_id, arg_opt_str(args, "viewId").as_deref());
    Ok(serde_json::json!({ "requested": true }))
}

/// Longest toast a plugin may raise.
const MAX_NOTIFY_CHARS: usize = 200;

fn ui_notify(
    cx: &DispatchContext<'_>,
    args: &serde_json::Value,
) -> PluginResult<serde_json::Value> {
    let message = arg_str(args, "message")?;
    if message.chars().count() > MAX_NOTIFY_CHARS {
        return Err(PluginError::invalid_arguments(format!(
            "a notification may be {MAX_NOTIFY_CHARS} characters; that one is {}",
            message.chars().count()
        )));
    }
    let tone = arg_opt_str(args, "tone")
        .and_then(|tone| serde_json::from_value(serde_json::Value::String(tone)).ok())
        .unwrap_or(dto::Tone::Neutral);
    cx.ui.notify(cx.plugin_id, tone, &message);
    Ok(serde_json::json!({ "shown": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_larger_than_the_limit_is_refused_while_reading() {
        let exactly = std::io::Cursor::new(vec![b'x'; 16]);
        assert_eq!(read_capped(exactly, 16).unwrap().len(), 16);

        /// An endless body that counts how many bytes were pulled from it, so
        /// the test can tell "stopped at the cap" from "read it all".
        struct Endless {
            served: usize,
            max: usize,
        }
        impl std::io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.served >= self.max {
                    return Ok(0);
                }
                self.served += 1;
                buf[0] = b'x';
                Ok(1)
            }
        }

        let too_large = Endless {
            served: 0,
            max: 10_000,
        };
        let mut too_large = too_large;
        let error = read_capped(&mut too_large, 16).unwrap_err();
        assert_eq!(error.code, PluginErrorCode::ResourceExhausted);
        assert_eq!(
            too_large.served, 17,
            "it reads at most one byte past the limit, not the whole body"
        );
    }

    #[test]
    fn every_method_name_is_unique() {
        let mut names: Vec<&str> = METHODS.iter().map(|(name, _, _)| *name).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "a method name is listed twice");
    }

    #[test]
    fn an_unknown_method_is_not_a_permission_problem() {
        assert_eq!(
            required_capability("instance.destroyEverything"),
            MethodRequirement::Unknown
        );
    }

    #[test]
    fn reads_and_writes_require_different_capabilities() {
        assert_eq!(
            required_capability("content.list"),
            MethodRequirement::Requires(Capability::ContentRead)
        );
        assert_eq!(
            required_capability("content.disable"),
            MethodRequirement::Requires(Capability::ContentWrite)
        );
    }

    #[test]
    fn a_plugins_own_storage_needs_no_capability() {
        for method in [
            "storage.get",
            "storage.set",
            "storage.all",
            "storage.remove",
            "storage.putBytes",
            "storage.getBytes",
            "storage.deleteBytes",
        ] {
            assert_eq!(
                required_capability(method),
                MethodRequirement::Open,
                "{method}"
            );
        }
    }

    #[test]
    fn blob_storage_is_open_and_refuses_a_path_key() {
        assert_eq!(
            required_capability("storage.putBytes"),
            MethodRequirement::Open
        );
        assert_eq!(
            required_capability("storage.getBytes"),
            MethodRequirement::Open
        );
        assert_eq!(
            required_capability("storage.deleteBytes"),
            MethodRequirement::Open
        );
        let error = dispatch_with(
            CapabilitySet::new(),
            "storage.putBytes",
            serde_json::json!({ "key": "../escape", "bytes": [1] }),
        )
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::InvalidArguments);
    }

    #[test]
    fn every_mutating_method_requires_a_mutating_capability() {
        for (name, capability, _) in METHODS {
            let mutates = name.contains(".set")
                || name.ends_with(".enable")
                || name.ends_with(".disable")
                || name.ends_with(".rename");
            // Storage is the plugin's own, so it is excluded by design.
            if mutates && !name.starts_with("storage.") {
                let capability = capability
                    .unwrap_or_else(|| panic!("`{name}` mutates but needs no capability"));
                assert!(
                    capability.is_mutating(),
                    "`{name}` mutates but requires the non-mutating `{capability}`"
                );
            }
        }
    }

    fn dispatch_with(
        granted: CapabilitySet,
        method: &str,
        args: serde_json::Value,
    ) -> PluginResult<serde_json::Value> {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().to_path_buf());
        let plugin_id = PluginId::parse("acme.test").unwrap();
        let subscriptions = EventSubscriptions::new();
        let cx = DispatchContext {
            ctx: &ctx,
            plugin_id: &plugin_id,
            granted: &granted,
            declared_hosts: &[],
            settings_schema: &[],
            subscriptions: &subscriptions,
            ui: &NoopUiSink,
            bound_instance: None,
        };
        dispatch(&cx, method, &args)
    }

    #[test]
    fn reads_without_game_read_are_refused() {
        for method in ["game.readBytes", "game.readLines", "game.list"] {
            let error = dispatch_with(
                CapabilitySet::new(),
                method,
                serde_json::json!({ "instanceId": "i", "path": "a.txt" }),
            )
            .unwrap_err();
            assert_eq!(error.code, PluginErrorCode::CapabilityDenied, "{method}");
        }
    }

    #[test]
    fn writes_without_game_write_are_refused() {
        for method in ["game.writeBytes", "game.writeText"] {
            let error = dispatch_with(
                CapabilitySet::new(),
                method,
                serde_json::json!({ "instanceId": "i", "path": "a.txt", "text": "x" }),
            )
            .unwrap_err();
            assert_eq!(error.code, PluginErrorCode::CapabilityDenied, "{method}");
        }
    }

    #[test]
    fn game_read_does_not_grant_game_write() {
        let mut granted = CapabilitySet::new();
        granted.insert(Capability::GameRead);
        let error = dispatch_with(
            granted,
            "game.writeText",
            serde_json::json!({ "instanceId": "i", "path": "a.txt", "text": "x" }),
        )
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::CapabilityDenied);
    }

    /// A core context with one instance folder. Each script request is one
    /// `dispatch` call, exactly as the bridge makes them; the budgets are meant
    /// to span the whole plugin call, so they must survive from one request to
    /// the next.
    fn instance_ctx() -> (tempfile::TempDir, crate::ctx::CoreContext) {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().to_path_buf());
        std::fs::create_dir_all(ctx.paths.instance_dir("i").unwrap().join("writable")).unwrap();
        (tmp, ctx)
    }

    fn dispatch_in(
        ctx: &crate::ctx::CoreContext,
        granted: &CapabilitySet,
        method: &str,
        args: serde_json::Value,
    ) -> PluginResult<serde_json::Value> {
        let plugin_id = PluginId::parse("acme.test").unwrap();
        let subscriptions = EventSubscriptions::new();
        let cx = DispatchContext {
            ctx,
            plugin_id: &plugin_id,
            granted,
            declared_hosts: &[],
            settings_schema: &[],
            subscriptions: &subscriptions,
            ui: &NoopUiSink,
            bound_instance: None,
        };
        dispatch(&cx, method, &args)
    }

    #[test]
    fn one_plugin_call_cannot_create_more_files_than_its_budget() {
        let (_tmp, ctx) = instance_ctx();
        let mut granted = CapabilitySet::new();
        granted.insert(Capability::GameWrite);
        let budget = crate::game_script::DEFAULT_MAX_WRITE_FILES as usize;
        let mut refused = None;
        for n in 0..=budget {
            let result = dispatch_in(
                &ctx,
                &granted,
                "game.writeText",
                serde_json::json!({ "instanceId": "i", "path": format!("f{n}.txt"), "text": "x" }),
            );
            if let Err(error) = result {
                refused = Some((n, error));
                break;
            }
        }
        let (n, error) = refused.expect("file number budget+1 in one plugin call must be refused");
        assert_eq!(n, budget);
        assert_eq!(error.detail.as_deref(), Some("file_budget_spent"));
    }

    #[test]
    fn one_plugin_call_cannot_read_more_than_its_budget() {
        let (_tmp, ctx) = instance_ctx();
        let one_mib = 1024 * 1024;
        std::fs::write(
            ctx.paths
                .instance_dir("i")
                .unwrap()
                .join("writable")
                .join("big.bin"),
            vec![0u8; one_mib],
        )
        .unwrap();
        let mut granted = CapabilitySet::new();
        granted.insert(Capability::GameRead);
        let budget = crate::game_script::DEFAULT_MAX_TOTAL_READ_BYTES as usize;
        let reads = budget / one_mib;
        let mut refused = None;
        for n in 0..=reads {
            let result = dispatch_in(
                &ctx,
                &granted,
                "game.readBytes",
                serde_json::json!({ "instanceId": "i", "path": "big.bin", "offset": 0, "len": one_mib }),
            );
            if let Err(error) = result {
                refused = Some((n, error));
                break;
            }
        }
        let (n, error) = refused.expect("read number budget+1 in one plugin call must be refused");
        assert_eq!(n, reads);
        assert_eq!(error.detail.as_deref(), Some("budget_spent"));
    }

    #[test]
    fn subscribing_to_an_event_needs_the_capability_that_guards_its_data() {
        assert_eq!(
            capability_for_event("content.installed"),
            Some(Capability::ContentRead)
        );
        assert_eq!(
            capability_for_event("launch.started"),
            Some(Capability::LaunchRead)
        );
    }

    #[test]
    fn every_documented_event_has_a_guarding_capability() {
        for name in PluginEvent::NAMES {
            assert!(
                capability_for_event(name).is_some(),
                "`{name}` can be subscribed to with no capability at all"
            );
        }
    }
    #[test]
    fn a_hook_bound_to_one_instance_refuses_a_different_id() {
        let (_tmp, ctx) = instance_ctx();
        let mut granted = CapabilitySet::new();
        granted.insert(Capability::GameRead);
        let plugin_id = PluginId::parse("acme.test").unwrap();
        let subscriptions = EventSubscriptions::new();
        let cx = DispatchContext {
            ctx: &ctx,
            plugin_id: &plugin_id,
            granted: &granted,
            declared_hosts: &[],
            settings_schema: &[],
            subscriptions: &subscriptions,
            ui: &NoopUiSink,
            bound_instance: Some("i"),
        };
        let error = dispatch(
            &cx,
            "game.readBytes",
            &serde_json::json!({ "instanceId": "other", "path": "a.txt" }),
        )
        .unwrap_err();
        assert_eq!(error.code, PluginErrorCode::CapabilityDenied);
        assert!(error.message.contains("bound to instance `i`"));

        // The bound instance itself is not refused for being the wrong one.
        let error = dispatch(
            &cx,
            "game.readBytes",
            &serde_json::json!({ "instanceId": "i", "path": "missing.txt" }),
        )
        .unwrap_err();
        assert_ne!(error.code, PluginErrorCode::CapabilityDenied);
    }
}
