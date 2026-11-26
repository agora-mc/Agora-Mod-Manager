//! Content store for immutable mod assets (MASTER_SPEC §26.6).
//!
//! A mod's files are stored once, by content hash, protected from modification,
//! and verifiable. Nothing deploys content to a game yet (slice 2).

pub mod collect;
#[cfg(test)]
mod collect_tests;
pub mod convert;
#[cfg(test)]
mod import_bench;
pub mod pending;
pub mod place;
pub mod protect;
mod rar;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use agora_game_api::{
    classify_entry_name, ArchiveError, ArchiveLimits, EntryName, RelPath, SafeArchive, Select,
    StrictRelPath,
};
/// How an import treats entries whose names a disk cannot hold, and the entries it left out: the
/// game contract's types, re-exported so a CLI or desktop caller needs one import, not two.
pub use agora_game_api::{NamePolicy, SkippedEntry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_paths::AppPaths;
use crate::ctx::Ctx;
use crate::lock_manager::LockResource;

pub use crate::game_base::VerifyDepth;
pub use place::{
    place_object_at, placement_mode, set_placement_mode, PlacedAs, PlacementMode,
    SHARE_CONTENT_SETTING,
};
pub use protect::Protection;

// ---------------------------------------------------------------------------
// Manifest and Outcome Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentFile {
    pub path: RelPath,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentSource {
    Archive {
        path: String,
        sha256: String,
        added_at_unix_ms: i64,
    },
    Folder {
        path: String,
        added_at_unix_ms: i64,
    },
    /// Derived from another item by a FOMOD installer (`content_fomod`): the chosen options are
    /// recorded so that a reinstall replays them.
    FomodInstall {
        from_item: String,
        choices: Vec<crate::content_fomod::Choice>,
        added_at_unix_ms: i64,
    },
    Thunderstore {
        from_item: String,
        package: String,
        version: String,
        added_at_unix_ms: i64,
    },
    /// A mod folder or an `overwrite` folder of a Mod Organizer 2 setup, imported as it is on disk
    /// (MASTER_SPEC §26.10). `provenance` is what `meta.ini` claims; nothing has verified it.
    Mo2Import {
        /// The setup's ModOrganizer.ini, as the import was run with it.
        setup: String,
        /// The folder's name in the MO2 setup, e.g. `090 - Nemesis Unlimited Behavior Engine 0.84`.
        mo2_folder: String,
        provenance: Mo2Provenance,
        added_at_unix_ms: i64,
    },
    /// A mod folder of a Vortex setup, imported as it is in the staging folder (MASTER_SPEC
    /// §26.10). `provenance` is what Vortex's state claims; no hash has verified it.
    VortexImport {
        /// The Vortex store it was read from (`state.v2`), as the import was run with it.
        store: String,
        /// Vortex's own game id, e.g. `skyrimse`.
        game: String,
        /// Vortex's mod id.
        mod_id: String,
        /// The mod's folder inside the staging folder (`installationPath`).
        installation_folder: String,
        provenance: VortexProvenance,
        added_at_unix_ms: i64,
    },
    /// An archive from the curated catalog for another game (MASTER_SPEC §26.8): the entry, the
    /// release and asset it came from, and whether a hash its source published checked the bytes.
    /// `release` is `None` for a `direct_hash` file. An unverified item's hash is remembered, so a
    /// later install of the same release file can ask the user before it differs.
    Catalog {
        item_id: String,
        game: String,
        release: Option<String>,
        asset: String,
        sha256: String,
        verified: bool,
        added_at_unix_ms: i64,
    },
    /// One file added on its own, without unpacking it: a `.jar` or `.zip` is this one file, never
    /// its entries.
    File {
        path: String,
        added_at_unix_ms: i64,
    },
}

/// What a mod's `meta.ini` claims about where it came from. An absent or unreadable `meta.ini` is
/// recorded as such, and the item is still imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Mo2Provenance {
    /// `meta.ini` was read. These are the file's own claims: no hash checks them against a
    /// download, so `note` says "provenance claimed, not verified".
    Claimed {
        modid: Option<u64>,
        version: Option<String>,
        installation_file: Option<String>,
        repository: Option<String>,
        game_name: Option<String>,
        note: String,
    },
    /// The folder has no `meta.ini`.
    Absent,
    /// A `meta.ini` exists but could not be read as one.
    Unreadable { reason: String },
}

/// What a Vortex `IMod` claims about where it came from. Vortex's state is the source, not a
/// verified download, so `note` says the claim was not checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VortexProvenance {
    /// The mod entry was read. These are its own claims.
    Claimed {
        modid: Option<u64>,
        fileid: Option<u64>,
        version: Option<String>,
        name: Option<String>,
        note: String,
    },
    /// The mod entry has no `attributes`.
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentItem {
    pub item_id: String,
    pub name: String,
    pub files: Vec<ContentFile>, // sorted by path
    pub total_size: u64,
    pub sources: Vec<ContentSource>, // oldest first
    pub added_at_unix_ms: i64,
    /// What the item is, in the user's or catalog's words. All metadata is optional; an item
    /// written before these fields existed reads with every one of them absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
    /// A generic label for the kind of source that supplied the item, chosen by the caller
    /// (`catalog`, `modrinth`, `manual`, `provider`, ...). Core does not interpret it; an offline
    /// add uses it to restore the same provenance the install recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The id that provider uses for the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Opaque provider provenance the installer recorded, so an offline add can restore it
    /// exactly. Core stores and returns it without interpreting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_origin: Option<String>,
    /// The SHA-256 of a stored object holding the icon, never a URL alone: the bytes are in the
    /// store like any other content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_sha256: Option<String>,
}

/// The optional description of an item that [`set_item_metadata`] replaces. Every field absent
/// means the item carries no metadata for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemMetadata {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub page_url: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub game_id: Option<String>,
    #[serde(default)]
    pub catalog_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub provider_origin: Option<String>,
    #[serde(default)]
    pub icon_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AddOutcome {
    Added {
        item: ContentItem,
        #[serde(alias = "new")]
        objects_new: usize,
        /// The archive entries that were left out because a disk cannot hold them, present only
        /// when the caller asked for [`NamePolicy::WithoutThem`]. Never silent: a front end shows
        /// this list to the person who asked for the import (MASTER_SPEC §32.3).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<SkippedEntry>,
    },
    Existing {
        item: ContentItem,
        #[serde(alias = "new")]
        objects_new: usize,
        #[serde(alias = "restored")]
        objects_restored: usize,
        #[serde(alias = "present")]
        objects_present: usize,
        /// See [`AddOutcome::Added`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<SkippedEntry>,
    },
}

impl AddOutcome {
    pub fn item(&self) -> &ContentItem {
        match self {
            AddOutcome::Added { item, .. } => item,
            AddOutcome::Existing { item, .. } => item,
        }
    }

    /// The entries that were not stored because a disk cannot hold their names.
    pub fn skipped(&self) -> &[SkippedEntry] {
        match self {
            AddOutcome::Added { skipped, .. } => skipped,
            AddOutcome::Existing { skipped, .. } => skipped,
        }
    }

    pub fn objects_new(&self) -> usize {
        match self {
            AddOutcome::Added { objects_new, .. } => *objects_new,
            AddOutcome::Existing { objects_new, .. } => *objects_new,
        }
    }

    pub fn objects_restored(&self) -> usize {
        match self {
            AddOutcome::Added { .. } => 0,
            AddOutcome::Existing {
                objects_restored, ..
            } => *objects_restored,
        }
    }

    pub fn objects_present(&self) -> usize {
        match self {
            AddOutcome::Added { .. } => 0,
            AddOutcome::Existing {
                objects_present, ..
            } => *objects_present,
        }
    }
}

