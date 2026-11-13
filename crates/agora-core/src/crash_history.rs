//! Finding the older crash logs a pack's sources point at, analysing each once, and reading the
//! pattern across crashes (MASTER_SPEC §30.4, §30.5, §30.11).
//!
//! This is the file-facing half of [`crate::crash_engine::history`]: the engine counts crashes,
//! this module finds them and remembers what they said. It holds no game knowledge. A pack's
//! `sources` say where evidence appears; the sweep reads the files those sources name, newest
//! first, and stores only a compact per-log record keyed by the file's content hash, so the same
//! log is never analysed twice. The bytes themselves are never copied here.
//!
//! Every failure is stated. A missing folder, a file too large to read, a log no parser recognised
//! and a sweep stopped by its bound are all reported; none of them is a clean result.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::SystemTime;

use agora_game_api::crash::{ArtifactUnit, BuiltInPack, CopyPolicy, CouldNotCheck};
use agora_game_api::{GamePath, RelPath};
use agora_plugin_api::host::ScriptHost;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crash_engine::evidence::{
    folder_below_trusted_root, glob_matches, is_safe_artifact_name, marker_present, unix_ms,
    SourceLocation, SourceRoots,
};
use crate::crash_engine::history::{self, CrashHistory, CrashRecord, RecordedCrash};
use crate::crash_engine::{
    analyze, AnalyzeInput, CrashFile, CrashHistoryInput, Inspector, InstanceAnalysis, RefResolver,
};
use crate::crash_live::StoredAnswer;
use crate::crash_packs::LoadedCrashPack;
use crate::ctx::Ctx;

/// The table that keeps one compact record per distinct log content.
pub const LOG_RESULTS_TABLE: &str = "crash_log_results";

/// How much one sweep looks at and keeps. The per-log record is small; the logs themselves are
/// read and dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSweepLimits {
    /// Logs analysed or re-used in one sweep, newest first. Older ones are reported as skipped.
    pub max_logs: usize,
    /// A larger file is not read: a crash log bigger than this is not worth an unbounded read.
    pub max_file_bytes: u64,
    /// Distinct log records kept per instance. The oldest are dropped first, like the evidence
    /// store's own retention.
    pub max_stored: usize,
}

impl Default for LogSweepLimits {
    fn default() -> Self {
        Self {
            max_logs: 50,
            max_file_bytes: 16 * 1024 * 1024,
            max_stored: 200,
        }
    }
}

/// One source folder the sweep could not look at, or one file it could not read. Named, never
/// silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepProblem {
    pub plugin_id: String,
    pub source_id: String,
    pub detail: String,
}

/// A file the sweep left alone, with why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedLog {
    pub source_id: String,
    pub name: String,
    pub reason: String,
}

/// What one sweep did. `skipped_older` is the count left for a later sweep because the per-sweep
/// bound stopped the work; it is not "nothing was there".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSweepReport {
    pub instance_id: String,
    /// Source declarations that apply to the instance's game.
    pub sources: usize,
    /// Files those sources pointed at.
    pub found: usize,
    /// Files analysed and stored for the first time.
    pub analyzed: usize,
    /// Files whose content hash was already stored.
    pub reused: usize,
    /// Newest-first work left by the per-sweep bound.
    pub skipped_older: usize,
    pub skipped: Vec<SkippedLog>,
    pub problems: Vec<SweepProblem>,
}

/// The roots a source's `where` resolves against. A root Agora cannot resolve is reported, never
/// guessed.
pub struct InstanceRoots {
    pub instance_dir: PathBuf,
}

impl SourceRoots for InstanceRoots {
    fn resolve(&self, location: &GamePath) -> Result<SourceLocation, String> {
        match location {
            GamePath::Instance { path } => Ok(SourceLocation {
                base: self.instance_dir.clone(),
                relative: path.as_str().to_string(),
            }),
            GamePath::UserData { location, path } => {
                let base = crate::game_user_files::user_data_root(location)
                    .ok_or_else(|| format!("the user data folder for {location:?} is not known"))?;
                Ok(SourceLocation {
                    base,
                    relative: path.as_str().to_string(),
                })
            }
            other => Err(format!("this source root cannot be resolved: {other:?}")),
        }
    }
}

