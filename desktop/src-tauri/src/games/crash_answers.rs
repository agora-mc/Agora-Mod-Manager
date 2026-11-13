//! The crash answer screen's commands: the stored answer for a session, the instance's earlier
//! answers, and the "Check a crash log" path for text or a picked file.
//!
//! Each command is a thin adapter over `agora_core::crash_live` and
//! `agora_core::crash_loose`. Core decides what an answer is; this file only turns it into the
//! shape the page reads and loads the packs a check runs with. Core holds no game knowledge, so
//! the built-in packs a compiled game supplies are passed in from the game package here.

use std::path::PathBuf;
use std::sync::Arc;

use agora_core::crash_engine::evidence::{EvidenceStore, RetentionLimits, EVIDENCE_STORE_DIR};
use agora_core::crash_engine::{AnalyzedFile, CrashFile};
use agora_core::crash_live::{answer_for_session, answers_for_instance, StoredAnswer};
use agora_core::crash_loose::{analyze_supplied, SuppliedAnalysis, SuppliedAnswer};
use agora_core::session_ledger::{self, ManifestChange};
use agora_game_api::crash::{LocalText, SessionVerdict};
use agora_plugin_api::host::ScriptHost;
use serde::Serialize;

use super::error::{GamesError, CORE_FAILED, TASK_FAILED};

/// No stored answer has this session id.
pub const CRASH_ANSWER_NOT_FOUND: &str = "crash_answer_not_found";
/// The picked file could not be read.
pub const CRASH_FILE_READ: &str = "crash_file_read";
/// The stored evidence file is no longer in Agora's store.
pub const CRASH_FILE_MISSING: &str = "crash_file_missing";
/// The stored evidence file is too large to hand to the page to copy.
pub const CRASH_FILE_UNREADABLE: &str = "crash_file_unreadable";

/// The most of one kept file Agora will hand to the page to copy. A crash log this large is not a
/// log Agora can usefully paste anywhere.
const MAX_COPY_BYTES: u64 = 4 * 1024 * 1024;

/// Whether an answer ran, or says why it could not. A missing log is an answer, never silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    Analyzed,
    NothingToRead,
    CouldNotAnalyze,
}

/// One line for the card and the earlier-crashes list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashAnswerSummary {
    pub session_id: String,
    pub instance_id: String,
    pub verdict: SessionVerdict,
    pub ended_at: Option<String>,
    pub created_at: String,
    pub status: AnswerStatus,
    /// The answer's own sentence, for an answer that ran.
    pub headline: Option<String>,
    pub level: Option<agora_game_api::crash::Level>,
    pub cause: Option<agora_game_api::crash::Cause>,
    /// Why nothing could be read, for the two answers that did not run.
    pub reason: Option<String>,
}

/// Everything the instance page needs: the answers, and whether its newest session was normal.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashAnswerList {
    /// Newest session first.
    pub answers: Vec<CrashAnswerSummary>,
    /// The newest recorded session's verdict, even when it ended normally. The card hides after a
    /// normal session, so the page cannot infer this from the answers alone.
    pub latest_verdict: Option<SessionVerdict>,
    /// How many crashes' evidence Agora keeps, for the Files section's sentence.
    pub retained_crashes: usize,
}

/// One answer, ready to render.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashAnswerDetail {
    pub session_id: String,
    pub instance_id: String,
    pub verdict: SessionVerdict,
    pub ended_at: Option<String>,
    pub created_at: String,
    pub status: AnswerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<agora_game_api::crash::CrashAnswer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<AnalyzedFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<ManifestChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The answer for a file the user supplied. It has no session, so no verdict or date.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuppliedCrashAnswer {
    pub status: AnswerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<agora_game_api::crash::CrashAnswer>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<AnalyzedFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The stored answer for one session.
#[tauri::command]
pub async fn get_crash_answer(
    app: tauri::AppHandle,
    session_id: String,
) -> Result<CrashAnswerDetail, GamesError> {
    let ctx = crate::core_context(&app)?;
    blocking(move || {
        let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())
            .map_err(core_failed)?;
        let stored = answer_for_session(&conn, &session_id)
            .map_err(core_failed)?
            .ok_or_else(|| {
                GamesError::new(
                    CRASH_ANSWER_NOT_FOUND,
                    "Agora has no stored answer for that crash.",
                )
            })?;
        let record = session_ledger::get(&conn, &session_id).map_err(core_failed)?;
        Ok(detail(stored, record))
    })
    .await
}

