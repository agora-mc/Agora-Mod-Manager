//! One recovery decision, one journal envelope, one capability (MASTER_SPEC §32.11).
//!
//! Agora keeps a crash journal beside every operation that changes files on disk, so an operation
//! cut off by a crash, a power loss or a kill can be finished or undone the next time it starts.
//! Each of those journals grew its own answer to the same three questions — *who owns this?*, *is
//! the writer still running?*, *what may recovery touch?* — and the differences between those
//! answers are where most of the review rounds this month went.
//!
//! This module answers the three questions once:
//!
//! * the **envelope** ([`Envelope`]) is what a journal must say for any of this to work: its format
//!   version, an operation id, its kind, the lock it was written under, the writing process, the
//!   paths it may touch and how each is proven, and a state **from that kind's own enum** — a
//!   plugin install never inherits a launch's states, because [`JournalState`] ties a state enum to
//!   its [`OperationKind`];
//! * the **liveness check** ([`writer_liveness`]) says whether a recorded writer is
//!   [`Alive`](WriterLiveness::Alive), provably [`Gone`](WriterLiveness::Gone) or
//!   [`CantTell`](WriterLiveness::CantTell), and it only *looks*: it never clears a lease, never
//!   removes a file and never rewrites a record, because a question about who owns something must
//!   not change the answer;
//! * the **decision** ([`decide`]) takes the operation's lock with no wait, reads and validates the
//!   journal under it, checks the writer, verifies each owned path's proof, and then either does
//!   nothing ([`Decision::Nothing`]), reports the operation as busy
//!   ([`Decision::Busy`]), asks for a hand ([`Decision::NeedsHand`]) or hands out a
//!   [`RecoveryCapability`] — the only way recovery may act.
//!
//! # The capability
//!
//! A [`RecoveryCapability`] holds the operation's lock for its whole life, and it can only touch a
//! path the journal proved it owns: [`remove_dir`](RecoveryCapability::remove_dir),
//! [`rename`](RecoveryCapability::rename) and [`restore`](RecoveryCapability::restore) refuse any
//! other path at runtime, with `ERR_RECOVERY_REFUSED`. A recovery handler therefore cannot delete a
//! folder the journal did not name, which is the mistake that costs a user their files.
//!
//! The capability acts on what it *verified*, not on what the journal said a moment ago: before
//! each action it re-checks the marker and the OS identity of the path (volume and file id on
//! Windows, device and inode on Unix), so a folder removed and recreated under the same name, or a
//! marker swapped for another operation's, is refused and reported. And
//! [`restore`](RecoveryCapability::restore) never deletes what it replaces: the content is moved to
//! a sibling named after the operation, then the stashed copy takes the path, so a restore that
//! turns out to be wrong loses nothing.
//!
//! # What is deliberately not here yet
//!
//! Only the plugin install writes an envelope so far; the other journals move onto it one at a time
//! (MASTER_SPEC §32.11, D1), each keeping its proven recovery logic behind the capability. A
//! journal that is a legacy one — no envelope, or nothing that proves who wrote it — is
//! [`Decision::NeedsHand`], never recovered automatically.

use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::{LockGuard, LockManager, LockProbe, LockRefusal, LockResource};
use crate::process_identity::{self, ProcessIdentity};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The envelope format this build writes and reads.
///
/// A journal naming a higher number was written by a newer Agora, which may have changed what a
/// state means: it is kept, and reported as needing a hand, never read as this version.
pub const ENVELOPE_FORMAT: u32 = 1;

/// The JSON key a marker file carries the operation id under.
const MARKER_OPERATION_KEY: &str = "operation_id";

/// The lock name recovery takes for its own work. Not a resource: it is what the lock file's owner
/// record says while a recovery holds the operation's lock.
const RECOVERY_OPERATION: &str = "recovery";

// ---------------------------------------------------------------------------
// Operation kinds and their states
// ---------------------------------------------------------------------------

/// The kind of operation a journal belongs to.
///
/// One variant per journal that writes an envelope. The others (tool swaps, user-files sessions,
/// Recently deleted restores, snapshot restores, launcher imports and the database's own marker)
/// still write their own shapes and move here one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    /// Installing, updating or uninstalling a plugin package under the plugin-install lock.
    PluginInstall,
}

impl OperationKind {
    /// The kind's stable name, as the journal writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            OperationKind::PluginInstall => "plugin-install",
        }
    }
}

/// A state a journal of one kind can be in.
///
/// Each kind has its own enum, so a state can only be read back as the kind that wrote it: a file
/// swap does not inherit process-launch states, and a journal carrying another kind's state is not
/// read as this one.
pub trait JournalState: Serialize + DeserializeOwned {
    /// The kind this state belongs to.
    const KIND: OperationKind;
}

// ---------------------------------------------------------------------------
// Ownership proofs
// ---------------------------------------------------------------------------

/// How the envelope proves that a path belongs to the operation that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "kebab-case")]
pub enum PathProof {
    /// A marker file the writer created before it touched the path, holding
    /// `{"format": 1, "operation_id": "…"}` ([`write_marker`]).
    ///
    /// [`decide`] reads it: present and naming this operation is ownership; absent, unreadable or
    /// naming another operation is not. [`RecoveryCapability::finish`] removes it.
    Marker {
        /// Where the marker is, next to or inside the path it proves.
        at: PathBuf,
    },
    /// A marker and the identities captured by the writer before moving folders.
    /// Several identities can occupy a destination across a recoverable rename.
    MarkedIdentity {
        /// The marker shared by the operation's paths.
        at: PathBuf,
        /// Identities allowed at this path when it is occupied.
        identities: Vec<PathIdentity>,
    },
    /// The path is a folder whose own record (its manifest, a marker inside it) names what this
    /// journal recorded. Only the feature knows where that record is, so it verifies this proof
    /// itself through [`ContentProof`].
    Content,
}

/// A path the journal says it owns, and how that is proven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedPath {
    /// The path recovery may touch, if the proof matches.
    pub path: PathBuf,
    /// How ownership of [`Self::path`] is proven.
    pub proof: PathProof,
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// What every crash journal says, whatever operation it belongs to (MASTER_SPEC §32.11).
///
/// The feature's own payload lives in its own struct, beside an [`Envelope`] of its state type:
///
/// ```ignore
/// #[derive(Serialize, Deserialize)]
/// struct PluginInstallJournal {
///     envelope: Envelope<PluginInstallState>,
///     previous_version: Option<String>,
///     new_version: String,
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<S> {
    /// [`ENVELOPE_FORMAT`] when the writer wrote it.
    pub format: u32,
    /// Names this operation: every marker and every message uses it.
    pub operation_id: String,
    /// The operation's kind. Must be [`JournalState::KIND`] of `S`.
    pub kind: OperationKind,
    /// The lock the writer held for the whole operation, by its name
    /// ([`LockResource::lock_name`]).
    ///
    /// The name rather than the Rust variant, so renaming a variant is not a silent change to what
    /// is on disk; it is also the value [`decide`] compares against the resource it locked.
    pub lock: String,
    /// The process that wrote the journal. `None` — a legacy journal, or a capture that failed —
    /// means liveness cannot be checked, and [`decide`] will not act on it automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<ProcessIdentity>,
    /// The paths recovery may touch, each with its proof.
    #[serde(default)]
    pub owned: Vec<OwnedPath>,
    /// Where the operation had got to, from this kind's own states.
    pub state: S,
}

