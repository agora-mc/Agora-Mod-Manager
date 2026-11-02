//! Per-user game files managed by journaled swap (MASTER_SPEC §26.5).
//!
//! Skyrim and Creation Engine games keep plugin lists and configuration INIs
//! in the user's profile (`LocalAppData`, `Documents`), one copy per store,
//! shared by every instance of that game.
//!
//! Each instance needs its own copies, swapped in for the launch session and
//! put back afterwards, without losing user files.
//!
//! All session state is journaled on disk at `<data>/user-files/<game>_<store>/journal.json`
//! so interruptions, crashes, or ungraceful terminations can be safely recovered.

use std::path::{Path, PathBuf};

use agora_game_api::{
    fold_path_case, GameDefinition, GameId, GamePath, RelPath, StoreId, UserDataLocation,
    UserFileMapping, UserFileStrategy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ctx::Ctx;
use crate::lock_manager::LockResource;
use crate::process_identity::{self, ProcessIdentity};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum UserFilesError {
    #[error("{0}")]
    SessionRunning(String),
    #[error("cannot restore while session is running: instance '{instance}' is active")]
    CannotRestoreRunning { instance: String },
    #[error("no session found for game '{game}' ({store})")]
    NoSession { game: GameId, store: StoreId },
    #[error("user data location '{0:?}' could not be resolved")]
    UserDataNotFound(UserDataLocation),
    #[error("hash mismatch on file '{path}': expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("lock error: {0}")]
    Lock(#[from] crate::error::LauncherError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot write '{path}': {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// The record of a user file session on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub game: GameId,
    pub store: StoreId,
    pub instance_id: String,
    pub created_at: String,
    pub running_from: PathBuf,
    pub processes: Vec<ProcessIdentity>,
    /// The Agora process that swapped these files in and has not yet recorded the game's process.
    /// While it is alive the session counts as running: a launch between the swap and the spawn is
    /// still a launch. A crash leaves a dead identity, so recovery is not blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_launcher: Option<ProcessIdentity>,
    pub files: Vec<JournaledFile>,
}

/// How far the write-ahead record of one file has got. A stop leaves the last state written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// The player's file is hashed and recorded; no backup is complete yet.
    BackingUp,
    /// A verified backup exists; the real file is untouched.
    BackedUp,
    /// The instance's file has been (or is about to be) written over the real file.
    Swapped,
}

/// The state a journal entry records. Old journals have no `state`; their `swapped` flag and
/// recorded hashes say how far they got.
fn file_state(file: &JournaledFile) -> FileState {
    match file.state {
        Some(state) => state,
        None if file.swapped => FileState::Swapped,
        None if file.backup_path.is_some() || file.original_sha256.is_some() => FileState::BackedUp,
        None => FileState::BackingUp,
    }
}

/// A specific file tracked in the session journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournaledFile {
    pub real_path: PathBuf,
    pub instance_path: RelPath,
    pub instance_copy_path: PathBuf,
    pub existed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_sha256: Option<String>,
    pub written_sha256: String,
    #[serde(default)]
    pub swapped: bool,
    /// The write-ahead state, when this journal was written by the hash-first protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<FileState>,
    /// Set once this entry's original is back on disk, so a re-run of an interrupted restore does
    /// not put the user's file over the session's saved copy again.
    #[serde(default)]
    pub restored: bool,
}

/// Status of an active or recoverable user-file session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub instance: String,
    pub running: bool,
    pub files: Vec<JournaledFile>,
}

/// Status of a file restored from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoredFile {
    pub real_path: PathBuf,
    pub instance_path: RelPath,
    pub changed: bool,
    pub original_sha256: Option<String>,
    pub final_sha256: Option<String>,
}

/// Summary report after restoring a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreReport {
    pub game: GameId,
    pub store: StoreId,
    pub instance_id: String,
    pub files: Vec<RestoredFile>,
}

