//! What `agora doctor` says about unresolved crash journals (MASTER_SPEC §32.11).
//!
//! Startup recovery settles a journal when it can, and the user sees the result. When it *cannot*
//! — because the journal needs a hand, or because its writer may still be running — the next start
//! is the first time anyone explains why, in one line, mixed in with everything else. This module
//! is the answer a user can ask for on purpose: one entry per journal found, with its kind, the
//! resource it covers, its state, and the decision recovery would take.
//!
//! # Read-only, all the way
//!
//! A report must not change what it reports on. Nothing here takes an operation's lock
//! ([`LockManager::probe`] asks without taking), writes, renames or deletes a file, clears a stale
//! lease, migrates the database or switches its journal mode. Every "is it there?" is
//! [`agora_game_api::probe`], so a path that is a directory, unreadable or a dangling link is
//! reported as unreadable rather than skipped. The database is opened with `SQLITE_OPEN_READ_ONLY`
//! for the journals it holds. The proof that this holds is a test that snapshots the whole data root
//! — file list, sizes and bytes — around the report.
//!
//! # Fail closed
//!
//! A journal that cannot be read is listed with its path and its error, never skipped. A kind whose
//! journals cannot be enumerated at all is reported in [`RecoveryReport::not_searched`] with the
//! reason — never as "no journals" — and one kind failing never speaks for another. A missing
//! database is named as such, and the journals an earlier build left in `plugins/previous/` are
//! still listed, because they do not need one.
//!
//! # Which journals, and why some are not decided
//!
//! * **plugin installs** and **launcher imports** live in the local database;
//! * **tool swaps** and **user-files sessions** are files under the data root;
//! * **a pending database recovery** and **Recently deleted restores** are markers rather than
//!   journals, and they are startup blockers all the same.
//!
//! The plugin install decides through the one recovery decision
//! ([`crate::recovery::decide_readonly`]) *and* its own refusal checks
//! (`plugins::install::journal_report`), so its answer is the answer the next start would give.
//! The other kinds still keep their own recovery code (§32.11 D1 moves them onto the envelope one
//! at a time); this report therefore says [`RecoveryOutcome::Undecided`] for them rather than
//! re-implementing their rules a second time, and reports only what it can prove without them: that
//! a journal's lock is held — or that it could not be asked, which may mean the same thing — and
//! that a journal could not be read at all.

use std::path::{Path, PathBuf};

use agora_game_api::probe::{probe, PresentKind, Probe};
use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::lock_manager::{LockManager, LockProbe, LockResource};
use crate::process_identity::{OsProcesses, ProcessView};

pub use crate::recovery::RecoveryOutcome;

type Result<T> = std::result::Result<T, String>;

/// Which operation a journal belongs to. One name per journal kind `agora doctor` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalKind {
    /// Installing, updating, rolling back or uninstalling a plugin package.
    PluginInstall,
    /// Importing an instance from another launcher.
    InstanceImport,
    /// A tool running on the real install's `Data` folder.
    ToolSwap,
    /// Game files swapped in for a run.
    UserFiles,
    /// The database's own crash recovery: a damaged file moved aside, its restore unfinished.
    DatabaseRecovery,
    /// A delete or a restore into, or out of, Recently deleted.
    RecentlyDeleted,
}

impl JournalKind {
    /// The stable name a report and its JSON share.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalKind::PluginInstall => "plugin-install",
            JournalKind::InstanceImport => "instance-import",
            JournalKind::ToolSwap => "tool-swap",
            JournalKind::UserFiles => "user-files",
            JournalKind::DatabaseRecovery => "database-recovery",
            JournalKind::RecentlyDeleted => "recently-deleted",
        }
    }
}

/// One journal found, and what recovery would do with it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub kind: JournalKind,
    /// What the journal covers: the plugin id, the instance id, the game and store, or the file.
    pub resource: String,
    /// Where the journal is: its file, or the local database that holds its row.
    pub path: String,
    /// This kind's own state, when the journal was readable.
    pub state: Option<String>,
    /// What recovery would do.
    pub outcome: RecoveryOutcome,
    /// Why, in a sentence — including the parse error of a journal that could not be read.
    pub reason: String,
    /// The command that resolves or overrides this journal now, when there is one.
    pub command: Option<String>,
}

