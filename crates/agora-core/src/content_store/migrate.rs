//! Move pre-B3a snapshot objects into the content store and retire the legacy store.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::artifact_receipt::receipt_path;
use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::{LockRefusal, LockResource};
use crate::process_identity::{self, OsProcesses};
use crate::recovery::{
    self, ContentProof, Decision, Envelope, JournalProblem, JournalRead, OwnedPath, PathIdentity,
    PathProof, ProofResult, RecoveryCapability,
};

const RESOURCE: LockResource = LockResource::ContentStore;

#[derive(Debug)]
pub enum MigrationOutcome {
    NothingToDo,
    Done(MigrationReport),
    Busy,
    NeedsHand(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    pub moved: usize,
    pub already_present: usize,
    pub replaced_damaged: usize,
    pub mismatched: Vec<PathBuf>,
    #[serde(with = "left_alone_paths")]
    pub left_alone: Vec<(PathBuf, String)>,
}

// Serde's PathBuf needs Unicode. A stray native filename is still a reportable path.
mod left_alone_paths {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        entries: &[(PathBuf, String)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            entries
                .iter()
                .map(|(path, reason)| (path.as_os_str(), reason)),
        )
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(PathBuf, String)>, D::Error> {
        let entries = Vec::<(OsString, String)>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|(path, reason)| (path.into(), reason))
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    Running,
    Done,
}

impl recovery::JournalState for State {
    const KIND: recovery::OperationKind = recovery::OperationKind::SnapshotObjectMigration;
}

#[derive(Serialize, Deserialize)]
struct MigrationJournal {
    #[serde(flatten)]
    envelope: Envelope<State>,
    legacy_identity: Option<PathIdentity>,
    report: MigrationReport,
    // Counts are written after placement but before unlink. If that unlink did not happen,
    // finish this object first on resume without counting its now-duplicate bytes again.
    last_counted: Option<PathBuf>,
}

pub fn journal_path(paths: &AppPaths) -> PathBuf {
    paths
        .content_root()
        .join("migrations/snapshot-objects.json")
}

pub(crate) fn legacy_root(paths: &AppPaths) -> PathBuf {
    paths.root().join("snapshot-objects")
}

/// Keep the fallback available until there is a valid retirement record.
pub fn legacy_store_active(paths: &AppPaths) -> bool {
    let root = legacy_root(paths);
    // An inspection failure must also reach the collector, which can report and refuse it.
    !matches!(metadata(&root), Ok(None))
        && !matches!(
            read_journal(&root, &journal_path(paths)),
            JournalRead::Valid(journal) if journal.envelope().state == State::Done
        )
}

/// What the migration journal says, read without taking a lock and without writing anything, so a
/// read-only report (`agora doctor`) can say what a start would make of it.
#[derive(Debug)]
pub(crate) enum JournalStatus {
    /// No migration was started here: nothing to finish, and nothing to report.
    Absent,
    /// A run is in progress, or was cut short. The next start finishes it.
    Running,
    /// The migration finished. The journal is its record, not work recovery has left to do.
    Done,
    /// A journal is there but cannot be acted on, with the reason.
    Unusable(String),
}

pub fn migrate_snapshot_objects(ctx: &Ctx) -> LauncherResult<MigrationOutcome> {
    run(ctx, false)
}

/// The finished migration's own record, read from its journal without writing anything.
///
/// `None` when there is no finished record: no journal, one still running, or one that cannot be
/// read. A command that has just been told there is nothing to do asks this to report what the
/// migration that did run moved, instead of saying nothing happened at all.
pub fn migration_status(paths: &AppPaths) -> Option<MigrationReport> {
    match read_journal(&legacy_root(paths), &journal_path(paths)) {
        JournalRead::Valid(journal) if journal.envelope().state == State::Done => journal
            .payload::<MigrationJournal>()
            .ok()
            .map(|journal| journal.report),
        _ => None,
    }
}

/// [`JournalStatus`] for `paths`: the journal's state, or why it cannot be read.
pub(crate) fn journal_status(paths: &AppPaths) -> JournalStatus {
    match read_journal(&legacy_root(paths), &journal_path(paths)) {
        JournalRead::Absent => JournalStatus::Absent,
        JournalRead::Unusable(problem) => JournalStatus::Unusable(problem.to_string()),
        JournalRead::Valid(journal) => match journal.envelope().state {
            State::Running => JournalStatus::Running,
            State::Done => JournalStatus::Done,
        },
    }
}

/// Archive an unusable journal and start from the legacy files still on disk.
pub fn restart_snapshot_object_migration(ctx: &Ctx) -> LauncherResult<MigrationOutcome> {
    run(ctx, true)
}

fn run(ctx: &Ctx, mut restart: bool) -> LauncherResult<MigrationOutcome> {
    let root = legacy_root(&ctx.paths);
    let path = journal_path(&ctx.paths);
    if matches!(metadata(&root), Ok(None)) && matches!(metadata(&path), Ok(None)) {
        return Ok(MigrationOutcome::NothingToDo);
    }
    let lock = match ctx.lock_manager.acquire_with_timeout_typed(
        RESOURCE,
        "snapshot-object-migration",
        Duration::ZERO,
        None,
    ) {
        Ok(lock) => lock,
        Err(LockRefusal::Held(_)) => return Ok(MigrationOutcome::Busy),
        Err(LockRefusal::Unavailable(error)) => return Err(error),
    };
    let view = ctx.test_seams.process_view();
    let view = view.as_deref().unwrap_or(&OsProcesses);

    loop {
        let read = read_journal(&root, &path);
        let journal = match &read {
            JournalRead::Absent => {
                let Some(entry) = metadata(&root)? else {
                    return Ok(MigrationOutcome::NothingToDo);
                };
                let identity = if entry.is_dir() && !agora_game_api::is_a_reparse_point(&entry) {
                    recovery::path_identity(&root).map_err(|error| file_error(&root, error))?
                } else {
                    None
                };
                let journal = MigrationJournal {
                    envelope: Envelope::new(&RESOURCE, State::Running, ownership(&root))?,
                    legacy_identity: identity,
                    report: MigrationReport::default(),
                    last_counted: None,
                };
                recovery::write(&path, &journal)?;
                // The journal's own rename flushed migrations/. Persist any parent directories
                // this first journal write just created before it authorizes an unlink.
                sync_parent(path.parent().expect("journal has a directory"))?;
                sync_parent(&ctx.paths.content_root())?;
                continue;
            }
            JournalRead::Valid(journal) if journal.envelope().state == State::Done => {
                return Ok(MigrationOutcome::NothingToDo);
            }
            JournalRead::Valid(journal) => {
                Some(journal.payload::<MigrationJournal>().map_err(|error| {
                    migration_error(format!("Could not read {}: {error}", path.display()))
                })?)
            }
            JournalRead::Unusable(_) => None,
        };
        let proof = LegacyProof {
            root: &root,
            identity: journal.as_ref().and_then(|journal| journal.legacy_identity),
        };
        // A previous invocation in this process may have returned an error. The same process
        // now holds the same lock again, so its earlier invocation has already stopped.
        let self_writer = journal.as_ref().and_then(|journal| {
            let writer = journal.envelope.writer.as_ref()?;
            (writer.pid == std::process::id()
                && process_identity::capture_self().as_ref() == Some(writer))
            .then_some(journal.envelope.operation_id.as_str())
        });
        let decision = recovery::decide_holding_reading(
            &lock,
            &RESOURCE,
            Some(&path),
            || read,
            &proof,
            view,
            self_writer,
        );
        match decision {
            Decision::Act(capability) => {
                return migrate(
                    ctx,
                    &root,
                    &path,
                    journal.expect("an actionable decision has a validated payload"),
                    &capability,
                );
            }
            Decision::Busy(_) => return Ok(MigrationOutcome::Busy),
            Decision::Nothing => return Ok(MigrationOutcome::NothingToDo),
            Decision::NeedsHand(_) if restart => {
                let archive = path.with_extension(format!("{}.json", uuid::Uuid::new_v4()));
                recovery::durable_rename(&path, &archive, false)
                    .map_err(|error| file_error(&path, error))?;
                restart = false;
            }
            Decision::NeedsHand(reason) => return Ok(MigrationOutcome::NeedsHand(reason)),
        }
    }
}

fn ownership(root: &Path) -> Vec<OwnedPath> {
    vec![OwnedPath {
        path: root.to_path_buf(),
        proof: PathProof::Content,
    }]
}

fn read_journal(root: &Path, path: &Path) -> JournalRead<State> {
    match metadata(path) {
        Ok(None) => return JournalRead::Absent,
        Ok(Some(entry)) if entry.is_file() && !agora_game_api::is_a_reparse_point(&entry) => {}
        Ok(Some(_)) => {
            return JournalRead::Unusable(JournalProblem::CantTell(format!(
                "{} is a link or is not a plain file",
                path.display()
            )));
        }
        Err(error) => return JournalRead::Unusable(JournalProblem::CantTell(error.to_string())),
    }
    let read = recovery::read::<State>(path);
    if let JournalRead::Valid(journal) = &read {
        let envelope = journal.envelope();
        let owns_legacy = matches!(
            envelope.owned.as_slice(),
            [entry] if entry.proof == PathProof::Content
                && agora_game_api::same_spelling(&entry.path, root)
        );
        let valid_scope = match envelope.state {
            State::Running => owns_legacy,
            State::Done => envelope.owned.is_empty(),
        };
        let valid_lock = matches!(
            RESOURCE.lock_name().as_deref(),
            Ok(lock) if lock == envelope.lock.as_str()
        );
        if !valid_lock || !valid_scope {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{} does not name the snapshot object store and its lock",
                path.display()
            )));
        }
        if let Err(problem) = journal.payload::<MigrationJournal>() {
            return JournalRead::Unusable(problem);
        }
    }
    read
}

