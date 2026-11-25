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

pub use agora_game_api::crash::{ExitObservation, MatchedBy, SessionVerdict};
use agora_game_api::{GameDefinition, StoreId};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::launch_history::{LaunchResult, HISTORY_LIMIT};

/// A session shorter than this that ended with a non-zero exit code probably never started.
pub const FAILED_TO_START_MS: u64 = 120_000;

impl From<SessionVerdict> for LaunchResult {
    /// How the legacy history column describes a verdict. Only `Crashed` counts as a crash and only
    /// `Normal` as ok; a start failure or an unknown ending claims neither.
    fn from(verdict: SessionVerdict) -> Self {
        match verdict {
            SessionVerdict::Crashed { .. } => Self::Crashed,
            SessionVerdict::Normal => Self::Ok,
            SessionVerdict::Stopped => Self::Stopped,
            SessionVerdict::FailedToStart
            | SessionVerdict::Froze
            | SessionVerdict::ClosedUnknown => Self::Unknown,
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
    /// The named entries of each load order, so a difference can name the plugin that changed.
    /// Empty in manifests stored before this was kept, which fall back to a generic load-order
    /// change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub load_order: Vec<LoadOrderEntry>,
    /// The runtime: `store:version`, or the Minecraft version and loader.
    pub runtime: String,
    /// Hash of the instance's own settings files. `None` when they could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_hash: Option<String>,
    /// The keys the instance's INI files set, recorded so a later settings change can name the key
    /// and both values instead of saying only "the settings changed". Empty in manifests stored
    /// before this was kept, which fall back to the generic change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingEntry>,
    /// Parts of the manifest that could not be read, in words.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    /// File-to-owner layers as they were when the session ran, so a historical crash can be
    /// attributed to that setup. Empty in manifests recorded before this was kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_layers: Vec<crate::crash_engine::attribution::LayerView>,
}

/// One key an instance's INI file sets, with the file and section that hold it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingEntry {
    /// The mapped instance file, as the pack named it.
    pub file: String,
    pub section: String,
    pub key: String,
    pub value: String,
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

/// What changed in one launch's setup compared with the last session that ended normally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Updated,
    Enabled,
    Disabled,
    Moved,
    LoadOrder,
    Settings,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Updated => "updated",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Moved => "moved",
            Self::LoadOrder => "load_order",
            Self::Settings => "settings",
        }
    }

    /// Whether this change names a single content item.
    pub fn names_an_item(self) -> bool {
        !matches!(self, Self::LoadOrder | Self::Settings)
    }
}

/// One named entry in a game's load order. The name is whatever the game calls it (a plugin file,
/// a package), so core never has to know the game's format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOrderEntry {
    /// Which declared load order this entry belongs to.
    pub list: String,
    pub name: String,
    pub active: bool,
}

/// One difference between two manifests, in the crash engine's terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestChange {
    pub kind: ChangeKind,
    /// The item's key, for a change that names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    /// Which INI setting changed, when the manifest recorded keys. A settings change with none
    /// keeps the generic wording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<SettingChange>,
}

/// One named setting change: the file and section that hold the key, and both values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingChange {
    pub file: String,
    pub section: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