/// An instance's stored answers, newest session first.
#[tauri::command]
pub async fn list_crash_answers(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<CrashAnswerList, GamesError> {
    let ctx = crate::core_context(&app)?;
    blocking(move || {
        let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db())
            .map_err(core_failed)?;
        let answers = answers_for_instance(&conn, &instance_id)
            .map_err(core_failed)?
            .into_iter()
            .map(|stored| {
                let record = session_ledger::get(&conn, &stored.session_id)
                    .ok()
                    .flatten();
                summary(stored, record)
            })
            .collect();
        let latest_verdict = session_ledger::list(&conn, &instance_id, 1)
            .map_err(core_failed)?
            .first()
            .and_then(|record| record.verdict);
        Ok(CrashAnswerList {
            answers,
            latest_verdict,
            retained_crashes: RetentionLimits::default().max_sessions,
        })
    })
    .await
}

/// Check a crash log the user pasted. With an instance the game is known and refs attribute to it.
#[tauri::command]
pub async fn analyze_crash_text(
    app: tauri::AppHandle,
    instance_id: Option<String>,
    text: String,
) -> Result<SuppliedCrashAnswer, GamesError> {
    let ctx = crate::core_context(&app)?;
    blocking(move || {
        // Pasted text may be a fragment: only pasted text may use a pack's snippet parsers.
        let files = vec![CrashFile {
            name: "pasted-crash-log.txt".to_string(),
            bytes: text.into_bytes(),
            origin: agora_core::crash_engine::CrashFileOrigin::Pasted,
        }];
        supplied(&ctx, instance_id.as_deref(), files)
    })
    .await
}

/// Check a crash log the user picked. Agora reads the file in the adapter; core sees bytes and a
/// name, never a path.
#[tauri::command]
pub async fn analyze_crash_file(
    app: tauri::AppHandle,
    instance_id: Option<String>,
    path: String,
) -> Result<SuppliedCrashAnswer, GamesError> {
    let ctx = crate::core_context(&app)?;
    blocking(move || {
        let path = PathBuf::from(path);
        let bytes = std::fs::read(&path).map_err(|error| {
            GamesError::new(
                CRASH_FILE_READ,
                format!("The crash log could not be read: {error}"),
            )
        })?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("crash log")
            .to_string();
        supplied(
            &ctx,
            instance_id.as_deref(),
            vec![CrashFile::new(name, bytes)],
        )
    })
    .await
}