// ---------------------------------------------------------------------------
// Problem and Verification Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProblemKind {
    Missing,
    SizeMismatch { expected: u64, actual: u64 },
    HashMismatch { expected: String, actual: String },
    Unprotected,
    CorruptManifest { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentProblem {
    pub item_id: String,
    pub path: String,
    pub kind: ProblemKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentVerification {
    pub item_id: String,
    pub checked: usize,
    pub hashed: usize,
    pub problems: Vec<ContentProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentVerificationReport {
    pub checked_items: usize,
    pub checked_files: usize,
    pub hashed_files: usize,
    pub problems: Vec<ContentProblem>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("item not found: {0}")]
    NotFound(String),
    #[error("prefix '{prefix}' matches multiple items: {matches}")]
    AmbiguousPrefix { prefix: String, matches: String },
    #[error("invalid path '{path}': {reason}")]
    InvalidPath { path: String, reason: String },
    /// Entries whose names a disk cannot hold: a Windows reserved name, a trailing dot or space, a
    /// `:`, an illegal character, or two names one case-insensitive disk stores as one.
    ///
    /// Refused by default, with every offending entry named, because an import that silently drops
    /// files is worse than one that stops. A caller may instead ask for the entries that can be
    /// written ([`NamePolicy::WithoutThem`]) and is then handed the rest in
    /// [`AddOutcome::skipped`] (MASTER_SPEC §32.3).
    #[error("{}", unwritable_names_message(archive, entries))]
    UnwritableNames {
        archive: String,
        entries: Vec<SkippedEntry>,
    },
    #[error("item has no files")]
    EmptyItem,
    #[error("archive entry '{path}' declared size {declared} but yielded more")]
    EntryExceedsDeclaredSize { path: String, declared: u64 },
    #[error("insufficient disk space: need {required} bytes (including 1 GiB headroom), but only {available} bytes available")]
    InsufficientSpace { required: u64, available: u64 },
    #[error("cannot remove item {item_id}: in use by instance(s): {instances}")]
    InUse {
        item_id: String,
        /// The instances' display names, for people (the Library and CLI show this).
        instances: String,
        /// The instances' ids, for code.
        instance_ids: Vec<String>,
    },
    #[error("cannot remove item {item_id}: manifest '{manifest}' is unreadable: {error}")]
    UnreadableManifest {
        item_id: String,
        manifest: String,
        error: String,
    },
    /// Snapshots that cannot be read might still name the item. The message
    /// names each one and the command that deletes it; `snapshots` carries the
    /// same facts for a front end that offers the action.
    #[error("cannot remove item {item_id}: {message}")]
    UnreadableSnapshots {
        item_id: String,
        message: String,
        snapshots: Vec<crate::game_snapshot::UnreadableSnapshotReference>,
    },
    #[error("unsupported archive format: supported formats are zip, 7z, rar")]
    UnsupportedArchiveFormat,
    #[error("archive is password-protected: password-protected archives are not supported")]
    PasswordProtected,
    #[error("corrupt archive: {0}")]
    CorruptArchive(String),
    #[error("lock error: {0}")]
    Lock(#[from] crate::error::LauncherError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
    #[error("not a SHA-256 hash: {0}")]
    InvalidHash(String),
    #[error("target already exists: {0}")]
    TargetExists(String),
    #[error("stored object is missing: {0}")]
    MissingObject(String),
    #[error("stored object is damaged or not what its hash says: {0}")]
    DamagedObject(String),
    #[error("content placement setting is invalid: {0}")]
    InvalidSetting(String),
    #[error("instance '{0}' is running; stop it before changing how its files are stored")]
    InstanceBusy(String),
}

/// The refusal for names a disk cannot hold, with every entry and its reason on its own line.
fn unwritable_names_message(archive: &str, entries: &[SkippedEntry]) -> String {
    let mut message = format!(
        "'{archive}' has {} entr{} a disk cannot hold, so nothing was imported:",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" }
    );
    for entry in entries {
        message.push_str(&format!("\n  '{}' {}", entry.name, entry.reason));
    }
    message
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const ONE_GIB: u64 = 1024 * 1024 * 1024;
const HASH_BUFFER_SIZE: usize = 128 * 1024;

pub(crate) fn now_unix_ms() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Reserved DOS device names: the game contract's rule, so a plugin and the content store refuse
/// the same ones. See [`agora_game_api::is_windows_device_name`].
pub use agora_game_api::is_windows_device_name;

/// Whether `path` is a link that redirects a name, from the game contract: a symbolic link, a
/// junction or a mount point. A cloud placeholder or a deduplicated file is an ordinary file, so a
/// library that lives on OneDrive imports like any other.
///
/// NOTE: an archive's *stored attributes* are not a path. A 7z entry carries the reparse-point bit
/// and no tag, so an archive entry that claims one is refused as unclassifiable — see
/// [`list_sevenz_entries`].
pub(crate) fn is_reparse_point_or_symlink(path: &std::path::Path) -> bool {
    agora_game_api::redirects_a_name(path)
}

/// The strict path rule the game contract owns: an archive entry becomes a file name only when
/// something writes it, and the rule it is held to is [`agora_game_api::RelPath::strict`]. The
/// result is the strict type, so an archive entry cannot reach a write without having been held to
/// the rule.
pub fn validate_entry_path(rel_str: &str) -> Result<StrictRelPath, ContentError> {
    RelPath::strict(rel_str).map_err(|error| ContentError::InvalidPath {
        path: rel_str.to_string(),
        reason: error.reason().to_string(),
    })
}

/// Hold a path this layer already holds to the strict rule before anything writes it, reporting the
/// value that fails rather than dropping it.
pub fn strict_before_write(path: &RelPath) -> Result<StrictRelPath, ContentError> {
    path.check_strict()
        .map_err(|error| ContentError::InvalidPath {
            path: path.to_string(),
            reason: error.reason().to_string(),
        })
}

/// The spelling NTFS compares names by: every Unicode letter folded, not just ASCII. Upper then
/// lower, so a letter with no single lower-case form (such as 'ß') still meets its capital. This
/// errs toward calling two names equal, which only costs a refusal.
pub(crate) fn fold_case(name: &str) -> String {
    name.chars()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Refuse a set of paths one case-insensitive disk cannot hold separately: the game contract's
/// rule, so a plugin and the content store refuse the same sets.
pub fn validate_path_set(paths: &[StrictRelPath]) -> Result<(), ContentError> {
    agora_game_api::validate_path_set(paths).map_err(|error| {
        let reason = error.reason();
        match error {
            agora_game_api::PathSetError::EmptySet => ContentError::EmptyItem,
            agora_game_api::PathSetError::Collides { path }
            | agora_game_api::PathSetError::FolderIsAFile { path, .. } => {
                ContentError::InvalidPath { path, reason }
            }
        }
    })
}

fn disk_free_space(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut cur = if path.exists() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| path.to_path_buf())
        };
        while !cur.exists() {
            if let Some(parent) = cur.parent() {
                cur = parent.to_path_buf();
            } else {
                break;
            }
        }
        let wide: Vec<u16> = cur
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_bytes_available: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut total_free_bytes: u64 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_bytes_available,
                &mut total_bytes,
                &mut total_free_bytes,
            )
        };
        if ok != 0 {
            return Some(free_bytes_available);
        }
    }
    crate::helpers::available_disk_space_bytes(path)
}

/// Refuse a declared total size that cannot fit, including 1 GiB of headroom. Free space that
/// cannot be determined refuses too: an unknown value must not silently disable the limit. The
/// arithmetic is checked, so an absurd declared size cannot wrap.
pub(crate) fn ensure_room_for_declared(
    content_root: &Path,
    declared_total: u64,
) -> Result<(), ContentError> {
    declared_size_fits(declared_total, disk_free_space(content_root))
}

/// The decision [`ensure_room_for_declared`] makes, separate from the platform query so it can be
/// tested where free space is always known.
fn declared_size_fits(declared_total: u64, available: Option<u64>) -> Result<(), ContentError> {
    let required = declared_total
        .checked_add(ONE_GIB)
        .ok_or_else(|| ContentError::Other("the declared size is too large".into()))?;
    let available = available.ok_or_else(|| {
        ContentError::Other(
            "Agora could not check the free disk space, so it did not accept this content".into(),
        )
    })?;
    if available < required {
        return Err(ContentError::InsufficientSpace {
            required,
            available,
        });
    }
    Ok(())
}

/// Whether an existing object can be kept as is. A protected object of the right size is trusted
/// (the quick check, as `VerifyDepth::Quick`); one that was unprotected or has the wrong size may
/// have been changed, so its bytes are re-hashed.
pub(crate) fn object_is_trustworthy(
    path: &Path,
    size: u64,
    sha256: &str,
) -> Result<bool, ContentError> {
    if std::fs::metadata(path)?.len() != size {
        return Ok(false);
    }
    if protect::protection(path)? == Protection::Protected {
        return Ok(true);
    }
    Ok(hash_file_path(path)? == sha256)
}

struct StagingGuard {
    path: PathBuf,
    active: bool,
}

impl StagingGuard {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            active: true,
        }
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.active && self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn clean_staging_dir(paths: &AppPaths) {
    let staging_root = paths.content_staging_dir();
    let entries = match std::fs::read_dir(&staging_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "[content] staging sweep skipped: cannot list {}: {error}",
                staging_root.display()
            );
            return;
        }
    };
    // Decide the whole sweep before deleting anything: one entry that cannot
    // be listed makes the listing incomplete, so nothing is removed on a guess.
    let mut to_remove = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => to_remove.push(entry.path()),
            Err(error) => {
                eprintln!("[content] staging sweep skipped: an entry could not be listed: {error}");
                return;
            }
        }
    }
    for p in to_remove {
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(&p);
        } else {
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn hash_file_path(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_SIZE];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// [`hash_file_path`] that also counts the bytes it hashed, so a file's size and its hash come from
/// the same read of the same bytes.
pub(super) fn hash_file_counting(path: &Path) -> Result<(String, u64), std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; HASH_BUFFER_SIZE];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buffer[..n]);
    }
    Ok((format!("{:x}", hasher.finalize()), total))
}

/// Copy `src` to `dst` while hashing the bytes, reading through `buf`. Returns the bytes copied and
/// their SHA-256 (lowercase hex).
fn copy_and_hash(src: &Path, dst: &Path, buf: &mut [u8]) -> Result<(u64, String), std::io::Error> {
    use std::io::Write;
    let mut file_in = std::fs::File::open(src)?;
    let mut staged_out = std::fs::File::create(dst)?;
    let mut hasher = Sha256::new();
    let mut bytes_written = 0u64;
    loop {
        let n = file_in.read(buf)?;
        if n == 0 {
            break;
        }
        bytes_written += n as u64;
        staged_out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    Ok((bytes_written, format!("{:x}", hasher.finalize())))
}

// ---------------------------------------------------------------------------
// Item Id and Manifest Operations
// ---------------------------------------------------------------------------

pub fn compute_canonical_item_id(files: &[ContentFile]) -> String {
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));

    let mut canonical = String::new();
    for f in sorted {
        use std::fmt::Write;
        let _ = writeln!(canonical, "{}\t{}\t{}", f.path.as_str(), f.size, f.sha256);
    }
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

