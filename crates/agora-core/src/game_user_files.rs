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
    #[error(
        "the backup '{path}' of the user file '{real_path}' does not match the recorded original: expected {expected}, got {actual}"
    )]
    HashMismatch {
        path: PathBuf,
        real_path: PathBuf,
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
    /// Identifies the launch that swapped these files in. The desktop is one process for every
    /// launch, so the process that made the swap is not enough: only the launch whose id matches
    /// may give up its own start (for example when its spawn fails).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<String>,
    /// Written before a tool is spawned for this session. While this is set and no recorded
    /// process is provably dead, automatic recovery cannot tell whether a tool is running — the
    /// writer may have died between the spawn and recording it — so the session needs a hand,
    /// never an automatic restore. Cleared when the spawned process is recorded.
    #[serde(default)]
    pub spawning: bool,
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
    /// Where `real_path` resolved to when the swap read it — the whole link chain and every
    /// ancestor directory resolved (the canonical path); `None` when it did not resolve (a
    /// legacy journal, or a dangling link). A restore never writes through a path that no
    /// longer resolves there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_resolved_path: Option<PathBuf>,
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
    /// The launch that swapped the files in. Its own spawn failure may clear its start with this;
    /// any other restore must wait for the launch to finish.
    pub launch_id: String,
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

/// Compare two already-resolved paths exactly. `canonicalize` returns the real casing on Windows,
/// so a case-only difference between two canonical paths is a real change of target, not an
/// equivalent spelling. This is for the restore's resolution guard; user-data folder matching
/// elsewhere still uses the case-insensitive comparison.
pub fn paths_eq_exact(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

/// The Windows user folder a `UserDataLocation` names, inside a Proton prefix. Proton's default
/// user is `steamuser`; a prefix that names another single user folder uses that one instead.
pub fn user_data_root_in_prefix(prefix: &Path, location: &UserDataLocation) -> PathBuf {
    let users = prefix.join("drive_c").join("users");
    let user = if users.join("steamuser").is_dir() {
        users.join("steamuser")
    } else {
        std::fs::read_dir(&users)
            .ok()
            .and_then(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| path.is_dir() && !path.ends_with("Public"))
            })
            .unwrap_or_else(|| users.join("steamuser"))
    };
    match location {
        UserDataLocation::Documents => user.join("Documents"),
        UserDataLocation::RoamingAppData => user.join("AppData").join("Roaming"),
        UserDataLocation::LocalAppData => user.join("AppData").join("Local"),
        UserDataLocation::Home => user,
    }
}