impl RestoreReport {
    pub fn changed_files(&self) -> Vec<&RestoredFile> {
        self.files.iter().filter(|f| f.changed).collect()
    }
}

/// Outcome of a swap-in operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapOutcome {
    pub swapped_files: usize,
}

// ---------------------------------------------------------------------------
// UserData resolution
// ---------------------------------------------------------------------------

/// Resolving `UserData` goes through this single function core owns.
/// An environment variable `AGORA_TEST_USER_DATA_ROOT=<dir>` redirects
/// Documents, LocalAppData, RoamingAppData and Home for hermetic tests.
pub fn user_data_root(location: &UserDataLocation) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AGORA_TEST_USER_DATA_ROOT") {
        if !dir.is_empty() {
            let base = PathBuf::from(dir);
            return Some(match location {
                UserDataLocation::Documents => base.join("documents"),
                UserDataLocation::LocalAppData => base.join("local"),
                UserDataLocation::RoamingAppData => base.join("roaming"),
                UserDataLocation::Home => base.join("home"),
            });
        }
    }
    match location {
        UserDataLocation::Documents => dirs::document_dir(),
        UserDataLocation::RoamingAppData => dirs::data_dir(),
        UserDataLocation::LocalAppData => dirs::data_local_dir(),
        UserDataLocation::Home => dirs::home_dir(),
    }
}

/// Resolve a relative path component-by-component, matching case-insensitively
/// when an entry already exists on disk (e.g. matching `plugins.txt` to `Plugins.txt`).
pub fn resolve_rel_case_insensitive(base: &Path, rel: &str) -> PathBuf {
    let mut current = base.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() {
            continue;
        }
        let mut matched = false;
        if let Ok(entries) = std::fs::read_dir(&current) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if fold_path_case(&name.to_string_lossy()) == fold_path_case(part) {
                    current.push(name);
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            current.push(part);
        }
    }
    current
}

/// Compare two paths component-by-component case-insensitively.
pub fn paths_eq_case_insensitive(a: &Path, b: &Path) -> bool {
    let a_comps: Vec<_> = a
        .components()
        .map(|c| fold_path_case(&c.as_os_str().to_string_lossy()))
        .collect();
    let b_comps: Vec<_> = b
        .components()
        .map(|c| fold_path_case(&c.as_os_str().to_string_lossy()))
        .collect();
    a_comps == b_comps
}

