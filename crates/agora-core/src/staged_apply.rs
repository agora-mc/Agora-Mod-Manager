//! Staged apply: the one durable way to put files into an instance (MASTER_SPEC §32.4).
//!
//! An apply collects its changes in a stage on the instance's own disk
//! (`<root>/.agora_stage/<operation id>/`), so every change is a rename, and journals them on the
//! §32.11 envelope before the first one happens. The promise is **the old instance or the new
//! one, after successful recovery — never a mix**:
//!
//! 1. [`Stage::begin`] settles any apply a crash left unresolved, then writes the journal (no
//!    changes yet) and the stage's ownership marker.
//! 2. [`Stage::write`], [`Stage::write_from`], [`Stage::remove`] and [`Stage::rename`] stage
//!    changes by [`StrictRelPath`], never by an absolute path; nothing in the instance changes yet.
//! 3. [`Stage::commit`] decides what happens to each file being replaced or removed, rewrites the
//!    journal with every planned move (flushed, with its folder), makes the moves, and then writes
//!    the journal's `committed` record. **That record is the commit point.** Before it, recovery
//!    rolls back; after it, recovery only finishes the cleanup.
//! 4. Cleanup removes the stage's backups and the journal.
//!
//! Rollback is decided from what is on disk, one step at a time, so a recovery that is itself cut
//! off by another crash simply runs again. Recovery runs through [`recovery::decide`]'s capability:
//! it may only move files below the instance folder and the stage the journal proves it owns (a
//! marker in the stage, and both folders' identities), never through a link.
//!
//! **Files the user changed are never deleted.** A file being replaced or removed is backed up in
//! the stage only when its bytes are what Agora last wrote ([`Expect::Agora`]); otherwise it is
//! moved to `<root>/.agora/moved-aside/` and reported, and nothing ever deletes it from there.
//!
//! This engine is game-agnostic and lives in core beside the recovery capability it extends; a
//! game package calls it with the instance guard it already holds.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agora_game_api::StrictRelPath;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{LauncherError, LauncherResult};
use crate::instance_runtime::InstanceGuard;
use crate::lock_manager::{LockGuard, LockManager, LockRefusal, LockResource};
use crate::process_identity;
use crate::recovery::{
    self, Decision, Envelope, JournalRead, JournalState, NoContentProof, OperationKind, OwnedPath,
    PathProof, RecoveryCapability,
};

/// The folder below an instance root that holds every stage and journal.
const STAGE_DIR: &str = ".agora_stage";
/// Where a file the user changed is kept, relative to the instance root.
const ASIDE_DIR: &str = ".agora/moved-aside";
/// The ownership marker inside each stage.
const MARKER: &str = ".owner.json";

/// The folder holding the stages and journals of the instance at `root`.
pub fn stage_root(root: &Path) -> PathBuf {
    root.join(STAGE_DIR)
}

/// What the caller knows about the file at a path it is about to replace or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Nothing should be there: an add. A file at the path refuses the commit.
    Absent,
    /// Agora last wrote a file with this SHA-256 (lowercase hex) there. If the file there still
    /// has it, it is Agora's and is replaced or removed; if not, the user changed it, and it is
    /// kept aside.
    Agora(String),
    /// There is no record of what Agora wrote: whatever is there is the user's and is kept aside.
    Unknown,
}

/// What an apply did besides the changes it was asked for.
#[derive(Debug, Default)]
pub struct ApplyReport {
    /// Files the user had changed, kept rather than replaced or removed.
    pub kept_aside: Vec<KeptAside>,
}

/// A file the user changed, and where it was kept.
#[derive(Debug, Clone)]
pub struct KeptAside {
    /// Where the file was in the instance.
    pub path: StrictRelPath,
    /// Where it is now.
    pub kept_at: PathBuf,
}

/// What recovery did about one unresolved apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedRecovery {
    /// The apply had not reached its commit point: the instance is as it was before it.
    RolledBack { operation: String },
    /// The apply had committed: its cleanup was finished.
    Finished { operation: String },
    /// Another Agora holds the instance lock: its apply may still be running. Nothing was touched.
    Busy(String),
    /// The journal cannot be acted on safely. Nothing was touched, and the journal was kept.
    NeedsHand(String),
}

/// Where a test stops a commit, as a crash would (test seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// Before the planned moves are journaled.
    BeforePlan,
    /// After the plan is journaled, before the first move.
    AfterPlan,
    /// After this many moves.
    AfterMove(usize),
    /// After the caller's database change and its receipt committed, before the journal's
    /// `committed` record ([`Stage::commit_with_receipt`]).
    AfterReceipt,
    /// After the `committed` record, before any cleanup.
    AfterCommit,
    /// After this many backups were removed by the cleanup.
    AfterCleanup(usize),
}

// ---------------------------------------------------------------------------
// The journal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum State {
    /// Changes may have been made; recovery rolls them back.
    Applying,
    /// The commit point was reached; recovery finishes the cleanup.
    Committed,
}

impl JournalState for State {
    const KIND: OperationKind = OperationKind::StagedApply;
}

/// Where a file that was replaced or removed went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "kebab-case")]
enum Previous {
    /// Into the stage: Agora's own file, deleted by the cleanup.
    Backup { name: String },
    /// Aside, below the instance root: the user's file, never deleted.
    Aside { path: String },
}