/// Resolve a `GamePath` targeting `UserData` to its absolute host path. A Proton prefix, when
/// given, replaces the Linux home folders with the prefix's Windows ones.
pub fn resolve_user_file_source_in(
    prefix: Option<&Path>,
    source: &GamePath,
) -> Result<PathBuf, UserFilesError> {
    match source {
        GamePath::UserData { location, path } => {
            let base = match prefix {
                Some(prefix) => Some(user_data_root_in_prefix(prefix, location)),
                None => user_data_root(location),
            };
            let Some(base) = base else {
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

/// Resolve a `GamePath` targeting `UserData` to its absolute host path.
pub fn resolve_user_file_source(source: &GamePath) -> Result<PathBuf, UserFilesError> {
    resolve_user_file_source_in(None, source)
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Where `path` resolves to right now — the whole link chain and every ancestor directory
/// resolved (the canonical path), so a retargeted link anywhere in the chain, or an ancestor
/// directory replaced by a link, changes the answer. Recorded at swap time so a restore never
/// writes through a path that no longer resolves where the swap read it.
fn resolved_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Where `path` resolves, or will resolve once it exists: the path itself when it resolves now,
/// otherwise the deepest ancestor that exists and resolves, plus the remaining components (the
/// file — or a folder on its way — does not exist yet at swap time; a redirected ancestor changes
/// this answer either way).
fn resolved_path_for(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    let mut suffix: Vec<std::ffi::OsString> = vec![path.file_name()?.to_os_string()];
    let mut dir = path.parent()?;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(dir) {
            let mut out = resolved;
            for component in suffix.iter().rev() {
                out.push(component);
            }
            return Some(out);
        }
        suffix.push(dir.file_name()?.to_os_string());
        dir = dir.parent()?;
    }
}

/// Whether two paths are provably different files: neither is a link, and their file identities
/// (device + inode on Unix, volume serial + file index on Windows) differ. Anything else — the
/// same file under two names (a hard link), either path a link, or an identity that cannot be
/// read — is NOT provably distinct.
fn provably_distinct_files(a: &Path, b: &Path) -> bool {
    if symlink_target(a).is_some() || symlink_target(b).is_some() {
        return false;
    }
    crate::game_base::get_file_identity(a)
        .zip(crate::game_base::get_file_identity(b))
        .is_some_and(|(left, right)| left != right)
}

/// Where `path` points when it is a symbolic link, `None` when it is not one (or its target
/// cannot be read).
fn symlink_target(path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    std::fs::read_link(path).ok()
}

/// Create a conflict archive without ever touching an existing file. `create_new` refuses both a
/// regular file and a symlink already at the name, so a file someone put there — or a link pointing
/// somewhere else — is never replaced or written through. If the name is taken, the next free
/// numbered name is used and returned.
fn write_conflict_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, UserFilesError> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_string();
    for attempt in 0..1000u32 {
        let candidate = if attempt == 0 {
            path.to_path_buf()
        } else {
            parent.join(format!("{name}-{attempt}"))
        };
        // `symlink_metadata` does not follow the link, so a symlink at the name counts as taken
        // too. Windows reports a directory at the name as a permission error rather than
        // AlreadyExists; checking first keeps the "pick a new name" rule portable.
        if std::fs::symlink_metadata(&candidate).is_ok() {
            continue;
        }
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(UserFilesError::Write {
                    path: candidate,
                    source: error,
                })
            }
        };
        file.write_all(bytes)
            .map_err(|source| UserFilesError::Write {
                path: candidate.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| UserFilesError::Write {
            path: candidate.clone(),
            source,
        })?;
        return Ok(candidate);
    }
    Err(UserFilesError::Other(format!(
        "could not find a free conflict file name next to {}",
        path.display()
    )))
}

/// Write a user file atomically: a temp file beside it, synced, renamed over the target, then the
/// directory synced where the platform supports it. A stop leaves the old bytes or the complete new
/// bytes, never a torn file. The target is named when the write is refused.
///
/// A file the player marked read-only is theirs: it is refused, and left exactly as it was. On
/// Windows a rename over it would fail anyway, but on Unix a rename only needs the folder to be
/// writable, so without this check the file would be silently replaced and lose its mode. A file
/// that is replaced keeps its permissions, so another attribute Agora has no business changing is
/// not lost on either platform.
fn write_user_file(path: &Path, bytes: &[u8]) -> Result<(), UserFilesError> {
    // A symlinked user file is the player's setup: write through to the file it points at so the
    // link stays a link. The atomic rename below would otherwise replace the link itself with a
    // regular file. A dangling or cyclic link is refused with the reason and left as it is.
    if std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        let target = std::fs::canonicalize(path).map_err(|source| UserFilesError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                source.kind(),
                format!("the file is a symbolic link that cannot be resolved: {source}"),
            ),
        })?;
        return write_user_file(&target, bytes);
    }
    let Some(parent) = path.parent() else {
        return Err(UserFilesError::Other(format!(
            "cannot write '{}': it has no folder",
            path.display()
        )));
    };
    std::fs::create_dir_all(parent)?;
    let existing_permissions = match std::fs::metadata(path) {
        Ok(metadata) => {
            let permissions = metadata.permissions();
            if permissions.readonly() {
                return Err(UserFilesError::Write {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "the file is read-only",
                    ),
                });
            }
            Some(permissions)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(UserFilesError::Write {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = parent.join(format!(
        ".{name}.agora-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(source) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(UserFilesError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    if let Some(permissions) = existing_permissions {
        if let Err(source) = std::fs::set_permissions(&tmp, permissions) {
            let _ = std::fs::remove_file(&tmp);
            return Err(UserFilesError::Write {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    if let Err(source) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(UserFilesError::Write {
            path: path.to_path_buf(),
            source,
        });
    }
    sync_directory(parent);
    Ok(())
}

/// Flush a directory entry where the platform supports it, so a rename is durable.
#[cfg(unix)]
fn sync_directory(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_dir: &Path) {}

fn write_journal(journal_path: &Path, journal: &Journal) -> Result<(), UserFilesError> {
    // Test seam only: `write_fault_fires` is false unless a test on this thread installed a
    // fault hook, so production writes never fail here.
    if crate::game_tool_swap::write_fault_fires(journal_path) {
        return Err(UserFilesError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "journal write fault injected by the test hook",
        )));
    }
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

/// Owns the "starting" mark a launch writes into the journal. The mark keeps a swap that has not
/// yet spawned from being restored under it, but it must not outlive the call that wrote it: a
/// failed swap keeps its journal for recovery, and a live mark would make that recovery and every
/// later launch refuse. `swap_in` disarms the guard when it hands the launch back to its caller,
/// which then owns clearing the mark through `record_process` or `finish_starting_launch`.
struct StartingMarkGuard<'a> {
    journal_path: &'a Path,
    finished: bool,
}

impl Drop for StartingMarkGuard<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let Ok(text) = std::fs::read_to_string(self.journal_path) else {
            return;
        };
        let Ok(mut journal) = serde_json::from_str::<Journal>(&text) else {
            return;
        };
        if journal.starting_launcher.is_some() {
            journal.starting_launcher = None;
            journal.spawning = false;
            let _ = write_journal(self.journal_path, &journal);
        }
    }
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
        // The launcher may be gone while a game it started lives on: the job it was launched in
        // still holds everything that descends from it.
        if proc
            .job
            .as_deref()
            .is_some_and(|job| crate::process_job::active(job) > 0)
        {
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

/// Whether a journal records no process identity at all: no starting launcher and no recorded
/// process. That is a legacy journal or a self-capture that failed, and it means liveness is
/// unknown — a launch between its swap and its spawn looks exactly like this — so automatic
/// recovery must not act on the journal; it is reported as needing a hand.
pub fn liveness_unknown(journal: &Journal) -> bool {
    journal.starting_launcher.is_none() && journal.processes.is_empty()
}

/// [`liveness_unknown`] for session details read without the full journal.
pub fn details_liveness_unknown(details: &SessionDetails) -> bool {
    details.starting_launcher.is_none() && details.processes.is_empty()
}

/// Whether a journal is in its "spawning" window with no provably dead recorded process: the
/// writer recorded that it was about to spawn a tool, and no recorded process can be proven dead,
/// so a tool may be running that no record can see. Automatic recovery treats that as needing a
/// hand, never as a dead session.
pub fn spawning_without_dead_process(journal: &Journal) -> bool {
    journal.spawning
        && !journal
            .processes
            .iter()
            .any(|process| process_identity::verify(process).is_err())
}

/// [`spawning_without_dead_process`] for session details read without the full journal.
pub fn details_spawning_without_dead_process(details: &SessionDetails) -> bool {
    details.spawning
        && !details
            .processes
            .iter()
            .any(|process| process_identity::verify(process).is_err())
}

/// The needs-a-hand line for a session in its spawning window.
pub fn spawning_needs_hand_message(journal_path: &Path, journal: &Journal) -> String {
    format!(
        "the user-files journal {} of instance {} ({}/{}) is marked as spawning a tool and \
         records no provably dead process, so Agora cannot tell whether a tool is still running; \
         it was left alone and nothing was changed. If no tool of {} is running, put the files \
         back by hand with `{}`",
        journal_path.display(),
        journal.instance_id,
        journal.game,
        journal.store,
        journal.game,
        restore_instruction(journal.game.as_str(), journal.store.as_str())
    )
}

/// The needs-a-hand line for a journal whose liveness is unknown: what was left alone, and the
/// command that can decide once nothing of that game is running.
pub fn needs_hand_message(journal_path: &Path, journal: &Journal) -> String {
    format!(
        "the user-files journal {} of instance {} ({}/{}) records no process identity, so Agora \
         cannot tell whether its session is still live; it was left alone and nothing was \
         changed. If no game or tool of {} is running, put the files back by hand with `{}`",
        journal_path.display(),
        journal.instance_id,
        journal.game,
        journal.store,
        journal.game,
        restore_instruction(journal.game.as_str(), journal.store.as_str())
    )
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
    swap_in_locked(ctx, instance_id, definition, store, running_from)
}

/// [`swap_in`] for a caller that already holds the game/store user-files lock (the lock manager
/// is not re-entrant). A tool swap holds that lock for the whole run — from before this journal
/// is written until the run's own swap journal is cleared — so a recovery in another process
/// can never act on the live run.
pub fn swap_in_locked(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    running_from: &Path,
) -> Result<SwapOutcome, UserFilesError> {
    // This launch's own identity, recorded in the journal before any real file is touched. Only
    // the launch that made the swap may give up its start; a restore from the same process but a
    // different launch (the previous session's exit watcher, the Restore command) must not.
    let launch_id = uuid::Uuid::new_v4().to_string();

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
        // Not running: recover/restore the previous session before proceeding. A journal that
        // records no process identity at all (a legacy journal, or a self-capture that failed)
        // has unknown liveness: another process's launch between its swap and its spawn looks
        // exactly like this, so this launch must not restore under it. It refuses and names the
        // command that can decide once nothing is running.
        if liveness_unknown(&existing) {
            return Err(UserFilesError::Other(needs_hand_message(
                &journal_path,
                &existing,
            )));
        }
        // In its spawning window with no provably dead process, a tool may be running that no
        // record can see: this launch must not restore under it.
        if spawning_without_dead_process(&existing) {
            return Err(UserFilesError::Other(spawning_needs_hand_message(
                &journal_path,
                &existing,
            )));
        }
        // A new launch's catch-up is automatic recovery, so it is strict.
        let _ = restore_journal_internal(ctx, definition, &existing, true, false)?;
    }

    // 2. Filter mappings that apply to this store
    let applicable_mappings: Vec<&UserFileMapping> = definition
        .user_files
        .iter()
        .filter(|m| m.strategy == UserFileStrategy::JournaledSwap && m.applies_to_store(store))
        .collect();

    if applicable_mappings.is_empty() {
        return Ok(SwapOutcome {
            swapped_files: 0,
            launch_id,
        });
    }

    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| UserFilesError::Other(e.to_string()))?;
    let backup_dir = ctx
        .paths
        .user_files_backup_dir(definition.id.as_str(), store.as_str());
    std::fs::create_dir_all(&backup_dir)?;
    // A Windows game run through Proton keeps its user folders in the prefix, not in the
    // Linux home folder. `None` on every other platform, store and native Linux game.
    let prefix = crate::game_instance::proton_prefix_checked(ctx, instance_id)
        .map_err(|e| UserFilesError::Other(e.to_string()))?;

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
        let real_path = resolve_user_file_source_in(prefix.as_deref(), &m.source)?;
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
            real_resolved_path: None,
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
        launch_id: Some(launch_id.clone()),
        spawning: false,
        files: journal_files,
    };

    write_journal(&journal_path, &journal)?;
    let mut starting_mark = StartingMarkGuard {
        journal_path: &journal_path,
        finished: false,
    };

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
                journal.files[i].real_resolved_path = resolved_path_for(&real_path);
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
                        real_path: real_path.clone(),
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

                // The file is NOT marked swapped yet. There is no backup that could prove what
                // happened, so the mark is written only once Agora has actually created the
                // real file: a stop in between leaves a journal that says the real file was
                // never touched, and recovery then never treats a file the player made in the
                // meantime as Agora's own write.
                journal.files[i].existed = false;
                journal.files[i].backup_path = None;
                journal.files[i].original_sha256 = None;
                journal.files[i].written_sha256 = written_hash;
                journal.files[i].swapped = false;
                journal.files[i].state = Some(FileState::BackingUp);
                journal.files[i].restored = false;
                journal.files[i].real_resolved_path = resolved_path_for(&real_path);
                write_journal(&journal_path, &journal)?;

                if let Some(instance_bytes) = instance_bytes {
                    if let Some(parent) = real_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    write_user_file(&real_path, &instance_bytes)?;
                }

                journal.files[i].swapped = true;
                journal.files[i].state = Some(FileState::Swapped);
                write_journal(&journal_path, &journal)?;
            }
        }
        Ok(())
    })();

    if let Err(e) = swap_result {
        // Any error puts back what was swapped so far. A restore that also fails is reported, and
        // its journal is left for the next launch or a restore to finish.
        return Err(match restore_journal_internal(ctx, definition, &journal, true, false) {
            Ok(_) => e,
            Err(restore_error) => UserFilesError::Other(format!(
                "{e}; putting the player's files back also failed: {restore_error}. The journal was kept so recovery can finish it."
            )),
        });
    }

    starting_mark.finished = true;
    Ok(SwapOutcome {
        swapped_files: journal.files.len(),
        launch_id,
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
    restore_locked(ctx, definition, store)
}

/// [`restore`] with the explicit override: on a both-changed conflict it still writes the
/// conflict copy first, then restores. This is the human override for that one protection;
/// automatic startup recovery never forces.
pub fn restore_forced(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(definition.id.clone(), store.clone()),
        "restore-forced",
    )?;
    restore_locked_forced(ctx, definition, store)
}