impl<S: JournalState> Envelope<S> {
    /// A new envelope in `state`, under `lock`, owned by the calling process.
    ///
    /// Written with [`write`] before the operation touches anything.
    pub fn new(lock: &LockResource, state: S, owned: Vec<OwnedPath>) -> LauncherResult<Self> {
        Ok(Envelope {
            format: ENVELOPE_FORMAT,
            operation_id: uuid::Uuid::new_v4().to_string(),
            kind: S::KIND,
            lock: lock.lock_name()?,
            writer: process_identity::capture_self(),
            owned,
            state,
        })
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Write a journal atomically: a sibling temporary file, flushed, renamed into place, and the
/// directory entry flushed where the platform supports it.
///
/// A journal is what makes an operation recoverable, so it is never half-written: a crash leaves
/// either the previous journal or the new one.
///
/// `journal` is either an [`Envelope`] or a kind's own file with an envelope flattened into it —
/// [`read`] and [`decide`] look for the envelope's fields at the top level.
pub fn write<T: Serialize>(path: &Path, journal: &T) -> LauncherResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| journal_error(format!("could not create {}: {e}", parent.display())))?;
    }
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|e| journal_error(format!("could not write the journal: {e}")))?;
    let tmp = temporary_path(path);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(journal_error(format!(
            "could not write the journal {}: {error}",
            path.display()
        )));
    }
    durable_rename(&tmp, path, true).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        journal_error(format!(
            "could not publish the journal {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

/// The temporary file a journal is built in before it is published.
fn temporary_path(path: &Path) -> PathBuf {
    let mut name = match path.file_name() {
        Some(name) => name.to_os_string(),
        None => std::ffi::OsString::from("journal"),
    };
    name.push(".tmp");
    path.with_file_name(name)
}

/// Write the marker file that proves an operation owns a path.
pub fn write_marker(path: &Path, operation_id: &str) -> LauncherResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| journal_error(format!("could not create {}: {e}", parent.display())))?;
    }
    let body = serde_json::json!({ "format": ENVELOPE_FORMAT, MARKER_OPERATION_KEY: operation_id });
    write(path, &body)
}

pub(crate) fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Move a directory and persist the changed entries before advancing a journal.
pub(crate) fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    durable_rename(from, to, false)
}

fn durable_rename(from: &Path, to: &Path, replace: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let wide = |path: &Path| -> std::io::Result<Vec<u16>> {
            Ok(std::path::absolute(path)?
                .as_os_str()
                .encode_wide()
                .chain([0])
                .collect())
        };
        let source = wide(from)?;
        let destination = wide(to)?;
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both paths are nul-terminated and live through the call.
        if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    {
        let _ = replace;
        std::fs::rename(from, to)?;
    }
    sync_parent(to)?;
    if from.parent() != to.parent() {
        sync_parent(from)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// What reading a journal found.
#[derive(Debug, Clone)]
pub enum JournalRead<S> {
    /// There is no journal at the path: the operation was not started, or it finished and deleted
    /// its journal. Nothing to recover.
    Absent,
    /// A journal this build can act on, after its envelope was validated.
    Valid(Box<Journal<S>>),
    /// A journal is there, but not one this build can act on. Kept for diagnosis; the reason says
    /// which of the cases below it is.
    Unusable(JournalProblem),
}

/// A validated journal: the envelope every operation shares, plus the document as it was read.
///
/// The document is kept so a feature can read its own fields from the bytes that were already
/// validated, rather than opening the file a second time — a second read is a second chance for
/// the file to have changed since the decision.
#[derive(Debug, Clone)]
pub struct Journal<S> {
    envelope: Envelope<S>,
    document: serde_json::Value,
}

impl<S> Journal<S> {
    /// The envelope the decision was made from.
    pub fn envelope(&self) -> &Envelope<S> {
        &self.envelope
    }

    /// This kind's own fields, read from the document the decision validated.
    pub fn payload<T: DeserializeOwned>(&self) -> Result<T, JournalProblem> {
        serde_json::from_value(self.document.clone()).map_err(|error| {
            JournalProblem::Malformed(format!("its own fields are not readable ({error})"))
        })
    }
}

/// Why a journal that exists cannot be acted on. Each case is distinct because each is a different
/// thing to tell the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalProblem {
    /// The journal could not be read at all: a permission denial, a sharing violation, a directory
    /// at the path. Not the same as absent, and never acted on.
    CantTell(String),
    /// The file is not a journal this build understands: not JSON, missing the envelope's fields,
    /// or a format number that is not one.
    Malformed(String),
    /// A journal from a newer Agora, which may give its states meanings this build does not know.
    NewerFormat {
        /// The format the file names.
        found: u64,
    },
    /// A journal of another kind: a plugin install's file where a tool swap's journal was expected,
    /// or the reverse.
    OtherKind {
        /// The kind the file names.
        found: OperationKind,
    },
}

impl std::fmt::Display for JournalProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalProblem::CantTell(reason) => write!(f, "it could not be read ({reason})"),
            JournalProblem::Malformed(reason) => {
                write!(f, "it is not a journal Agora understands ({reason})")
            }
            JournalProblem::NewerFormat { found } => write!(
                f,
                "it was written by a newer Agora (format {found}, this one reads {ENVELOPE_FORMAT})"
            ),
            JournalProblem::OtherKind { found } => {
                write!(f, "it is a journal of another kind ({})", found.as_str())
            }
        }
    }
}

/// Read and validate a journal (MASTER_SPEC §32.13: absent, can't tell, malformed and newer are all
/// distinct, and only "not found" is absent).
///
/// Read this under the operation's lock — [`decide`] does.
pub fn read<S: JournalState>(path: &Path) -> JournalRead<S> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return JournalRead::Absent,
        Err(error) => {
            return JournalRead::Unusable(JournalProblem::CantTell(format!(
                "{} could not be read: {error}",
                path.display()
            )))
        }
    };
    read_document(&bytes, &path.display().to_string())
}

/// Validate a journal document read from any store.
pub(crate) fn read_document<S: JournalState>(bytes: &[u8], location: &str) -> JournalRead<S> {
    let value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{location} is not JSON: {error}"
            )))
        }
    };
    match value.get("format").and_then(serde_json::Value::as_u64) {
        Some(format) if format > u64::from(ENVELOPE_FORMAT) => {
            return JournalRead::Unusable(JournalProblem::NewerFormat { found: format })
        }
        Some(format) if format < u64::from(ENVELOPE_FORMAT) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{location} names format {format}, which is not one Agora wrote"
            )))
        }
        Some(_) => {}
        None => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{location} names no format version"
            )))
        }
    }
    let envelope: Envelope<S> = match serde_json::from_value(value.clone()) {
        Ok(envelope) => envelope,
        Err(error) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{location} is not a complete journal: {error}"
            )))
        }
    };
    if envelope.kind != S::KIND {
        return JournalRead::Unusable(JournalProblem::OtherKind {
            found: envelope.kind,
        });
    }
    if envelope.operation_id.is_empty() {
        return JournalRead::Unusable(JournalProblem::Malformed(format!(
            "{location} names no operation"
        )));
    }
    JournalRead::Valid(Box::new(Journal {
        envelope,
        document: value,
    }))
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/// What is known about a journal's recorded writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterLiveness {
    /// The recorded process is still the same live process: it is doing the operation.
    Alive,
    /// The recorded process is provably not running: its pid is free, or the pid now belongs to
    /// another process.
    Gone,
    /// The record cannot answer the question: no writer was recorded, the identity is unusable, or
    /// the operating system's process table could not be read. Never treated as gone.
    CantTell(String),
}

/// Whether a journal's recorded writer is still running. Read-only: it looks at the process table
/// and nothing else — it never clears a lease, never removes a file and never writes a record.
///
/// A question about who owns something must not change the answer, which is why the mutation path's
/// stale-lease clearing ([`crate::instance_runtime`]) is deliberately not reused here. `view` is
/// the process table the answer comes from; production passes
/// [`OsProcesses`](crate::process_identity::OsProcesses), and a test passes a stand-in so each
/// source of "could not tell" can be made to happen (MASTER_SPEC §32.13).
pub fn writer_liveness(
    writer: Option<&ProcessIdentity>,
    view: &dyn crate::process_identity::ProcessView,
) -> WriterLiveness {
    let Some(identity) = writer else {
        return WriterLiveness::CantTell("the journal records no writing process".into());
    };
    match process_identity::check_with(identity, view) {
        process_identity::ProcessCheck::Alive => WriterLiveness::Alive,
        process_identity::ProcessCheck::Gone => WriterLiveness::Gone,
        // Never read as gone: an answer the OS did not give is not proof that the writer stopped.
        process_identity::ProcessCheck::CantTell(why) => WriterLiveness::CantTell(why),
    }
}

// ---------------------------------------------------------------------------
// Verified identities
// ---------------------------------------------------------------------------

