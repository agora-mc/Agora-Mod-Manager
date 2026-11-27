//! Move pre-B3a snapshot objects into the content store and retire the legacy store.

use std::collections::BTreeMap;
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
pub(crate) const MIGRATE_COMMAND: &str = "agora games content migrate";
const RESTART_COMMAND: &str = "agora games content migrate --restart";

#[derive(Debug)]
pub enum MigrationOutcome {
    NothingToDo,
    Done(MigrationReport),
    Busy,
    NeedsHand(MigrationRefusal),
}

#[derive(Debug)]
pub enum MigrationRefusal {
    UnusableJournal(String),
    LegacyLink(PathBuf),
    Other(String),
}

impl MigrationRefusal {
    pub fn command(&self) -> Option<&'static str> {
        match self {
            Self::UnusableJournal(_) => Some(RESTART_COMMAND),
            Self::LegacyLink(_) => Some(MIGRATE_COMMAND),
            Self::Other(_) => None,
        }
    }
}

impl std::fmt::Display for MigrationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnusableJournal(problem) => write!(
                f,
                "The migration journal cannot be used ({problem}). Run `{RESTART_COMMAND}` to set it aside and walk the old store again."
            ),
            Self::LegacyLink(path) => write!(
                f,
                "The legacy snapshot store has a link or reparse point at {}. Remove or replace it, then run `{MIGRATE_COMMAND}`.",
                path.display()
            ),
            Self::Other(reason) => f.write_str(reason),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MigrationReport {
    pub moved: usize,
    pub already_present: usize,
    pub replaced_damaged: usize,
    pub mismatched: Vec<PathBuf>,
    #[serde(with = "left_alone_paths")]
    pub left_alone: Vec<(PathBuf, String)>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    mismatch_hashes: BTreeMap<PathBuf, String>,
}

impl MigrationReport {
    fn unchanged_since(&self, previous: &Self) -> bool {
        self.moved == 0
            && self.already_present == 0
            && self.replaced_damaged == 0
            && self.mismatched == previous.mismatched
            && self.left_alone == previous.left_alone
            // Older Done journals recorded only the paths. Learn their hashes on this pass.
            && previous
                .mismatch_hashes
                .iter()
                .all(|(path, hash)| self.mismatch_hashes.get(path) == Some(hash))
    }