/// Resolve a `GamePath` targeting `UserData` to its absolute host path.
pub fn resolve_user_file_source(source: &GamePath) -> Result<PathBuf, UserFilesError> {
    match source {
        GamePath::UserData { location, path } => {
            let Some(base) = user_data_root(location) else {
                return Err(UserFilesError::UserDataNotFound(location.clone()));
            };
            let rel = path.as_str();
            if rel.is_empty() {
                Ok(base)
            } else {
                Ok(resolve_rel_case_insensitive(&base, rel))
            }
        }
        other => Err(UserFilesError::Other(format!(
            "unsupported user file source root: {other:?}"
        ))),
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Write a user file, naming it when the write is refused (a read-only file the game changed).
fn write_user_file(path: &Path, bytes: &[u8]) -> Result<(), UserFilesError> {
    std::fs::write(path, bytes).map_err(|source| UserFilesError::Write {
        path: path.to_path_buf(),
        source,
    })
}

fn write_journal(journal_path: &Path, journal: &Journal) -> Result<(), UserFilesError> {
    if let Some(parent) = journal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(journal)?;
    let tmp_path = journal_path.with_extension("tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(data.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, journal_path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Running session detection
// ---------------------------------------------------------------------------

/// Check whether a journal's session is still running.
/// A session is running if any recorded process identity is still alive
/// (`process_identity::verify`) or any process runs from the recorded folder
/// (`processes_running_from`).
pub fn is_session_running(journal: &Journal) -> bool {
    if session_running_apart_from_starting(journal) {
        return true;
    }
    journal
        .starting_launcher
        .as_ref()
        .is_some_and(|launcher| process_identity::verify(launcher).is_ok())
}

/// The session's own processes, and any process running from its game folder. A launch that has
/// swapped in but has not recorded its game yet is not counted here.
fn session_running_apart_from_starting(journal: &Journal) -> bool {
    for proc in &journal.processes {
        if process_identity::verify(proc).is_ok() {
            return true;
        }
    }
    if journal.running_from.exists() {
        let procs = crate::game_launch::processes_running_from(&journal.running_from);
        if !procs.is_empty() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Core Operations: swap_in, restore, status
// ---------------------------------------------------------------------------

/// Swap in per-user files for an instance launch session.
pub fn swap_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    running_from: &Path,
) -> Result<SwapOutcome, UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(definition.id.clone(), store.clone()),
        "swap-in",
    )?;

    // An interrupted tool swap for this game and store is put back before a session starts
    // (MASTER_SPEC §26.9). The lock is held here, so the check does not take it again.
    crate::game_tool_swap::recover_store_unlocked(ctx, definition.id.as_str(), store.as_str())
        .map_err(|e| UserFilesError::Other(e.to_string()))?;

    let journal_path = ctx
        .paths
        .user_files_journal_path(definition.id.as_str(), store.as_str());

    // 1. If a journal exists, check if running.
    if journal_path.exists() {
        let text = std::fs::read_to_string(&journal_path)?;
        let existing: Journal = serde_json::from_str(&text)?;
        if is_session_running(&existing) {
            let store_display = match store.as_str() {
                "steam" => "Steam",
                "gog" => "GOG",
                other => other,
            };
            let game_display = if definition.name.is_empty() {
                definition.id.as_str()
            } else {
                &definition.name
            };
            return Err(UserFilesError::SessionRunning(format!(
                "{} ({}) is running as instance {}; close it first",
                game_display, store_display, existing.instance_id
            )));
        }
        // Not running: recover/restore the previous session before proceeding.
        let _ = restore_journal_internal(ctx, definition, &existing)?;
    }

    // 2. Filter mappings that apply to this store
    let applicable_mappings: Vec<&UserFileMapping> = definition
        .user_files
        .iter()
        .filter(|m| m.strategy == UserFileStrategy::JournaledSwap && m.applies_to_store(store))
        .collect();

    if applicable_mappings.is_empty() {
        return Ok(SwapOutcome { swapped_files: 0 });
    }

    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| UserFilesError::Other(e.to_string()))?;
    let backup_dir = ctx
        .paths
        .user_files_backup_dir(definition.id.as_str(), store.as_str());
    std::fs::create_dir_all(&backup_dir)?;

    // 3. Settle the mappings before anything else. Two mappings for one real file, or two that
    // share one instance path, cannot be swapped and restored unambiguously: the journal is looked
    // up by instance path, and two entries for one real file would back up each other's writes.
    // Refuse and name both, touching nothing.
    let mut chosen: Vec<(&UserFileMapping, PathBuf)> = Vec::new();
    let mut real_to_instance: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut instance_to_real: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in &applicable_mappings {
        let real_path = resolve_user_file_source(&m.source)?;
        let real_key = fold_path_case(&real_path.to_string_lossy());
        let instance_key = fold_path_case(m.instance_path.as_str());
        if let Some(other) = real_to_instance.get(&real_key) {
            return Err(UserFilesError::Other(format!(
                "two user-file mappings name the same real file '{}' (instance paths '{}' and '{}'); fix the game definition",
                real_path.display(),
                other,
                m.instance_path
            )));
        }
        if let Some(other) = instance_to_real.get(&instance_key) {
            return Err(UserFilesError::Other(format!(
                "two user-file mappings share instance path '{}' (real files '{}' and '{}'); fix the game definition",
                m.instance_path,
                other,
                real_path.display()
            )));
        }
        real_to_instance.insert(real_key, m.instance_path.as_str().to_string());
        instance_to_real.insert(instance_key, real_path.display().to_string());
        chosen.push((m, real_path));
    }

    // 4. Initial journal record, before touching any real file.
    let journal_files: Vec<JournaledFile> = chosen
        .into_iter()
        .map(|(m, real_path)| JournaledFile {
            real_path,
            instance_path: m.instance_path.clone(),
            instance_copy_path: instance_dir.join(m.instance_path.as_str()),
            existed: false,
            backup_path: None,
            original_sha256: None,
            written_sha256: String::new(),
            swapped: false,
            state: Some(FileState::BackingUp),
            restored: false,
        })
        .collect();

    let mut journal = Journal {
        game: definition.id.clone(),
        store: store.clone(),
        instance_id: instance_id.to_string(),
        created_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
        running_from: running_from.to_path_buf(),
        processes: Vec::new(),
        starting_launcher: process_identity::capture_self(),
        files: journal_files,
    };

    write_journal(&journal_path, &journal)?;

    // 5. Hash first, then back up, then journal, then swap. Every journal write is fsynced, so a
    // stop leaves a record of exactly what the disk may hold: a complete backup is never mistaken
    // for the player's file until its hash matches the hash recorded before the copy.
    let swap_result: Result<(), UserFilesError> = (|| {
        for i in 0..journal.files.len() {
            let real_path = journal.files[i].real_path.clone();
            let instance_copy_path = journal.files[i].instance_copy_path.clone();
            let final_backup = backup_dir.join(format!("{i}"));
            let temp_backup = backup_dir.join(format!("{i}.tmp"));

            if real_path.exists() {
                let original_bytes = std::fs::read(&real_path)?;
                let orig_hash = hash_bytes(&original_bytes);

                // The player's hash is durable before any copy exists.
                journal.files[i].existed = true;
                journal.files[i].backup_path = None;
                journal.files[i].original_sha256 = Some(orig_hash.clone());
                journal.files[i].written_sha256 = String::new();
                journal.files[i].swapped = false;
                journal.files[i].state = Some(FileState::BackingUp);
                journal.files[i].restored = false;
                write_journal(&journal_path, &journal)?;

                // Copy to a temp name, make it durable, verify it, and only then give it the final
                // name. A torn copy never becomes the backup restore trusts.
                let _ = std::fs::remove_file(&temp_backup);
                {
                    // A fresh file, not a permission-preserving copy: a read-only player file must
                    // not make its backup read-only, because the backup is verified and then used.
                    let mut source = std::fs::File::open(&real_path)?;
                    let mut copy = std::fs::File::create(&temp_backup)?;
                    std::io::copy(&mut source, &mut copy)?;
                    copy.sync_all()?;
                }
                let backup_bytes = std::fs::read(&temp_backup)?;
                let b_hash = hash_bytes(&backup_bytes);
                if b_hash != orig_hash {
                    let _ = std::fs::remove_file(&temp_backup);
                    return Err(UserFilesError::HashMismatch {
                        path: temp_backup,
                        expected: orig_hash,
                        actual: b_hash,
                    });
                }
                let _ = std::fs::remove_file(&final_backup);
                std::fs::rename(&temp_backup, &final_backup)?;

                journal.files[i].backup_path = Some(final_backup);
                journal.files[i].state = Some(FileState::BackedUp);
                write_journal(&journal_path, &journal)?;

                let instance_bytes = if instance_copy_path.exists() {
                    Some(std::fs::read(&instance_copy_path)?)
                } else {
                    None
                };
                let written_hash = match &instance_bytes {
                    Some(bytes) => hash_bytes(bytes),
                    // First launch keeps the user's settings: write nothing and
                    // record original hash as what Agora "wrote".
                    None => orig_hash.clone(),
                };

                journal.files[i].written_sha256 = written_hash;
                journal.files[i].swapped = true;
                journal.files[i].state = Some(FileState::Swapped);
                write_journal(&journal_path, &journal)?;

                if let Some(instance_bytes) = instance_bytes {
                    write_user_file(&real_path, &instance_bytes)?;
                }
            } else {
                let instance_bytes = if instance_copy_path.exists() {
                    Some(std::fs::read(&instance_copy_path)?)
                } else {
                    None
                };
                let written_hash = match &instance_bytes {
                    Some(bytes) => hash_bytes(bytes),
                    None => String::new(),
                };

                journal.files[i].existed = false;
                journal.files[i].backup_path = None;
                journal.files[i].original_sha256 = None;
                journal.files[i].written_sha256 = written_hash;
                journal.files[i].swapped = true;
                journal.files[i].state = Some(FileState::Swapped);
                journal.files[i].restored = false;
                write_journal(&journal_path, &journal)?;

                if let Some(instance_bytes) = instance_bytes {
                    if let Some(parent) = real_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    write_user_file(&real_path, &instance_bytes)?;
                }
            }
        }
        Ok(())
    })();

    if let Err(e) = swap_result {
        // Any error restores what was swapped so far and returns the error.
        let _ = restore_journal_internal(ctx, definition, &journal);
        return Err(e);
    }

    Ok(SwapOutcome {
        swapped_files: journal.files.len(),
    })
}

/// Restore user files after session ends, at next launch, or by command.
pub fn restore(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(definition.id.clone(), store.clone()),
        "restore",
    )?;

    let journal_path = ctx
        .paths
        .user_files_journal_path(definition.id.as_str(), store.as_str());
    if !journal_path.exists() {
        return Err(UserFilesError::NoSession {
            game: definition.id.clone(),
            store: store.clone(),
        });
    }

    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;

    if session_running_apart_from_starting(&journal) {
        // The restore outcome is known now. If this process is the one that swapped the files in,
        // its "starting" mark has done its job: keeping it would make another Agora refuse until
        // this process exits, even after the game and its helpers are gone.
        if journal
            .starting_launcher
            .as_ref()
            .is_some_and(|launcher| launcher.pid == std::process::id())
        {
            journal.starting_launcher = None;
            write_journal(&journal_path, &journal)?;
        }
        return Err(UserFilesError::CannotRestoreRunning {
            instance: journal.instance_id.clone(),
        });
    }
    match journal.starting_launcher.as_ref() {
        Some(launcher) if process_identity::verify(launcher).is_ok() => {
            // The process that started the launch is the one restoring: this is a launch that never
            // reported a game, not a second launch. Any other process restoring must wait for it.
            if launcher.pid != std::process::id() {
                return Err(UserFilesError::CannotRestoreRunning {
                    instance: journal.instance_id.clone(),
                });
            }
            journal.starting_launcher = None;
            write_journal(&journal_path, &journal)?;
        }
        _ => {}
    }

    restore_journal_internal(ctx, definition, &journal)
}

/// Clear the "starting" mark of a launch that will never spawn. The caller then restores its
/// files; a launch that failed must not leave the session counting as running.
pub fn finish_starting(ctx: &Ctx, game: &GameId, store: &StoreId) -> Result<(), UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(game.clone(), store.clone()),
        "finish-starting",
    )?;
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    let ours = journal
        .starting_launcher
        .as_ref()
        .is_some_and(|launcher| launcher.pid == std::process::id());
    if ours {
        journal.starting_launcher = None;
        write_journal(&journal_path, &journal)?;
    }
    Ok(())
}