/// One planned change, in the order it is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
enum Op {
    /// The staged file `staged` takes `path`; what was there went to `previous`.
    Write {
        path: String,
        staged: String,
        previous: Option<Previous>,
    },
    /// What was at `path` went to `previous`.
    Remove { path: String, previous: Previous },
    /// `from` became `to`.
    Rename { from: String, to: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StagedJournal {
    #[serde(flatten)]
    envelope: Envelope<State>,
    instance_id: String,
    #[serde(default)]
    ops: Vec<Op>,
    /// The commit point is a row in `staged_apply_receipts` written with the caller's database
    /// change, not the `committed` record alone ([`Stage::commit_with_receipt`]).
    #[serde(default)]
    receipt: bool,
}

fn journal_path(root: &Path, operation: &str) -> PathBuf {
    stage_root(root).join(format!("{operation}.json"))
}

fn stage_dir(root: &Path, operation: &str) -> PathBuf {
    stage_root(root).join(operation)
}

// ---------------------------------------------------------------------------
// The stage
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Pending {
    Write {
        path: StrictRelPath,
        staged: String,
        expect: Expect,
    },
    Remove {
        path: StrictRelPath,
        expect: Expect,
    },
    Rename {
        from: StrictRelPath,
        to: StrictRelPath,
    },
}

/// An apply being prepared under the instance guard. Dropping it without [`Self::commit`] changes
/// nothing in the instance and removes the stage.
pub struct Stage<'g> {
    guard: &'g InstanceGuard,
    root: PathBuf,
    dir: PathBuf,
    journal_path: PathBuf,
    journal: StagedJournal,
    pending: Vec<Pending>,
    next_name: usize,
    /// Set by a commit that finished, or stopped by the test seam: nothing for `Drop` to do.
    settled: bool,
    crash: Option<Boundary>,
}

impl std::fmt::Debug for Stage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage")
            .field("root", &self.root)
            .field("operation", &self.journal.envelope.operation_id)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl<'g> Stage<'g> {
    /// Start an apply to the instance at `root`, under its guard.
    ///
    /// An apply a crash left unresolved in this instance is settled first (rolled back, or its
    /// cleanup finished). One that cannot be settled safely refuses the new apply, so it is never
    /// overwritten.
    pub fn begin(guard: &'g InstanceGuard, root: &Path) -> LauncherResult<Self> {
        Self::begin_settling(guard, root, None)
    }

    /// [`Self::begin`] for a writer that also changes the database: an earlier apply whose commit
    /// point is a receipt ([`Self::commit_with_receipt`]) is judged by `conn`.
    pub fn begin_with_receipts(
        guard: &'g InstanceGuard,
        root: &Path,
        conn: &rusqlite::Connection,
    ) -> LauncherResult<Self> {
        Self::begin_settling(guard, root, Some(conn))
    }

    fn begin_settling(
        guard: &'g InstanceGuard,
        root: &Path,
        receipts: Option<&rusqlite::Connection>,
    ) -> LauncherResult<Self> {
        plain_directory(root)?;
        for outcome in settle(guard.lock(), guard.instance_id(), root, None, receipts) {
            if let StagedRecovery::NeedsHand(reason) | StagedRecovery::Busy(reason) = outcome {
                return Err(refused(format!(
                    "An earlier change to this instance was interrupted and cannot be finished \
                     safely: {reason}"
                )));
            }
        }

        let resource = LockResource::Instance(guard.instance_id().to_string());
        let mut envelope = Envelope::new(&resource, State::Applying, Vec::new())?;
        let operation = envelope.operation_id.clone();
        let dir = stage_dir(root, &operation);
        std::fs::create_dir_all(&dir).map_err(|error| io(&dir, error))?;
        let marker = dir.join(MARKER);
        recovery::write_marker(&marker, &operation)?;
        let marked = |path: &Path| -> LauncherResult<OwnedPath> {
            let identity = recovery::path_identity(path)
                .map_err(|error| io(path, error))?
                .ok_or_else(|| refused(format!("{} vanished", path.display())))?;
            Ok(OwnedPath {
                path: path.to_path_buf(),
                proof: PathProof::MarkedIdentity {
                    at: marker.clone(),
                    identities: vec![identity],
                },
            })
        };
        envelope.owned = vec![marked(root)?, marked(&dir)?];
        let journal = StagedJournal {
            envelope,
            instance_id: guard.instance_id().to_string(),
            ops: Vec::new(),
            receipt: false,
        };
        let journal_path = journal_path(root, &operation);
        recovery::write(&journal_path, &journal)?;
        Ok(Stage {
            guard,
            root: root.to_path_buf(),
            dir,
            journal_path,
            journal,
            pending: Vec::new(),
            next_name: 0,
            settled: false,
            crash: None,
        })
    }

    /// Stage `bytes` to become the file at `path`.
    pub fn write(
        &mut self,
        path: &StrictRelPath,
        bytes: &[u8],
        expect: Expect,
    ) -> LauncherResult<()> {
        self.claim(path)?;
        let staged = self.new_name("new");
        let file = self.dir.join(&staged);
        write_synced(&file, bytes)?;
        self.pending.push(Pending::Write {
            path: path.clone(),
            staged,
            expect,
        });
        Ok(())
    }

    /// Stage a copy of the file at `source` to become the file at `path`.
    pub fn write_from(
        &mut self,
        path: &StrictRelPath,
        source: &Path,
        expect: Expect,
    ) -> LauncherResult<()> {
        self.claim(path)?;
        let staged = self.new_name("new");
        let file = self.dir.join(&staged);
        std::fs::copy(source, &file).map_err(|error| io(source, error))?;
        sync_file(&file)?;
        self.pending.push(Pending::Write {
            path: path.clone(),
            staged,
            expect,
        });
        Ok(())
    }

    /// Stage the file at `source` to become the file at `path`, moving it into the stage (it must be
    /// a file the caller owns, on the instance's disk or not: a move that cannot be a rename is a
    /// copy, synced, and the source removed).
    pub fn write_moving(
        &mut self,
        path: &StrictRelPath,
        source: &Path,
        expect: Expect,
    ) -> LauncherResult<()> {
        self.claim(path)?;
        let staged = self.new_name("new");
        let file = self.dir.join(&staged);
        if std::fs::rename(source, &file).is_err() {
            std::fs::copy(source, &file).map_err(|error| io(source, error))?;
            sync_file(&file)?;
            std::fs::remove_file(source).map_err(|error| io(source, error))?;
        }
        self.pending.push(Pending::Write {
            path: path.clone(),
            staged,
            expect,
        });
        Ok(())
    }

    /// Stage a file the caller makes itself at the stage path it is given (a content-store link,
    /// for example), to become the file at `path`. The caller is responsible for the file's bytes
    /// being durable: a content-store object already is.
    pub fn write_with(
        &mut self,
        path: &StrictRelPath,
        expect: Expect,
        make: impl FnOnce(&Path) -> LauncherResult<()>,
    ) -> LauncherResult<()> {
        self.claim(path)?;
        let staged = self.new_name("new");
        make(&self.dir.join(&staged))?;
        self.pending.push(Pending::Write {
            path: path.clone(),
            staged,
            expect,
        });
        Ok(())
    }

    /// Stage the instance manifest (`instance_manifest.json`) becoming `manifest`, by the same rule
    /// [`crate::helpers::atomic_write_manifest`] follows: the same bytes; nothing when the file
    /// already says exactly this; and, for an older manifest version, its original bytes kept in
    /// `instance_manifest.json.v<N>.bak` (a backup with different bytes refuses).
    pub fn write_manifest(
        &mut self,
        manifest: &crate::models::InstanceManifest,
    ) -> LauncherResult<()> {
        let live = self.root.join("instance_manifest.json");
        let parts = crate::helpers::manifest_write_parts(&live, manifest)?;
        let Some(text) = parts.text else {
            return Ok(());
        };
        if let Some((backup, bytes)) = parts.backup {
            let name = backup
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| refused(format!("{} has no usable name", backup.display())))?
                .to_string();
            let backup = agora_game_api::RelPath::strict(name.as_str())
                .map_err(|error| refused(error.to_string()))?;
            self.write(&backup, &bytes, Expect::Absent)?;
        }
        let expect = match std::fs::read(&live) {
            Ok(bytes) => Expect::Agora(format!("{:x}", Sha256::digest(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Expect::Absent,
            Err(error) => return Err(io(&live, error)),
        };
        let path = agora_game_api::RelPath::strict("instance_manifest.json")
            .map_err(|error| refused(error.to_string()))?;
        self.write(&path, &text, expect)
    }

    /// The stage folder, for a caller that must prepare it (grant a right the platform needs to
    /// move a protected file out of it, for example). Nothing in it is the caller's to change.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stage the removal of the file at `path`. A path with nothing there is not an error.
    pub fn remove(&mut self, path: &StrictRelPath, expect: Expect) -> LauncherResult<()> {
        self.claim(path)?;
        self.pending.push(Pending::Remove {
            path: path.clone(),
            expect,
        });
        Ok(())
    }

    /// Stage the rename of the file at `from` to `to`, which must be free.
    pub fn rename(&mut self, from: &StrictRelPath, to: &StrictRelPath) -> LauncherResult<()> {
        self.claim(from)?;
        self.claim(to)?;
        self.pending.push(Pending::Rename {
            from: from.clone(),
            to: to.clone(),
        });
        Ok(())
    }

    /// Stop the next commit at `boundary`, as a crash would: nothing is rolled back and the stage
    /// is left for recovery (test seam).
    #[cfg(any(test, feature = "test-support"))]
    pub fn crash_at(&mut self, boundary: Boundary) {
        self.crash = Some(boundary);
    }

    /// Make the staged changes: all of them, or (after recovery) none.
    pub fn commit(self) -> LauncherResult<ApplyReport> {
        self.commit_inner(None)
    }

    /// Make the staged changes together with a database change. The files are moved first; then
    /// `change` and a receipt row for this apply are committed in **one** SQLite transaction on
    /// `conn`, which is the commit point: a crash before it leaves the old files (and no change), a
    /// crash after it is finished by recovery. If `change` or the transaction fails, the files are
    /// put back.
    pub fn commit_with_receipt<'c>(
        self,
        conn: &'c mut rusqlite::Connection,
        change: impl FnOnce(&rusqlite::Transaction<'_>) -> LauncherResult<()> + 'c,
    ) -> LauncherResult<ApplyReport> {
        self.commit_inner(Some(Box::new(ReceiptCommit {
            conn,
            change: Some(Box::new(change)),
        })))
    }

    fn commit_inner(
        mut self,
        mut receipt: Option<Box<ReceiptCommit<'_>>>,
    ) -> LauncherResult<ApplyReport> {
        self.journal.receipt = receipt.is_some();
        if self.crash == Some(Boundary::BeforePlan) {
            return self.crashed();
        }
        let (ops, report) = match self.plan() {
            Ok(planned) => planned,
            Err(error) => {
                // Nothing in the instance has changed: drop the stage the ordinary way.
                return Err(error);
            }
        };
        self.journal.ops = ops;
        recovery::write(&self.journal_path, &self.journal)?;
        if self.crash == Some(Boundary::AfterPlan) {
            return self.crashed();
        }

        let mut moves = 0usize;
        let applied = (|| -> LauncherResult<()> {
            for op in self.journal.ops.clone() {
                for (from, to) in forward_moves(&self.root, &self.dir, &op) {
                    move_file(&self.root, &from, &to)?;
                    moves += 1;
                    if self.crash == Some(Boundary::AfterMove(moves)) {
                        return Err(crash_error());
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = applied {
            if self.crash.is_some() {
                return self.crashed();
            }
            // A real failure before the commit point: roll back now, through the same recovery
            // that would run after a crash.
            self.settled = true;
            let outcomes = self.settle_here(None, receipt.as_deref());
            let undone = outcomes
                .iter()
                .any(|outcome| matches!(outcome, StagedRecovery::RolledBack { .. }));
            return Err(if undone {
                LauncherError::Generic {
                    code: "ERR_STAGED_APPLY".into(),
                    message: format!("{error}; nothing in the instance was changed."),
                }
            } else {
                LauncherError::Generic {
                    code: "ERR_STAGED_APPLY".into(),
                    message: format!(
                        "{error}; putting the instance back did not finish ({outcomes:?}). It will \
                         be tried again the next time Agora starts or changes this instance."
                    ),
                }
            });
        }

        if let Some(receipt) = receipt.as_deref_mut() {
            // The commit point: the caller's change and this apply's receipt, in one transaction.
            if let Err(error) = receipt.commit(&self.journal.envelope.operation_id) {
                self.settled = true;
                let outcomes = self.settle_here(None, Some(&*receipt));
                let undone = outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, StagedRecovery::RolledBack { .. }));
                return Err(LauncherError::Generic {
                    code: "ERR_STAGED_APPLY".into(),
                    message: if undone {
                        format!("{error}; nothing in the instance was changed.")
                    } else {
                        format!(
                            "{error}; putting the instance back did not finish ({outcomes:?}). It \
                             will be tried again the next time Agora starts."
                        )
                    },
                });
            }
            if self.crash == Some(Boundary::AfterReceipt) {
                self.settled = true;
                return Err(crash_error());
            }
        }

        // The commit point (for an apply without a receipt), and the record that it was reached.
        self.journal.envelope.state = State::Committed;
        recovery::write(&self.journal_path, &self.journal)?;
        self.settled = true;
        if self.crash == Some(Boundary::AfterCommit) {
            return Err(crash_error());
        }
        let cleanup_crash = match self.crash {
            Some(Boundary::AfterCleanup(n)) => Some(n),
            _ => None,
        };
        let outcomes = self.settle_here(cleanup_crash, receipt.as_deref());
        if cleanup_crash.is_some() {
            return Err(crash_error());
        }
        if let Some(problem) = outcomes.iter().find(|outcome| {
            matches!(
                outcome,
                StagedRecovery::NeedsHand(_) | StagedRecovery::Busy(_)
            )
        }) {
            // The change is made; only its cleanup is left, and the next settle finishes it.
            eprintln!(
                "[staged-apply] the change was made, but its cleanup did not finish: {problem:?}"
            );
        }
        Ok(report)
    }

    /// Settle this instance's applies under the guard this stage holds, judging a receipt apply by
    /// the connection `receipt` commits on.
    fn settle_here(
        &self,
        cleanup_crash: Option<usize>,
        receipt: Option<&ReceiptCommit<'_>>,
    ) -> Vec<StagedRecovery> {
        match receipt {
            Some(receipt) => {
                let conn: &rusqlite::Connection = &*receipt.conn;
                settle(
                    self.guard.lock(),
                    self.guard.instance_id(),
                    &self.root,
                    cleanup_crash,
                    Some(conn),
                )
            }
            None => settle(
                self.guard.lock(),
                self.guard.instance_id(),
                &self.root,
                cleanup_crash,
                None,
            ),
        }
    }

    fn crashed(mut self) -> LauncherResult<ApplyReport> {
        self.settled = true;
        Err(crash_error())
    }

    /// Turn the staged changes into journaled moves, deciding where each replaced or removed file
    /// goes. Nothing in the instance changes here.
    fn plan(&mut self) -> LauncherResult<(Vec<Op>, ApplyReport)> {
        let mut ops = Vec::new();
        let mut report = ApplyReport::default();
        let mut aside_taken = Vec::<String>::new();
        let pending = std::mem::take(&mut self.pending);
        for change in &pending {
            match change {
                Pending::Write {
                    path,
                    staged,
                    expect,
                } => {
                    let present = inspect(&self.root, path.as_str())?;
                    let previous = match (present, expect) {
                        (None, _) => None,
                        (Some(_), Expect::Absent) => {
                            return Err(refused(format!(
                                "{} is already in the instance, so it was not added. Nothing was \
                                 changed.",
                                path.as_str()
                            )))
                        }
                        (Some(()), expect) => {
                            Some(self.previous_for(path, expect, &mut aside_taken, &mut report)?)
                        }
                    };
                    ops.push(Op::Write {
                        path: path.as_str().to_string(),
                        staged: staged.clone(),
                        previous,
                    });
                }
                Pending::Remove { path, expect } => {
                    if inspect(&self.root, path.as_str())?.is_none() {
                        continue;
                    }
                    let previous =
                        self.previous_for(path, expect, &mut aside_taken, &mut report)?;
                    ops.push(Op::Remove {
                        path: path.as_str().to_string(),
                        previous,
                    });
                }
                Pending::Rename { from, to } => {
                    if inspect(&self.root, from.as_str())?.is_none() {
                        return Err(refused(format!(
                            "{} is not in the instance, so it was not renamed. Nothing was changed.",
                            from.as_str()
                        )));
                    }
                    if inspect(&self.root, to.as_str())?.is_some() {
                        return Err(refused(format!(
                            "{} is already in the instance, so {} was not renamed to it. Nothing \
                             was changed.",
                            to.as_str(),
                            from.as_str()
                        )));
                    }
                    ops.push(Op::Rename {
                        from: from.as_str().to_string(),
                        to: to.as_str().to_string(),
                    });
                }
            }
        }
        self.pending = pending;
        Ok((ops, report))
    }

    /// Where the file at `path` goes when it is replaced or removed: the stage if it is Agora's,
    /// aside if it is the user's.
    fn previous_for(
        &mut self,
        path: &StrictRelPath,
        expect: &Expect,
        aside_taken: &mut Vec<String>,
        report: &mut ApplyReport,
    ) -> LauncherResult<Previous> {
        let live = self.root.join(path.as_str());
        let agoras = match expect {
            Expect::Agora(recorded) => {
                let actual = hash_file(&live)?;
                actual.eq_ignore_ascii_case(recorded.trim())
            }
            Expect::Absent | Expect::Unknown => false,
        };
        if agoras {
            return Ok(Previous::Backup {
                name: self.new_name("old"),
            });
        }
        let aside = free_aside(&self.root, path.as_str(), aside_taken)?;
        report.kept_aside.push(KeptAside {
            path: path.clone(),
            kept_at: self.root.join(&aside),
        });
        aside_taken.push(aside.clone());
        Ok(Previous::Aside { path: aside })
    }

    fn claim(&self, path: &StrictRelPath) -> LauncherResult<()> {
        let spelling = path.as_str();
        let reserved = |prefix: &str| {
            spelling.eq_ignore_ascii_case(prefix)
                || spelling
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}/", prefix.to_ascii_lowercase()))
        };
        if reserved(STAGE_DIR) || reserved(ASIDE_DIR) {
            return Err(refused(format!(
                "{spelling} is inside a folder Agora keeps for itself"
            )));
        }
        let taken = self.pending.iter().any(|change| {
            let paths: Vec<&StrictRelPath> = match change {
                Pending::Write { path, .. } | Pending::Remove { path, .. } => vec![path],
                Pending::Rename { from, to } => vec![from, to],
            };
            paths
                .into_iter()
                .any(|other| other.as_str().eq_ignore_ascii_case(spelling))
        });
        if taken {
            return Err(refused(format!(
                "{spelling} is already part of this change"
            )));
        }
        // A link along the way would write outside the instance: refuse it now, not at commit.
        inspect(&self.root, spelling).map(|_| ())
    }

    fn new_name(&mut self, kind: &str) -> String {
        self.next_name += 1;
        format!("{kind}-{}", self.next_name)
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Not committed: the instance never changed. Settling removes the stage and its journal.
        let _ = settle(
            self.guard.lock(),
            self.guard.instance_id(),
            &self.root,
            None,
            None,
        );
    }
}

/// The moves that make `op`, in order.
fn forward_moves(root: &Path, dir: &Path, op: &Op) -> Vec<(PathBuf, PathBuf)> {
    match op {
        Op::Write {
            path,
            staged,
            previous,
        } => {
            let live = root.join(path);
            let mut moves = Vec::new();
            if let Some(previous) = previous {
                moves.push((live.clone(), previous_location(root, dir, previous)));
            }
            moves.push((dir.join(staged), live));
            moves
        }
        Op::Remove { path, previous } => {
            vec![(root.join(path), previous_location(root, dir, previous))]
        }
        Op::Rename { from, to } => vec![(root.join(from), root.join(to))],
    }
}

fn previous_location(root: &Path, dir: &Path, previous: &Previous) -> PathBuf {
    match previous {
        Previous::Backup { name } => dir.join(name),
        Previous::Aside { path } => root.join(path),
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Settle every apply a crash left unresolved in the instance at `root`, without waiting: an
/// instance another Agora is changing right now is reported busy and left alone.
///
/// Every adapter's startup pass calls this through its game package, and [`Stage::begin`] calls it
/// before every apply.
pub fn recover(locks: &LockManager, instance_id: &str, root: &Path) -> Vec<StagedRecovery> {
    recover_inner(locks, instance_id, root, None, None)
}

/// [`recover`], judging an apply whose commit point is a receipt by the receipts in `conn` (the
/// local state database). Without the database such an apply needs a hand: rolling it back could
/// pair old files with a database change that committed.
pub fn recover_with_receipts(
    locks: &LockManager,
    instance_id: &str,
    root: &Path,
    conn: &rusqlite::Connection,
) -> Vec<StagedRecovery> {
    recover_inner(locks, instance_id, root, None, Some(conn))
}

/// Settle the interrupted applies of every instance in the context's instances folder, for the
/// startup pass: what was done comes back as `actions`, what could not be as `warnings`.
pub fn recover_instances(ctx: &crate::ctx::Ctx) -> (Vec<String>, Vec<String>) {
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let root = ctx.paths.instances_root();
    // Opened only when an instance has a stage: an apply whose commit point is a receipt is judged
    // by it, and without it such an apply is left for a hand rather than guessed.
    let mut conn: Option<rusqlite::Connection> = None;
    let mut conn_tried = false;
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (actions, warnings),
        Err(error) => {
            warnings.push(format!(
                "Agora could not look for interrupted changes in {}: {error}",
                root.display()
            ));
            return (actions, warnings);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "Agora could not look for interrupted changes in {}: {error}",
                    root.display()
                ));
                continue;
            }
        };
        let instance = entry.path();
        match std::fs::symlink_metadata(&instance) {
            Ok(kind) if kind.is_dir() && !agora_game_api::is_a_reparse_point(&kind) => {}
            _ => continue,
        }
        if !has_stage(&instance) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        if !conn_tried {
            conn_tried = true;
            match crate::db::local_state_connection(&ctx.paths.local_state_db()) {
                Ok(opened) => conn = Some(opened),
                Err(error) => warnings.push(format!(
                    "Agora could not open the local database to finish interrupted changes: {error}"
                )),
            }
        }
        let outcomes = match &conn {
            Some(conn) => recover_with_receipts(&ctx.lock_manager, &id, &instance, conn),
            None => recover(&ctx.lock_manager, &id, &instance),
        };
        for outcome in outcomes {
            match outcome {
                StagedRecovery::RolledBack { .. } => actions.push(format!(
                    "Put instance '{id}' back as it was before a change that was interrupted."
                )),
                StagedRecovery::Finished { .. } => actions.push(format!(
                    "Finished a change to instance '{id}' that was interrupted after it was made."
                )),
                StagedRecovery::Busy(reason) => warnings.push(format!(
                    "An interrupted change to instance '{id}' was left alone: {reason}"
                )),
                StagedRecovery::NeedsHand(reason) => warnings.push(format!(
                    "An interrupted change to instance '{id}' needs a hand: {reason}"
                )),
            }
        }
    }
    (actions, warnings)
}

/// [`recover`] cut off after `moves` of its rollback moves, as a second crash would (test seam).
#[cfg(any(test, feature = "test-support"))]
pub fn recover_crashing_after(
    locks: &LockManager,
    instance_id: &str,
    root: &Path,
    moves: usize,
) -> Vec<StagedRecovery> {
    recover_inner(locks, instance_id, root, Some(moves), None)
}

fn recover_inner(
    locks: &LockManager,
    instance_id: &str,
    root: &Path,
    crash_after: Option<usize>,
    receipts: Option<&rusqlite::Connection>,
) -> Vec<StagedRecovery> {
    if !has_stage(root) {
        return Vec::new();
    }
    let resource = LockResource::Instance(instance_id.to_string());
    let lock = match locks.acquire_with_timeout_typed(
        resource,
        "settling an interrupted change",
        Duration::ZERO,
        None,
    ) {
        Ok(lock) => lock,
        Err(LockRefusal::Held(error)) => {
            return vec![StagedRecovery::Busy(format!(
                "another Agora is changing this instance ({error}); it will be checked again the \
                 next time Agora starts"
            ))]
        }
        Err(LockRefusal::Unavailable(error)) => {
            return vec![StagedRecovery::NeedsHand(format!(
                "the instance lock could not be checked: {error}"
            ))]
        }
    };
    settle_inner(&lock, instance_id, root, crash_after, None, receipts)
}

/// Settle under a lock the caller holds. `cleanup_crash` stops a committed cleanup after that many
/// backup removals (test seam for [`Boundary::AfterCleanup`]).
fn settle(
    lock: &LockGuard,
    instance_id: &str,
    root: &Path,
    cleanup_crash: Option<usize>,
    receipts: Option<&rusqlite::Connection>,
) -> Vec<StagedRecovery> {
    settle_inner(lock, instance_id, root, None, cleanup_crash, receipts)
}

fn has_stage(root: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(stage_root(root)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn settle_inner(
    lock: &LockGuard,
    instance_id: &str,
    root: &Path,
    undo_crash: Option<usize>,
    cleanup_crash: Option<usize>,
    receipts: Option<&rusqlite::Connection>,
) -> Vec<StagedRecovery> {
    let base = stage_root(root);
    let entries = match std::fs::read_dir(&base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            return vec![StagedRecovery::NeedsHand(format!(
                "{} could not be read: {error}",
                base.display()
            ))]
        }
    };
    let mut journals = Vec::new();
    let mut folders = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return vec![StagedRecovery::NeedsHand(format!(
                    "{} could not be read: {error}",
                    base.display()
                ))]
            }
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        match std::fs::symlink_metadata(&path) {
            Ok(kind) if agora_game_api::is_a_reparse_point(&kind) => {
                return vec![StagedRecovery::NeedsHand(format!(
                    "{} is a link inside Agora's stage folder",
                    path.display()
                ))]
            }
            Ok(kind) if kind.is_dir() => folders.push(name),
            Ok(_) if name.ends_with(".json") && !name.contains(".tmp") => journals.push(path),
            Ok(_) => {}
            Err(error) => {
                return vec![StagedRecovery::NeedsHand(format!(
                    "{} could not be examined: {error}",
                    path.display()
                ))]
            }
        }
    }
    journals.sort();
    let mut outcomes = Vec::new();
    for journal in &journals {
        outcomes.push(settle_one(
            lock,
            instance_id,
            root,
            journal,
            undo_crash,
            cleanup_crash,
            receipts,
        ));
    }
    // A stage whose journal was never written (a crash inside `begin`) holds at most its marker:
    // nothing in the instance was changed for it.
    for folder in folders {
        if journals.iter().any(|journal| {
            journal
                .file_stem()
                .is_some_and(|stem| stem == folder.as_str())
        }) {
            continue;
        }
        let dir = base.join(&folder);
        let only_marker = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .all(|entry| entry.file_name() == MARKER)
            })
            .unwrap_or(false);
        if only_marker {
            let _ = std::fs::remove_file(dir.join(MARKER));
            let _ = std::fs::remove_dir(&dir);
        } else {
            outcomes.push(StagedRecovery::NeedsHand(format!(
                "{} holds files but no journal, so Agora cannot tell what they were for. Nothing \
                 was changed.",
                dir.display()
            )));
        }
    }
    let _ = std::fs::remove_dir(&base);
    outcomes
}