/// A kind whose journals could not be searched at all, with the reason.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindNotSearched {
    pub kind: JournalKind,
    /// Why nothing could be said about this kind's journals. Never "there are none".
    pub reason: String,
    /// Whether this is something a person has to look at — a kind that could not be read — rather
    /// than a fact about a data root that is simply new (there is no database yet, so nothing has
    /// been written in it). Only a blocking kind counts as a problem.
    pub blocking: bool,
}

/// The whole read-only recovery report.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryReport {
    /// Every journal found, one entry each — including the ones that could not be read.
    pub entries: Vec<JournalEntry>,
    /// Kinds that could not be searched at all.
    pub not_searched: Vec<KindNotSearched>,
    /// Plain facts about what was searched: "no local database yet", and the like.
    pub notes: Vec<String>,
}

impl RecoveryReport {
    /// Whether anything here needs a person before the next start can settle it.
    ///
    /// A kind that could not be read counts: it may be hiding the journal that blocks startup, and
    /// "could not tell" is never the same as "nothing found". A kind with nothing in it yet does
    /// not: a fresh data root is not a problem.
    pub fn needs_a_hand(&self) -> bool {
        self.not_searched.iter().any(|missing| missing.blocking)
            || self
                .entries
                .iter()
                .any(|entry| entry.outcome == RecoveryOutcome::NeedsHand)
    }

    /// Whether nothing at all was found: no journal, and no kind left unsearched.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.not_searched.is_empty()
    }
}

/// Every journal this data root has, decided without touching anything.
pub fn report(paths: &AppPaths) -> RecoveryReport {
    report_with(paths, &OsProcesses)
}

/// [`report`], with the process table the writers' liveness is read from, so a test can make each
/// "could not tell" happen.
pub fn report_with(paths: &AppPaths, view: &dyn ProcessView) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let pending = database_recovery(paths, &mut report);
    database_journals(
        paths,
        view,
        &mut report,
        pending_claims_restore(paths, &pending),
    );
    file_journals(paths, &mut report);
    recently_deleted(paths, &mut report);
    report
}

// ---------------------------------------------------------------------------
// The local database
// ---------------------------------------------------------------------------

/// What could be done with the local database file.
enum DatabaseAccess {
    /// Nothing at the path.
    Missing,
    /// An open, read-only connection.
    Usable(Box<rusqlite::Connection>),
    /// Something is there but could not be used, with the reason.
    CantTell(String),
}

/// Open the local database read-only, changing nothing.
fn database(paths: &AppPaths) -> DatabaseAccess {
    let db = paths.local_state_db();
    match probe(&db) {
        Probe::Absent => DatabaseAccess::Missing,
        Probe::CantTell(error) => DatabaseAccess::CantTell(format!(
            "{} could not be examined ({error}). The plugin-install and import journals are \
             unknown, which is not the same as there being none.",
            db.display()
        )),
        Probe::Present(kind) if kind != PresentKind::File => DatabaseAccess::CantTell(format!(
            "{} is not a plain file, so the plugin-install and import journals are unknown.",
            db.display()
        )),
        Probe::Present(_) => match crate::db::local_state_connection_read_only(&db) {
            Ok(conn) => match crate::db::recorded_schema_version(&conn) {
                // A schema this build does not know may give these tables meanings it does not
                // share. Reading them anyway would read rows as this version, which is exactly what
                // the next start refuses to do.
                Ok(version) if version > crate::db::LOCAL_STATE_SCHEMA_VERSION => {
                    DatabaseAccess::CantTell(format!(
                        "The database was written by a newer Agora (schema {version}; this build \
                         supports up to {}), so its journals are not read as this version's.",
                        crate::db::LOCAL_STATE_SCHEMA_VERSION
                    ))
                }
                Ok(_) => DatabaseAccess::Usable(Box::new(conn)),
                Err(error) => DatabaseAccess::CantTell(format!(
                    "The database's schema version could not be read ({error})."
                )),
            },
            Err(error) => DatabaseAccess::CantTell(format!(
                "{} could not be opened read-only ({error}). The plugin-install and import journals \
                 are unknown, which is not the same as there being none.",
                db.display()
            )),
        },
    }
}