/// Which instance an answer is about, and the pieces the engine needs to analyse its old logs.
pub struct HistorySetup<'a> {
    pub instance_id: &'a str,
    pub game_id: &'a str,
    pub runtime_version: Option<&'a str>,
    pub packs: &'a [LoadedCrashPack],
    pub built_in: &'a [BuiltInPack],
    pub host: &'a dyn ScriptHost,
    pub resolver: &'a dyn RefResolver,
    pub inspector: &'a dyn Inspector,
    pub could_not_check: &'a [CouldNotCheck],
    /// The setup this analysis runs, as (manifest hash, session start). Absent leaves
    /// `predates_last_change` unstated rather than guessed.
    pub setup: Option<(&'a str, &'a str)>,
    /// Content hashes the caller is analysing right now, so the same crash is not counted twice.
    pub exclude_hashes: &'a [String],
}

/// What the history stage loaded, together with the sweep that produced it.
pub struct PreparedHistory {
    pub prior: Vec<RecordedCrash>,
    pub sessions_since_first: BTreeMap<String, u64>,
    pub setup_since_unix_ms: Option<i64>,
    pub sweep: Option<LogSweepReport>,
}

impl PreparedHistory {
    /// The input the evaluator takes. It borrows this value and the confirmations, so keep both
    /// alive for the analysis.
    pub fn input<'a>(
        &'a self,
        local_confirmations: &'a [crate::crash_engine::history::LocalConfirmation],
    ) -> CrashHistoryInput<'a> {
        CrashHistoryInput {
            prior: &self.prior,
            sessions_since_first: &self.sessions_since_first,
            setup_since_unix_ms: self.setup_since_unix_ms,
            local_confirmations,
        }
    }

    pub fn summary(&self) -> CrashHistory {
        history::summarize_recorded(&self.prior)
    }

    /// What to tell the user about logs the sweep did not reach, if any.
    pub fn skipped_note(&self, limits: LogSweepLimits) -> Option<String> {
        let sweep = self.sweep.as_ref()?;
        if sweep.skipped_older == 0 {
            return None;
        }
        Some(format!(
            "{} older crash log(s) were left for a later sweep because one sweep reads at most {}.",
            sweep.skipped_older, limits.max_logs
        ))
    }
}

/// Sweep a source's cache for an instance, newest first, then load the pattern across its stored
/// crashes. `sweep` false only reads what is already stored.
pub fn prepare(
    ctx: &Ctx,
    conn: &Connection,
    setup: HistorySetup<'_>,
    limits: LogSweepLimits,
    sweep: bool,
) -> Result<PreparedHistory, String> {
    let sweep_report = if sweep {
        Some(sweep_instance_logs(ctx, conn, &setup, limits)?)
    } else {
        None
    };
    let prior = load_recorded(conn, setup.instance_id, setup.exclude_hashes)
        .map_err(|error| format!("the stored crash history could not be read: {error}"))?;
    let sessions_since_first = sessions_since_first(conn, setup.instance_id, &prior)?;
    let setup_since_unix_ms = match setup.setup {
        Some((hash, started_at)) => setup_since(conn, setup.instance_id, hash, started_at)?,
        None => None,
    };
    Ok(PreparedHistory {
        prior,
        sessions_since_first,
        setup_since_unix_ms,
        sweep: sweep_report,
    })
}

/// How many distinct logs are stored for an instance.
pub fn stored_log_count(conn: &Connection, instance_id: &str) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM crash_log_results WHERE instance_id = ?1",
        [instance_id],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