fn write_manifest_atomic(paths: &AppPaths, item: &ContentItem) -> Result<(), ContentError> {
    let items_dir = paths.content_items_dir();
    std::fs::create_dir_all(&items_dir)?;

    let target_path = paths.content_item_path(&item.item_id);
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let tmp_path = items_dir.join(format!("{}.json.tmp-{}", item.item_id, unique));

    let json_bytes = serde_json::to_string_pretty(item)?;
    std::fs::write(&tmp_path, json_bytes)?;
    std::fs::rename(&tmp_path, &target_path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Adding: Archives and Folders
// ---------------------------------------------------------------------------

struct StagedEntry {
    rel_path: StrictRelPath,
    staged_path: PathBuf,
    size: u64,
    sha256: String,
}

/// Open an archive with the one reader (MASTER_SPEC §32.3), under the store's own preset.
///
/// `GAME_MOD` carries the numbers the store enforced before the reader existed, and meant them:
/// none for sizes, because a Skyrim texture or a BSA/BA2 mod is routinely past the limits a Technic
/// pack is held to. What the store did check — each entry's path, a link, case-insensitive
/// collisions, a password — is the reader's own rule, unchanged.
///
/// The RAR backend is the one core fills in (`rar.rs`); a zip, 7z or tar never touches it, and a
/// RAR read where it cannot run is an error rather than an empty listing.
fn open_archive(archive_path: &Path) -> Result<SafeArchive, ContentError> {
    SafeArchive::open_with_rar(archive_path, ArchiveLimits::GAME_MOD, rar::rar_reader())
        .map_err(archive_error)
}

/// The reader's refusal, as the store's own error.
///
/// The store answers with the kinds it always has, so a caller that matched on them (a download
/// watcher skipping a file that is not an archive, the desktop showing a reason) keeps working:
/// what changed is which code decides, not what a person is told.
fn archive_error(error: ArchiveError) -> ContentError {
    match error {
        ArchiveError::Unreadable {
            path,
            error: reason,
        } if reason == "it is missing" => ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("archive not found: {path}"),
        )),
        // Never an empty item, and never the store's path rules refusing a file that is simply not
        // an archive this build reads.
        ArchiveError::UnknownFormat { .. } => ContentError::UnsupportedArchiveFormat,
        ArchiveError::PasswordProtected { .. } => ContentError::PasswordProtected,
        ArchiveError::Corrupt { reason, .. } => ContentError::CorruptArchive(reason),
        ArchiveError::EntrySizeLies {
            entry, declared, ..
        } => ContentError::EntryExceedsDeclaredSize {
            path: entry,
            declared,
        },
        ArchiveError::Escapes { entry, reason, .. } => ContentError::InvalidPath {
            path: entry,
            reason,
        },
        ArchiveError::LinkEntry { entry, .. } => ContentError::InvalidPath {
            path: entry,
            reason: "an archive entry that claims to be a link (a reparse point) is not allowed: \
                     its tag is not stored, so it cannot be told from one that redirects"
                .into(),
        },
        ArchiveError::DuplicateNames { names, .. } => ContentError::InvalidPath {
            path: names.join(", "),
            reason:
                "the archive holds more than one entry with this name, and which one was meant \
                     cannot be told"
                    .into(),
        },
        // A name a disk cannot hold is the one refusal a person can answer: the entries are named,
        // and a caller may ask to import the rest (`NamePolicy::WithoutThem`).
        ArchiveError::UnsafeNames { path, entries } => ContentError::UnwritableNames {
            archive: path,
            entries,
        },
        other => ContentError::CorruptArchive(other.to_string()),
    }
}

/// Read every file entry into the store's own staging folder, and hash it there.
///
/// One walk of the archive, in the archive's own order, with the one reader deciding every name, the
/// path set, a link and the duplicates (§32.3); nothing is read twice and nothing is re-decided
/// here. The free-space check comes first, on the bytes the archive *declares*: they are not
/// trusted, but a declared total that cannot fit is refused before anything is written.
///
/// The bytes are hashed from the files the reader wrote, not from a count it reported, so an entry
/// that delivered something other than what it declared cannot reach the object store.
fn stage_archive(
    ctx: &Ctx,
    archive: &mut SafeArchive,
    names: NamePolicy,
) -> Result<(Vec<StagedEntry>, Vec<SkippedEntry>, StagingGuard, PathBuf), ContentError> {
    // Untrusted sizes: check free disk space.
    let content_root = ctx.paths.content_root();
    std::fs::create_dir_all(&content_root)?;
    ensure_room_for_declared(&content_root, archive.total_declared_bytes())?;

    // Staging directory setup, in the store's own staging area so placing an object is a rename.
    let staging_root = ctx.paths.content_staging_dir();
    std::fs::create_dir_all(&staging_root)?;
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let staging_dir = staging_root.join(&unique);
    std::fs::create_dir_all(&staging_dir)?;
    // Made before the extraction, so an error half way through leaves nothing behind.
    let guard = StagingGuard::new(&staging_dir);

    let report = archive
        .extract_checked(&staging_dir, names, &mut |_| Select::Write, &|| false)
        .map_err(archive_error)?;
    if report.written.is_empty() {
        return Err(ContentError::EmptyItem);
    }

    let mut staged_entries = Vec::with_capacity(report.written.len());
    for written in &report.written {
        // A path the reader wrote is already under the strict rule; it is held to it again rather
        // than trusted, and a path that fails is refused with its name.
        let rel_path =
            StrictRelPath::new(written.clone()).map_err(|error| ContentError::InvalidPath {
                path: written.clone(),
                reason: error.reason().to_string(),
            })?;
        let staged_path = staging_dir.join(written);
        let (sha256, size) = hash_file_counting(&staged_path)?;
        staged_entries.push(StagedEntry {
            rel_path,
            staged_path,
            size,
            sha256,
        });
    }
    Ok((staged_entries, report.skipped, guard, staging_dir))
}

/// Every file entry in a zip, 7z, tar, tar.gz or RAR archive, without extracting anything.
///
/// This is the same reader `add_archive` uses, so recognition sees exactly the files an import
/// would store, with the same path and symlink rules. The paths are relative, forward-slash and
/// validated. An unsupported or unreadable archive is an error, never an empty list.
pub fn list_archive_entries(archive_path: &Path) -> Result<Vec<String>, ContentError> {
    if !archive_path.exists() {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("archive not found: {}", archive_path.display()),
        )));
    }
    let archive = open_archive(archive_path)?;
    // The decision an import makes, made once here: names held to the strict rule, the set to one
    // case-insensitive disk, and a link refused. Nothing is dropped quietly.
    archive
        .left_out_names(NamePolicy::Refuse)
        .map_err(archive_error)?;
    let mut paths = Vec::new();
    for entry in archive.file_entries() {
        match classify_entry_name(&entry.name) {
            EntryName::Writable(strict) => paths.push(strict.as_str().to_string()),
            EntryName::Escapes { reason } => {
                return Err(ContentError::InvalidPath {
                    path: entry.name.clone(),
                    reason,
                })
            }
            // `left_out_names` above refused these already; a name that became unwritable between
            // the two decisions is refused here rather than dropped.
            EntryName::Unwritable { reason } => {
                return Err(ContentError::InvalidPath {
                    path: entry.name.clone(),
                    reason,
                })
            }
        }
    }
    if paths.is_empty() {
        return Err(ContentError::EmptyItem);
    }
    Ok(paths)
}

