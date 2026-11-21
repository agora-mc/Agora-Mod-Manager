//! Diagnosing a crash the moment a session ends (MASTER_SPEC §30.4, §30.5, §30.8).
//!
//! The launch ledger knows how a session ended and what it ran with. This module connects that to
//! the crash doctor: it collects the evidence the applicable packs name, copies it at once (a
//! logger may rotate it away), runs the one analyze pipeline with the instance and the engine
//! facts, and stores the answer beside the session. It never blocks or fails a launch: every
//! failure becomes a stored "could not analyze: <why>", and a panic is caught here too.
//!
//! It holds no game knowledge. The sources, parsers and rules all come from packs; the instance
//! view comes from core's attribution; only the generic engine facts are built here.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use agora_game_api::crash::{
    Artifact, AttrValue, BuiltInPack, CopyPolicy, CrashAnswer, Fact, GameHandle, MatchedBy,
    ParserLimits, Reads, RefMap, RoleRule, Scalar, SessionVerdict, SourceRole, DETECT_HEAD_BYTES,
};
use agora_game_api::GamePath;
use agora_plugin_api::host::{ScriptHost, ScriptReads};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::crash_engine::attribution::Resolver;
use crate::crash_engine::evidence::{
    collect_sources, ArtifactFacts, CollectionLimits, CopiedSession, CopyLimits, CrashSession,
    EvidenceStore, FoundArtifact, ProcessFacts, RetentionLimits, SessionWindow, SourceCollection,
    SourceLocation, SourceRoots, SourceSpec, EVIDENCE_STORE_DIR,
};
use crate::crash_engine::{
    analyze_with_history, history::LocalConfirmation, history::RecordedCrash, AnalyzeInput,
    AnalyzedFile, CrashFile, InstanceAnalysis, InstanceInspector,
};
use crate::crash_packs::{
    artifact_handle, detect_parsers_with_snippets, load_built_in_packs, parser_request,
    run_parser_with_builtin, LoadedCrashPack, MemoryArtifact, ParserDispatch, BUILT_IN_PLUGIN_ID,
};
use crate::ctx::Ctx;
use crate::session_ledger::{self, ExitObservation, ManifestChange, SessionRecord};

/// The answer stored for one session. A failed analysis is an answer too: it says what went wrong
/// instead of being absent.
// The analyzed variant is the serialized contract, and its size only grows. Boxing its fields to
// satisfy this lint would change the shape every reader matches against for no runtime gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StoredAnswer {
    /// The one analyze pipeline ran.
    Analyzed {
        answer: Box<CrashAnswer>,
        files: Vec<AnalyzedFile>,
        /// What changed since the last session that ended normally, when one exists. Stored so the
        /// answer screen can list it later; absent in answers written before this was kept.
        #[serde(default)]
        changes: Vec<ManifestChange>,
        /// What this crash contributes to the pattern across crashes. Older answers stored before
        /// this field existed have none, and are still read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<RecordedCrash>,
        /// Whole-file content hashes of the artifacts this session read. Kept for storage dedup and
        /// history sweep identity; live evidence exclusion uses occurrences. Older answers have none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifact_hashes: Vec<String>,
        /// Artifact occurrences this session read. Two separate crashes with byte-identical logs are
        /// different occurrences and both remain evidence. Older answers have none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifact_occurrences: Vec<ArtifactOccurrence>,
    },
    /// Nothing was readable for this session. That is not a clean bill of health.
    NothingToRead { why: String },
    /// The analysis itself failed or panicked. The launch path was not touched.
    CouldNotAnalyze { why: String },
}

/// One artifact occurrence a session read as evidence: file origin plus when it was last written,
/// bound to the session that consumed it. The same bytes elsewhere or at another time are not the
/// same event.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ArtifactOccurrence {
    /// The artifact's path when it was read.
    pub path: String,
    /// When the file was last written, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    /// File size when read, to tell a rewritten-in-place file apart when times are coarse.
    #[serde(default)]
    pub size: u64,
}

/// Evidence identities another session's stored answer already consumed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnsweredEvidence {
    /// Occurrences read by answers that recorded them.
    pub occurrences: BTreeSet<ArtifactOccurrence>,
    /// Content hashes from every answered artifact, for storage/history dedup.
    pub hashes: BTreeSet<String>,
    /// Hashes from answers that predate occurrence tracking; used only as a compatibility fallback.
    pub legacy_hashes: BTreeSet<String>,
}

/// One session's stored answer, with the ids needed to show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAnswer {
    pub session_id: String,
    pub instance_id: String,
    pub created_at: String,
    pub answer: StoredAnswer,
}

/// A session the catch-up closed, and the answer it stored when the evidence named a crash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaughtUp {
    pub session_id: String,
    pub instance_id: String,
    /// `None` when the session was already answered, or when nothing in the evidence named a crash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<SessionAnswer>,
}

/// What the catch-up did with this instance's per-user files, when a finished swap session was
/// still holding them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserFilesCatchUp {
    pub instance_id: String,
    pub game: String,
    pub store: String,
    pub restored: usize,
    pub changed: usize,
    /// Why the files are still swapped in. A journal that exists was not treated as success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything one catch-up pass did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatchUpReport {
    pub sessions: Vec<CaughtUp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_files: Option<UserFilesCatchUp>,
}

/// Store an answer, replacing an earlier one for the same session. The answer is a few KB of
/// JSON and lives under the same retention limit as its history row.
pub fn store_answer(conn: &Connection, stored: &SessionAnswer) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO session_answers (session_id, instance_id, created_at, answer_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET
             instance_id = excluded.instance_id,
             created_at = excluded.created_at,
             answer_json = excluded.answer_json",
        params![
            stored.session_id,
            stored.instance_id,
            stored.created_at,
            serde_json::to_string(&stored.answer)?,
        ],
    )?;
    Ok(())
}

/// The stored answer for one session.
pub fn answer_for_session(
    conn: &Connection,
    session_id: &str,
) -> anyhow::Result<Option<SessionAnswer>> {
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT session_id, instance_id, created_at, answer_json
               FROM session_answers WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    Ok(row.map(decode_answer))
}

/// Every stored answer for an instance, newest session first. The newest is the card the overview
/// shows; the rest are the instance's earlier crashes. "Newest" is the latest session by when it
/// started, because an older session can be diagnosed later (a catch-up) and must not displace a
/// crash the player already saw. A session with no row in `launch_history` (only possible for an
/// answer written by hand) is ordered by when it was stored.
pub fn answers_for_instance(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<SessionAnswer>> {
    let mut stmt = conn.prepare(
        "SELECT a.session_id, a.instance_id, a.created_at, a.answer_json
           FROM session_answers a
           LEFT JOIN launch_history h ON h.session_id = a.session_id
          WHERE a.instance_id = ?1
          ORDER BY COALESCE(h.started_at, a.created_at) DESC, h.id DESC, a.session_id DESC",
    )?;
    let rows = stmt
        .query_map([instance_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(decode_answer).collect())
}

/// What a session was, for telling a reader which one an answer belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBrief {
    pub session_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub verdict: Option<SessionVerdict>,
}

impl From<SessionRecord> for SessionBrief {
    fn from(record: SessionRecord) -> Self {
        Self {
            session_id: record.session_id,
            started_at: record.started_at,
            ended_at: record.ended_at,
            verdict: record.verdict,
        }
    }
}

/// The newest session and the newest stored answer, which need not be the same session. A launch
/// with no answer must not make an older one look like the newest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatestAnswer {
    /// The newest session the ledger knows, answered or not.
    pub newest_session: Option<SessionBrief>,
    /// The newest stored answer, by the session it belongs to.
    pub answer: Option<SessionAnswer>,
    /// The session that answer belongs to, when the ledger still knows it.
    pub answer_session: Option<SessionBrief>,
}