#[allow(clippy::too_many_arguments)]
fn settle_one(
    lock: &LockGuard,
    instance_id: &str,
    root: &Path,
    journal_path: &Path,
    undo_crash: Option<usize>,
    cleanup_crash: Option<usize>,
    receipts: Option<&rusqlite::Connection>,
) -> StagedRecovery {
    let resource = LockResource::Instance(instance_id.to_string());
    let read = recovery::read::<State>(journal_path);
    // A journal this very process wrote under the lock it now holds again: that invocation has
    // stopped (the lock was released), so its writer being alive proves nothing.
    let self_writer = match &read {
        JournalRead::Valid(journal) => journal.envelope().writer.as_ref().and_then(|writer| {
            (writer.pid == std::process::id()
                && process_identity::capture_self().as_ref() == Some(writer))
            .then(|| journal.envelope().operation_id.clone())
        }),
        _ => None,
    };
    let decision = recovery::decide_holding_reading(
        lock,
        &resource,
        Some(journal_path),
        || read,
        &NoContentProof,
        &process_identity::OsProcesses,
        self_writer.as_deref(),
    );
    let mut capability = match decision {
        Decision::Nothing => {
            return StagedRecovery::NeedsHand(format!(
                "{} disappeared while it was being read",
                journal_path.display()
            ))
        }
        Decision::Busy(reason) => return StagedRecovery::Busy(reason),
        Decision::NeedsHand(reason) => return StagedRecovery::NeedsHand(reason),
        Decision::Act(capability) => capability,
    };
    if !capability.left_alone().is_empty() {
        return StagedRecovery::NeedsHand(format!(
            "the folders this change recorded are not the ones on disk now ({:?}). Nothing was \
             changed; the journal was kept.",
            capability.left_alone()
        ));
    }
    let journal: StagedJournal = match capability.payload() {
        Ok(journal) => journal,
        Err(error) => return StagedRecovery::NeedsHand(error.to_string()),
    };
    if journal.instance_id != instance_id {
        return StagedRecovery::NeedsHand(format!(
            "the journal belongs to instance {}, not {instance_id}",
            journal.instance_id
        ));
    }
    let operation = journal.envelope.operation_id.clone();
    let dir = stage_dir(root, &operation);
    // An apply whose commit point is a receipt committed if, and only if, its receipt is there; the
    // `committed` record only follows it. Without the receipts, or with ones that cannot be read,
    // it cannot be judged, and is left alone.
    let committed = match (journal.envelope.state, journal.receipt) {
        (State::Committed, _) => true,
        (State::Applying, false) => false,
        (State::Applying, true) => match receipts.map(|conn| receipt_present(conn, &operation)) {
            Some(Ok(present)) => present,
            Some(Err(error)) => {
                return StagedRecovery::NeedsHand(format!(
                    "whether change {operation} reached its commit point is recorded in the local \
                     database, which could not be read: {error}. Nothing was changed; the journal \
                     was kept."
                ))
            }
            None => {
                return StagedRecovery::NeedsHand(format!(
                    "whether change {operation} reached its commit point is recorded in the local \
                     database, which this recovery was not given. Nothing was changed; the journal \
                     was kept."
                ))
            }
        },
    };
    let result = if committed {
        finish_cleanup(&mut capability, &dir, &journal, cleanup_crash)
    } else {
        roll_back(&mut capability, root, &dir, &journal, undo_crash)
            .and_then(|()| clear_stage(&capability, &dir))
    };
    match result {
        Ok(()) => {}
        Err(error) => return StagedRecovery::NeedsHand(error.to_string()),
    }
    if let Err(error) = capability.finish() {
        return StagedRecovery::NeedsHand(error.to_string());
    }
    let _ = std::fs::remove_dir(&dir);
    if committed && journal.receipt {
        // The apply is finished: its receipt has done its job. A row left behind is harmless.
        if let Some(conn) = receipts {
            let _ = forget_receipt(conn, &operation);
        }
    }
    if committed {
        StagedRecovery::Finished { operation }
    } else {
        StagedRecovery::RolledBack { operation }
    }
}