/// Whether the store already holds an item whose SHA-256 is `sha256`: an archive imported as that
/// archive, or a file stored whole. A damaged record is skipped; it cannot be the copy asked about.
pub fn contains_hash(ctx: &Ctx, sha256: &str) -> Result<bool, ContentError> {
    if !is_sha256_hex(sha256) {
        return Err(ContentError::InvalidHash(sha256.to_string()));
    }
    for entry in list_items(ctx)? {
        let Some(item) = entry.readable() else {
            continue;
        };
        for source in &item.sources {
            if let ContentSource::Archive {
                sha256: archived, ..
            } = source
            {
                if archived.eq_ignore_ascii_case(sha256) {
                    return Ok(true);
                }
            }
        }
        if item.files.len() == 1 && item.files[0].sha256.eq_ignore_ascii_case(sha256) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Add a zip, 7z or RAR archive as a content item. The format is chosen by the file's first bytes,
/// never its extension. Zip and 7z are read in process; RAR is read through Windows' own
/// `tar.exe` (`rar.rs`), because the only mature RAR library is not GPL-compatible. All three go
/// through the same path, size and free-space rules and leave nothing behind when refused.
///
/// An entry whose name a disk cannot hold refuses the whole import, naming every one. A
/// user-facing caller that wants to offer the person the files that *can* be written asks with
/// [`add_archive_with_names`] instead (MASTER_SPEC §32.3).
pub fn add_archive(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
) -> Result<AddOutcome, ContentError> {
    add_archive_with_names(ctx, archive_path, name, NamePolicy::Refuse)
}

/// [`add_archive`] with the caller's answer about entries whose names a disk cannot hold.
///
/// [`NamePolicy::Refuse`] is the default everywhere, including every internal caller (a backup
/// import, a conversion, a catalog install): nothing is ever skipped without the person asking.
/// [`NamePolicy::WithoutThem`] is what a user-facing import offers once it has shown the refusal,
/// and the entries it leaves out come back in [`AddOutcome::skipped`].
pub fn add_archive_with_names(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
    names: NamePolicy,
) -> Result<AddOutcome, ContentError> {
    add_archive_with_source_and_names(ctx, archive_path, name, names, |sha256, path| {
        ContentSource::Archive {
            path: path.to_string(),
            sha256: sha256.to_string(),
            added_at_unix_ms: now_unix_ms(),
        }
    })
}

/// [`add_archive`] with the source record built by `source` from the archive's SHA-256 and its
/// display path. The catalog install records its own provenance this way, not a path to a
/// temporary download.
pub fn add_archive_with_source(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
    source: impl FnOnce(&str, &str) -> ContentSource,
) -> Result<AddOutcome, ContentError> {
    add_archive_with_source_and_names(ctx, archive_path, name, NamePolicy::Refuse, source)
}

/// [`add_archive_with_source`] with the caller's answer about entries whose names a disk cannot
/// hold; see [`add_archive_with_names`].
fn add_archive_with_source_and_names(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
    names: NamePolicy,
    source: impl FnOnce(&str, &str) -> ContentSource,
) -> Result<AddOutcome, ContentError> {
    add_archive_replacing(ctx, archive_path, name, names, &HashSet::new(), source)
}

/// [`add_archive_with_source`] that replaces the stored objects whose SHA-256 is in `replace`,
/// instead of trusting them. A caller passes the hashes a verify found damaged, so the re-import
/// repairs exactly those objects, protected or not. The bytes that replace them are the archive's
/// own, extracted and hashed as usual.
pub fn add_archive_replacing(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
    names: NamePolicy,
    replace: &HashSet<String>,
    source: impl FnOnce(&str, &str) -> ContentSource,
) -> Result<AddOutcome, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-add")?;
    add_archive_replacing_locked(ctx, archive_path, name, names, replace, source)
}

/// [`add_archive_replacing`] for a caller that already holds the content-store
/// lock. An async caller takes it with `acquire_async` before calling this.
pub(crate) fn add_archive_replacing_locked(
    ctx: &Ctx,
    archive_path: &Path,
    name: Option<&str>,
    names: NamePolicy,
    replace: &HashSet<String>,
    source: impl FnOnce(&str, &str) -> ContentSource,
) -> Result<AddOutcome, ContentError> {
    if !archive_path.exists() {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("archive not found: {}", archive_path.display()),
        )));
    }

    clean_staging_dir(&ctx.paths);

    let archive_sha256 = hash_file_path(archive_path)?;
    let archive_display_path = archive_path.to_string_lossy().to_string();

    // A RAR is read out of process, and its backend lists, extracts and checks through the same
    // reader (`rar.rs`), so the whole import is handed to it. Every other format is read here, and
    // one this build cannot read is refused by the reader rather than by a magic-number table here.
    let (staged_entries, skipped, mut guard, staging_dir) = if rar::is_rar(archive_path)? {
        let (entries, guard, staging_dir) = rar::stage_rar(ctx, archive_path)?;
        (entries, Vec::new(), guard, staging_dir)
    } else {
        let mut archive = open_archive(archive_path)?;
        stage_archive(ctx, &mut archive, names)?
    };

    // 5. Store objects, record counts, compute item ID
    let item_name = name
        .map(|s| s.to_string())
        .or_else(|| {
            archive_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "content".into());

    let source = source(&archive_sha256, &archive_display_path);

    let outcome = finish_adding(ctx, &item_name, staged_entries, source, replace, skipped)?;
    guard.active = false;
    let _ = std::fs::remove_dir_all(&staging_dir);
    Ok(outcome)
}

pub fn add_folder(
    ctx: &Ctx,
    folder_path: &Path,
    name: Option<&str>,
) -> Result<AddOutcome, ContentError> {
    let source = ContentSource::Folder {
        path: folder_path.to_string_lossy().to_string(),
        added_at_unix_ms: now_unix_ms(),
    };
    add_folder_as(ctx, folder_path, name, source)
}

/// Every regular file under `folder_path`, with its validated relative path. Symlinks and reparse
/// points are refused, as are names no store item may hold. Reads the folder and nothing else.
pub(crate) fn collect_folder_files(
    folder_path: &Path,
) -> Result<Vec<(RelPath, PathBuf)>, ContentError> {
    if !folder_path.exists() {
        return Err(ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("folder not found: {}", folder_path.display()),
        )));
    }
    if !folder_path.is_dir() {
        return Err(ContentError::Other(format!(
            "path is not a directory: {}",
            folder_path.display()
        )));
    }

    fn walk_dir(
        root: &Path,
        rel: &Path,
        collected: &mut Vec<(RelPath, PathBuf)>,
    ) -> Result<(), ContentError> {
        let cur = if rel.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        for entry in std::fs::read_dir(&cur)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let entry_rel = rel.join(&file_name);
            let abs_path = entry.path();
            let meta = std::fs::symlink_metadata(&abs_path)?;

            let rel_str = entry_rel.to_string_lossy().replace('\\', "/");

            if is_reparse_point_or_symlink(&abs_path) {
                return Err(ContentError::InvalidPath {
                    path: rel_str,
                    reason: "a link that points elsewhere is not allowed in a stored folder".into(),
                });
            }

            if meta.is_dir() {
                walk_dir(root, &entry_rel, collected)?;
            } else if meta.is_file() {
                let rel_path = validate_entry_path(&rel_str)?;
                collected.push((rel_path.into_rel_path(), abs_path));
            }
        }
        Ok(())
    }

    let mut collected = Vec::new();
    walk_dir(folder_path, Path::new(""), &mut collected)?;
    Ok(collected)
}

/// [`add_folder`] with the source the caller names, so an importer can record where a folder came
/// from as well as the folder itself.
pub(crate) fn add_folder_as(
    ctx: &Ctx,
    folder_path: &Path,
    name: Option<&str>,
    source: ContentSource,
) -> Result<AddOutcome, ContentError> {
    let collected = collect_folder_files(folder_path)?;
    let item_name = name
        .map(|s| s.to_string())
        .or_else(|| {
            folder_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "content".into());
    add_collected(ctx, &item_name, collected, source)
}

/// Add one file as an item, without unpacking it. A `.jar` or `.zip` is stored as that one file,
/// never as its entries. `rel_path` is the path the item holds it under (the file's own name when
/// `None`), and `name` is the item's display name (the file's stem when `None`). Identical bytes
/// added again share the stored object.
pub fn add_file(
    ctx: &Ctx,
    file_path: &Path,
    name: Option<&str>,
    rel_path: Option<&str>,
) -> Result<AddOutcome, ContentError> {
    let meta = std::fs::symlink_metadata(file_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ContentError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("file not found: {}", file_path.display()),
            ))
        } else {
            ContentError::Io(e)
        }
    })?;
    if is_reparse_point_or_symlink(file_path) {
        return Err(ContentError::InvalidPath {
            path: file_path.display().to_string(),
            reason: "a link that points elsewhere is not a file to store".into(),
        });
    }
    if !meta.is_file() {
        return Err(ContentError::Other(format!(
            "path is not a file: {}",
            file_path.display()
        )));
    }

    let default_rel = file_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or_else(|| ContentError::Other(format!("file has no name: {}", file_path.display())))?;
    let rel = validate_entry_path(rel_path.unwrap_or(&default_rel))?;

    let item_name = name
        .map(|s| s.to_string())
        .or_else(|| {
            file_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "content".into());
    let source = ContentSource::File {
        path: file_path.to_string_lossy().to_string(),
        added_at_unix_ms: now_unix_ms(),
    };

    add_collected(
        ctx,
        &item_name,
        vec![(rel.into_rel_path(), file_path.to_path_buf())],
        source,
    )
}

/// Store an explicit list of files as one item: each file is copied into staging, hashed, and kept
/// as a protected object. Identical bytes are stored once. The paths must not clash, as a folder's
/// would not.
pub fn add_collected(
    ctx: &Ctx,
    item_name: &str,
    collected: Vec<(RelPath, PathBuf)>,
    source: ContentSource,
) -> Result<AddOutcome, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-add")?;

    clean_staging_dir(&ctx.paths);

    let validated_paths: Vec<StrictRelPath> = collected
        .iter()
        .map(|(p, _)| strict_before_write(p))
        .collect::<Result<_, _>>()?;
    validate_path_set(&validated_paths)?;

    // Untrusted sizes: check free disk space
    let mut declared_total_size: u64 = 0;
    for (_, abs) in &collected {
        declared_total_size = declared_total_size.saturating_add(std::fs::metadata(abs)?.len());
    }
    let content_root = ctx.paths.content_root();
    std::fs::create_dir_all(&content_root)?;
    ensure_room_for_declared(&content_root, declared_total_size)?;

    // Staging directory setup
    let staging_root = ctx.paths.content_staging_dir();
    std::fs::create_dir_all(&staging_root)?;
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let staging_dir = staging_root.join(&unique);
    std::fs::create_dir_all(&staging_dir)?;
    let mut guard = StagingGuard::new(&staging_dir);

    // Copy each file to staging while hashing, several files at once. Results come back in the
    // order of `collected`, so the manifest is the same as a one-at-a-time copy would give.
    let staged_entries = parallel_indexed(
        collected.len(),
        || vec![0u8; HASH_BUFFER_SIZE],
        |buf: &mut Vec<u8>, idx| -> Result<StagedEntry, ContentError> {
            let abs_path = &collected[idx].1;
            let staged_path = staging_dir.join(format!("obj_{idx}"));
            let (size, sha256) = copy_and_hash(abs_path, &staged_path, buf)?;
            Ok(StagedEntry {
                rel_path: validated_paths[idx].clone(),
                staged_path,
                size,
                sha256,
            })
        },
    )?;

    let outcome = finish_adding(
        ctx,
        item_name,
        staged_entries,
        source,
        &HashSet::new(),
        Vec::new(),
    )?;
    guard.active = false;
    let _ = std::fs::remove_dir_all(&staging_dir);
    Ok(outcome)
}