/// The body of [`restore`] under the caller's game-user-files lock: read the journal, refuse a
/// live session, put the files back. Callers that already hold the lock (startup recovery) use
/// this instead of taking the lock again — the lock manager is not re-entrant.
pub fn restore_locked(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    restore_locked_mode(ctx, definition, store, false, false)
}

/// [`restore_forced`] for a caller that already holds the game/store user-files lock.
pub fn restore_locked_forced(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    restore_locked_mode(ctx, definition, store, false, true)
}

/// [`restore`] as automatic crash recovery, under the caller's game-user-files lock: the same
/// put-back, but strict — it never acts on a file whose content changed since the swap (see
/// [`restore_journal_internal`]). The explicit [`restore`] command is the human override that
/// may do the full put-back after checking nothing of that game is running.
pub fn restore_locked_recovery(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    restore_locked_mode(ctx, definition, store, true, false)
}

/// [`restore_locked_recovery`] for an automatic caller that does not hold the lock.
pub fn restore_for_recovery(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<RestoreReport, UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(definition.id.clone(), store.clone()),
        "restore-recovery",
    )?;
    restore_locked_recovery(ctx, definition, store)
}

fn restore_locked_mode(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
    strict: bool,
    force: bool,
) -> Result<RestoreReport, UserFilesError> {
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
    let journal: Journal = serde_json::from_str(&text)?;

    if session_running_apart_from_starting(&journal) {
        return Err(UserFilesError::CannotRestoreRunning {
            instance: journal.instance_id.clone(),
        });
    }
    // A launch that has swapped the files in and not yet recorded the game is still a launch,
    // whoever asks to restore. The desktop is one process for every launch, so a matching pid
    // proves nothing: the previous session's exit watcher or the Restore command would put the
    // player's files back under the next launch. Only that launch may give up its own start, with
    // `finish_starting_launch`, when its spawn fails.
    if journal
        .starting_launcher
        .as_ref()
        .is_some_and(|launcher| process_identity::verify(launcher).is_ok())
    {
        return Err(UserFilesError::CannotRestoreRunning {
            instance: journal.instance_id.clone(),
        });
    }

    restore_journal_internal(ctx, definition, &journal, strict, force)
}

