//! Snapshots and restore for generic game instances (MASTER_SPEC §23, §26).
//!
//! A game instance's snapshot holds its own state, not its mod bytes: the instance
//! manifest (whose content layers name content-store items by id), the plugin lists
//! and INI copies the game definition declares, the shared loadout store, the save
//! choice (in the manifest), and the load-after rules, which live in the database and
//! are written into the snapshot as data. Content objects stay in the content store,
//! so an 84 GB setup costs kilobytes here.
//!
//! The state files are captured individually as data, not by folder: a game may
//! keep a plugin list beside its mods (Cyberpunk's `mods/modlist.txt`), and walking
//! that folder would copy mod bytes. The scope is therefore declared by the game
//! definition plus the two core-owned pieces (`instance_manifest.json` and
//! `.agora_loadouts`), never hard-coded per game.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agora_game_api::{GameDefinition, LayerSource, LoadOrderDestination};
use serde::{Deserialize, Serialize};

use agora_game_api::crash::SessionVerdict;

use crate::ctx::Ctx;
use crate::game_instance::{self, GameInstanceManifest};
use crate::game_load_order::{self, LoadAfterRule};
use crate::game_loadout::GameLoadoutRules;
use crate::lkg::{self, Diff, FileEntry, LaunchOutcome, LkgState};
use crate::lock_manager::LockResource;
use crate::snapshot::{
    self, RestoreCoverageKind, RestoreOutcome, Snapshot, SnapshotAux, SnapshotInputFile,
    SnapshotOrigin,
};

/// The instance manifest is always part of a game instance's state.
const INSTANCE_MANIFEST_ROOT: &str = "instance_manifest.json";

/// Set while a game-instance restore is writing its files. It names the undo
/// snapshot, so an interrupted restore is recovered by restoring that snapshot
/// rather than guessed at.
const GAME_RESTORE_PENDING: &str = ".agora_instance_restore_pending";

/// What a game-instance snapshot records besides its files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSnapshotData {
    /// Every list's load-after rules, by list id.
    #[serde(default)]
    pub rules: Vec<GameLoadoutRules>,
    /// Content-store item ids the captured state names.
    #[serde(default)]
    pub content_refs: Vec<String>,
}

/// The error code a caller gets when the game is running or cannot be proven
/// idle. It is the one case with an override, so front ends branch on it.
pub const ERR_SNAPSHOT_NOT_IDLE: &str = "ERR_SNAPSHOT_NOT_IDLE";

/// One snapshot that names a content item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReference {
    pub instance_id: String,
    pub snapshot_id: String,
    pub label: Option<String>,
    pub created_at: String,
    pub path: String,
    /// The exact command that deletes this snapshot.
    pub command: String,
    pub in_trash: bool,
}

/// One snapshot whose manifest cannot be read, so it might name any item.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadableSnapshotReference {
    pub instance_id: String,
    pub snapshot_id: String,
    pub created_at: String,
    pub path: String,
    pub error: String,
    /// The exact command that deletes this snapshot.
    pub command: String,
    pub in_trash: bool,
}

/// What a content-removal check found.
#[derive(Debug, Clone, Default)]
pub struct SnapshotContentUsage {
    /// Readable snapshots that name the item.
    pub using: Vec<SnapshotReference>,
    /// Snapshots that cannot be read; they might name the item.
    pub unreadable: Vec<UnreadableSnapshotReference>,
}

/// Why a game-instance restore did not run.
#[derive(Debug, thiserror::Error)]
pub enum GameRestoreError {
    #[error("'{instance_id}' is running; close the game before restoring a snapshot")]
    Running { instance_id: String },
    #[error("could not tell whether '{instance_id}' is running: {reason}; nothing was changed")]
    CouldNotTell { instance_id: String, reason: String },
    #[error("{0}")]
    Other(String),
}

impl GameRestoreError {
    /// The stable code the CLI and desktop branch on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Running { .. } | Self::CouldNotTell { .. } => ERR_SNAPSHOT_NOT_IDLE,
            Self::Other(_) => "ERR_SNAPSHOT",
        }
    }
}

impl From<String> for GameRestoreError {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}

/// What a restore did.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameRestoreOutcome {
    #[serde(flatten)]
    pub outcome: RestoreOutcome,
    /// The snapshot taken just before the restore, so the restore can be undone.
    pub undo_snapshot_id: String,
    /// True when the restore ran while the game was running or could not be
    /// proven idle, because the caller explicitly overrode that refusal.
    pub forced_while_not_idle: bool,
}

/// The game definition an instance belongs to, or a refusal naming the missing id.
pub fn definition_for(ctx: &Ctx, instance_id: &str) -> Result<GameDefinition, String> {
    let manifest = game_instance::get_manifest(ctx, instance_id).map_err(|e| e.to_string())?;
    ctx.games.game(&manifest.game).cloned().ok_or_else(|| {
        format!(
            "the definition for game '{}' is not installed, so this instance's snapshot scope \
                 cannot be declared",
            manifest.game.as_str()
        )
    })
}