/// What `place_object` did with one staged file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    /// The bytes became a new object (or replaced an untrustworthy one).
    New,
    /// A trustworthy object with these bytes was already there.
    Present,
}

/// Put one staged file's bytes into the objects folder, or find that a trustworthy object already
/// holds them. The staged copy is consumed either way. With `replace`, an object already there is
/// replaced without the check: a verify has named it damaged, and a protected object of the right
/// size would otherwise be trusted unhashed.
fn place_object(
    paths: &AppPaths,
    entry: &StagedEntry,
    replace: bool,
) -> Result<Placed, ContentError> {
    let obj_path = paths.content_object_path(&entry.sha256);
    if let Some(parent) = obj_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if obj_path.exists() && !replace && object_is_trustworthy(&obj_path, entry.size, &entry.sha256)?
    {
        let _ = std::fs::remove_file(&entry.staged_path);
        if protect::protection(&obj_path)? != Protection::Protected {
            protect::protect(&obj_path)?;
        }
        Ok(Placed::Present)
    } else if obj_path.exists() {
        // The object no longer holds what its name says (changed while unprotected, or
        // truncated): the staged bytes are the real content, so they replace it. Protecting
        // the old bytes as they are would turn a corruption into the recorded truth.
        protect::unprotect(&obj_path)?;
        std::fs::remove_file(&obj_path)?;
        std::fs::rename(&entry.staged_path, &obj_path)?;
        protect::protect_by_handle(&obj_path)?;
        Ok(Placed::New)
    } else {
        std::fs::rename(&entry.staged_path, &obj_path)?;
        protect::protect_by_handle(&obj_path)?;
        Ok(Placed::New)
    }
}

/// Put the bytes of `source` into the one object store, returning the object's SHA-256 and size.
///
/// This is how everything that is not a content item's own file enters `content/objects`: the
/// snapshot walk and a backup import both come through here. It stages the bytes, syncs them, and
/// commits them with the same [`place_object`] an added item uses, so one rule decides whether an
/// object already there can be trusted and how a new one is protected.
pub(crate) fn place_object_from_file(
    paths: &AppPaths,
    source: &Path,
) -> Result<(String, u64), ContentError> {
    let staging_dir = new_object_staging_dir(paths)?;
    let mut guard = StagingGuard::new(&staging_dir);
    let staged_path = staging_dir.join("object");
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    let (size, sha256) = copy_and_hash(source, &staged_path, &mut buffer)?;
    sync_staged_object(&staged_path)?;
    place_staged(paths, staged_path, size, sha256.clone())?;
    guard.active = false;
    let _ = std::fs::remove_dir_all(&staging_dir);
    Ok((sha256, size))
}

/// [`place_object_from_file`] for bytes already in memory, such as a captured state file.
pub(crate) fn place_object_from_bytes(
    paths: &AppPaths,
    bytes: &[u8],
) -> Result<(String, u64), ContentError> {
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let size = bytes.len() as u64;
    let obj_path = paths.content_object_path(&sha256);
    if let Some(parent) = obj_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if obj_path.exists() && object_is_trustworthy(&obj_path, size, &sha256)? {
        if protect::protection(&obj_path)? != Protection::Protected {
            protect::protect(&obj_path)?;
        }
        return Ok((sha256, size));
    }

    let staging_dir = new_object_staging_dir(paths)?;
    let mut guard = StagingGuard::new(&staging_dir);
    let staged_path = staging_dir.join("object");
    std::fs::write(&staged_path, bytes)?;
    sync_staged_object(&staged_path)?;
    place_staged(paths, staged_path, size, sha256.clone())?;
    guard.active = false;
    let _ = std::fs::remove_dir_all(&staging_dir);
    Ok((sha256, size))
}

/// A fresh folder under the store's staging root for one object on its way in.
fn new_object_staging_dir(paths: &AppPaths) -> Result<PathBuf, ContentError> {
    let staging_root = paths.content_staging_dir();
    std::fs::create_dir_all(&staging_root)?;
    let unique = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple());
    let staging_dir = staging_root.join(unique);
    std::fs::create_dir_all(&staging_dir)?;
    Ok(staging_dir)
}

/// Make staged bytes durable before the rename that gives them their name: a crash between the
/// write and the rename must not leave an object that reads back as the content it is not.
///
/// The handle asks for write access even though nothing more is written: Windows refuses to flush a
/// handle it opened read-only, so opening the staged file to read would fail the sync itself.
fn sync_staged_object(path: &Path) -> Result<(), ContentError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

/// Hand one staged file to [`place_object`].
fn place_staged(
    paths: &AppPaths,
    staged_path: PathBuf,
    size: u64,
    sha256: String,
) -> Result<Placed, ContentError> {
    let entry = StagedEntry {
        rel_path: validate_entry_path("object")?,
        staged_path,
        size,
        sha256,
    };
    place_object(paths, &entry, false)
}

/// Run `work` for each index in `0..count` on a bounded pool of scoped threads (at most 8, and no
/// more than the machine's parallelism). Each thread has its own state from `init`. The results
/// come back in index order. The first error stops the threads from taking new work and is
/// returned; work already running finishes.
fn parallel_indexed<S, T, E, I, W>(count: usize, init: I, work: W) -> Result<Vec<T>, E>
where
    T: Send,
    E: Send,
    I: Fn() -> S + Sync,
    W: Fn(&mut S, usize) -> Result<T, E> + Sync,
{
    let pool = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
        .min(count.max(1));
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let slots: Mutex<Vec<Option<T>>> = Mutex::new((0..count).map(|_| None).collect());
    let first_error: Mutex<Option<E>> = Mutex::new(None);

    std::thread::scope(|s| {
        for _ in 0..pool {
            s.spawn(|| {
                let mut state = init();
                loop {
                    if failed.load(Ordering::Acquire) {
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= count {
                        break;
                    }
                    match work(&mut state, idx) {
                        Ok(value) => slots.lock().unwrap()[idx] = Some(value),
                        Err(e) => {
                            failed.store(true, Ordering::Release);
                            let mut slot = first_error.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                            break;
                        }
                    }
                }
            });
        }
    });

    if let Some(e) = first_error.into_inner().unwrap() {
        return Err(e);
    }
    Ok(slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|slot| slot.expect("every index ran when no error was recorded"))
        .collect())
}

fn finish_adding(
    ctx: &Ctx,
    item_name: &str,
    staged_entries: Vec<StagedEntry>,
    source: ContentSource,
    replace: &HashSet<String>,
    skipped: Vec<SkippedEntry>,
) -> Result<AddOutcome, ContentError> {
    // Entries with the same bytes share one object. Each group runs on one thread, in order: its
    // first entry places the object and the others then find it present. So two threads never
    // race for the same object path, and the counts are those of a one-at-a-time pass.
    let mut group_of: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, entry) in staged_entries.iter().enumerate() {
        match group_of.get(entry.sha256.as_str()) {
            Some(&g) => groups[g].push(i),
            None => {
                group_of.insert(entry.sha256.as_str(), groups.len());
                groups.push(vec![i]);
            }
        }
    }

    let outcomes = parallel_indexed(
        groups.len(),
        || (),
        |_, g| -> Result<Vec<Placed>, ContentError> {
            groups[g]
                .iter()
                .map(|&i| {
                    let entry = &staged_entries[i];
                    place_object(&ctx.paths, entry, replace.contains(&entry.sha256))
                })
                .collect()
        },
    )?;

    let mut objects_new = 0usize;
    let mut objects_restored = 0usize;
    let mut objects_present = 0usize;
    for placed in outcomes.iter().flatten() {
        match placed {
            Placed::New => objects_new += 1,
            Placed::Present => objects_present += 1,
        }
    }

    let mut files = Vec::with_capacity(staged_entries.len());
    let mut total_size = 0u64;
    for entry in staged_entries {
        total_size += entry.size;
        files.push(ContentFile {
            path: entry.rel_path.into_rel_path(),
            size: entry.size,
            sha256: entry.sha256,
        });
    }

    let (item, existed) = commit_manifest(ctx, item_name, files, total_size, source)?;
    if existed {
        // If objects were created on disk for an existing manifest, they were restored
        if objects_new > 0 {
            objects_restored = objects_new;
            objects_new = 0;
        }
        Ok(AddOutcome::Existing {
            item,
            objects_new,
            objects_restored,
            objects_present,
            skipped,
        })
    } else {
        Ok(AddOutcome::Added {
            item,
            objects_new,
            skipped,
        })
    }
}