/// The database-held journals, each kind reporting its own trouble.
fn database_journals(
    paths: &AppPaths,
    view: &dyn ProcessView,
    report: &mut RecoveryReport,
    recovery_pending: bool,
) {
    let locks = LockManager::new(paths.locks_root());
    let access = database(paths);
    let (conn, reason): (Option<rusqlite::Connection>, Option<String>) = match access {
        DatabaseAccess::Usable(conn) => (Some(*conn), None),
        DatabaseAccess::Missing => (
            None,
            Some(if recovery_pending {
                "There is no local database file beside its recovery marker: the next start \
                 finishes that recovery before it reads any journal."
                    .to_string()
            } else {
                "No local database yet: the next start creates it, and the plugin-install and \
                 import journals live in it."
                    .to_string()
            }),
        ),
        DatabaseAccess::CantTell(reason) => (None, Some(reason)),
    };
    if let Some(reason) = &reason {
        report.notes.push(reason.clone());
    }
    // Plugin installs: the database's rows when there is one, and always the journals an earlier
    // build left in `plugins/previous/`, which need no database at all. Without a database the
    // kind is still reported as not searched — the legacy stashes are listed, the rows cannot be.
    match crate::plugins::install::journal_report(conn.as_ref(), paths, view) {
        Ok(journals) => {
            for journal in journals {
                report.entries.push(JournalEntry {
                    kind: JournalKind::PluginInstall,
                    resource: journal.plugin_id.to_string(),
                    path: journal.path.display().to_string(),
                    state: journal.state.map(|state| state.as_str().to_string()),
                    outcome: journal.outcome,
                    reason: journal.reason,
                    command: journal.command,
                });
            }
            if let Some(reason) = &reason {
                report.not_searched.push(KindNotSearched {
                    kind: JournalKind::PluginInstall,
                    reason: reason.clone(),
                    blocking: !is_new_data_root(reason),
                });
            }
        }
        Err(error) => report.not_searched.push(KindNotSearched {
            kind: JournalKind::PluginInstall,
            reason: format!("The plugin-install journals could not be listed ({error})."),
            blocking: true,
        }),
    }
    // Launcher imports are only readable through the database.
    let Some(conn) = conn else {
        let reason = reason.unwrap_or_else(|| "The local database could not be read.".into());
        report.not_searched.push(KindNotSearched {
            kind: JournalKind::InstanceImport,
            blocking: !is_new_data_root(&reason),
            reason,
        });
        return;
    };
    match import_journals(&conn, paths, &locks) {
        Ok(entries) => report.entries.extend(entries),
        Err(reason) => report.not_searched.push(KindNotSearched {
            kind: JournalKind::InstanceImport,
            reason,
            blocking: true,
        }),
    }
}

/// How the reason for an unread database-backed kind starts when the data root is simply new.
const NEW_DATA_ROOT: &str = "No local database yet";

/// Whether a reason for "not searched" is simply that this data root is new.
///
/// A data root with no database has had no plugin installed and no instance imported, so nothing is
/// hidden by not reading it. Everything else — a file that cannot be read, a schema this build does
/// not know — may be hiding a journal, and is a problem.
fn is_new_data_root(reason: &str) -> bool {
    reason.starts_with(NEW_DATA_ROOT)
}