/// The files the game definition declares as an instance's state.
fn declared_state_paths(definition: &GameDefinition) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    paths.insert(INSTANCE_MANIFEST_ROOT.to_string());
    for rule in &definition.load_orders {
        // A view-destination file is generated output, not instance state.
        if rule.destination == LoadOrderDestination::Instance {
            paths.insert(rule.file.as_str().to_string());
        }
    }
    for mapping in &definition.user_files {
        paths.insert(mapping.instance_path.as_str().to_string());
    }
    paths
}

/// Every file under `.agora_loadouts`, relative to the instance folder.
fn loadout_state_paths(dir: &Path) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    let root = crate::loadout::loadouts_dir(dir);
    if !root.is_dir() {
        return Ok(paths);
    }
    let mut stack = vec![root.clone()];
    while let Some(folder) = stack.pop() {
        let entries = std::fs::read_dir(&folder)
            .map_err(|error| format!("cannot read {}: {error}", folder.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("cannot read {}: {error}", folder.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let relative = path
                    .strip_prefix(dir)
                    .map_err(|error| format!("cannot place {}: {error}", path.display()))?
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                paths.push(relative);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_state_file(dir: &Path, relative: &str) -> Result<Option<Vec<u8>>, String> {
    let path = dir.join(Path::new(relative));
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read '{}': {error}", path.display())),
    }
}

/// The state files, as data. A missing declared file is recorded as absent.
fn capture_files(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<SnapshotInputFile>, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    let mut paths = declared_state_paths(definition);
    paths.extend(loadout_state_paths(&dir)?);
    let mut files = Vec::with_capacity(paths.len());
    for relative in paths {
        snapshot::validate_relative_path(&relative)?;
        files.push(SnapshotInputFile {
            contents: read_state_file(&dir, &relative)?,
            relative_path: relative,
        });
    }
    Ok(files)
}

/// Read the state a snapshot records as data: every declared list's rules, and
/// the content-store items the manifest and loadout profiles name.
pub fn capture_data(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<GameSnapshotData, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    // A backup imported into a fresh data folder has no live manifest yet. Its
    // snapshot still restores one, and the undo point records that there was
    // none rather than refusing the restore. A manifest that exists but cannot
    // be read is still an error.
    let manifest = if dir.join(INSTANCE_MANIFEST_ROOT).exists() {
        Some(game_instance::get_manifest(ctx, instance_id).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut refs = BTreeSet::new();
    if let Some(manifest) = &manifest {
        for layer in manifest.layers.layers() {
            if let LayerSource::Content { content } = &layer.source {
                refs.insert(content.clone());
            }
        }
    }
    for profile in crate::loadout::list_profiles(&dir)? {
        if let Some(state) = &profile.game {
            for item in &state.content {
                refs.insert(item.item.clone());
            }
        }
    }

    let mut rules = Vec::with_capacity(definition.load_orders.len());
    for rule in &definition.load_orders {
        rules.push(GameLoadoutRules {
            list: rule.id.clone(),
            rules: game_load_order::load_after_rules(ctx, instance_id, &rule.id)
                .map_err(|e| e.to_string())?,
        });
    }

    Ok(GameSnapshotData {
        rules,
        content_refs: refs.into_iter().collect(),
    })
}

fn aux_for(data: &GameSnapshotData) -> Result<SnapshotAux, String> {
    Ok(SnapshotAux {
        content_refs: data.content_refs.clone(),
        state: Some(serde_json::to_value(data).map_err(|e| e.to_string())?),
    })
}

fn pending_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join(GAME_RESTORE_PENDING)
}

fn read_pending(instance_dir: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(pending_path(instance_dir)) {
        Ok(undo) if !undo.trim().is_empty() => Ok(Some(undo.trim().to_string())),
        Ok(_) => Err(
            "a restore was interrupted and its undo snapshot is not recorded; check the instance \
             before trying again"
                .into(),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "a restore may have been interrupted, and its marker cannot be read: {error}"
        )),
    }
}

fn refuse_if_pending(instance_dir: &Path, recovering: Option<&str>) -> Result<(), String> {
    match read_pending(instance_dir)? {
        Some(undo) if recovering == Some(undo.as_str()) => Ok(()),
        Some(undo) => Err(format!(
            "a previous restore was interrupted. Restore snapshot {undo} to put the instance \
             back before taking another snapshot"
        )),
        None => Ok(()),
    }
}

/// Take a snapshot of a game instance. Acquires the instance lock and refuses
/// while the game may be running, like every other front-end snapshot.
pub fn create(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    label: Option<&str>,
    origin: SnapshotOrigin,
) -> Result<Snapshot, String> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::Instance(instance_id.to_string()), "snapshot")
        .map_err(|e| e.to_string())?;
    crate::instance_runtime::ensure_game_not_running(ctx, instance_id, "take a snapshot")?;
    let snapshot = create_locked(ctx, instance_id, definition, label, origin)?;
    if let Err(error) = crate::lkg::run_retention(
        &ctx.paths
            .instance_dir(instance_id)
            .map_err(|e| e.to_string())?,
    ) {
        eprintln!("[snapshot] retention after capture failed: {error}");
    }
    Ok(snapshot)
}

/// Take a snapshot while the caller already holds the instance lock.
pub fn create_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    label: Option<&str>,
    origin: SnapshotOrigin,
) -> Result<Snapshot, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    refuse_if_pending(&dir, None)?;
    let files = capture_files(ctx, instance_id, definition)?;
    let data = capture_data(ctx, instance_id, definition)?;
    let aux = aux_for(&data)?;
    snapshot::create_snapshot_from_data(&dir, label, &files, origin, &aux)
}