struct LegacyProof<'a> {
    root: &'a Path,
    identity: Option<PathIdentity>,
}

impl ContentProof for LegacyProof<'_> {
    fn prove(&self, path: &Path) -> ProofResult {
        // The journal's proof is the identity recorded for the app's fixed legacy store.
        match recovery::path_identity(path) {
            Ok(identity)
                if agora_game_api::same_spelling(path, self.root) && identity == self.identity =>
            {
                ProofResult::Proven
            }
            Ok(_) => ProofResult::CantTell(format!(
                "{} is no longer the legacy directory recorded in the journal",
                path.display()
            )),
            Err(error) => ProofResult::CantTell(format!(
                "{} cannot be identified as a plain legacy directory: {error}",
                path.display()
            )),
        }
    }
}

fn migrate(
    ctx: &Ctx,
    root: &Path,
    path: &Path,
    mut journal: MigrationJournal,
    capability: &RecoveryCapability<'_, State>,
) -> LauncherResult<MigrationOutcome> {
    let identity = capability.verified_identity(root)?;
    if identity != journal.legacy_identity {
        return Ok(MigrationOutcome::NeedsHand(format!(
            "{} changed while its ownership was being checked",
            root.display()
        )));
    }
    if identity.is_some() {
        let mut files = Vec::new();
        let mut directories = vec![root.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(&directory).map_err(|error| file_error(&directory, error))? {
                let entry = entry.map_err(|error| file_error(&directory, error))?.path();
                let Some(kind) = metadata(&entry)? else {
                    continue;
                };
                if agora_game_api::is_a_reparse_point(&kind) {
                    return Ok(MigrationOutcome::NeedsHand(format!(
                        "{} is a link or reparse point in the legacy snapshot store",
                        entry.display()
                    )));
                }
                if kind.is_dir() {
                    if directory != root || !is_shard(&entry) {
                        record_left(&mut journal.report, &entry, "unexpected directory");
                    }
                    directories.push(entry);
                } else if kind.is_file() {
                    files.push(entry);
                } else {
                    record_left(&mut journal.report, &entry, "not a regular object file");
                }
            }
        }
        files.sort_unstable();
        if let Some(Ok(index)) = journal
            .last_counted
            .as_ref()
            .map(|file| files.binary_search(file))
        {
            files[..=index].rotate_right(1);
        }
        recovery::write(path, &journal)?;

        #[cfg(test)]
        let mut placed_this_run = 0;
        for file in files {
            // The receipt of an earlier object may already have been removed.
            let Some(kind) = metadata(&file)? else {
                continue;
            };
            if !kind.is_file() || agora_game_api::is_a_reparse_point(&kind) {
                return Ok(MigrationOutcome::NeedsHand(format!(
                    "{} is no longer a plain legacy file",
                    file.display()
                )));
            }
            let Some(name) = object_name(root, &file) else {
                let reason = if file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".agora-verified.json"))
                {
                    "receipt of an object that did not move"
                } else {
                    "filename or fan-out directory does not match the object layout"
                };
                if record_left(&mut journal.report, &file, reason) {
                    recovery::write(path, &journal)?;
                }
                continue;
            };
            let actual = super::hash_file_path(&file).map_err(|error| file_error(&file, error))?;
            if !actual.eq_ignore_ascii_case(name) {
                if !journal.report.mismatched.contains(&file) {
                    journal.report.mismatched.push(file);
                    recovery::write(path, &journal)?;
                }
                continue;
            }

            let destination = ctx.paths.content_object_path(&actual);
            let present = plain_destination(&ctx.paths, &destination)?;
            let verified = present
                && super::hash_file_path(&destination)
                    .map_err(|error| file_error(&destination, error))?
                    == actual;
            if !verified {
                super::place_object_from_file_locked(&ctx.paths, &file, &actual, present).map_err(
                    |error| migration_error(format!("Could not place {}: {error}", file.display())),
                )?;
            }
            // Placement flushes the destination shard. Its parent entries also have to survive
            // the crash that could follow removal of the only other copy.
            sync_parent(destination.parent().expect("object has a shard directory"))?;
            sync_parent(&ctx.paths.content_objects_dir())?;

            if journal.last_counted.as_ref() != Some(&file) {
                if verified {
                    journal.report.already_present += 1;
                } else if present {
                    journal.report.replaced_damaged += 1;
                } else {
                    journal.report.moved += 1;
                }
                journal.last_counted = Some(file.clone());
            }
            recovery::write(path, &journal)?;
            #[cfg(test)]
            if !verified {
                placed_this_run += 1;
                if ctx
                    .test_seams
                    .snapshot_migration_should_stop(placed_this_run)
                {
                    return Err(migration_error(format!(
                        "Stopped after placing {}",
                        file.display()
                    )));
                }
            }

            let receipt = receipt_path(&file);
            if let Some(kind) = metadata(&receipt)? {
                if agora_game_api::is_a_reparse_point(&kind) {
                    return Ok(MigrationOutcome::NeedsHand(format!(
                        "{} is a link or reparse point in the legacy snapshot store",
                        receipt.display()
                    )));
                }
                if kind.is_file() {
                    capability.remove_file(root, &receipt)?;
                }
            }
            capability.remove_file(root, &file)?;
        }
    }
    journal.envelope.state = State::Done;
    // The terminal journal grants no capability. It stays valid if the data root is relocated.
    journal.envelope.owned.clear();
    journal.legacy_identity = None;
    journal.last_counted = None;
    recovery::write(path, &journal)?;
    Ok(MigrationOutcome::Done(journal.report))
}