/// The launcher-import journals, listed with their facts and **no prediction**.
///
/// This deliberately does not decide them. Their recovery (`launcher_import_service.rs`,
/// `recover_one_job`) asks more questions than a journal row answers — the runtime lease, the
/// quarantine folder a failed update left, which marker each folder carries — and a second copy of
/// those rules here already drifted from them once. Until launcher import moves onto the envelope
/// (MASTER_SPEC §32.11 D1) the report says what it can see: which import, at which phase, over which
/// folders, and that the next start settles it.
fn import_journals(
    conn: &rusqlite::Connection,
    paths: &AppPaths,
    locks: &LockManager,
) -> Result<Vec<JournalEntry>> {
    let rows = crate::db::instance_import_job_rows(conn)
        .map_err(|error| format!("The import journals could not be read ({error})."))?;
    let mut entries = Vec::new();
    for row in rows {
        let job = match row.job {
            Ok(job) => job,
            // One unreadable row is that row's answer, not the listing's: the other rows are still
            // journals a person wants to see.
            Err(error) => {
                entries.push(JournalEntry {
                    resource: row
                        .instance_id
                        .unwrap_or_else(|| "a row that cannot be read".to_string()),
                    kind: JournalKind::InstanceImport,
                    path: paths.local_state_db().display().to_string(),
                    state: None,
                    outcome: RecoveryOutcome::NeedsHand,
                    reason: format!(
                        "This row of the import journal cannot be read ({error}), so neither what \
                         it covers nor what the next start would do with it is known. Nothing was \
                         changed."
                    ),
                    command: None,
                });
                continue;
            }
        };
        let mut entry = JournalEntry {
            kind: JournalKind::InstanceImport,
            resource: job.instance_id.clone(),
            // The folder recovery works in, which is what a person can look at.
            path: job.staging_path.clone(),
            state: Some(job.state.clone()),
            outcome: RecoveryOutcome::Undecided,
            reason: format!(
                "An import of instance {} is at \"{}\", over the staging folder {} and the instance \
                 folder {}. The next start settles it; if it keeps appearing, that import is stuck \
                 and nothing has been changed.",
                job.instance_id, job.state, job.staging_path, job.final_path
            ),
            command: None,
        };
        // The one thing that needs no rules from that kind: a running import holds the instance's
        // lock for its whole life, so a held lock — or one that cannot be asked about — means the
        // import may still be running, whatever its journal says.
        match locks.probe(&LockResource::Instance(job.instance_id.clone())) {
            LockProbe::Held(metadata) => {
                entry.outcome = RecoveryOutcome::Busy;
                entry.reason = format!(
                    "Another Agora holds this instance's lock right now{}, so an import or a run \
                     may still be using these folders. Nothing would be done; the next start \
                     retries.",
                    match metadata {
                        Some(metadata) => format!(
                            " ({} on {}, operation: {})",
                            metadata.program, metadata.hostname, metadata.operation
                        ),
                        None => String::new(),
                    }
                );
            }
            LockProbe::CantTell(why) => {
                entry.outcome = RecoveryOutcome::Busy;
                entry.reason = format!(
                    "Agora could not tell whether this instance's lock is held ({why}), so an \
                     import may still be running. Nothing was changed."
                );
            }
            LockProbe::Free => {}
        }
        entries.push(entry);
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// The database's own recovery
// ---------------------------------------------------------------------------

/// The pending database recovery, listed as the startup blocker it is.
///
/// Returns whether a recovery is genuinely pending, so the "no database yet" note can say the truth
/// instead of promising that the next start creates one.
fn database_recovery(paths: &AppPaths, report: &mut RecoveryReport) -> bool {
    let db = paths.local_state_db();
    match crate::db::pending_database_recovery(&db) {
        crate::db::PendingDatabaseRecovery::None => false,
        crate::db::PendingDatabaseRecovery::Pending {
            marker,
            identity_present,
        } => {
            report.entries.push(JournalEntry {
                kind: JournalKind::DatabaseRecovery,
                resource: db
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| db.display().to_string()),
                path: marker.display().to_string(),
                state: Some("restore-pending".into()),
                outcome: RecoveryOutcome::WouldFinish,
                reason: format!(
                    "The recovery marker was written before a damaged database was moved aside{}. \
                     The next start finishes the restore: it checks the database, keeps a damaged \
                     file aside (never deleting it), and restores the newest backup that passes the \
                     check — or starts with a fresh database when none does.",
                    if identity_present {
                        ", and its identity file is beside it so the marker is this database's"
                    } else {
                        ""
                    }
                ),
                command: None,
            });
            true
        }
        crate::db::PendingDatabaseRecovery::CantTell { marker, reason } => {
            report.entries.push(JournalEntry {
                kind: JournalKind::DatabaseRecovery,
                resource: db
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| db.display().to_string()),
                path: marker.display().to_string(),
                state: Some("restore-pending".into()),
                outcome: RecoveryOutcome::NeedsHand,
                reason: format!(
                    "A database recovery marker is beside {} and {reason}. Nothing was restored and \
                     nothing was changed.",
                    db.display()
                ),
                command: None,
            });
            false
        }
    }
}