/// The newest session beside the newest stored answer. Use this when telling a person what they are
/// reading; [`latest_answer`] alone cannot say that the newest session has no answer yet.
pub fn latest_answer_with_context(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<LatestAnswer> {
    let newest_session = session_ledger::list(conn, instance_id, 1)?
        .into_iter()
        .next()
        .map(SessionBrief::from);
    let answer = answers_for_instance(conn, instance_id)?.into_iter().next();
    let answer_session = answer
        .as_ref()
        .and_then(|answer| session_ledger::get(conn, &answer.session_id).ok().flatten())
        .map(SessionBrief::from);
    Ok(LatestAnswer {
        newest_session,
        answer,
        answer_session,
    })
}

/// The newest stored answer for an instance: the answer of the latest session by when it started,
/// not of the session whose answer was written most recently. A catch-up that diagnoses an old
/// session later must not make it look newer than a crash the player already saw.
pub fn latest_answer(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Option<SessionAnswer>> {
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT a.session_id, a.instance_id, a.created_at, a.answer_json
               FROM session_answers a
               LEFT JOIN launch_history h ON h.session_id = a.session_id
              WHERE a.instance_id = ?1
              ORDER BY COALESCE(h.started_at, a.created_at) DESC, h.id DESC, a.session_id DESC
              LIMIT 1",
            [instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    Ok(row.map(decode_answer))
}

/// One stored answer. A row this build cannot decode is reported as "could not analyze" for that
/// session, never as an error that hides the other crashes: the database says it is there, and a
/// reader must not turn "could not read" into "nothing there".
fn decode_answer(
    (session_id, instance_id, created_at, json): (String, String, String, String),
) -> SessionAnswer {
    let answer =
        serde_json::from_str(&json).unwrap_or_else(|error| StoredAnswer::CouldNotAnalyze {
            why: format!("the stored crash answer could not be read: {error}"),
        });
    SessionAnswer {
        session_id,
        instance_id,
        created_at,
        answer,
    }
}

/// The answer's candidates in the old test-it flow's shape, in rank order. Only an installed item
/// the engine may test is included; a framework or hook framework never becomes a suspect, and an
/// item the instance view cannot list carries its display name in place of a file.
pub fn suspects_for_test(
    answer: &CrashAnswer,
    view: Option<&crate::crash_engine::attribution::InstanceView>,
) -> Vec<crate::crash_service::SuspectScore> {
    crate::crash_engine::suspects::installable_candidates(answer, view)
        .into_iter()
        // An item the instance view cannot list has no file to disable. Offering its display
        // name as if it were one presents a stale suspect as a live experiment.
        .filter(|candidate| candidate.file.is_some())
        .map(|candidate| crate::crash_service::SuspectScore {
            mod_id: candidate.owner_key,
            filename: candidate.file.unwrap_or(candidate.name),
            total_score: f64::from(candidate.score),
            breakdown: serde_json::json!({ "level": candidate.level }),
            is_dependent_of: None,
            under_review: false,
        })
        .collect()
}

/// The newest stored answer for an instance when it is an engine diagnosis. "Nothing to read" and
/// "could not analyze" carry no suspects, so they read as no answer here.
pub fn latest_analyzed_answer(
    ctx: &Ctx,
    instance_id: &str,
) -> crate::error::LauncherResult<Option<CrashAnswer>> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).map_err(|error| {
        crate::error::LauncherError::Generic {
            code: "ERR_LOCAL_STATE_FAILED".into(),
            message: error.to_string(),
        }
    })?;
    let stored = latest_answer(&conn, instance_id).map_err(|error| {
        crate::error::LauncherError::Generic {
            code: "ERR_CRASH_ANSWER".into(),
            message: error.to_string(),
        }
    })?;
    Ok(stored.and_then(|stored| match stored.answer {
        StoredAnswer::Analyzed { answer, .. } => Some(*answer),
        _ => None,
    }))
}

/// The next experiment once the player says the game still crashes without
/// `ruled_out_owner_key`. The answer already ranks what remains; the ruled-out candidate is
/// filtered out so the next experiment is about a different item. No answer means no suspects.
pub fn next_suspects(
    answer: Option<&CrashAnswer>,
    view: Option<&crate::crash_engine::attribution::InstanceView>,
    ruled_out_owner_key: &str,
) -> crate::crash_service::InvestigationResult {
    use crate::crash_service::{InvestigationResult, SuggestedAction};
    let mut suspects = answer
        .map(|answer| suspects_for_test(answer, view))
        .unwrap_or_default();
    suspects.retain(|suspect| suspect.mod_id != ruled_out_owner_key);
    let suggested_action = suspects
        .first()
        .map_or(SuggestedAction::NoSuspects, |next| {
            SuggestedAction::GuidedDisable {
                next_suspect: next.clone(),
            }
        });
    InvestigationResult {
        suspects,
        suggested_action,
        ruled_out: vec![ruled_out_owner_key.to_string()],
    }
}

/// [`next_suspects`] for an instance: reads its newest stored answer and, when there is one, the
/// instance view that answer's candidates are matched against. `instances` is the game package's
/// own instance system, as for [`crate::crash_engine::instance_view::resolve_crash_instance`].
pub fn report_still_crashing(
    ctx: &Ctx,
    instance_id: &str,
    ruled_out_owner_key: &str,
    instances: Option<&dyn crate::crash_engine::instance_view::ExtraInstanceProvider>,
) -> crate::error::LauncherResult<crate::crash_service::InvestigationResult> {
    let Some(answer) = latest_analyzed_answer(ctx, instance_id)? else {
        return Ok(next_suspects(None, None, ruled_out_owner_key));
    };
    let resolved =
        crate::crash_engine::instance_view::resolve_crash_instance(ctx, instance_id, instances)?;
    Ok(next_suspects(
        Some(&answer),
        Some(&resolved.view),
        ruled_out_owner_key,
    ))
}

/// The file name an engine candidate provides in `view`, so the experiment flow can disable it.
/// The owner key must name an installed item (`item:<content key>`); a key the view cannot list is
/// an error, never a silent no-op.
pub fn suspect_file(
    view: &crate::crash_engine::attribution::InstanceView,
    owner_key: &str,
) -> crate::error::LauncherResult<String> {
    let content_key =
        owner_key
            .strip_prefix("item:")
            .ok_or_else(|| crate::error::LauncherError::Generic {
                code: "ERR_CRASH_SUSPECT_NOT_ITEM".into(),
                message: "That suspect is not an installed item, so it cannot be turned off."
                    .to_string(),
            })?;
    view.item_files(content_key)
        .and_then(|files| files.into_iter().next())
        .and_then(|path| path.rsplit(['/', '\\']).next().map(str::to_string))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| crate::error::LauncherError::Generic {
            code: "ERR_CRASH_SUSPECT_NO_FILE".into(),
            message: "Agora could not find the file for that suspect in this instance.".to_string(),
        })
}

/// [`suspect_file`] for an instance, resolving its view the way every crash reader does.
pub fn resolve_suspect_file(
    ctx: &Ctx,
    instance_id: &str,
    owner_key: &str,
    instances: Option<&dyn crate::crash_engine::instance_view::ExtraInstanceProvider>,
) -> crate::error::LauncherResult<String> {
    let resolved =
        crate::crash_engine::instance_view::resolve_crash_instance(ctx, instance_id, instances)?;
    suspect_file(&resolved.view, owner_key)
}

/// The packs and host a live analysis runs with. The caller loads the packs (the plugin service
/// and the game package's built-ins); core decides what to do with them.
pub struct LivePacks<'a> {
    pub packs: &'a [LoadedCrashPack],
    pub built_in: &'a [BuiltInPack],
    pub host: &'a dyn ScriptHost,
    /// The game package that owns this instance system, when core does not read it itself. A
    /// Minecraft session needs this; a generic game instance leaves it `None`.
    pub instances: Option<&'a dyn crate::crash_engine::instance_view::ExtraInstanceProvider>,
}

/// How far back `crash investigate` looks when the instance has no recorded session to take a
/// window from.
pub const INVESTIGATION_RECENT_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(60 * 60);

/// The window `crash investigate` searches with: the newest recorded session's span, or a recent
/// span when the instance has no session yet. The live diagnosis uses the same [`SessionWindow`]
/// and the same pack sources; this only decides which span an unwatched search covers.
pub fn investigation_window(ctx: &Ctx, instance_id: &str) -> SessionWindow {
    let now = ctx.clock.now();
    let latest = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .ok()
        .and_then(|conn| crate::session_ledger::list(&conn, instance_id, 1).ok())
        .and_then(|mut sessions| sessions.pop());
    if let Some(record) = latest {
        if let Ok(started) = parse_time(&record.started_at) {
            let ended = record
                .ended_at
                .as_deref()
                .and_then(|text| parse_time(text).ok())
                .unwrap_or(now);
            return SessionWindow::new(started, ended.max(started));
        }
    }
    SessionWindow::new(
        now.checked_sub(INVESTIGATION_RECENT_WINDOW).unwrap_or(now),
        now,
    )
}

/// The evidence one instance's packs name for a window, found the one way the live diagnosis and
/// `crash investigate` both search. It holds no game knowledge: the folders, formats and binds all
/// come from the packs.
pub struct InstanceEvidence {
    pub resolved: crate::crash_engine::instance_view::ResolvedCrashInstance,
    /// Every pack that may read this evidence: the installed packs plus the built-ins, unfiltered,
    /// because the analyzer and its inspectors take the whole set.
    pub packs: Vec<LoadedCrashPack>,
    pub window: SessionWindow,
    pub collection: SourceCollection,
    /// The explicit files the caller named, resolved and safe to read as text.
    pub explicit: Vec<PathBuf>,
}

/// One evidence file as `crash investigate` lists it.
#[derive(Debug, Clone)]
pub struct InstanceEvidenceLine {
    pub basename: String,
    pub primary: bool,
}

