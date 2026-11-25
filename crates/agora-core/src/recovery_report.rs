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
//! lease, migrates the database or switches its journal mode. The database is opened with
//! `SQLITE_OPEN_READ_ONLY` for the journals it holds. The proof that this holds is a test that
//! snapshots the whole data root — file list, sizes and bytes — around the report.
//!
//! # Fail closed
//!
//! A journal that cannot be read is listed with its path and its error, never skipped. A kind whose
//! journals cannot be enumerated at all (a database this build does not know, a table that cannot be
//! read, a folder that cannot be listed) is reported in
//! [`RecoveryReport::not_searched`] with the reason — never as "no journals". A missing database is
//! named as such: it is not the same as having no journals.
//!
//! # Which journals, and why not all of them are decided
//!
//! * **plugin installs** and **launcher imports** live in the local database;
//! * **tool swaps** and **user-files sessions** are files under the data root.
//!
//! The plugin install decides through the one recovery decision
//! ([`crate::recovery::decide_readonly`]), so its answer is the answer the next start would give.
//! The other kinds still keep their own recovery code (§32.11 D1 moves them onto the envelope one
//! at a time); this report therefore says [`RecoveryOutcome::Undecided`] for them rather than
//! re-implementing their rules a second time, and reports only what it can prove without them: that
//! a journal's lock is held (busy), and that a journal could not be read at all.

use std::path::{Path, PathBuf};

use serde::Serialize;

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
}

impl JournalKind {
    /// The stable name a report and its JSON share.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalKind::PluginInstall => "plugin-install",
            JournalKind::InstanceImport => "instance-import",
            JournalKind::ToolSwap => "tool-swap",
            JournalKind::UserFiles => "user-files",
        }
    }
}

/// One journal found, and what recovery would do with it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub kind: JournalKind,
    /// What the journal covers: the plugin id, the instance id, or the game and store.
    pub resource: String,
    /// Where the journal is: its file, or the local database that holds its row.
    pub path: String,
    /// This kind's own state, when the journal was readable.
    pub state: Option<String>,
    /// What recovery would do.
    pub outcome: RecoveryOutcome,
    /// Why, in a sentence — including the parse error of a journal that could not be read.
    pub reason: Option<String>,
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
    pub fn needs_a_hand(&self) -> bool {
        !self.not_searched.is_empty()
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
    if let Err(reason) = read_database_journals(paths, view, &mut report) {
        // Both database kinds are reached through the same file, so a database that cannot be
        // read leaves both of them unknown. Saying so beats saying nothing.
        for kind in [JournalKind::PluginInstall, JournalKind::InstanceImport] {
            report.not_searched.push(KindNotSearched {
                kind,
                reason: reason.clone(),
            });
        }
    }
    file_journals(paths, &mut report);
    report
}

