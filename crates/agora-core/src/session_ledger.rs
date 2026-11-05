//! The session ledger: what each launch ran with, how it ended, and the game-free verdict
//! (MASTER_SPEC §30.4, §30.6).
//!
//! History cannot be recorded backwards, so every launch of a game instance writes one row to
//! `launch_history` (the same table Minecraft's history uses) at the moment it starts:
//!
//! * a **manifest** of what ran: the deployment fingerprint, every content item with its key,
//!   version, hash, enabled state and order, a hash of the load order, the runtime, and a hash of
//!   the instance's settings;
//! * when it ends, the **exit observation** of the game's own process and the **verdict**.
//!
//! # Compact storage
//!
//! A manifest of a 368-mod setup is about 55 KB as JSON and about 17 KB deflated. It is stored once
//! per distinct manifest in `launch_manifests`, named by the hash of its canonical JSON, and
//! `launch_history` rows point at it. A hundred launches of an unchanged setup therefore cost one
//! blob, and a setup that changes at every launch costs about 17 KB a launch at the worst. Trimming
//! the history ([`crate::launch_history::prune_history`]) drops the manifests no remaining row
//! points at, and deleting the instance drops both (database triggers).
//!
//! # Never in the way
//!
//! Recording is best effort. Every entry point the launch path calls ([`record_launch`]) turns
//! its own failures, and panics, into "no session recorded": a launch is never blocked or failed
//! by its own bookkeeping.
//!
//! # Honest verdicts
//!
//! [`verdict`] never reports `Normal` unless the game's own process was seen to exit with one of
//! the game's declared normal codes, and never reports `Crashed` from a non-zero exit code alone.
//! A session no one watched to the end (Agora closed mid-session) is `ClosedUnknown`.

use std::io::{Read, Write};

use agora_game_api::{GameDefinition, StoreId};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::launch_history::{LaunchResult, HISTORY_LIMIT};

/// A session shorter than this that ended with a non-zero exit code probably never started.
pub const FAILED_TO_START_MS: u64 = 120_000;

/// How an artifact (a crash log) was tied to the session. Strongest first. This slice produces
/// only [`MatchedBy::ExitStatus`]; the evidence slices add the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// The game's exit code was an NTSTATUS failure code and no log was found.
    ExitStatus,
    /// The log names the process id Agora held.
    Pid,
    /// The log names this instance's working directory.
    WorkingDir,
    /// The log's time falls inside the session. Likely, not certain.
    Time,
}

/// What Agora saw of the game's end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitObservation {
    /// The game's own process was held, and this is the code it exited with.
    Code { pid: u32, image: String, code: u32 },
    /// The exit could not be observed, and why. Never a guess.
    Unavailable { reason: String },
}

impl ExitObservation {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

impl Default for ExitObservation {
    fn default() -> Self {
        Self::unavailable("the session was not watched")
    }
}

/// The game-free verdict on how a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionVerdict {
    Crashed { how: MatchedBy },
    FailedToStart,
    Froze,
    Stopped,
    Normal,
    ClosedUnknown,
}

impl SessionVerdict {
    /// How the legacy history column describes this verdict. Only `Crashed` counts as a crash and
    /// only `Normal` as ok; a start failure or an unknown ending claims neither.
    pub fn as_result(self) -> LaunchResult {
        match self {
            Self::Crashed { .. } => LaunchResult::Crashed,
            Self::Normal => LaunchResult::Ok,
            Self::Stopped => LaunchResult::Stopped,
            Self::FailedToStart | Self::Froze | Self::ClosedUnknown => LaunchResult::Unknown,
        }
    }
}

/// A crash artifact tied to the session. The evidence slices fill this in; the verdict already
/// gives it priority over the exit code, since a crash handler can exit 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactEvidence {
    pub bound_by: MatchedBy,
}

/// Everything the verdict is made from.
#[derive(Debug, Clone)]
pub struct VerdictInput<'a> {
    pub exit: &'a ExitObservation,
    pub stop_requested: bool,
    /// How long the game ran, in milliseconds.
    pub runtime_ms: u64,
    /// The game's declared normal exit codes (`[0]` when it declares none).
    pub normal_exit_codes: &'a [u32],
    /// A crash artifact bound to this session, when one was found.
    pub artifact: Option<ArtifactEvidence>,
    /// A thread dump was written during the session.
    pub froze: bool,
}

/// NTSTATUS codes that mean a program could not be loaded, which is a start failure, not a crash.
const LOAD_FAILURES: [u32; 6] = [
    0xC000_0135, // STATUS_DLL_NOT_FOUND
    0xC000_0138, // STATUS_ORDINAL_NOT_FOUND
    0xC000_0139, // STATUS_ENTRYPOINT_NOT_FOUND
    0xC000_007B, // STATUS_INVALID_IMAGE_FORMAT
    0xC000_0142, // STATUS_DLL_INIT_FAILED
    0xC000_0143, // STATUS_MISSING_SYSTEMROOT_FILE
];

/// `STATUS_CONTROL_C_EXIT`: a console closed or interrupted the process.
const CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Whether an exit code is an NTSTATUS failure (severity bits `11`), which Windows uses as the
/// exit code of a process that died of an unhandled exception.
pub fn is_ntstatus_failure(code: u32) -> bool {
    code >= 0xC000_0000
}

