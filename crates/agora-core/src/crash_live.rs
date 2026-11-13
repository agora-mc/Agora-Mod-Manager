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

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::SystemTime;

use agora_game_api::crash::{
    Artifact, BuiltInPack, CrashAnswer, Fact, RefMap, RoleRule, SessionVerdict,
};
use agora_game_api::GamePath;
use agora_plugin_api::host::ScriptHost;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::crash_engine::attribution::Resolver;
use crate::crash_engine::evidence::{
    collect_sources, ArtifactFacts, CollectionLimits, CopiedSession, CopyLimits, CrashSession,
    EvidenceStore, NoArtifactFacts, RetentionLimits, SessionWindow, SourceLocation, SourceRoots,
    SourceSpec, EVIDENCE_STORE_DIR,
};
use crate::crash_engine::{
    analyze_with_history, history::RecordedCrash, AnalyzeInput, AnalyzedFile, CrashFile,
    InstanceAnalysis, InstanceInspector,
};
use crate::crash_packs::{load_built_in_packs, LoadedCrashPack};
use crate::ctx::Ctx;
use crate::session_ledger::{self, ExitObservation, ManifestChange, SessionRecord};

/// The answer stored for one session. A failed analysis is an answer too: it says what went wrong
/// instead of being absent.
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
    },
    /// Nothing was readable for this session. That is not a clean bill of health.
    NothingToRead { why: String },
    /// The analysis itself failed or panicked. The launch path was not touched.
    CouldNotAnalyze { why: String },
}

/// One session's stored answer, with the ids needed to show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAnswer {
    pub session_id: String,
    pub instance_id: String,
    pub created_at: String,
    pub answer: StoredAnswer,
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
    row.map(decode_answer).transpose()
}