/// Write the manifest for `files`, or append `source` to the manifest an identical item already
/// has. Returns the item and whether it already existed.
fn commit_manifest(
    ctx: &Ctx,
    item_name: &str,
    mut files: Vec<ContentFile>,
    total_size: u64,
    source: ContentSource,
) -> Result<(ContentItem, bool), ContentError> {
    files.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    let item_id = compute_canonical_item_id(&files);
    let manifest_path = ctx.paths.content_item_path(&item_id);

    if manifest_path.exists() {
        let content = std::fs::read_to_string(&manifest_path)?;
        let mut existing_item: ContentItem = serde_json::from_str(&content)?;

        // Append source if it is new
        let is_new_source = match &source {
            ContentSource::Archive { path, sha256, .. } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::Archive {
                        path: p,
                        sha256: h,
                        ..
                    } if p == path && h == sha256
                )
            }),
            ContentSource::Folder { path, .. } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::Folder { path: p, .. } if p == path
                )
            }),
            ContentSource::FomodInstall {
                from_item, choices, ..
            } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::FomodInstall {
                        from_item: f,
                        choices: c,
                        ..
                    } if f == from_item && c == choices
                )
            }),
            ContentSource::Thunderstore {
                from_item,
                package,
                version,
                ..
            } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::Thunderstore {
                        from_item: f,
                        package: p,
                        version: v,
                        ..
                    } if f == from_item && p == package && v == version
                )
            }),
            ContentSource::Mo2Import {
                setup, mo2_folder, ..
            } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::Mo2Import {
                        setup: t,
                        mo2_folder: f,
                        ..
                    } if t == setup && f == mo2_folder
                )
            }),
            ContentSource::VortexImport {
                store,
                game,
                mod_id,
                ..
            } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::VortexImport {
                        store: t,
                        game: g,
                        mod_id: m,
                        ..
                    } if t == store && g == game && m == mod_id
                )
            }),
            ContentSource::Catalog {
                item_id,
                release,
                asset,
                sha256,
                ..
            } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::Catalog {
                        item_id: i,
                        release: r,
                        asset: a,
                        sha256: h,
                        ..
                    } if i == item_id && r == release && a == asset && h == sha256
                )
            }),
            ContentSource::File { path, .. } => !existing_item.sources.iter().any(|s| {
                matches!(
                    s,
                    ContentSource::File { path: p, .. } if p == path
                )
            }),
        };

        if is_new_source {
            existing_item.sources.push(source);
            write_manifest_atomic(&ctx.paths, &existing_item)?;
        }

        Ok((existing_item, true))
    } else {
        let item = ContentItem {
            item_id,
            name: item_name.to_string(),
            files,
            total_size,
            sources: vec![source],
            added_at_unix_ms: now_unix_ms(),
            description: None,
            page_url: None,
            version: None,
            game_id: None,
            catalog_id: None,
            provider: None,
            provider_id: None,
            provider_origin: None,
            icon_sha256: None,
        };
        write_manifest_atomic(&ctx.paths, &item)?;
        Ok((item, false))
    }
}

/// Replace an item's display name, leaving its identity, files and metadata alone. Used when a
/// source's own title is learned after the file was stored under its file name.
pub fn set_item_name(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    name: &str,
) -> Result<ContentItem, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-name")?;
    let mut item = get_item(ctx, item_id_or_prefix)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(ContentError::Other("an item name cannot be empty".into()));
    }
    item.name = name.to_string();
    write_manifest_atomic(&ctx.paths, &item)?;
    Ok(item)
}

/// Replace an item's optional metadata. The item's identity and files do not change, and a field
/// passed as `None` is removed rather than kept. Returns the item as written.
pub fn set_item_metadata(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    metadata: ItemMetadata,
) -> Result<ContentItem, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-metadata")?;
    set_item_metadata_locked(ctx, item_id_or_prefix, metadata)
}

/// Record the game an item was added for, unless it already names one. An item added from a
/// game's Library belongs to that game; a game recorded earlier (by a catalog install or a
/// download) is never replaced. Returns the item as stored.
pub fn record_game_if_missing(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    game_id: &str,
) -> Result<ContentItem, ContentError> {
    let game_id = game_id.trim();
    if game_id.is_empty() {
        return get_item(ctx, item_id_or_prefix);
    }
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-metadata")?;
    let mut item = get_item(ctx, item_id_or_prefix)?;
    if item.game_id.is_none() {
        item.game_id = Some(game_id.to_string());
        write_manifest_atomic(&ctx.paths, &item)?;
    }
    Ok(item)
}

/// [`set_item_metadata`] for a caller that already holds the content-store lock. Taking the lock
/// again would deadlock, so the store's own icon path uses this to record the object and the item
/// together.
pub(crate) fn set_item_metadata_locked(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    metadata: ItemMetadata,
) -> Result<ContentItem, ContentError> {
    let mut item = get_item(ctx, item_id_or_prefix)?;
    item.description = metadata.description;
    item.page_url = metadata.page_url;
    item.version = metadata.version;
    item.game_id = metadata.game_id;
    item.catalog_id = metadata.catalog_id;
    item.provider = metadata.provider;
    item.provider_id = metadata.provider_id;
    item.provider_origin = metadata.provider_origin;
    item.icon_sha256 = metadata.icon_sha256;
    write_manifest_atomic(&ctx.paths, &item)?;
    Ok(item)
}

/// Whether `text` is a lowercase SHA-256 hex digest, the only form an object path accepts.
fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Store raw bytes as a protected content object and return the object's SHA-256. Used for bytes
/// that are not an item file, such as a saved icon. The bytes are never unpacked, and identical
/// bytes share one object.
///
/// The content-store lock is held while the object lands, so a concurrent sweep cannot delete it
/// before the caller records it in an item. A caller that records the hash in the same operation
/// takes the lock once and uses [`store_object_bytes_locked`].
pub fn store_object_bytes(ctx: &Ctx, bytes: &[u8]) -> Result<String, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-object")?;
    store_object_bytes_locked(ctx, bytes)
}

/// [`store_object_bytes`] for a caller that already holds the content-store lock.
pub(crate) fn store_object_bytes_locked(ctx: &Ctx, bytes: &[u8]) -> Result<String, ContentError> {
    place_object_from_bytes(&ctx.paths, bytes).map(|(sha256, _)| sha256)
}

/// Read a stored object's bytes, refusing a hash that is not an object name and an object that no
/// longer holds what its name says.
pub fn read_object_bytes(ctx: &Ctx, sha256: &str) -> Result<Vec<u8>, ContentError> {
    read_object_bytes_limited(ctx, sha256, u64::MAX)
}