/// Judge how a session ended. Pure and game-free: games supply only `normal_exit_codes`.
///
/// Order (MASTER_SPEC §30.6): a bound crash artifact beats the exit code; then a freeze; then a
/// requested Stop (or Agora's own stop code); then the game's declared normal codes; then an
/// NTSTATUS failure. A non-zero code without an artifact is a start failure when the session was
/// short and otherwise unknown. It is never called a crash.
pub fn verdict(input: &VerdictInput<'_>) -> SessionVerdict {
    if let Some(artifact) = input.artifact {
        return SessionVerdict::Crashed {
            how: artifact.bound_by,
        };
    }
    if input.froze {
        return SessionVerdict::Froze;
    }
    let stop_code = matches!(
        input.exit,
        ExitObservation::Code { code, .. } if *code == crate::process_job::STOP_EXIT_CODE
    );
    if input.stop_requested || stop_code {
        return SessionVerdict::Stopped;
    }
    let ExitObservation::Code { code, .. } = input.exit else {
        return SessionVerdict::ClosedUnknown;
    };
    if input.normal_exit_codes.contains(code) {
        return SessionVerdict::Normal;
    }
    if LOAD_FAILURES.contains(code) {
        return SessionVerdict::FailedToStart;
    }
    if *code == CONTROL_C_EXIT {
        return SessionVerdict::ClosedUnknown;
    }
    if is_ntstatus_failure(*code) {
        return SessionVerdict::Crashed {
            how: MatchedBy::ExitStatus,
        };
    }
    if input.runtime_ms < FAILED_TO_START_MS {
        SessionVerdict::FailedToStart
    } else {
        SessionVerdict::ClosedUnknown
    }
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// One thing that was part of the launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestItem {
    /// What it is, stable across versions where the item says so: a catalog id, a provider id,
    /// a name, or `framework:<id>` / `generated:<tool>` / `folder:<path>`.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The content hash (a content item's canonical id, or a Minecraft file's SHA-256).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub enabled: bool,
    /// Position in the instance's layer order, lowest first.
    pub order: u32,
    /// Why the item could not be read. Recorded rather than dropped: an item that is missing from
    /// a manifest looks like an item that was never there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,
}

/// What a launch ran with.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaunchManifest {
    /// The deployment's fingerprint (`DeploymentPlan.fingerprint`), when the launch was deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_fingerprint: Option<String>,
    pub items: Vec<ManifestItem>,
    /// Hash of the active plugin load order. `None` when it could not be read (see `unreadable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_orders_hash: Option<String>,
    /// The runtime: `store:version`, or the Minecraft version and loader.
    pub runtime: String,
    /// Hash of the instance's own settings files. `None` when they could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_hash: Option<String>,
    /// Parts of the manifest that could not be read, in words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
}

impl LaunchManifest {
    /// Items that were switched on.
    pub fn enabled_count(&self) -> i64 {
        self.items.iter().filter(|item| item.enabled).count() as i64
    }

    /// The canonical JSON that names the manifest and is what gets compressed.
    fn canonical_json(&self) -> Vec<u8> {
        // Struct field order is fixed and there are no maps, so equal manifests serialise equally.
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// The SHA-256 of the canonical JSON, in hex.
    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.canonical_json()))
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn compress(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    encoder.write_all(raw)?;
    encoder.finish()
}

/// Largest manifest this build will inflate: a damaged or hostile row cannot balloon memory.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

fn decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(MAX_MANIFEST_BYTES)
        .read_to_end(&mut out)?;
    Ok(out)
}

/// A file or folder's content hash for the settings hash. Folders are walked in sorted order,
/// to a bounded size.
fn hash_path(root: &std::path::Path, hasher: &mut Sha256, label: &str) -> Result<(), String> {
    const MAX_FILES: usize = 5000;
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            hasher.update(format!("{label}\tabsent\n").as_bytes());
            return Ok(());
        }
        Err(e) => return Err(format!("{}: {e}", root.display())),
    };
    let mut files = Vec::new();
    if meta.is_file() {
        files.push((String::new(), root.to_path_buf()));
    } else if meta.is_dir() {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
                let path = entry.path();
                let kind = entry
                    .file_type()
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if kind.is_dir() {
                    stack.push(path);
                } else if kind.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push((rel, path));
                    if files.len() > MAX_FILES {
                        return Err(format!("{}: more than {MAX_FILES} files", root.display()));
                    }
                }
            }
        }
    } else {
        hasher.update(format!("{label}\tnot-a-file\n").as_bytes());
        return Ok(());
    }
    files.sort();
    for (rel, path) in files {
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        hasher.update(format!("{label}\t{rel}\t{}\n", sha_hex(&bytes)).as_bytes());
    }
    Ok(())
}