/// Take an automatic recovery point before a change, for a caller that already
/// holds the instance lock. A game whose definition is not installed has no
/// declared scope, so the change proceeds without a snapshot and the reason is
/// logged; every other failure refuses the change.
pub fn snapshot_before_change_locked(
    ctx: &Ctx,
    instance_id: &str,
    label: &str,
) -> Result<(), String> {
    let definition = match definition_for(ctx, instance_id) {
        Ok(definition) => definition,
        Err(error) => {
            eprintln!(
                "[snapshot] no automatic recovery point before '{label}' for '{instance_id}': {error}"
            );
            return Ok(());
        }
    };
    create_locked(
        ctx,
        instance_id,
        &definition,
        Some(label),
        SnapshotOrigin::Automatic,
    )
    .map(|_| ())
    .map_err(|error| format!("could not take a recovery point before '{label}': {error}"))
}

// ---------------------------------------------------------------------------
// Last-known-good
// ---------------------------------------------------------------------------

/// Set while a launch is in flight: the session and the snapshot that must be
/// promoted if the session ends well.
const LKG_PENDING: &str = ".agora_instance_lkg_pending";

/// The recovery point a launch may promote when the session ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrelaunchPoint {
    pub snapshot_id: String,
    /// False when the state had not changed since the last snapshot, so no new
    /// snapshot was written.
    pub created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingLaunch {
    session_id: String,
    snapshot_id: String,
}

/// The changes between an instance's current state and its last-known-good
/// snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LkgDiff {
    pub snapshot_id: String,
    pub files: Diff,
    /// Content-store items the current state names that the LKG did not.
    pub added_content: Vec<String>,
    /// Content-store items the LKG named that the current state no longer does.
    pub removed_content: Vec<String>,
    /// Load-order lists whose load-after rules changed, by list id.
    pub rules_changed: Vec<String>,
}

fn fingerprint_entries(entries: &[(String, Option<String>)], data: &GameSnapshotData) -> String {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(b"agora-instance-lkg-state-v1");
    for (path, hash) in entries {
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        match hash {
            Some(hash) => {
                hasher.update([1u8]);
                hasher.update(hash.as_bytes());
            }
            None => hasher.update([0u8]),
        }
    }
    hasher.update(serde_json::to_vec(data).unwrap_or_default());
    format!("{:x}", hasher.finalize())
}

fn live_fingerprint(files: &[SnapshotInputFile], data: &GameSnapshotData) -> String {
    let entries: Vec<(String, Option<String>)> = files
        .iter()
        .map(|file| {
            (
                file.relative_path.clone(),
                file.contents
                    .as_ref()
                    .map(|bytes| snapshot::sha256_hex(bytes)),
            )
        })
        .collect();
    fingerprint_entries(&entries, data)
}

fn snapshot_fingerprint(
    instance_dir: &Path,
    snapshot_id: &str,
    data: &GameSnapshotData,
) -> Result<String, String> {
    let entries = snapshot::snapshot_file_entries(instance_dir, snapshot_id)?;
    let entries: Vec<(String, Option<String>)> = entries
        .into_iter()
        .map(|entry| {
            (
                entry.relative_path,
                if entry.absent { None } else { entry.sha256 },
            )
        })
        .collect();
    Ok(fingerprint_entries(&entries, data))
}

/// The newest snapshot, when it is a readable game-instance snapshot whose
/// state can be compared. A damaged or foreign newest snapshot cannot prove
/// the state unchanged, so the caller takes a new one.
fn newest_comparable_snapshot(
    instance_dir: &Path,
) -> Result<Option<(Snapshot, GameSnapshotData)>, String> {
    let Some(newest) = snapshot::list_snapshots(instance_dir)?.into_iter().next() else {
        return Ok(None);
    };
    if newest.unreadable.is_some() {
        return Ok(None);
    }
    let Some(data) = snapshot_data(instance_dir, &newest.id)? else {
        return Ok(None);
    };
    Ok(Some((newest, data)))
}