/// List every file the applicable packs' `sources` point at, analyse the newest ones once, and
/// store a compact record for each. The logs themselves are read and dropped.
pub fn sweep_instance_logs(
    ctx: &Ctx,
    conn: &Connection,
    setup: &HistorySetup<'_>,
    limits: LogSweepLimits,
) -> Result<LogSweepReport, String> {
    let instance_dir = ctx
        .paths
        .instance_dir(setup.instance_id)
        .map_err(|error| format!("the instance folder could not be resolved: {error}"))?;
    let roots = InstanceRoots { instance_dir };

    let game_packs: Vec<&LoadedCrashPack> = setup
        .packs
        .iter()
        .filter(|pack| {
            pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == setup.game_id)
        })
        .collect();
    let sources: Vec<_> = game_packs
        .iter()
        .flat_map(|pack| {
            pack.pack
                .sources
                .iter()
                .map(move |source| (pack.plugin_id.as_str(), source.id.as_str(), source))
        })
        .collect();

    let mut report = LogSweepReport {
        instance_id: setup.instance_id.to_string(),
        sources: sources.len(),
        ..LogSweepReport::default()
    };
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();

    for (plugin_id, source_id, source) in &sources {
        if source.unit != ArtifactUnit::File {
            report.problems.push(SweepProblem {
                plugin_id: (*plugin_id).to_string(),
                source_id: (*source_id).to_string(),
                detail: "a folder artifact is not swept for older logs; only files are".to_string(),
            });
            continue;
        }
        if source.copy == CopyPolicy::Never {
            report.problems.push(SweepProblem {
                plugin_id: (*plugin_id).to_string(),
                source_id: (*source_id).to_string(),
                detail:
                    "this source says its files are never read, so no older log was taken from it"
                        .to_string(),
            });
            continue;
        }
        let folder = match source_folder(&roots, source) {
            Ok(folder) => folder,
            Err(detail) => {
                report.problems.push(SweepProblem {
                    plugin_id: (*plugin_id).to_string(),
                    source_id: (*source_id).to_string(),
                    detail,
                });
                continue;
            }
        };
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) => {
                report.problems.push(SweepProblem {
                    plugin_id: (*plugin_id).to_string(),
                    source_id: (*source_id).to_string(),
                    detail: format!("`{}`: {error}", folder.display()),
                });
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.problems.push(SweepProblem {
                        plugin_id: (*plugin_id).to_string(),
                        source_id: (*source_id).to_string(),
                        detail: format!("`{}`: {error}", folder.display()),
                    });
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !glob_matches(&source.matching.glob, &name) || !is_safe_artifact_name(&name) {
                continue;
            }
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    report.skipped.push(SkippedLog {
                        source_id: (*source_id).to_string(),
                        name,
                        reason: format!("its metadata could not be read: {error}"),
                    });
                    continue;
                }
            };
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                report.skipped.push(SkippedLog {
                    source_id: (*source_id).to_string(),
                    name,
                    reason: "a junction or symlink is never followed".to_string(),
                });
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if !marker_present(&path, &source.matching, false) {
                report.skipped.push(SkippedLog {
                    source_id: (*source_id).to_string(),
                    name,
                    reason: "the source says a marker file must show it is finished".to_string(),
                });
                continue;
            }
            let key = path.to_string_lossy().to_lowercase();
            if !seen_paths.insert(key) {
                continue;
            }
            candidates.push(Candidate {
                plugin_id: (*plugin_id).to_string(),
                source_id: (*source_id).to_string(),
                name,
                path,
                modified: metadata.modified().ok(),
                size: metadata.len(),
            });
        }
    }

    // Newest first. A file with no modification time goes after the dated ones rather than
    // pretending to be new.
    candidates.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    report.found = candidates.len();
    report.skipped_older = candidates.len().saturating_sub(limits.max_logs);
    candidates.truncate(limits.max_logs);

    let instance = InstanceAnalysis {
        game_id: setup.game_id,
        resolver: setup.resolver,
        inspector: setup.inspector,
        could_not_check: setup.could_not_check,
    };
    let analyzed_at = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
    for candidate in &candidates {
        if candidate.size > limits.max_file_bytes {
            report.skipped.push(SkippedLog {
                source_id: candidate.source_id.clone(),
                name: candidate.name.clone(),
                reason: format!(
                    "it is {} bytes, over the {} byte limit for one log",
                    candidate.size, limits.max_file_bytes
                ),
            });
            continue;
        }
        let bytes = match std::fs::read(&candidate.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.skipped.push(SkippedLog {
                    source_id: candidate.source_id.clone(),
                    name: candidate.name.clone(),
                    reason: format!("it could not be read: {error}"),
                });
                continue;
            }
        };
        let hash = content_hash(&bytes);
        if log_result_exists(conn, setup.instance_id, &hash)
            .map_err(|error| format!("the stored log result could not be checked: {error}"))?
        {
            let _ = touch_log_result(conn, setup.instance_id, &hash, candidate, &analyzed_at);
            report.reused += 1;
            continue;
        }
        let output = match analyze(&AnalyzeInput {
            packs: setup.packs,
            files: &[CrashFile {
                name: candidate.name.clone(),
                bytes,
                // The sweep found it: a whole file, never read by a snippet parser.
                origin: crate::crash_engine::CrashFileOrigin::Collected,
            }],
            runtime_version: setup.runtime_version,
            host: setup.host,
            built_in: setup.built_in,
            instance: Some(instance),
        }) {
            Ok(output) => output,
            Err(error) => {
                report.problems.push(SweepProblem {
                    plugin_id: candidate.plugin_id.clone(),
                    source_id: candidate.source_id.clone(),
                    detail: format!("`{}` could not be analysed: {error}", candidate.name),
                });
                continue;
            }
        };
        let recorded = RecordedCrash {
            record: output.record,
            bucket: history::parsed_bucket(&output.parsed),
            at_unix_ms: candidate.modified.map(unix_ms),
        };
        store_log_result(
            conn,
            setup.instance_id,
            &hash,
            candidate,
            &analyzed_at,
            &recorded,
        )
        .map_err(|error| format!("the log result could not be stored: {error}"))?;
        report.analyzed += 1;
    }

    prune_log_results(conn, setup.instance_id, limits.max_stored)
        .map_err(|error| format!("the stored log results could not be trimmed: {error}"))?;
    report
        .skipped
        .sort_by(|left, right| (&left.source_id, &left.name).cmp(&(&right.source_id, &right.name)));
    report.problems.sort_by(|left, right| {
        (&left.source_id, &left.detail).cmp(&(&right.source_id, &right.detail))
    });
    Ok(report)
}