/// Every stored answer for an instance, newest session first. The newest is the card the overview
/// shows; the rest are the instance's earlier crashes. A session with no row in `launch_history`
/// (only possible for an answer written by hand) is ordered by when it was stored.
pub fn answers_for_instance(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Vec<SessionAnswer>> {
    let mut stmt = conn.prepare(
        "SELECT a.session_id, a.instance_id, a.created_at, a.answer_json
           FROM session_answers a
           LEFT JOIN launch_history h ON h.session_id = a.session_id
          WHERE a.instance_id = ?1
          ORDER BY COALESCE(h.ended_at, h.started_at, a.created_at) DESC, a.session_id DESC",
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
    rows.into_iter().map(decode_answer).collect()
}

/// The newest stored answer for an instance.
pub fn latest_answer(
    conn: &Connection,
    instance_id: &str,
) -> anyhow::Result<Option<SessionAnswer>> {
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT session_id, instance_id, created_at, answer_json
               FROM session_answers WHERE instance_id = ?1
               ORDER BY created_at DESC, session_id DESC LIMIT 1",
            [instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(decode_answer).transpose()
}

fn decode_answer(
    (session_id, instance_id, created_at, json): (String, String, String, String),
) -> anyhow::Result<SessionAnswer> {
    Ok(SessionAnswer {
        session_id,
        instance_id,
        created_at,
        answer: serde_json::from_str(&json)?,
    })
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
    let report = service.crash_packs();
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
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    let record = session_ledger::get(&conn, session_id).ok().flatten()?;
    if !diagnoses(record.verdict) || record.ended_at.is_none() {
        return None;
    }
    let stored = std::panic::catch_unwind(AssertUnwindSafe(|| {
        diagnose_record(ctx, &conn, &record, &live)
    }))
    .unwrap_or_else(|_| Err("the crash analysis panicked".to_string()));
    let answer = match stored {
        Ok(answer) => answer,
        Err(why) => StoredAnswer::CouldNotAnalyze { why },
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

/// Store "could not analyze" for a session whose packs or host could not even be prepared. This is
/// still an answer, never a silent absence. Returns `None` for a session Agora does not diagnose.
pub fn record_could_not_analyze(ctx: &Ctx, session_id: &str, why: String) -> Option<SessionAnswer> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    let record = session_ledger::get(&conn, session_id).ok().flatten()?;
    if !diagnoses(record.verdict) || record.ended_at.is_none() {
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

fn diagnose_record(
    ctx: &Ctx,
    conn: &Connection,
    record: &SessionRecord,
    live: &LivePacks<'_>,
) -> Result<StoredAnswer, String> {
    let resolved = crate::crash_engine::instance_view::resolve_crash_instance(
        ctx,
        &record.instance_id,
        live.instances,
    )
    .map_err(|error| format!("the instance could not be read: {error}"))?;
    let game_id = resolved.game_id.clone();
    let runtime_version = resolved.runtime_version.clone();
    let could_not_check = resolved.could_not_check.clone();

    let built_in_report = load_built_in_packs(live.built_in);
    let mut packs: Vec<LoadedCrashPack> = live.packs.to_vec();
    packs.extend(built_in_report.packs);
    let game_packs: Vec<&LoadedCrashPack> = packs
        .iter()
        .filter(|pack| {
            pack.pack.games.is_empty() || pack.pack.games.iter().any(|game| game == &game_id)
        })
        .collect();
    if game_packs.is_empty() {
        return Ok(StoredAnswer::NothingToRead {
            why: format!(
                "No crash pack applies to the game '{game_id}', so there was nothing to read the \
                 session with."
            ),
        });
    }

    let window = record_window(record)?;
    let pid = match &record.exit {
        Some(ExitObservation::Code { pid, .. }) => Some(*pid),
        _ => None,
    };
    let instance_dir = ctx
        .paths
        .instance_dir(&record.instance_id)
        .map_err(|error| format!("the instance folder could not be resolved: {error}"))?;
    let roots = InstanceRoots { instance_dir };

    let session = CrashSession {
        session_id: record.session_id.clone(),
        pid,
        working_dir: None,
        window,
    };
    let specs: Vec<SourceSpec<'_>> = game_packs
        .iter()
        .flat_map(|pack| {
            pack.pack.sources.iter().map(|source| SourceSpec {
                plugin_id: &pack.plugin_id,
                source,
            })
        })
        .collect();
    let facts = NoArtifactFacts;
    let collection = collect_sources(
        &specs,
        &roots,
        &facts as &dyn ArtifactFacts,
        &session,
        CollectionLimits::default(),
    );

    let store_root = ctx.paths.root().join(EVIDENCE_STORE_DIR);
    let store = EvidenceStore::new(store_root);
    let copied = store.copy(&record.session_id, &collection, CopyLimits::default());
    // Retention is best effort and never touches the session being read.
    let _ = store.retention(
        std::slice::from_ref(&record.session_id),
        RetentionLimits::default(),
    );
    let copied = copied.map_err(|error| format!("the evidence could not be copied: {error}"))?;

    let files = readable_files(&copied);
    if files.is_empty() {
        return Ok(StoredAnswer::NothingToRead {
            why: nothing_to_read(&collection.folders, &collection.problems, &copied),
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
    let resolver = Resolver::new(resolved.view, ref_maps, roles);
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
    let engine_facts = session_engine_facts(record, &changes);

    // The same crash's own files may sit in a source folder, so leave them out of the history
    // comparison by their content hash.
    let exclude_hashes: Vec<String> = files
        .iter()
        .map(|file| crate::crash_history::content_hash(&file.bytes))
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
        },
        limits,
        true,
    ) {
        Ok(prepared) => (Some(prepared), None),
        // A history failure must not erase this crash's diagnosis. It is stated as something
        // Agora could not check, never as "no earlier crashes".
        Err(why) => (None, Some(why)),
    };
    let history_input = prepared
        .as_ref()
        .map(crate::crash_history::PreparedHistory::input);

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

    let at_unix_ms = record_window(record)
        .ok()
        .map(|window| crate::crash_engine::evidence::unix_ms(window.started));
    let recorded = RecordedCrash {
        record: output.record.clone(),
        bucket: crate::crash_engine::history::parsed_bucket(&output.parsed),
        at_unix_ms,
    };

    Ok(StoredAnswer::Analyzed {
        answer: Box::new(output.answer),
        files: output.files,
        changes,
        record: Some(recorded),
    })
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

fn readable_files(copied: &CopiedSession) -> Vec<CrashFile> {
    let mut names: BTreeMap<&str, &str> = BTreeMap::new();
    fn walk<'a>(artifacts: &'a [Artifact], names: &mut BTreeMap<&'a str, &'a str>) {
        for artifact in artifacts {
            names.insert(artifact.id.as_str(), artifact.name.as_str());
            walk(&artifact.children, names);
        }
    }
    walk(&copied.artifacts, &mut names);

    let mut files = Vec::new();
    for (id, path) in &copied.objects {
        let Some(name) = names.get(id.as_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        files.push(CrashFile {
            name: (*name).to_string(),
            bytes,
            // Agora found it: a whole file, never read by a snippet parser.
            origin: crate::crash_engine::CrashFileOrigin::Collected,
        });
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

fn session_engine_facts(record: &SessionRecord, changes: &[ManifestChange]) -> Vec<Fact> {
    let verdict = match record.verdict {
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

/// Resolve the roots a live game's crash sources name. Core resolves the instance and user-data
/// folders; a root it cannot resolve is reported by collection, never guessed.
struct InstanceRoots {
    instance_dir: PathBuf,
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
            other => Err(format!(
                "a live session cannot resolve this source root: {other:?}"
            )),
        }
    }
}
