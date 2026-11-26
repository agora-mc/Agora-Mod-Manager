//! The record an in-flight operation writes before it reuses or adds an object (MASTER_SPEC §32.8).
//!
//! A collection works out what is in use from the owners' own records. An operation that has just
//! put an object in the store but has not yet published the record that names it (a snapshot
//! manifest, an item record, an instance manifest) would otherwise look exactly like an abandoned
//! object, and be deleted from under it. So it writes one small file first:
//! `content/pending/<pid>-<uuid>.json`, naming the hashes it is about to use and the process that
//! will publish the owner record. The collection keeps those hashes while that process is alive or
//! cannot be judged, and ignores them once the one liveness check proves it dead.
//!
//! The file is written under the content-store lock, so a collection that is already running has
//! either listed the records before this one existed (and will not delete inside its grace period,
//! which starts at the object's own write) or will see it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::lock_manager::{LockManager, LockResource};
use crate::process_identity::{self, ProcessIdentity};

/// The schema of a pending record on disk. A record from a newer schema is read as "cannot tell",
/// which keeps its hashes: a pre-release experimental format is the only thing that can have
/// written one, and guessing its contents is the wrong way to find out.
pub(crate) const PENDING_SCHEMA_VERSION: u32 = 1;

/// What one in-flight operation says it is about to use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingRecord {
    pub schema_version: u32,
    /// The process that will publish the owner record, as the liveness check reads it.
    pub process: ProcessIdentity,
    /// The object hashes this operation reuses or adds.
    pub hashes: Vec<String>,
    /// What the operation is, for the message a collection prints.
    pub operation: String,
    /// When the record was written, in RFC 3339, for the same message.
    pub created_at: String,
}

impl PendingRecord {
    /// Read one record. Anything other than a complete, current-schema record is an error: an
    /// unreadable record may name any object, so the caller refuses rather than passing it over.
    pub(crate) fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let record: PendingRecord = serde_json::from_str(&text)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if record.schema_version != PENDING_SCHEMA_VERSION {
            return Err(format!(
                "{} is written in schema {} and this build reads schema {PENDING_SCHEMA_VERSION}, \
                 so which objects it holds cannot be told",
                path.display(),
                record.schema_version
            ));
        }
        Ok(record)
    }
}

/// A pending record this process wrote, removed when the operation finishes or fails.
#[derive(Debug)]
pub(crate) struct PendingGuard {
    path: PathBuf,
    active: bool,
}

impl PendingGuard {
    /// The record's path, for a message that has to say what it left behind.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The owner record is published: the objects this operation used are in use on their own
    /// account now.
    pub(crate) fn published(mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.active = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.active {
            // The operation failed, or ended without publishing. Either way nobody is about to name
            // these objects, so the record goes with it.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write a pending record naming `hashes` before this operation reuses or adds any of them.
///
/// The record is written under the content-store lock, which is released before this returns: the
/// lock is not reentrant, and an operation that already holds it (a content add) publishes its owner
/// record inside that one hold and needs no record at all.
pub(crate) fn begin_pending_use(
    paths: &AppPaths,
    hashes: &[String],
    operation: &str,
) -> Result<PendingGuard, String> {
    let manager = LockManager::new(paths.locks_root());
    let _lock = manager
        .acquire(LockResource::ContentStore, operation)
        .map_err(|error| {
            format!("cannot register the objects {operation} is about to use: {error}")
        })?;
    write_pending_record(paths, hashes, operation)
}

/// Write the record itself. The caller holds the content-store lock.
fn write_pending_record(
    paths: &AppPaths,
    hashes: &[String],
    operation: &str,
) -> Result<PendingGuard, String> {
    let dir = paths.content_pending_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let identity = process_identity::capture_self().ok_or_else(|| {
        format!(
            "cannot read this process's own identity, so {operation} cannot say which objects it \
             is about to use"
        )
    })?;
    let record = PendingRecord {
        schema_version: PENDING_SCHEMA_VERSION,
        process: identity,
        hashes: hashes.to_vec(),
        operation: operation.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let text = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("cannot serialize the {operation} record: {error}"))?;
    let path = dir.join(format!(
        "{}-{}.json",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temp, &text)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot publish {}: {error}", path.display())
    })?;
    Ok(PendingGuard { path, active: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_from_a_newer_schema_is_cannot_tell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1-x.json");
        std::fs::write(
            &path,
            br#"{"schema_version":99,"process":{"pid":1,"start_time":1},"hashes":[],"operation":"x","created_at":"now"}"#,
        )
        .unwrap();
        let error = PendingRecord::read(&path).unwrap_err();
        assert!(
            error.contains("cannot be told"),
            "a newer schema must read as can't tell, not as an empty list: {error}"
        );
    }

    #[test]
    fn a_record_is_removed_when_it_is_published() {
        let root = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(root.path().to_path_buf());
        let guard = begin_pending_use(&paths, &["a".repeat(64)], "test").unwrap();
        let path = guard.path().to_path_buf();
        assert!(path.is_file());
        guard.published();
        assert!(!path.exists());
    }
}