struct Candidate {
    plugin_id: String,
    source_id: String,
    name: String,
    path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
}

fn source_folder(
    roots: &InstanceRoots,
    source: &agora_game_api::crash::CrashSource,
) -> Result<PathBuf, String> {
    let location = roots.resolve(&source.location)?;
    let relative = RelPath::new(location.relative.clone()).map_err(|error| error.to_string())?;
    folder_below_trusted_root(&location.base, relative.as_str())
        .map_err(|(_, detail): (crate::crash_engine::evidence::SourceProblemKind, String)| detail)
}

/// The SHA-256 of one log's bytes, in lower-case hex. The key a log is remembered by.
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn log_result_exists(conn: &Connection, instance_id: &str, hash: &str) -> anyhow::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM crash_log_results WHERE instance_id = ?1 AND content_hash = ?2",
            params![instance_id, hash],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn touch_log_result(
    conn: &Connection,
    instance_id: &str,
    hash: &str,
    candidate: &Candidate,
    analyzed_at: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE crash_log_results
            SET source_id = ?3, plugin_id = ?4, name = ?5, relative_path = ?6,
                modified_at = ?7, analyzed_at = ?8
          WHERE instance_id = ?1 AND content_hash = ?2",
        params![
            instance_id,
            hash,
            candidate.source_id,
            candidate.plugin_id,
            candidate.name,
            candidate.path.to_string_lossy(),
            candidate.modified.map(modified_rfc3339),
            analyzed_at,
        ],
    )?;
    Ok(())
}

fn store_log_result(
    conn: &Connection,
    instance_id: &str,
    hash: &str,
    candidate: &Candidate,
    analyzed_at: &str,
    recorded: &RecordedCrash,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO crash_log_results
             (instance_id, content_hash, source_id, plugin_id, name, relative_path,
              modified_at, analyzed_at, record_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            instance_id,
            hash,
            candidate.source_id,
            candidate.plugin_id,
            candidate.name,
            candidate.path.to_string_lossy(),
            candidate.modified.map(modified_rfc3339),
            analyzed_at,
            serde_json::to_string(recorded)?,
        ],
    )?;
    Ok(())
}

fn prune_log_results(
    conn: &Connection,
    instance_id: &str,
    max_stored: usize,
) -> anyhow::Result<()> {
    if max_stored == 0 {
        conn.execute(
            "DELETE FROM crash_log_results WHERE instance_id = ?1",
            [instance_id],
        )?;
        return Ok(());
    }
    conn.execute(
        "DELETE FROM crash_log_results
          WHERE instance_id = ?1
            AND content_hash NOT IN (
                SELECT content_hash FROM crash_log_results
                 WHERE instance_id = ?1
                 ORDER BY COALESCE(modified_at, analyzed_at) DESC,
                          analyzed_at DESC, content_hash
                 LIMIT ?2
            )",
        params![instance_id, max_stored as i64],
    )?;
    Ok(())
}

