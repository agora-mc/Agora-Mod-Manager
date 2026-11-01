//! Converting an existing instance between shared links and separate copies.
//!
//! This is the deliberate one-time counterpart to the "Share mod files between instances" setting
//! ([`super::place::SHARE_CONTENT_SETTING`]): it rewrites files an instance already has, so it is
//! only ever run because the user asked for it. It holds the instance lock, refuses a running
//! instance, and keeps every original in a journal until the replacement is in place, so an
//! interruption never loses a file and a second run finishes the job.
//!
//! Every file is matched against the hash its manifest entry recorded. A file that does not match
//! belongs to the user: it is left exactly as it is and reported.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::place::same_file;
use super::{add_file, place_object_at, PlacementMode};
use crate::ctx::Ctx;
use crate::helpers::{atomic_write_manifest, content_subdir, hash_file_sha256, read_manifest};
use crate::lock_manager::LockResource;
use crate::models::{InstalledMod, InstanceManifest};
use crate::paths::sanitize_id;

/// Where an interrupted conversion keeps the file it moved out of the way until the replacement
/// is safely in place.
const JOURNAL_DIR: &str = ".agora/content-convert";
/// Where a file is put when the journal cannot cleanly resolve it, never deleted.
const MOVED_ASIDE_DIR: &str = ".agora/moved-aside";

/// Why one file was left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvertSkipReason {
    /// The manifest lists the file but it is not on disk.
    Missing,
    /// The file's bytes are not the ones the manifest recorded, so it is the user's.
    Changed,
    /// The entry is not a single file (a world is a folder) or the path is not a regular file.
    Unsupported,
    /// An earlier interrupted run left a second copy that could not be matched to the file now on
    /// disk. It was moved into `.agora/moved-aside/` rather than deleted.
    MovedAside,
}

/// One file the conversion did not touch, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvertSkip {
    /// The instance-relative path, using forward slashes (`mods/example.jar`).
    pub path: String,
    pub reason: ConvertSkipReason,
    /// Where a `MovedAside` copy was put, for the user to find it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_to: Option<String>,
}

/// What a conversion did, or would do for a preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionSummary {
    /// Files whose storage changed.
    pub files_converted: usize,
    /// Files already stored the way the requested mode wants.
    pub files_already: usize,
    /// Disk space freed (to shared) or additionally used (to separate).
    pub bytes: u64,
    pub skipped: Vec<ConvertSkip>,
}

impl ConversionSummary {
    fn merge(&mut self, other: ConversionSummary) {
        self.files_converted += other.files_converted;
        self.files_already += other.files_already;
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.skipped.extend(other.skipped);
    }
}

/// One instance a bulk conversion could not finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversionFailure {
    pub instance_id: String,
    pub message: String,
}

/// The result of previewing every instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkPreview {
    pub instances: usize,
    pub summary: ConversionSummary,
    /// Instances whose manifests could not be read. Never silently counted as empty.
    pub warnings: Vec<String>,
}

/// The result of converting every instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkConversion {
    pub instances: usize,
    pub summary: ConversionSummary,
    /// Instances that were refused or failed; the rest still ran.
    pub failures: Vec<ConversionFailure>,
    pub warnings: Vec<String>,
}

/// Preview one instance: which files would change and how much space that frees or uses. Read-only.
pub fn preview_instance(
    ctx: &Ctx,
    instance_id: &str,
    mode: PlacementMode,
) -> Result<ConversionSummary, crate::content_store::ContentError> {
    let (dir, manifest) = load_instance(ctx, instance_id)?;
    Ok(preview_manifest(ctx, &dir, &manifest, mode))
}

/// Convert one instance between shared links and separate copies. Refused while it runs or while
/// another operation holds its lock, and safe to run again after an interruption.
pub fn convert_instance(
    ctx: &Ctx,
    instance_id: &str,
    mode: PlacementMode,
) -> Result<ConversionSummary, crate::content_store::ContentError> {
    let sanitized = sanitize_id(instance_id);
    let dir = ctx
        .paths
        .instance_dir(&sanitized)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    let manifest_path = ctx
        .paths
        .instance_manifest(&sanitized)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    convert_instance_dir(ctx, &sanitized, &dir, &manifest_path, mode)
}

fn load_instance(
    ctx: &Ctx,
    instance_id: &str,
) -> Result<(PathBuf, InstanceManifest), crate::content_store::ContentError> {
    let sanitized = sanitize_id(instance_id);
    let dir = ctx
        .paths
        .instance_dir(&sanitized)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    let manifest_path = ctx
        .paths
        .instance_manifest(&sanitized)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    let manifest = read_manifest(&manifest_path)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    Ok((dir, manifest))
}

