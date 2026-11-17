//! A bounded, Agora-owned copy of an instance's saves, taken before a crash test.
//!
//! Removing a mod partway through a playthrough can damage a save. Before a test Agora copies the
//! folder the game reads for the instance (`game_saves` resolves which one, from the game's
//! definition) into its own store under the instance, so the test can offer a restore. The save
//! folder comes from the existing resolver; this module never guesses at a game's layout.
//!
//! Copies are staged and then renamed into place, so a copy that fails leaves nothing that
//! [`list`] counts as a backup. Restore never deletes: it puts the backed-up files back, keeps the
//! current copy of a file it is about to overwrite beside it as `<name>.before-restore`, and
//! leaves files made after the backup exactly where they are.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agora_game_api::GameDefinition;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_ini;
use crate::game_instance::{self, InstanceError, SavesChoice};
use crate::game_saves::{self, SavesError};
use crate::helpers;
use crate::lock_manager::LockResource;

/// The newest backups kept per instance. A crash test makes one per suspect tried; five covers a
/// normal round of testing while bounding the store, and each backup is a full copy of the folder.
pub const MAX_BACKUPS: usize = 5;

/// A backup format change that an older reader must refuse rather than guess at.
const BACKUP_SCHEMA_VERSION: u32 = 1;
const BACKUP_DIR: &str = ".agora_save_backups";
const BACKUP_MANIFEST: &str = "backup.json";
const BACKUP_FILES: &str = "files";
const STAGING_PREFIX: &str = ".staging-";
/// Space kept free beyond the size of the copy, so a nearly full disk is not filled to the last
/// byte by a backup.
const DISK_HEADROOM: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SaveBackupError {
    #[error("instance '{0}' not found")]
    InstanceNotFound(String),
    #[error("this instance has no save folder Agora can find")]
    NoSaveFolder,
    #[error("the save folder is empty, so there is nothing to back up")]
    EmptySaveFolder,
    #[error(
        "there is not enough room to back up the saves: they take {needed} bytes and only {free} \
         bytes are free"
    )]
    NotEnoughRoom { needed: u64, free: u64 },
    #[error("backup '{0}' not found")]
    BackupNotFound(String),
    #[error("'{0}' is not a backup id")]
    InvalidBackupId(String),
    #[error("{0}")]
    Other(String),
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl From<SavesError> for SaveBackupError {
    fn from(error: SavesError) -> Self {
        match error {
            SavesError::NotFound(id) => SaveBackupError::InstanceNotFound(id),
            SavesError::NoSaveLocation { .. } => SaveBackupError::NoSaveFolder,
            other => SaveBackupError::Other(other.to_string()),
        }
    }
}

/// A completed backup of an instance's save folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveBackup {
    pub id: String,
    pub created_at: String,
    /// The folder the files were copied from, as the game saw it at backup time.
    pub source: PathBuf,
    /// The backup's own folder.
    pub path: PathBuf,
    pub file_count: usize,
    pub bytes: u64,
}

/// What a restore put back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveRestore {
    pub backup_id: String,
    /// The save folder the files were written back into.
    pub folder: PathBuf,
    /// Files written, including ones identical to the current save.
    pub restored: usize,
    /// Files whose current copy differed and was kept as a `.before-restore` sibling.
    pub kept_current: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredBackup {
    schema_version: u32,
    id: String,
    created_at: String,
    source: String,
    file_count: usize,
    bytes: u64,
    files: Vec<StoredFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFile {
    /// A `/`-separated path relative to the save folder.
    path: String,
    size: u64,
}

fn backup_root(ctx: &Ctx, instance_id: &str) -> Result<PathBuf, SaveBackupError> {
    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| SaveBackupError::Other(e.to_string()))?;
    Ok(instance_dir.join(BACKUP_DIR))
}

fn manifest_of(
    ctx: &Ctx,
    instance_id: &str,
) -> Result<game_instance::GameInstanceManifest, SaveBackupError> {
    game_instance::get_manifest(ctx, instance_id).map_err(|error| match error {
        InstanceError::NotFound(id) => SaveBackupError::InstanceNotFound(id),
        other => SaveBackupError::Other(other.to_string()),
    })
}