/// What a catch-up did with the per-user files of one finished session.
#[derive(Debug, Clone, PartialEq)]
pub struct FinishedRestore {
    pub game: GameId,
    pub store: StoreId,
    pub instance_id: String,
    pub restored: usize,
    pub changed: usize,
    /// Why the journal is still swapped in. `None` means it was restored and removed.
    pub error: Option<String>,
}

/// Put back the user files of this instance's finished swap session, if there is one and its game
/// is not running. A running game is left alone, and a journal that cannot be read or restored is
/// reported rather than called success. Game-free: the journal itself names the game and store.
pub fn restore_finished_for_instance(ctx: &Ctx, instance_id: &str) -> Option<FinishedRestore> {
    let root = ctx.paths.user_files_root();
    let entries = std::fs::read_dir(&root).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().ok().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        let journal_path = entry.path().join("journal.json");
        let Ok(text) = std::fs::read_to_string(&journal_path) else {
            continue;
        };
        let Ok(journal) = serde_json::from_str::<Journal>(&text) else {
            continue;
        };
        if journal.instance_id != instance_id {
            continue;
        }
        let report = |error: Option<String>, restored: usize, changed: usize| FinishedRestore {
            game: journal.game.clone(),
            store: journal.store.clone(),
            instance_id: journal.instance_id.clone(),
            restored,
            changed,
            error,
        };
        if is_session_running(&journal) {
            return None;
        }
        // Unknown liveness (no recorded identity): automatic recovery must not act on it; the
        // report says it needs a hand instead.
        if liveness_unknown(&journal) {
            return Some(report(
                Some(needs_hand_message(&journal_path, &journal)),
                0,
                0,
            ));
        }
        if spawning_without_dead_process(&journal) {
            return Some(report(
                Some(spawning_needs_hand_message(&journal_path, &journal)),
                0,
                0,
            ));
        }
        let Some(definition) = ctx.games.game(&journal.game) else {
            return Some(report(
                Some(format!(
                    "the game '{}' is not registered, so its user files could not be restored",
                    journal.game
                )),
                0,
                0,
            ));
        };
        return Some(
            match restore_for_recovery(ctx, definition, &journal.store) {
                Ok(found) => report(
                    None,
                    found.files.len(),
                    found.files.iter().filter(|file| file.changed).count(),
                ),
                Err(error) => report(Some(error.to_string()), 0, 0),
            },
        );
    }
    None
}