/// The text of one file Agora kept, for "copy the crash log". The id comes from the answer; the
/// path is resolved in Agora's own store and never returned to the page.
#[tauri::command]
pub async fn read_crash_artifact(
    app: tauri::AppHandle,
    session_id: String,
    artifact_id: String,
) -> Result<String, GamesError> {
    let ctx = crate::core_context(&app)?;
    let store = EvidenceStore::new(ctx.paths.root().join(EVIDENCE_STORE_DIR));
    let path = store
        .artifact_path(&session_id, &artifact_id)
        .ok_or_else(|| {
            GamesError::new(
                CRASH_FILE_MISSING,
                "That file is no longer in the crash evidence Agora kept.",
            )
        })?;
    let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    if size > MAX_COPY_BYTES {
        return Err(GamesError::new(
            CRASH_FILE_UNREADABLE,
            "That crash log is too large to copy.",
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        GamesError::new(
            CRASH_FILE_UNREADABLE,
            format!("The crash log could not be read: {error}"),
        )
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Show one file Agora kept in the user's file manager. The id comes from the answer; the path is
/// resolved in Agora's own store and never returned to the page.
#[tauri::command]
pub async fn reveal_crash_artifact(
    app: tauri::AppHandle,
    session_id: String,
    artifact_id: String,
) -> Result<(), GamesError> {
    let ctx = crate::core_context(&app)?;
    let store = EvidenceStore::new(ctx.paths.root().join(EVIDENCE_STORE_DIR));
    let path = store
        .artifact_path(&session_id, &artifact_id)
        .ok_or_else(|| {
            GamesError::new(
                CRASH_FILE_MISSING,
                "That file is no longer in the crash evidence Agora kept.",
            )
        })?;
    crate::commands::reveal_in_explorer(&path)
        .map_err(|message| GamesError::new(CRASH_FILE_MISSING, message))
}

fn supplied(
    ctx: &agora_core::ctx::Ctx,
    instance_id: Option<&str>,
    files: Vec<CrashFile>,
) -> Result<SuppliedCrashAnswer, GamesError> {
    let host: Arc<dyn ScriptHost> = Arc::new(agora_plugin_host::QuickJsHost::new());
    let installed = agora_core::crash_live::load_installed_packs(ctx, host.clone())
        .map_err(GamesError::from)?;
    let built_in = agora_game_minecraft::crash::built_in_packs();
    let built_in_report = agora_core::crash_packs::load_built_in_packs(&built_in);
    let mut packs = installed.packs;
    packs.extend(built_in_report.packs);
    let answer = analyze_supplied(
        ctx,
        SuppliedAnalysis {
            packs: &packs,
            built_in: &built_in,
            host: host.as_ref(),
            files: &files,
            instance_id,
            runtime_version: None,
        },
    )
    .map_err(GamesError::from)?;
    Ok(match answer {
        SuppliedAnswer::Analyzed { answer, files } => SuppliedCrashAnswer {
            status: AnswerStatus::Analyzed,
            answer: Some(*answer),
            files,
            reason: None,
        },
        SuppliedAnswer::NothingToRead { why } => SuppliedCrashAnswer {
            status: AnswerStatus::NothingToRead,
            answer: None,
            files: Vec::new(),
            reason: Some(why),
        },
    })
}

fn summary(
    stored: agora_core::crash_live::SessionAnswer,
    record: Option<session_ledger::SessionRecord>,
) -> CrashAnswerSummary {
    let verdict =
        record
            .as_ref()
            .and_then(|record| record.verdict)
            .unwrap_or(match &stored.answer {
                StoredAnswer::Analyzed { answer, .. } => answer.verdict,
                _ => SessionVerdict::ClosedUnknown,
            });
    let (status, headline, level, cause, reason) = match &stored.answer {
        StoredAnswer::Analyzed { answer, .. } => (
            AnswerStatus::Analyzed,
            Some(english(&answer.headline)),
            Some(answer.level),
            answer.cause,
            None,
        ),
        StoredAnswer::NothingToRead { why } => (
            AnswerStatus::NothingToRead,
            None,
            None,
            None,
            Some(why.clone()),
        ),
        StoredAnswer::CouldNotAnalyze { why } => (
            AnswerStatus::CouldNotAnalyze,
            None,
            None,
            None,
            Some(why.clone()),
        ),
    };
    CrashAnswerSummary {
        session_id: stored.session_id,
        instance_id: stored.instance_id,
        verdict,
        ended_at: record.and_then(|record| record.ended_at),
        created_at: stored.created_at,
        status,
        headline,
        level,
        cause,
        reason,
    }
}

fn detail(
    stored: agora_core::crash_live::SessionAnswer,
    record: Option<session_ledger::SessionRecord>,
) -> CrashAnswerDetail {
    let verdict =
        record
            .as_ref()
            .and_then(|record| record.verdict)
            .unwrap_or(match &stored.answer {
                StoredAnswer::Analyzed { answer, .. } => answer.verdict,
                _ => SessionVerdict::ClosedUnknown,
            });
    let ended_at = record.and_then(|record| record.ended_at);
    match stored.answer {
        StoredAnswer::Analyzed {
            answer,
            files,
            changes,
        } => CrashAnswerDetail {
            session_id: stored.session_id,
            instance_id: stored.instance_id,
            verdict,
            ended_at,
            created_at: stored.created_at,
            status: AnswerStatus::Analyzed,
            answer: Some(*answer),
            files,
            changes,
            reason: None,
        },
        StoredAnswer::NothingToRead { why } => CrashAnswerDetail {
            session_id: stored.session_id,
            instance_id: stored.instance_id,
            verdict,
            ended_at,
            created_at: stored.created_at,
            status: AnswerStatus::NothingToRead,
            answer: None,
            files: Vec::new(),
            changes: Vec::new(),
            reason: Some(why),
        },
        StoredAnswer::CouldNotAnalyze { why } => CrashAnswerDetail {
            session_id: stored.session_id,
            instance_id: stored.instance_id,
            verdict,
            ended_at,
            created_at: stored.created_at,
            status: AnswerStatus::CouldNotAnalyze,
            answer: None,
            files: Vec::new(),
            changes: Vec::new(),
            reason: Some(why),
        },
    }
}

fn english(text: &LocalText) -> String {
    text.get("en").cloned().unwrap_or_default()
}

fn core_failed(error: anyhow::Error) -> GamesError {
    GamesError::new(
        CORE_FAILED,
        format!("The crash answer could not be read: {error}"),
    )
}

async fn blocking<T, F>(work: F) -> Result<T, GamesError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, GamesError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| GamesError::new(TASK_FAILED, "The crash work did not finish."))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::crash_live::SessionAnswer;
    use agora_core::session_ledger::{ChangeKind, ManifestChange};
    use agora_game_api::crash::{
        CrashAnswer, Level, LocalText, MatchedBy, NextStep, NextStepKind, SessionVerdict,
        CRASH_CONTRACT_VERSION,
    };

    fn stored() -> SessionAnswer {
        let mut headline = LocalText::new();
        headline.insert(
            "en".to_string(),
            "The game crashed, and Example Mod is likely the cause.".to_string(),
        );
        SessionAnswer {
            session_id: "session-1".to_string(),
            instance_id: "inst".to_string(),
            created_at: "2026-11-12T21:15:00Z".to_string(),
            answer: StoredAnswer::Analyzed {
                answer: Box::new(CrashAnswer {
                    contract: CRASH_CONTRACT_VERSION,
                    verdict: SessionVerdict::Crashed {
                        how: MatchedBy::Pid,
                    },
                    headline,
                    level: Level::Likely,
                    cause: None,
                    candidates: Vec::new(),
                    findings: Vec::new(),
                    inspections: Vec::new(),
                    could_not_check: Vec::new(),
                    trust_notes: Vec::new(),
                    next: NextStep {
                        kind: NextStepKind::Bisect,
                        keys: Vec::new(),
                    },
                    repairs: Vec::new(),
                    artifacts: Vec::new(),
                }),
                files: Vec::new(),
                changes: vec![ManifestChange {
                    kind: ChangeKind::Added,
                    key: Some("acme.example".to_string()),
                    from_version: None,
                    to_version: Some("2.1".to_string()),
                }],
            },
        }
    }

    fn record() -> session_ledger::SessionRecord {
        session_ledger::SessionRecord {
            session_id: "session-1".to_string(),
            instance_id: "inst".to_string(),
            started_at: "2026-11-12T21:10:00Z".to_string(),
            ended_at: Some("2026-11-12T21:14:00Z".to_string()),
            exit: None,
            stop_requested: false,
            manifest: None,
            verdict: Some(SessionVerdict::Crashed {
                how: MatchedBy::Pid,
            }),
        }
    }

    #[test]
    fn a_summary_carries_the_verdict_date_and_headline() {
        let summary = summary(stored(), Some(record()));
        assert_eq!(
            summary.verdict,
            SessionVerdict::Crashed {
                how: MatchedBy::Pid
            }
        );
        assert_eq!(summary.ended_at.as_deref(), Some("2026-11-12T21:14:00Z"));
        assert_eq!(
            summary.headline.as_deref(),
            Some("The game crashed, and Example Mod is likely the cause.")
        );
        assert_eq!(summary.status, AnswerStatus::Analyzed);
    }

    #[test]
    fn an_answer_that_did_not_run_is_summarized_with_its_reason() {
        let mut answer = stored();
        answer.answer = StoredAnswer::NothingToRead {
            why: "No crash log or other evidence for this session could be read.".to_string(),
        };
        let summary = summary(answer, Some(record()));
        assert_eq!(summary.status, AnswerStatus::NothingToRead);
        assert!(summary.headline.is_none());
        assert!(summary.reason.as_deref().unwrap().contains("No crash log"));
    }

    #[test]
    fn a_detail_keeps_the_change_summary() {
        let detail = detail(stored(), Some(record()));
        assert_eq!(detail.changes.len(), 1);
        assert_eq!(detail.changes[0].key.as_deref(), Some("acme.example"));
        assert_eq!(detail.ended_at.as_deref(), Some("2026-11-12T21:14:00Z"));
    }
}