/// Whether a pending recovery explains an absent database file.
fn pending_claims_restore(_paths: &AppPaths, pending: &bool) -> bool {
    *pending
}
// ---------------------------------------------------------------------------
// Journals that are files
// ---------------------------------------------------------------------------

/// Tool swaps and user-files sessions: files, each with the game/store lock its writer holds.
fn file_journals(paths: &AppPaths, report: &mut RecoveryReport) {
    let locks = LockManager::new(paths.locks_root());
    match read_children(paths.tool_swaps_root(), Some("json")) {
        Ok(found) => {
            for path in found {
                report
                    .entries
                    .push(file_entry(JournalKind::ToolSwap, &path, &locks));
            }
        }
        Err(reason) => report.not_searched.push(KindNotSearched {
            kind: JournalKind::ToolSwap,
            reason,
            blocking: true,
        }),
    }
    // `<data>/user-files/<game>_<store>/journal.json`, the same place `AppPaths` names. A session
    // folder with no journal is not a journal: the startup pass that reads these folders skips it
    // in exactly the same way, so the report does not invent one either. One that cannot be asked
    // about is listed as unreadable, never dropped.
    match read_children(paths.user_files_root(), None) {
        Ok(dirs) => {
            for dir in dirs {
                let journal = dir.join("journal.json");
                if matches!(probe(&journal), Probe::Absent) {
                    continue;
                }
                report
                    .entries
                    .push(file_entry(JournalKind::UserFiles, &journal, &locks));
            }
        }
        Err(reason) => report.not_searched.push(KindNotSearched {
            kind: JournalKind::UserFiles,
            reason,
            blocking: true,
        }),
    }
}

/// A file-based journal, reported as far as it can be proven without its kind's own rules.
///
/// The one thing that needs no rules is its lock: while a game's store lock is held, a run is using
/// those files, and that is busy whatever the journal says. No command is printed: the explicit
/// restore commands for these kinds need a registered game, and this report does not get to promise
/// a command it has not been able to run.
fn file_entry(kind: JournalKind, path: &Path, locks: &LockManager) -> JournalEntry {
    let mut entry = JournalEntry {
        kind,
        resource: path
            .parent()
            .and_then(|dir| dir.file_name())
            .or_else(|| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        path: path.display().to_string(),
        state: None,
        outcome: RecoveryOutcome::Undecided,
        reason: format!(
            "This build does not report a decision for {} journals yet; the next start's recovery \
             decides it with that kind's own rules.",
            kind.as_str()
        ),
        command: None,
    };
    let document = match read_journal_file(path) {
        Ok(document) => document,
        Err(reason) => {
            entry.outcome = RecoveryOutcome::NeedsHand;
            entry.reason = reason;
            return entry;
        }
    };
    // Whatever the kind records about how far it got, said in its own words.
    entry.state = document
        .get("state")
        .or_else(|| document.get("version"))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_u64().map(|version| format!("version {version}")))
        });
    match lock_of(&document) {
        Ok(Some(resource)) => match locks.probe(&resource) {
            LockProbe::Held(metadata) => {
                entry.outcome = RecoveryOutcome::Busy;
                entry.reason = match metadata {
                    Some(metadata) => format!(
                        "Another Agora holds this session's lock right now ({} on {}, operation: \
                         {}), so a run is using these files. Nothing would be done; the next start \
                         retries.",
                        metadata.program, metadata.hostname, metadata.operation
                    ),
                    None => "Another Agora holds this session's lock right now, so a run is using \
                             these files. Nothing would be done; the next start retries."
                        .into(),
                };
                return entry;
            }
            LockProbe::CantTell(why) => {
                entry.outcome = RecoveryOutcome::Busy;
                entry.reason = format!(
                    "Agora could not tell whether a run holds this session's lock ({why}), so it \
                     may be running. Nothing was changed."
                );
                return entry;
            }
            LockProbe::Free => {}
        },
        Ok(None) => {}
        Err(reason) => {
            entry.outcome = RecoveryOutcome::NeedsHand;
            entry.reason = reason;
            return entry;
        }
    }
    entry.command = None;
    entry
}