impl InstanceEvidence {
    /// The packs that apply to this instance's game, in the same order the live path uses.
    pub fn game_packs(&self) -> impl Iterator<Item = &LoadedCrashPack> {
        let game_id = self.resolved.game_id.as_str();
        self.packs.iter().filter(move |pack| {
            pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == game_id)
        })
    }

    /// Whether a found artifact came from a source the pack marks `primary`.
    fn is_primary(&self, found: &FoundArtifact) -> bool {
        self.game_packs().any(|pack| {
            pack.plugin_id == found.plugin_id
                && pack.pack.sources.iter().any(|source| {
                    source.id == found.source_id && source.role == SourceRole::Primary
                })
        })
    }

    /// Bound artifacts, primary first and newest first. A folder source stays whole; its files are
    /// read when the analyzer needs them.
    fn ordered_bound_artifacts(&self) -> Vec<(bool, &FoundArtifact)> {
        let mut found: Vec<(bool, &FoundArtifact)> = self
            .collection
            .bound
            .iter()
            .map(|artifact| (self.is_primary(artifact), artifact))
            .collect();
        found.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.modified.cmp(&left.1.modified))
                .then_with(|| left.1.name.cmp(&right.1.name))
        });
        found
    }

    /// Bound files only, for the evidence list and the legacy collected view. A folder is one
    /// source, not one crash log.
    fn ordered_bound(&self) -> Vec<(bool, &FoundArtifact)> {
        self.ordered_bound_artifacts()
            .into_iter()
            .filter(|(_, found)| found.unit == agora_game_api::crash::ArtifactUnit::File)
            .collect()
    }

    /// The evidence lines `crash investigate` prints, in reading order.
    pub fn lines(&self) -> Vec<InstanceEvidenceLine> {
        let mut primary_seen = false;
        let mut lines: Vec<InstanceEvidenceLine> = self
            .ordered_bound()
            .into_iter()
            .map(|(primary_source, found)| {
                let primary = primary_source && !primary_seen;
                if primary {
                    primary_seen = true;
                }
                InstanceEvidenceLine {
                    basename: found.name.clone(),
                    primary,
                }
            })
            .collect();
        for path in &self.explicit {
            lines.push(InstanceEvidenceLine {
                basename: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".to_string()),
                primary: lines.is_empty(),
            });
        }
        lines
    }

    /// The same evidence in the older [`crate::crash_evidence::CollectedEvidence`] shape the
    /// desktop's crash board reads. The pack's `primary` role becomes the crash-log kind and
    /// everything else a supporting log; no game's names are involved.
    pub fn collected(&self) -> crate::crash_evidence::CollectedEvidence {
        use crate::crash_evidence::{CrashEvidenceService, EvidenceCandidate, EvidenceSourceKind};
        let mut primary_seen = false;
        let candidates: Vec<EvidenceCandidate> = self
            .ordered_bound()
            .into_iter()
            .map(|(primary_source, found)| {
                let primary = primary_source && !primary_seen;
                if primary {
                    primary_seen = true;
                }
                EvidenceCandidate {
                    path: found.path.clone(),
                    kind: if primary {
                        EvidenceSourceKind::CrashReport
                    } else {
                        EvidenceSourceKind::LatestLog
                    },
                    supplementary: !primary,
                    modified_at: found.modified.unwrap_or(std::time::UNIX_EPOCH),
                }
            })
            .collect();
        CrashEvidenceService::new().collect_candidates(&candidates, &self.explicit)
    }

    /// The bound and explicit files as whole-file analyzer inputs, read where they are. A file
    /// larger than the live path's copy limit is read as its head and tail, exactly as the store
    /// would keep it.
    pub fn files(&self) -> Vec<crate::crash_engine::CrashFile> {
        use crate::crash_engine::CrashFile;
        let mut files: Vec<CrashFile> = Vec::new();
        for (_, found) in self.ordered_bound_artifacts() {
            match found.unit {
                agora_game_api::crash::ArtifactUnit::File => {
                    let Some(bytes) = read_for_parsing(&found.path) else {
                        continue;
                    };
                    files.push(CrashFile {
                        name: found.name.clone(),
                        bytes,
                        origin: crate::crash_engine::CrashFileOrigin::Collected,
                        source: Some(found.source_id.clone()),
                    });
                }
                agora_game_api::crash::ArtifactUnit::Folder => {
                    files.extend(folder_crash_files(found));
                }
            }
        }
        for path in &self.explicit {
            let Some(bytes) = read_for_parsing(path) else {
                continue;
            };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            files.push(CrashFile {
                name,
                bytes,
                origin: crate::crash_engine::CrashFileOrigin::File,
                source: None,
            });
        }
        files
    }
}

/// The artifact id a parser is given while collection reads a candidate's process facts. It is the
/// same id the analyzer uses; a parser never sees a path.
const FACTS_ARTIFACT_ID: &str = "artifact";

/// Process facts read from a candidate artifact, so a pack's `pid_in_facts` and
/// `working_dir_in_facts` binds work in live collection as they do in a hand-run test. The same
/// file is parsed at most once per collection: a source can declare both binds.
struct LiveArtifactFacts<'a> {
    packs: &'a [LoadedCrashPack],
    built_in: &'a [BuiltInPack],
    host: &'a dyn ScriptHost,
    game_id: &'a str,
    runtime_version: Option<&'a str>,
    cache: RefCell<BTreeMap<PathBuf, Option<ProcessFacts>>>,
}

impl<'a> LiveArtifactFacts<'a> {
    fn new(
        packs: &'a [LoadedCrashPack],
        built_in: &'a [BuiltInPack],
        host: &'a dyn ScriptHost,
        game_id: &'a str,
        runtime_version: Option<&'a str>,
    ) -> Self {
        Self {
            packs,
            built_in,
            host,
            game_id,
            runtime_version,
            cache: RefCell::new(BTreeMap::new()),
        }
    }

    /// Run the primary parser a pack's own detection chooses for this file and read the
    /// `core.process` fact it reports. A parser that fails or does not recognise the file
    /// contributes nothing; that is not a contradiction, so the other binds still decide.
    fn read_facts(&self, path: &Path) -> Option<ProcessFacts> {
        let name = path.file_name()?.to_string_lossy().into_owned();
        let bytes = read_facts_bytes(path)?;
        if bytes.is_empty() {
            return None;
        }
        let head = &bytes[..bytes.len().min(DETECT_HEAD_BYTES)];
        let limits = ParserLimits::DEFAULT;
        for pack in self.packs {
            if !(pack.pack.games.is_empty()
                || pack.pack.games.iter().any(|game| game == self.game_id))
            {
                continue;
            }
            let built_in = if pack.plugin_id == BUILT_IN_PLUGIN_ID {
                self.built_in
                    .iter()
                    .find(|built_in| built_in.file == pack.file)
            } else {
                None
            };
            let Ok(parsers) = detect_parsers_with_snippets(&pack.pack, &name, head) else {
                continue;
            };
            let Some(parser) = parsers
                .iter()
                .copied()
                .find(|parser| parser.reads == Reads::Primary)
            else {
                continue;
            };
            let mut reads = MemoryArtifact::new(limits);
            reads.insert(FACTS_ARTIFACT_ID, bytes.clone());
            let reads: Arc<dyn ScriptReads> = Arc::new(reads);
            let handle = artifact_handle(
                FACTS_ARTIFACT_ID,
                name.clone(),
                bytes.len() as u64,
                Vec::new(),
            );
            let request = parser_request(
                "",
                handle,
                GameHandle {
                    id: self.game_id.to_string(),
                    runtime_version: self.runtime_version.map(str::to_string),
                },
                limits,
            );
            let attempt = run_parser_with_builtin(
                self.host,
                &pack.plugin_id,
                parser,
                &request,
                reads,
                built_in,
            );
            let ParserDispatch::Parsed(output) = attempt.outcome else {
                continue;
            };
            let facts = process_facts(&output.facts);
            if facts.pid.is_some() || facts.working_dir.is_some() {
                return Some(facts);
            }
        }
        None
    }
}

impl ArtifactFacts for LiveArtifactFacts<'_> {
    fn process_facts(&self, artifact: &Path) -> Option<ProcessFacts> {
        if let Some(cached) = self.cache.borrow().get(artifact) {
            return cached.clone();
        }
        let facts = self.read_facts(artifact);
        self.cache
            .borrow_mut()
            .insert(artifact.to_path_buf(), facts.clone());
        facts
    }
}

/// The file's bytes, bounded by what one parser read may hold. A larger file is not parsed here:
/// the analyzer would refuse it the same way, so there are no facts to find.
fn read_facts_bytes(path: &Path) -> Option<Vec<u8>> {
    let size = std::fs::metadata(path).ok()?.len();
    if size > ParserLimits::DEFAULT.max_total_read_bytes {
        return None;
    }
    std::fs::read(path).ok()
}

/// Pull the pid and working folder a parser reported for the artifact.
fn process_facts(facts: &[Fact]) -> ProcessFacts {
    let mut process = ProcessFacts::default();
    for fact in facts.iter().filter(|fact| fact.kind == "core.process") {
        if process.pid.is_none() {
            process.pid = match fact.attrs.get("pid") {
                Some(AttrValue::Scalar(Scalar::Number(pid))) => number_to_pid(*pid),
                Some(AttrValue::Scalar(Scalar::Text(pid))) => pid.parse().ok(),
                _ => None,
            };
        }
        if process.working_dir.is_none() {
            process.working_dir = match fact.attrs.get("cwd") {
                Some(AttrValue::Scalar(Scalar::Text(path))) if !path.is_empty() => {
                    Some(PathBuf::from(path))
                }
                _ => None,
            };
        }
    }
    process
}

fn number_to_pid(pid: f64) -> Option<u32> {
    (pid.is_finite() && pid >= 0.0 && pid <= f64::from(u32::MAX)).then_some(pid as u32)
}

/// Search the folders the applicable packs name for the evidence of one window, plus the files the
/// caller named. This is the one search the live diagnosis and `crash investigate` both use.
pub fn collect_instance_evidence(
    ctx: &Ctx,
    instance_id: &str,
    live: &LivePacks<'_>,
    session: &CrashSession,
    explicit_files: &[PathBuf],
) -> Result<InstanceEvidence, String> {
    let resolved = crate::crash_engine::instance_view::resolve_crash_instance(
        ctx,
        instance_id,
        live.instances,
    )
    .map_err(|error| format!("the instance could not be read: {error}"))?;
    let game_id = resolved.game_id.clone();

    let built_in_report = load_built_in_packs(live.built_in);
    let mut packs: Vec<LoadedCrashPack> = live.packs.to_vec();
    packs.extend(built_in_report.packs);

    let instance_dir = ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|error| format!("the instance folder could not be resolved: {error}"))?;
    let roots = InstanceRoots {
        instance_dir,
        runtime_dir: crash_runtime_root(ctx, instance_id),
        proton_prefix: crate::game_instance::proton_prefix(ctx, instance_id),
    };
    let specs: Vec<SourceSpec<'_>> = packs
        .iter()
        .filter(|pack| {
            pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == &game_id)
        })
        .flat_map(|pack| {
            pack.pack.sources.iter().map(|source| SourceSpec {
                plugin_id: &pack.plugin_id,
                source,
            })
        })
        .collect();
    let facts = LiveArtifactFacts::new(
        &packs,
        live.built_in,
        live.host,
        &game_id,
        resolved.runtime_version.as_deref(),
    );
    let collection = collect_sources(
        &specs,
        &roots,
        &facts as &dyn ArtifactFacts,
        session,
        CollectionLimits::default(),
    );
    let explicit = explicit_files
        .iter()
        .filter_map(|path| {
            let resolved = std::fs::canonicalize(path).ok()?;
            (resolved.is_file()
                && crate::crash_evidence::CrashEvidenceService::is_safe_text_path(&resolved))
            .then_some(resolved)
        })
        .collect();

    Ok(InstanceEvidence {
        resolved,
        packs,
        window: session.window,
        collection,
        explicit,
    })
}