fn restore_journal_internal(
    ctx: &Ctx,
    definition: &GameDefinition,
    journal: &Journal,
) -> Result<RestoreReport, UserFilesError> {
    if journal.game != definition.id {
        return Err(UserFilesError::Other(format!(
            "journal game '{}' does not match definition '{}'",
            journal.game, definition.id
        )));
    }

    let session_dir = ctx
        .paths
        .user_files_dir(definition.id.as_str(), journal.store.as_str());
    let journal_path = ctx
        .paths
        .user_files_journal_path(definition.id.as_str(), journal.store.as_str());
    let backup_dir = ctx
        .paths
        .user_files_backup_dir(definition.id.as_str(), journal.store.as_str());

    // 1. Validate instance_id the way AppPaths does
    let instance_dir = ctx
        .paths
        .instance_dir(&journal.instance_id)
        .map_err(|e| UserFilesError::Other(e.to_string()))?;

    // 2. Applicable mappings for this store
    let applicable_mappings: Vec<&UserFileMapping> = definition
        .user_files
        .iter()
        .filter(|m| {
            m.strategy == UserFileStrategy::JournaledSwap && m.applies_to_store(&journal.store)
        })
        .collect();

    // 3. Pre-validate all journal entries BEFORE modifying or deleting any file on disk.
    // "A journal entry whose instance_path matches no mapping for that store,
    // or whose recorded real_path differs from the derived one (case-insensitive),
    // makes restore refuse, keep the journal and change nothing."
    struct ValidatedEntry {
        index: usize,
        file: JournaledFile,
        instance_path: RelPath,
        original_sha256: Option<String>,
        derived_real_path: PathBuf,
        derived_instance_copy: PathBuf,
        derived_backup_path: PathBuf,
        state: FileState,
        existed: bool,
    }

    let mut journal = journal.clone();
    let mut validated_entries = Vec::with_capacity(journal.files.len());

    for (i, file) in journal.files.iter().enumerate() {
        let mapping = applicable_mappings
            .iter()
            .find(|m| m.instance_path == file.instance_path);
        let Some(mapping) = mapping else {
            return Err(UserFilesError::Other(format!(
                "journal entry instance_path '{}' matches no mapping for store '{}'",
                file.instance_path.as_str(),
                journal.store
            )));
        };

        let derived_real_path = resolve_user_file_source(&mapping.source)?;
        if !paths_eq_case_insensitive(&file.real_path, &derived_real_path) {
            return Err(UserFilesError::Other(format!(
                "journal entry real_path '{:?}' differs from derived real_path '{:?}'",
                file.real_path, derived_real_path
            )));
        }

        let derived_instance_copy = instance_dir.join(mapping.instance_path.as_str());
        let derived_backup_path = backup_dir.join(format!("{i}"));

        if let Some(ref bp) = file.backup_path {
            if !paths_eq_case_insensitive(bp, &derived_backup_path) {
                return Err(UserFilesError::Other(format!(
                    "journal backup path '{:?}' does not match expected backup index path '{:?}'",
                    bp, derived_backup_path
                )));
            }
        }

        // A backup on disk proves the real file was there, even when a stop happened before the
        // journal said so. It is only usable when its hash matches the recorded original.
        let has_backup = derived_backup_path.exists();
        let existed = file.existed || has_backup || file.original_sha256.is_some();

        validated_entries.push(ValidatedEntry {
            index: i,
            file: file.clone(),
            instance_path: mapping.instance_path.clone(),
            original_sha256: file.original_sha256.clone(),
            derived_real_path,
            derived_instance_copy,
            derived_backup_path,
            state: file_state(file),
            existed,
        });
    }

    // 4. Restore trusts only what it can prove. A backup is written only when its hash equals the
    // journal's recorded original. A real file that still holds that original never changed (or was
    // already put back): it and the instance copy are left alone. A torn or unverifiable backup is
    // never written over anything; the entry is reported with both paths and everything is kept.
    let mut restored_files = Vec::new();

    for entry in &validated_entries {
        if entry.file.restored {
            continue;
        }

        let real_bytes = if entry.derived_real_path.exists() {
            Some(std::fs::read(&entry.derived_real_path)?)
        } else {
            None
        };
        let real_hash = real_bytes.as_ref().map(|bytes| hash_bytes(bytes));
        let backup_bytes = if entry.derived_backup_path.exists() {
            Some(std::fs::read(&entry.derived_backup_path)?)
        } else {
            None
        };
        let backup_hash = backup_bytes.as_ref().map(|bytes| hash_bytes(bytes));

        // The real file still holding the recorded original means the swap never changed it (or an
        // earlier run already restored it). With no recorded hash, a backup byte-identical to the
        // real file proves the same thing.
        let real_is_original = entry.original_sha256.is_some()
            && real_hash.as_deref() == entry.original_sha256.as_deref();
        let real_matches_backup =
            real_hash.is_some() && backup_hash.is_some() && real_hash == backup_hash;

        if real_is_original || real_matches_backup {
            if entry.derived_backup_path.exists() {
                let _ = std::fs::remove_file(&entry.derived_backup_path);
            }
            let _ = std::fs::remove_file(entry.derived_backup_path.with_extension("tmp"));
            journal.files[entry.index].restored = true;
            write_journal(&journal_path, &journal)?;
            restored_files.push(RestoredFile {
                real_path: entry.derived_real_path.clone(),
                instance_path: entry.instance_path.clone(),
                changed: false,
                original_sha256: entry.original_sha256.clone(),
                final_sha256: real_hash,
            });
            continue;
        }

        if entry.state != FileState::Swapped {
            // The journal records that no write reached the real file. A backup that matches it was
            // already handled; one that does not may be the only copy of the player's file, so it
            // is kept and reported instead of removed.
            if entry.derived_backup_path.exists() {
                return Err(UserFilesError::Other(format!(
                    "the user file '{}' does not match its backup '{}', and the journal records no swap; both were left in place",
                    entry.derived_real_path.display(),
                    entry.derived_backup_path.display()
                )));
            }
            let _ = std::fs::remove_file(entry.derived_backup_path.with_extension("tmp"));
            journal.files[entry.index].restored = true;
            write_journal(&journal_path, &journal)?;
            continue;
        }

        if entry.existed {
            let Some(original) = entry.original_sha256.as_deref() else {
                return Err(UserFilesError::Other(format!(
                    "the backup '{}' of the user file '{}' cannot be verified: the journal has no original hash; both were left in place",
                    entry.derived_backup_path.display(),
                    entry.derived_real_path.display()
                )));
            };
            let Some(backup_hash) = backup_hash.as_deref() else {
                return Err(UserFilesError::Other(format!(
                    "the backup '{}' of the user file '{}' is missing; the file and the journal were left in place",
                    entry.derived_backup_path.display(),
                    entry.derived_real_path.display()
                )));
            };
            if backup_hash != original {
                return Err(UserFilesError::HashMismatch {
                    path: entry.derived_backup_path.clone(),
                    expected: original.to_string(),
                    actual: backup_hash.to_string(),
                });
            }

            // The session's saved copy is the game's changed file, and only when the game changed
            // it from what Agora wrote.
            if let Some(current) = &real_bytes {
                if real_hash.as_deref() != Some(entry.file.written_sha256.as_str()) {
                    if let Some(parent) = entry.derived_instance_copy.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    write_user_file(&entry.derived_instance_copy, current)?;
                }
            }

            let backup_bytes = backup_bytes.expect("checked above");
            if let Some(parent) = entry.derived_real_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            write_user_file(&entry.derived_real_path, &backup_bytes)?;
        } else {
            // Agora created this file during the session; the session's copy goes back and the real
            // file goes.
            if let Some(current) = &real_bytes {
                if real_hash.as_deref() != Some(entry.file.written_sha256.as_str()) {
                    if let Some(parent) = entry.derived_instance_copy.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    write_user_file(&entry.derived_instance_copy, current)?;
                }
            }
            if entry.derived_real_path.exists() {
                std::fs::remove_file(&entry.derived_real_path)?;
            }
        }

        let final_sha256 = if entry.derived_real_path.exists() {
            let bytes = std::fs::read(&entry.derived_real_path)?;
            Some(hash_bytes(&bytes))
        } else {
            None
        };

        // Record the entry before its backup can go, so a stop from here on does not restore it a
        // second time.
        journal.files[entry.index].restored = true;
        write_journal(&journal_path, &journal)?;
        let _ = std::fs::remove_file(&entry.derived_backup_path);
        let _ = std::fs::remove_file(entry.derived_backup_path.with_extension("tmp"));

        restored_files.push(RestoredFile {
            real_path: entry.derived_real_path.clone(),
            instance_path: entry.instance_path.clone(),
            changed: real_hash != entry.original_sha256,
            original_sha256: entry.original_sha256.clone(),
            final_sha256,
        });
    }

    // Delete the journal and backups only after every file is restored.
    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }
    if journal_path.exists() {
        let _ = std::fs::remove_file(&journal_path);
    }
    if session_dir.exists() {
        let _ = std::fs::remove_dir(&session_dir);
    }

    Ok(RestoreReport {
        game: journal.game.clone(),
        store: journal.store.clone(),
        instance_id: journal.instance_id.clone(),
        files: restored_files,
    })
}