/// The folder the game reads and writes for this instance; the same resolver the INI & saves tab
/// uses, so the backup can never copy a different folder from the one the game uses.
pub fn save_folder(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<PathBuf, SaveBackupError> {
    let manifest = manifest_of(ctx, instance_id)?;
    let store =
        game_ini::store_of(ctx, instance_id).map_err(|e| SaveBackupError::Other(e.to_string()))?;
    let rule = game_saves::rule_for(definition, &store).ok_or(SaveBackupError::NoSaveFolder)?;
    match manifest.saves {
        SavesChoice::Own => game_saves::own_folder(rule, instance_id),
        SavesChoice::Shared => game_saves::shared_folder(rule),
    }
    .map_err(SaveBackupError::from)
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn read_error(path: &Path, source: std::io::Error) -> SaveBackupError {
    SaveBackupError::Read {
        path: path.to_path_buf(),
        source,
    }
}

fn write_error(path: &Path, source: std::io::Error) -> SaveBackupError {
    SaveBackupError::Write {
        path: path.to_path_buf(),
        source,
    }
}

/// Every file under `folder`, as `/`-separated relative paths, with its size.
fn collect_files(folder: &Path) -> Result<Vec<(String, u64)>, SaveBackupError> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, u64)>) -> Result<(), SaveBackupError> {
        let entries = fs::read_dir(dir).map_err(|source| read_error(dir, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| read_error(dir, source))?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| read_error(&path, source))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if metadata.file_type().is_symlink() {
                return Err(SaveBackupError::Other(format!(
                    "cannot back up {}: symbolic links are not supported",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                walk(&path, &child_rel, out)?;
            } else if metadata.is_file() {
                out.push((child_rel, metadata.len()));
            } else {
                return Err(SaveBackupError::Other(format!(
                    "cannot back up {}: it is not a file or a folder",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(folder, "", &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Refuse a copy the disk cannot hold, naming what is needed and what is free. Unknown free space
/// is not treated as a refusal: the copy itself still fails loudly if it runs out.
fn check_room(needed: u64, free: Option<u64>) -> Result<(), SaveBackupError> {
    if let Some(free) = free {
        if free < needed.saturating_add(DISK_HEADROOM) {
            return Err(SaveBackupError::NotEnoughRoom { needed, free });
        }
    }
    Ok(())
}

fn copy_tree(
    files: &[(String, u64)],
    source: &Path,
    destination: &Path,
) -> Result<(), SaveBackupError> {
    for (rel, _) in files {
        let from = source.join(rel);
        let to = destination.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|source| write_error(parent, source))?;
        }
        fs::copy(&from, &to).map_err(|source| write_error(&to, source))?;
    }
    Ok(())
}

/// A per-process sequence keeps ids in creation order when two backups share a clock tick.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn new_id() -> String {
    let stamp = Utc::now().format("%Y%m%dT%H%M%S");
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{stamp}-{sequence:06}-{}", &suffix[..8])
}

fn write_manifest(dir: &Path, stored: &StoredBackup) -> Result<(), SaveBackupError> {
    let path = dir.join(BACKUP_MANIFEST);
    let bytes = serde_json::to_vec_pretty(stored)
        .map_err(|e| SaveBackupError::Other(format!("cannot write the backup record: {e}")))?;
    fs::write(&path, bytes).map_err(|source| write_error(&path, source))
}

fn read_manifest(dir: &Path) -> Result<StoredBackup, SaveBackupError> {
    let path = dir.join(BACKUP_MANIFEST);
    let bytes = fs::read(&path).map_err(|source| read_error(&path, source))?;
    let stored: StoredBackup = serde_json::from_slice(&bytes).map_err(|e| {
        SaveBackupError::Other(format!(
            "cannot read the backup record {}: {e}",
            path.display()
        ))
    })?;
    if stored.schema_version > BACKUP_SCHEMA_VERSION {
        return Err(SaveBackupError::Other(format!(
            "backup {} was written by a newer Agora (format {})",
            stored.id, stored.schema_version
        )));
    }
    Ok(stored)
}

fn backup_from(stored: &StoredBackup, dir: &Path) -> SaveBackup {
    SaveBackup {
        id: stored.id.clone(),
        created_at: stored.created_at.clone(),
        source: PathBuf::from(&stored.source),
        path: dir.to_path_buf(),
        file_count: stored.file_count,
        bytes: stored.bytes,
    }
}

/// What a backup store holds, and the folders that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackupList {
    pub backups: Vec<SaveBackup>,
    /// One line per backup folder that could not be read. It is not listed and is never pruned.
    pub warnings: Vec<String>,
}

fn list_dir(root: &Path) -> Result<BackupList, SaveBackupError> {
    if !root.is_dir() {
        return Ok(BackupList::default());
    }
    let entries = fs::read_dir(root).map_err(|source| read_error(root, source))?;
    let mut list = BackupList::default();
    for entry in entries {
        let entry = entry.map_err(|source| read_error(root, source))?;
        let name = entry.file_name().to_string_lossy().to_string();
        // Staging folders are dot-prefixed and are never a finished backup.
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // One unreadable backup is reported and skipped; it must not hide the others, and it is
        // never treated as a finished backup, so pruning can never delete it.
        let stored = match read_manifest(&path) {
            Ok(stored) => stored,
            Err(error) => {
                list.warnings
                    .push(format!("backup {} was skipped: {error}", path.display()));
                continue;
            }
        };
        if !is_valid_id(&stored.id) {
            list.warnings.push(format!(
                "backup {} was skipped: '{}' is not a backup id",
                path.display(),
                stored.id
            ));
            continue;
        }
        list.backups.push(backup_from(&stored, &path));
    }
    list.backups.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(list)
}

fn prune(root: &Path, keep: &str) {
    let found = match list_dir(root) {
        Ok(found) => found,
        Err(_) => {
            eprintln!("[save-backup] retention skipped: the backup store could not be read");
            return;
        }
    };
    for warning in &found.warnings {
        eprintln!("[save-backup] {warning}");
    }
    for old in found.backups.into_iter().skip(MAX_BACKUPS) {
        if old.id == keep {
            continue;
        }
        if let Err(error) = fs::remove_dir_all(&old.path) {
            eprintln!(
                "[save-backup] could not remove old backup {}: {error}",
                old.path.display()
            );
        }
    }
}

/// Copy the instance's save folder into a new Agora-owned backup.
///
/// The size is measured before copying and the copy is refused when the disk lacks room. A failed
/// copy leaves nothing [`list`] returns: files are written into a staging folder and only renamed
/// into place once the record is written.
pub fn backup(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<SaveBackup, SaveBackupError> {
    let folder = save_folder(ctx, instance_id, definition)?;
    if !folder.is_dir() {
        return Err(SaveBackupError::NoSaveFolder);
    }
    let files = collect_files(&folder)?;
    if files.is_empty() {
        return Err(SaveBackupError::EmptySaveFolder);
    }
    let bytes = files.iter().map(|(_, size)| *size).sum::<u64>();
    let root = backup_root(ctx, instance_id)?;
    check_room(bytes, helpers::available_disk_space_bytes(&root))?;

    let id = new_id();
    let staging = root.join(format!("{STAGING_PREFIX}{id}"));
    fs::create_dir_all(&staging).map_err(|source| write_error(&staging, source))?;
    let files_dir = staging.join(BACKUP_FILES);
    let created_at = Utc::now().to_rfc3339();

    let result = (|| -> Result<(), SaveBackupError> {
        copy_tree(&files, &folder, &files_dir)?;
        let stored = StoredBackup {
            schema_version: BACKUP_SCHEMA_VERSION,
            id: id.clone(),
            created_at: created_at.clone(),
            source: folder.display().to_string(),
            file_count: files.len(),
            bytes,
            files: files
                .iter()
                .map(|(path, size)| StoredFile {
                    path: path.clone(),
                    size: *size,
                })
                .collect(),
        };
        write_manifest(&staging, &stored)?;
        fs::rename(&staging, root.join(&id)).map_err(|source| write_error(&staging, source))?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    prune(&root, &id);
    Ok(SaveBackup {
        path: root.join(&id),
        id,
        created_at,
        source: folder,
        file_count: files.len(),
        bytes,
    })
}

/// Every finished backup for an instance, newest first. A store Agora has never written to is
/// empty, not an error; a backup that exists but cannot be read is reported and skipped, so it
/// never hides the others. Use [`list_detailed`] to see the report.
pub fn list(ctx: &Ctx, instance_id: &str) -> Result<Vec<SaveBackup>, SaveBackupError> {
    let found = list_detailed(ctx, instance_id)?;
    for warning in &found.warnings {
        eprintln!("[save-backup] {warning}");
    }
    Ok(found.backups)
}

/// Every finished backup for an instance plus the folders that could not be read, newest first. A
/// store Agora has never written to is empty, not an error.
pub fn list_detailed(ctx: &Ctx, instance_id: &str) -> Result<BackupList, SaveBackupError> {
    list_dir(&backup_root(ctx, instance_id)?)
}

/// The backup taken before one session's crash test. It lives in the database, keyed by session,
/// not in user settings: clearing settings must not lose the answer screen's Restore button.
pub fn set_session_backup(
    conn: &Connection,
    instance_id: &str,
    session_id: &str,
    backup_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO crash_save_backups (session_id, instance_id, backup_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET
             instance_id = excluded.instance_id,
             backup_id = excluded.backup_id,
             created_at = excluded.created_at",
        params![session_id, instance_id, backup_id, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// The backup recorded for one session, if any.
pub fn session_backup(
    conn: &Connection,
    instance_id: &str,
    session_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT backup_id FROM crash_save_backups
              WHERE session_id = ?1 AND instance_id = ?2",
            params![session_id, instance_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn safe_relative(path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains(':')
        {
            return None;
        }
        out.push(part);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn same_contents(left: &Path, right: &Path) -> Result<bool, SaveBackupError> {
    let left_meta = fs::metadata(left).map_err(|source| read_error(left, source))?;
    let right_meta = fs::metadata(right).map_err(|source| read_error(right, source))?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut a = fs::File::open(left).map_err(|source| read_error(left, source))?;
    let mut b = fs::File::open(right).map_err(|source| read_error(right, source))?;
    let mut buf_a = [0u8; 64 * 1024];
    let mut buf_b = [0u8; 64 * 1024];
    loop {
        let read_a = a
            .read(&mut buf_a)
            .map_err(|source| read_error(left, source))?;
        let read_b = b
            .read(&mut buf_b)
            .map_err(|source| read_error(right, source))?;
        if read_a != read_b || buf_a[..read_a] != buf_b[..read_b] {
            return Ok(false);
        }
        if read_a == 0 {
            return Ok(true);
        }
    }
}

/// `dest` with a free `.before-restore` (then `-2`, `-3`, ...) suffix beside it.
fn kept_copy_path(dest: &Path) -> Result<PathBuf, SaveBackupError> {
    let name = dest
        .file_name()
        .ok_or_else(|| SaveBackupError::Other(format!("{} has no file name", dest.display())))?
        .to_string_lossy()
        .to_string();
    for index in 1..=u32::MAX {
        let candidate = if index == 1 {
            format!("{name}.before-restore")
        } else {
            format!("{name}.before-restore-{index}")
        };
        let path = dest.with_file_name(candidate);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(SaveBackupError::Other(format!(
        "cannot find a free name for a copy of {}",
        dest.display()
    )))
}

/// Put `from` at `to` without ever leaving `to` missing: copy to a temp name beside it, verify the
/// copy, and only then swap it in. A failed copy removes the temp and leaves the current save in
/// place. A current file that is replaced is kept beside itself in `kept_current`.
fn copy_into_place(
    from: &Path,
    to: &Path,
    kept_current: &mut Vec<String>,
) -> Result<(), SaveBackupError> {
    let Some(parent) = to.parent() else {
        return Err(SaveBackupError::Other(format!(
            "{} has no folder",
            to.display()
        )));
    };
    let name = to
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("save");
    let temp = parent.join(format!(
        ".{name}.agora-restore-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(source) = fs::copy(from, &temp) {
        let _ = fs::remove_file(&temp);
        return Err(read_error(from, source));
    }
    if !same_contents(from, &temp)? {
        let _ = fs::remove_file(&temp);
        return Err(SaveBackupError::Other(format!(
            "the copy of {} does not match its backup; the current save was left in place",
            to.display()
        )));
    }
    let kept = if to.is_file() {
        let kept = kept_copy_path(to)?;
        if let Err(source) = fs::rename(to, &kept) {
            let _ = fs::remove_file(&temp);
            return Err(write_error(to, source));
        }
        Some(kept)
    } else {
        None
    };
    if let Err(source) = fs::rename(&temp, to) {
        let _ = fs::remove_file(&temp);
        // Put the current save back under its own name; a crash here still leaves it in `kept`.
        if let Some(kept) = &kept {
            let _ = fs::rename(kept, to);
        }
        return Err(write_error(to, source));
    }
    if let Some(kept) = kept {
        kept_current.push(kept.display().to_string());
    }
    Ok(())
}

/// Put a backup's files back into the instance's save folder.
///
/// Never deletes. A current file of the same name is kept beside itself as
/// `<name>.before-restore` before the backed-up copy replaces it; files that are not in the
/// backup, including saves made after it, are left exactly as they are.
pub fn restore(
    ctx: &Ctx,
    instance_id: &str,
    _definition: &GameDefinition,
    backup_id: &str,
) -> Result<SaveRestore, SaveBackupError> {
    if !is_valid_id(backup_id) {
        return Err(SaveBackupError::InvalidBackupId(backup_id.to_string()));
    }
    // Hold the instance lock across the whole restore, exactly as a snapshot restore does, and
    // refuse while the game is running: writing saves under a live game corrupts them. The one
    // activity check also sees an open session with no journaled files and a Linux view session,
    // and refuses when a store that would answer cannot be read.
    let _guard = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "restore_save_backup",
        )
        .map_err(|error| SaveBackupError::Other(error.to_string()))?;
    crate::instance_runtime::ensure_game_not_running(ctx, instance_id, "restoring its saves")
        .map_err(SaveBackupError::Other)?;
    let root = backup_root(ctx, instance_id)?;
    let dir = root.join(backup_id);
    if !dir.is_dir() {
        return Err(SaveBackupError::BackupNotFound(backup_id.to_string()));
    }
    let stored = read_manifest(&dir)?;
    if stored.id != backup_id {
        return Err(SaveBackupError::Other(format!(
            "backup {backup_id} records a different id ({})",
            stored.id
        )));
    }

    // The folder the backup was taken from, as it was recorded at backup time. The instance's
    // current Shared/Own choice may have changed since, and the restore must not follow it: a
    // backup taken under Own belongs back in the Own folder, and a shared save belongs in the
    // shared folder it came from.
    let folder = PathBuf::from(&stored.source);
    fs::create_dir_all(&folder).map_err(|source| write_error(&folder, source))?;
    let files_dir = dir.join(BACKUP_FILES);

    let mut restored = 0usize;
    let mut kept_current = Vec::new();
    for file in &stored.files {
        let Some(relative) = safe_relative(&file.path) else {
            return Err(SaveBackupError::Other(format!(
                "backup {backup_id} holds an unsafe path ({})",
                file.path
            )));
        };
        let from = files_dir.join(&relative);
        let to = folder.join(&relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|source| write_error(parent, source))?;
        }
        if to.is_file() && same_contents(&from, &to)? {
            restored += 1;
            continue;
        }
        copy_into_place(&from, &to, &mut kept_current)?;
        restored += 1;
    }

    Ok(SaveRestore {
        backup_id: backup_id.to_string(),
        folder,
        restored,
        kept_current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_that_does_not_fit_is_refused_with_both_numbers() {
        let error = check_room(12 * 1024 * 1024, Some(20 * 1024 * 1024)).unwrap_err();
        match error {
            SaveBackupError::NotEnoughRoom { needed, free } => {
                assert_eq!(needed, 12 * 1024 * 1024);
                assert_eq!(free, 20 * 1024 * 1024);
            }
            other => panic!("expected NotEnoughRoom, got {other:?}"),
        }
        assert!(check_room(1024, Some(20 * 1024 * 1024)).is_ok());
    }

    #[test]
    fn unknown_free_space_does_not_refuse_the_copy() {
        assert!(check_room(8 * 1024 * 1024 * 1024, None).is_ok());
    }

    #[test]
    fn a_backup_record_cannot_reach_outside_the_save_folder() {
        assert!(safe_relative("saves/Run.ess").is_some());
        assert!(safe_relative("../escape.ess").is_none());
        assert!(safe_relative("a/../../escape.ess").is_none());
        assert!(safe_relative("/absolute.ess").is_none());
        assert!(safe_relative("C:/escape.ess").is_none());
        assert!(safe_relative(r"a\b.ess").is_none());
        assert!(safe_relative("").is_none());
    }

    #[test]
    fn a_session_backup_marker_is_kept_in_the_database() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();

        assert_eq!(session_backup(&conn, "inst", "session-1").unwrap(), None);
        set_session_backup(&conn, "inst", "session-1", "backup-1").unwrap();
        assert_eq!(
            session_backup(&conn, "inst", "session-1").unwrap(),
            Some("backup-1".to_string())
        );
        // A second test in the same session replaces the marker rather than adding a row.
        set_session_backup(&conn, "inst", "session-1", "backup-2").unwrap();
        assert_eq!(
            session_backup(&conn, "inst", "session-1").unwrap(),
            Some("backup-2".to_string())
        );
        assert_eq!(session_backup(&conn, "inst", "session-2").unwrap(), None);
    }

    #[test]
    fn a_kept_copy_never_clobbers_an_earlier_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("Run.ess");
        std::fs::write(&dest, b"current").unwrap();

        let first = kept_copy_path(&dest).unwrap();
        std::fs::write(&first, b"earlier kept copy").unwrap();
        let second = kept_copy_path(&dest).unwrap();

        assert_eq!(second, tmp.path().join("Run.ess.before-restore-2"));
    }
}