/// Take the recovery point a launch may promote. The state is captured before
/// the game starts; an unchanged state reuses the newest snapshot rather than
/// writing another copy of the same state.
pub fn prepare_launch(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<PrelaunchPoint, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "launch-snapshot",
        )
        .map_err(|e| e.to_string())?;
    refuse_if_pending(&dir, None)?;

    let current_files = capture_files(ctx, instance_id, definition)?;
    let current_data = capture_data(ctx, instance_id, definition)?;
    let current = live_fingerprint(&current_files, &current_data);

    if let Some((snapshot, data)) = newest_comparable_snapshot(&dir)? {
        if snapshot_fingerprint(&dir, &snapshot.id, &data)? == current {
            return Ok(PrelaunchPoint {
                snapshot_id: snapshot.id,
                created: false,
            });
        }
    }

    let aux = aux_for(&current_data)?;
    let snapshot = snapshot::create_snapshot_from_data(
        &dir,
        Some("pre-launch"),
        &current_files,
        SnapshotOrigin::Automatic,
        &aux,
    )?;
    if let Err(error) = lkg::run_retention(&dir) {
        eprintln!("[snapshot] retention after pre-launch capture failed: {error}");
    }
    Ok(PrelaunchPoint {
        snapshot_id: snapshot.id,
        created: true,
    })
}

/// Tie a launch's recovery point to the session that will judge it. Without a
/// session there will be no verdict, so a launch that could not open one is not
/// recorded here and nothing is promoted later.
pub fn record_launch_point(
    ctx: &Ctx,
    instance_id: &str,
    session_id: Option<&str>,
    point: &PrelaunchPoint,
) -> Result<(), String> {
    let Some(session_id) = session_id else {
        return Ok(());
    };
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    let pending = PendingLaunch {
        session_id: session_id.to_string(),
        snapshot_id: point.snapshot_id.clone(),
    };
    let bytes = serde_json::to_vec(&pending)
        .map_err(|e| format!("could not serialize the launch recovery point: {e}"))?;
    snapshot::atomic_write_pub(&dir.join(LKG_PENDING), &bytes)
}

fn read_pending_launch(dir: &Path) -> Option<PendingLaunch> {
    let bytes = std::fs::read(dir.join(LKG_PENDING)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Map the session ledger's verdict to §23.2's launch classes.
///
/// `Normal` after the minimum runtime is the only success. A clean exit that
/// was too short is `Abandoned`; a crash, a freeze and a start failure are
/// `Crash`; a player Stop is `Cancelled`; an end nobody watched is `Unknown`.
/// None of those promotes.
pub fn outcome_from_verdict(verdict: SessionVerdict, runtime_ms: u64) -> LaunchOutcome {
    match verdict {
        SessionVerdict::Normal => {
            if runtime_ms >= lkg::DEFAULT_ABANDON_THRESHOLD_MS {
                LaunchOutcome::Success
            } else {
                LaunchOutcome::Abandoned
            }
        }
        SessionVerdict::Crashed { .. } | SessionVerdict::Froze | SessionVerdict::FailedToStart => {
            LaunchOutcome::Crash
        }
        SessionVerdict::Stopped => LaunchOutcome::Cancelled,
        SessionVerdict::ClosedUnknown => LaunchOutcome::Unknown,
    }
}

/// Record how a launch ended and promote its recovery point only for a
/// success. Called by the session watcher, which is the one place both front
/// ends report the game's end.
pub fn finish_launch_session(
    ctx: &Ctx,
    instance_id: &str,
    session_id: &str,
    verdict: Option<SessionVerdict>,
    runtime_ms: u64,
) -> Option<LaunchOutcome> {
    let dir = ctx.paths.instance_dir(instance_id).ok()?;
    let pending = read_pending_launch(&dir)?;
    if pending.session_id != session_id {
        return None;
    }
    // Remove it first: two watchers must not both promote the same launch.
    let _ = std::fs::remove_file(dir.join(LKG_PENDING));
    let outcome = match verdict {
        Some(verdict) => outcome_from_verdict(verdict, runtime_ms),
        // The ledger could not judge the session. "Could not tell" never promotes.
        None => LaunchOutcome::Unknown,
    };
    if let Err(error) = lkg::record_launch_outcome(
        &dir,
        Some(&pending.snapshot_id),
        session_id,
        outcome.clone(),
    ) {
        eprintln!("[lkg] could not record the launch outcome for '{instance_id}': {error}");
    }
    if let Err(error) = lkg::run_retention(&dir) {
        eprintln!("[snapshot] retention after a launch ended failed: {error}");
    }
    Some(outcome)
}

fn capture_files_as_entries(files: &[SnapshotInputFile]) -> Vec<FileEntry> {
    files
        .iter()
        .filter_map(|file| {
            file.contents.as_ref().map(|bytes| FileEntry {
                path: file.relative_path.clone(),
                sha256: snapshot::sha256_hex(bytes),
                size: bytes.len() as u64,
            })
        })
        .collect()
}

fn rules_by_list(data: &GameSnapshotData) -> std::collections::BTreeMap<&str, &[LoadAfterRule]> {
    data.rules
        .iter()
        .map(|list| (list.list.as_str(), list.rules.as_slice()))
        .collect()
}

/// What changed since the current last-known-good snapshot. A reordered plugin
/// list and a changed INI show up as modified state files; content items and
/// load-after rules are named separately because they are not files.
pub fn diff_since_lkg(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<LkgDiff, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    let state = lkg::read_lkg_state(&dir)?;
    let snapshot_id = state.current_lkg_snapshot_id.clone().ok_or_else(|| {
        format!(
            "'{instance_id}' has no last-known-good setup yet: launch it once, then make a change"
        )
    })?;
    diff_against_snapshot(ctx, instance_id, definition, &snapshot_id)
}

/// What changed between the current state and one snapshot. Used for "changes
/// since LKG" and for the snapshot diff a front end shows.
pub fn diff_against_snapshot(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    snapshot_id: &str,
) -> Result<LkgDiff, String> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;

    let reference = snapshot::snapshot_file_index(&dir, snapshot_id)?;
    let current_files = capture_files(ctx, instance_id, definition)?;
    let current_data = capture_data(ctx, instance_id, definition)?;
    let current = capture_files_as_entries(&current_files);
    let files = lkg::compute_diff(&reference, &current, Some(snapshot_id.to_string()), None);

    let reference_data = snapshot_data(&dir, snapshot_id)?.unwrap_or_default();
    let reference_content: std::collections::BTreeSet<String> =
        reference_data.content_refs.iter().cloned().collect();
    let current_content: std::collections::BTreeSet<String> =
        current_data.content_refs.iter().cloned().collect();
    let added_content = current_content
        .difference(&reference_content)
        .cloned()
        .collect();
    let removed_content = reference_content
        .difference(&current_content)
        .cloned()
        .collect();

    let list_ids: std::collections::BTreeSet<String> = reference_data
        .rules
        .iter()
        .chain(current_data.rules.iter())
        .map(|list| list.list.clone())
        .collect();
    let reference_rules = rules_by_list(&reference_data);
    let current_rules = rules_by_list(&current_data);
    let mut rules_changed: Vec<String> = list_ids
        .into_iter()
        .filter(|list| reference_rules.get(list.as_str()) != current_rules.get(list.as_str()))
        .collect();
    rules_changed.sort();
    rules_changed.dedup();

    Ok(LkgDiff {
        snapshot_id: snapshot_id.to_string(),
        files,
        added_content,
        removed_content,
        rules_changed,
    })
}

/// Restore an instance to the setup that last launched well, through the same
/// guarded restore a snapshot uses.
pub fn restore_to_lkg(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    force_while_running: bool,
) -> Result<GameRestoreOutcome, GameRestoreError> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    let state: LkgState = lkg::read_lkg_state(&dir).map_err(GameRestoreError::Other)?;
    let snapshot_id = state.current_lkg_snapshot_id.ok_or_else(|| {
        GameRestoreError::Other(format!(
            "'{instance_id}' has no last-known-good setup yet: launch it once to record one"
        ))
    })?;
    restore(
        ctx,
        instance_id,
        definition,
        &snapshot_id,
        force_while_running,
    )
}