/// Read a folder source's files as analyzer inputs, without copying the folder. The walk follows
/// the same copy limits, reparse-point rule and child copy policy as the live path's store copy, so
/// a folder source finds the same files whether a session or `crash investigate` reads it.
fn folder_crash_files(found: &FoundArtifact) -> Vec<crate::crash_engine::CrashFile> {
    use crate::crash_engine::CrashFile;
    use agora_game_api::crash::CopyPolicy;

    let limits = CopyLimits::default();
    let mut files = Vec::new();
    let mut stack = vec![(found.path.clone(), 0usize)];
    let mut entries = 0usize;
    while let Some((folder, depth)) = stack.pop() {
        if depth > limits.max_depth {
            break;
        }
        let Ok(read) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in read {
            entries += 1;
            if entries > limits.max_entries {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if crate::content_store::is_reparse_point_or_symlink(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = relative_artifact_name(&found.path, &path);
            if !crate::crash_engine::evidence::is_safe_artifact_name(&relative) {
                continue;
            }
            let child = found.children.iter().find(|policy| {
                let name = relative.rsplit('/').next().unwrap_or(&relative);
                crate::crash_engine::evidence::glob_matches(&policy.glob, &relative)
                    || crate::crash_engine::evidence::glob_matches(&policy.glob, name)
            });
            let copy = child.map(|policy| policy.copy).unwrap_or(found.copy);
            if matches!(copy, CopyPolicy::Never | CopyPolicy::FactsOnly) {
                continue;
            }
            let Some(bytes) = read_for_parsing(&path) else {
                continue;
            };
            files.push(CrashFile {
                name: relative,
                bytes,
                origin: crate::crash_engine::CrashFileOrigin::Collected,
                source: Some(found.source_id.clone()),
            });
        }
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    files
}

fn relative_artifact_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Read a file the way the live path's copy would keep it, without writing a copy: whole up to the
/// copy limit, else its head and tail.
fn read_for_parsing(path: &Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let limits = CopyLimits::default();
    let size = std::fs::metadata(path).ok()?.len();
    if size <= limits.max_file_bytes {
        return std::fs::read(path).ok();
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; limits.head_bytes as usize];
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    file.seek(SeekFrom::End(-(limits.tail_bytes as i64))).ok()?;
    let mut tail = vec![0u8; limits.tail_bytes as usize];
    let read = file.read(&mut tail).ok()?;
    tail.truncate(read);
    head.extend_from_slice(&tail);
    Some(head)
}

/// Load every installed plugin's crash packs, activating the script host for any that need it.
/// The game package supplies its built-in packs separately, because core cannot name a game; the
/// live path lints and applies those itself.
pub fn load_installed_packs(
    ctx: &Ctx,
    host: std::sync::Arc<dyn ScriptHost>,
) -> crate::error::LauncherResult<crate::crash_packs::CrashPackReport> {
    use agora_plugin_api::manifest::PluginId;

    let service = crate::plugins::PluginService::headless(ctx.clone(), host);
    service.reload()?;
    let mut report = service.crash_packs();
    // The signed catalog's community signatures ride along wherever installed packs are loaded.
    // They add rules, never parsers, so they need no script host of their own.
    let community = crate::crash_packs::load_community_pack_from_registry(ctx.paths.root());
    report.packs.extend(community.packs);
    report.problems.extend(community.problems);
    let script_ids: Vec<String> = report
        .packs
        .iter()
        .filter(|pack| {
            pack.pack.parsers.iter().any(|parser| {
                matches!(
                    parser.entry,
                    agora_game_api::crash::CodeEntry::Script { .. }
                )
            }) || pack.pack.inspectors.iter().any(|inspector| {
                matches!(
                    inspector.entry,
                    agora_game_api::crash::CodeEntry::Script { .. }
                )
            })
        })
        .map(|pack| pack.plugin_id.clone())
        .collect();
    for id in script_ids {
        if let Ok(plugin_id) = PluginId::parse(&id) {
            let _ = service.activate(&plugin_id);
        }
    }
    Ok(report)
}

/// Diagnose a finished session and store the answer. Returns `None` when the session is not one
/// Agora diagnoses (a normal exit, a Stop, an unknown ending, or an unknown session).
///
/// This is best effort, like the ledger itself: every error is stored as
/// [`StoredAnswer::CouldNotAnalyze`], and a panic is caught. It never fails the caller.
pub fn diagnose_session(ctx: &Ctx, session_id: &str, live: LivePacks<'_>) -> Option<SessionAnswer> {
    diagnose_session_with(ctx, session_id, &live, false)
}

/// Close the sessions of this instance that nobody watched to the end, then diagnose the evidence
/// written since they started.
///
/// Only a session whose owner process is gone **and** whose game is not running is closed: a gone
/// owner proves nothing about the game. A session already finished is never touched, so nothing is
/// diagnosed twice. Closing and diagnosing are best effort, like the ledger itself: a failure
/// returns fewer entries, never an error that could block the caller. An unknown ending is stored
/// as an answer only when the evidence names a crash, so a clean unwatched session stays quiet.
pub fn catch_up(ctx: &Ctx, instance_id: &str, live: LivePacks<'_>) -> CatchUpReport {
    let Ok(conn) = crate::db::local_state_connection(&ctx.paths.local_state_db()) else {
        return CatchUpReport::default();
    };
    let now = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
    let open = session_ledger::open_sessions_for_instance(&conn, instance_id).unwrap_or_default();
    // A stale `Running` lease is proof the game is gone. No lease at all is not: a delegated
    // launch keeps no process identity, so the game may still be running.
    let provably_gone = crate::instance_runtime::game_is_provably_gone(&ctx.paths, instance_id);
    let mut caught = Vec::new();
    for session in open {
        if let (Some(pid), Some(started)) = (session.owner_pid, session.owner_start) {
            if session_ledger::Owner::is_alive(pid, started as u64) {
                continue;
            }
        }
        let mut evidence = None;
        let mut normal_stop = false;
        if !provably_gone {
            // A recorded launch that is still alive, or was never finished starting, is not ours
            // to close. Only a launch with no runtime record at all can be ended on positive
            // evidence: a crash log inside the session, or a log whose last lines say the game
            // stopped normally. A failed check is not evidence.
            if crate::instance_runtime::game_has_record(&ctx.paths, instance_id) {
                continue;
            }
            let Some(record) = session_ledger::get(&conn, &session.session_id)
                .ok()
                .flatten()
            else {
                continue;
            };
            let mut provisional = record.clone();
            provisional.ended_at = Some(now.clone());
            let stored = std::panic::catch_unwind(AssertUnwindSafe(|| {
                diagnose_record(ctx, &conn, &provisional, &live)
            }))
            .unwrap_or_else(|_| Err("the crash analysis panicked".to_string()));
            match stored {
                Ok(diagnosed)
                    if diagnosed.crash_artifact.is_some()
                        || answer_finds_a_crash(&diagnosed.answer) =>
                {
                    evidence = Some(diagnosed);
                }
                Ok(diagnosed) if diagnosed.normal_stop => normal_stop = true,
                _ => continue,
            }
        }
        let (why, judged) = if let Some(found) = &evidence {
            (
                "a crash log was written during the session",
                Some(SessionVerdict::Crashed {
                    how: found.crash_artifact.unwrap_or(MatchedBy::Time),
                }),
            )
        } else if normal_stop {
            (
                "the game's log shows it stopped normally",
                Some(SessionVerdict::Normal),
            )
        } else {
            (
                "Agora was not watching when the game ended, so nothing saw how it ended",
                None,
            )
        };
        let Some(closed) =
            session_ledger::close_open_session(&conn, &session.session_id, &now, why, judged)
                .ok()
                .flatten()
        else {
            continue;
        };
        // A stored answer belongs to a session that was already diagnosed.
        let already_answered = answer_for_session(&conn, &closed.session_id)
            .ok()
            .flatten()
            .is_some();
        let answer = if already_answered {
            None
        } else if let Some(found) = evidence {
            let session_answer = SessionAnswer {
                session_id: closed.session_id.clone(),
                instance_id: closed.instance_id.clone(),
                created_at: now.clone(),
                answer: found.answer,
            };
            let _ = store_answer(&conn, &session_answer);
            Some(session_answer)
        } else if normal_stop {
            None
        } else {
            diagnose_session_with(ctx, &closed.session_id, &live, true)
        };
        caught.push(CaughtUp {
            session_id: closed.session_id,
            instance_id: closed.instance_id,
            answer,
        });
    }
    // A launch path closes this instance's orphans itself before any catch-up runs. Those rows are
    // already closed as `ClosedUnknown`, so the open list above never sees them; a crash in one of
    // them would otherwise be lost. Diagnose the ones with no answer. A window bounded by the next
    // session is always safe; with none, only when the game is provably gone, because a running
    // game could still write into the old session's window.
    let handled: BTreeSet<String> = caught
        .iter()
        .map(|entry| entry.session_id.clone())
        .collect();
    let closed_unknown =
        session_ledger::list(&conn, instance_id, session_ledger::KEPT_SESSIONS).unwrap_or_default();
    for record in closed_unknown {
        if handled.contains(record.session_id.as_str())
            || record.verdict != Some(SessionVerdict::ClosedUnknown)
        {
            continue;
        }
        if answer_for_session(&conn, &record.session_id)
            .ok()
            .flatten()
            .is_some()
        {
            continue;
        }
        let bounded = parse_time(&record.started_at)
            .ok()
            .and_then(|started| {
                next_session_start(&conn, instance_id, started)
                    .ok()
                    .flatten()
            })
            .is_some();
        if !bounded && !provably_gone {
            continue;
        }
        let Some(answer) = diagnose_session_with(ctx, &record.session_id, &live, true) else {
            continue;
        };
        caught.push(CaughtUp {
            session_id: record.session_id,
            instance_id: record.instance_id,
            answer: Some(answer),
        });
    }
    CatchUpReport {
        sessions: caught,
        user_files: restore_user_files_if_finished(ctx, instance_id),
    }
}

/// Put the player's per-user files back when a finished swap session is not running. The journal
/// itself names the game and store; only this instance's journal is restored, and a running game
/// is left alone. A journal that cannot be read, or a restore that fails, is reported as an error
/// and never as success.
fn restore_user_files_if_finished(ctx: &Ctx, instance_id: &str) -> Option<UserFilesCatchUp> {
    let found = crate::game_user_files::restore_finished_for_instance(ctx, instance_id)?;
    Some(UserFilesCatchUp {
        instance_id: instance_id.to_string(),
        game: found.game.as_str().to_string(),
        store: found.store.as_str().to_string(),
        restored: found.restored,
        changed: found.changed,
        error: found.error,
    })
}

fn diagnose_session_with(
    ctx: &Ctx,
    session_id: &str,
    live: &LivePacks<'_>,
    allow_unknown: bool,
) -> Option<SessionAnswer> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    let record = session_ledger::get(&conn, session_id).ok().flatten()?;
    // A long session that exits non-zero is judged `ClosedUnknown` only because no artifact was
    // known when the verdict was written. Losing that race must not lose the crash: the evidence
    // is looked at here, and a log inside the window turns it into `Crashed`.
    let nonzero = is_unexplained_nonzero_exit(&record);
    // A clean exit is normally not a crash to explain. It is still worth an answer when the
    // evidence says the game refused a plugin at startup, which the player may have exited on or
    // played past. The cheap parser-only look runs before any evidence is copied or stored.
    let clean = matches!(record.verdict, Some(SessionVerdict::Normal));
    let startup_refusal = clean && has_refused_load(ctx, &record, live);
    let diagnosable = diagnoses(record.verdict)
        || nonzero
        || startup_refusal
        || (allow_unknown && record.verdict == Some(SessionVerdict::ClosedUnknown));
    if !diagnosable || record.ended_at.is_none() {
        return None;
    }
    let stored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        diagnose_record(ctx, &conn, &record, live)
    }))
    .unwrap_or_else(|_| Err("the crash analysis panicked".to_string()));
    let (answer, crash_artifact, found_refusal) = match stored {
        Ok(diagnosed) => (
            diagnosed.answer,
            diagnosed.crash_artifact,
            diagnosed.startup_refusal,
        ),
        Err(why) => (StoredAnswer::CouldNotAnalyze { why }, None, false),
    };
    // A clean exit with nothing refused at startup is an ordinary session: no answer.
    if clean && !found_refusal {
        return None;
    }
    // Crash evidence inside the session window is the verdict, whatever the exit code or the
    // session's length said. A bound artifact from a primary source is enough on its own; the
    // rules do not have to recognize the log. Corrections are best effort like every ledger write.
    // A clean exit stays clean: a refused plugin is a problem, not a crash.
    let found_crash = crash_artifact.is_some() || answer_finds_a_crash(&answer);
    if !clean {
        if let Some(how) = crash_artifact {
            let _ = session_ledger::mark_crashed(&conn, session_id, how);
        } else if answer_finds_a_crash(&answer) {
            let _ = session_ledger::mark_crashed(&conn, session_id, MatchedBy::Time);
        }
    }
    // An unwatched session with no crash evidence is not a crash to explain; leave it unanswered
    // rather than storing a lead that would be invented from the verdict.
    if allow_unknown && !found_crash {
        return None;
    }
    // A non-zero exit with nothing to read still gets a stored reason. A blank row would read as
    // "nothing happened", which is not what an unobserved abnormal ending means.
    let answer = if clean {
        clean_exit_answer(answer)
    } else if nonzero && !found_crash {
        StoredAnswer::CouldNotAnalyze {
            why: no_crash_evidence_reason(&record),
        }
    } else {
        answer
    };
    let created_at = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339();
    let session_answer = SessionAnswer {
        session_id: record.session_id.clone(),
        instance_id: record.instance_id.clone(),
        created_at,
        answer,
    };
    let _ = store_answer(&conn, &session_answer);
    Some(session_answer)
}