fn record_left(report: &mut MigrationReport, path: &Path, reason: &str) -> bool {
    if !report
        .left_alone
        .iter()
        .any(|(existing, _)| existing == path)
    {
        report.left_alone.push((path.to_path_buf(), reason.into()));
        return true;
    }
    false
}

fn is_shard(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub(crate) fn object_name<'a>(root: &Path, file: &'a Path) -> Option<&'a str> {
    let shard = file.parent()?;
    let prefix = shard.file_name()?.to_str()?;
    let name = file.file_name()?.to_str()?;
    (shard.parent() == Some(root)
        && crate::snapshot::validate_blob_hash(name).is_ok()
        && name[..2].eq_ignore_ascii_case(prefix))
    .then_some(name)
}

/// Check the destination before hashing it. A redirected object is not a verified store copy.
fn plain_destination(paths: &AppPaths, file: &Path) -> LauncherResult<bool> {
    for directory in [
        paths.content_root(),
        paths.content_objects_dir(),
        file.parent().unwrap().to_path_buf(),
    ] {
        match metadata(&directory)? {
            Some(entry) if entry.is_dir() && !agora_game_api::is_a_reparse_point(&entry) => {}
            None => return Ok(false),
            Some(_) => {
                return Err(migration_error(format!(
                    "{} is not a plain content directory",
                    directory.display()
                )))
            }
        }
    }
    match metadata(file)? {
        Some(entry) if entry.is_file() && !agora_game_api::is_a_reparse_point(&entry) => Ok(true),
        None => Ok(false),
        Some(_) => Err(migration_error(format!(
            "{} is not a plain content object",
            file.display()
        ))),
    }
}

fn metadata(path: &Path) -> LauncherResult<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(file_error(path, error)),
    }
}

fn file_error(path: &Path, error: io::Error) -> LauncherError {
    migration_error(format!("{}: {error}", path.display()))
}

fn sync_parent(path: &Path) -> LauncherResult<()> {
    recovery::sync_parent(path).map_err(|error| file_error(path, error))
}

fn migration_error(message: String) -> LauncherError {
    LauncherError::Generic {
        code: crate::error_code::ErrorCode::SnapshotObjectMigration
            .as_str()
            .into(),
        message,
    }
}