/// Startup recovery over every user-files journal on disk: a session whose game is no longer
/// running has its files put back. A session that is still running (a live game or launch, in
/// this or another process) is left alone — that is a normal session, not interrupted work — and
/// a store whose lock another Agora process holds is skipped and reported rather than recovered
/// underneath the live operation. Returns `(actions, warnings)`; nothing is raised.
pub fn recover_finished_sessions(ctx: &Ctx) -> (Vec<String>, Vec<String>) {
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let root = ctx.paths.user_files_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return (actions, warnings);
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let journal_path = dir.join("journal.json");
        if !journal_path.exists() {
            continue;
        }
        // Fail closed on an unreadable journal: never read "cannot tell" as "nothing pending".
        let text = match std::fs::read_to_string(&journal_path) {
            Ok(text) => text,
            Err(e) => {
                warnings.push(format!(
                    "the user-files journal {} cannot be read ({e}), so it was left alone",
                    journal_path.display()
                ));
                continue;
            }
        };
        let journal: Journal = match serde_json::from_str(&text) {
            Ok(journal) => journal,
            Err(e) => {
                warnings.push(format!(
                    "the user-files journal {} is malformed ({e}), so it was left alone",
                    journal_path.display()
                ));
                continue;
            }
        };
        if is_session_running(&journal) {
            continue;
        }
        let Some(definition) = ctx.games.game(&journal.game) else {
            warnings.push(format!(
                "the user-files session of instance {} was left alone: its game '{}' is not registered",
                journal.instance_id, journal.game
            ));
            continue;
        };
        // Never wait on the lock at startup: another Agora may be mid-swap on this game and store.
        let lock = ctx.lock_manager.acquire_with_timeout(
            LockResource::GameUserFiles(journal.game.clone(), journal.store.clone()),
            "startup-recovery",
            std::time::Duration::ZERO,
            None,
        );
        let _lock = match lock {
            Ok(lock) => lock,
            Err(error) => {
                warnings.push(format!(
                    "the user-files session of instance {} ({}/{}) was left alone: another Agora \
                     holds its lock right now ({error}). Nothing was changed.",
                    journal.instance_id, journal.game, journal.store
                ));
                continue;
            }
        };
        // Under the lock, re-read the journal: another process may have finished or changed it
        // between the directory scan and the lock. A journal that records no process identity
        // has unknown liveness, so it is reported as needing a hand, never recovered.
        let text = match std::fs::read_to_string(&journal_path) {
            Ok(text) => text,
            Err(e) => {
                warnings.push(format!(
                    "the user-files journal {} cannot be read ({e}), so it was left alone",
                    journal_path.display()
                ));
                continue;
            }
        };
        let journal: Journal = match serde_json::from_str(&text) {
            Ok(journal) => journal,
            Err(e) => {
                warnings.push(format!(
                    "the user-files journal {} is malformed ({e}), so it was left alone",
                    journal_path.display()
                ));
                continue;
            }
        };
        if liveness_unknown(&journal) {
            warnings.push(needs_hand_message(&journal_path, &journal));
            continue;
        }
        if spawning_without_dead_process(&journal) {
            warnings.push(spawning_needs_hand_message(&journal_path, &journal));
            continue;
        }
        match restore_locked_recovery(ctx, definition, &journal.store) {
            Ok(report) => actions.push(format!(
                "Put back the user files of {} ({}) after the interrupted session of instance {} \
                 ({} file(s) restored).",
                report.game,
                report.store,
                report.instance_id,
                report.files.len()
            )),
            // Another process finished the session between the directory scan and the lock.
            Err(UserFilesError::NoSession { .. }) => {}
            Err(UserFilesError::CannotRestoreRunning { instance }) => warnings.push(format!(
                "the user-files session of instance {instance} ({}/{}) was left alone: the game is still running",
                journal.game, journal.store
            )),
            Err(e) => warnings.push(format!(
                "the user-files session of instance {} ({}/{}) could not be put back: {e}",
                journal.instance_id, journal.game, journal.store
            )),
        }
    }
    (actions, warnings)
}

/// Restore user files, retrying while the game is still leaving the process table. A game that
/// stays alive for the whole `timeout` gets the normal `CannotRestoreRunning` refusal, so a
/// journal is never silently left swapped in. A Windows sharing violation is retried too: the
/// crashing game or a crash handler can still hold a file after its process is gone.
pub fn restore_retrying_while_running(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
    timeout: std::time::Duration,
) -> Result<RestoreReport, UserFilesError> {
    retry_while_running(timeout, || restore(ctx, definition, store))
}

/// The exact command that finishes a restore a launch or catch-up could not run.
pub fn restore_instruction(game: &str, store: &str) -> String {
    format!("agora games user-files restore {game} {store}")
}

/// The explicit restore command with its override flag, for a conflict that refuses without it
/// (both sides changed since the swap). Automatic startup recovery never forces.
pub fn restore_instruction_forced(game: &str, store: &str) -> String {
    format!("agora games user-files restore {game} {store} --force")
}

/// What a person is told when a restore did not finish. It names the error, says the files are
/// still the instance's, and gives the exact command. Never success.
pub fn restore_failure_message(reason: &str, game: &str, store: &str) -> String {
    format!(
        "the per-user files could not be restored ({reason}); they are still swapped in. Run `{}` once the game has closed.",
        restore_instruction(game, store)
    )
}

/// How many times a sharing violation is retried before the file is called locked for good. The
/// deadline still bounds the whole wait; this keeps a permanently held file from using all of it.
const SHARING_RETRIES: u32 = 50;

fn retry_while_running<T>(
    timeout: std::time::Duration,
    mut attempt: impl FnMut() -> Result<T, UserFilesError>,
) -> Result<T, UserFilesError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut sharing_retries = 0;
    loop {
        match attempt() {
            Err(error) if std::time::Instant::now() < deadline && retryable_restore(&error) => {
                if sharing_violation(&error) {
                    sharing_retries += 1;
                    if sharing_retries > SHARING_RETRIES {
                        return Err(error);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            other => return other,
        }
    }
}

/// A session still being torn down can answer "running"; a file still held can answer with a
/// Windows sharing violation. Both are worth another look before the restore is called failed.
fn retryable_restore(error: &UserFilesError) -> bool {
    matches!(error, UserFilesError::CannotRestoreRunning { .. }) || sharing_violation(error)
}

fn sharing_violation(error: &UserFilesError) -> bool {
    let source = match error {
        UserFilesError::Io(source) | UserFilesError::Write { source, .. } => source,
        _ => return false,
    };
    is_sharing_violation(source)
}

#[cfg(windows)]
fn is_sharing_violation(error: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
    matches!(error.raw_os_error(), Some(32) | Some(33))
}

#[cfg(not(windows))]
fn is_sharing_violation(_error: &std::io::Error) -> bool {
    false
}

/// Clear the "starting" mark of a launch that will never spawn. The caller then restores its
/// files; a launch that failed must not leave the session counting as running.
///
/// This is for a caller that is not the launch itself (tests and recovery). A launch clears only
/// its own start with [`finish_starting_launch`].
pub fn finish_starting(ctx: &Ctx, game: &GameId, store: &StoreId) -> Result<(), UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(game.clone(), store.clone()),
        "finish-starting",
    )?;
    finish_starting_locked(ctx, game, store)
}