/// What differs between the setup that ended normally and the one this session ran.
///
/// Only differences that can explain a change in behaviour are returned: an item added, removed,
/// updated or switched on or off; an item's position among the items both manifests have; the
/// load order; and the settings. Additions and removals do not make every other item look moved,
/// because order is compared only between the items both sides already have.
pub fn diff_manifests(previous: &LaunchManifest, current: &LaunchManifest) -> Vec<ManifestChange> {
    use std::collections::BTreeMap;

    let mut changes = Vec::new();
    let previous_items: BTreeMap<&str, &ManifestItem> = previous
        .items
        .iter()
        .map(|item| (item.key.as_str(), item))
        .collect();
    let current_items: BTreeMap<&str, &ManifestItem> = current
        .items
        .iter()
        .map(|item| (item.key.as_str(), item))
        .collect();

    for (key, item) in &current_items {
        match previous_items.get(key) {
            None => changes.push(ManifestChange {
                kind: ChangeKind::Added,
                key: Some((*key).to_string()),
                from_version: None,
                to_version: item.version.clone(),
                setting: None,
            }),
            Some(before) => {
                if before.enabled != item.enabled {
                    changes.push(ManifestChange {
                        kind: if item.enabled {
                            ChangeKind::Enabled
                        } else {
                            ChangeKind::Disabled
                        },
                        key: Some((*key).to_string()),
                        from_version: before.version.clone(),
                        to_version: item.version.clone(),
                        setting: None,
                    });
                } else if before.version != item.version || before.hash != item.hash {
                    changes.push(ManifestChange {
                        kind: ChangeKind::Updated,
                        key: Some((*key).to_string()),
                        from_version: before.version.clone(),
                        to_version: item.version.clone(),
                        setting: None,
                    });
                }
            }
        }
    }
    for (key, item) in &previous_items {
        if !current_items.contains_key(key) {
            changes.push(ManifestChange {
                kind: ChangeKind::Removed,
                key: Some((*key).to_string()),
                from_version: item.version.clone(),
                to_version: None,
                setting: None,
            });
        }
    }

    // Position among the items present on both sides. An inserted item shifts every later order
    // number, but not the relative order of the items that were there already.
    let mut shared: Vec<&str> = current_items
        .keys()
        .copied()
        .filter(|key| previous_items.contains_key(key))
        .collect();
    shared.sort_by_key(|key| (current_items[*key].order, *key));
    let mut previous_order: Vec<&str> = shared.clone();
    previous_order.sort_by_key(|key| (previous_items[*key].order, *key));
    let previous_rank: BTreeMap<&str, usize> = previous_order
        .into_iter()
        .enumerate()
        .map(|(rank, key)| (key, rank))
        .collect();
    for (rank, key) in shared.iter().enumerate() {
        if let Some(before) = previous_rank.get(key) {
            if *before != rank {
                changes.push(ManifestChange {
                    kind: ChangeKind::Moved,
                    key: Some((*key).to_string()),
                    from_version: previous_items[*key].version.clone(),
                    to_version: current_items[*key].version.clone(),
                    setting: None,
                });
            }
        }
    }

    // Name the plugin that was turned on or off when both manifests kept the list. A manifest
    // stored before this was kept has no entries, so it falls back to the generic change.
    let mut named_load_order = false;
    if !previous.load_order.is_empty() && !current.load_order.is_empty() {
        let key = |entry: &LoadOrderEntry| (entry.list.clone(), entry.name.clone());
        let previous_entries: BTreeMap<(String, String), &LoadOrderEntry> = previous
            .load_order
            .iter()
            .map(|entry| (key(entry), entry))
            .collect();
        let current_entries: BTreeMap<(String, String), &LoadOrderEntry> = current
            .load_order
            .iter()
            .map(|entry| (key(entry), entry))
            .collect();
        for (entry_key, entry) in &current_entries {
            match previous_entries.get(entry_key) {
                None => {
                    changes.push(ManifestChange {
                        kind: ChangeKind::Added,
                        key: Some(entry.name.clone()),
                        from_version: None,
                        to_version: None,
                        setting: None,
                    });
                    named_load_order = true;
                }
                Some(before) if before.active != entry.active => {
                    changes.push(ManifestChange {
                        kind: if entry.active {
                            ChangeKind::Enabled
                        } else {
                            ChangeKind::Disabled
                        },
                        key: Some(entry.name.clone()),
                        from_version: None,
                        to_version: None,
                        setting: None,
                    });
                    named_load_order = true;
                }
                Some(_) => {}
            }
        }
        for (entry_key, entry) in &previous_entries {
            if !current_entries.contains_key(entry_key) {
                changes.push(ManifestChange {
                    kind: ChangeKind::Removed,
                    key: Some(entry.name.clone()),
                    from_version: None,
                    to_version: None,
                    setting: None,
                });
                named_load_order = true;
            }
        }
    }
    if previous.load_orders_hash != current.load_orders_hash && !named_load_order {
        changes.push(ManifestChange {
            kind: ChangeKind::LoadOrder,
            key: None,
            from_version: None,
            to_version: None,
            setting: None,
        });
    }
    if previous.settings_hash != current.settings_hash {
        let named = diff_settings(previous, current);
        if named.is_empty() {
            changes.push(ManifestChange {
                kind: ChangeKind::Settings,
                key: None,
                from_version: None,
                to_version: None,
                setting: None,
            });
        } else {
            changes.extend(named);
        }
    }

    changes.sort_by(|left, right| {
        (left.kind as u8, left.key.as_deref().unwrap_or(""))
            .cmp(&(right.kind as u8, right.key.as_deref().unwrap_or("")))
    });
    changes
}

/// Name each INI key that differs. Both manifests must have recorded keys: an older manifest has
/// none, and guessing "set X to Y" from one side would invent a change. Names compare the way INI
/// names do, case-insensitively.
fn diff_settings(previous: &LaunchManifest, current: &LaunchManifest) -> Vec<ManifestChange> {
    use std::collections::BTreeMap;

    if previous.settings.is_empty() || current.settings.is_empty() {
        return Vec::new();
    }
    let key = |entry: &SettingEntry| {
        (
            entry.file.to_ascii_lowercase(),
            entry.section.to_ascii_lowercase(),
            entry.key.to_ascii_lowercase(),
        )
    };
    let previous_entries: BTreeMap<_, &SettingEntry> = previous
        .settings
        .iter()
        .map(|entry| (key(entry), entry))
        .collect();
    let current_entries: BTreeMap<_, &SettingEntry> = current
        .settings
        .iter()
        .map(|entry| (key(entry), entry))
        .collect();

    let named = |entry: &SettingEntry, from: Option<&String>, to: Option<&String>| ManifestChange {
        kind: ChangeKind::Settings,
        key: None,
        from_version: None,
        to_version: None,
        setting: Some(SettingChange {
            file: entry.file.clone(),
            section: entry.section.clone(),
            key: entry.key.clone(),
            from: from.cloned(),
            to: to.cloned(),
        }),
    };

    let mut changes = Vec::new();
    for (identity, entry) in &current_entries {
        match previous_entries.get(identity) {
            None => changes.push(named(entry, None, Some(&entry.value))),
            Some(before) if before.value != entry.value => {
                changes.push(named(entry, Some(&before.value), Some(&entry.value)))
            }
            Some(_) => {}
        }
    }
    for (identity, entry) in &previous_entries {
        if !current_entries.contains_key(identity) {
            changes.push(named(entry, Some(&entry.value), None));
        }
    }
    changes
}