/// What a launch of a plugin-game instance runs with, read from the instance on disk.
///
/// Never fails: anything that cannot be read is recorded in the manifest as unreadable, and the
/// rest is still recorded.
pub fn build_manifest(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
) -> LaunchManifest {
    use agora_game_api::{BaseReference, LayerSource};

    let mut manifest = LaunchManifest::default();
    let instance = match crate::game_instance::get_manifest(ctx, instance_id) {
        Ok(instance) => instance,
        Err(e) => {
            manifest.unreadable.push(format!("instance manifest: {e}"));
            manifest.runtime = "unknown".to_string();
            return manifest;
        }
    };

    manifest.runtime = match &instance.base {
        BaseReference::Pinned { runtime, .. } => Some(runtime.clone()),
        BaseReference::Unpinned { .. } => instance.runtime_identity.clone(),
    }
    .map(|runtime| match runtime.build {
        Some(build) => format!("{}:{}+{build}", runtime.store, runtime.version),
        None => format!("{}:{}", runtime.store, runtime.version),
    })
    .unwrap_or_else(|| "unknown".to_string());

    // The fingerprint is read from the deployment record rather than planned again.
    manifest.plan_fingerprint = crate::game_deploy::deployed_fingerprint(ctx, instance_id);

    let ids = crate::content_store::item_ids(ctx);
    if let Err(e) = &ids {
        manifest.unreadable.push(format!("content store: {e}"));
    }
    for (index, layer) in instance.layers.layers().iter().enumerate() {
        let order = index as u32;
        match &layer.source {
            LayerSource::Content { content } => {
                let read = match &ids {
                    Ok(ids) => crate::content_store::get_item_indexed(ctx, ids, content)
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                manifest.items.push(match read {
                    Ok(item) => ManifestItem {
                        key: item
                            .catalog_id
                            .clone()
                            .filter(|id| !id.is_empty())
                            .or_else(|| {
                                item.provider
                                    .as_ref()
                                    .zip(item.provider_id.as_ref())
                                    .map(|(provider, id)| format!("{provider}:{id}"))
                            })
                            .unwrap_or_else(|| item.name.clone()),
                        version: item.version.clone(),
                        hash: Some(item.item_id.clone()),
                        enabled: layer.enabled,
                        order,
                        unreadable: None,
                    },
                    Err(reason) => ManifestItem {
                        key: layer.id.as_str().to_string(),
                        version: None,
                        hash: Some(content.clone()),
                        enabled: layer.enabled,
                        order,
                        unreadable: Some(reason),
                    },
                });
            }
            LayerSource::InstanceContent { path, .. } => manifest.items.push(ManifestItem {
                key: format!("folder:{}", path.as_str()),
                version: None,
                hash: None,
                enabled: layer.enabled,
                order,
                unreadable: None,
            }),
            LayerSource::Generated {
                tool,
                generation,
                inputs,
            } => manifest.items.push(ManifestItem {
                key: format!("generated:{}", tool.as_str()),
                version: Some(generation.clone()),
                hash: match inputs {
                    agora_game_api::InputFingerprint::Known(value) => Some(value.clone()),
                    agora_game_api::InputFingerprint::Unknown => None,
                },
                enabled: layer.enabled,
                order,
                unreadable: None,
            }),
            // The base is the `runtime`; the writable and staging layers are the game's own output.
            LayerSource::Base { .. }
            | LayerSource::Writable { .. }
            | LayerSource::Staging { .. } => {}
        }
    }
    for framework in &instance.frameworks {
        let order = manifest.items.len() as u32;
        manifest.items.push(ManifestItem {
            key: format!("framework:{}", framework.id.as_str()),
            version: Some(framework.version.clone()),
            hash: None,
            enabled: true,
            order,
            unreadable: None,
        });
    }

    // Load order.
    if definition.plugin_list.is_some() {
        match crate::game_plugins::list(ctx, instance_id, definition) {
            Ok(list) => {
                let mut hasher = Sha256::new();
                for entry in &list.entries {
                    hasher.update(format!("{}\t{}\n", entry.name, entry.active).as_bytes());
                }
                manifest.load_orders_hash = Some(hex::encode(hasher.finalize()));
            }
            Err(e) => manifest.unreadable.push(format!("load order: {e}")),
        }
    } else {
        manifest.load_orders_hash = Some(sha_hex(b""));
    }

    // Settings: the instance's own copies of the game's per-user files.
    match ctx.paths.instance_dir(instance_id) {
        Ok(dir) => {
            let mut hasher = Sha256::new();
            let mut failed = false;
            for mapping in definition
                .user_files
                .iter()
                .filter(|mapping| mapping.applies_to_store(store))
            {
                let path = crate::game_ini::join_rel(&dir, mapping.instance_path.as_str());
                if let Err(e) = hash_path(&path, &mut hasher, mapping.instance_path.as_str()) {
                    manifest.unreadable.push(format!("settings: {e}"));
                    failed = true;
                }
            }
            if !failed {
                manifest.settings_hash = Some(hex::encode(hasher.finalize()));
            }
        }
        Err(e) => manifest.unreadable.push(format!("settings: {e}")),
    }
    manifest
}

/// What a Minecraft launch runs with, from the instance manifest. Minecraft has no deployment
/// fingerprint (its files are read in place), so `plan_fingerprint` stays empty; the files'
/// SHA-256 digests are in the items.
pub fn minecraft_manifest(instance: &crate::models::InstanceManifest) -> LaunchManifest {
    let mut manifest = LaunchManifest {
        runtime: format!(
            "minecraft:{} {} {}",
            instance.minecraft_version, instance.loader, instance.loader_version
        ),
        load_orders_hash: Some(sha_hex(b"")),
        settings_hash: None,
        ..LaunchManifest::default()
    };
    manifest
        .unreadable
        .push("settings: not recorded for Minecraft".to_string());
    let groups: [(&str, &[crate::models::InstalledMod]); 3] = [
        ("", &instance.mods),
        ("resourcepack:", &instance.resourcepacks),
        ("shader:", &instance.shaders),
    ];
    for (prefix, entries) in groups {
        for entry in entries {
            let name = entry
                .mod_jar_id
                .clone()
                .or_else(|| entry.registry_id.clone())
                .or_else(|| entry.modrinth_id.clone())
                .unwrap_or_else(|| entry.filename.clone());
            let order = manifest.items.len() as u32;
            manifest.items.push(ManifestItem {
                key: format!("{prefix}{name}"),
                version: entry.version.clone(),
                hash: Some(entry.sha256.clone()).filter(|hash| !hash.is_empty()),
                enabled: entry.enabled,
                order,
                unreadable: None,
            });
        }
    }
    manifest
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Which running copy of Agora is watching a session. A session whose owner is gone was never
/// watched to its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    /// The process's start time, which tells a live owner from a recycled pid.
    pub started: u64,
}

impl Owner {
    /// This process.
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            pid,
            started: start_time_of(pid).unwrap_or(0),
        }
    }

    /// Whether the same process is still running.
    pub fn is_alive(pid: u32, started: u64) -> bool {
        start_time_of(pid).is_some_and(|now| now == started)
    }
}