/// The lock a tool-swap or user-files journal's writer holds, from the game and store it records.
fn lock_of(document: &serde_json::Value) -> Result<Option<LockResource>> {
    let game = document.get("game").and_then(serde_json::Value::as_str);
    let store = document.get("store").and_then(serde_json::Value::as_str);
    let (Some(game), Some(store)) = (game, store) else {
        return Ok(None);
    };
    let game_id = agora_game_api::GameId::new(game)
        .map_err(|error| format!("It records a game that is not a game id: {error}."))?;
    let store_id = agora_game_api::StoreId::new(store)
        .map_err(|error| format!("It records a store that is not a store id: {error}."))?;
    Ok(Some(LockResource::GameUserFiles(game_id, store_id)))
}

/// Read one journal file, or say why it cannot be read.
///
/// A path that is a directory, a link, or anything else that is not a readable file is an answer of
/// "unreadable", never a reason to skip the entry.
fn read_journal_file(path: &Path) -> std::result::Result<serde_json::Value, String> {
    match probe(path) {
        Probe::Absent => Err(format!(
            "{} is not there any more, so what it held is unknown.",
            path.display()
        )),
        Probe::CantTell(error) => Err(format!(
            "{} could not be examined ({error}), so it was not read.",
            path.display()
        )),
        Probe::Present(kind) if kind != PresentKind::File && kind != PresentKind::Symlink => {
            Err(format!(
                "{} is a {}, not a journal file, so nothing about it can be acted on.",
                path.display(),
                match kind {
                    PresentKind::Directory => "folder",
                    _ => "special file",
                }
            ))
        }
        Probe::Present(_) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("{} could not be read: {error}", path.display()))?;
            serde_json::from_str(&text).map_err(|error| {
                format!("The journal is not JSON, so nothing about it can be acted on: {error}")
            })
        }
    }
}

/// The entries in `dir`: files with `suffix` when one is given, the folder's own children
/// otherwise. A folder that is not there is an empty list; one that cannot be listed is an error,
/// never "nothing found".
fn read_children(dir: PathBuf, suffix: Option<&str>) -> std::result::Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "{} could not be listed ({error}), so its journals are unknown — which is not the \
                 same as there being none.",
                dir.display()
            ))
        }
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Err(format!(
                    "An entry in {} could not be read ({error}), so its journals are unknown.",
                    dir.display()
                ))
            }
        };
        let path = entry.path();
        // By name, not by kind: a journal path that is a directory or a link is an entry the report
        // must list as unreadable, not one it may drop.
        if let Some(suffix) = suffix {
            if !path.extension().is_some_and(|found| found == suffix) {
                continue;
            }
        }
        found.push(path);
    }
    found.sort();
    Ok(found)
}

// ---------------------------------------------------------------------------
// Recently deleted
// ---------------------------------------------------------------------------