/// Whether an answer actually found a crash: a parsed finding or a named candidate. An empty or
/// `NothingToRead` answer did not.
fn answer_finds_a_crash(answer: &StoredAnswer) -> bool {
    match answer {
        StoredAnswer::Analyzed { answer, .. } => {
            !answer.findings.is_empty() || !answer.candidates.is_empty()
        }
        StoredAnswer::NothingToRead { .. } | StoredAnswer::CouldNotAnalyze { .. } => false,
    }
}

/// The answer for a clean exit: the verdict stays `Normal`, and nobody is a crash suspect. The
/// findings (a refused plugin, say) still name what is wrong and what to do about it.
fn clean_exit_answer(answer: StoredAnswer) -> StoredAnswer {
    match answer {
        StoredAnswer::Analyzed {
            answer,
            files,
            changes,
            record,
            artifact_hashes,
            artifact_occurrences,
        } => {
            let mut answer = *answer;
            answer.verdict = SessionVerdict::Normal;
            answer.candidates.clear();
            StoredAnswer::Analyzed {
                answer: Box::new(answer),
                files,
                changes,
                record,
                artifact_hashes,
                artifact_occurrences,
            }
        }
        other => other,
    }
}

/// A `core.load_verdict` fact saying the game refused a load. The kind is core's own vocabulary,
/// so this decision is game-free; the pack decides which log line produces it.
fn is_refused_load(fact: &Fact) -> bool {
    fact.kind == "core.load_verdict"
        && matches!(
            fact.attrs.get("outcome"),
            Some(AttrValue::Scalar(Scalar::Text(outcome))) if outcome == "refused"
        )
}

/// Whether a clean session's evidence names a refused load. This is a parser-only look: the full
/// diagnosis, with its evidence copy and history, runs only when it finds one, so an ordinary
/// clean session stores and copies nothing.
fn has_refused_load(ctx: &Ctx, record: &SessionRecord, live: &LivePacks<'_>) -> bool {
    let Ok(window) = record_window(record) else {
        return false;
    };
    let pid = match &record.exit {
        Some(ExitObservation::Code { pid, .. }) => Some(*pid),
        _ => None,
    };
    let session = CrashSession {
        session_id: record.session_id.clone(),
        pid,
        working_dir: None,
        window,
    };
    let Ok(evidence) = collect_instance_evidence(ctx, &record.instance_id, live, &session, &[])
    else {
        return false;
    };
    if evidence.game_packs().next().is_none() {
        return false;
    }
    let files = bound_files_for_probe(&evidence.collection);
    if files.is_empty() {
        return false;
    }
    let Ok(output) = crate::crash_engine::analyze(&AnalyzeInput {
        packs: &evidence.packs,
        files: &files,
        runtime_version: evidence.resolved.runtime_version.as_deref(),
        host: live.host,
        built_in: live.built_in,
        instance: None,
    }) else {
        return false;
    };
    output
        .parsed
        .iter()
        .flat_map(|artifact| &artifact.output.facts)
        .any(is_refused_load)
}

/// The bound files as parser input, read where they are. Folders and files over one parser read
/// are left out; this probe only needs the small startup log.
fn bound_files_for_probe(collection: &SourceCollection) -> Vec<CrashFile> {
    let max = agora_game_api::crash::ParserLimits::DEFAULT.max_total_read_bytes;
    collection
        .bound
        .iter()
        .filter(|found| found.unit == agora_game_api::crash::ArtifactUnit::File)
        .filter(|found| found.size <= max)
        .filter_map(|found| {
            let bytes = std::fs::read(&found.path).ok()?;
            Some(CrashFile {
                name: found.name.clone(),
                bytes,
                origin: crate::crash_engine::CrashFileOrigin::Collected,
                source: Some(found.source_id.clone()),
            })
        })
        .collect()
}

/// Store "could not analyze" for a session whose packs or host could not even be prepared. This is
/// still an answer, never a silent absence. Returns `None` for a session Agora does not diagnose.
pub fn record_could_not_analyze(ctx: &Ctx, session_id: &str, why: String) -> Option<SessionAnswer> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    let record = session_ledger::get(&conn, session_id).ok().flatten()?;
    let needed = diagnoses(record.verdict) || is_unexplained_nonzero_exit(&record);
    if !needed || record.ended_at.is_none() {
        return None;
    }
    let stored = SessionAnswer {
        session_id: record.session_id.clone(),
        instance_id: record.instance_id.clone(),
        created_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
        answer: StoredAnswer::CouldNotAnalyze { why },
    };
    let _ = store_answer(&conn, &stored);
    Some(stored)
}

/// A session that exited non-zero but was judged `ClosedUnknown`: a long session whose crash log
/// was not known when the verdict was written. The evidence is the only thing that can promote it
/// to `Crashed`, and it must still be looked at.
fn is_unexplained_nonzero_exit(record: &SessionRecord) -> bool {
    matches!(record.verdict, Some(SessionVerdict::ClosedUnknown))
        && matches!(
            &record.exit,
            Some(ExitObservation::Code { code, .. }) if *code != 0
        )
}

