//! Placing stored objects into a folder as hard links or copies, and the setting that chooses.
//!
//! A link to a protected object can only be deleted when the folder holding it grants the user
//! `FILE_DELETE_CHILD`; a folder with only the usual `Modify` right does not. So link placements
//! make sure that grant exists (once per folder in this process) and fall back to an owned copy
//! when they cannot: another volume, a file system without links, the NTFS link limit, or a folder
//! whose ACL cannot be changed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::protect;
use super::ContentError;

/// How a placement should try to put a stored object at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementMode {
    /// Hard link when the file system allows it, else an owned copy. The default.
    Link,
    /// Always an owned, writable copy.
    Copy,
}

/// What a placement actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacedAs {
    Link,
    Copy,
}

/// "Share mod files between instances": on means placements try hard links, off means copies.
/// An absent setting is on.
pub const SHARE_CONTENT_SETTING: &str = "share_content_between_instances";

/// The mode new placements should use. An absent setting is [`PlacementMode::Link`]. A value that
/// is not a boolean (a malformed one included) is an error, not the default: the caller must not
/// silently get sharing it did not ask for, or stop sharing it did ask for.
pub fn placement_mode(ctx: &crate::ctx::Ctx) -> Result<PlacementMode, ContentError> {
    let conn = connection(ctx)?;
    mode_from_conn(&conn)
}

/// Store the placement mode the user chose.
pub fn set_placement_mode(ctx: &crate::ctx::Ctx, mode: PlacementMode) -> Result<(), ContentError> {
    let conn = connection(ctx)?;
    crate::db::set_setting(
        &conn,
        SHARE_CONTENT_SETTING,
        &serde_json::Value::Bool(mode == PlacementMode::Link),
    )
    .map_err(|e| ContentError::InvalidSetting(e.to_string()))
}

fn connection(ctx: &crate::ctx::Ctx) -> Result<Connection, ContentError> {
    crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|e| ContentError::InvalidSetting(e.to_string()))
}

fn mode_from_conn(conn: &Connection) -> Result<PlacementMode, ContentError> {
    match crate::db::get_setting(conn, SHARE_CONTENT_SETTING)
        .map_err(|e| ContentError::InvalidSetting(e.to_string()))?
    {
        None => Ok(PlacementMode::Link),
        Some(serde_json::Value::Bool(true)) => Ok(PlacementMode::Link),
        Some(serde_json::Value::Bool(false)) => Ok(PlacementMode::Copy),
        Some(other) => Err(ContentError::InvalidSetting(format!(
            "'{SHARE_CONTENT_SETTING}' must be true or false, found {other}"
        ))),
    }
}

/// Put the stored object `sha256` at `target`, as a hard link or an owned copy according to `mode`,
/// and say which. The folder is created if needed.
///
/// An existing target is never replaced: the call returns [`ContentError::TargetExists`] and leaves
/// the file alone.
pub fn place_object_at(
    ctx: &crate::ctx::Ctx,
    sha256: &str,
    target: &Path,
    mode: PlacementMode,
) -> Result<PlacedAs, ContentError> {
    if !is_sha256_hex(sha256) {
        return Err(ContentError::InvalidHash(sha256.to_string()));
    }
    let object = ctx.paths.content_object_path(sha256);
    if !object.is_file() {
        return Err(ContentError::MissingObject(sha256.to_string()));
    }
    ensure_absent(target)?;

    let folder = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(folder)?;

    match mode {
        PlacementMode::Copy => {
            ensure_absent(target)?;
            copy_owned(&object, target)?;
            Ok(PlacedAs::Copy)
        }
        PlacementMode::Link => {
            if at_link_limit(&object) || ensure_delete_child(folder).is_err() {
                ensure_absent(target)?;
                copy_owned(&object, target)?;
                return Ok(PlacedAs::Copy);
            }
            match std::fs::hard_link(&object, target) {
                Ok(()) => Ok(PlacedAs::Link),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(ContentError::TargetExists(target.display().to_string()))
                }
                // A link that cannot be made is a copy, and the result says so.
                Err(_) => {
                    ensure_absent(target)?;
                    copy_owned(&object, target)?;
                    Ok(PlacedAs::Copy)
                }
            }
        }
    }
}

fn ensure_absent(target: &Path) -> Result<(), ContentError> {
    // `symlink_metadata` sees a broken link too, which `exists` would call absent.
    if target.symlink_metadata().is_ok() {
        return Err(ContentError::TargetExists(target.display().to_string()));
    }
    Ok(())
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn delete_child_folders() -> &'static Mutex<HashSet<PathBuf>> {
    static FOLDERS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    FOLDERS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Make sure a file placed in `folder` can be deleted from it. The grant is written once per
/// folder in this process; a folder whose ACL cannot be changed is an error, and the caller places
/// a copy instead.
fn ensure_delete_child(folder: &Path) -> Result<(), std::io::Error> {
    let key = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    if delete_child_folders()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&key)
    {
        return Ok(());
    }
    protect::grant_delete_child(folder)?;
    delete_child_folders()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key);
    Ok(())
}

/// Copy a file the game may then write. `fs::copy` carries the source's permissions, and on Unix
/// a content store object is read-only; on Windows its protection is an ACL a copy does not take.
pub(crate) fn copy_owned(source: &Path, dest: &Path) -> std::io::Result<u64> {
    let bytes = std::fs::copy(source, dest)?;
    #[cfg(unix)]
    protect::unprotect(dest)?;
    Ok(bytes)
}

const NTFS_LINK_LIMIT: u32 = 1023;

/// Whether the file already has as many hard links as the file system allows. NTFS permits 1,023
/// links to one file; identical bytes share one content object, so a mod with thousands of
/// identical files would otherwise keep trying a link that cannot be made.
pub(crate) fn at_link_limit(path: &Path) -> bool {
    link_count(path).is_some_and(|n| n >= NTFS_LINK_LIMIT)
}

#[cfg(windows)]
fn link_count(path: &Path) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let file = std::fs::File::open(path).ok()?;
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is valid, and the handle is open for the call.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
    (ok != 0).then_some(info.nNumberOfLinks)
}

#[cfg(unix)]
fn link_count(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| u32::try_from(m.nlink()).unwrap_or(u32::MAX))
}

#[cfg(not(any(windows, unix)))]
fn link_count(_path: &Path) -> Option<u32> {
    None
}