/// Record a launched process identity into the session journal.
pub fn record_process(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
    identity: ProcessIdentity,
) -> Result<(), UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(game.clone(), store.clone()),
        "record-process",
    )?;

    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(());
    }

    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    journal.processes.push(identity);
    // The game now has an identity that says whether it is alive; the launcher mark is done.
    journal.starting_launcher = None;
    write_journal(&journal_path, &journal)?;
    Ok(())
}

/// Inspect the status of a user-file session for a game and store. An unreadable journal is an
/// error, never "no session": launching over it would have to guess what it holds.
pub fn status(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
) -> Result<Option<SessionStatus>, UserFilesError> {
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&journal_path).map_err(|e| {
        UserFilesError::Other(format!(
            "the user-files journal {} cannot be read: {e}",
            journal_path.display()
        ))
    })?;
    let journal: Journal = serde_json::from_str(&text).map_err(|e| {
        UserFilesError::Other(format!(
            "the user-files journal {} cannot be read: {e}",
            journal_path.display()
        ))
    })?;
    let running = is_session_running(&journal);
    Ok(Some(SessionStatus {
        instance: journal.instance_id,
        running,
        files: journal.files,
    }))
}

/// List active/recoverable user-file sessions across all games, optionally filtered.
pub fn list_statuses(
    ctx: &Ctx,
    filter_game: Option<&GameId>,
) -> Result<Vec<(GameId, StoreId, SessionStatus)>, UserFilesError> {
    let root = ctx.paths.user_files_root();
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let entries = std::fs::read_dir(&root)?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let journal_path = path.join("journal.json");
        if !journal_path.exists() {
            continue;
        }
        // An unreadable journal is a session nobody can see the state of: report it rather
        // than listing "no sessions", which would invite launching over it.
        let journal = std::fs::read_to_string(&journal_path)
            .map_err(UserFilesError::from)
            .and_then(|text| serde_json::from_str::<Journal>(&text).map_err(UserFilesError::from))
            .map_err(|e| {
                UserFilesError::Other(format!(
                    "the user-files journal {} cannot be read: {e}",
                    journal_path.display()
                ))
            })?;
        if let Some(filter) = filter_game {
            if &journal.game != filter {
                continue;
            }
        }
        let running = is_session_running(&journal);
        results.push((
            journal.game,
            journal.store,
            SessionStatus {
                instance: journal.instance_id,
                running,
                files: journal.files,
            },
        ));
    }

    results.sort_by(|a, b| (a.0.as_str(), a.1.as_str()).cmp(&(b.0.as_str(), b.1.as_str())));
    Ok(results)
}