/// The stored reason a non-zero ending with no crash evidence gets. It is a reason, never a
/// verdict and never silence.
fn no_crash_evidence_reason(record: &SessionRecord) -> String {
    let how = match &record.exit {
        Some(ExitObservation::Code { code, .. }) => format!("exit code {code}"),
        Some(ExitObservation::Unavailable { reason }) => {
            format!("an exit Agora could not observe ({reason})")
        }
        None => "no recorded exit".to_string(),
    };
    let ran = record_window(record)
        .ok()
        .and_then(|window| window.ended.duration_since(window.started).ok())
        .map(|elapsed| format!(" after {} second(s)", elapsed.as_secs()))
        .unwrap_or_default();
    format!(
        "The game ended with {how}{ran}, but no crash log or other crash evidence was written \
         inside the session. There is no crash to explain."
    )
}

/// Only a session that crashed, failed to start or froze has a crash to explain. A normal or
/// stopped ending is not a crash, and an unknown ending without evidence is not one either.
fn diagnoses(verdict: Option<SessionVerdict>) -> bool {
    matches!(
        verdict,
        Some(SessionVerdict::Crashed { .. })
            | Some(SessionVerdict::FailedToStart)
            | Some(SessionVerdict::Froze)
    )
}

/// What one diagnosis found beyond the answer itself.
struct Diagnosed {
    answer: StoredAnswer,
    /// A bound artifact from a source that declares itself primary: a crash log written inside the
    /// session window, whatever the rules made of it.
    crash_artifact: Option<MatchedBy>,
    /// A log whose last lines say the game stopped normally.
    normal_stop: bool,
    /// A load the evidence says the game refused at startup, even when the session ended cleanly.
    startup_refusal: bool,
}

/// The first artifact bound from a `primary` source. That declaration is the pack's way of saying
/// "a file from here is a crash log", so the verdict does not depend on a rule recognizing it.
fn primary_crash_artifact(
    collection: &SourceCollection,
    game_packs: &[&LoadedCrashPack],
) -> Option<MatchedBy> {
    collection.bound.iter().find_map(|found| {
        let primary = game_packs.iter().any(|pack| {
            pack.plugin_id == found.plugin_id
                && pack.pack.sources.iter().any(|source| {
                    source.id == found.source_id && source.role == SourceRole::Primary
                })
        });
        primary.then_some(found.matched_by)
    })
}

/// Whether a bound log whose source declares `normal_stop` says the game stopped normally. Only
/// the last lines count: a log that merely stops growing is not evidence, because a paused game
/// also stops logging.
fn log_shows_normal_stop(
    collection: &SourceCollection,
    game_packs: &[&LoadedCrashPack],
    copied: Option<&CopiedSession>,
) -> bool {
    collection.bound.iter().any(|found| {
        let markers = game_packs
            .iter()
            .find(|pack| pack.plugin_id == found.plugin_id)
            .and_then(|pack| {
                pack.pack
                    .sources
                    .iter()
                    .find(|source| source.id == found.source_id)
            })
            .map(|source| source.normal_stop.as_slice())
            .unwrap_or(&[]);
        if markers.is_empty() {
            return false;
        }
        let text = std::fs::read_to_string(&found.path).ok().or_else(|| {
            let copy = copied?;
            let artifact = copy
                .artifacts
                .iter()
                .find(|artifact| artifact.name == found.name)?;
            let path = copy.objects.get(&artifact.id)?;
            std::fs::read_to_string(path).ok()
        });
        text.is_some_and(|text| {
            text.lines()
                .rev()
                .take(40)
                .any(|line| markers.iter().any(|marker| line.contains(marker)))
        })
    })
}