/// The content-store items a snapshot named that can no longer be restored,
/// by item id (and the missing object path when the item itself still exists).
pub fn missing_content(ctx: &Ctx, refs: &[String]) -> Vec<String> {
    let mut missing = Vec::new();
    for item_id in refs {
        let path = ctx.paths.content_item_path(item_id);
        let item = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<crate::content_store::ContentItem>(&text).ok());
        let Some(item) = item else {
            missing.push(item_id.clone());
            continue;
        };
        for file in &item.files {
            if !ctx.paths.content_object_path(&file.sha256).is_file() {
                missing.push(format!("{item_id} ({})", file.path.as_str()));
            }
        }
    }
    missing
}

/// Keep the live instance's id and name after a snapshot wrote its manifest.
/// A snapshot captured from another instance (an imported backup, a clone's
/// source) may carry a different identity; restoring its state must not rename
/// the instance or change its id.
fn restore_manifest_identity(
    ctx: &Ctx,
    instance_id: &str,
    instance_dir: &Path,
    identity: Option<&(String, String)>,
) -> Result<(), String> {
    let manifest_path = instance_dir.join(INSTANCE_MANIFEST_ROOT);
    if !manifest_path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("cannot read the restored manifest: {error}"))?;
    let mut manifest: GameInstanceManifest = serde_json::from_str(&text)
        .map_err(|error| format!("the restored manifest cannot be read: {error}"))?;
    let (wanted_id, wanted_name) = match identity {
        Some((id, name)) => (id.as_str(), Some(name.as_str())),
        None => (instance_id, None),
    };
    let mut changed = false;
    if manifest.instance_id != wanted_id {
        manifest.instance_id = wanted_id.to_string();
        changed = true;
    }
    if let Some(name) = wanted_name {
        if manifest.name != name {
            manifest.name = name.to_string();
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    crate::game_deploy::write_instance_manifest_atomic(ctx, instance_id, &manifest)
        .map_err(|error| format!("the restored manifest could not keep its identity: {error}"))
}

fn pre_restore_label(at: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "pre-restore-{}",
        at.with_timezone(&chrono::Local).format("%Y%m%d-%H%M%S")
    )
}