/// The manifest of the most recent session of this instance that ended normally before
/// `before_started_at`, or `None` when there is no such session. A normal ending is the only one
/// that can be a baseline: a crash is the thing being explained, not a known-good setup.
pub fn previous_normal_manifest(
    conn: &Connection,
    instance_id: &str,
    before_started_at: &str,
) -> anyhow::Result<Option<LaunchManifest>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(verdict, ''), manifest_hash FROM launch_history
          WHERE instance_id = ?1 AND ended_at IS NOT NULL AND manifest_hash IS NOT NULL
            AND started_at < ?2
          ORDER BY started_at DESC, id DESC
          LIMIT 200",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![instance_id, before_started_at], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    for (verdict, hash) in rows {
        let Ok(verdict) = serde_json::from_str::<SessionVerdict>(&verdict) else {
            continue;
        };
        if verdict == SessionVerdict::Normal {
            return read_manifest(conn, instance_id, &hash);
        }
    }
    Ok(None)
}

/// The manifest of this instance's most recent recorded session, whatever its verdict. The
/// pre-launch check uses its load order to say which conflicts are new since the last launch.
pub fn latest_manifest(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Option<LaunchManifest>> {
    let hash: Option<String> = conn
        .query_row(
            "SELECT manifest_hash FROM launch_history
              WHERE instance_id = ?1 AND manifest_hash IS NOT NULL
              ORDER BY started_at DESC, id DESC
              LIMIT 1",
            params![instance_id],
            |row| row.get(0),
        )
        .optional()?;
    match hash {
        Some(hash) => read_manifest(conn, instance_id, &hash),
        None => Ok(None),
    }
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
    if !definition.load_orders.is_empty() {
        let mut hasher = Sha256::new();
        let mut failed = false;
        for rule in &definition.load_orders {
            match crate::game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id)) {
                Ok(list) => {
                    for entry in &list.entries {
                        hasher.update(
                            format!("{}\t{}\t{}\n", rule.id, entry.name, entry.active).as_bytes(),
                        );
                        manifest.load_order.push(LoadOrderEntry {
                            list: rule.id.clone(),
                            name: entry.name.clone(),
                            active: entry.active,
                        });
                    }
                }
                Err(e) => {
                    failed = true;
                    manifest
                        .unreadable
                        .push(format!("load order '{}': {e}", rule.id));
                }
            }
        }
        if failed {
            manifest.load_orders_hash = None;
        } else {
            manifest.load_orders_hash = Some(hex::encode(hasher.finalize()));
        }
    } else {
        manifest.load_orders_hash = Some(sha_hex(b""));
    }

    // Settings: the instance's own copies of the game's per-user files. Their keys are recorded
    // too, so a later change can say which setting moved and between what values.
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
                if let Some(entries) = setting_entries(&path, mapping.instance_path.as_str()) {
                    manifest.settings.extend(entries);
                }
            }
            if !failed {
                manifest.settings_hash = Some(hex::encode(hasher.finalize()));
            }
        }
        Err(e) => manifest.unreadable.push(format!("settings: {e}")),
    }

    match crate::crash_engine::attribution::from_instance(ctx, instance_id) {
        Ok(view) => manifest.owner_layers = view.layers().to_vec(),
        Err(error) => manifest.unreadable.push(format!("file ownership: {error}")),
    }
    manifest
}

/// The most keys one manifest records from the settings files. A file over this is left to the
/// generic settings change; a manifest must stay bounded.
const MAX_SETTING_ENTRIES: usize = 4096;