/// Preview every instance Agora can read. Unreadable manifests are reported, not treated as empty.
pub fn preview_all(
    ctx: &Ctx,
    mode: PlacementMode,
) -> Result<BulkPreview, crate::content_store::ContentError> {
    let mut report = BulkPreview::default();
    for found in scan_instances(ctx) {
        match found {
            Ok(found) => match read_manifest(&found.manifest_path) {
                Ok(manifest) => {
                    report.instances += 1;
                    report
                        .summary
                        .merge(preview_manifest(ctx, &found.dir, &manifest, mode));
                }
                Err(error) => report.warnings.push(format!(
                    "Instance '{}' could not be read, so it was not previewed: {error}",
                    found.name
                )),
            },
            Err(warning) => report.warnings.push(warning),
        }
    }
    Ok(report)
}

/// Convert every instance Agora can read. One refused (running or locked) instance does not stop
/// the others; each is reported in `failures`.
pub fn convert_all(
    ctx: &Ctx,
    mode: PlacementMode,
) -> Result<BulkConversion, crate::content_store::ContentError> {
    let mut report = BulkConversion::default();
    for found in scan_instances(ctx) {
        match found {
            Ok(found) => {
                report.instances += 1;
                // The lock names the folder that is actually written, never a value from a
                // manifest that could point somewhere else.
                let lock_id = sanitize_id(&found.name);
                match convert_instance_dir(ctx, &lock_id, &found.dir, &found.manifest_path, mode) {
                    Ok(summary) => report.summary.merge(summary),
                    Err(error) => report.failures.push(ConversionFailure {
                        instance_id: found.name,
                        message: error.to_string(),
                    }),
                }
            }
            Err(warning) => report.warnings.push(warning),
        }
    }
    Ok(report)
}

struct FoundInstance {
    /// The folder name, which is the instance id other commands use.
    name: String,
    dir: PathBuf,
    manifest_path: PathBuf,
}

/// Every instance folder with a manifest, sorted by folder name so runs are repeatable. The
/// manifest is read by the caller, under the instance lock where it will be written.
fn scan_instances(ctx: &Ctx) -> Vec<Result<FoundInstance, String>> {
    let root = ctx.paths.instances_root();
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return found,
        Err(error) => return vec![Err(format!("Instances could not be listed: {error}"))],
    };
    let mut dirs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    for dir in dirs {
        let manifest_path = dir.join("instance_manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        found.push(Ok(FoundInstance {
            name,
            dir,
            manifest_path,
        }));
    }
    found
}

fn convert_instance_dir(
    ctx: &Ctx,
    lock_id: &str,
    instance_dir: &Path,
    manifest_path: &Path,
    mode: PlacementMode,
) -> Result<ConversionSummary, crate::content_store::ContentError> {
    // A running game has the files open; replacing them under it is neither safe nor useful.
    if ctx
        .process_session_manager
        .list()
        .iter()
        .any(|session| session.instance_id == lock_id)
    {
        return Err(crate::content_store::ContentError::InstanceBusy(
            lock_id.to_string(),
        ));
    }
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(lock_id.to_string()),
        "content-convert",
    )?;

    // Read the manifest under the lock: anything installed while we waited is in this copy and
    // will be written back, never overwritten by a stale one.
    let mut manifest = read_manifest(manifest_path)
        .map_err(|error| crate::content_store::ContentError::Other(error.to_string()))?;
    // Refuse a manifest we may not write before any file is replaced. A newer Agora owns its
    // shape, and a locked instance belongs to whatever has it open.
    if manifest.is_read_only() {
        return Err(crate::content_store::ContentError::Lock(
            crate::helpers::manifest_read_only_error(),
        ));
    }
    if manifest.is_locked {
        return Err(crate::content_store::ContentError::Lock(
            crate::error::LauncherError::InstanceLocked,
        ));
    }

    let journal_root = instance_dir.join(JOURNAL_DIR);
    let expected = expected_hashes(&manifest);
    let mut summary = ConversionSummary::default();
    // Finish whatever an earlier run left half-done before touching anything new.
    summary
        .skipped
        .extend(recover_journal(instance_dir, &journal_root, &expected)?);

    let mut dirty = false;
    let mut failure = None;
    for entry in content_entries_mut(&mut manifest) {
        match convert_entry(ctx, instance_dir, &journal_root, entry, mode, &mut summary) {
            Ok(changed) => dirty |= changed,
            Err(error) => {
                // A later file's failure must not discard the records of files already converted.
                failure = Some(error);
                break;
            }
        }
    }

    if dirty {
        if let Err(write_error) = atomic_write_manifest(manifest_path, &manifest) {
            return Err(match failure {
                Some(error) => crate::content_store::ContentError::Other(format!(
                    "{error}; the records of the files already converted could not be saved: {write_error}"
                )),
                None => crate::content_store::ContentError::Other(write_error.to_string()),
            });
        }
    }
    prune_empty_dirs(&journal_root, &journal_root);
    let _ = std::fs::remove_dir(&journal_root);
    match failure {
        Some(error) => Err(error),
        None => Ok(summary),
    }
}