fn start_time_of(pid: u32) -> Option<u64> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_process(pid);
    system.process(pid).map(|process| process.start_time())
}

/// One recorded session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub instance_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// `None` while the session is still open.
    pub exit: Option<ExitObservation>,
    pub stop_requested: bool,
    pub manifest: Option<LaunchManifest>,
    pub verdict: Option<SessionVerdict>,
}

/// Store a manifest (once per distinct manifest) and return its hash.
fn store_manifest(
    conn: &Connection,
    instance_id: &str,
    manifest: &LaunchManifest,
) -> anyhow::Result<String> {
    let raw = manifest.canonical_json();
    let hash = sha_hex(&raw);
    let data = compress(&raw)?;
    conn.execute(
        "INSERT OR IGNORE INTO launch_manifests (instance_id, hash, data, raw_len)
         VALUES (?1, ?2, ?3, ?4)",
        params![instance_id, hash, data, raw.len() as i64],
    )?;
    Ok(hash)
}

/// The stored manifest with this hash.
pub fn read_manifest(
    conn: &Connection,
    instance_id: &str,
    hash: &str,
) -> anyhow::Result<Option<LaunchManifest>> {
    let data: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM launch_manifests WHERE instance_id = ?1 AND hash = ?2",
            params![instance_id, hash],
            |row| row.get(0),
        )
        .optional()?;
    data.map(|data| Ok(serde_json::from_slice(&decompress(&data)?)?))
        .transpose()
}

/// Open a session row with its manifest. Returns the session id.
pub fn begin(
    conn: &Connection,
    instance_id: &str,
    started_at: &str,
    manifest: &LaunchManifest,
    owner: Owner,
) -> anyhow::Result<String> {
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let tx = conn.unchecked_transaction()?;
    let hash = store_manifest(&tx, instance_id, manifest)?;
    tx.execute(
        "INSERT INTO launch_history
             (instance_id, started_at, enabled_mod_count, session_id, manifest_hash,
              owner_pid, owner_start)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            instance_id,
            started_at,
            manifest.enabled_count(),
            session_id,
            hash,
            owner.pid,
            owner.started as i64
        ],
    )?;
    tx.commit()?;
    Ok(session_id)
}

/// Attach a manifest to a history row that was opened without one (Minecraft's).
pub fn attach_manifest(
    conn: &Connection,
    row_id: i64,
    instance_id: &str,
    manifest: &LaunchManifest,
) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let hash = store_manifest(&tx, instance_id, manifest)?;
    tx.execute(
        "UPDATE launch_history SET manifest_hash = ?2 WHERE id = ?1",
        params![row_id, hash],
    )?;
    tx.commit()?;
    Ok(())
}

/// Record how a history row's exit looked, without judging it. Minecraft keeps its own outcome.
pub fn note_exit(conn: &Connection, row_id: i64, exit: &ExitObservation) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE launch_history SET exit_json = ?2 WHERE id = ?1",
        params![row_id, serde_json::to_string(exit)?],
    )?;
    Ok(())
}

/// Note that the player pressed Stop: every open session of the instance is flagged. Called before
/// the processes are ended, so the flag is there when the watcher sees them go.
pub fn mark_stop_requested(conn: &Connection, instance_id: &str) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE launch_history SET stop_requested = 1
          WHERE instance_id = ?1 AND session_id IS NOT NULL AND ended_at IS NULL",
        [instance_id],
    )?)
}

/// What a finished session needs to be judged.
#[derive(Debug, Clone)]
pub struct SessionFinish {
    pub ended_at: String,
    /// Wall-clock length of the whole session, in milliseconds.
    pub duration_ms: i64,
    pub exit: ExitObservation,
    /// How long the game itself ran, when its exit was observed.
    pub game_runtime_ms: Option<u64>,
    pub normal_exit_codes: Vec<u32>,
    pub artifact: Option<ArtifactEvidence>,
    pub froze: bool,
}

/// Close a session: judge it, and store the exit and the verdict. An already-closed session is
/// left alone, so two watchers cannot overwrite each other. Returns the verdict, or `None` when
/// there was no open session with this id.
pub fn finish(
    conn: &Connection,
    session_id: &str,
    done: &SessionFinish,
) -> anyhow::Result<Option<SessionVerdict>> {
    let open: Option<(i64, String, bool)> = conn
        .query_row(
            "SELECT id, instance_id, stop_requested FROM launch_history
              WHERE session_id = ?1 AND ended_at IS NULL",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
        )
        .optional()?;
    let Some((row_id, instance_id, stop_requested)) = open else {
        return Ok(None);
    };
    let judged = verdict(&VerdictInput {
        exit: &done.exit,
        stop_requested,
        runtime_ms: done
            .game_runtime_ms
            .unwrap_or_else(|| done.duration_ms.max(0) as u64),
        normal_exit_codes: &done.normal_exit_codes,
        artifact: done.artifact,
        froze: done.froze,
    });
    write_end(
        conn,
        row_id,
        &done.ended_at,
        Some(done.duration_ms),
        &done.exit,
        judged,
    )?;
    // The history is trimmed with its manifests; a failure to trim is not a failure to record.
    let _ = crate::launch_history::prune_history(conn, &instance_id);
    Ok(Some(judged))
}