fn activity_is_running(ctx: &Ctx, instance_id: &str) -> Result<bool, String> {
    match crate::instance_runtime::game_activity(ctx, instance_id) {
        crate::instance_runtime::GameActivity::Idle => Ok(false),
        crate::instance_runtime::GameActivity::Running => Ok(true),
        // A launch that never finished starting proves no game is running, but it does not prove
        // one is gone either, so the restore refuses — and says why, instead of claiming a game the
        // player could close.
        crate::instance_runtime::GameActivity::Starting => Err(format!(
            "a launch of '{instance_id}' was recorded but never finished starting"
        )),
        crate::instance_runtime::GameActivity::CouldNotTell(reason) => Err(format!(
            "could not tell whether '{instance_id}' is running: {reason}"
        )),
    }
}

fn roots_of(paths: &[String]) -> Vec<String> {
    let mut roots: Vec<String> = paths
        .iter()
        .filter_map(|path| path.split('/').next().map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    roots.sort();
    roots
}

/// Restore a game instance to a snapshot.
///
/// Takes a pre-restore snapshot first, verifies every state blob, writes the
/// state files atomically, then writes every list's rules in one database
/// transaction and marks the deployment for a rebuild. Refused while the game
/// is running, unless `force_while_running` is set (the caller has told the user
/// what that means). A content item the snapshot names but the store no longer
/// holds refuses the restore, naming it, before anything is touched.
pub fn restore(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    snapshot_id: &str,
    force_while_running: bool,
) -> Result<GameRestoreOutcome, GameRestoreError> {
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "restore_game_snapshot",
        )
        .map_err(|e| e.to_string())?;
    restore_locked(
        ctx,
        instance_id,
        definition,
        snapshot_id,
        force_while_running,
    )
}