/// What the operating system said one path *is*, so recovery can tell later whether it is still
/// the same folder (MASTER_SPEC §32.11).
///
/// A proof answers "is this the operation's?" when the decision is made. A path can still be
/// swapped between the decision and the action — a folder deleted and another created in its place
/// keeps the same name. Acting only on what the decision verified closes that window: the identity
/// is read again immediately before anything is touched, and a path that is no longer the same one
/// is refused and reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathIdentity {
    /// The volume (or device) and file (or inode) the OS reported for the path.
    File {
        /// The volume serial (Windows) or device id (Unix).
        volume: u64,
        /// The file index (Windows) or inode (Unix).
        file: u64,
    },
}

/// What is at `path`: `Ok(None)` when nothing is, so a path that has never existed is not an
/// error; `Err` when the OS would not say, which is never read as "nothing is there".
pub(crate) fn path_identity(path: &Path) -> std::io::Result<Option<PathIdentity>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => metadata,
            Ok(_) => return Err(std::io::Error::other("expected a plain directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(PathIdentity::File {
            volume: metadata.dev(),
            file: metadata.ino(),
        }))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        // A directory can only be opened by handle with backup semantics; the handle is what names
        // the file on disk, whatever its name is now.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `file` is open for the call, and `info` is a valid out-parameter.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(std::io::Error::other("expected a plain directory"));
        }
        Ok(Some(PathIdentity::File {
            volume: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        }))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::other(
            "this platform reports no file identity".to_string(),
        ))
    }
}

/// A path whose proof matched when the decision was made, with what the OS said it was then.
#[derive(Debug, Clone)]
struct VerifiedPath {
    path: PathBuf,
    /// `None` when the path did not exist at decision time. A path that exists *now* but did not
    /// then was not verified and is not acted on.
    identity: Option<PathIdentity>,
    /// The marker that proved it, when the proof was a marker: re-checked before every action, so a
    /// marker removed or replaced in between refuses the action.
    marker: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// What a per-kind proof of a folder's content says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofResult {
    /// The folder is the copy the journal recorded (its own record names what the journal says).
    Proven,
    /// The folder is provably not this operation's: another plugin, another version, another
    /// operation's marker. Left alone and reported, never touched.
    NotOwned(String),
    /// The folder's own record could not be read, so nothing is proven either way.
    CantTell(String),
}

/// The proof of a folder's content, checked by the feature that knows what its record is
/// ([`PathProof::Content`]).
pub trait ContentProof {
    /// Whether `path` is provably the copy this operation recorded.
    fn prove(&self, path: &Path) -> ProofResult;
}

/// For a kind whose owned paths are all proven by markers.
///
/// A journal that names a [`PathProof::Content`] path gives it to this, which answers "could not
/// tell" — so that path is neither touched nor assumed, and the decision needs a hand.
pub struct NoContentProof;

impl ContentProof for NoContentProof {
    fn prove(&self, path: &Path) -> ProofResult {
        ProofResult::CantTell(format!(
            "this build cannot check what {} holds, so it cannot prove the folder is the one the \
             operation wrote",
            path.display()
        ))
    }
}

/// What [`decide`] found, and the only things a recovery handler may do next.
#[derive(Debug)]
pub enum Decision<'a, S> {
    /// No journal: the operation was not started, or it finished. Nothing to do, nothing to report.
    Nothing,
    /// The operation's lock is held right now — another Agora may be in the middle of it. A held
    /// lock is busy, not necessarily alive: report it and try again next start. Nothing was touched.
    Busy(String),
    /// Something about this journal cannot be decided safely, with the reason and what a person can
    /// do. Nothing was touched, and the journal is kept for diagnosis.
    NeedsHand(String),
    /// Recovery may act, through the capability, on exactly the paths the journal proved it owns.
    Act(Box<RecoveryCapability<'a, S>>),
}

/// What recovery would do about one journal, as a report states it (MASTER_SPEC §32.11).
///
/// `agora doctor` prints one of these per journal it found. The two "would" answers are the
/// feature's own direction, read from its state enum; the rest are the decision's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryOutcome {
    /// No journal, or one that is already settled: nothing to do.
    NothingToDo,
    /// Recovery would finish the operation: roll forward through its cleanup.
    WouldFinish,
    /// Recovery would undo the operation: put what it replaced back.
    WouldUndo,
    /// The operation's lock is held, or its writer may still be running, so recovery would do
    /// nothing at all. Report and wait for the next start.
    Busy,
    /// Recovery needs a person, with the reason.
    NeedsHand,
    /// This build cannot say: the journal's kind does not share the recovery decision yet (D1).
    Undecided,
}

impl RecoveryOutcome {
    /// The stable name a report and its JSON share.
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryOutcome::NothingToDo => "nothingToDo",
            RecoveryOutcome::WouldFinish => "wouldFinish",
            RecoveryOutcome::WouldUndo => "wouldUndo",
            RecoveryOutcome::Busy => "busy",
            RecoveryOutcome::NeedsHand => "needsHand",
            RecoveryOutcome::Undecided => "undecided",
        }
    }
}

/// What [`decide_readonly`] found: the same answer [`decide`] gives, without the capability.
#[derive(Debug)]
pub enum ReadOnlyDecision<S> {
    /// No journal: nothing to do, nothing to report.
    Nothing,
    /// The operation's lock is held — or could not be asked about — or its recorded writer may
    /// still be running.
    Busy(String),
    /// Something about this journal cannot be decided safely, with the reason.
    NeedsHand(String),
    /// Recovery *would* act on this journal, through the capability [`decide`] would hand out. The
    /// journal is the one that was validated, with the paths whose proof did not match: recovery
    /// leaves those alone, and a handler refuses to act while any is there. Nothing here can act.
    WouldAct(Box<Journal<S>>, Vec<(PathBuf, String)>),
}

/// The reason a handler gives when it cannot tell whether the writer is still running.
fn liveness_hand(reason: &str) -> String {
    format!(
        "The journal cannot say whether the operation is still running: {reason}. Nothing was \
         changed; the journal was kept."
    )
}

/// Who holds a lock, in words for a report.
fn holder_text(
    resource: &LockResource,
    metadata: Option<&crate::lock_manager::LockMetadata>,
) -> String {
    let name = resource.lock_name().unwrap_or_else(|_| "operation".into());
    match metadata {
        Some(metadata) => format!(
            "Another Agora holds the '{}' lock right now ({} on {}, since {}, operation: {}), so \
             the operation may still be running. Nothing was changed.",
            name,
            if metadata.program.is_empty() {
                format!("PID {}", metadata.pid)
            } else {
                format!("the {}, PID {}", metadata.program, metadata.pid)
            },
            metadata.hostname,
            metadata.acquired_at,
            metadata.operation,
        ),
        None => format!(
            "Another Agora holds the '{}' lock right now and its lock file has no readable owner \
             record, so the operation may still be running. Nothing was changed.",
            name
        ),
    }
}

/// Decide what to do about one operation's journal (MASTER_SPEC §32.11).
///
/// 1. the operation's lock, with no wait: held means [`Decision::Busy`]; a lock that could not be
///    asked for at all means [`Decision::NeedsHand`] (nothing was learned about any holder);
/// 2. the journal, read and validated *under* that lock: absent means [`Decision::Nothing`], and
///    anything that is not a valid journal of this kind, under this lock, means
///    [`Decision::NeedsHand`] — the file is kept;
/// 3. the recorded writer: [`WriterLiveness::Alive`] means busy, `CantTell` means a hand, `Gone`
///    means carry on;
/// 4. each owned path's proof: a path whose proof does not match is left alone and reported, a path
///    whose proof cannot be *checked* means a hand.
///
/// The lock manager rather than the whole context: the decision uses nothing else, and the one
/// startup pass that recovers plugin installs runs inside `CoreContext::initialize`, before the
/// context it is building exists. `view` is the process table the writer's liveness is read from,
/// so a test can make each "could not tell" happen.
pub fn decide<S: JournalState>(
    locks: &LockManager,
    resource: &LockResource,
    journal_path: &Path,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
) -> Decision<'static, S> {
    decide_reading(
        locks,
        resource,
        Some(journal_path),
        || read(journal_path),
        proof,
        view,
    )
}