fn write_end(
    conn: &Connection,
    row_id: i64,
    ended_at: &str,
    duration_ms: Option<i64>,
    exit: &ExitObservation,
    judged: SessionVerdict,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE launch_history
            SET ended_at = ?2, duration_ms = ?3, exit_json = ?4, verdict = ?5, outcome = ?6
          WHERE id = ?1 AND ended_at IS NULL",
        params![
            row_id,
            ended_at,
            duration_ms,
            serde_json::to_string(exit)?,
            serde_json::to_string(&judged)?,
            judged.as_result().as_str(),
        ],
    )?;
    Ok(())
}

/// Close every open session whose owner is gone, as `ClosedUnknown` (or `Stopped`, when the player
/// had pressed Stop). Agora closing mid-session leaves a row nobody will finish; calling it
/// "normal" would be a false claim, so it says what is true: nothing watched how it ended.
/// `owner_alive` decides whether the owner still runs; [`Owner::is_alive`] is the real check.
pub fn close_orphans(
    conn: &Connection,
    now: &str,
    owner_alive: &dyn Fn(u32, u64) -> bool,
) -> anyhow::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, owner_pid, owner_start, stop_requested FROM launch_history
          WHERE session_id IS NOT NULL AND ended_at IS NULL",
    )?;
    let open: Vec<(i64, Option<u32>, Option<i64>, bool)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut closed = 0;
    for (row_id, pid, started, stop_requested) in open {
        if let (Some(pid), Some(started)) = (pid, started) {
            if owner_alive(pid, started as u64) {
                continue;
            }
        }
        let exit = ExitObservation::unavailable(
            "Agora closed while the game was running, so nothing watched how it ended",
        );
        let judged = verdict(&VerdictInput {
            exit: &exit,
            stop_requested,
            runtime_ms: 0,
            normal_exit_codes: &[],
            artifact: None,
            froze: false,
        });
        write_end(conn, row_id, now, None, &exit, judged)?;
        closed += 1;
    }
    Ok(closed)
}

fn parse_row(conn: &Connection, row: RawRow) -> SessionRecord {
    let (session_id, instance_id, started_at, ended_at, exit_json, stop, verdict_json, hash) = row;
    SessionRecord {
        manifest: hash
            .as_deref()
            .and_then(|hash| read_manifest(conn, &instance_id, hash).ok().flatten()),
        session_id,
        instance_id,
        started_at,
        ended_at,
        exit: exit_json.and_then(|json| serde_json::from_str(&json).ok()),
        stop_requested: stop != 0,
        verdict: verdict_json.and_then(|json| serde_json::from_str(&json).ok()),
    }
}

type RawRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
);

const SESSION_COLUMNS: &str = "session_id, instance_id, started_at, ended_at, exit_json,
                               stop_requested, verdict, manifest_hash";

fn raw_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

/// One session by id.
pub fn get(conn: &Connection, session_id: &str) -> anyhow::Result<Option<SessionRecord>> {
    let row = conn
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM launch_history WHERE session_id = ?1"),
            [session_id],
            raw_row,
        )
        .optional()?;
    Ok(row.map(|row| parse_row(conn, row)))
}

/// An instance's recorded sessions, newest first. Rows from before the ledger have no session id
/// and are not listed.
pub fn list(
    conn: &Connection,
    instance_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<SessionRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SESSION_COLUMNS} FROM launch_history
          WHERE instance_id = ?1 AND session_id IS NOT NULL
          ORDER BY started_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows: Vec<RawRow> = stmt
        .query_map(params![instance_id, limit as i64], raw_row)?
        .collect::<Result<_, _>>()?;
    Ok(rows.into_iter().map(|row| parse_row(conn, row)).collect())
}

/// Size of the stored manifests of an instance: (distinct manifests, compressed bytes, raw bytes).
pub fn manifest_footprint(conn: &Connection, instance_id: &str) -> anyhow::Result<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(data)), 0), COALESCE(SUM(raw_len), 0)
           FROM launch_manifests WHERE instance_id = ?1",
        [instance_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

/// The most history rows kept per instance; the manifests go with them.
pub const KEPT_SESSIONS: usize = HISTORY_LIMIT;

// ---------------------------------------------------------------------------
// The launch path's entry points
// ---------------------------------------------------------------------------

/// Open a session for a launch that is about to be reported as started. Returns the session id, or
/// `None` when nothing could be recorded. Never fails and never panics: a launch is not blocked by
/// its own bookkeeping. Sessions whose owner is gone are closed first.
pub fn record_launch(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
) -> Option<String> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
        let now = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
        let _ = close_orphans(&conn, &now, &Owner::is_alive);
        let manifest = build_manifest(ctx, instance_id, definition, store);
        begin(&conn, instance_id, &now, &manifest, Owner::current()).ok()
    }));
    attempt.ok().flatten()
}

/// Close a session from what the watcher saw. Best effort, like [`record_launch`].
pub fn record_end(
    ctx: &Ctx,
    session_id: &str,
    definition: &GameDefinition,
    report: &crate::game_launch::SessionExitReport,
    duration_ms: i64,
) -> Option<SessionVerdict> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
        let done = SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
            duration_ms,
            exit: report.exit.clone(),
            game_runtime_ms: report.game_runtime_ms,
            normal_exit_codes: definition.session.normal_codes(),
            artifact: None,
            froze: false,
        };
        finish(&conn, session_id, &done).ok().flatten()
    }));
    attempt.ok().flatten()
}