/// The recorded hash of every manifest file under its instance-relative name, including the
/// enabled and `.disabled` names, so recovery can tell a complete target from a truncated one.
fn expected_hashes(manifest: &InstanceManifest) -> std::collections::BTreeMap<String, String> {
    let mut expected = std::collections::BTreeMap::new();
    for entry in manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter())
    {
        let Some(hash) = recorded_hash(entry) else {
            continue;
        };
        let folder = content_subdir(&entry.content_type);
        expected.insert(format!("{folder}/{}", entry.filename), hash.clone());
        expected.insert(format!("{folder}/{}.disabled", entry.filename), hash);
    }
    expected
}

fn content_entries_mut(manifest: &mut InstanceManifest) -> Vec<&mut InstalledMod> {
    let mut entries = Vec::new();
    entries.extend(manifest.mods.iter_mut());
    entries.extend(manifest.resourcepacks.iter_mut());
    entries.extend(manifest.shaders.iter_mut());
    entries.extend(manifest.datapacks.iter_mut());
    entries.extend(manifest.worlds.iter_mut());
    entries
}

/// Read an entry, decide whether its file needs changing, and change it. Returns whether the
/// manifest entry gained store fields.
fn convert_entry(
    ctx: &Ctx,
    instance_dir: &Path,
    journal_root: &Path,
    entry: &mut InstalledMod,
    mode: PlacementMode,
    summary: &mut ConversionSummary,
) -> Result<bool, crate::content_store::ContentError> {
    // A world is a folder, never one placed file.
    if entry.content_type == "world" {
        return Ok(false);
    }
    let Some((target, rel)) = resolve_target(instance_dir, entry) else {
        summary.skipped.push(ConvertSkip {
            path: display_path(entry),
            reason: ConvertSkipReason::Missing,
            moved_to: None,
        });
        return Ok(false);
    };
    let metadata = match std::fs::metadata(&target) {
        Ok(metadata) => metadata,
        Err(_) => {
            summary.skipped.push(ConvertSkip {
                path: rel.clone(),
                reason: ConvertSkipReason::Missing,
                moved_to: None,
            });
            return Ok(false);
        }
    };
    if !metadata.is_file() {
        summary.skipped.push(ConvertSkip {
            path: rel,
            reason: ConvertSkipReason::Unsupported,
            moved_to: None,
        });
        return Ok(false);
    }
    let recorded = recorded_hash(entry);
    let Some(recorded) = recorded else {
        summary.skipped.push(ConvertSkip {
            path: rel,
            reason: ConvertSkipReason::Unsupported,
            moved_to: None,
        });
        return Ok(false);
    };
    let actual = hash_file_sha256(&target)?;
    if !actual.eq_ignore_ascii_case(&recorded) {
        summary.skipped.push(ConvertSkip {
            path: rel,
            reason: ConvertSkipReason::Changed,
            moved_to: None,
        });
        return Ok(false);
    }
    let object = ctx.paths.content_object_path(&actual.to_ascii_lowercase());

    match mode {
        PlacementMode::Link => {
            if object.is_file() && same_file(&target, &object) {
                summary.files_already += 1;
                // A file linked by an earlier run whose record was lost still needs its record:
                // otherwise a future run sees "already" and never writes one.
                if entry.content_item_id.is_some() && entry.object_sha256.is_some() {
                    return Ok(false);
                }
                let (item_id, stored_hash) = store_from_file(ctx, &target, entry)?;
                return Ok(update_record(entry, item_id, stored_hash));
            }
            // Store the object first so the replacement has something to point at.
            let stored = if object.is_file() {
                None
            } else {
                Some(store_from_file(ctx, &target, entry)?)
            };
            match replace_with(ctx, instance_dir, journal_root, &target, &actual, mode)? {
                Replacement::Changed => {
                    summary.skipped.push(ConvertSkip {
                        path: rel,
                        reason: ConvertSkipReason::Changed,
                        moved_to: None,
                    });
                    Ok(false)
                }
                Replacement::Replaced(placed) => {
                    let dirty = match stored {
                        Some((item_id, stored_hash)) => update_record(entry, item_id, stored_hash),
                        None => update_object_hash(entry, &actual),
                    };
                    if placed == super::PlacedAs::Link {
                        summary.files_converted += 1;
                        summary.bytes = summary.bytes.saturating_add(metadata.len());
                    } else {
                        // The file system refused a link, so this is still an owned copy.
                        summary.files_already += 1;
                    }
                    Ok(dirty)
                }
            }
        }
        PlacementMode::Copy => {
            if !object.is_file() || !same_file(&target, &object) {
                summary.files_already += 1;
                return Ok(false);
            }
            match replace_with(ctx, instance_dir, journal_root, &target, &actual, mode)? {
                Replacement::Changed => {
                    summary.skipped.push(ConvertSkip {
                        path: rel,
                        reason: ConvertSkipReason::Changed,
                        moved_to: None,
                    });
                    Ok(false)
                }
                Replacement::Replaced(_) => {
                    summary.files_converted += 1;
                    summary.bytes = summary.bytes.saturating_add(metadata.len());
                    Ok(false)
                }
            }
        }
    }
}