/// [`read_object_bytes`] with a size cap, checked before any bytes are read. Used for content that
/// a caller only expects to be small, such as a saved icon.
pub fn read_object_bytes_limited(
    ctx: &Ctx,
    sha256: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ContentError> {
    if !is_sha256_hex(sha256) {
        return Err(ContentError::InvalidHash(sha256.to_string()));
    }
    let path = ctx.paths.content_object_path(sha256);
    if !path.exists() {
        return Err(ContentError::MissingObject(sha256.to_string()));
    }
    let size = std::fs::metadata(&path)?.len();
    if size > max_bytes {
        return Err(ContentError::Other(format!(
            "the stored object named {sha256} is {size} bytes; the limit is {max_bytes}"
        )));
    }
    if !object_is_trustworthy(&path, size, sha256)? {
        return Err(ContentError::DamagedObject(format!(
            "the object named {sha256} no longer holds those bytes"
        )));
    }
    Ok(std::fs::read(&path)?)
}

/// Create an item from some of another item's files, under new paths, without extracting
/// anything again: the new manifest points at the objects `from_item` already holds. A FOMOD
/// install is the first user. `files` pairs each new path with the sha256 of an object `from_item`
/// contains.
pub fn derive_item(
    ctx: &Ctx,
    from_item: &str,
    files: Vec<(RelPath, String)>,
    name: &str,
    source: ContentSource,
) -> Result<AddOutcome, ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-derive")?;

    let from = get_item(ctx, from_item)?;
    let mut known: HashMap<&str, u64> = HashMap::with_capacity(from.files.len());
    for f in &from.files {
        known.insert(f.sha256.as_str(), f.size);
    }

    let mut content_files = Vec::with_capacity(files.len());
    let mut paths = Vec::with_capacity(files.len());
    let mut total_size = 0u64;
    let mut present = HashSet::new();
    for (path, sha256) in files {
        let path = validate_entry_path(path.as_str())?;
        let Some(&size) = known.get(sha256.as_str()) else {
            return Err(ContentError::Other(format!(
                "'{path}' refers to an object that item {} does not contain",
                from.item_id
            )));
        };
        let obj_path = ctx.paths.content_object_path(&sha256);
        match std::fs::metadata(&obj_path) {
            Ok(m) if m.len() == size => {}
            _ => {
                return Err(ContentError::Other(format!(
                    "the stored object for '{path}' is missing or the wrong size; verify item {}",
                    from.item_id
                )));
            }
        }
        total_size += size;
        present.insert(sha256.clone());
        paths.push(path.clone());
        content_files.push(ContentFile {
            path: path.into_rel_path(),
            size,
            sha256,
        });
    }
    validate_path_set(&paths)?;

    let (item, existed) = commit_manifest(ctx, name, content_files, total_size, source)?;
    if existed {
        Ok(AddOutcome::Existing {
            item,
            objects_new: 0,
            objects_restored: 0,
            objects_present: present.len(),
            skipped: Vec::new(),
        })
    } else {
        Ok(AddOutcome::Added {
            item,
            objects_new: 0,
            skipped: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Prefix Resolution & Queries
// ---------------------------------------------------------------------------

/// Whether `text` can name an item: non-empty lowercase hex, which can never become a path.
fn is_item_id(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn resolve_item_id(ctx: &Ctx, prefix: &str) -> Result<String, ContentError> {
    // An empty prefix would match every item, so `remove ""` could delete the only one; ids are
    // lowercase hex, so anything else cannot name an item (and can never become a path).
    if !is_item_id(prefix) {
        return Err(ContentError::NotFound(prefix.to_string()));
    }
    let items_dir = ctx.paths.content_items_dir();
    if !items_dir.exists() {
        return Err(ContentError::NotFound(prefix.to_string()));
    }
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(items_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem.starts_with(prefix) {
                    matches.push(stem.to_string());
                }
            }
        }
    }
    match matches.len() {
        0 => Err(ContentError::NotFound(prefix.to_string())),
        1 => Ok(matches.remove(0)),
        _ => {
            matches.sort();
            Err(ContentError::AmbiguousPrefix {
                prefix: prefix.to_string(),
                matches: matches.join(", "),
            })
        }
    }
}

pub fn get_item(ctx: &Ctx, item_id_or_prefix: &str) -> Result<ContentItem, ContentError> {
    let item_id = resolve_item_id(ctx, item_id_or_prefix)?;
    let path = ctx.paths.content_item_path(&item_id);
    let content = std::fs::read_to_string(&path)?;
    let item: ContentItem = serde_json::from_str(&content)?;
    Ok(item)
}

/// The ids of the items in the store's folder, listed once. [`get_item_indexed`] resolves against
/// them, so a caller that reads many items (a deployment plan) lists the folder once, not per item.
/// A folder that does not exist has no items.
pub fn item_ids(ctx: &Ctx) -> Result<std::collections::BTreeSet<String>, ContentError> {
    let mut ids = std::collections::BTreeSet::new();
    let items_dir = ctx.paths.content_items_dir();
    if !items_dir.exists() {
        return Ok(ids);
    }
    for entry in std::fs::read_dir(items_dir)? {
        let path = entry?.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.insert(stem.to_string());
            }
        }
    }
    Ok(ids)
}

/// [`get_item`], resolving the id against `ids` (from [`item_ids`]) instead of listing the folder
/// again. It gives the same item, or the same error, as `get_item`.
pub fn get_item_indexed(
    ctx: &Ctx,
    ids: &std::collections::BTreeSet<String>,
    item_id_or_prefix: &str,
) -> Result<ContentItem, ContentError> {
    let prefix = item_id_or_prefix;
    if !is_item_id(prefix) {
        return Err(ContentError::NotFound(prefix.to_string()));
    }
    let mut matches: Vec<String> = ids
        .range(prefix.to_string()..)
        .take_while(|id| id.starts_with(prefix))
        .cloned()
        .collect();
    let item_id = match matches.len() {
        0 => return Err(ContentError::NotFound(prefix.to_string())),
        1 => matches.remove(0),
        _ => {
            matches.sort();
            return Err(ContentError::AmbiguousPrefix {
                prefix: prefix.to_string(),
                matches: matches.join(", "),
            });
        }
    };
    let content = std::fs::read_to_string(ctx.paths.content_item_path(&item_id))?;
    let item: ContentItem = serde_json::from_str(&content)?;
    Ok(item)
}

/// One record in the store's items folder: an item that reads, or a record that does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ItemEntry {
    Readable(Box<ContentItem>),
    Damaged(DamagedItem),
}

/// An item record that cannot be read as a content item. It is listed, not dropped: the user can
/// see it, read why it cannot be used, and remove it (`remove_item` does not parse the target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamagedItem {
    /// The record's file name without `.json`.
    pub item_id: String,
    pub manifest_path: String,
    /// Why the record cannot be used, naming the file and the cause.
    pub error: String,
}

impl ItemEntry {
    /// The item's id: from the record when it reads, otherwise from the file name.
    pub fn item_id(&self) -> &str {
        match self {
            ItemEntry::Readable(item) => &item.item_id,
            ItemEntry::Damaged(damaged) => &damaged.item_id,
        }
    }

    /// The item, when the record reads.
    pub fn readable(&self) -> Option<&ContentItem> {
        match self {
            ItemEntry::Readable(item) => Some(item),
            ItemEntry::Damaged(_) => None,
        }
    }

    /// The item, or the damaged record that stands in its place.
    pub fn into_item(self) -> Result<ContentItem, DamagedItem> {
        match self {
            ItemEntry::Readable(item) => Ok(*item),
            ItemEntry::Damaged(damaged) => Err(damaged),
        }
    }
}

/// Every record in the store, sorted by id. A record that cannot be read is returned as
/// [`ItemEntry::Damaged`] with the reason, never skipped: a store that looks emptier than it is
/// would hide the damage from the person who has to fix it.
pub fn list_items(ctx: &Ctx) -> Result<Vec<ItemEntry>, ContentError> {
    let items_dir = ctx.paths.content_items_dir();
    if !items_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(items_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let item_id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read = std::fs::read_to_string(&path)
                .map_err(|e| format!("{} cannot be read: {e}", path.display()))
                .and_then(|text| {
                    serde_json::from_str::<ContentItem>(&text)
                        .map_err(|e| format!("{} is not a valid item record: {e}", path.display()))
                });
            entries.push(match read {
                Ok(item) => ItemEntry::Readable(Box::new(item)),
                Err(error) => ItemEntry::Damaged(DamagedItem {
                    // Removal names items by hex id; a file named otherwise is not one of ours.
                    error: if is_item_id(&item_id) {
                        error
                    } else {
                        format!("{error} Its name is not an item id, so Agora cannot remove it: delete the file yourself.")
                    },
                    item_id,
                    manifest_path: path.display().to_string(),
                }),
            });
        }
    }
    entries.sort_by(|a, b| a.item_id().cmp(b.item_id()));
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

pub fn verify_item(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    depth: VerifyDepth,
) -> Result<ContentVerification, ContentError> {
    let item_id = resolve_item_id(ctx, item_id_or_prefix)?;
    let path = ctx.paths.content_item_path(&item_id);

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ContentVerification {
                item_id: item_id.clone(),
                checked: 0,
                hashed: 0,
                problems: vec![ContentProblem {
                    item_id,
                    path: path.display().to_string(),
                    kind: ProblemKind::CorruptManifest {
                        error: e.to_string(),
                    },
                }],
            });
        }
    };

    let item: ContentItem = match serde_json::from_str(&content) {
        Ok(it) => it,
        Err(e) => {
            return Ok(ContentVerification {
                item_id: item_id.clone(),
                checked: 0,
                hashed: 0,
                problems: vec![ContentProblem {
                    item_id,
                    path: path.display().to_string(),
                    kind: ProblemKind::CorruptManifest {
                        error: e.to_string(),
                    },
                }],
            });
        }
    };

    let mut problems = Vec::new();
    let mut checked = 0usize;
    let mut hashed = 0usize;

    for file in &item.files {
        checked += 1;
        let obj_path = ctx.paths.content_object_path(&file.sha256);
        let meta = match std::fs::metadata(&obj_path) {
            Ok(m) => m,
            Err(_) => {
                problems.push(ContentProblem {
                    item_id: item.item_id.clone(),
                    path: file.path.to_string(),
                    kind: ProblemKind::Missing,
                });
                continue;
            }
        };

        if meta.len() != file.size {
            problems.push(ContentProblem {
                item_id: item.item_id.clone(),
                path: file.path.to_string(),
                kind: ProblemKind::SizeMismatch {
                    expected: file.size,
                    actual: meta.len(),
                },
            });
        }

        match protect::protection(&obj_path) {
            Ok(Protection::Unprotected) => {
                problems.push(ContentProblem {
                    item_id: item.item_id.clone(),
                    path: file.path.to_string(),
                    kind: ProblemKind::Unprotected,
                });
            }
            Ok(Protection::Protected) | Ok(Protection::Unsupported) => {}
            Err(e) => {
                problems.push(ContentProblem {
                    item_id: item.item_id.clone(),
                    path: file.path.to_string(),
                    kind: ProblemKind::CorruptManifest {
                        error: format!("protection check failed: {e}"),
                    },
                });
            }
        }

        if depth == VerifyDepth::Full {
            hashed += 1;
            match hash_file_path(&obj_path) {
                Ok(h) => {
                    if h != file.sha256 {
                        problems.push(ContentProblem {
                            item_id: item.item_id.clone(),
                            path: file.path.to_string(),
                            kind: ProblemKind::HashMismatch {
                                expected: file.sha256.clone(),
                                actual: h,
                            },
                        });
                    }
                }
                Err(e) => {
                    problems.push(ContentProblem {
                        item_id: item.item_id.clone(),
                        path: file.path.to_string(),
                        kind: ProblemKind::CorruptManifest {
                            error: format!("hash read error: {e}"),
                        },
                    });
                }
            }
        }
    }

    Ok(ContentVerification {
        item_id: item.item_id,
        checked,
        hashed,
        problems,
    })
}