/// Take out evidence another session's stored answer already read. A crash log is not evidence for
/// two crashes: a caught-up session must not claim a log a later session already answered, even
/// when the file still sits in a shared folder. The files stay listed as seen, with the reason, so
/// nothing vanishes silently.
fn exclude_answered_evidence(
    conn: &Connection,
    record: &SessionRecord,
    collection: &mut SourceCollection,
) -> Result<(), String> {
    let answered = crate::crash_history::answered_artifact_identities(
        conn,
        &record.instance_id,
        Some(&record.session_id),
    )
    .map_err(|error| format!("the stored crash answers could not be read: {error}"))?;
    if answered.occurrences.is_empty() && answered.legacy_hashes.is_empty() {
        return Ok(());
    }
    let mut kept = Vec::with_capacity(collection.bound.len());
    for found in collection.bound.drain(..) {
        let already = if found.unit == agora_game_api::crash::ArtifactUnit::File {
            if let Some(occurrence) = artifact_occurrence(&found) {
                if answered.occurrences.contains(&occurrence) {
                    true
                } else if !answered.legacy_hashes.is_empty() {
                    crate::crash_history::content_hash_file(&found.path)
                        .ok()
                        .is_some_and(|hash| answered.legacy_hashes.contains(&hash))
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if already {
            collection
                .seen
                .push(crate::crash_engine::evidence::SeenArtifact {
                    plugin_id: found.plugin_id,
                    source_id: found.source_id,
                    name: found.name,
                    path: found.path,
                    reason: crate::crash_engine::evidence::RejectedReason::AlreadyAnswered,
                    modified_at: found.modified.map(crate::crash_engine::evidence::rfc3339),
                });
            continue;
        }
        kept.push(found);
    }
    collection.bound = kept;
    collection
        .seen
        .sort_by(|left, right| (&left.source_id, &left.name).cmp(&(&right.source_id, &right.name)));
    Ok(())
}

fn artifact_occurrence(found: &FoundArtifact) -> Option<ArtifactOccurrence> {
    (found.unit == agora_game_api::crash::ArtifactUnit::File).then(|| ArtifactOccurrence {
        path: found.path.to_string_lossy().into_owned(),
        modified_at: found.modified.map(crate::crash_engine::evidence::rfc3339),
        size: found.size,
    })
}

fn manifest_owner_view(
    record: &SessionRecord,
) -> Option<crate::crash_engine::attribution::InstanceView> {
    let manifest = record.manifest.as_ref()?;
    if manifest.owner_layers.is_empty() {
        return None;
    }
    Some(crate::crash_engine::attribution::InstanceView::new(
        manifest.owner_layers.clone(),
    ))
}

fn attribution_caveat_note() -> agora_game_api::crash::CouldNotCheck {
    agora_game_api::crash::CouldNotCheck {
        reason: agora_game_api::crash::CouldNotCheckReason::InstanceIncomplete,
        subject: "file ownership at crash time".to_string(),
        detail: Some(
            "This older session did not record which mod owned each file, so it was attributed with the current setup."
                .to_string(),
        ),
        source: None,
    }
}

fn diagnose_record(
    ctx: &Ctx,
    conn: &Connection,
    record: &SessionRecord,
    live: &LivePacks<'_>,
) -> Result<Diagnosed, String> {
    let window = evidence_window(conn, record, ctx.clock.now())?;
    let pid = match &record.exit {
        Some(ExitObservation::Code { pid, .. }) => Some(*pid),
        _ => None,
    };
    let session = CrashSession {
        session_id: record.session_id.clone(),
        pid,
        working_dir: None,
        window,
    };
    let InstanceEvidence {
        resolved,
        packs,
        mut collection,
        ..
    } = collect_instance_evidence(ctx, &record.instance_id, live, &session, &[])?;
    exclude_answered_evidence(conn, record, &mut collection)?;
    let game_id = resolved.game_id.clone();
    let runtime_version = resolved.runtime_version.clone();
    let could_not_check = resolved.could_not_check.clone();
    let game_packs: Vec<&LoadedCrashPack> = packs
        .iter()
        .filter(|pack| {
            pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == &game_id)
        })
        .collect();
    if game_packs.is_empty() {
        return Ok(Diagnosed {
            answer: StoredAnswer::NothingToRead {
                why: format!(
                    "No crash rules apply to the game '{game_id}', so there was nothing to read the \
                     session with."
                ),
            },
            crash_artifact: None,
            normal_stop: false,
            startup_refusal: false,
        });
    }
    // The verdict rests on the evidence, not on whether a rule understood it.
    let crash_artifact = primary_crash_artifact(&collection, &game_packs);

    let store_root = ctx.paths.root().join(EVIDENCE_STORE_DIR);
    let store = EvidenceStore::new(store_root);
    let copied = store.copy(&record.session_id, &collection, CopyLimits::default());
    // Retention is best effort and never touches the session being read.
    let _ = store.retention(
        std::slice::from_ref(&record.session_id),
        RetentionLimits::default(),
    );
    let copied = copied.map_err(|error| format!("the evidence could not be copied: {error}"))?;
    let normal_stop = log_shows_normal_stop(&collection, &game_packs, Some(&copied));

    let mut files = readable_files(&copied);
    // Facts-only artifacts are parsed in place within the copy budget and never stored raw.
    files.extend(facts_only_files(&collection));
    files.sort_by(|left, right| left.name.cmp(&right.name));
    files.dedup_by(|left, right| left.name == right.name && left.source == right.source);
    if files.is_empty() {
        return Ok(Diagnosed {
            answer: StoredAnswer::NothingToRead {
                why: nothing_to_read(&collection.folders, &collection.problems, &copied),
            },
            crash_artifact,
            normal_stop,
            startup_refusal: false,
        });
    }

    let ref_maps: Vec<RefMap> = game_packs
        .iter()
        .flat_map(|pack| pack.pack.ref_maps.iter().cloned())
        .collect();
    let roles: Vec<RoleRule> = game_packs
        .iter()
        .flat_map(|pack| pack.pack.roles.iter().cloned())
        .collect();
    let (owner_view, attributed_with_current_setup) = match manifest_owner_view(record) {
        Some(view) => (view, false),
        None => (resolved.view, true),
    };
    let resolver = Resolver::new(owner_view, ref_maps, roles);
    let scope = std::sync::Arc::new(
        crate::game_script::instance_reads(ctx, &record.instance_id)
            .map_err(|error| format!("the instance files could not be opened: {error}"))?,
    );
    let game_handle = agora_game_api::crash::GameHandle {
        id: game_id.clone(),
        runtime_version: runtime_version.clone(),
    };
    let inspector = InstanceInspector::new(live.host, &packs, &resolver, scope, game_handle);
    let instance = InstanceAnalysis {
        game_id: &game_id,
        resolver: &resolver,
        inspector: &inspector,
        could_not_check: &could_not_check,
    };

    let changes = previous_changes(conn, record)?;
    // A bound crash log is the verdict, whatever the exit code said when the session was closed.
    // The rules must see `crashed`, or a resolved save-load frame would still be capped as unknown.
    let effective_verdict = crash_artifact
        .map(|how| SessionVerdict::Crashed { how })
        .or(record.verdict);
    let mut engine_facts = session_engine_facts(record, effective_verdict, &changes);
    // The same runtime-file check the instance and launch checks run, so a crash can say a
    // framework file the session's plugins needed was missing. A check that could not run is
    // stated, never silently absent.
    if let Some(definition) = agora_game_api::GameId::new(&game_id)
        .ok()
        .and_then(|game| ctx.games.game(&game))
    {
        match crate::game_instance::runtime_findings(ctx, &record.instance_id, definition) {
            Ok(findings) => {
                engine_facts.extend(crate::crash_engine::runtime::runtime_file_facts(&findings))
            }
            Err(error) => engine_facts.push(
                crate::crash_engine::runtime::runtime_file_check_failed(&error.to_string()),
            ),
        }
    } else {
        engine_facts.push(crate::crash_engine::runtime::runtime_file_check_failed(
            "the game's package is not loaded, so its runtime files could not be checked",
        ));
    }

    // The same crash's own files may sit in a source folder, so leave them out of the history
    // comparison by their content identity. Identity is computed once and bounded for large logs
    // (head+tail, like the copy the store keeps) so a huge log is not read in full.
    let identity_limits = CopyLimits::default();
    let bound_files: Vec<&FoundArtifact> = collection
        .bound
        .iter()
        .filter(|found| found.unit == agora_game_api::crash::ArtifactUnit::File)
        .collect();
    let exclude_hashes: Vec<String> = bound_files
        .iter()
        .filter_map(|found| {
            crate::crash_history::bounded_content_identity(&found.path, &identity_limits)
                .ok()
                .map(|id| id.hash)
        })
        .collect();
    let artifact_occurrences: Vec<ArtifactOccurrence> = bound_files
        .iter()
        .filter_map(|found| artifact_occurrence(found))
        .collect();
    let setup_hash = record.manifest.as_ref().map(|manifest| manifest.hash());
    let limits = crate::crash_history::LogSweepLimits::default();
    let (prepared, history_error) = match crate::crash_history::prepare(
        ctx,
        conn,
        crate::crash_history::HistorySetup {
            instance_id: &record.instance_id,
            game_id: &game_id,
            runtime_version: runtime_version.as_deref(),
            packs: &packs,
            built_in: live.built_in,
            host: live.host,
            resolver: &resolver,
            inspector: &inspector,
            could_not_check: &could_not_check,
            setup: setup_hash
                .as_deref()
                .map(|hash| (hash, record.started_at.as_str())),
            exclude_hashes: &exclude_hashes,
            exclude_session_id: Some(&record.session_id),
        },
        limits,
        true,
    ) {
        Ok(prepared) => (Some(prepared), None),
        // A history failure must not erase this crash's diagnosis. It is stated as something
        // Agora could not check, never as "no earlier crashes".
        Err(why) => (None, Some(why)),
    };
    let confirmations = local_confirmations(conn);
    let history_input = prepared
        .as_ref()
        .map(|prepared| prepared.input(&confirmations));

    let mut output = analyze_with_history(
        &AnalyzeInput {
            packs: &packs,
            files: &files,
            runtime_version: runtime_version.as_deref(),
            host: live.host,
            built_in: live.built_in,
            instance: Some(instance),
        },
        &engine_facts,
        &changes,
        effective_verdict.unwrap_or(SessionVerdict::Crashed {
            how: MatchedBy::Supplied,
        }),
        history_input.as_ref(),
    )
    .map_err(|error| format!("the crash analysis failed: {error}"))?;

    if let Some(note) = prepared
        .as_ref()
        .and_then(|prepared| prepared.skipped_note(limits))
    {
        push_history_note(&mut output, note);
    }
    if let Some(why) = history_error {
        push_history_note(
            &mut output,
            format!("the older crash logs could not be read: {why}"),
        );
    }
    if attributed_with_current_setup {
        output
            .answer
            .could_not_check
            .push(attribution_caveat_note());
    }

    let at_unix_ms = Some(crate::crash_engine::evidence::unix_ms(window.started));
    let recorded = RecordedCrash {
        record: output.record.clone(),
        bucket: crate::crash_engine::history::parsed_bucket(&output.parsed),
        at_unix_ms,
    };
    let startup_refusal = output
        .parsed
        .iter()
        .flat_map(|artifact| &artifact.output.facts)
        .any(is_refused_load);

    Ok(Diagnosed {
        answer: StoredAnswer::Analyzed {
            answer: Box::new(output.answer),
            files: output.files,
            changes,
            record: Some(recorded),
            artifact_hashes: exclude_hashes,
            artifact_occurrences,
        },
        crash_artifact,
        normal_stop,
        startup_refusal,
    })
}

/// Every fix the user confirmed on this PC. A failure is not raised: the current crash's diagnosis
/// must not be lost because the optional local memory could not be read.
fn local_confirmations(conn: &Connection) -> Vec<LocalConfirmation> {
    crate::db::all_confirmed_attributions(conn)
        .unwrap_or_default()
        .into_iter()
        .map(|(bucket, owner_key, count)| LocalConfirmation {
            bucket,
            owner_key,
            count: count.clamp(0, u32::MAX as i64) as u32,
        })
        .collect()
}

fn push_history_note(output: &mut crate::crash_engine::AnalyzeOutput, note: String) {
    output
        .answer
        .could_not_check
        .push(agora_game_api::crash::CouldNotCheck {
            reason: agora_game_api::crash::CouldNotCheckReason::NotRetained,
            subject: "older crash logs".to_string(),
            detail: Some(note),
            source: None,
        });
}

fn record_window(record: &SessionRecord) -> Result<SessionWindow, String> {
    let started = parse_time(&record.started_at)?;
    let ended = record
        .ended_at
        .as_deref()
        .ok_or_else(|| "the session has no end time".to_string())
        .and_then(parse_time)?;
    Ok(SessionWindow::new(started, ended))
}

/// The span a session's evidence may cover. A known ending ends it. An unknown ending (a session
/// Agora caught up on, or one nobody finished) ends when the next session of the same instance
/// started, so it can never swallow a later session's crash log; with no later session it ends at
/// `now`, because nothing else can bound it.
fn evidence_window(
    conn: &Connection,
    record: &SessionRecord,
    now: SystemTime,
) -> Result<SessionWindow, String> {
    let started = parse_time(&record.started_at)?;
    let end_is_unknown = record.ended_at.is_none()
        || matches!(record.verdict, None | Some(SessionVerdict::ClosedUnknown));
    let ended = if end_is_unknown {
        next_session_start(conn, &record.instance_id, started)?.unwrap_or(now)
    } else {
        record
            .ended_at
            .as_deref()
            .ok_or_else(|| "the session has no end time".to_string())
            .and_then(parse_time)?
    };
    Ok(SessionWindow::new(started, ended.max(started)))
}

/// When the next session of this instance started, after `after`. Times are parsed rather than
/// compared as text: rows written by older builds may use a different RFC 3339 offset.
fn next_session_start(
    conn: &Connection,
    instance_id: &str,
    after: SystemTime,
) -> Result<Option<SystemTime>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT started_at FROM launch_history
              WHERE instance_id = ?1 AND session_id IS NOT NULL",
        )
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([instance_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut next: Option<SystemTime> = None;
    for row in rows {
        let Ok(text) = row else {
            continue;
        };
        let Ok(candidate) = parse_time(&text) else {
            continue;
        };
        if candidate > after && next.is_none_or(|current| candidate < current) {
            next = Some(candidate);
        }
    }
    Ok(next)
}

fn parse_time(text: &str) -> Result<SystemTime, String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|error| format!("the session time '{text}' could not be read: {error}"))?;
    Ok(parsed.with_timezone(&chrono::Utc).into())
}

fn nothing_to_read(
    folders: &[crate::crash_engine::evidence::FolderObservation],
    problems: &[crate::crash_engine::evidence::SourceProblem],
    copied: &CopiedSession,
) -> String {
    let mut why = "No crash log or other evidence for this session could be read.".to_string();
    if let Some(stopped) = copied.stopped_early {
        why.push_str(&format!(
            " Copying stopped early ({stopped:?}), so the evidence is incomplete."
        ));
    }
    if !copied.problems.is_empty() {
        why.push_str(&format!(
            " {} file(s) could not be copied.",
            copied.problems.len()
        ));
    }
    if !problems.is_empty() {
        why.push_str(&format!(
            " {} source(s) could not be looked at.",
            problems.len()
        ));
    }
    if folders.iter().all(|folder| {
        !matches!(
            folder.state,
            crate::crash_engine::evidence::FolderState::Read { .. }
        )
    }) && !folders.is_empty()
    {
        why.push_str(" Every folder a source names was empty, missing or unreadable.");
    }
    why
}