/// [`finish_starting`] for a caller that already holds the game/store user-files lock.
pub fn finish_starting_locked(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
) -> Result<(), UserFilesError> {
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    if journal.starting_launcher.is_some() {
        journal.starting_launcher = None;
        journal.spawning = false;
        write_journal(&journal_path, &journal)?;
    }
    Ok(())
}

/// Clear the "starting" mark of `launch_id`, and only that launch. A launch whose spawn failed may
/// give up its own start; another launch's restore must not touch it. A journal that belongs to a
/// different launch, or has no matching id, is left exactly as it is.
pub fn finish_starting_launch(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
    launch_id: &str,
) -> Result<(), UserFilesError> {
    let _lock = ctx.lock_manager.acquire(
        LockResource::GameUserFiles(game.clone(), store.clone()),
        "finish-starting-launch",
    )?;
    finish_starting_launch_locked(ctx, game, store, launch_id)
}

/// [`finish_starting_launch`] for a caller that already holds the game/store user-files lock.
pub fn finish_starting_launch_locked(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
    launch_id: &str,
) -> Result<(), UserFilesError> {
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    if journal.starting_launcher.is_none() {
        return Ok(());
    }
    if journal.launch_id.as_deref() != Some(launch_id) {
        return Err(UserFilesError::Other(
            "the per-user files were swapped in by a different launch, so this launch left them \
             in place"
                .to_string(),
        ));
    }
    journal.starting_launcher = None;
    journal.spawning = false;
    write_journal(&journal_path, &journal)?;
    Ok(())
}