/// The stored crashes for an instance: older logs first, then the answers sessions stored. A
/// session whose answer says nothing was readable counts as an unreadable crash, never absent.
pub fn load_recorded(
    conn: &Connection,
    instance_id: &str,
    exclude_hashes: &[String],
) -> anyhow::Result<Vec<RecordedCrash>> {
    let mut crashes: Vec<RecordedCrash> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT content_hash, record_json FROM crash_log_results
              WHERE instance_id = ?1
              ORDER BY COALESCE(modified_at, analyzed_at) ASC, analyzed_at ASC, content_hash",
        )?;
        let rows = stmt.query_map([instance_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (hash, json) = row?;
            if exclude_hashes.iter().any(|excluded| excluded == &hash) {
                continue;
            }
            if let Ok(recorded) = serde_json::from_str::<RecordedCrash>(&json) {
                crashes.push(recorded);
            }
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT sa.session_id, sa.answer_json, sa.created_at, lh.started_at
               FROM session_answers sa
               LEFT JOIN launch_history lh ON lh.session_id = sa.session_id
              WHERE sa.instance_id = ?1
              ORDER BY sa.created_at ASC, sa.session_id ASC",
        )?;
        let rows = stmt.query_map([instance_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (session_id, json, created_at, started_at) = row?;
            let at = started_at
                .as_deref()
                .or(Some(created_at.as_str()))
                .and_then(parse_unix_ms);
            let Ok(stored) = serde_json::from_str::<StoredAnswer>(&json) else {
                crashes.push(RecordedCrash {
                    record: unreadable_record(format!("session {session_id}")),
                    bucket: Vec::new(),
                    at_unix_ms: at,
                });
                continue;
            };
            let recorded = match stored {
                StoredAnswer::Analyzed { answer, record, .. } => {
                    if let Some(mut record) = record {
                        if record.at_unix_ms.is_none() {
                            record.at_unix_ms = at;
                        }
                        record
                    } else {
                        RecordedCrash {
                            record: CrashRecord {
                                artifact: format!("session {session_id}"),
                                readable: true,
                                candidates: answer
                                    .candidates
                                    .iter()
                                    .map(|candidate| history::HistorySuspect {
                                        owner: candidate.owner.clone(),
                                        role: None,
                                    })
                                    .collect(),
                                place: None,
                            },
                            bucket: Vec::new(),
                            at_unix_ms: at,
                        }
                    }
                }
                StoredAnswer::NothingToRead { .. } | StoredAnswer::CouldNotAnalyze { .. } => {
                    RecordedCrash {
                        record: unreadable_record(format!("session {session_id}")),
                        bucket: Vec::new(),
                        at_unix_ms: at,
                    }
                }
            };
            crashes.push(recorded);
        }
    }
    crashes.sort_by_key(|crash| crash.at_unix_ms);
    Ok(crashes)
}

fn unreadable_record(artifact: String) -> CrashRecord {
    CrashRecord {
        artifact,
        readable: false,
        candidates: Vec::new(),
        place: None,
    }
}

/// For each bucket already stored, how many sessions have run since it first appeared.
pub fn sessions_since_first(
    conn: &Connection,
    instance_id: &str,
    prior: &[RecordedCrash],
) -> Result<BTreeMap<String, u64>, String> {
    let mut first_seen: BTreeMap<String, i64> = BTreeMap::new();
    for crash in prior {
        let (Some(bucket), Some(at)) = (crash.bucket_key(), crash.at_unix_ms) else {
            continue;
        };
        let entry = first_seen.entry(bucket).or_insert(at);
        if at < *entry {
            *entry = at;
        }
    }
    if first_seen.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT started_at FROM launch_history WHERE instance_id = ?1")
        .map_err(|error| error.to_string())?;
    let starts: Vec<i64> = stmt
        .query_map([instance_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .filter_map(|row| row.ok())
        .filter_map(|text| parse_unix_ms(&text))
        .collect();
    Ok(first_seen
        .into_iter()
        .map(|(bucket, first)| {
            let count = starts.iter().filter(|started| **started >= first).count();
            (bucket, count as u64)
        })
        .collect())
}

/// When the setup named by `manifest_hash` first ran, in Unix milliseconds. Used to say whether a
/// crash bucket predates the last change.
pub fn setup_since(
    conn: &Connection,
    instance_id: &str,
    manifest_hash: &str,
    before_started_at: &str,
) -> Result<Option<i64>, String> {
    let started: Option<String> = conn
        .query_row(
            "SELECT MIN(started_at) FROM launch_history
              WHERE instance_id = ?1 AND manifest_hash = ?2 AND started_at <= ?3",
            params![instance_id, manifest_hash, before_started_at],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(started.as_deref().and_then(parse_unix_ms))
}

fn parse_unix_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn modified_rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}