/// A deleted instance's `entry.json`, as far as the report needs it.
///
/// The entry is written with camelCase field names; the row and the version around it are not
/// read here, so a change to them cannot make this unreadable to the report.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeletedEntryFile {
    #[serde(default)]
    instance_id: String,
    #[serde(default)]
    name: String,
}

/// The `restoring.json` a restore writes before it moves anything. Only the id it was restoring is
/// a fact this report uses: it does not predict what the next start will make of the marker.
#[derive(Debug, Deserialize)]
struct RestoringFile {
    #[serde(default)]
    new_id: String,
}

/// Entries in Recently deleted, reported as facts and **no prediction**.
///
/// This kind is not on the journal envelope yet either (MASTER_SPEC §32.11 D1), so — exactly like a
/// launcher import — the report says what it can see and nothing more: the entry's id, the instance
/// it covers, whether `entry.json` and `restoring.json` are there and readable (with the read error
/// when one is not), and its instance's lock. A restore marker is `undecided`: the next start runs
/// that kind's own recovery, which decides from the folders and the markers inside them.
fn recently_deleted(paths: &AppPaths, report: &mut RecoveryReport) {
    let locks = LockManager::new(paths.locks_root());
    let root = paths.recently_deleted_games_root();
    let children = match read_children(root, None) {
        Ok(children) => children,
        Err(reason) => {
            report.not_searched.push(KindNotSearched {
                kind: JournalKind::RecentlyDeleted,
                reason,
                blocking: true,
            });
            return;
        }
    };
    for child in children {
        // Only a folder is an entry: the kind's own reader looks at `<root>/<id>/entry.json`.
        match probe(&child) {
            Probe::Present(PresentKind::Directory) => {}
            Probe::Present(_) => continue,
            Probe::CantTell(error) => {
                report.entries.push(JournalEntry {
                    kind: JournalKind::RecentlyDeleted,
                    resource: name_of(&child),
                    path: child.display().to_string(),
                    state: None,
                    outcome: RecoveryOutcome::NeedsHand,
                    reason: format!(
                        "{} could not be examined ({error}), so whether it holds a deleted instance \
                         is unknown. Nothing was changed.",
                        child.display()
                    ),
                    command: None,
                });
                continue;
            }
            Probe::Absent => continue,
        }
        report.entries.push(deleted_entry(&child, &locks));
    }
}