fn restore_journal_internal(
    ctx: &Ctx,
    definition: &GameDefinition,
    journal: &Journal,
    strict: bool,
    force: bool,
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
    // A session that journaled no files has nothing whose real path needs resolving, so ending it
    // must not fail just because the Proton prefix (and its base record) is unavailable.
    let prefix = if journal.files.is_empty() {
        None
    } else {
        crate::game_instance::proton_prefix_checked(ctx, &journal.instance_id)
            .map_err(|e| UserFilesError::Other(e.to_string()))?
    };

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

        let derived_real_path = resolve_user_file_source_in(prefix.as_deref(), &mapping.source)?;
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

        let real_bytes = std::fs::read(&file.real_path).ok();
        let real_hash = real_bytes.as_ref().map(|bytes| hash_bytes(bytes));
        let backup_hash = if has_backup {
            std::fs::read(&derived_backup_path)
                .ok()
                .map(|bytes| hash_bytes(&bytes))
        } else {
            None
        };
        // The restore moves no bytes for this entry when the real file already holds the
        // original or the backup's bytes: the entry is then only marked restored.
        let real_is_original = file.original_sha256.as_deref().is_some()
            && real_hash.as_deref() == file.original_sha256.as_deref();
        let real_matches_backup = real_hash.is_some() && real_hash == backup_hash;

        // The real path is compared fully resolved — the whole link chain and every ancestor
        // directory — against where it resolved when the swap read it. Writing the backup
        // through a path whose resolution changed since the swap (the final link retargeted, a
        // link deeper in a chain retargeted, an ancestor directory replaced by a link) would
        // overwrite whatever it resolves to now, so any difference stops the restore here,
        // before anything is modified, and asks for a hand.
        let resolved_now = resolved_path(&file.real_path);
        let would_be_now = resolved_path_for(&file.real_path);
        let resolution_ok = match &file.real_resolved_path {
            Some(recorded) => {
                // A final component that is a link but no longer resolves is a changed (or
                // broken) link, not a deleted file.
                if symlink_target(&file.real_path).is_some() && resolved_now.is_none() {
                    false
                } else {
                    match &resolved_now {
                        Some(now) => paths_eq_exact(recorded, now),
                        // The path no longer fully resolves (the file is gone, or an ancestor
                        // link now dangles): compare the anchored resolution instead, so a
                        // redirected ancestor still refuses.
                        None => would_be_now
                            .as_ref()
                            .is_some_and(|now| paths_eq_exact(recorded, now)),
                    }
                }
            }
            None => {
                // A legacy journal recorded no resolved path, so nothing proves the real path
                // still resolves where the swap read it — an ancestor could have been replaced
                // by a link while the final component stays an ordinary file. Fail closed on
                // any entry whose restore would write; only entries that move no bytes (already
                // restored, the real file already holding the original or the backup's bytes,
                // or a not-swapped entry with nothing to remove) may proceed.
                if symlink_target(&file.real_path).is_some() {
                    false
                } else {
                    file.restored
                        || real_is_original
                        || real_matches_backup
                        || (file_state(file) != FileState::Swapped
                            && !has_backup
                            && !(real_hash.as_deref() == Some(file.written_sha256.as_str())
                                && !file.written_sha256.is_empty()))
                }
            }
        };
        if !resolution_ok {
            let recorded = match (&file.real_resolved_path, &resolved_now) {
                (Some(recorded), Some(now)) => format!(
                    "resolved to '{}' at the swap and to '{}' now",
                    recorded.display(),
                    now.display()
                ),
                (Some(recorded), None) => format!(
                    "resolved to '{}' at the swap and no longer resolves now",
                    recorded.display()
                ),
                (None, _) => "the journal records no resolved path for it (a journal from before \
                              resolved paths were recorded)"
                    .to_string(),
            };
            // A legacy journal that cannot prove the resolution refuses any write; name the
            // backup too, so the report says where every copy is.
            let backup_note = if file.real_resolved_path.is_none() && has_backup {
                format!(
                    "; its backup '{}' was left in place",
                    derived_backup_path.display()
                )
            } else {
                String::new()
            };
            return Err(UserFilesError::Other(format!(
                "the user file '{}' cannot be verified to still resolve where the journal records \
                 ({}{}); Agora did not write through it and changed nothing. Check the links by \
                 hand, then put the files back with `{}`",
                file.real_path.display(),
                recorded,
                backup_note,
                restore_instruction(journal.game.as_str(), journal.store.as_str())
            )));
        }

        // Three-way check, before anything is modified: when the journal records the swap as
        // done, both the real file and the instance's copy are compared against what the
        // session wrote. When BOTH changed since the swap, recovery cannot tell whose bytes are
        // the ones to keep, so it overwrites neither: the real file's current bytes are kept as
        // a clearly named conflict copy next to the instance's copy, and the restore stops and
        // asks for a hand. Only when at most one side changed may bytes move — and equal bytes
        // on both sides count as unambiguous only when the two paths are provably DIFFERENT
        // files: if the instance copy is a link to the real file (or a hard link to it), both
        // sides read the same changed bytes, and putting the backup over the shared target
        // would lose the edit, so that is a needs-a-hand case too.
        if !file.restored
            && file_state(file) == FileState::Swapped
            && !real_is_original
            && !real_matches_backup
        {
            let real_changed = real_hash
                .as_deref()
                .is_some_and(|hash| hash != file.written_sha256.as_str());
            let instance_hash = std::fs::read(&derived_instance_copy)
                .ok()
                .map(|bytes| hash_bytes(&bytes));
            let instance_changed = instance_hash
                .as_deref()
                .is_some_and(|hash| hash != file.written_sha256.as_str());
            if real_changed
                && instance_changed
                && (real_hash != instance_hash
                    || !provably_distinct_files(&file.real_path, &derived_instance_copy))
            {
                let real_bytes = real_bytes.expect("read changed bytes above");
                let real_hash = real_hash.expect("hashed above");
                // The forced restore is about to overwrite the instance's independently edited
                // copy, so that is the version to archive. Without the override nothing is
                // overwritten and the real file's current bytes are the ones worth naming.
                let (archived_bytes, archive_hash) = if force {
                    let instance_bytes = std::fs::read(&derived_instance_copy)?;
                    let instance_hash = hash_bytes(&instance_bytes);
                    (instance_bytes, instance_hash)
                } else {
                    (real_bytes.to_vec(), real_hash.clone())
                };
                let conflict_path = derived_instance_copy.with_file_name(format!(
                    "{}.agora-conflict-{}",
                    derived_instance_copy
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("file"),
                    &archive_hash[..8]
                ));
                let conflict_path = write_conflict_file(&conflict_path, &archived_bytes)?;
                // The explicit restore command's override (`--force`) writes the conflict
                // copy first (done above) and then restores; automatic recovery never does.
                if !force {
                    return Err(UserFilesError::Other(format!(
                        "the user file '{}' and the instance's copy '{}' both changed since \
                         the swap, so neither was overwritten: the real file's current bytes \
                         were kept as '{}'. Check both by hand, then put the files back \
                         with `{}`",
                        file.real_path.display(),
                        derived_instance_copy.display(),
                        conflict_path.display(),
                        restore_instruction_forced(journal.game.as_str(), journal.store.as_str())
                    )));
                }
            }
        }

        // Strict mode is crash recovery, not a human at the restore command: a file Agora
        // created during the session (no original existed) whose content no longer matches
        // what the session wrote may have been made by the player after the crash. Recovery
        // never overwrites the instance's copy with such bytes and never deletes the file, so
        // the whole restore stops here, before anything is modified, and asks for a hand.
        if strict && !existed && file_state(file) == FileState::Swapped {
            if let Ok(bytes) = std::fs::read(&file.real_path) {
                if hash_bytes(&bytes) != file.written_sha256 {
                    return Err(UserFilesError::Other(format!(
                        "the user file '{}' changed since the swap (it does not match what the \
                         session wrote), so recovery left it and the instance's copy '{}' exactly \
                         as they are and changed nothing. Check the file by hand, then run `{}`",
                        file.real_path.display(),
                        derived_instance_copy.display(),
                        restore_instruction(journal.game.as_str(), journal.store.as_str())
                    )));
                }
            }
        }

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

        // By design: a session that ends on exactly the player's original bytes has no changed
        // bytes to save, so the instance copy is left as it was.
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
            // A stop between Agora's write and the "swapped" mark leaves the file Agora wrote
            // under a journal that says no swap happened. Only bytes exactly matching the
            // recorded write are removed; anything else was not written by Agora and is left
            // exactly as it is.
            if entry.derived_real_path.exists()
                && !entry.file.written_sha256.is_empty()
                && real_hash.as_deref() == Some(entry.file.written_sha256.as_str())
            {
                let _ = std::fs::remove_file(&entry.derived_real_path);
            }
            let _ = std::fs::remove_file(entry.derived_backup_path.with_extension("tmp"));
            journal.files[entry.index].restored = true;
            write_journal(&journal_path, &journal)?;
            continue;
        }

        // Agora's own writes are atomic (a temp file renamed over the target), so a real file that
        // differs from what Agora wrote is a real change by the game, including a shorter file the
        // game emptied or trimmed. It is kept as the instance's copy; a torn write cannot be told
        // from a game edit by looking at a prefix, and guessing wrongly throws the game's change away.

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
                    real_path: entry.derived_real_path.clone(),
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
            // file goes. (Crash recovery never reaches a file whose content changed since the
            // swap: the strict check in the validation above stops the whole restore first.)
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
    record_process_locked(ctx, game, store, identity)
}