    fn sort_findings(&mut self) {
        self.mismatched.sort_unstable();
        self.mismatched.dedup();
        self.left_alone.sort_unstable();
        self.left_alone.dedup_by(|left, right| left.0 == right.0);
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_done: Option<MigrationReport>,
}

pub fn journal_path(paths: &AppPaths) -> PathBuf {
    paths
        .content_root()
        .join("migrations/snapshot-objects.json")
}

pub(crate) fn legacy_root(paths: &AppPaths) -> PathBuf {
    paths.root().join("snapshot-objects")
}

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyStoreState {
    Active,
    Retired,
    Unknown(String),
}

/// Decide whether to consult the legacy store before verifying any bytes there.
pub fn legacy_store_state(paths: &AppPaths) -> LegacyStoreState {
    let root = legacy_root(paths);
    let present = match metadata(&root) {
        Ok(None) => false,
        Ok(Some(entry)) if entry.is_dir() && !agora_game_api::is_a_reparse_point(&entry) => true,
        Ok(Some(_)) => {
            return LegacyStoreState::Unknown(format!(
                "{} is not a plain directory",
                root.display()
            ))
        }
        Err(error) => return LegacyStoreState::Unknown(error.to_string()),
    };
    match read_journal(&root, &journal_path(paths)) {
        JournalRead::Unusable(problem) => LegacyStoreState::Unknown(problem.to_string()),
        JournalRead::Valid(journal) if journal.envelope().state == State::Done => {
            LegacyStoreState::Retired
        }
        _ if present => LegacyStoreState::Active,
        _ => LegacyStoreState::Retired,
    }
}

/// Uncertain journal or folder state still allows independently verified legacy bytes.
pub fn legacy_store_active(paths: &AppPaths) -> bool {
    legacy_store_state(paths) != LegacyStoreState::Retired
}

/// The migration's status for `agora doctor`, inspected without taking a lock or writing.
#[derive(Debug)]
pub(crate) enum JournalStatus {
    Absent,
    Running,
    Done,
    PendingReopen,
    NeedsHand(Option<&'static str>, MigrationRefusal),
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

/// Inspect the journal and legacy directory without opening or hashing objects.
pub(crate) fn journal_status(paths: &AppPaths) -> JournalStatus {
    let root = legacy_root(paths);
    let read = match read_journal(&root, &journal_path(paths)) {
        JournalRead::Unusable(problem) => {
            return JournalStatus::NeedsHand(
                None,
                MigrationRefusal::UnusableJournal(problem.to_string()),
            );
        }
        read => read,
    };
    let state = match &read {
        JournalRead::Valid(journal) => Some(match journal.envelope().state {
            State::Running => "running",
            State::Done => "done",
        }),
        _ => None,
    };
    match inspect_legacy_store(&root, read) {
        Ok(status) => status,
        Err(cause) => JournalStatus::NeedsHand(state, cause),
    }
}

fn inspect_legacy_store(
    root: &Path,
    read: JournalRead<State>,
) -> Result<JournalStatus, MigrationRefusal> {
    let present = legacy_directory_present(root)?;
    let JournalRead::Valid(journal) = read else {
        if present {
            scan_legacy_store(root)?;
        }
        return Ok(JournalStatus::Absent);
    };
    match journal.envelope().state {
        State::Done => {
            if !present || !has_legacy_objects(root)? {
                return Ok(JournalStatus::Done);
            }
            scan_legacy_store(root)?;
            Ok(JournalStatus::PendingReopen)
        }
        State::Running => {
            let journal = journal
                .payload::<MigrationJournal>()
                .map_err(|problem| MigrationRefusal::UnusableJournal(problem.to_string()))?;
            let proof = LegacyProof {
                root,
                identity: journal.legacy_identity,
            };
            if let ProofResult::CantTell(reason) = proof.prove(root) {
                return Err(MigrationRefusal::Other(reason));
            }
            if present {
                scan_legacy_store(root)?;
            }
            // Without taking the store lock, doctor cannot tell whether this run is still active.
            Ok(JournalStatus::Running)
        }
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
        if let JournalRead::Unusable(problem) = &read {
            if !restart {
                return Ok(MigrationOutcome::NeedsHand(
                    MigrationRefusal::UnusableJournal(problem.to_string()),
                ));
            }
            let archive = path.with_extension(format!("{}.json", uuid::Uuid::new_v4()));
            recovery::durable_rename(&path, &archive, false)
                .map_err(|error| file_error(&path, error))?;
            restart = false;
            continue;
        }
        let present = match legacy_directory_present(&root) {
            Ok(present) => present,
            Err(cause) => return Ok(MigrationOutcome::NeedsHand(cause)),
        };
        let journal = match &read {
            JournalRead::Absent => {
                if !present {
                    return Ok(MigrationOutcome::NothingToDo);
                }
                let identity =
                    recovery::path_identity(&root).map_err(|error| file_error(&root, error))?;
                start_running(&root, &path, identity, None)?;
                continue;
            }
            JournalRead::Valid(journal) => {
                let mut journal = journal.payload::<MigrationJournal>().map_err(|error| {
                    migration_error(format!("Could not read {}: {error}", path.display()))
                })?;
                if journal.envelope.state == State::Done {
                    if !present {
                        return Ok(MigrationOutcome::NothingToDo);
                    }
                    match has_legacy_objects(&root) {
                        Ok(true) => {}
                        Ok(false) => return Ok(MigrationOutcome::NothingToDo),
                        Err(cause) => return Ok(MigrationOutcome::NeedsHand(cause)),
                    }
                    let identity =
                        recovery::path_identity(&root).map_err(|error| file_error(&root, error))?;
                    journal.report.sort_findings();
                    start_running(&root, &path, identity, Some(journal.report))?;
                    continue;
                }
                journal
            }
            JournalRead::Unusable(_) => unreachable!("unusable journals are handled above"),
        };
        let proof = LegacyProof {
            root: &root,
            identity: journal.legacy_identity,
        };
        // A previous invocation in this process may have returned an error. The same process
        // now holds the same lock again, so its earlier invocation has already stopped.
        let self_writer = journal.envelope.writer.as_ref().and_then(|writer| {
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
                return migrate(ctx, &root, &path, journal, &capability);
            }
            Decision::Busy(_) => return Ok(MigrationOutcome::Busy),
            Decision::Nothing => return Ok(MigrationOutcome::NothingToDo),
            Decision::NeedsHand(reason) => {
                return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::Other(reason)));
            }
        }
    }
}

fn start_running(
    root: &Path,
    path: &Path,
    identity: Option<PathIdentity>,
    previous_done: Option<MigrationReport>,
) -> LauncherResult<()> {
    let journal = MigrationJournal {
        envelope: Envelope::new(&RESOURCE, State::Running, ownership(root))?,
        legacy_identity: identity,
        report: MigrationReport::default(),
        last_counted: None,
        previous_done,
    };
    recovery::write(path, &journal)?;
    // The journal rename flushes migrations/. Persist its parent directories before unlinking.
    let journal_dir = path.parent().expect("journal has a directory");
    sync_parent(journal_dir)?;
    sync_parent(
        journal_dir
            .parent()
            .expect("journal directory has a parent"),
    )
}

fn legacy_directory_present(root: &Path) -> Result<bool, MigrationRefusal> {
    match legacy_metadata(root)? {
        None => Ok(false),
        Some(entry) if agora_game_api::is_a_reparse_point(&entry) => {
            Err(MigrationRefusal::LegacyLink(root.to_path_buf()))
        }
        Some(entry) if entry.is_dir() => Ok(true),
        Some(_) => Err(MigrationRefusal::Other(format!(
            "{} is not a plain legacy directory.",
            root.display()
        ))),
    }
}

/// A retired store is only reopened when its object layout contains a regular file.
fn has_legacy_objects(root: &Path) -> Result<bool, MigrationRefusal> {
    for shard in legacy_entries(root)? {
        let shard = shard.map_err(|error| legacy_io(root, error))?.path();
        if !is_shard(&shard)
            || !legacy_metadata(&shard)?
                .is_some_and(|entry| entry.is_dir() && !agora_game_api::is_a_reparse_point(&entry))
        {
            continue;
        }
        for file in legacy_entries(&shard)? {
            let file = file.map_err(|error| legacy_io(&shard, error))?.path();
            if object_name(root, &file).is_some()
                && legacy_metadata(&file)?.is_some_and(|entry| {
                    entry.is_file() && !agora_game_api::is_a_reparse_point(&entry)
                })
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn legacy_entries(path: &Path) -> Result<fs::ReadDir, MigrationRefusal> {
    fs::read_dir(path).map_err(|error| legacy_io(path, error))
}

fn legacy_metadata(path: &Path) -> Result<Option<Metadata>, MigrationRefusal> {
    metadata(path).map_err(|error| MigrationRefusal::Other(error.to_string()))
}

fn legacy_io(path: &Path, error: io::Error) -> MigrationRefusal {
    MigrationRefusal::Other(file_error(path, error).to_string())
}

#[derive(Default)]
struct LegacyEntries {
    files: Vec<PathBuf>,
    left_alone: Vec<(PathBuf, &'static str)>,
}

fn scan_legacy_store(root: &Path) -> Result<LegacyEntries, MigrationRefusal> {
    let mut entries = LegacyEntries::default();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in legacy_entries(&directory)? {
            let entry = entry.map_err(|error| legacy_io(&directory, error))?.path();
            let Some(kind) = legacy_metadata(&entry)? else {
                continue;
            };
            if agora_game_api::is_a_reparse_point(&kind) {
                return Err(MigrationRefusal::LegacyLink(entry));
            }
            if kind.is_dir() {
                if directory != root || !is_shard(&entry) {
                    entries
                        .left_alone
                        .push((entry.clone(), "unexpected directory"));
                }
                directories.push(entry);
            } else if kind.is_file() {
                entries.files.push(entry);
            } else {
                entries
                    .left_alone
                    .push((entry, "not a regular object file"));
            }
        }
    }
    Ok(entries)
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
        return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::Other(
            format!(
                "{} changed while its ownership was being checked",
                root.display()
            ),
        )));
    }
    if identity.is_some() {
        let LegacyEntries {
            mut files,
            left_alone,
        } = match scan_legacy_store(root) {
            Ok(entries) => entries,
            Err(cause) => return Ok(MigrationOutcome::NeedsHand(cause)),
        };
        for (entry, reason) in left_alone {
            record_left(&mut journal.report, &entry, reason);
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
            if agora_game_api::is_a_reparse_point(&kind) {
                return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::LegacyLink(
                    file,
                )));
            }
            if !kind.is_file() {
                return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::Other(
                    format!("{} is no longer a plain legacy file", file.display()),
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
                    journal.report.mismatched.push(file.clone());
                }
                if journal.report.mismatch_hashes.get(&file) != Some(&actual) {
                    journal.report.mismatch_hashes.insert(file, actual);
                    recovery::write(path, &journal)?;
                }
                continue;
            }

            let destination = ctx.paths.content_object_path(&actual);
            let present = plain_destination(&ctx.paths, &destination)?.is_some();
            let verified = present
                && super::hash_file_path(&destination)
                    .map_err(|error| file_error(&destination, error))?
                    == actual;
            if verified {
                super::protect::protect(&destination)
                    .map_err(|error| file_error(&destination, error))?;
            } else {
                super::place_object_from_file_locked(&ctx.paths, &file, &actual, present).map_err(
                    |error| migration_error(format!("Could not place {}: {error}", file.display())),
                )?;
            }
            journal.report.mismatched.retain(|path| path != &file);
            journal.report.mismatch_hashes.remove(&file);
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
            let receipt_to_remove = match metadata(&receipt)? {
                Some(entry) if agora_game_api::is_a_reparse_point(&entry) => {
                    return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::LegacyLink(
                        receipt,
                    )));
                }
                Some(entry) if entry.is_file() => Some(&receipt),
                _ => None,
            };
            for legacy_file in receipt_to_remove.into_iter().chain(std::iter::once(&file)) {
                #[cfg(test)]
                ctx.test_seams.snapshot_migration_before_unlink(legacy_file);
                if let Err(error) = check_destination_size(&ctx.paths, &destination, kind.len()) {
                    return Ok(MigrationOutcome::NeedsHand(MigrationRefusal::Other(
                        error.to_string(),
                    )));
                }
                capability.remove_file(root, legacy_file)?;
            }
        }
    }
    journal.envelope.state = State::Done;
    // The terminal journal grants no capability. It stays valid if the data root is relocated.
    journal.envelope.owned.clear();
    journal.legacy_identity = None;
    journal.last_counted = None;
    let previous = journal.previous_done.take();
    if let Some(previous) = &previous {
        for (path, reason) in &previous.left_alone {
            record_left(&mut journal.report, path, reason);
        }
    }
    journal.report.sort_findings();
    let unchanged = if let Some(previous) = previous {
        let unchanged = journal.report.unchanged_since(&previous);
        // The scan refreshes mismatches so a repaired object no longer appears among them.
        // Merge the counts at completion so a resumed pass does not add them twice.
        journal.report.moved += previous.moved;
        journal.report.already_present += previous.already_present;
        journal.report.replaced_damaged += previous.replaced_damaged;
        unchanged
    } else {
        false
    };
    recovery::write(path, &journal)?;
    if unchanged {
        Ok(MigrationOutcome::NothingToDo)
    } else {
        Ok(MigrationOutcome::Done(journal.report))
    }
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
fn plain_destination(paths: &AppPaths, file: &Path) -> LauncherResult<Option<Metadata>> {
    for directory in [
        paths.content_root(),
        paths.content_objects_dir(),
        file.parent().unwrap().to_path_buf(),
    ] {
        match metadata(&directory)? {
            Some(entry) if entry.is_dir() && !agora_game_api::is_a_reparse_point(&entry) => {}
            None => return Ok(None),
            Some(_) => {
                return Err(migration_error(format!(
                    "{} is not a plain content directory",
                    directory.display()
                )))
            }
        }
    }
    match metadata(file)? {
        Some(entry) if entry.is_file() && !agora_game_api::is_a_reparse_point(&entry) => {
            Ok(Some(entry))
        }
        None => Ok(None),
        Some(_) => Err(migration_error(format!(
            "{} is not a plain content object",
            file.display()
        ))),
    }
}

fn check_destination_size(paths: &AppPaths, file: &Path, size: u64) -> LauncherResult<()> {
    if plain_destination(paths, file)?.is_some_and(|entry| entry.len() == size) {
        return Ok(());
    }
    Err(migration_error(format!(
        "{} is missing or changed size before removing the legacy copy (expected {size} bytes)",
        file.display()
    )))
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