fn update_record(entry: &mut InstalledMod, item_id: String, stored_hash: String) -> bool {
    let mut dirty = false;
    if entry.content_item_id.as_deref() != Some(item_id.as_str()) {
        entry.content_item_id = Some(item_id);
        dirty = true;
    }
    dirty |= update_object_hash(entry, &stored_hash);
    dirty
}

fn update_object_hash(entry: &mut InstalledMod, sha256: &str) -> bool {
    if entry.object_sha256.as_deref() != Some(sha256) {
        entry.object_sha256 = Some(sha256.to_string());
        true
    } else {
        false
    }
}

/// The file's path inside the instance (`mods/example.jar` or its `.disabled` name), and the
/// manifest's plain name when it is disabled.
fn resolve_target(instance_dir: &Path, entry: &InstalledMod) -> Option<(PathBuf, String)> {
    if !safe_filename(&entry.filename) {
        return None;
    }
    let folder = content_subdir(&entry.content_type);
    let effective = if entry.enabled {
        entry.filename.clone()
    } else {
        format!("{}.disabled", entry.filename)
    };
    let other = if entry.enabled {
        format!("{}.disabled", entry.filename)
    } else {
        entry.filename.clone()
    };
    for name in [effective, other] {
        let path = instance_dir.join(folder).join(&name);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                return Some((path, format!("{folder}/{name}")));
            }
            _ => {}
        }
    }
    None
}

fn safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains('\0')
}

fn displayed_name(entry: &InstalledMod) -> String {
    if entry.enabled {
        entry.filename.clone()
    } else {
        format!("{}.disabled", entry.filename)
    }
}

fn display_path(entry: &InstalledMod) -> String {
    format!(
        "{}/{}",
        content_subdir(&entry.content_type),
        displayed_name(entry)
    )
}

fn recorded_hash(entry: &InstalledMod) -> Option<String> {
    entry
        .object_sha256
        .as_deref()
        .or(Some(entry.sha256.as_str()))
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Add the file to the store as a single-file item and return its item id and object hash.
fn store_from_file(
    ctx: &Ctx,
    file: &Path,
    entry: &InstalledMod,
) -> Result<(String, String), crate::content_store::ContentError> {
    let rel = format!("{}/{}", content_subdir(&entry.content_type), entry.filename);
    let name = Path::new(&entry.filename)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.filename.clone());
    let outcome = add_file(ctx, file, Some(&name), Some(&rel))?;
    let item = outcome.item();
    let stored = item
        .files
        .first()
        .ok_or(crate::content_store::ContentError::EmptyItem)?;
    Ok((item.item_id.clone(), stored.sha256.clone()))
}

enum Replacement {
    Replaced(super::PlacedAs),
    /// The file no longer hashed to the recorded value after it was moved aside. It was put back
    /// with the change intact, and the conversion left it alone.
    Changed,
}