/// Restore a game instance while the caller already holds the instance lock.
pub fn restore_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    snapshot_id: &str,
    force_while_running: bool,
) -> Result<GameRestoreOutcome, GameRestoreError> {
    let dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| e.to_string())?;
    // A restore replaces the manifest, but never the instance's identity. The
    // row is the authority; fall back to the live manifest for an instance that
    // has not been registered yet.
    let identity = game_instance::get(ctx, instance_id)
        .ok()
        .flatten()
        .map(|record| (record.instance_id, record.name))
        .or_else(|| {
            game_instance::get_manifest(ctx, instance_id)
                .ok()
                .map(|manifest| (manifest.instance_id, manifest.name))
        });

    let forced_while_not_idle = match activity_is_running(ctx, instance_id) {
        Ok(false) => false,
        Ok(true) if force_while_running => true,
        Ok(true) => {
            return Err(GameRestoreError::Running {
                instance_id: instance_id.to_string(),
            })
        }
        Err(error) if force_while_running => {
            eprintln!("[snapshot] restoring '{instance_id}' despite: {error}");
            true
        }
        Err(reason) => {
            return Err(GameRestoreError::CouldNotTell {
                instance_id: instance_id.to_string(),
                reason,
            })
        }
    };

    // Validate everything before a single live file moves.
    refuse_if_pending(&dir, Some(snapshot_id))?;
    let aux = snapshot::snapshot_aux(&dir, snapshot_id)?;
    let state = aux
        .state
        .ok_or_else(|| format!("snapshot {snapshot_id} is not a game-instance snapshot"))?;
    let data: GameSnapshotData = serde_json::from_value(state)
        .map_err(|e| format!("snapshot {snapshot_id} records unreadable game state: {e}"))?;

    let missing = missing_content(ctx, &data.content_refs);
    if !missing.is_empty() {
        return Err(GameRestoreError::Other(format!(
            "snapshot {snapshot_id} needs content the store no longer holds: {}; nothing was changed",
            missing.join(", ")
        )));
    }

    // The undo point records the current state, including the current rules, so
    // undoing the restore puts back everything the restore replaces.
    let current = capture_data(ctx, instance_id, definition)?;
    let undo_aux = aux_for(&current)?;
    let current_files = capture_files(ctx, instance_id, definition)?;
    let undo = snapshot::create_snapshot_from_data(
        &dir,
        Some(&pre_restore_label(chrono::Utc::now())),
        &current_files,
        SnapshotOrigin::Automatic,
        &undo_aux,
    )?;

    // Durable before the first write: an interruption names the snapshot that
    // puts the instance back.
    snapshot::atomic_write_pub(&pending_path(&dir), undo.id.as_bytes())?;

    let restored = match snapshot::restore_snapshot_data(&dir, snapshot_id) {
        Ok(restored) => restored,
        Err(error) => {
            let rollback = snapshot::restore_snapshot_data(&dir, &undo.id);
            let _ = std::fs::remove_file(pending_path(&dir));
            return Err(GameRestoreError::Other(match rollback {
                Ok(_) => format!("{error}; the instance was put back as it was"),
                Err(rollback) => format!(
                    "{error}; putting the instance back also failed ({rollback}). Restore \
                     snapshot {} to try again.",
                    undo.id
                ),
            }));
        }
    };

    if let Err(error) = restore_manifest_identity(ctx, instance_id, &dir, identity.as_ref()) {
        let rollback = snapshot::restore_snapshot_data(&dir, &undo.id);
        let _ = std::fs::remove_file(pending_path(&dir));
        return Err(GameRestoreError::Other(match rollback {
            Ok(_) => format!("{error}; the instance was put back as it was"),
            Err(rollback) => format!(
                "{error}; putting the instance back also failed ({rollback}). Restore snapshot {} \
                 to try again.",
                undo.id
            ),
        }));
    }

    let rules: Vec<(String, Vec<LoadAfterRule>)> = data
        .rules
        .iter()
        .map(|list| (list.list.clone(), list.rules.clone()))
        .collect();
    if let Err(error) = game_load_order::replace_all_load_after_rules(ctx, instance_id, &rules) {
        // Put the files and rules back: a restore either lands whole or not at all.
        let files = snapshot::restore_snapshot_data(&dir, &undo.id);
        let undo_rules: Vec<(String, Vec<LoadAfterRule>)> = current
            .rules
            .iter()
            .map(|list| (list.list.clone(), list.rules.clone()))
            .collect();
        let rules_back =
            game_load_order::replace_all_load_after_rules(ctx, instance_id, &undo_rules);
        let _ = std::fs::remove_file(pending_path(&dir));
        return Err(GameRestoreError::Other(match (files, rules_back) {
            (Ok(_), Ok(())) => format!(
                "the snapshot's rules could not be restored ({error}); the instance was put back as it was"
            ),
            (files, rules_back) => format!(
                "the snapshot's rules could not be restored ({error}); the attempt to put the \
                 instance back also failed (files: {files:?}, rules: {rules_back:?}). Undo snapshot \
                 {} holds the previous state.",
                undo.id
            ),
        }));
    }
    let _ = std::fs::remove_file(pending_path(&dir));

    if let Err(error) = crate::game_deploy::mark_for_redeploy(ctx, instance_id) {
        eprintln!(
            "[snapshot] restored '{instance_id}' but could not mark its deployment for a rebuild: {error}"
        );
    }
    if let Err(error) = crate::lkg::run_retention(&dir) {
        eprintln!("[snapshot] retention after restore failed: {error}");
    }

    let outcome = RestoreOutcome {
        snapshot_id: snapshot_id.to_string(),
        coverage: RestoreCoverageKind::Exact,
        restored_roots: roots_of(&restored),
        preserved_roots: Vec::new(),
        cleanup_warning: None,
        preserved_recovery_dir: None,
        undo_snapshot_id: Some(undo.id.clone()),
    };
    Ok(GameRestoreOutcome {
        outcome,
        undo_snapshot_id: undo.id,
        forced_while_not_idle,
    })
}

/// The state an instance's snapshot should record, for callers that want to
/// compare or inspect it without restoring.
pub fn snapshot_data(
    instance_dir: &Path,
    snapshot_id: &str,
) -> Result<Option<GameSnapshotData>, String> {
    let aux = snapshot::snapshot_aux(instance_dir, snapshot_id)?;
    let Some(state) = aux.state else {
        return Ok(None);
    };
    serde_json::from_value(state)
        .map(Some)
        .map_err(|e| format!("snapshot {snapshot_id} records unreadable game state: {e}"))
}

/// One instance's snapshot folder.
struct SnapshotFolder {
    instance_id: String,
    instance_dir: PathBuf,
    snapshots_dir: PathBuf,
    in_trash: bool,
}

fn delete_command(instance_id: &str, snapshot_id: &str, in_trash: bool) -> String {
    if in_trash {
        format!(
            "restore the instance from Recently deleted, then run: agora snapshots delete              {instance_id} {snapshot_id}"
        )
    } else {
        format!("agora snapshots delete {instance_id} {snapshot_id}")
    }
}