pub(crate) fn decide_reading<S: JournalState>(
    locks: &LockManager,
    resource: &LockResource,
    journal_path: Option<&Path>,
    read: impl FnOnce() -> JournalRead<S>,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
) -> Decision<'static, S> {
    let lock = match locks.acquire_with_timeout_typed(
        resource.clone(),
        RECOVERY_OPERATION,
        Duration::ZERO,
        None,
    ) {
        Ok(lock) => lock,
        // Someone holds it: possibly a live operation, possibly one of ours that died with the
        // handle still recorded. Either way recovery does not act underneath it.
        Err(LockRefusal::Held(error)) => return Decision::Busy(error.to_string()),
        // The lock could not be taken at all (a lock directory that is not a directory, a name that
        // is not a lock name): nothing was learned about any holder.
        Err(LockRefusal::Unavailable(error)) => return Decision::NeedsHand(error.to_string()),
    };
    match examine(resource, read(), proof, view, None) {
        Examined::Nothing => Decision::Nothing,
        Examined::Busy(why) => Decision::Busy(why),
        // Acting on "could not tell" is worse than not acting: the handler gets a hand.
        Examined::CantTellLiveness(why) => Decision::NeedsHand(liveness_hand(&why)),
        Examined::NeedsHand(why) => Decision::NeedsHand(why),
        Examined::Ready(ready) => Decision::Act(Box::new(RecoveryCapability::under(
            LockHold::Owned(lock),
            journal_path,
            ready,
        ))),
    }
}

/// [`decide`] for a caller that must not act and must not change anything: `agora doctor`.
///
/// The same questions, in the same order, with the same validation and the same liveness check —
/// and no capability, because there is nothing here to act with. It differs in exactly two ways,
/// both because a report may not touch what it reports on:
///
/// * the operation's lock is **probed**, not taken: a held lock is [`ReadOnlyDecision::Busy`], and
///   the lock file, its directory and its owner record are left exactly as they were;
/// * a lock that cannot be asked at all is also [`ReadOnlyDecision::Busy`]: a journal whose owner
///   cannot be asked about may still be running, and "may still be running" is what busy says.
///
/// A writer that may still be alive is busy; a liveness answer of "could not tell" is busy too, for
/// the same reason — never [`ReadOnlyDecision::WouldAct`]. A caller that must *act* turns "could not
/// tell" into a hand instead ([`Examined::CantTellLiveness`]), because acting on an unknown is
/// worse than not acting; a report only has to be honest that the operation may be running. `view`
/// is the process table the writer's liveness is read from.
pub fn decide_readonly<S: JournalState>(
    locks: &LockManager,
    resource: &LockResource,
    read: impl FnOnce() -> JournalRead<S>,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
) -> ReadOnlyDecision<S> {
    match locks.probe(resource) {
        LockProbe::Held(metadata) => {
            ReadOnlyDecision::Busy(holder_text(resource, metadata.as_ref()))
        }
        LockProbe::CantTell(why) => ReadOnlyDecision::Busy(format!(
            "Agora could not tell whether another operation holds this journal's lock, so the \
             operation may still be running: {why}. Nothing was changed."
        )),
        LockProbe::Free => match examine(resource, read(), proof, view, None) {
            Examined::Nothing => ReadOnlyDecision::Nothing,
            Examined::Busy(why) => ReadOnlyDecision::Busy(why),
            // The operation may still be running, which is what a report says "busy" for.
            Examined::CantTellLiveness(why) => ReadOnlyDecision::Busy(format!(
                "The journal cannot say whether the operation is still running, so it may still be \
                 going: {why}. Nothing was changed."
            )),
            Examined::NeedsHand(why) => ReadOnlyDecision::NeedsHand(why),
            // The journal is valid, its writer is gone and its paths are proven, so [`decide`]
            // would hand out a capability. This caller gets what the decision learned — the
            // journal and the paths whose proof did not match — and nothing it can act with.
            Examined::Ready(ready) => ReadOnlyDecision::WouldAct(ready.journal, ready.left_alone),
        },
    }
}

/// [`decide`] for a caller that already holds the operation's lock.
///
/// A running operation asks this before it starts its own work — after taking the lock, before it
/// writes anything — so an unresolved journal from a crash is finished or undone first, instead of
/// being overwritten by the new journal. It cannot answer [`Decision::Busy`]: the lock is already
/// this caller's, so nobody else is in the middle of the same operation.
pub fn decide_holding<'a, S: JournalState>(
    lock: &'a LockGuard,
    resource: &LockResource,
    journal_path: &Path,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
) -> Decision<'a, S> {
    decide_holding_reading(
        lock,
        resource,
        Some(journal_path),
        || read(journal_path),
        proof,
        view,
        None,
    )
}

pub(crate) fn decide_holding_reading<'a, S: JournalState>(
    lock: &'a LockGuard,
    resource: &LockResource,
    journal_path: Option<&Path>,
    read: impl FnOnce() -> JournalRead<S>,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
    writer_operation: Option<&str>,
) -> Decision<'a, S> {
    match examine(resource, read(), proof, view, writer_operation) {
        Examined::Nothing => Decision::Nothing,
        // This caller holds the lock, so no live writer can be working on the same operation:
        // the record contradicts the lock, which is a hand, not permission.
        Examined::Busy(why) => Decision::NeedsHand(format!(
            "{why} This Agora holds the operation's lock, so the journal's record of its writer \
             does not agree with the lock; nothing was changed and the journal was kept."
        )),
        Examined::NeedsHand(why) => Decision::NeedsHand(why),
        Examined::CantTellLiveness(why) => Decision::NeedsHand(liveness_hand(&why)),
        Examined::Ready(ready) => Decision::Act(Box::new(RecoveryCapability::under(
            LockHold::Borrowed(lock),
            journal_path,
            ready,
        ))),
    }
}

/// What one examination of a journal found, before it is wrapped in a capability.
enum Examined<S> {
    Nothing,
    /// The recorded writer is still running. For a caller that took the lock this is
    /// [`Decision::Busy`]; a caller that already held it treats it as a hand, because its own lock
    /// rules out a live writer and the record therefore contradicts itself.
    Busy(String),
    /// The journal cannot say whether its writer is still running. A caller that must act needs a
    /// hand; a report says busy, because the operation may be running.
    CantTellLiveness(String),
    NeedsHand(String),
    Ready(Ready<S>),
}

/// A journal that may be acted on, with what the examination verified.
struct Ready<S> {
    journal: Box<Journal<S>>,
    owned: Vec<VerifiedPath>,
    left_alone: Vec<(PathBuf, String)>,
    markers: Vec<PathBuf>,
}