/// Move the current file into the journal, verify the moved file still has the bytes that were
/// checked, and only then place the replacement. An edit that lands after the first hash is
/// therefore put back, not deleted. A failure puts the moved file back.
fn replace_with(
    ctx: &Ctx,
    instance_dir: &Path,
    journal_root: &Path,
    target: &Path,
    sha256: &str,
    mode: PlacementMode,
) -> Result<Replacement, crate::content_store::ContentError> {
    let journaled = move_to_journal(instance_dir, journal_root, target)?;
    let still_matches = match hash_file_sha256(&journaled) {
        Ok(actual) => actual.eq_ignore_ascii_case(sha256),
        Err(error) => {
            return Err(restore_journaled(
                &journaled,
                target,
                crate::content_store::ContentError::Io(error),
            ));
        }
    };
    if !still_matches {
        return match std::fs::rename(&journaled, target) {
            Ok(()) => Ok(Replacement::Changed),
            Err(restore) => Err(crate::content_store::ContentError::Other(format!(
                "the file changed while it was being converted and could not be put back: {restore}; \
                 the changed file is kept at {}",
                journaled.display()
            ))),
        };
    }
    match place_object_at(ctx, sha256, target, mode) {
        Ok(placed) => {
            // The moved original is now verified and no longer needed.
            std::fs::remove_file(&journaled)?;
            prune_empty_dirs(journaled.parent().unwrap_or(journal_root), journal_root);
            Ok(Replacement::Replaced(placed))
        }
        Err(error) => Err(restore_journaled(&journaled, target, error)),
    }
}

/// Put a journaled file back at its target, returning `error` when it succeeds and a combined
/// error (naming the journal path, so the file is still findable) when it cannot.
fn restore_journaled(
    journaled: &Path,
    target: &Path,
    error: crate::content_store::ContentError,
) -> crate::content_store::ContentError {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(journaled, target) {
        Ok(()) => error,
        Err(restore) => crate::content_store::ContentError::Other(format!(
            "{error}; the original file is kept at {} because it could not be put back: {restore}",
            journaled.display()
        )),
    }
}

fn move_to_journal(
    instance_dir: &Path,
    journal_root: &Path,
    target: &Path,
) -> Result<PathBuf, crate::content_store::ContentError> {
    let relative = target.strip_prefix(instance_dir).map_err(|_| {
        crate::content_store::ContentError::InvalidPath {
            path: target.display().to_string(),
            reason: "file is outside the instance folder".into(),
        }
    })?;
    let journaled = journal_root.join(relative);
    if let Some(parent) = journaled.parent() {
        std::fs::create_dir_all(parent)?;
        // A link to a protected object cannot be deleted from a folder that lacks
        // FILE_DELETE_CHILD. Grant it here, before the link is moved in, and never ignore a
        // refusal: the journal could not be cleaned up later otherwise.
        super::place::ensure_delete_child(parent)?;
    }
    if journaled.symlink_metadata().is_ok() {
        return Err(crate::content_store::ContentError::Other(format!(
            "a previous conversion already keeps a file at {}; run it again to finish it first",
            journaled.display()
        )));
    }
    std::fs::rename(target, &journaled)?;
    Ok(journaled)
}