/// Read the keys an instance's INI file sets. A file that is not there, is not a file, or has more
/// keys than the limit contributes no detail. The hash still records that the file changed.
fn setting_entries(path: &std::path::Path, file: &str) -> Option<Vec<SettingEntry>> {
    use std::collections::BTreeMap;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let document = crate::game_ini::IniDocument::parse(&bytes);
    let entries = document.entries();
    if entries.len() > MAX_SETTING_ENTRIES {
        return None;
    }
    // The last line for a key wins, the way the INI reader reads it, and the order is fixed.
    let mut by_key: BTreeMap<(String, String), SettingEntry> = BTreeMap::new();
    for entry in entries {
        let entry = SettingEntry {
            file: file.to_string(),
            section: entry.section,
            key: entry.key,
            value: entry.value,
        };
        by_key.insert(
            (
                entry.section.to_ascii_lowercase(),
                entry.key.to_ascii_lowercase(),
            ),
            entry,
        );
    }
    Some(by_key.into_values().collect())
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
        owner_layers: Vec::new(),
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

    /// Whether the watcher may still be running.
    pub fn is_alive(pid: u32, started: u64) -> bool {
        use crate::process_identity::{self, ProcessCheck, ProcessIdentity};
        process_identity::check(&ProcessIdentity {
            pid,
            start_time: started,
            expected_exe: None,
            job: None,
        }) != ProcessCheck::Gone
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
    begin_with_process(conn, instance_id, started_at, manifest, owner, None)
}

/// Open a session with the identity captured when the game was spawned.
pub fn begin_with_process(
    conn: &Connection,
    instance_id: &str,
    started_at: &str,
    manifest: &LaunchManifest,
    owner: Owner,
    process: Option<&crate::process_identity::ProcessIdentity>,
) -> anyhow::Result<String> {
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let tx = conn.unchecked_transaction()?;
    let hash = store_manifest(&tx, instance_id, manifest)?;
    let process = process.map(serde_json::to_string).transpose()?;
    tx.execute(
        "INSERT INTO launch_history
             (instance_id, started_at, enabled_mod_count, session_id, manifest_hash,
              owner_pid, owner_start, game_process_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            instance_id,
            started_at,
            manifest.enabled_count(),
            session_id,
            hash,
            owner.pid,
            owner.started as i64,
            process,
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

fn record_play_time_ms(
    conn: &Connection,
    instance_id: &str,
    ended_at: &str,
    duration_ms: i64,
) -> rusqlite::Result<()> {
    let elapsed = duration_ms.max(0);
    let last_played_at = chrono::DateTime::parse_from_rfc3339(ended_at)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| ended_at.to_string());
    conn.execute(
        "UPDATE instances
            SET total_play_ms = total_play_ms + ?1,
                last_played_at = ?2
          WHERE instance_id = ?3",
        params![elapsed, last_played_at, instance_id],
    )?;
    Ok(())
}

/// Close a session: judge it, and store the exit and the verdict. An already-closed session is
/// left alone, so two watchers cannot overwrite each other. Returns the verdict, or `None` when
/// there was no open session with this id. When it closes the session, it also records playtime
/// once for the instance.
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
    let closed = write_end(
        conn,
        row_id,
        &done.ended_at,
        Some(done.duration_ms),
        &done.exit,
        judged,
    )?;
    if !closed {
        return Ok(None);
    }
    let played_ms = done
        .game_runtime_ms
        .map(|ms| i64::try_from(ms).unwrap_or(i64::MAX))
        .unwrap_or_else(|| done.duration_ms.max(0));
    let _ = record_play_time_ms(conn, &instance_id, &done.ended_at, played_ms);
    // The history is trimmed with its manifests; a failure to trim is not a failure to record.
    let _ = crate::launch_history::prune_history(conn, &instance_id);
    Ok(Some(judged))
}

/// Correct a finished session's verdict to `Crashed` after evidence found a crash written inside
/// its window. The exit code and the session's length are not consulted: a crash artifact is the
/// evidence, whatever the process said or how long it ran. An open row and a row already marked
/// crashed are left alone. Returns whether the row changed.
pub fn mark_crashed(conn: &Connection, session_id: &str, how: MatchedBy) -> anyhow::Result<bool> {
    let current: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT ended_at, verdict FROM launch_history WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((ended_at, verdict)) = current else {
        return Ok(false);
    };
    if ended_at.is_none() {
        return Ok(false);
    }
    let already = verdict
        .as_deref()
        .and_then(|json| serde_json::from_str::<SessionVerdict>(json).ok());
    if matches!(already, Some(SessionVerdict::Crashed { .. })) {
        return Ok(false);
    }
    let judged = SessionVerdict::Crashed { how };
    conn.execute(
        "UPDATE launch_history SET verdict = ?2, outcome = ?3 WHERE session_id = ?1",
        params![
            session_id,
            serde_json::to_string(&judged)?,
            LaunchResult::from(judged).as_str(),
        ],
    )?;
    Ok(true)
}

fn write_end(
    conn: &Connection,
    row_id: i64,
    ended_at: &str,
    duration_ms: Option<i64>,
    exit: &ExitObservation,
    judged: SessionVerdict,
) -> anyhow::Result<bool> {
    let changed = conn.execute(
        "UPDATE launch_history
            SET ended_at = ?2, duration_ms = ?3, exit_json = ?4, verdict = ?5, outcome = ?6
          WHERE id = ?1 AND ended_at IS NULL",
        params![
            row_id,
            ended_at,
            duration_ms,
            serde_json::to_string(exit)?,
            serde_json::to_string(&judged)?,
            LaunchResult::from(judged).as_str(),
        ],
    )?;
    Ok(changed > 0)
}

/// An open session the catch-up closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedSession {
    pub session_id: String,
    pub instance_id: String,
    pub verdict: SessionVerdict,
}

/// One open session, with what a catch-up needs to decide and close it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    pub session_id: String,
    pub instance_id: String,
    pub started_at: String,
    pub owner_pid: Option<u32>,
    pub owner_start: Option<i64>,
    pub stop_requested: bool,
    pub game_process: Option<crate::process_identity::ProcessIdentity>,
}

