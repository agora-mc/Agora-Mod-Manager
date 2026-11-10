//! Diagnose loose crash files: the simplest chain through the crash doctor (MASTER_SPEC §30.4,
//! §30.5, §30.11).
//!
//! A person hands Agora one or more files with no instance and no launch record behind them. This
//! module detects a parser by content, runs it, evaluates every pack's rules, reaches consensus and
//! builds the answer. It holds no game knowledge: every file format is a pack's.
//!
//! The answer says the user supplied the evidence ([`MatchedBy::Supplied`]), and every ref names
//! itself unless a pack's `roles` say what part it plays. Every parser failure is kept and shown as
//! "could not check", naming the plugin; none is ever read as "nothing found".

use super::attribution::LooseResolver;
use super::{
    answer, consensus, evaluate, AnswerInput, ConsensusInput, ConsensusProfile, EvaluateInput,
    Inspection, InspectionRequest, Inspector, ParsedArtifact, PluginIdentity,
};
use crate::crash_packs::{
    artifact_handle, parse_artifact_with_builtin, parser_request, LoadedCrashPack, MemoryArtifact,
    ParserFailureKind, BUILT_IN_PLUGIN_ID,
};
use crate::error::{LauncherError, LauncherResult};
use agora_game_api::crash::{
    Artifact, BuiltInPack, CouldNotCheck, CouldNotCheckReason, CrashAnswer, GameHandle, MatchedBy,
    ParserLimits, Privacy, Retained, SessionVerdict, DETECT_HEAD_BYTES,
};
use agora_plugin_api::host::{ScriptHost, ScriptReads};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The artifact id a parser is given. A parser sees a name and an id, never a path.
const ARTIFACT_ID: &str = "artifact";

/// A game id for evidence that has no instance behind it. Nothing filters packs by it: with no
/// instance, Agora cannot say which game the file came from, so every installed pack may try.
const UNKNOWN_GAME: &str = "unknown";

/// One file the user supplied. Core sees bytes and a name, never a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Everything one loose-file analysis needs.
pub struct AnalyzeInput<'a> {
    /// The crash packs that loaded and linted clean.
    pub packs: &'a [LoadedCrashPack],
    pub files: &'a [CrashFile],
    /// The game version the user says this came from, when they know it.
    pub runtime_version: Option<&'a str>,
    /// The script host a script parser runs in. Generic parsers never use it.
    pub host: &'a dyn ScriptHost,
    /// The packs compiled into Agora, for the parsers their JSON names. Each must also be in
    /// `packs`, loaded by `load_built_in_packs`.
    pub built_in: &'a [BuiltInPack],
}

/// The pack and parser that read one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReader {
    pub plugin_id: String,
    pub plugin_name: String,
    pub parser_id: String,
}

/// A parser that could not check a file, with the reason. The plugin is named because that is what
/// makes it actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileParserFailure {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub parser_id: String,
    pub kind: ParserFailureKind,
    pub message: String,
}

/// What became of one file. `read_by` is absent when nothing read it, and `reason` then says why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzedFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_by: Option<FileReader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<FileParserFailure>,
}

/// The answer and the per-file report. The CLI prints both.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeOutput {
    pub answer: CrashAnswer,
    pub files: Vec<AnalyzedFile>,
    /// What each parser found, for someone writing or checking rules. The answer never needs it.
    pub parsed: Vec<ParsedArtifact>,
}

impl AnalyzeOutput {
    /// Whether at least one file was read by a parser.
    pub fn read_any(&self) -> bool {
        self.files.iter().any(|file| file.read_by.is_some())
    }
}

/// Inspectors need a file a rule points at. Loose files have none, so nothing can be inspected.
struct NoInspector;

impl Inspector for NoInspector {
    fn inspect(&self, _request: &InspectionRequest<'_>) -> Inspection {
        Inspection::Missing
    }
}