fn folder_instance_id(dir: &Path, fallback: String) -> String {
    std::fs::read_to_string(dir.join("instance_manifest.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<game_instance::GameInstanceManifest>(&text).ok())
        .map(|manifest| manifest.instance_id)
        .unwrap_or(fallback)
}

/// Every snapshot folder under the data root: live instances and instances
/// waiting in either Recently deleted (their snapshot store travels with them,
/// so a restore still needs their content).
fn snapshot_folders(ctx: &Ctx) -> Result<Vec<SnapshotFolder>, String> {
    snapshot_folders_with(ctx, |path| std::fs::symlink_metadata(path))
}

/// Folder discovery with the metadata lookups injected, so a failed lookup can
/// be exercised without depending on the OS to fail on cue. The public wrapper
/// always stats the real filesystem.
fn snapshot_folders_with(
    ctx: &Ctx,
    metadata: impl Fn(&Path) -> std::io::Result<std::fs::Metadata>,
) -> Result<Vec<SnapshotFolder>, String> {
    let mut folders = Vec::new();
    let mut roots: Vec<(std::path::PathBuf, bool)> = vec![(ctx.paths.instances_root(), false)];
    roots.push((ctx.paths.recently_deleted_games_root(), true));
    roots.push((ctx.paths.recently_deleted_root(), true));
    for (root, in_trash) in roots {
        #[cfg(test)]
        if root == ctx.paths.recently_deleted_games_root() {
            crate::instance_location_tests::pause(
                ctx,
                crate::instance_location_tests::ScanPoint::LiveSnapshots,
            );
        }
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot list '{}' to check snapshot content references: {error}",
                    root.display()
                ))
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "cannot read an entry in '{}' to check snapshot content references: {error}",
                    root.display()
                )
            })?;
            let fallback = entry.file_name().to_string_lossy().into_owned();
            let dir = if in_trash {
                entry.path().join("instance")
            } else {
                entry.path()
            };
            match metadata(&dir) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect '{}' to check snapshot content references: {error}",
                        dir.display()
                    ))
                }
            }
            let snapshots = dir.join(".agora_snapshots");
            match metadata(&snapshots) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect '{}' to check snapshot content references: {error}",
                        snapshots.display()
                    ))
                }
            }
            let instance_id = if in_trash {
                folder_instance_id(&dir, fallback)
            } else {
                fallback
            };
            folders.push(SnapshotFolder {
                instance_id,
                instance_dir: dir,
                snapshots_dir: snapshots,
                in_trash,
            });
        }
    }
    #[cfg(test)]
    crate::instance_location_tests::pause(
        ctx,
        crate::instance_location_tests::ScanPoint::SnapshotFolders,
    );
    Ok(folders)
}

/// The snapshots that name a content item, and the ones that cannot be read.
///
/// Fails closed on a folder that cannot be listed: if the answer cannot be
/// determined, removal must treat it as "still needed". Caller holds InstanceLocations
/// through both folder discovery and manifest reads.
pub(crate) fn snapshots_using_content_locked(
    ctx: &Ctx,
    item_id: &str,
) -> Result<SnapshotContentUsage, String> {
    let mut usage = SnapshotContentUsage::default();
    for folder in snapshot_folders(ctx)? {
        let entries = snapshot::list_snapshots(&folder.instance_dir).map_err(|error| {
            format!(
                "cannot list snapshots for instance '{}': {error}",
                folder.instance_id
            )
        })?;
        for entry in entries {
            let json = folder.snapshots_dir.join(format!("{}.json", entry.id));
            let path = if json.is_file() {
                json
            } else {
                folder.snapshots_dir.join(format!("{}.zip", entry.id))
            };
            let command = delete_command(&folder.instance_id, &entry.id, folder.in_trash);
            if let Some(error) = &entry.unreadable {
                usage.unreadable.push(UnreadableSnapshotReference {
                    instance_id: folder.instance_id.clone(),
                    snapshot_id: entry.id.clone(),
                    created_at: entry.created_at.clone(),
                    path: path.display().to_string(),
                    error: error.clone(),
                    command,
                    in_trash: folder.in_trash,
                });
                continue;
            }
            match snapshot::snapshot_aux(&folder.instance_dir, &entry.id) {
                Ok(aux) => {
                    if aux.content_refs.iter().any(|content| content == item_id) {
                        usage.using.push(SnapshotReference {
                            instance_id: folder.instance_id.clone(),
                            snapshot_id: entry.id.clone(),
                            label: entry.label.clone(),
                            created_at: entry.created_at.clone(),
                            path: path.display().to_string(),
                            command,
                            in_trash: folder.in_trash,
                        });
                    }
                }
                // `list_snapshots` called it readable, so this is a race or a
                // partly written side file. Treat it as unreadable: fail closed.
                Err(error) => usage.unreadable.push(UnreadableSnapshotReference {
                    instance_id: folder.instance_id.clone(),
                    snapshot_id: entry.id.clone(),
                    created_at: entry.created_at.clone(),
                    path: path.display().to_string(),
                    error,
                    command,
                    in_trash: folder.in_trash,
                }),
            }
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn review_snapshot_folders_refuse_when_folder_metadata_fails() {
        let tmp = TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        let blocked = ctx
            .paths
            .instances_root()
            .join("one")
            .join(".agora_snapshots");
        std::fs::create_dir_all(&blocked).unwrap();

        let error = snapshot_folders_with(&ctx, |path| {
            if path == blocked {
                Err(std::io::Error::other("metadata unavailable"))
            } else {
                std::fs::symlink_metadata(path)
            }
        })
        .err()
        .expect("cannot tell whether the folder holds snapshots");
        assert!(
            error.contains("metadata unavailable"),
            "the reason is reported: {error}"
        );
    }
}