/// This instance's open sessions, newest first.
pub fn open_sessions_for_instance(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<OpenSession>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, instance_id, started_at, owner_pid, owner_start, stop_requested, game_process_json
           FROM launch_history
          WHERE instance_id = ?1 AND session_id IS NOT NULL AND ended_at IS NULL
          ORDER BY started_at DESC, id DESC",
    )?;
    let rows = stmt
        .query_map([instance_id], |row| {
            Ok(OpenSession {
                session_id: row.get(0)?,
                instance_id: row.get(1)?,
                started_at: row.get(2)?,
                owner_pid: row.get(3)?,
                owner_start: row.get(4)?,
                stop_requested: row.get::<_, i64>(5)? != 0,
                game_process: read_game_process(row, 6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

fn read_game_process(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<crate::process_identity::ProcessIdentity>> {
    let process: Option<String> = row.get(index)?;
    process
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

/// Close one open session whose owner is gone, as `ClosedUnknown` (or `Stopped` when the player
/// had pressed Stop). Returns `None` when the row was already closed. This is what a catch-up
/// calls after it has evidence the game ended.
pub fn close_unwatched_session(
    conn: &Connection,
    session_id: &str,
    now: &str,
    why: &str,
) -> anyhow::Result<Option<ClosedSession>> {
    close_open_session(conn, session_id, now, why, None)
}

/// Close one open session with a verdict already decided by positive evidence. The exit is
/// unobserved; the evidence is why the row is closed. `None` for `judged` asks for the normal
/// stop/unknown judgement.
pub fn close_open_session(
    conn: &Connection,
    session_id: &str,
    now: &str,
    why: &str,
    judged: Option<SessionVerdict>,
) -> anyhow::Result<Option<ClosedSession>> {
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
    let exit = ExitObservation::unavailable(why);
    let judged = judged.unwrap_or_else(|| {
        verdict(&VerdictInput {
            exit: &exit,
            stop_requested,
            runtime_ms: 0,
            normal_exit_codes: &[],
            artifact: None,
            froze: false,
        })
    });
    let closed = write_end(conn, row_id, now, None, &exit, judged)?;
    if !closed {
        return Ok(None);
    }
    Ok(Some(ClosedSession {
        session_id: session_id.to_string(),
        instance_id,
        verdict: judged,
    }))
}

/// Close this instance's open sessions whose owner is gone and whose game is not running, as
/// `ClosedUnknown` (or `Stopped`, when the player had pressed Stop). A session whose game might
/// still be running is left alone: a gone owner proves nothing about the game.
/// `owner_alive` decides whether the owner still runs ([`Owner::is_alive`] is the real check);
/// `game_may_be_running` is the caller's "not provably gone" check in production
/// ([`crate::instance_runtime::game_is_provably_gone`]).
pub fn close_unwatched_for_instance(
    conn: &Connection,
    instance_id: &str,
    now: &str,
    owner_alive: &dyn Fn(u32, u64) -> bool,
    game_may_be_running: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<ClosedSession>> {
    close_unwatched(
        conn,
        Some(instance_id),
        now,
        "Agora was not watching when the game ended, so nothing saw how it ended",
        owner_alive,
        game_may_be_running,
    )
}

/// Close every open session whose owner is gone, as `ClosedUnknown` (or `Stopped`, when the player
/// had pressed Stop). Agora closing mid-session leaves a row nobody will finish; calling it
/// "normal" would be a false claim, so it says what is true: nothing watched how it ended. A
/// session whose game might still be running is left open.
/// `owner_alive` decides whether the owner still runs; [`Owner::is_alive`] is the real check.
/// `game_may_be_running` is the caller's "not provably gone" check in production
/// ([`crate::instance_runtime::game_is_provably_gone`]).
pub fn close_orphans(
    conn: &Connection,
    now: &str,
    owner_alive: &dyn Fn(u32, u64) -> bool,
    game_may_be_running: &dyn Fn(&str) -> bool,
) -> anyhow::Result<usize> {
    Ok(close_unwatched(
        conn,
        None,
        now,
        "Agora closed while the game was running, so nothing watched how it ended",
        owner_alive,
        game_may_be_running,
    )?
    .len())
}

type OpenSessionRow = (
    i64,
    String,
    String,
    Option<u32>,
    Option<i64>,
    bool,
    Option<crate::process_identity::ProcessIdentity>,
);

fn close_unwatched(
    conn: &Connection,
    only_instance: Option<&str>,
    now: &str,
    why: &str,
    owner_alive: &dyn Fn(u32, u64) -> bool,
    game_may_be_running: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<ClosedSession>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, instance_id, owner_pid, owner_start, stop_requested, game_process_json
           FROM launch_history
          WHERE session_id IS NOT NULL AND ended_at IS NULL
            AND (?1 IS NULL OR instance_id = ?1)",
    )?;
    let open: Vec<OpenSessionRow> = stmt
        .query_map([only_instance], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get::<_, i64>(5)? != 0,
                read_game_process(row, 6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut closed = Vec::new();
    for (row_id, session_id, instance_id, pid, started, stop_requested, process) in open {
        if process.as_ref().is_some_and(|process| {
            crate::process_identity::check(process) != crate::process_identity::ProcessCheck::Gone
        }) {
            continue;
        }
        if let (Some(pid), Some(started)) = (pid, started) {
            if owner_alive(pid, started as u64) {
                continue;
            }
        }
        // A gone owner says nothing about the game; never close one that might still run.
        if game_may_be_running(&instance_id) {
            continue;
        }
        let exit = ExitObservation::unavailable(why);
        let judged = verdict(&VerdictInput {
            exit: &exit,
            stop_requested,
            runtime_ms: 0,
            normal_exit_codes: &[],
            artifact: None,
            froze: false,
        });
        if write_end(conn, row_id, now, None, &exit, judged)? {
            closed.push(ClosedSession {
                session_id,
                instance_id,
                verdict: judged,
            });
        }
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

/// Whether this instance's user-files journal says its game is still running. A game instance has
/// no runtime lease, so the journal is the record that its game started and has not ended.
fn user_files_session_running(ctx: &Ctx, instance_id: &str) -> bool {
    let Ok(Some(record)) = crate::game_instance::get(ctx, instance_id) else {
        return false;
    };
    let Ok(manifest) = crate::game_instance::get_manifest(ctx, instance_id) else {
        return false;
    };
    let Some(store) = crate::game_instance::session_store(&record.base, &manifest) else {
        return false;
    };
    crate::game_user_files::status(ctx, &record.game, &store)
        .ok()
        .flatten()
        .is_some_and(|status| status.running)
}

/// Open a session for a launch that is about to be reported as started. Returns the session id, or
/// `None` when nothing could be recorded. Never fails and never panics: a launch is not blocked by
/// its own bookkeeping. Sessions whose owner is gone are closed first.
pub fn record_launch(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    process: Option<&crate::process_identity::ProcessIdentity>,
) -> Option<String> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
        let now = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
        let _ = close_orphans(&conn, &now, &Owner::is_alive, &|instance_id| {
            user_files_session_running(ctx, instance_id)
        });
        let manifest = build_manifest(ctx, instance_id, definition, store);
        begin_with_process(
            &conn,
            instance_id,
            &now,
            &manifest,
            Owner::current(),
            process,
        )
        .ok()
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

/// A Minecraft launch row that also carries the session ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinecraftSession {
    /// The row `launch_history` uses for the legacy history view.
    pub row_id: i64,
    /// The ledger id, for the crash answer.
    pub session_id: String,
}

/// Open a Minecraft launch's session at the moment it starts. Minecraft keeps its own history row
/// and its own launch service, so the row is opened the way it always was and the ledger columns
/// are attached to it. Best effort: a launch is never blocked by its bookkeeping.
pub fn open_minecraft(ctx: &Ctx, instance_id: &str) -> Option<MinecraftSession> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
        let now = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
        let _ = close_orphans(&conn, &now, &Owner::is_alive, &|instance_id| {
            // A delegated launch keeps no process identity, so no runtime record is not proof
            // that the game is gone. The explicit catch-up closes it with crash evidence.
            !crate::instance_runtime::game_is_provably_gone(&ctx.paths, instance_id)
        });
        let manifest_path = ctx.paths.instance_manifest(instance_id).ok()?;
        let manifest = crate::helpers::read_manifest(&manifest_path).ok()?;
        let row_id = crate::launch_history::begin_launch(
            &conn,
            instance_id,
            &now,
            manifest.mods.iter().filter(|entry| entry.enabled).count() as i64,
            &manifest.minecraft_version,
            &manifest.loader,
        )
        .ok()?;
        let ledger = minecraft_manifest(&manifest);
        let session_id =
            attach_ledger(&conn, row_id, instance_id, &ledger, Owner::current()).ok()?;
        Some(MinecraftSession { row_id, session_id })
    }));
    attempt.ok().flatten()
}

/// Attach a session id, manifest and watcher to an already-open history row. Returns the id.
pub fn attach_ledger(
    conn: &Connection,
    row_id: i64,
    instance_id: &str,
    manifest: &LaunchManifest,
    owner: Owner,
) -> anyhow::Result<String> {
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let tx = conn.unchecked_transaction()?;
    let hash = store_manifest(&tx, instance_id, manifest)?;
    tx.execute(
        "UPDATE launch_history
            SET session_id = ?2, manifest_hash = ?3, owner_pid = ?4, owner_start = ?5
          WHERE id = ?1",
        params![row_id, session_id, hash, owner.pid, owner.started as i64],
    )?;
    tx.commit()?;
    Ok(session_id)
}

/// Close a Minecraft session from the launch service's own outcome. The service already decides
/// whether the launch was a crash, a stop or a clean end; this turns that into the ledger's verdict
/// so the crash doctor can read the session. Best effort, like every other ledger write.
pub fn finish_minecraft(
    ctx: &Ctx,
    session_id: &str,
    pid: u32,
    outcome: &crate::lkg::LaunchOutcome,
    duration_ms: i64,
) -> Option<SessionVerdict> {
    use crate::lkg::LaunchOutcome;

    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
        let instance_id: Option<String> = conn
            .query_row(
                "SELECT instance_id FROM launch_history WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if matches!(outcome, LaunchOutcome::Cancelled) {
            if let Some(instance_id) = &instance_id {
                let _ = mark_stop_requested(&conn, instance_id);
            }
        }
        let exit = ExitObservation::Code {
            pid,
            image: "Minecraft".to_string(),
            code: 0,
        };
        let (exit, artifact) = match outcome {
            // The launch service reports an outcome, not the game's exit code. A clean or brief
            // end is its own statement that this was not a crash.
            LaunchOutcome::Success | LaunchOutcome::Abandoned => (exit, None),
            // A crash is not visible in an exit code the service did not keep, so the service's
            // verdict is the evidence here.
            LaunchOutcome::Crash => (
                ExitObservation::unavailable(
                    "Minecraft's launch service reported a crash without keeping the exit code",
                ),
                Some(ArtifactEvidence {
                    bound_by: MatchedBy::Time,
                }),
            ),
            LaunchOutcome::Cancelled => (
                ExitObservation::unavailable("the player stopped the game"),
                None,
            ),
            LaunchOutcome::Unknown => (
                ExitObservation::unavailable(
                    "Minecraft's launch service could not tell how the game ended",
                ),
                None,
            ),
        };
        let done = SessionFinish {
            ended_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
            duration_ms,
            exit,
            game_runtime_ms: u64::try_from(duration_ms).ok(),
            normal_exit_codes: vec![0],
            artifact,
            froze: false,
        };
        finish(&conn, session_id, &done).ok().flatten()
    }));
    attempt.ok().flatten()
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
            load_order: Vec::new(),
            runtime: "steam:1.6.1170.0".into(),
            settings_hash: Some(sha_hex(b"settings")),
            settings: Vec::new(),
            unreadable: vec![],
            owner_layers: Vec::new(),
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
            owner_layers: Vec::new(),
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
            close_orphans(&conn, "2026-01-02T00:00:00Z", &|_, _| false, &|_| false).unwrap(),
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
        let closed = close_orphans(&conn, "2026-01-01T02:00:00Z", &|pid, _| pid == 222, &|_| {
            false
        })
        .unwrap();
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
    fn a_session_is_not_closed_while_its_game_might_still_be_running() {
        let conn = db();
        let open = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        let closed = close_unwatched_for_instance(
            &conn,
            "skyrim",
            "2026-01-01T02:00:00Z",
            &|_, _| false,
            &|instance| instance == "skyrim",
        )
        .unwrap();
        assert!(
            closed.is_empty(),
            "a game that may run keeps its session open"
        );
        assert!(get(&conn, &open).unwrap().unwrap().ended_at.is_none());

        // Once the game is provably gone, the same call closes it as unknown.
        let closed = close_unwatched_for_instance(
            &conn,
            "skyrim",
            "2026-01-01T02:00:00Z",
            &|_, _| false,
            &|_| false,
        )
        .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].session_id, open);
        assert_eq!(closed[0].verdict, SessionVerdict::ClosedUnknown);
        assert!(get(&conn, &open).unwrap().unwrap().ended_at.is_some());
    }

    #[test]
    fn an_already_finished_session_is_not_closed_again() {
        let conn = db();
        let finished = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        finish(&conn, &finished, &finish_with(code(0))).unwrap();

        let closed = close_unwatched_for_instance(
            &conn,
            "skyrim",
            "2026-01-01T02:00:00Z",
            &|_, _| false,
            &|_| false,
        )
        .unwrap();
        assert!(closed.is_empty());
        assert_eq!(
            get(&conn, &finished).unwrap().unwrap().verdict,
            Some(SessionVerdict::Normal)
        );
    }

    #[test]
    fn a_crash_artifact_makes_a_short_nonzero_exit_a_crash() {
        let exit = code(1);
        for runtime_ms in [1, 59_000, 600_000] {
            let judged = verdict(&VerdictInput {
                exit: &exit,
                stop_requested: false,
                runtime_ms,
                normal_exit_codes: &NORMAL,
                artifact: Some(ArtifactEvidence {
                    bound_by: MatchedBy::Time,
                }),
                froze: false,
            });
            assert_eq!(
                judged,
                SessionVerdict::Crashed {
                    how: MatchedBy::Time
                },
                "runtime {runtime_ms} ms"
            );
        }
    }

    #[test]
    fn a_finished_session_is_corrected_to_crashed_when_evidence_finds_one() {
        let conn = db();
        let session = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        // Exit 1 after 59 seconds is a start failure until a crash artifact says otherwise.
        let mut failed = finish_with(code(1));
        failed.game_runtime_ms = Some(59_000);
        failed.duration_ms = 59_000;
        assert_eq!(
            finish(&conn, &session, &failed).unwrap(),
            Some(SessionVerdict::FailedToStart)
        );

        assert!(mark_crashed(&conn, &session, MatchedBy::Time).unwrap());
        let record = get(&conn, &session).unwrap().unwrap();
        assert_eq!(
            record.verdict,
            Some(SessionVerdict::Crashed {
                how: MatchedBy::Time
            })
        );
        // A second discovery does not rewrite it.
        assert!(!mark_crashed(&conn, &session, MatchedBy::Time).unwrap());
    }

    #[test]
    fn an_open_session_is_not_corrected_before_it_ends() {
        let conn = db();
        let session = begin(
            &conn,
            "skyrim",
            "2026-01-01T00:00:00Z",
            &big_manifest(0),
            owner(111),
        )
        .unwrap();
        assert!(!mark_crashed(&conn, &session, MatchedBy::Time).unwrap());
        assert!(get(&conn, &session).unwrap().unwrap().verdict.is_none());
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
        conn.execute_batch(
            "INSERT INTO instances (instance_id, game, name, created_at)
             VALUES ('mc', 'minecraft', 'Mc', '2026-01-01T00:00:00Z');
             INSERT INTO minecraft_instances
                (instance_id, minecraft_version, loader, loader_version)
             VALUES ('mc', '1.21', 'fabric', '0.15.0');",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO instances (instance_id, game, name)
             VALUES ('skyrim', 'skyrim-se', 'Skyrim');
             INSERT INTO game_instance_bases (instance_id, base_json) VALUES ('skyrim', '{}');",
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
            "DELETE FROM instances WHERE instance_id = 'skyrim'",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM instances WHERE instance_id = 'mc'", [])
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
        conn.execute_batch(
            "INSERT INTO instances (instance_id, game, name, created_at)
             VALUES ('mc', 'minecraft', 'Mc', '2026-01-01T00:00:00Z');
             INSERT INTO minecraft_instances
                (instance_id, minecraft_version, loader, loader_version)
             VALUES ('mc', '1.21', 'fabric', '0.15.0');",
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

    fn item(key: &str, version: &str, enabled: bool, order: u32) -> ManifestItem {
        ManifestItem {
            key: key.to_string(),
            version: Some(version.to_string()),
            hash: Some(format!("hash-{key}-{version}")),
            enabled,
            order,
            unreadable: None,
        }
    }

    #[test]
    fn a_diff_names_every_kind_of_change_and_no_baseline_names_none() {
        let previous = LaunchManifest {
            items: vec![
                item("kept", "1", true, 0),
                item("updated", "1", true, 1),
                item("switched-off", "1", true, 2),
                item("removed", "1", true, 3),
                item("moved", "1", true, 4),
            ],
            load_orders_hash: Some("load-1".into()),
            settings_hash: Some("settings-1".into()),
            runtime: "steam:1".into(),
            ..LaunchManifest::default()
        };
        let current = LaunchManifest {
            items: vec![
                item("kept", "1", true, 1),
                item("updated", "2", true, 0),
                item("switched-off", "1", false, 2),
                item("added", "1", true, 3),
                item("moved", "1", true, 5),
            ],
            load_orders_hash: Some("load-2".into()),
            settings_hash: Some("settings-2".into()),
            runtime: "steam:1".into(),
            ..LaunchManifest::default()
        };
        let changes = diff_manifests(&previous, &current);
        let has = |kind: ChangeKind, key: &str| {
            changes
                .iter()
                .any(|change| change.kind == kind && change.key.as_deref() == Some(key))
        };
        assert!(has(ChangeKind::Added, "added"));
        assert!(has(ChangeKind::Removed, "removed"));
        assert!(has(ChangeKind::Updated, "updated"));
        assert!(has(ChangeKind::Disabled, "switched-off"));
        assert!(has(ChangeKind::Moved, "kept"));
        assert!(changes
            .iter()
            .any(|change| change.kind == ChangeKind::LoadOrder));
        assert!(changes
            .iter()
            .any(|change| change.kind == ChangeKind::Settings));
        assert!(
            !has(ChangeKind::Moved, "added"),
            "a new item is added, not moved"
        );
        assert!(
            !has(ChangeKind::Moved, "removed"),
            "a gone item is removed, not moved"
        );

        assert!(diff_manifests(&previous, &previous).is_empty());
    }

    #[test]
    fn a_plugin_turned_off_is_named_instead_of_a_generic_load_order_change() {
        let entry = |name: &str, active: bool| LoadOrderEntry {
            list: "plugins".to_string(),
            name: name.to_string(),
            active,
        };
        let previous = LaunchManifest {
            load_orders_hash: Some("load-1".into()),
            load_order: vec![
                entry("Example.esm", true),
                entry("Unofficial Example Patch.esp", true),
            ],
            ..LaunchManifest::default()
        };
        let current = LaunchManifest {
            load_orders_hash: Some("load-2".into()),
            load_order: vec![
                entry("Example.esm", true),
                entry("Unofficial Example Patch.esp", false),
            ],
            ..LaunchManifest::default()
        };
        let changes = diff_manifests(&previous, &current);
        assert!(changes.iter().any(|change| {
            change.kind == ChangeKind::Disabled
                && change.key.as_deref() == Some("Unofficial Example Patch.esp")
        }));
        assert!(
            !changes
                .iter()
                .any(|change| change.kind == ChangeKind::LoadOrder),
            "{changes:?}"
        );
    }

    #[test]
    fn only_crashed_counts_as_a_crash_and_only_normal_as_ok() {
        use SessionVerdict::*;
        assert_eq!(
            LaunchResult::from(Crashed {
                how: MatchedBy::Pid
            }),
            LaunchResult::Crashed
        );
        assert_eq!(LaunchResult::from(Normal), LaunchResult::Ok);
        assert_eq!(LaunchResult::from(Stopped), LaunchResult::Stopped);
        for other in [FailedToStart, Froze, ClosedUnknown] {
            assert_eq!(LaunchResult::from(other), LaunchResult::Unknown);
        }
    }
}