/// Read and validate one journal, check its writer and prove each path it claims. Nothing is
/// touched; the lock is the caller's business (see [`decide`] and [`decide_holding`]).
fn examine<S: JournalState>(
    resource: &LockResource,
    journal_read: JournalRead<S>,
    proof: &dyn ContentProof,
    view: &dyn crate::process_identity::ProcessView,
    writer_operation: Option<&str>,
) -> Examined<S> {
    let journal = match journal_read {
        JournalRead::Absent => return Examined::Nothing,
        JournalRead::Unusable(problem) => {
            return Examined::NeedsHand(format!(
                "The journal for this operation cannot be used: {problem}."
            ))
        }
        JournalRead::Valid(journal) => journal,
    };
    let envelope = journal.envelope();

    if let Ok(name) = resource.lock_name() {
        if envelope.lock != name {
            return Examined::NeedsHand(format!(
                "The journal (operation {}) says it was written under the '{}' lock, not '{}'. \
                 Nothing was changed; the journal was kept.",
                envelope.operation_id, envelope.lock, name
            ));
        }
    }

    // The writer holding the lock can authorize actions for its own operation.
    let liveness = if writer_operation == Some(envelope.operation_id.as_str()) {
        WriterLiveness::Gone
    } else {
        writer_liveness(envelope.writer.as_ref(), view)
    };
    match liveness {
        WriterLiveness::Alive => {
            let pid = match envelope.writer.as_ref() {
                Some(writer) => writer.pid.to_string(),
                None => "unknown".to_string(),
            };
            return Examined::Busy(format!(
                "The process that wrote the journal (pid {pid}) is still running, so the operation \
                 may still be going. Nothing was changed."
            ));
        }
        WriterLiveness::CantTell(reason) => return Examined::CantTellLiveness(reason),
        WriterLiveness::Gone => {}
    }

    let mut owned = Vec::new();
    let mut left_alone = Vec::new();
    let mut markers = Vec::new();
    for entry in &envelope.owned {
        let marker = match &entry.proof {
            PathProof::Marker { at } | PathProof::MarkedIdentity { at, .. } => {
                match marker_verdict(at, &envelope.operation_id) {
                    MarkerVerdict::Ours => {
                        markers.push(at.clone());
                        Some(at.clone())
                    }
                    MarkerVerdict::NotOurs(why) => {
                        left_alone.push((entry.path.clone(), why));
                        continue;
                    }
                    MarkerVerdict::CantTell(why) => {
                        return Examined::NeedsHand(format!(
                        "A path this journal owns cannot be checked: {why}. Nothing was changed; \
                         the journal was kept."
                    ))
                    }
                }
            }
            PathProof::Content => match proof.prove(&entry.path) {
                ProofResult::Proven => None,
                ProofResult::NotOwned(why) => {
                    left_alone.push((entry.path.clone(), why));
                    continue;
                }
                ProofResult::CantTell(why) => {
                    return Examined::NeedsHand(format!(
                        "A path this journal owns cannot be checked: {why}. Nothing was changed; \
                         the journal was kept."
                    ))
                }
            },
        };
        // What the path *is*, not only what the marker says: recovery re-checks this before it
        // acts, so a folder swapped for another one between the decision and the action is
        // refused rather than replaced (MASTER_SPEC §32.11).
        let identity = match path_identity(&entry.path) {
            Ok(identity) => identity,
            Err(error) => {
                return Examined::NeedsHand(format!(
                    "A path this journal owns cannot be identified, so Agora cannot tell later \
                     whether it is still the same one: {error}. Nothing was changed; the journal \
                     was kept."
                ))
            }
        };
        if let PathProof::MarkedIdentity { identities, .. } = &entry.proof {
            if identity.is_some_and(|identity| !identities.contains(&identity)) {
                left_alone.push((
                    entry.path.clone(),
                    "its directory identity differs from the journal".into(),
                ));
                continue;
            }
        }
        owned.push(VerifiedPath {
            path: entry.path.clone(),
            identity,
            marker,
        });
    }

    Examined::Ready(Ready {
        journal,
        owned,
        left_alone,
        markers,
    })
}

/// What a marker file says about ownership.
enum MarkerVerdict {
    Ours,
    NotOurs(String),
    CantTell(String),
}

/// Whether the marker at `at` names this operation.
fn marker_verdict(at: &Path, operation_id: &str) -> MarkerVerdict {
    let bytes = match std::fs::read(at) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MarkerVerdict::NotOurs(format!(
                "{} holds no marker of this operation, so it is not provably this operation's",
                at.display()
            ))
        }
        Err(error) => {
            return MarkerVerdict::CantTell(format!(
                "the marker {} could not be read: {error}",
                at.display()
            ))
        }
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return MarkerVerdict::NotOurs(format!(
                "the marker {} is not a marker Agora wrote ({error})",
                at.display()
            ))
        }
    };
    match value
        .get(MARKER_OPERATION_KEY)
        .and_then(serde_json::Value::as_str)
    {
        Some(found) if found == operation_id => MarkerVerdict::Ours,
        Some(found) => MarkerVerdict::NotOurs(format!(
            "{} belongs to another operation ({found})",
            at.display()
        )),
        None => MarkerVerdict::NotOurs(format!("the marker {} names no operation", at.display())),
    }
}

// ---------------------------------------------------------------------------
// The capability
// ---------------------------------------------------------------------------

/// Permission to act on exactly what one journal proves the operation owns (MASTER_SPEC §32.11).
///
/// It holds the operation's lock for its whole life, so no other Agora can start the same operation
/// while recovery is under way, and its three operations refuse any path the journal did not prove.
/// Recovery handlers act only through this: they cannot delete a folder the journal did not name,
/// which is the mistake that costs a user their files.
///
/// Two more refusals sit inside it. Before each action the marker and the OS identity of the path
/// are checked again, so a folder swapped for another one, or a marker replaced, between the
/// decision and the action is refused rather than replaced. And the three operations take
/// `&mut self`: an action that succeeded updates what is recorded for the path it moved, so the
/// next action is checked against the state this capability itself left.
#[derive(Debug)]
pub struct RecoveryCapability<'a, S> {
    /// Held for the capability's whole life; dropped last.
    lock: LockHold<'a>,
    /// The journal this capability was issued from, deleted by [`Self::finish`].
    journal_path: Option<PathBuf>,
    /// The journal as it was read and validated: the one read the decision made.
    journal: Box<Journal<S>>,
    /// The paths whose proof matched, with what the OS said they were then: the only paths the
    /// operations below accept.
    owned: Vec<VerifiedPath>,
    /// The paths whose proof did *not* match, with the reason. Never touched, always reported.
    left_alone: Vec<(PathBuf, String)>,
    /// The marker files this operation wrote; removed with the journal.
    markers: Vec<PathBuf>,
}

/// The lock a capability acts under: one it took itself, or the one a caller already held when it
/// asked ([`decide_holding`]) — a running operation asks under its own lock before it acts.
#[derive(Debug)]
enum LockHold<'a> {
    Owned(LockGuard),
    Borrowed(&'a LockGuard),
}

impl<'a, S> RecoveryCapability<'a, S> {
    /// The capability for one examined journal, under `hold`.
    fn under(hold: LockHold<'a>, journal_path: Option<&Path>, ready: Ready<S>) -> Self {
        RecoveryCapability {
            lock: hold,
            journal_path: journal_path.map(Path::to_path_buf),
            journal: ready.journal,
            owned: ready.owned,
            left_alone: ready.left_alone,
            markers: ready.markers,
        }
    }

    /// The id of the operation this capability was issued for.
    pub fn operation_id(&self) -> &str {
        &self.journal.envelope.operation_id
    }

    /// The state the journal recorded.
    pub fn state(&self) -> &S {
        &self.journal.envelope.state
    }

    /// This kind's own fields, read from the document the decision validated — never a second
    /// read of the file, which could have changed since.
    pub fn payload<T: DeserializeOwned>(&self) -> LauncherResult<T> {
        self.journal.payload::<T>().map_err(|problem| {
            journal_error(format!(
                "the journal of operation {} is not readable in a way this build understands: \
                 {problem}",
                self.operation_id()
            ))
        })
    }

    /// The paths this operation proved it owns. These, and no others, are accepted below.
    pub fn owned(&self) -> Vec<&Path> {
        self.owned
            .iter()
            .map(|owned| owned.path.as_path())
            .collect()
    }

    /// The paths the journal claimed but could not prove, with the reason: left alone, and to be
    /// reported to the user.
    pub fn left_alone(&self) -> &[(PathBuf, String)] {
        &self.left_alone
    }

    /// The lock this capability acts under, for a call that must pass it on rather than re-take it.
    pub fn lock(&self) -> &LockGuard {
        match &self.lock {
            LockHold::Owned(lock) => lock,
            LockHold::Borrowed(lock) => lock,
        }
    }

    /// Where the journal this capability came from lives.
    pub fn journal_path(&self) -> Option<&Path> {
        self.journal_path.as_deref()
    }

    /// Remove an owned folder.
    pub fn remove_dir(&mut self, path: &Path) -> LauncherResult<()> {
        if self.verified_identity(path)?.is_none() {
            return Ok(());
        }
        match std::fs::remove_dir_all(path) {
            Ok(()) => {
                self.set_identity(path, None);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.set_identity(path, None);
                Ok(())
            }
            Err(error) => Err(journal_error(format!(
                "could not remove {}: {error}",
                path.display()
            ))),
        }
    }