/// Undo the journal's moves, newest first, judging each one by what is on disk.
fn roll_back(
    capability: &mut RecoveryCapability<'_, State>,
    root: &Path,
    dir: &Path,
    journal: &StagedJournal,
    crash_after: Option<usize>,
) -> LauncherResult<()> {
    let mut moves = 0usize;
    let mut undo = |capability: &mut RecoveryCapability<'_, State>,
                    from: &Path,
                    to: &Path|
     -> LauncherResult<()> {
        capability.rename_file_below(root, from, to)?;
        moves += 1;
        if crash_after == Some(moves) {
            return Err(crash_error());
        }
        Ok(())
    };
    for op in journal.ops.iter().rev() {
        match op {
            Op::Rename { from, to } => {
                let (from, to) = (root.join(from), root.join(to));
                if present(&to)? && !present(&from)? {
                    undo(capability, &to, &from)?;
                }
            }
            Op::Remove { path, previous } => {
                let live = root.join(path);
                let kept = previous_location(root, dir, previous);
                if present(&kept)? && !present(&live)? {
                    undo(capability, &kept, &live)?;
                }
            }
            Op::Write {
                path,
                staged,
                previous,
            } => {
                let live = root.join(path);
                let staged = dir.join(staged);
                if !present(&staged)? && present(&live)? {
                    // The new file was made live: take it back into the stage.
                    undo(capability, &live, &staged)?;
                }
                if let Some(previous) = previous {
                    let kept = previous_location(root, dir, previous);
                    if present(&kept)? && !present(&live)? {
                        undo(capability, &kept, &live)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// After the commit point: delete the stage's backups of Agora's old files. Files kept aside are
/// below the instance root, not in the stage, and are never touched.
fn finish_cleanup(
    capability: &mut RecoveryCapability<'_, State>,
    dir: &Path,
    journal: &StagedJournal,
    crash_after: Option<usize>,
) -> LauncherResult<()> {
    let mut removed = 0usize;
    for op in &journal.ops {
        let backup = match op {
            Op::Write {
                previous: Some(Previous::Backup { name }),
                ..
            }
            | Op::Remove {
                previous: Previous::Backup { name },
                ..
            } => dir.join(name),
            _ => continue,
        };
        if present(&backup)? {
            capability.remove_file(dir, &backup)?;
            removed += 1;
            if crash_after == Some(removed) {
                return Err(crash_error());
            }
        }
    }
    clear_stage(capability, dir)
}

/// Remove what is left in the stage (staged files never made live, or taken back), except the
/// ownership marker, which [`RecoveryCapability::finish`] removes.
fn clear_stage(capability: &RecoveryCapability<'_, State>, dir: &Path) -> LauncherResult<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io(dir, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| io(dir, error))?;
        if entry.file_name() == MARKER {
            continue;
        }
        capability.remove_file(dir, &entry.path())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------

/// The caller's own database change, run inside the receipt's transaction.
type DatabaseChange<'c> = Box<dyn FnOnce(&rusqlite::Transaction<'_>) -> LauncherResult<()> + 'c>;

/// A caller's database change, committed with this apply's receipt as its commit point.
struct ReceiptCommit<'c> {
    conn: &'c mut rusqlite::Connection,
    change: Option<DatabaseChange<'c>>,
}

impl ReceiptCommit<'_> {
    fn commit(&mut self, operation: &str) -> LauncherResult<()> {
        let change = self
            .change
            .take()
            .ok_or_else(|| refused("the database change of this apply already ran".into()))?;
        let tx = self.conn.transaction().map_err(database)?;
        change(&tx)?;
        tx.execute(
            "INSERT INTO staged_apply_receipts (operation_id, created_at) VALUES (?1, ?2)",
            rusqlite::params![operation, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(database)?;
        tx.commit().map_err(database)
    }
}

/// Whether the receipt of `operation` is in the database. A database that cannot answer is an
/// error, never "no receipt".
fn receipt_present(conn: &rusqlite::Connection, operation: &str) -> LauncherResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM staged_apply_receipts WHERE operation_id = ?1)",
        [operation],
        |row| row.get::<_, bool>(0),
    )
    .map_err(database)
}

fn forget_receipt(conn: &rusqlite::Connection, operation: &str) -> LauncherResult<()> {
    conn.execute(
        "DELETE FROM staged_apply_receipts WHERE operation_id = ?1",
        [operation],
    )
    .map(|_| ())
    .map_err(database)
}

fn database(error: rusqlite::Error) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_STAGED_APPLY".into(),
        message: format!("the local database: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Whether a plain file is at `path`. A folder, a link or a path the OS will not answer about is
/// an error, never "absent".
fn present(path: &Path) -> LauncherResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(kind) if kind.is_file() && !agora_game_api::is_a_reparse_point(&kind) => Ok(true),
        Ok(_) => Err(refused(format!("{} is not a plain file", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(path, error)),
    }
}

/// What is at `relative` below `root`: `Some(())` for a plain file, `None` for nothing. A link or a
/// non-folder along the way, a folder or link at the end, or an unanswered question is an error.
fn inspect(root: &Path, relative: &str) -> LauncherResult<Option<()>> {
    let mut current = root.to_path_buf();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        current.push(part);
        let kind = match std::fs::symlink_metadata(&current) {
            Ok(kind) => kind,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(&current, error)),
        };
        if agora_game_api::is_a_reparse_point(&kind) {
            return Err(refused(format!(
                "{} is a link, and Agora does not write through links in an instance",
                current.display()
            )));
        }
        let last = parts.peek().is_none();
        if last {
            if kind.is_file() {
                return Ok(Some(()));
            }
            return Err(refused(format!("{} is not a file", current.display())));
        }
        if !kind.is_dir() {
            return Err(refused(format!("{} is not a folder", current.display())));
        }
    }
    Ok(None)
}

/// Move a plain file, both ends below `root`, without following a link (the forward path; recovery
/// uses the capability's own copy of this rule).
fn move_file(root: &Path, from: &Path, to: &Path) -> LauncherResult<()> {
    recovery::move_file_below(root, from, to)
}

fn free_aside(root: &Path, relative: &str, taken: &[String]) -> LauncherResult<String> {
    let base = format!("{ASIDE_DIR}/{relative}");
    let mut candidate = base.clone();
    let mut counter = 1;
    loop {
        let used = taken
            .iter()
            .any(|other| other.eq_ignore_ascii_case(&candidate));
        if !used && inspect(root, &candidate)?.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{base}.{counter}");
        counter += 1;
    }
}

fn plain_directory(path: &Path) -> LauncherResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(kind) if kind.is_dir() && !agora_game_api::is_a_reparse_point(&kind) => Ok(()),
        Ok(_) => Err(refused(format!("{} is not a plain folder", path.display()))),
        Err(error) => Err(io(path, error)),
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> LauncherResult<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path).map_err(|error| io(path, error))?;
    file.write_all(bytes).map_err(|error| io(path, error))?;
    file.sync_all().map_err(|error| io(path, error))
}

fn sync_file(path: &Path) -> LauncherResult<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| io(path, error))
}

fn hash_file(path: &Path) -> LauncherResult<String> {
    let mut file = std::fs::File::open(path).map_err(|error| io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn refused(message: String) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_STAGED_APPLY".into(),
        message,
    }
}

fn io(path: &Path, error: std::io::Error) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_STAGED_APPLY".into(),
        message: format!("{}: {error}", path.display()),
    }
}

fn crash_error() -> LauncherError {
    LauncherError::Generic {
        code: "ERR_STAGED_APPLY".into(),
        message: "stopped by the test seam, as a crash would".into(),
    }
}

#[cfg(test)]
#[path = "staged_apply_tests.rs"]
mod tests;