/// Flag the player's Stop on the instance's open sessions. Best effort.
pub fn record_stop(ctx: &Ctx, instance_id: &str) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(conn) = crate::db::local_state_connection(&ctx.paths.local_state_db()) {
            let _ = mark_stop_requested(&conn, instance_id);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: [u32; 1] = [0];

    fn code(code: u32) -> ExitObservation {
        ExitObservation::Code {
            pid: 1,
            image: "game.exe".into(),
            code,
        }
    }

    fn judge(exit: &ExitObservation, stop: bool, runtime_ms: u64) -> SessionVerdict {
        verdict(&VerdictInput {
            exit,
            stop_requested: stop,
            runtime_ms,
            normal_exit_codes: &NORMAL,
            artifact: None,
            froze: false,
        })
    }

    #[test]
    fn a_normal_quit_is_normal() {
        assert_eq!(judge(&code(0), false, 600_000), SessionVerdict::Normal);
    }

    #[test]
    fn a_stop_beats_a_code_that_looks_like_a_crash() {
        for looks_like in [1, 0xC000_0005, 0xC000_0409, 0] {
            assert_eq!(
                judge(&code(looks_like), true, 600_000),
                SessionVerdict::Stopped,
                "code {looks_like:#x}"
            );
        }
        // Even an unobserved exit, and even a short session.
        assert_eq!(
            judge(&ExitObservation::unavailable("x"), true, 10),
            SessionVerdict::Stopped
        );
    }

    #[test]
    fn agoras_stop_code_is_a_stop_without_the_flag() {
        assert_eq!(
            judge(&code(crate::process_job::STOP_EXIT_CODE), false, 600_000),
            SessionVerdict::Stopped
        );
    }

    #[test]
    fn exit_one_is_never_a_crash_on_its_own() {
        assert_eq!(
            judge(&code(1), false, 600_000),
            SessionVerdict::ClosedUnknown
        );
        assert_eq!(judge(&code(1), false, 5_000), SessionVerdict::FailedToStart);
        assert_eq!(
            judge(&code(255), false, 600_000),
            SessionVerdict::ClosedUnknown
        );
    }

    #[test]
    fn an_ntstatus_failure_is_a_crash_with_no_log() {
        for status in [0xC000_0005, 0xC000_0409, 0xC000_0374, 0xE06D_7363] {
            assert_eq!(
                judge(&code(status), false, 600_000),
                SessionVerdict::Crashed {
                    how: MatchedBy::ExitStatus
                },
                "{status:#x}"
            );
        }
    }

    #[test]
    fn load_failures_and_ctrl_c_are_not_crashes() {
        assert_eq!(
            judge(&code(0xC000_0135), false, 100),
            SessionVerdict::FailedToStart
        );
        assert_eq!(
            judge(&code(0xC000_0142), false, 600_000),
            SessionVerdict::FailedToStart
        );
        assert_eq!(
            judge(&code(CONTROL_C_EXIT), false, 600_000),
            SessionVerdict::ClosedUnknown
        );
    }

    #[test]
    fn an_unobserved_exit_is_unknown_never_normal() {
        let unseen = ExitObservation::unavailable("the game was never seen");
        assert_eq!(
            judge(&unseen, false, 600_000),
            SessionVerdict::ClosedUnknown
        );
        assert_eq!(judge(&unseen, false, 1), SessionVerdict::ClosedUnknown);
        assert_eq!(
            judge(&ExitObservation::default(), false, 600_000),
            SessionVerdict::ClosedUnknown
        );
    }

    #[test]
    fn normal_codes_are_the_games_data() {
        let exit = code(3);
        let declared = [0, 3];
        let input = |codes: &'static [u32]| VerdictInput {
            exit: Box::leak(Box::new(exit.clone())),
            stop_requested: false,
            runtime_ms: 600_000,
            normal_exit_codes: codes,
            artifact: None,
            froze: false,
        };
        assert_eq!(verdict(&input(&[0])), SessionVerdict::ClosedUnknown);
        assert_eq!(
            verdict(&input(Box::leak(Box::new(declared)))),
            SessionVerdict::Normal
        );
        // A game that declares an NTSTATUS as its normal code gets it, ahead of the crash rule.
        let odd = code(0xC000_0005);
        assert_eq!(
            verdict(&VerdictInput {
                exit: &odd,
                stop_requested: false,
                runtime_ms: 600_000,
                normal_exit_codes: &[0xC000_0005],
                artifact: None,
                froze: false,
            }),
            SessionVerdict::Normal
        );
    }

    #[test]
    fn a_bound_artifact_beats_the_exit_code() {
        // Network Script Framework exits 0 after writing its crash log.
        let exit = code(0);
        for bound_by in [MatchedBy::Pid, MatchedBy::Time] {
            assert_eq!(
                verdict(&VerdictInput {
                    exit: &exit,
                    stop_requested: false,
                    runtime_ms: 600_000,
                    normal_exit_codes: &NORMAL,
                    artifact: Some(ArtifactEvidence { bound_by }),
                    froze: false,
                }),
                SessionVerdict::Crashed { how: bound_by }
            );
        }
    }

    #[test]
    fn a_freeze_is_a_freeze() {
        let exit = code(crate::process_job::STOP_EXIT_CODE);
        assert_eq!(
            verdict(&VerdictInput {
                exit: &exit,
                stop_requested: true,
                runtime_ms: 600_000,
                normal_exit_codes: &NORMAL,
                artifact: None,
                froze: true,
            }),
            SessionVerdict::Froze
        );
    }

    // -- storage ------------------------------------------------------------------------------

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn
    }

    fn owner(pid: u32) -> Owner {
        Owner { pid, started: 7 }
    }

    /// A manifest the size and shape of a 368-mod Skyrim setup.
    fn big_manifest(changed: usize) -> LaunchManifest {
        let items = (0..368)
            .map(|i| ManifestItem {
                key: format!("catalog:author-mod-{i:03}"),
                version: Some(format!("{}.{}.{}", i % 7, i % 13, (i + changed) % 5)),
                hash: Some(sha_hex(
                    format!("content {i} {}", usize::from(i == changed)).as_bytes(),
                )),
                enabled: i % 9 != 0,
                order: i as u32,
                unreadable: None,
            })
            .collect();
        LaunchManifest {
            plan_fingerprint: Some(sha_hex(format!("plan {changed}").as_bytes())),
            items,
            load_orders_hash: Some(sha_hex(b"load order")),
            runtime: "steam:1.6.1170.0".into(),
            settings_hash: Some(sha_hex(b"settings")),
            unreadable: vec![],
        }
    }

    fn finish_with(code: ExitObservation) -> SessionFinish {
        SessionFinish {
            ended_at: "2026-01-01T01:00:00Z".into(),
            duration_ms: 3_600_000,
            exit: code,
            game_runtime_ms: Some(3_600_000),
            normal_exit_codes: vec![0],
            artifact: None,
            froze: false,
        }
    }

    #[test]
    fn a_368_mod_manifest_is_small_and_an_unchanged_setup_is_stored_once() {
        let conn = db();
        let manifest = big_manifest(0);
        let raw = manifest.canonical_json().len();
        for i in 0..HISTORY_LIMIT {
            begin(
                &conn,
                "skyrim",
                &format!("2026-01-01T00:{:02}:00Z", i % 60),
                &manifest,
                owner(1),
            )
            .unwrap();
        }
        let (count, compressed, stored_raw) = manifest_footprint(&conn, "skyrim").unwrap();
        println!(
            "368 items: {raw} bytes as JSON, {compressed} bytes stored, {HISTORY_LIMIT} launches"
        );
        assert_eq!(
            count, 1,
            "an unchanged setup is one blob, not one per launch"
        );
        assert_eq!(stored_raw as usize, raw);
        assert!(
            compressed < 30_000,
            "{compressed} bytes for one 368-mod manifest"
        );
    }

    #[test]
    fn a_setup_that_changes_every_launch_stays_bounded_by_the_history_limit() {
        let conn = db();
        for i in 0..(HISTORY_LIMIT + 25) {
            begin(
                &conn,
                "skyrim",
                &format!("2026-01-01T{:02}:{:02}:00Z", i / 60, i % 60),
                &big_manifest(i % 368),
                owner(1),
            )
            .unwrap();
            crate::launch_history::prune_history(&conn, "skyrim").unwrap();
        }
        let (count, compressed, _) = manifest_footprint(&conn, "skyrim").unwrap();
        println!("{count} distinct manifests, {compressed} bytes at the worst");
        assert_eq!(
            count as usize, HISTORY_LIMIT,
            "manifests are trimmed with their rows"
        );
        assert!(compressed < 3_000_000);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM launch_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows as usize, HISTORY_LIMIT);
    }

    #[test]
    fn a_manifest_round_trips_with_an_unreadable_item_kept() {
        let conn = db();
        let mut manifest = big_manifest(0);
        manifest.items[3].unreadable = Some("not a valid item record".into());
        manifest.unreadable.push("settings: denied".into());
        manifest.settings_hash = None;
        let id = begin(&conn, "skyrim", "2026-01-01T00:00:00Z", &manifest, owner(1)).unwrap();
        let record = get(&conn, &id).unwrap().unwrap();
        let back = record.manifest.expect("the manifest is stored");
        assert_eq!(back, manifest);
        assert_eq!(
            back.items.len(),
            368,
            "the unreadable item is kept, not dropped"
        );
        assert!(back.items[3].unreadable.is_some());
        assert!(record.exit.is_none() && record.verdict.is_none());
    }

    #[test]
    fn an_empty_mod_list_is_a_manifest_too() {
        let conn = db();
        let manifest = LaunchManifest {
            runtime: "steam:1".into(),
            load_orders_hash: Some(sha_hex(b"")),
            settings_hash: Some(sha_hex(b"")),
            ..LaunchManifest::default()
        };
        let id = begin(&conn, "skyrim", "2026-01-01T00:00:00Z", &manifest, owner(1)).unwrap();
        let back = get(&conn, &id).unwrap().unwrap().manifest.unwrap();
        assert!(back.items.is_empty());
        assert_eq!(back.enabled_count(), 0);
    }

    #[test]
    fn a_session_is_judged_when_it_finishes_and_only_once() {
        let conn = db();
        let id = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(1),
        )
        .unwrap();
        let first = finish(&conn, &id, &finish_with(code(0))).unwrap();
        assert_eq!(first, Some(SessionVerdict::Normal));
        // A second watcher, or a late orphan sweep, cannot rewrite it.
        assert_eq!(
            finish(&conn, &id, &finish_with(code(0xC000_0005))).unwrap(),
            None
        );
        let record = get(&conn, &id).unwrap().unwrap();
        assert_eq!(record.verdict, Some(SessionVerdict::Normal));
        assert_eq!(record.exit, Some(code(0)));
        assert_eq!(
            close_orphans(&conn, "2026-01-02T00:00:00Z", &|_, _| false).unwrap(),
            0
        );
    }

    #[test]
    fn a_stop_requested_in_the_ledger_beats_a_crash_looking_code() {
        let conn = db();
        let other = begin(
            &conn,
            "fallout",
            "2026-01-01T00:00:00Z",
            &big_manifest(1),
            owner(1),
        )
        .unwrap();
        for (n, code_seen) in [1u32, 0xC000_0005].into_iter().enumerate() {
            let id = begin(
                &conn,
                "skyrim",
                &format!("2026-01-01T00:0{n}:00Z"),
                &big_manifest(0),
                owner(1),
            )
            .unwrap();
            assert_eq!(mark_stop_requested(&conn, "skyrim").unwrap(), 1);
            let judged = finish(&conn, &id, &finish_with(code(code_seen))).unwrap();
            assert_eq!(judged, Some(SessionVerdict::Stopped), "code {code_seen:#x}");
            assert!(get(&conn, &id).unwrap().unwrap().stop_requested);
        }
        // The other instance was not flagged, and its exit 1 stays unknown.
        assert!(!get(&conn, &other).unwrap().unwrap().stop_requested);
        assert_eq!(
            finish(&conn, &other, &finish_with(code(1))).unwrap(),
            Some(SessionVerdict::ClosedUnknown)
        );
    }

    #[test]
    fn a_session_agora_never_saw_end_is_unknown_never_normal() {
        let conn = db();
        let orphan = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        let watched = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:10:00Z",
            &big_manifest(0),
            owner(222),
        )
        .unwrap();
        let flagged = begin(
            &conn,
            "fallout",
            "2026-01-01T00:20:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        mark_stop_requested(&conn, "fallout").unwrap();
        // A Minecraft row opened the old way has no session id and is not ours to close.
        let old = crate::launch_history::begin_launch(
            &conn,
            "mc",
            "2026-01-01T00:00:00Z",
            3,
            "1.21",
            "fabric",
        )
        .unwrap();

        // Agora (pid 111) closed and was reopened; pid 222 is still running.
        let closed = close_orphans(&conn, "2026-01-01T02:00:00Z", &|pid, _| pid == 222).unwrap();
        assert_eq!(closed, 2);

        let orphan = get(&conn, &orphan).unwrap().unwrap();
        assert_eq!(orphan.verdict, Some(SessionVerdict::ClosedUnknown));
        assert!(matches!(
            orphan.exit,
            Some(ExitObservation::Unavailable { .. })
        ));
        assert_ne!(orphan.verdict, Some(SessionVerdict::Normal));
        // The player did press Stop on this one, so that much is known.
        assert_eq!(
            get(&conn, &flagged).unwrap().unwrap().verdict,
            Some(SessionVerdict::Stopped)
        );
        // Another running Agora is still watching its own session.
        assert!(get(&conn, &watched).unwrap().unwrap().ended_at.is_none());
        let still_open: Option<String> = conn
            .query_row(
                "SELECT outcome FROM launch_history WHERE id = ?1",
                [old],
                |r| r.get(0),
            )
            .unwrap();
        assert!(still_open.is_none());
    }

    #[test]
    fn a_recycled_owner_pid_is_not_a_live_owner() {
        let me = Owner::current();
        assert!(Owner::is_alive(me.pid, me.started));
        assert!(
            !Owner::is_alive(me.pid, me.started + 1000),
            "same pid, different process"
        );
        assert!(!Owner::is_alive(u32::MAX - 1, 5));
    }

    #[test]
    fn deleting_an_instance_deletes_its_sessions_and_manifests() {
        let conn = db();
        conn.execute(
            "INSERT INTO user_instances
                (instance_id, name, minecraft_version, loader, loader_version, created_at)
             VALUES ('mc', 'Mc', '1.21', 'fabric', '0.15.0', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO game_instances (instance_id, game, name, base_json)
             VALUES ('skyrim', 'skyrim-se', 'Skyrim', '{}')",
            [],
        )
        .unwrap();
        begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(1),
        )
        .unwrap();
        begin(
            &conn,
            "mc",
            "2026-01-01T00:00:00Z",
            &big_manifest(1),
            owner(1),
        )
        .unwrap();
        begin(
            &conn,
            "kept",
            "2026-01-01T00:00:00Z",
            &big_manifest(2),
            owner(1),
        )
        .unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };

        conn.execute(
            "DELETE FROM game_instances WHERE instance_id = 'skyrim'",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM user_instances WHERE instance_id = 'mc'", [])
            .unwrap();
        assert_eq!(
            count("SELECT COUNT(*) FROM launch_history WHERE instance_id IN ('skyrim','mc')"),
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM launch_manifests WHERE instance_id IN ('skyrim','mc')"),
            0
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM launch_history WHERE instance_id = 'kept'"),
            1
        );
    }

    #[test]
    fn minecraft_history_rows_still_work_on_the_rebuilt_table() {
        let conn = db();
        conn.execute(
            "INSERT INTO user_instances
                (instance_id, name, minecraft_version, loader, loader_version, created_at)
             VALUES ('mc', 'Mc', '1.21', 'fabric', '0.15.0', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let row = crate::launch_history::begin_launch(
            &conn,
            "mc",
            "2026-01-01T00:00:00Z",
            4,
            "1.21",
            "fabric",
        )
        .unwrap();
        crate::launch_history::finish_launch(
            &conn,
            row,
            Some(10),
            Some(60_000),
            LaunchResult::Ok,
            None,
        )
        .unwrap();
        let rows = crate::launch_history::list_history(&conn, "mc", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, Some(LaunchResult::Ok));
        assert_eq!(
            crate::launch_history::total_play_ms(&conn, "mc").unwrap(),
            60_000
        );
        attach_manifest(&conn, row, "mc", &big_manifest(0)).unwrap();
        note_exit(&conn, row, &ExitObservation::unavailable("outcome only")).unwrap();
    }

    #[test]
    fn only_crashed_counts_as_a_crash_and_only_normal_as_ok() {
        use SessionVerdict::*;
        assert_eq!(
            Crashed {
                how: MatchedBy::Pid
            }
            .as_result(),
            LaunchResult::Crashed
        );
        assert_eq!(Normal.as_result(), LaunchResult::Ok);
        assert_eq!(Stopped.as_result(), LaunchResult::Stopped);
        for other in [FailedToStart, Froze, ClosedUnknown] {
            assert_eq!(other.as_result(), LaunchResult::Unknown);
        }
    }
}