pub fn verify_all(
    ctx: &Ctx,
    depth: VerifyDepth,
) -> Result<ContentVerificationReport, ContentError> {
    let items_dir = ctx.paths.content_items_dir();
    if !items_dir.exists() {
        return Ok(ContentVerificationReport {
            checked_items: 0,
            checked_files: 0,
            hashed_files: 0,
            problems: Vec::new(),
        });
    }

    let mut checked_items = 0usize;
    let mut checked_files = 0usize;
    let mut hashed_files = 0usize;
    let mut all_problems = Vec::new();

    for entry in std::fs::read_dir(items_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            checked_items += 1;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let ver = verify_item(ctx, stem, depth)?;
            checked_files += ver.checked;
            hashed_files += ver.hashed;
            all_problems.extend(ver.problems);
        }
    }

    all_problems.sort_by(|a, b| a.item_id.cmp(&b.item_id).then_with(|| a.path.cmp(&b.path)));

    Ok(ContentVerificationReport {
        checked_items,
        checked_files,
        hashed_files,
        problems: all_problems,
    })
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

/// Remove a content item and any objects that no remaining item references.
///
/// Under the lock, every instance manifest is checked first; if any instance
/// references the item, removal refuses naming the instances. If any instance
/// manifest cannot be read or parsed, removal refuses too (fail closed).
/// Then every other content manifest is read and validated; if any cannot be read
/// or parsed, removal refuses and deletes nothing to prevent sweeping objects that
/// an unreadable item still needs.
/// Answers for [`remove_item_with_options`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOptions {
    /// Proceed even when a snapshot manifest cannot be read. The ignored
    /// snapshots are named on stderr. The caller has told the user that those
    /// snapshots will no longer be able to restore the item.
    pub ignore_unreadable_snapshots: bool,
}

pub fn remove_item(ctx: &Ctx, item_id_or_prefix: &str) -> Result<(), ContentError> {
    remove_item_with_options(ctx, item_id_or_prefix, RemoveOptions::default())
}

pub fn remove_item_with_options(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    options: RemoveOptions,
) -> Result<(), ContentError> {
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-remove")?;

    let item_id = resolve_item_id(ctx, item_id_or_prefix)?;
    let target_manifest_path = ctx.paths.content_item_path(&item_id);

    if !target_manifest_path.exists() {
        return Err(ContentError::NotFound(item_id));
    }

    // 0. Check every instance first (Minecraft and generic), through the same join the Library
    //    uses. A generic instance names the item in a Content layer; a Minecraft instance records
    //    the item id, or the object hash its installed file came from. If an instance uses the
    //    item, removal refuses naming the instances. If any manifest cannot be read, removal
    //    refuses too (fail closed), naming the file.
    //
    //    The target's own record is read only to learn its object hashes; a damaged record still
    //    can be removed, but only when no instance could be using objects whose hashes cannot be
    //    read (checked below).
    let parsed_target = std::fs::read_to_string(&target_manifest_path)
        .map_err(|error| error.to_string())
        .and_then(|content| {
            serde_json::from_str::<ContentItem>(&content).map_err(|error| error.to_string())
        });
    let target_hashes: Vec<String> = match &parsed_target {
        Ok(item) => item.files.iter().map(|file| file.sha256.clone()).collect(),
        Err(_) => Vec::new(),
    };
    let locations = ctx.lock_manager.try_instance_locations("content-remove")?;
    let usage = crate::library::instances_using_locked(ctx, &item_id, &target_hashes);

    if let Some(unreadable) = usage.unreadable.first() {
        return Err(ContentError::UnreadableManifest {
            item_id,
            manifest: unreadable.manifest_path.clone(),
            error: unreadable.error.clone(),
        });
    }

    if !usage.instances.is_empty() {
        let mut using: Vec<(String, String)> = usage
            .instances
            .iter()
            .map(|instance| {
                let name = instance.name.trim();
                let label = if name.is_empty() {
                    instance.instance_id.clone()
                } else {
                    name.to_string()
                };
                (label, instance.instance_id.clone())
            })
            .collect();
        using.sort();
        return Err(ContentError::InUse {
            item_id,
            instances: using
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            instance_ids: using.into_iter().map(|(_, id)| id).collect(),
        });
    }

    // A game-instance snapshot names its content-store items in its manifest. Removing an item a
    // snapshot still holds would make that restore point unusable, so it refuses, naming the
    // snapshots and the command that deletes each. An unreadable snapshot manifest refuses too
    // (it might name any item) unless the caller explicitly overrides it.
    let snapshot_usage = crate::game_snapshot::snapshots_using_content_locked(ctx, &item_id)
        .map_err(|error| ContentError::UnreadableManifest {
            item_id: item_id.clone(),
            manifest: "game-instance snapshots".into(),
            error,
        })?;
    if !snapshot_usage.using.is_empty() {
        let mut instance_ids: Vec<String> = snapshot_usage
            .using
            .iter()
            .map(|snapshot| snapshot.instance_id.clone())
            .collect();
        instance_ids.sort();
        instance_ids.dedup();
        let instances = snapshot_usage
            .using
            .iter()
            .map(|snapshot| {
                format!(
                    "snapshot '{}' of instance '{}' ({}); delete it with: {}",
                    snapshot.label.as_deref().unwrap_or(&snapshot.snapshot_id),
                    snapshot.instance_id,
                    snapshot.created_at,
                    snapshot.command
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ContentError::InUse {
            item_id,
            instances,
            instance_ids,
        });
    }
    if !snapshot_usage.unreadable.is_empty() {
        if options.ignore_unreadable_snapshots {
            for snapshot in &snapshot_usage.unreadable {
                eprintln!(
                    "[content] removing {item_id} while ignoring unreadable snapshot '{}' of                      instance '{}' ({}): {}; delete it with: {}",
                    snapshot.snapshot_id,
                    snapshot.instance_id,
                    snapshot.created_at,
                    snapshot.error,
                    snapshot.command
                );
            }
        } else {
            let message = snapshot_usage
                .unreadable
                .iter()
                .map(|snapshot| {
                    format!(
                        "snapshot '{}' of instance '{}' ({}) at {} cannot be read ({}), so it                          might use the item; delete it with: {}",
                        snapshot.snapshot_id,
                        snapshot.instance_id,
                        snapshot.created_at,
                        snapshot.path,
                        snapshot.error,
                        snapshot.command
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ContentError::UnreadableSnapshots {
                item_id,
                message,
                snapshots: snapshot_usage.unreadable,
            });
        }
    }

    // The target's own manifest is not parsed, so a corrupt item can still be removed. Another
    // item record that cannot be read cannot name this item (an item record holds files, not
    // references to other items), so it does not block the removal. It may still hold the hash of
    // an object, though, so while any record is unreadable no object is deleted: they stay until
    // the next removal with every record readable sweeps whatever nothing references.
    //
    // A damaged target has no readable object hashes, so a Minecraft instance that recorded only
    // an object hash cannot be seen as using it. Refuse rather than delete files an instance may
    // still need; removing the record alone would let the next sweep delete them.
    if parsed_target.is_err() && usage.records_objects {
        return Err(ContentError::UnreadableManifest {
            item_id,
            manifest: target_manifest_path.display().to_string(),
            error: "the record cannot be read, so the files it stores cannot be identified; an \
                    instance may still use them"
                .into(),
        });
    }
    drop(locations);
    // 1. Publish the removal: the item's own record goes, so nothing names its objects any more.
    std::fs::remove_file(&target_manifest_path)?;

    // 2. Then let the one collection decide what is unreferenced. It reads every owner record
    //    fresh — items, instance manifests, snapshot manifests here and in both Recently deleted
    //    roots, pending records — frees what nothing names in either object store, and refuses to
    //    free anything at all when a record, a listing or an object's time cannot be read. The
    //    removal itself has succeeded either way; the report says what was not reclaimed and why.
    let report = collect::collect_with_ctx(
        ctx,
        &collect::CollectRequest {
            store: collect::LockRequest::Held,
            locations: collect::LockRequest::Wait,
            grace: None,
            pause: None,
            ignore_unreadable_snapshots: options.ignore_unreadable_snapshots,
        },
    );
    if report.refused() {
        eprintln!(
            "[content] removed {item_id}, but no stored object was reclaimed: {}",
            report.summary()
        );
    }

    // 3. Clean up any staging leftovers
    clean_staging_dir(&ctx.paths);

    Ok(())
}

#[cfg(test)]
mod free_space_tests {
    use super::*;

    #[test]
    fn unknown_free_space_does_not_disable_the_declared_size_limit() {
        let result = declared_size_fits(10, None);
        assert!(
            matches!(result, Err(ContentError::Other(_))),
            "an unknown free-space value must refuse, not skip the check: {result:?}"
        );
    }

    #[test]
    fn an_overflowing_declared_size_is_refused() {
        let result = declared_size_fits(u64::MAX, Some(u64::MAX));
        assert!(
            result.is_err(),
            "declared size plus headroom must not wrap: {result:?}"
        );
    }

    #[test]
    fn a_declared_size_that_fits_passes() {
        assert!(declared_size_fits(10, Some(ONE_GIB + 10)).is_ok());
        assert!(matches!(
            declared_size_fits(10, Some(ONE_GIB + 9)),
            Err(ContentError::InsufficientSpace { .. })
        ));
    }
}