/// Run one file through every pack's parsers, then evaluate every pack's rules over what was read.
pub fn analyze(input: &AnalyzeInput<'_>) -> LauncherResult<AnalyzeOutput> {
    if input.files.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_CRASH_ANALYZE_NO_FILES".to_string(),
            message: "No crash files were given to analyze.".to_string(),
        });
    }

    let mut packs: Vec<&LoadedCrashPack> = input.packs.iter().collect();
    packs.sort_by(|a, b| {
        a.plugin_id
            .cmp(&b.plugin_id)
            .then_with(|| a.file.cmp(&b.file))
    });

    let limits = ParserLimits::DEFAULT;
    let mut parsed: Vec<ParsedArtifact> = Vec::new();
    let mut reports: Vec<AnalyzedFile> = Vec::with_capacity(input.files.len());
    let mut failures: Vec<(String, FileParserFailure)> = Vec::new();

    for file in input.files {
        let mut report = AnalyzedFile {
            name: file.name.clone(),
            read_by: None,
            reason: None,
            failures: Vec::new(),
        };

        if file.bytes.is_empty() {
            report.reason = Some("the file is empty; there is nothing to read".to_string());
            reports.push(report);
            continue;
        }
        let size = file.bytes.len() as u64;
        if size > limits.max_total_read_bytes {
            report.reason = Some(format!(
                "the file is {size} bytes; one analysis reads at most {} bytes",
                limits.max_total_read_bytes
            ));
            reports.push(report);
            continue;
        }

        let mut reads = MemoryArtifact::new(limits);
        reads.insert(ARTIFACT_ID, file.bytes.clone());
        let reads: Arc<dyn ScriptReads> = Arc::new(reads);
        let handle = artifact_handle(ARTIFACT_ID, file.name.clone(), size, Vec::new());
        let head = &file.bytes[..file.bytes.len().min(DETECT_HEAD_BYTES)];
        let request = parser_request(
            "",
            handle,
            GameHandle {
                id: UNKNOWN_GAME.to_string(),
                runtime_version: input.runtime_version.map(str::to_string),
            },
            limits,
        );

        for pack in &packs {
            let built_in = if pack.plugin_id == BUILT_IN_PLUGIN_ID {
                input
                    .built_in
                    .iter()
                    .find(|built_in| built_in.file == pack.file)
            } else {
                None
            };
            let outcome = parse_artifact_with_builtin(
                input.host,
                &pack.plugin_id,
                &pack.pack,
                &request,
                reads.clone(),
                None,
                head,
                built_in,
            );
            for attempt in &outcome.attempts {
                match &attempt.outcome {
                    crate::crash_packs::ParserDispatch::Parsed(output) => {
                        // The first parser that read it is the one the answer speaks from; a
                        // second reader of the same file would double its facts.
                        if report.read_by.is_none() {
                            report.read_by = Some(FileReader {
                                plugin_id: pack.plugin_id.clone(),
                                plugin_name: plugin_name(pack),
                                parser_id: attempt.parser_id.clone(),
                            });
                            parsed.push(ParsedArtifact {
                                plugin: identity(pack),
                                parser_id: attempt.parser_id.clone(),
                                artifact: file.name.clone(),
                                source: None,
                                output: output.clone(),
                            });
                        }
                    }
                    crate::crash_packs::ParserDispatch::WrongFormat(_) => {}
                    crate::crash_packs::ParserDispatch::Failed(failure) => {
                        let reported = FileParserFailure {
                            plugin_id: pack.plugin_id.clone(),
                            plugin_name: plugin_name(pack),
                            plugin_version: pack.plugin_version.clone(),
                            parser_id: attempt.parser_id.clone(),
                            kind: failure.kind,
                            message: failure.message.clone(),
                        };
                        report.failures.push(reported.clone());
                        failures.push((file.name.clone(), reported));
                    }
                }
            }
        }

        if report.read_by.is_none() {
            report.reason = Some(if report.failures.is_empty() {
                "no installed crash pack recognised this file".to_string()
            } else {
                "no parser could read this file; see the failures".to_string()
            });
        }
        reports.push(report);
    }

    // A loose file names no game, so it stands for the games of the packs that read it: another
    // game's rules must not accuse a module in this one. Packs for every game always take part.
    let read_games: std::collections::BTreeSet<&str> = reports
        .iter()
        .filter_map(|report| report.read_by.as_ref())
        .flat_map(|reader| {
            packs
                .iter()
                .filter(move |pack| pack.plugin_id == reader.plugin_id)
                .flat_map(|pack| pack.pack.games.iter().map(String::as_str))
        })
        .collect();
    let packs: Vec<&LoadedCrashPack> = packs
        .into_iter()
        .filter(|pack| {
            read_games.is_empty()
                || pack.pack.games.is_empty()
                || pack
                    .pack
                    .games
                    .iter()
                    .any(|game| read_games.contains(game.as_str()))
        })
        .collect();

    let pack_inputs: Vec<super::PackInput> = packs
        .iter()
        .map(|pack| super::PackInput {
            plugin: identity(pack),
            pack: pack.pack.clone(),
        })
        .collect();
    let engine_facts = Vec::new();
    let resolver = LooseResolver::from_packs(packs.iter().map(|pack| &pack.pack), &parsed);
    let inspector = NoInspector;
    let evaluated = evaluate(&EvaluateInput {
        packs: &pack_inputs,
        parsed: &parsed,
        engine_facts: &engine_facts,
        runtime_version: input.runtime_version,
        resolver: &resolver,
        inspector: &inspector,
    });
    let level_caps: Vec<_> = evaluated.level_cap.into_iter().collect();
    let scored = consensus(
        &ConsensusInput {
            signals: &evaluated.signals,
            roles: &evaluated.roles,
            level_caps: &level_caps,
        },
        &ConsensusProfile::default(),
    );
    let mut crash_answer = answer(&AnswerInput {
        verdict: SessionVerdict::Crashed {
            how: MatchedBy::Supplied,
        },
        evaluated: &evaluated,
        consensus: &scored,
    });

    for (file, failure) in &failures {
        crash_answer.could_not_check.push(CouldNotCheck {
            reason: CouldNotCheckReason::CodeFailed,
            subject: failure.parser_id.clone(),
            detail: Some(format!("{file}: {}", failure.message)),
            source: Some(
                PluginIdentity {
                    id: failure.plugin_id.clone(),
                    name: failure.plugin_name.clone(),
                    version: failure.plugin_version.clone(),
                }
                .parser_source(&failure.parser_id),
            ),
        });
    }
    crash_answer
        .could_not_check
        .sort_by_key(could_not_check_key);

    let mut artifacts: Vec<Artifact> = input
        .files
        .iter()
        .map(|file| Artifact {
            id: file.name.clone(),
            name: file.name.clone(),
            size: file.bytes.len() as u64,
            modified_at: None,
            source: None,
            bound_by: Some(MatchedBy::Supplied),
            privacy: Privacy::Normal,
            retained: if file.bytes.len() as u64 > ParserLimits::DEFAULT.max_total_read_bytes {
                Retained::NotRetained {
                    why: "the file is larger than one analysis reads".to_string(),
                }
            } else {
                Retained::Whole
            },
            attributed_to: None,
            children: Vec::new(),
        })
        .collect();
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    crash_answer.artifacts = artifacts;

    Ok(AnalyzeOutput {
        answer: crash_answer,
        files: reports,
        parsed,
    })
}

fn identity(pack: &LoadedCrashPack) -> PluginIdentity {
    PluginIdentity {
        id: pack.plugin_id.clone(),
        name: plugin_name(pack),
        version: pack.plugin_version.clone(),
    }
}

fn plugin_name(pack: &LoadedCrashPack) -> String {
    plugin_name_for(&pack.plugin_id, &pack.plugin_name)
}

/// The display name, falling back to the id for a report that predates the name.
fn plugin_name_for(id: &str, name: &str) -> String {
    if name.trim().is_empty() {
        id.to_string()
    } else {
        name.to_string()
    }
}

fn could_not_check_key(check: &CouldNotCheck) -> String {
    let source = check
        .source
        .as_ref()
        .map(|source| format!("{}\0{}", source.plugin_id, source.plugin_name))
        .unwrap_or_default();
    format!(
        "{:?}\0{}\0{}\0{}",
        check.reason,
        check.subject,
        check.detail.as_deref().unwrap_or(""),
        source
    )
}