    /// Rename between two owned paths.
    pub fn rename(&mut self, from: &Path, to: &Path) -> LauncherResult<()> {
        if self.verified_identity(to)?.is_some() {
            return Err(refused(format!("{} is occupied", to.display())));
        }
        let identity = self
            .verified_identity(from)?
            .ok_or_else(|| refused(format!("{} is absent", from.display())))?;
        rename_directory(from, to).map_err(|error| {
            journal_error(format!(
                "could not move {} to {}: {error}",
                from.display(),
                to.display()
            ))
        })?;
        self.set_identity(from, None);
        self.set_identity(to, Some(identity));
        Ok(())
    }

    /// Undo a stash: `from` takes `to`'s place. Both must be owned.
    ///
    /// Whatever is at `to` is **moved aside**, never deleted: it is renamed to a sibling named
    /// after this operation (`<name>.replaced-<operation id>`), so a restore that turns out to be
    /// wrong leaves the user's files on disk and named rather than gone. If the stash cannot be put
    /// in place afterwards, the content moved aside is put back. A restore therefore never leaves
    /// the package folder missing: it either holds the stashed copy or holds exactly what it held
    /// before.
    ///
    /// "Is something at `to`?" is answered from the metadata: a path that cannot be examined is an
    /// error, never "nothing there".
    pub fn restore(&mut self, from: &Path, to: &Path) -> LauncherResult<Option<PathBuf>> {
        self.require_owned(from)?;
        self.require_owned(to)?;
        let aside = match std::fs::symlink_metadata(to) {
            Ok(_) => {
                let name = to
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "folder".to_string());
                let aside = to.with_file_name(format!(
                    "{name}.replaced-{}",
                    self.journal.envelope.operation_id
                ));
                // A folder already there is not merged into and not overwritten: what is in it is
                // not something this operation can account for.
                if std::fs::symlink_metadata(&aside).is_ok() {
                    return Err(journal_error(format!(
                        "{} is already there, so the content of {} was not moved aside and the \
                         stashed copy was not put back. Nothing was changed.",
                        aside.display(),
                        to.display()
                    )));
                }
                if let Err(error) = std::fs::rename(to, &aside) {
                    return Err(journal_error(format!(
                        "the content of {} could not be moved aside to {} ({error}), so the stashed \
                         copy was not put back. Nothing was changed.",
                        to.display(),
                        aside.display()
                    )));
                }
                Some(aside)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(journal_error(format!(
                    "could not examine {}: {error}",
                    to.display()
                )))
            }
        };
        match std::fs::rename(from, to) {
            Ok(()) => {
                self.refresh(from)?;
                self.refresh(to)?;
                Ok(aside)
            }
            Err(error) => {
                // Put the moved-aside content back, so a failed restore leaves the folder as it
                // was rather than missing.
                if let Some(aside) = &aside {
                    let _ = std::fs::rename(aside, to);
                }
                Err(journal_error(format!(
                    "could not put {} back in place of {} ({error}). Nothing was changed.",
                    from.display(),
                    to.display()
                )))
            }
        }
    }

    /// Finish the operation: remove the ownership markers, then delete the journal — last, so a
    /// crash before this point still recovers.
    ///
    /// A marker that cannot be removed is reported; the journal is still deleted, because the
    /// operation is over and a journal left behind would be recovered again over a finished job.
    pub fn finish(self) -> LauncherResult<()> {
        let journal_path = self
            .journal_path
            .as_deref()
            .ok_or_else(|| journal_error("the journal must be finished in its own store".into()))?;
        let mut first_error = None;
        for marker in &self.markers {
            match std::fs::remove_file(marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(journal_error(format!(
                            "could not remove the marker {}: {error}",
                            marker.display()
                        )));
                    }
                }
            }
        }
        match std::fs::remove_file(journal_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(journal_error(format!(
                    "could not remove the journal {}: {error}",
                    journal_path.display()
                )))
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Refuse a path the journal did not prove: the one thing a recovery handler cannot do.
    ///
    /// Two re-checks, because the decision was made a moment ago and the disk can have moved since:
    /// a marker that no longer names this operation, and a path whose OS identity is not the one
    /// verified. Either is a refusal, and nothing is touched.
    pub(crate) fn verified_identity(&self, path: &Path) -> LauncherResult<Option<PathIdentity>> {
        let Some(owned) = self
            .owned
            .iter()
            .find(|owned| crate::game_tool_swap::same_path(&owned.path, path))
        else {
            return Err(LauncherError::Generic {
                code: "ERR_RECOVERY_REFUSED".into(),
                message: format!(
                    "Recovery for operation {} may only touch the paths its journal proves it \
                     owns; {} is not one of them. Nothing was changed.",
                    self.operation_id(),
                    path.display()
                ),
            });
        };
        if let Some(marker) = &owned.marker {
            match marker_verdict(marker, self.operation_id()) {
                MarkerVerdict::Ours => {}
                MarkerVerdict::NotOurs(why) => return Err(refused(why)),
                MarkerVerdict::CantTell(why) => return Err(refused(why)),
            }
        }
        let now = path_identity(path).map_err(|error| {
            journal_error(format!(
                "{} could not be identified any more: {error}. Nothing was changed.",
                path.display()
            ))
        })?;
        match (owned.identity, now) {
            // The same folder the decision verified, or a path that is still not there.
            (wanted, found) if wanted == found => Ok(found),
            (None, Some(_)) => Err(refused(format!(
                "{} did not exist when recovery decided what to do and is there now, so it is not \
                 a path this recovery verified",
                path.display()
            ))),
            _ => Err(refused(format!(
                "{} is not the folder recovery verified: it was removed and another one created in \
                 its place",
                path.display()
            ))),
        }
    }

    fn require_owned(&self, path: &Path) -> LauncherResult<()> {
        self.verified_identity(path).map(|_| ())
    }

    /// Record what the OS says about `path` now, after this capability moved or removed it: a
    /// successful action is verified against the state the capability itself left.
    fn refresh(&mut self, path: &Path) -> LauncherResult<()> {
        let identity = path_identity(path).map_err(|error| {
            journal_error(format!(
                "{} could not be identified after recovery changed it: {error}",
                path.display()
            ))
        })?;
        self.set_identity(path, identity);
        Ok(())
    }

    fn set_identity(&mut self, path: &Path, identity: Option<PathIdentity>) {
        if let Some(owned) = self
            .owned
            .iter_mut()
            .find(|owned| crate::game_tool_swap::same_path(&owned.path, path))
        {
            owned.identity = identity;
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An error from writing or reading a journal, or from acting through a capability.
fn journal_error(message: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_RECOVERY_JOURNAL".into(),
        message,
    }
}

/// Recovery refused to touch a path, with the reason: the path the journal did not prove, a marker
/// that no longer matched, or a folder that is not the one the decision verified.
fn refused(why: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_RECOVERY_REFUSED".into(),
        message: format!("Recovery refused to change a path: {why}. Nothing was changed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    enum TestState {
        Starting,
        Done,
    }

    impl JournalState for TestState {
        const KIND: OperationKind = OperationKind::PluginInstall;
    }

    fn envelope() -> Envelope<TestState> {
        Envelope {
            format: ENVELOPE_FORMAT,
            operation_id: "op-1".into(),
            kind: OperationKind::PluginInstall,
            lock: "plugin-install".into(),
            writer: None,
            owned: Vec::new(),
            state: TestState::Starting,
        }
    }

    #[test]
    fn a_journal_that_is_not_there_is_absent_and_nothing_else_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        assert!(matches!(read::<TestState>(&path), JournalRead::Absent));

        // A directory where the journal should be is not "absent": it could not be read.
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            read::<TestState>(&path),
            JournalRead::Unusable(JournalProblem::CantTell(_))
        ));
    }

    #[test]
    fn a_newer_format_is_reported_as_newer_not_as_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let mut value = serde_json::to_value(envelope()).unwrap();
        value["format"] = serde_json::json!(ENVELOPE_FORMAT + 1);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        match read::<TestState>(&path) {
            JournalRead::Unusable(JournalProblem::NewerFormat { found }) => {
                assert_eq!(found, u64::from(ENVELOPE_FORMAT) + 1)
            }
            other => panic!("expected a newer-format problem, got {other:?}"),
        }
    }

    #[test]
    fn a_journal_of_another_kind_is_not_read_as_this_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let mut value = serde_json::to_value(envelope()).unwrap();
        value["state"] = serde_json::json!("starting");
        let mut other = envelope();
        other.kind = OperationKind::PluginInstall;
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(read::<TestState>(&path), JournalRead::Valid(_)));

        // A state that does not belong to this kind's list is malformed, not a default.
        value["state"] = serde_json::json!("running");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            read::<TestState>(&path),
            JournalRead::Unusable(JournalProblem::Malformed(_))
        ));
    }

    #[test]
    fn a_written_journal_reads_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.json");
        let mut written = envelope();
        written.owned = vec![OwnedPath {
            path: dir.path().join("folder"),
            proof: PathProof::Marker {
                at: dir.path().join("marker.json"),
            },
        }];
        write(&path, &written).unwrap();
        match read::<TestState>(&path) {
            JournalRead::Valid(read_back) => assert_eq!(*read_back.envelope(), written),
            other => panic!("expected the journal back, got {other:?}"),
        }
        // The temporary file it was built in is not left behind.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["journal.json".to_string()]);
    }

    /// A process table a test writes the answers of, so every "could not tell" has a source the
    /// test can make happen (MASTER_SPEC §32.13).
    struct StandInTable {
        answer: process_identity::Lookup,
        job: Result<usize, String>,
    }

    impl StandInTable {
        /// A table that was read and does not list the pid.
        fn without_the_pid() -> Self {
            Self {
                answer: process_identity::Lookup::Absent,
                job: Ok(0),
            }
        }

        /// A table that lists `start_time` for the pid.
        fn listing(start_time: u64) -> Self {
            Self {
                answer: process_identity::Lookup::Found {
                    start_time,
                    exe: None,
                    zombie: false,
                    exited: false,
                },
                job: Ok(0),
            }
        }
    }

    impl process_identity::ProcessView for StandInTable {
        fn lookup(&self, _pid: u32) -> process_identity::Lookup {
            self.answer.clone()
        }
        fn job_members(&self, _job: &str) -> Result<usize, String> {
            self.job.clone()
        }
    }

    /// An identity that a stand-in table can be asked about.
    fn recorded_writer() -> ProcessIdentity {
        ProcessIdentity {
            pid: 4242,
            start_time: 1_700_000_000,
            expected_exe: None,
            job: None,
        }
    }

    #[test]
    fn a_failed_lookup_of_the_recorded_pid_is_never_proof_that_the_writer_is_gone() {
        let table = StandInTable {
            answer: process_identity::Lookup::CantTell(
                "the process table could not be read".into(),
            ),
            ..StandInTable::without_the_pid()
        };
        let answer = writer_liveness(Some(&recorded_writer()), &table);
        assert!(
            matches!(answer, WriterLiveness::CantTell(_)),
            "a lookup that failed must not read as gone, got {answer:?}"
        );
    }

    #[test]
    fn an_empty_targeted_refresh_is_not_proof_that_the_writer_is_gone() {
        let table = StandInTable {
            answer: process_identity::Lookup::CantTell(
                "the targeted refresh returned no entry".into(),
            ),
            ..StandInTable::without_the_pid()
        };
        let answer = writer_liveness(Some(&recorded_writer()), &table);
        assert!(
            matches!(answer, WriterLiveness::CantTell(_)),
            "an unreadable process table must not read as gone, got {answer:?}"
        );
    }

    #[test]
    fn a_failed_job_query_is_never_proof_that_the_writer_is_gone() {
        let mut writer = recorded_writer();
        writer.job = Some("agora-launch-1".into());
        let table = StandInTable {
            job: Err("the job object could not be opened".into()),
            ..StandInTable::without_the_pid()
        };
        let answer = writer_liveness(Some(&writer), &table);
        assert!(
            matches!(answer, WriterLiveness::CantTell(_)),
            "a job query that failed must not read as gone, got {answer:?}"
        );
    }

    #[test]
    fn a_pid_the_os_reports_absent_is_gone() {
        let table = StandInTable::without_the_pid();
        assert_eq!(
            writer_liveness(Some(&recorded_writer()), &table),
            WriterLiveness::Gone
        );
    }

    #[test]
    fn a_reused_pid_is_gone() {
        let table = StandInTable::listing(1_700_000_555);
        assert_eq!(
            writer_liveness(Some(&recorded_writer()), &table),
            WriterLiveness::Gone
        );
    }

    #[test]
    fn the_same_pid_and_start_time_is_alive() {
        let table = StandInTable::listing(1_700_000_000);
        assert_eq!(
            writer_liveness(Some(&recorded_writer()), &table),
            WriterLiveness::Alive
        );
    }

    #[test]
    fn an_identity_that_was_never_captured_is_not_checked_at_all() {
        let mut identity = process_identity::capture_self().expect("the test process is readable");
        identity.start_time = 0;
        let table = StandInTable::without_the_pid();
        assert!(matches!(
            writer_liveness(Some(&identity), &table),
            WriterLiveness::CantTell(_)
        ));
        assert!(matches!(
            writer_liveness(None, &table),
            WriterLiveness::CantTell(_)
        ));
    }

    #[test]
    fn the_live_test_process_is_alive() {
        let identity = process_identity::capture_self().expect("the test process is readable");
        assert_eq!(
            writer_liveness(Some(&identity), &process_identity::OsProcesses),
            WriterLiveness::Alive
        );
    }

    // -- the decision and the capability ------------------------------------

    /// A proof that says every path is the operation's, for the tests about identity and restore
    /// rather than about the content proof.
    struct AnyContentProof;

    impl ContentProof for AnyContentProof {
        fn prove(&self, _path: &Path) -> ProofResult {
            ProofResult::Proven
        }
    }

    /// An envelope whose writer provably no longer exists: what a crash leaves.
    fn crashed_envelope(owned: Vec<OwnedPath>) -> Envelope<TestState> {
        let mut envelope = envelope();
        envelope.owned = owned;
        envelope.writer = Some(ProcessIdentity {
            pid: u32::MAX,
            start_time: 1,
            expected_exe: None,
            job: None,
        });
        envelope
    }

    #[test]
    fn a_second_process_holding_the_lock_is_busy_and_nothing_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        write(&path, &crashed_envelope(Vec::new())).unwrap();

        // A second lock manager on the same root is what another process sees.
        let other_process = LockManager::new(dir.path().join("locks"));
        let held = other_process
            .acquire_with_timeout(
                LockResource::PluginInstall,
                "plugin-install",
                Duration::ZERO,
                None,
            )
            .expect("the live operation takes the lock");
        match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Busy(why) => assert!(!why.is_empty()),
            other => panic!("a held lock is busy, got {other:?}"),
        }
        // Nothing was consumed: the journal is still there for the next start.
        assert!(matches!(read::<TestState>(&path), JournalRead::Valid(_)));
        drop(held);
        assert!(matches!(
            decide::<TestState>(
                &locks,
                &LockResource::PluginInstall,
                &path,
                &NoContentProof,
                &StandInTable::without_the_pid()
            ),
            Decision::Act(_)
        ));
    }

    #[test]
    fn a_live_recorded_writer_makes_the_decision_busy() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let mut envelope = envelope();
        envelope.writer = process_identity::capture_self();
        write(&path, &envelope).unwrap();
        // The real process table: this test process is the writer, and it is alive.
        match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &process_identity::OsProcesses,
        ) {
            Decision::Busy(why) => assert!(why.contains("still running"), "{why}"),
            other => panic!("a live writer is busy, got {other:?}"),
        }
    }

    #[test]
    fn a_journal_with_no_writer_needs_a_hand() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        write(&path, &envelope()).unwrap();
        assert!(matches!(
            decide::<TestState>(
                &locks,
                &LockResource::PluginInstall,
                &path,
                &NoContentProof,
                &StandInTable::without_the_pid()
            ),
            Decision::NeedsHand(_)
        ));
    }

    /// A recorded writer the OS will not answer about is a person's decision, not permission:
    /// recovery must not read "could not tell" as "gone" and act (MASTER_SPEC §32.13).
    #[test]
    fn a_writer_that_cannot_be_checked_needs_a_hand_and_nothing_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let folder = dir.path().join("folder");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(&folder).unwrap();
        let mut envelope = crashed_envelope(vec![OwnedPath {
            path: folder.clone(),
            proof: PathProof::Marker { at: marker.clone() },
        }]);
        envelope.operation_id = "op-1".into();
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();

        let table = StandInTable {
            answer: process_identity::Lookup::CantTell("the table could not be read".into()),
            ..StandInTable::without_the_pid()
        };
        match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &table,
        ) {
            Decision::NeedsHand(why) => assert!(why.contains("cannot say whether"), "{why}"),
            other => panic!("a writer that cannot be checked needs a hand, got {other:?}"),
        }
        assert!(folder.is_dir(), "nothing was touched");
        assert!(matches!(read::<TestState>(&path), JournalRead::Valid(_)));
    }

    #[test]
    fn a_path_whose_marker_belongs_to_another_operation_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let folder = dir.path().join("folder");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(&folder).unwrap();
        write_marker(&marker, "another-operation").unwrap();

        let mut envelope = crashed_envelope(vec![OwnedPath {
            path: folder.clone(),
            proof: PathProof::Marker { at: marker },
        }]);
        envelope.operation_id = "this-operation".into();
        write(&path, &envelope).unwrap();

        match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => {
                assert!(capability.owned().is_empty(), "it is not ours to touch");
                assert_eq!(capability.left_alone().len(), 1);
                assert!(capability.left_alone()[0].1.contains("another operation"));
            }
            other => panic!("expected a capability with nothing to touch, got {other:?}"),
        }
        assert!(folder.is_dir(), "nothing was touched");
    }

    #[test]
    fn the_capability_refuses_a_path_the_journal_did_not_prove() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let owned = dir.path().join("owned");
        let unowned = dir.path().join("unowned");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(&unowned).unwrap();
        let envelope = crashed_envelope(vec![OwnedPath {
            path: owned.clone(),
            proof: PathProof::Marker { at: marker.clone() },
        }]);
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();

        let mut capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };
        assert_eq!(capability.owned(), [&owned]);
        let refused = capability
            .remove_dir(&unowned)
            .expect_err("a path the journal did not prove is refused");
        assert_eq!(refused.code(), "ERR_RECOVERY_REFUSED");
        assert!(unowned.is_dir(), "the refused path was not touched");
        // And the proven path is accepted.
        capability.remove_dir(&owned).unwrap();
        capability.finish().unwrap();
        assert!(
            matches!(read::<TestState>(&path), JournalRead::Absent),
            "finish deletes the journal, last"
        );
        assert!(
            std::fs::symlink_metadata(&marker).is_err(),
            "finish removes the marker too"
        );
    }

    // -- the capability acts on what it verified (MASTER_SPEC §32.11) --------------------------

    /// A folder removed and another created in its place keeps the same name but is not the folder
    /// the decision verified. The capability must refuse it, and leave it alone.
    #[test]
    fn a_folder_swapped_after_the_decision_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let owned = dir.path().join("owned");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(owned.join("ours")).unwrap();
        let envelope = crashed_envelope(vec![OwnedPath {
            path: owned.clone(),
            proof: PathProof::Marker { at: marker.clone() },
        }]);
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();

        let mut capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };

        // Between the decision and the action: the folder is replaced by another one with the same
        // name.
        std::fs::remove_dir_all(&owned).unwrap();
        std::fs::create_dir_all(owned.join("someone-elses")).unwrap();

        let refused = capability
            .remove_dir(&owned)
            .expect_err("a folder that is not the one verified is refused");
        assert_eq!(refused.code(), "ERR_RECOVERY_REFUSED");
        assert!(
            owned.join("someone-elses").is_dir(),
            "the folder that was not verified was touched"
        );
    }

    /// A marker replaced after the decision is refused: the proof is re-checked, not remembered.
    #[test]
    fn a_marker_replaced_after_the_decision_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let owned = dir.path().join("owned");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(&owned).unwrap();
        let envelope = crashed_envelope(vec![OwnedPath {
            path: owned.clone(),
            proof: PathProof::Marker { at: marker.clone() },
        }]);
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();

        let mut capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };
        write_marker(&marker, "another-operation").unwrap();

        let refused = capability
            .remove_dir(&owned)
            .expect_err("a marker that no longer names this operation is refused");
        assert_eq!(refused.code(), "ERR_RECOVERY_REFUSED");
        assert!(owned.is_dir(), "nothing was touched");
    }

    /// A restore never deletes what it replaces: the content is moved to a named sibling, and the
    /// stashed copy takes the path. A restore that turns out to be wrong loses nothing.
    #[test]
    fn a_restore_keeps_what_it_replaces_instead_of_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let stash = dir.path().join("stash");
        let destination = dir.path().join("destination");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(stash.join("previous-version")).unwrap();
        std::fs::create_dir_all(destination.join("half-installed")).unwrap();
        let envelope = crashed_envelope(vec![
            OwnedPath {
                path: stash.clone(),
                proof: PathProof::Marker { at: marker.clone() },
            },
            OwnedPath {
                path: destination.clone(),
                proof: PathProof::Content,
            },
        ]);
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();
        let operation_id = envelope.operation_id.clone();

        let mut capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &AnyContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };

        let aside = capability
            .restore(&stash, &destination)
            .expect("the restore")
            .expect("content was there, so it was moved aside");
        assert!(
            destination.join("previous-version").is_dir(),
            "the stashed copy is in place"
        );
        assert_eq!(
            aside,
            dir.path()
                .join(format!("destination.replaced-{operation_id}")),
            "the replaced content goes to a sibling named after the operation"
        );
        assert!(
            aside.join("half-installed").is_dir(),
            "the content that was there is kept, not deleted"
        );
    }

    /// A kept-aside folder already there is not merged into or overwritten: the restore refuses
    /// and nothing is moved.
    #[test]
    fn a_restore_refuses_when_the_kept_folder_is_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let stash = dir.path().join("stash");
        let destination = dir.path().join("destination");
        let marker = dir.path().join("marker.json");
        std::fs::create_dir_all(stash.join("previous-version")).unwrap();
        std::fs::create_dir_all(destination.join("half-installed")).unwrap();
        let envelope = crashed_envelope(vec![
            OwnedPath {
                path: stash.clone(),
                proof: PathProof::Marker { at: marker.clone() },
            },
            OwnedPath {
                path: destination.clone(),
                proof: PathProof::Content,
            },
        ]);
        write_marker(&marker, &envelope.operation_id).unwrap();
        write(&path, &envelope).unwrap();
        let aside = dir
            .path()
            .join(format!("destination.replaced-{}", envelope.operation_id));
        std::fs::create_dir_all(aside.join("older-content")).unwrap();

        let mut capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &AnyContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };

        let refused = capability
            .restore(&stash, &destination)
            .expect_err("a kept folder already there is not merged into");
        assert_eq!(refused.code(), "ERR_RECOVERY_JOURNAL");
        assert!(destination.join("half-installed").is_dir(), "nothing moved");
        assert!(stash.join("previous-version").is_dir(), "nothing moved");
        assert!(
            aside.join("older-content").is_dir(),
            "what was already in the kept folder is untouched"
        );
    }

    /// The journal's own fields come from the document the decision validated, not a second read:
    /// a file rewritten after the decision does not change what recovery acts on.
    #[test]
    fn the_payload_is_read_once_before_anything_is_decided() {
        #[derive(Debug, serde::Deserialize)]
        struct TestPayload {
            new_version: String,
        }

        let dir = tempfile::tempdir().unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        let path = dir.path().join("journal.json");
        let mut value = serde_json::to_value(crashed_envelope(Vec::new())).unwrap();
        value["new_version"] = serde_json::json!("2.0.0");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let capability = match decide::<TestState>(
            &locks,
            &LockResource::PluginInstall,
            &path,
            &NoContentProof,
            &StandInTable::without_the_pid(),
        ) {
            Decision::Act(capability) => capability,
            other => panic!("expected a capability, got {other:?}"),
        };

        // The file changes underneath the handler: the decision's document is what it reads.
        value["new_version"] = serde_json::json!("9.9.9");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let payload: TestPayload = capability.payload().expect("its own fields");
        assert_eq!(
            payload.new_version, "2.0.0",
            "the payload was read again from a file that changed"
        );
    }
}