/// Read the database-held journals, read-only. Returns the reason the database could not be asked,
/// and never stops the file-based kinds from being listed.
fn read_database_journals(
    paths: &AppPaths,
    view: &dyn ProcessView,
    report: &mut RecoveryReport,
) -> Result<()> {
    let db = paths.local_state_db();
    if !db.is_file() {
        report
            .notes
            .push("No local database yet, so there are no plugin-install or import journals. The next start creates it.".into());
        return Ok(());
    }
    let conn = crate::db::local_state_connection_read_only(&db).map_err(|error| {
        format!(
            "{} could not be opened read-only ({error}), so the plugin-install and import journals \
             are unknown — which is not the same as there being none.",
            db.display()
        )
    })?;
    // A schema this build does not know may give these tables meanings it does not share. Asking
    // anyway would read rows as this version, which is exactly what the next start refuses to do.
    let version = crate::db::recorded_schema_version(&conn)
        .map_err(|error| format!("The database's schema version could not be read ({error})."))?;
    if version > crate::db::LOCAL_STATE_SCHEMA_VERSION {
        return Err(format!(
            "The database was written by a newer Agora (schema {version}; this build supports up \
             to {}), so its journals are not read as this version's.",
            crate::db::LOCAL_STATE_SCHEMA_VERSION
        ));
    }
    let mut first_error: Option<String> = None;
    match crate::plugins::install::journal_report(&conn, paths, view) {
        Ok(journals) => {
            for journal in journals {
                report.entries.push(JournalEntry {
                    kind: JournalKind::PluginInstall,
                    resource: journal.plugin_id.to_string(),
                    path: journal.path.display().to_string(),
                    state: journal.state.map(|state| state.as_str().to_string()),
                    outcome: journal.outcome,
                    command: (journal.outcome != RecoveryOutcome::NothingToDo)
                        .then(|| format!("agora plugin recover {} --yes", journal.plugin_id)),
                    reason: journal.reason,
                });
            }
        }
        Err(error) => {
            first_error = Some(format!(
                "The plugin-install journals could not be read ({error})."
            ));
        }
    }
    match import_journals(&conn, paths) {
        Ok(entries) => report.entries.extend(entries),
        Err(reason) => first_error = first_error.or(Some(reason)),
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The launcher-import journals, decided read-only.
///
/// This repeats the two questions the import recovery asks (`launcher_import_service.rs`,
/// `recover_one_job`) and nothing else: is the instance's lock held (a live import may still be
/// running), and did the import commit? Recovery commits by writing the import record and the
/// instance row together, so a job whose row is still there was never committed: its files are
/// rolled back, while a committed job only has its staging folder tidied. Keeping this in step with
/// that function is exactly the reason §32.11 D1 moves launcher import onto the envelope and the one
/// decision; until then, this is the second place that rule is written.
fn import_journals(
    conn: &rusqlite::Connection,
    paths: &AppPaths,
) -> std::result::Result<Vec<JournalEntry>, String> {
    let jobs = crate::db::list_instance_import_jobs(conn)
        .map_err(|error| format!("The import journals could not be read ({error})."))?;
    let locks = LockManager::new(paths.locks_root());
    let mut entries = Vec::new();
    for job in jobs {
        let mut entry = JournalEntry {
            kind: JournalKind::InstanceImport,
            resource: job.instance_id.clone(),
            path: paths.local_state_db().display().to_string(),
            state: Some(job.state.clone()),
            outcome: RecoveryOutcome::Undecided,
            reason: None,
            // Nothing to run: the next start's import recovery settles this on its own.
            command: None,
        };
        // The folder recovery proves ownership with: the staging folder, or the promoted instance
        // folder while an update is in flight. An unreadable marker is a hand, not a guess.
        let staging = PathBuf::from(&job.staging_path);
        if !staging.starts_with(paths.staging_root()) {
            entry.outcome = RecoveryOutcome::NeedsHand;
            entry.reason = Some(format!(
                "It names a staging folder outside the staging root ({}), which recovery refuses \
                 to touch.",
                staging.display()
            ));
            entries.push(entry);
            continue;
        }
        match read_import_marker(&staging) {
            Ok(None) => {
                entry.outcome = RecoveryOutcome::NeedsHand;
                entry.reason = Some(format!(
                    "Its staging folder {} is not marked as this job's, so recovery keeps it and \
                     reports it rather than guessing.",
                    staging.display()
                ));
                entry.path = staging.display().to_string();
            }
            Ok(Some(job_id)) if job_id != job.job_id => {
                entry.outcome = RecoveryOutcome::NeedsHand;
                entry.reason = Some(format!(
                    "Its staging folder {} is marked as another job's ({}), so recovery keeps it.",
                    staging.display(),
                    job_id
                ));
                entry.path = staging.join(IMPORT_MARKER).display().to_string();
            }
            Ok(Some(_)) => match locks.probe(&LockResource::Instance(job.instance_id.clone())) {
                LockProbe::Held(_) => {
                    entry.outcome = RecoveryOutcome::Busy;
                    entry.reason = Some(
                        "Another Agora holds the instance's lock right now, so the import may \
                         still be running. Nothing would be done."
                            .into(),
                    );
                }
                LockProbe::CantTell(why) => {
                    entry.outcome = RecoveryOutcome::NeedsHand;
                    entry.reason = Some(format!(
                        "Agora could not tell whether another operation holds the instance's lock \
                         ({why}), so it will not say what recovery would do."
                    ));
                }
                LockProbe::Free => {
                    let committed = import_committed(conn, &job)?;
                    if committed {
                        entry.outcome = RecoveryOutcome::WouldFinish;
                        entry.reason = Some(
                            "The instance and its import record are both in place, so recovery would \
                             only tidy the staging folder."
                                .into(),
                        );
                    } else {
                        entry.outcome = RecoveryOutcome::WouldUndo;
                        entry.reason = Some(format!(
                            "The import never committed (state \"{}\"), so recovery would roll its \
                             files back and keep anything it cannot prove is the job's.",
                            job.state
                        ));
                    }
                }
            },
            Err(reason) => {
                entry.outcome = RecoveryOutcome::NeedsHand;
                entry.reason = Some(format!(
                    "Its ownership marker {} could not be read: {reason}. Recovery keeps the folder \
                     and reports it rather than acting on a guess.",
                    staging.join(IMPORT_MARKER).display()
                ));
                entry.path = staging.join(IMPORT_MARKER).display().to_string();
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// The ownership marker the import writes inside its staging folder
/// (`launcher_import_service.rs`, `IMPORT_OWNER_MARKER`), read without changing it.
const IMPORT_MARKER: &str = ".agora-import-owner.json";

/// The job id the marker names: `None` when there is no marker, an error when it cannot be read.
fn read_import_marker(dir: &Path) -> Result<Option<String>> {
    let path = dir.join(IMPORT_MARKER);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("not JSON: {error}"))?;
    value
        .get("job_id")
        .and_then(serde_json::Value::as_str)
        .map(|id| Ok(Some(id.to_string())))
        .unwrap_or_else(|| Err("it names no job".into()))
}

/// Whether the import reached its commit point: the instance row and its import record, both
/// present, is what the import writes together at the end.
fn import_committed(
    conn: &rusqlite::Connection,
    job: &crate::db::InstanceImportJob,
) -> std::result::Result<bool, String> {
    let record = crate::db::get_instance_import(conn, &job.instance_id)
        .map_err(|error| format!("The import record could not be read ({error})."))?;
    let instance = crate::db::get_instance(conn, &job.instance_id)
        .map_err(|error| format!("The instance record could not be read ({error})."))?;
    Ok(
        record.is_some_and(|record| record.source_fingerprint == job.plan_fingerprint)
            && instance.is_some(),
    )
}

/// The journals that are files: tool swaps and user-files sessions.
fn file_journals(paths: &AppPaths, report: &mut RecoveryReport) {
    let swaps = match read_journals_in(paths.tool_swaps_root(), Some("json")) {
        Ok(paths) => paths,
        Err(reason) => {
            report.not_searched.push(KindNotSearched {
                kind: JournalKind::ToolSwap,
                reason,
            });
            Vec::new()
        }
    };
    for path in swaps {
        let name = file_stem(&path);
        report
            .entries
            .push(file_entry(JournalKind::ToolSwap, name, &path));
    }
    // `<data>/user-files/<game>_<store>/journal.json`, the same place `AppPaths` names.
    let root = paths.user_files_root();
    let dirs = match read_journals_in(root.clone(), None) {
        Ok(dirs) => dirs,
        Err(reason) => {
            report.not_searched.push(KindNotSearched {
                kind: JournalKind::UserFiles,
                reason,
            });
            Vec::new()
        }
    };
    for dir in dirs {
        let journal = dir.join("journal.json");
        if !journal.is_file() {
            continue;
        }
        report.entries.push(file_entry(
            JournalKind::UserFiles,
            file_stem(&dir),
            &journal,
        ));
    }
}

/// A file-based journal, reported as far as it can be proven without its kind's own rules.
fn file_entry(kind: JournalKind, resource: String, path: &Path) -> JournalEntry {
    let mut entry = JournalEntry {
        kind,
        resource,
        path: path.display().to_string(),
        state: None,
        outcome: RecoveryOutcome::Undecided,
        reason: Some(format!(
            "This build does not report a decision for {} journals yet; the next start's recovery \
             decides it with that kind's own rules.",
            kind.as_str()
        )),
        command: None,
    };
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(document) => {
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
            }
            Err(error) => {
                entry.outcome = RecoveryOutcome::NeedsHand;
                entry.state = None;
                entry.reason = Some(format!(
                    "The journal is not JSON, so nothing about it can be acted on: {error}"
                ));
            }
        },
        Err(error) => {
            entry.outcome = RecoveryOutcome::NeedsHand;
            entry.reason = Some(format!("The journal could not be read: {error}"));
        }
    }
    entry
}

/// The journals in `dir`: files with `suffix` when one is given, the folder's own subdirectories
/// otherwise. A folder that is not there is an empty list; one that cannot be listed is an error,
/// never "nothing found".
fn read_journals_in(
    dir: PathBuf,
    suffix: Option<&str>,
) -> std::result::Result<Vec<PathBuf>, String> {
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
        let matches = match suffix {
            Some(suffix) => path.extension().is_some_and(|found| found == suffix),
            None => path.is_dir(),
        };
        if matches {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// A journal's file name without its extension, for the resource it covers.
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