/// [`record_process`] for a caller that already holds the game/store user-files lock.
pub fn record_process_locked(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
    identity: ProcessIdentity,
) -> Result<(), UserFilesError> {
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        // A launch that swapped files in must have a journal. A missing one means the launch this
        // record belongs to never swapped anything: that is not a success, it is a session nobody
        // can account for.
        return Err(UserFilesError::NoSession {
            game: game.clone(),
            store: store.clone(),
        });
    }

    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    journal.processes.push(identity);
    // The game now has an identity that says whether it is alive; the launcher mark is done, and
    // the spawn is recorded, so the spawning window is over.
    journal.starting_launcher = None;
    journal.spawning = false;
    write_journal(&journal_path, &journal)?;
    Ok(())
}

/// Mark the session as spawning a tool, written (durably) before the spawn. For a caller that
/// already holds the game/store user-files lock. While the mark is set and no recorded process
/// is provably dead, automatic recovery treats the session as needing a hand (see
/// [`spawning_without_dead_process`]); [`record_process_locked`] clears it.
pub fn mark_spawning_locked(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
) -> Result<(), UserFilesError> {
    let journal_path = ctx
        .paths
        .user_files_journal_path(game.as_str(), store.as_str());
    if !journal_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&journal_path)?;
    let mut journal: Journal = serde_json::from_str(&text)?;
    if !journal.spawning {
        journal.spawning = true;
        write_journal(&journal_path, &journal)?;
    }
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

/// The parts of a session a stop needs: when it began, where the game runs from, and the process
/// identities the launch recorded. Reading is the same fail-closed read `status` does.
#[derive(Debug, Clone)]
pub struct SessionDetails {
    pub instance: String,
    pub running: bool,
    pub created_at: String,
    pub running_from: PathBuf,
    pub processes: Vec<ProcessIdentity>,
    pub starting_launcher: Option<ProcessIdentity>,
    pub spawning: bool,
}

/// Read a session's details. An unreadable journal is an error, never "no session".
pub fn details(
    ctx: &Ctx,
    game: &GameId,
    store: &StoreId,
) -> Result<Option<SessionDetails>, UserFilesError> {
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
    Ok(Some(SessionDetails {
        instance: journal.instance_id,
        running,
        created_at: journal.created_at,
        running_from: journal.running_from,
        processes: journal.processes,
        starting_launcher: journal.starting_launcher,
        spawning: journal.spawning,
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

#[cfg(test)]
mod tests {
    use super::{write_user_file, JournaledFile};
    use std::path::{Path, PathBuf};

    use agora_game_api::RelPath;

    /// Canonical paths are compared exactly: a case-only retarget is a different resolved file.
    /// The case-insensitive comparator stays for user-data folder matching, not for this guard.
    #[test]
    fn the_canonical_path_guard_does_not_fold_case() {
        let recorded = Path::new("/home/Player/Documents/My Games/Plugins.txt");
        let retargeted = Path::new("/home/player/documents/my games/Plugins.txt");

        assert!(
            !super::paths_eq_exact(recorded, retargeted),
            "a case-only change must not compare equal"
        );
        assert!(super::paths_eq_exact(recorded, recorded));
        assert!(
            super::paths_eq_case_insensitive(recorded, retargeted),
            "the other comparator is deliberately case-insensitive for user-data folders"
        );
    }

    #[test]
    fn the_recorded_resolved_path_round_trips_and_old_journals_have_none() {
        let file = JournaledFile {
            real_path: PathBuf::from("/home/player/.local/share/Test/Plugins.txt"),
            instance_path: RelPath::new("user/Plugins.txt").unwrap(),
            instance_copy_path: PathBuf::from("/data/inst/user/Plugins.txt"),
            existed: true,
            backup_path: None,
            original_sha256: Some("ab".into()),
            written_sha256: "cd".into(),
            swapped: true,
            state: None,
            restored: false,
            real_resolved_path: Some(PathBuf::from("/home/player/elsewhere/Plugins.txt")),
        };
        let text = serde_json::to_string(&file).unwrap();
        assert!(text.contains("real_resolved_path"), "{text}");
        let back: JournaledFile = serde_json::from_str(&text).unwrap();
        assert_eq!(
            back.real_resolved_path,
            Some(PathBuf::from("/home/player/elsewhere/Plugins.txt"))
        );
        // A journal written before the field existed — including one that recorded the old
        // link-text field — parses with no resolved path: unknown, which recovery treats as
        // "cannot prove the path still resolves where the swap read it".
        let old = r#"{
            "real_path": "/home/player/.local/share/Test/Plugins.txt",
            "instance_path": "user/Plugins.txt",
            "instance_copy_path": "/data/inst/user/Plugins.txt",
            "existed": true,
            "written_sha256": "cd",
            "swapped": true,
            "real_link_target": "/home/player/elsewhere/Plugins.txt"
        }"#;
        let parsed: JournaledFile = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.real_resolved_path, None);
    }

    fn make_read_writable(path: &std::path::Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn a_read_only_target_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plugins.txt");
        std::fs::write(&path, b"the player's list").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        let error = write_user_file(&path, b"the instance's list").unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("Plugins.txt") && text.contains("read-only"),
            "the error names the file and says why: {text}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"the player's list",
            "a refused write left different bytes"
        );
        assert!(
            std::fs::metadata(&path).unwrap().permissions().readonly(),
            "a refused write removed the read-only mode"
        );

        make_read_writable(&path);
    }

    #[test]
    fn a_replace_keeps_the_targets_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Skyrim.ini");
        std::fs::write(&path, b"the player's ini").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let before = std::fs::metadata(&path).unwrap().permissions();

        write_user_file(&path, b"the instance's ini").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"the instance's ini");
        let after = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(after.readonly(), before.readonly());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                after.mode() & 0o777,
                before.mode() & 0o777,
                "the replaced file did not keep its mode"
            );
        }
    }
}