/// One Recently deleted entry, as the facts a person can act on.
fn deleted_entry(dir: &Path, locks: &LockManager) -> JournalEntry {
    let entry_file = dir.join("entry.json");
    let restoring_file = dir.join("restoring.json");
    let entry = read_json::<DeletedEntryFile>(&entry_file);
    let restoring = read_json::<RestoringFile>(&restoring_file);
    let instance = instance_of(&entry, &restoring);
    let facts = format!(
        "Entry {} covers {}{}: {}; {}.",
        name_of(dir),
        match &instance {
            Some(instance) => format!("instance {instance}"),
            // No file it can read names the instance, so there is nothing to ask its lock about.
            None => "no instance it can name".to_string(),
        },
        match &entry {
            Ok(Some(entry)) if !entry.name.is_empty() => format!(" (\"{}\")", entry.name),
            _ => String::new(),
        },
        described("entry.json", &entry),
        described("restoring.json", &restoring)
    );
    let mut listed = JournalEntry {
        kind: JournalKind::RecentlyDeleted,
        resource: name_of(dir),
        path: match probe(&entry_file) {
            Probe::Absent => dir.display().to_string(),
            _ => entry_file.display().to_string(),
        },
        state: Some(
            match (&restoring, &entry) {
                (Ok(Some(_)), _) => "restoring",
                (Err(_), _) => "restoring-unreadable",
                (Ok(None), Ok(Some(_))) => "kept",
                (Ok(None), Err(_)) => "unreadable-record",
                (Ok(None), Ok(None)) => "empty",
            }
            .to_string(),
        ),
        outcome: RecoveryOutcome::NothingToDo,
        reason: String::new(),
        command: None,
    };
    // The instance's lock first: while a run holds it, nothing about this entry is decided. With no
    // instance it can name there is no lock to ask, and the facts already say what is known.
    let probed = instance
        .as_ref()
        .map(|instance| locks.probe(&LockResource::Instance(instance.clone())));
    match probed {
        Some(LockProbe::Held(metadata)) => {
            listed.outcome = RecoveryOutcome::Busy;
            listed.reason = format!(
                "{facts} Another Agora holds this instance's lock right now{}, so a run may still \
                 be using it. Nothing would be done; the next start retries.",
                match metadata {
                    Some(metadata) => format!(
                        " ({} on {}, operation: {})",
                        metadata.program, metadata.hostname, metadata.operation
                    ),
                    None => String::new(),
                }
            );
            return listed;
        }
        Some(LockProbe::CantTell(why)) => {
            listed.outcome = RecoveryOutcome::Busy;
            listed.reason = format!(
                "{facts} Agora could not tell whether this instance's lock is held ({why}), so a \
                 run may still be using it. Nothing was changed."
            );
            return listed;
        }
        Some(LockProbe::Free) | None => {}
    }
    match (restoring, entry) {
        // A restore was started: this build does not decide what a start will make of it.
        (Ok(Some(_)), _) | (Err(_), _) => {
            listed.outcome = RecoveryOutcome::Undecided;
            listed.reason = format!(
                "{facts} A restore of this entry was started; the next start settles it with that \
                 kind's own rules, and nothing has been changed."
            );
        }
        (Ok(None), Ok(Some(_))) => {
            listed.outcome = RecoveryOutcome::NothingToDo;
            listed.reason = format!(
                "{facts} The entry is held until it expires; nothing is pending. Bring it back \
                 with `agora games instance restore {}`.",
                name_of(dir)
            );
            listed.command = Some(format!("agora games instance restore {}", name_of(dir)));
        }
        (Ok(None), Err(error)) => {
            listed.outcome = RecoveryOutcome::NeedsHand;
            listed.reason = format!(
                "{facts} It cannot be brought back while its record is unreadable ({error}); no \
                 restore was started, so nothing is pending, and the files are left as they are."
            );
        }
        (Ok(None), Ok(None)) => {
            listed.outcome = RecoveryOutcome::NothingToDo;
            listed.reason = format!(
                "{facts} There is no deleted instance to bring back: the folder holds neither \
                 file, so nothing is pending."
            );
        }
    }
    listed
}

/// The instance an entry covers: its own record's instance, then the restore's target id.
fn instance_of(
    entry: &std::result::Result<Option<DeletedEntryFile>, String>,
    restoring: &std::result::Result<Option<RestoringFile>, String>,
) -> Option<String> {
    match (entry, restoring) {
        (Ok(Some(entry)), _) if !entry.instance_id.is_empty() => Some(entry.instance_id.clone()),
        (_, Ok(Some(restoring))) if !restoring.new_id.is_empty() => Some(restoring.new_id.clone()),
        _ => None,
    }
}

/// One of the entry's two files, said in one clause: there and readable, not there, or the error.
fn described<T>(what: &str, read: &std::result::Result<Option<T>, String>) -> String {
    match read {
        Ok(Some(_)) => format!("{what} is there and readable"),
        Ok(None) => format!("{what} is not there"),
        Err(error) => format!("{what} could not be read ({error})"),
    }
}
/// A folder's own name, for the resource a marker covers when its record is unreadable.
fn name_of(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

/// Read a JSON file: `None` when it is not there, an error when it cannot be read or parsed.
fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> std::result::Result<Option<T>, String> {
    match probe(path) {
        Probe::Absent => Ok(None),
        Probe::CantTell(error) => Err(format!(
            "{} could not be examined ({error})",
            path.display()
        )),
        Probe::Present(kind) if kind != PresentKind::File && kind != PresentKind::Symlink => {
            Err(format!("{} is not a file", path.display()))
        }
        Probe::Present(_) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("{} could not be read ({error})", path.display()))?;
            serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| format!("{} is not readable JSON ({error})", path.display()))
        }
    }
}