/// Restore or tidy every file an interrupted run left in the journal.
///
/// A target that does not match the hash the manifest recorded is a half-written replacement, not
/// the file: it is moved aside (never deleted) and the intact original is put back.
fn recover_journal(
    instance_dir: &Path,
    journal_root: &Path,
    expected: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<ConvertSkip>, crate::content_store::ContentError> {
    let mut skipped = Vec::new();
    if !journal_root.is_dir() {
        return Ok(skipped);
    }
    // A journal left by an older build may hold links in a folder that lacks delete-child.
    super::place::ensure_delete_child(journal_root)?;
    let files = super::collect_folder_files(journal_root)?;
    for (relative, journaled) in files {
        let rel = relative.as_str().to_string();
        if let Some(parent) = journaled.parent() {
            super::place::ensure_delete_child(parent)?;
        }
        let target = instance_dir.join(&rel);
        if target.symlink_metadata().is_err() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&journaled, &target)?;
            continue;
        }
        if let Some(expected_hash) = expected.get(&rel) {
            let target_ok = hash_file_sha256(&target)
                .map(|actual| actual.eq_ignore_ascii_case(expected_hash))
                .unwrap_or(false);
            if target_ok {
                // The replacement completed; the journal copy is the original. Remove it only
                // when it is still the recorded file; anything else is preserved.
                let journal_ok = hash_file_sha256(&journaled)
                    .map(|actual| actual.eq_ignore_ascii_case(expected_hash))
                    .unwrap_or(false);
                if journal_ok {
                    std::fs::remove_file(&journaled)?;
                } else {
                    skipped.push(move_aside(instance_dir, &rel, &journaled)?);
                }
                continue;
            }
            // A truncated or otherwise wrong target is not the file. Keep it (it may hold
            // something a person wrote) and restore the original from the journal.
            let moved_to = unique_moved_aside(instance_dir, &rel);
            if let Some(parent) = moved_to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&target, &moved_to)?;
            std::fs::rename(&journaled, &target)?;
            skipped.push(ConvertSkip {
                path: rel,
                reason: ConvertSkipReason::MovedAside,
                moved_to: Some(moved_to.display().to_string()),
            });
            continue;
        }
        // No manifest record (for example, a file removed from the manifest since). When the
        // bytes agree the journal copy is a leftover from before the final cleanup.
        let same_bytes = match (
            hash_file_sha256(&journaled),
            std::fs::metadata(&target).map(|meta| meta.is_file()),
        ) {
            (Ok(left), Ok(true)) => hash_file_sha256(&target)
                .map(|right| left.eq_ignore_ascii_case(&right))
                .unwrap_or(false),
            _ => false,
        };
        if same_bytes {
            std::fs::remove_file(&journaled)?;
        } else {
            skipped.push(move_aside(instance_dir, &rel, &journaled)?);
        }
    }
    Ok(skipped)
}

fn move_aside(
    instance_dir: &Path,
    rel: &str,
    file: &Path,
) -> Result<ConvertSkip, crate::content_store::ContentError> {
    let moved_to = unique_moved_aside(instance_dir, rel);
    if let Some(parent) = moved_to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(file, &moved_to)?;
    Ok(ConvertSkip {
        path: rel.to_string(),
        reason: ConvertSkipReason::MovedAside,
        moved_to: Some(moved_to.display().to_string()),
    })
}

fn unique_moved_aside(instance_dir: &Path, relative: &str) -> PathBuf {
    let base = instance_dir.join(MOVED_ASIDE_DIR).join(relative);
    if base.symlink_metadata().is_err() {
        return base;
    }
    for suffix in 1..1000 {
        let candidate = base.with_file_name(format!(
            "{}.{suffix}",
            base.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into())
        ));
        if candidate.symlink_metadata().is_err() {
            return candidate;
        }
    }
    base
}

fn prune_empty_dirs(start: &Path, stop: &Path) {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir == stop || dir.as_os_str().is_empty() {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

fn preview_manifest(
    ctx: &Ctx,
    instance_dir: &Path,
    manifest: &InstanceManifest,
    mode: PlacementMode,
) -> ConversionSummary {
    let mut summary = ConversionSummary::default();
    let entries = manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter());
    for entry in entries {
        if entry.content_type == "world" {
            continue;
        }
        let Some((target, rel)) = resolve_target(instance_dir, entry) else {
            summary.skipped.push(ConvertSkip {
                path: display_path(entry),
                reason: ConvertSkipReason::Missing,
                moved_to: None,
            });
            continue;
        };
        let size = std::fs::metadata(&target)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let Some(recorded) = recorded_hash(entry) else {
            summary.skipped.push(ConvertSkip {
                path: rel,
                reason: ConvertSkipReason::Unsupported,
                moved_to: None,
            });
            continue;
        };
        match hash_file_sha256(&target) {
            Ok(actual) if actual.eq_ignore_ascii_case(&recorded) => {
                let object = ctx.paths.content_object_path(&actual.to_ascii_lowercase());
                let shared = object.is_file() && same_file(&target, &object);
                match mode {
                    PlacementMode::Link if shared => summary.files_already += 1,
                    PlacementMode::Link => {
                        summary.files_converted += 1;
                        summary.bytes = summary.bytes.saturating_add(size);
                    }
                    PlacementMode::Copy if shared => {
                        summary.files_converted += 1;
                        summary.bytes = summary.bytes.saturating_add(size);
                    }
                    PlacementMode::Copy => summary.files_already += 1,
                }
            }
            Ok(_) => summary.skipped.push(ConvertSkip {
                path: rel,
                reason: ConvertSkipReason::Changed,
                moved_to: None,
            }),
            Err(_) => summary.skipped.push(ConvertSkip {
                path: rel,
                reason: ConvertSkipReason::Missing,
                moved_to: None,
            }),
        }
    }
    summary
}
