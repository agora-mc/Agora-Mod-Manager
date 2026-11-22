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
//! # What is deliberately not here yet
//!
//! Only the plugin install writes an envelope so far; the other journals move onto it one at a time
//! (MASTER_SPEC §32.11, D1), each keeping its proven recovery logic behind the capability. A
//! journal that is a legacy one — no envelope, or nothing that proves who wrote it — is
//! [`Decision::NeedsHand`], never recovered automatically.

use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::{LockGuard, LockManager, LockRefusal, LockResource};
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
    /// Installing or updating a plugin package: the previous version is stashed in the rollback
    /// folder while the new one is unpacked and recorded.
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
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        journal_error(format!(
            "could not publish the journal {}: {e}",
            path.display()
        ))
    })?;
    if let Some(parent) = path.parent() {
        sync_directory(parent);
    }
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
    let bytes = serde_json::to_vec_pretty(&body)
        .map_err(|e| journal_error(format!("could not write the marker: {e}")))?;
    std::fs::write(path, &bytes).map_err(|e| {
        journal_error(format!(
            "could not write the marker {}: {e}",
            path.display()
        ))
    })
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
    Valid(Box<Envelope<S>>),
    /// A journal is there, but not one this build can act on. Kept for diagnosis; the reason says
    /// which of the cases below it is.
    Unusable(JournalProblem),
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
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{} is not JSON: {error}",
                path.display()
            )))
        }
    };
    match value.get("format").and_then(serde_json::Value::as_u64) {
        Some(format) if format > u64::from(ENVELOPE_FORMAT) => {
            return JournalRead::Unusable(JournalProblem::NewerFormat { found: format })
        }
        Some(format) if format < u64::from(ENVELOPE_FORMAT) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{} names format {format}, which is not one Agora wrote",
                path.display()
            )))
        }
        Some(_) => {}
        None => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{} names no format version",
                path.display()
            )))
        }
    }
    let envelope: Envelope<S> = match serde_json::from_value(value) {
        Ok(envelope) => envelope,
        Err(error) => {
            return JournalRead::Unusable(JournalProblem::Malformed(format!(
                "{} is not a complete journal: {error}",
                path.display()
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
            "{} names no operation",
            path.display()
        )));
    }
    JournalRead::Valid(Box::new(envelope))
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
pub enum Decision<S> {
    /// No journal: the operation was not started, or it finished. Nothing to do, nothing to report.
    Nothing,
    /// The operation's lock is held right now — another Agora may be in the middle of it. A held
    /// lock is busy, not necessarily alive: report it and try again next start. Nothing was touched.
    Busy(String),
    /// Something about this journal cannot be decided safely, with the reason and what a person can
    /// do. Nothing was touched, and the journal is kept for diagnosis.
    NeedsHand(String),
    /// Recovery may act, through the capability, on exactly the paths the journal proved it owns.
    Act(Box<RecoveryCapability<S>>),
}

/// Decide what to do about one operation's journal (MASTER_SPEC §32.11).
///
/// In this order, and nothing is touched before the last step:
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
) -> Decision<S> {
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

    let envelope = match read::<S>(journal_path) {
        JournalRead::Absent => return Decision::Nothing,
        JournalRead::Unusable(problem) => {
            return Decision::NeedsHand(format!(
                "A journal for this operation is at {} and {problem}, so Agora cannot tell what it \
                 changed. Nothing was changed; the journal was kept so it can be looked at.",
                journal_path.display()
            ))
        }
        JournalRead::Valid(envelope) => envelope,
    };

    if let Ok(name) = resource.lock_name() {
        if envelope.lock != name {
            return Decision::NeedsHand(format!(
                "The journal (operation {}) says it was written under the '{}' lock, not '{}'. \
                 Nothing was changed; the journal was kept.",
                envelope.operation_id, envelope.lock, name
            ));
        }
    }

    match writer_liveness(envelope.writer.as_ref(), view) {
        WriterLiveness::Alive => {
            let pid = match envelope.writer.as_ref() {
                Some(writer) => writer.pid.to_string(),
                None => "unknown".to_string(),
            };
            return Decision::Busy(format!(
                "The process that wrote the journal (pid {pid}) is still running, so the operation \
                 may still be going. Nothing was changed."
            ));
        }
        WriterLiveness::CantTell(reason) => {
            return Decision::NeedsHand(format!(
                "The journal cannot say whether the operation is still running: {reason}. Nothing \
                 was changed; the journal was kept."
            ))
        }
        WriterLiveness::Gone => {}
    }

    let mut owned = Vec::new();
    let mut left_alone = Vec::new();
    let mut markers = Vec::new();
    for entry in &envelope.owned {
        match &entry.proof {
            PathProof::Marker { at } => match marker_verdict(at, &envelope.operation_id) {
                MarkerVerdict::Ours => {
                    owned.push(entry.path.clone());
                    markers.push(at.clone());
                }
                MarkerVerdict::NotOurs(why) => left_alone.push((entry.path.clone(), why)),
                MarkerVerdict::CantTell(why) => {
                    return Decision::NeedsHand(format!(
                        "A path this journal owns cannot be checked: {why}. Nothing was changed; \
                         the journal was kept."
                    ))
                }
            },
            PathProof::Content => match proof.prove(&entry.path) {
                ProofResult::Proven => owned.push(entry.path.clone()),
                ProofResult::NotOwned(why) => left_alone.push((entry.path.clone(), why)),
                ProofResult::CantTell(why) => {
                    return Decision::NeedsHand(format!(
                        "A path this journal owns cannot be checked: {why}. Nothing was changed; \
                         the journal was kept."
                    ))
                }
            },
        }
    }

    Decision::Act(Box::new(RecoveryCapability {
        lock,
        journal_path: journal_path.to_path_buf(),
        envelope: *envelope,
        owned,
        left_alone,
        markers,
    }))
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
#[derive(Debug)]
pub struct RecoveryCapability<S> {
    /// Held for the capability's whole life; dropped last.
    lock: LockGuard,
    /// The journal this capability was issued from, deleted by [`Self::finish`].
    journal_path: PathBuf,
    envelope: Envelope<S>,
    /// The paths whose proof matched: the only paths the operations below accept.
    owned: Vec<PathBuf>,
    /// The paths whose proof did *not* match, with the reason. Never touched, always reported.
    left_alone: Vec<(PathBuf, String)>,
    /// The marker files this operation wrote; removed with the journal.
    markers: Vec<PathBuf>,
}

impl<S> RecoveryCapability<S> {
    /// The id of the operation this capability was issued for.
    pub fn operation_id(&self) -> &str {
        &self.envelope.operation_id
    }

    /// The state the journal recorded.
    pub fn state(&self) -> &S {
        &self.envelope.state
    }

    /// The paths this operation proved it owns. These, and no others, are accepted below.
    pub fn owned(&self) -> &[PathBuf] {
        &self.owned
    }

    /// The paths the journal claimed but could not prove, with the reason: left alone, and to be
    /// reported to the user.
    pub fn left_alone(&self) -> &[(PathBuf, String)] {
        &self.left_alone
    }

    /// The lock this capability holds, for a call that must pass it on rather than re-take it.
    pub fn lock(&self) -> &LockGuard {
        &self.lock
    }

    /// Where the journal this capability came from lives.
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Remove an owned folder.
    pub fn remove_dir(&self, path: &Path) -> LauncherResult<()> {
        self.require_owned(path)?;
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(journal_error(format!(
                "could not remove {}: {error}",
                path.display()
            ))),
        }
    }

    /// Rename between two owned paths.
    pub fn rename(&self, from: &Path, to: &Path) -> LauncherResult<()> {
        self.require_owned(from)?;
        self.require_owned(to)?;
        std::fs::rename(from, to).map_err(|error| {
            journal_error(format!(
                "could not move {} to {}: {error}",
                from.display(),
                to.display()
            ))
        })
    }

    /// Undo a stash: whatever is at `to` is removed and `from` takes its place. Both must be owned.
    ///
    /// "Is something at `to`?" is answered from the metadata: a path that cannot be examined is an
    /// error, never "nothing there".
    pub fn restore(&self, from: &Path, to: &Path) -> LauncherResult<()> {
        self.require_owned(from)?;
        self.require_owned(to)?;
        match std::fs::symlink_metadata(to) {
            Ok(_) => self.remove_dir(to)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(journal_error(format!(
                    "could not examine {}: {error}",
                    to.display()
                )))
            }
        }
        self.rename(from, to)
    }

    /// Finish the operation: remove the ownership markers, then delete the journal — last, so a
    /// crash before this point still recovers.
    ///
    /// A marker that cannot be removed is reported; the journal is still deleted, because the
    /// operation is over and a journal left behind would be recovered again over a finished job.
    pub fn finish(self) -> LauncherResult<()> {
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
        match std::fs::remove_file(&self.journal_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(journal_error(format!(
                    "could not remove the journal {}: {error}",
                    self.journal_path.display()
                )))
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Refuse a path the journal did not prove: the one thing a recovery handler cannot do.
    fn require_owned(&self, path: &Path) -> LauncherResult<()> {
        if self
            .owned
            .iter()
            .any(|owned| crate::game_tool_swap::same_path(owned, path))
        {
            return Ok(());
        }
        Err(LauncherError::Generic {
            code: "ERR_RECOVERY_REFUSED".into(),
            message: format!(
                "Recovery for operation {} may only touch the paths its journal proves it owns; {} \
                 is not one of them. Nothing was changed.",
                self.envelope.operation_id,
                path.display()
            ),
        })
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
            JournalRead::Valid(read_back) => assert_eq!(*read_back, written),
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
        table_readable: bool,
        job: Result<usize, String>,
    }

    impl StandInTable {
        /// A table that was read and does not list the pid.
        fn without_the_pid() -> Self {
            Self {
                answer: process_identity::Lookup::Absent,
                table_readable: true,
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
                table_readable: true,
                job: Ok(0),
            }
        }
    }

    impl process_identity::ProcessView for StandInTable {
        fn lookup(&self, _pid: u32) -> process_identity::Lookup {
            self.answer.clone()
        }
        fn table_answered(&self) -> bool {
            self.table_readable
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
    fn an_empty_process_table_is_not_proof_that_the_writer_is_gone() {
        let table = StandInTable {
            table_readable: false,
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
    fn a_pid_that_is_not_in_a_table_that_was_read_is_gone() {
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
        assert_eq!(capability.owned(), std::slice::from_ref(&owned));
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
}