/// Bound facts-only file artifacts read in place for diagnosis. The raw file is never copied to the
/// store; only the parsed facts flow into diagnosis.
fn facts_only_files(collection: &SourceCollection) -> Vec<CrashFile> {
    collection
        .bound
        .iter()
        .filter(|found| {
            found.unit == agora_game_api::crash::ArtifactUnit::File
                && found.copy == CopyPolicy::FactsOnly
        })
        .filter_map(|found| {
            let bytes = read_for_parsing(&found.path)?;
            Some(CrashFile {
                name: found.name.clone(),
                bytes,
                origin: crate::crash_engine::CrashFileOrigin::Collected,
                source: Some(found.source_id.clone()),
            })
        })
        .collect()
}

fn readable_files(copied: &CopiedSession) -> Vec<CrashFile> {
    let mut names: BTreeMap<&str, (&str, Option<&str>)> = BTreeMap::new();
    fn walk<'a>(
        artifacts: &'a [Artifact],
        names: &mut BTreeMap<&'a str, (&'a str, Option<&'a str>)>,
    ) {
        for artifact in artifacts {
            names.insert(
                artifact.id.as_str(),
                (artifact.name.as_str(), artifact.source.as_deref()),
            );
            walk(&artifact.children, names);
        }
    }
    walk(&copied.artifacts, &mut names);

    let mut files = Vec::new();
    for (id, path) in &copied.objects {
        let Some((name, source)) = names.get(id.as_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let file = CrashFile {
            name: (*name).to_string(),
            bytes,
            // Agora found it: a whole file, never read by a snippet parser.
            origin: crate::crash_engine::CrashFileOrigin::Collected,
            source: source.map(str::to_string),
        };
        files.push(file);
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    files
}

/// The differences from the last session that ended normally, or nothing when there is no such
/// session. With no baseline, Agora says nothing rather than inventing a change.
fn previous_changes(
    conn: &Connection,
    record: &SessionRecord,
) -> Result<Vec<ManifestChange>, String> {
    let Some(current) = record.manifest.as_ref() else {
        return Ok(Vec::new());
    };
    let previous =
        session_ledger::previous_normal_manifest(conn, &record.instance_id, &record.started_at)
            .map_err(|error| format!("the last good session could not be read: {error}"))?;
    Ok(previous
        .as_ref()
        .map(|previous| session_ledger::diff_manifests(previous, current))
        .unwrap_or_default())
}

fn session_engine_facts(
    record: &SessionRecord,
    verdict: Option<SessionVerdict>,
    changes: &[ManifestChange],
) -> Vec<Fact> {
    let verdict = match verdict {
        Some(SessionVerdict::Crashed { .. }) => "crashed",
        Some(SessionVerdict::FailedToStart) => "failed_to_start",
        Some(SessionVerdict::Froze) => "froze",
        Some(SessionVerdict::Stopped) => "stopped",
        Some(SessionVerdict::Normal) => "normal",
        Some(SessionVerdict::ClosedUnknown) | None => "closed_unknown",
    };
    let exit_code = match &record.exit {
        Some(ExitObservation::Code { code, .. }) => Some(*code),
        _ => None,
    };
    let runtime_ms = record_window(record)
        .ok()
        .and_then(|window| window.ended.duration_since(window.started).ok())
        .map(|elapsed| elapsed.as_millis() as u64);
    crate::crash_engine::change::session_facts(verdict, exit_code, runtime_ms, changes)
}

/// The folder the game ran from: its deployment when it has one, else the pinned base. The
/// preloader's log lives here, not in the instance's writable layer.
fn crash_runtime_root(ctx: &Ctx, instance_id: &str) -> Option<PathBuf> {
    if let Ok(Some(dir)) = crate::game_deploy::deployment_dir(ctx, instance_id) {
        return Some(dir);
    }
    let manifest = crate::game_instance::get_manifest(ctx, instance_id).ok()?;
    let agora_game_api::BaseReference::Pinned { id, .. } = &manifest.base else {
        return None;
    };
    let text = std::fs::read_to_string(ctx.paths.base_manifest_path(id)).ok()?;
    let base: crate::game_base::BaseManifest = serde_json::from_str(&text).ok()?;
    Some(base.location)
}

/// Resolve the roots a live game's crash sources name. Core resolves the instance and user-data
/// folders; a root it cannot resolve is reported by collection, never guessed.
struct InstanceRoots {
    instance_dir: PathBuf,
    /// The folder the game ran from, when it could be resolved. The preloader writes its log there.
    runtime_dir: Option<PathBuf>,
    /// The Proton prefix the game's Windows user folders live in, when it runs through Proton.
    proton_prefix: Option<PathBuf>,
}

impl SourceRoots for InstanceRoots {
    fn resolve(&self, location: &GamePath) -> Result<SourceLocation, String> {
        match location {
            GamePath::Instance { path } => Ok(SourceLocation {
                base: self.instance_dir.clone(),
                relative: path.as_str().to_string(),
            }),
            GamePath::Runtime { path } => {
                let base = self.runtime_dir.clone().ok_or_else(|| {
                    "the folder the game ran from could not be resolved".to_string()
                })?;
                Ok(SourceLocation {
                    base,
                    relative: path.as_str().to_string(),
                })
            }
            GamePath::UserData { location, path } => {
                let base = match &self.proton_prefix {
                    Some(prefix) => {
                        crate::game_user_files::user_data_root_in_prefix(prefix, location)
                    }
                    None => crate::game_user_files::user_data_root(location).ok_or_else(|| {
                        format!("the user data folder for {location:?} is not known")
                    })?,
                };
                Ok(SourceLocation {
                    base,
                    relative: path.as_str().to_string(),
                })
            }
            other => Err(format!(
                "a live session cannot resolve this source root: {other:?}"
            )),
        }
    }
}

#[cfg(test)]
mod experiment_tests {
    use super::*;
    use crate::crash_engine::attribution::{InstanceView, LayerOwner, LayerView};
    use crate::crash_service::SuggestedAction;
    use agora_game_api::crash::{
        Candidate, Cause, Level, LocalText, NextStep, NextStepKind, Owner, CRASH_CONTRACT_VERSION,
    };

    fn item(key: &str, score: u32) -> Candidate {
        Candidate {
            owner: Owner::Item {
                key: key.to_string(),
                name: key.to_string(),
            },
            level: Level::Possible,
            score,
            cause: Cause::Mod,
            role: None,
            families: Vec::new(),
            sources: Vec::new(),
            also_provided_by: Vec::new(),
            save_risk: None,
        }
    }

    fn answer(candidates: Vec<Candidate>) -> CrashAnswer {
        CrashAnswer {
            contract: CRASH_CONTRACT_VERSION,
            verdict: SessionVerdict::Crashed {
                how: MatchedBy::Supplied,
            },
            headline: LocalText::new(),
            level: Level::Likely,
            cause: Some(Cause::Mod),
            candidates,
            findings: Vec::new(),
            inspections: Vec::new(),
            could_not_check: Vec::new(),
            trust_notes: Vec::new(),
            next: NextStep {
                kind: NextStepKind::ConfirmExperiment,
                keys: Vec::new(),
            },
            repairs: Vec::new(),
            actions: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    fn layer(key: &str, file: &str) -> LayerView {
        LayerView::new(
            format!("mod:{key}"),
            true,
            LayerOwner::Item {
                key: key.to_string(),
                name: key.to_string(),
            },
            vec![file.to_string()],
        )
    }

    fn view() -> InstanceView {
        InstanceView::new(vec![
            layer("one", "mods/one.jar"),
            layer("two", "mods/two.jar"),
        ])
    }

    #[test]
    fn the_ruled_out_suspect_is_dropped_and_the_next_one_is_offered() {
        let answer = answer(vec![item("one", 90), item("two", 40)]);
        let result = next_suspects(Some(&answer), Some(&view()), "item:one");
        assert_eq!(result.ruled_out, vec!["item:one".to_string()]);
        assert_eq!(result.suspects.len(), 1);
        assert_eq!(result.suspects[0].filename, "two.jar");
        match result.suggested_action {
            SuggestedAction::GuidedDisable { next_suspect } => {
                assert_eq!(next_suspect.mod_id, "item:two")
            }
            SuggestedAction::NoSuspects => panic!("expected a next suspect"),
        }
    }

    #[test]
    fn no_answer_means_no_suspects() {
        let result = next_suspects(None, None, "item:one");
        assert!(result.suspects.is_empty());
        assert!(matches!(
            result.suggested_action,
            SuggestedAction::NoSuspects
        ));
        assert_eq!(result.ruled_out, vec!["item:one".to_string()]);
    }

    #[test]
    fn a_suspect_file_is_the_file_name_its_item_provides() {
        assert_eq!(suspect_file(&view(), "item:two").unwrap(), "two.jar");
    }

    #[test]
    fn a_suspect_that_is_not_an_item_or_not_listed_is_an_error() {
        let not_item = suspect_file(&view(), "framework:forge").unwrap_err();
        assert_eq!(not_item.code(), "ERR_CRASH_SUSPECT_NOT_ITEM");
        let missing = suspect_file(&view(), "item:three").unwrap_err();
        assert_eq!(missing.code(), "ERR_CRASH_SUSPECT_NO_FILE");
    }

    #[test]
    fn no_stored_answer_means_no_suspects() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().to_path_buf());
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        assert!(latest_analyzed_answer(&ctx, "nothing").unwrap().is_none());
        let result = report_still_crashing(&ctx, "nothing", "item:one", None).unwrap();
        assert!(result.suspects.is_empty());
    }
}
