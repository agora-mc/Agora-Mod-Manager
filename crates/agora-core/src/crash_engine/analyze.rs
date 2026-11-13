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
    Inspection, InspectionOutcome, InspectionRequest, Inspector, ParsedArtifact, PluginIdentity,
    RefResolver,
};
use crate::crash_packs::{
    artifact_handle, detect_parsers_with_snippets, parser_request, run_parser_with_builtin,
    LoadedCrashPack, MemoryArtifact, ParserFailureKind, BUILT_IN_PLUGIN_ID,
};
use crate::error::{LauncherError, LauncherResult};
use agora_game_api::crash::{
    Artifact, AttrValue, BuiltInPack, CouldNotCheck, CouldNotCheckReason, CrashAnswer, Fact,
    GameHandle, Level, MatchedBy, ParserDecl, ParserLimits, Privacy, Reads, Retained, Scalar,
    SessionVerdict, TrustNote, DETECT_HEAD_BYTES,
};
use agora_plugin_api::host::{ScriptHost, ScriptReads};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The artifact id a parser is given. A parser sees a name and an id, never a path.
const ARTIFACT_ID: &str = "artifact";

/// A game id for evidence that has no instance behind it. Nothing filters packs by it: with no
/// instance, Agora cannot say which game the file came from, so every installed pack may try.
const UNKNOWN_GAME: &str = "unknown";

/// The wording a snippet answer carries, unchanged wherever it is shown. Agora says why the
/// answer is weaker rather than hiding the cap.
pub const SNIPPET_TRUST_NOTE: &str = "Agora is reading a pasted snippet, not a whole log, so this \
answer is less certain. Paste or pick the whole crash log for the best result.";

/// Where the bytes the analyser is holding came from. Only [`Pasted`](CrashFileOrigin::Pasted)
/// text may fall back to a pack's `snippet` parsers: a file Agora found or the user picked must be
/// recognised as a whole file, or the answer says there was nothing to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CrashFileOrigin {
    /// The user pasted a fragment into Agora.
    Pasted,
    /// The user picked a file.
    #[default]
    File,
    /// Agora's evidence sweep found the file.
    Collected,
}

/// One file the user supplied. Core sees bytes and a name, never a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub origin: CrashFileOrigin,
}

impl CrashFile {
    /// A file the user picked: a whole file, never eligible for a snippet parser.
    pub fn new(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
            origin: CrashFileOrigin::File,
        }
    }

    /// Text the user pasted. Only this origin may use a pack's `snippet` parsers.
    pub fn pasted(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
            origin: CrashFileOrigin::Pasted,
        }
    }

    /// A file Agora's evidence sweep found.
    pub fn collected(name: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            bytes: bytes.into(),
            origin: CrashFileOrigin::Collected,
        }
    }

    /// The same file with another origin.
    pub fn with_origin(mut self, origin: CrashFileOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// What an analysis did not have to build for itself: the instance behind the crash.
#[derive(Clone, Copy)]
pub struct InstanceAnalysis<'a> {
    /// The game the instance runs. Only this game's packs, and those for every game, take part.
    pub game_id: &'a str,
    /// Who owns each ref in this instance's deployed view.
    pub resolver: &'a dyn RefResolver,
    /// The deeper checks this instance's packs can make.
    pub inspector: &'a dyn Inspector,
    /// What the instance itself could not supply, such as installed items whose metadata is
    /// missing. These reach the answer's "could not check", so a gap is never read as a clean
    /// result.
    pub could_not_check: &'a [CouldNotCheck],
}

/// Everything one crash analysis needs.
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
    /// The instance the crash came from, when the caller knows it. `None` is a loose file: names
    /// stand for themselves and nothing can be inspected.
    pub instance: Option<InstanceAnalysis<'a>>,
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
    /// Primary readers that would also have claimed this file. They were not run: one primary
    /// speaks for a file, so its facts are never counted twice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_recognised_by: Vec<FileReader>,
    /// `extra` readers that ran alongside the primary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_by_extra: Vec<FileReader>,
    /// A `snippet` parser read this pasted text, so the answer carries the snippet trust note and
    /// its cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub snippet: bool,
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
    /// What this crash contributes to a cross-crash summary. One incident even when several files
    /// were handed to one call.
    pub record: super::history::CrashRecord,
}

impl AnalyzeOutput {
    /// Whether at least one parser read a file.
    pub fn read_any(&self) -> bool {
        !self.parsed.is_empty()
    }
}

/// Inspectors need a file a rule points at. Loose files have none, so nothing can be inspected.
struct NoInspector;

impl Inspector for NoInspector {
    fn inspect(&self, _request: &InspectionRequest<'_>) -> InspectionOutcome {
        Inspection::Missing.into()
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
    analyze_with_facts(input, &[], &[])
}

/// [`analyze`] with facts only a live session can supply: `core.session` and `core.change`.
/// `changes` are the same differences those facts describe, used for the boost signals and for
/// the finding shown when nothing else is named. A loose file passes neither.
pub fn analyze_with_facts(
    input: &AnalyzeInput<'_>,
    extra_facts: &[Fact],
    changes: &[crate::session_ledger::ManifestChange],
) -> LauncherResult<AnalyzeOutput> {
    let mut packs: Vec<&LoadedCrashPack> = input.packs.iter().collect();
    packs.sort_by(|a, b| {
        pack_rank(a)
            .cmp(&pack_rank(b))
            .then_with(|| a.plugin_id.cmp(&b.plugin_id))
            .then_with(|| a.file.cmp(&b.file))
    });

    let limits = ParserLimits::DEFAULT;
    let game_id = input
        .instance
        .as_ref()
        .map(|instance| instance.game_id)
        .unwrap_or(UNKNOWN_GAME);
    let mut parsed: Vec<ParsedArtifact> = Vec::new();
    let mut reports: Vec<AnalyzedFile> = Vec::with_capacity(input.files.len());
    let mut failures: Vec<(String, FileParserFailure)> = Vec::new();
    // The snippet parser that stood in for a whole-file reader, if one did. Its source names the
    // trust note, and its presence caps the answer at "possible".
    let mut snippet_source: Option<agora_game_api::crash::SourceAttribution> = None;

    for file in input.files {
        let mut report = AnalyzedFile {
            name: file.name.clone(),
            read_by: None,
            also_recognised_by: Vec::new(),
            read_by_extra: Vec::new(),
            snippet: false,
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
                id: game_id.to_string(),
                runtime_version: input.runtime_version.map(str::to_string),
            },
            limits,
        );

        // Who claims this file? Detection reads only the first bytes. One primary speaks for the
        // file: built-in first, then official, then by plugin id. Every extra runs alongside it.
        struct Claim<'a> {
            pack: &'a LoadedCrashPack,
            parser: &'a ParserDecl,
            built_in: Option<&'a BuiltInPack>,
            reader: FileReader,
        }
        let mut claimed: Vec<Claim<'_>> = Vec::new();
        for pack in &packs {
            let built_in = if pack.plugin_id == BUILT_IN_PLUGIN_ID {
                input
                    .built_in
                    .iter()
                    .find(|built_in| built_in.file == pack.file)
            } else {
                None
            };
            match detect_parsers_with_snippets(&pack.pack, &file.name, head) {
                Ok(parsers) => {
                    for parser in parsers {
                        claimed.push(Claim {
                            pack,
                            parser,
                            built_in,
                            reader: FileReader {
                                plugin_id: pack.plugin_id.clone(),
                                plugin_name: plugin_name(pack),
                                parser_id: parser.id.clone(),
                            },
                        });
                    }
                }
                Err(mut failure) => {
                    failure.plugin_id = pack.plugin_id.clone();
                    let reported = FileParserFailure {
                        plugin_id: pack.plugin_id.clone(),
                        plugin_name: plugin_name(pack),
                        plugin_version: pack.plugin_version.clone(),
                        parser_id: failure.parser_id.clone(),
                        kind: failure.kind,
                        message: failure.message.clone(),
                    };
                    report.failures.push(reported.clone());
                    failures.push((file.name.clone(), reported));
                }
            }
        }

        let primary = claimed
            .iter()
            .position(|claim| claim.parser.reads == Reads::Primary);
        // A primary that detected this text speaks for it, even if it later fails: a fragment
        // fallback must never stand in for a parser that said it could not check the file.
        let primary_claimed = primary.is_some();
        if let Some(index) = primary {
            report.read_by = Some(claimed[index].reader.clone());
        }
        let mut to_run: Vec<(bool, &Claim<'_>)> = Vec::new();
        if let Some(index) = primary {
            to_run.push((true, &claimed[index]));
        }
        for (index, claim) in claimed.iter().enumerate() {
            match claim.parser.reads {
                Reads::Primary if Some(index) != primary => {
                    report.also_recognised_by.push(claim.reader.clone());
                }
                Reads::Extra => to_run.push((false, claim)),
                Reads::Primary | Reads::Snippet => {}
            }
        }

        let mut read_any = false;
        for (is_primary, claim) in to_run {
            let attempt = run_parser_with_builtin(
                input.host,
                &claim.pack.plugin_id,
                claim.parser,
                &request,
                reads.clone(),
                claim.built_in,
            );
            match &attempt.outcome {
                crate::crash_packs::ParserDispatch::Parsed(output) => {
                    read_any = true;
                    parsed.push(ParsedArtifact {
                        plugin: identity(claim.pack),
                        parser_id: attempt.parser_id.clone(),
                        artifact: file.name.clone(),
                        source: None,
                        output: output.clone(),
                    });
                    if !is_primary {
                        report.read_by_extra.push(claim.reader.clone());
                    }
                }
                crate::crash_packs::ParserDispatch::WrongFormat(_) => {
                    if is_primary {
                        report.read_by = None;
                    }
                }
                crate::crash_packs::ParserDispatch::Failed(failure) => {
                    if is_primary {
                        report.read_by = None;
                    }
                    let reported = FileParserFailure {
                        plugin_id: claim.pack.plugin_id.clone(),
                        plugin_name: plugin_name(claim.pack),
                        plugin_version: claim.pack.plugin_version.clone(),
                        parser_id: attempt.parser_id.clone(),
                        kind: failure.kind,
                        message: failure.message.clone(),
                    };
                    report.failures.push(reported.clone());
                    failures.push((file.name.clone(), reported));
                }
            }
        }

        // A fragment the user pasted, which no primary parser claimed, may be read by a pack's
        // snippet parsers. Never for a file the user picked or Agora collected: those must be
        // recognised as whole files, or the answer says there was nothing to read.
        if !primary_claimed && file.origin == CrashFileOrigin::Pasted {
            for claim in claimed
                .iter()
                .filter(|claim| claim.parser.reads == Reads::Snippet)
            {
                let attempt = run_parser_with_builtin(
                    input.host,
                    &claim.pack.plugin_id,
                    claim.parser,
                    &request,
                    reads.clone(),
                    claim.built_in,
                );
                match &attempt.outcome {
                    crate::crash_packs::ParserDispatch::Parsed(output) => {
                        read_any = true;
                        parsed.push(ParsedArtifact {
                            plugin: identity(claim.pack),
                            parser_id: attempt.parser_id.clone(),
                            artifact: file.name.clone(),
                            source: None,
                            output: output.clone(),
                        });
                        if report.read_by.is_none() {
                            report.read_by = Some(claim.reader.clone());
                        }
                        report.snippet = true;
                        snippet_source.get_or_insert_with(|| {
                            identity(claim.pack).parser_source(&claim.parser.id)
                        });
                    }
                    crate::crash_packs::ParserDispatch::WrongFormat(_) => {}
                    crate::crash_packs::ParserDispatch::Failed(failure) => {
                        let reported = FileParserFailure {
                            plugin_id: claim.pack.plugin_id.clone(),
                            plugin_name: plugin_name(claim.pack),
                            plugin_version: claim.pack.plugin_version.clone(),
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

        if !read_any {
            report.read_by = None;
            report.read_by_extra.clear();
            report.reason = Some(if report.failures.is_empty() {
                "no installed crash pack recognised this file".to_string()
            } else {
                "no parser could read this file; see the failures".to_string()
            });
        }
        reports.push(report);
    }

    // With an instance, the game is known and only its packs, plus those for every game, take part.
    // A loose file names no game, so it stands for the games of the packs that read it: another
    // game's rules must not accuse a module in this one. Packs for every game always take part.
    let packs: Vec<&LoadedCrashPack> = match &input.instance {
        Some(instance) => packs
            .into_iter()
            .filter(|pack| {
                pack.pack.games.is_empty()
                    || pack.pack.games.iter().any(|game| game == instance.game_id)
            })
            .collect(),
        None => {
            let read_games: std::collections::BTreeSet<&str> = reports
                .iter()
                .flat_map(|report| report.read_by.iter().chain(report.read_by_extra.iter()))
                .flat_map(|reader| {
                    packs
                        .iter()
                        .filter(move |pack| pack.plugin_id == reader.plugin_id)
                        .flat_map(|pack| pack.pack.games.iter().map(String::as_str))
                })
                .collect();
            packs
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
                .collect()
        }
    };

    let pack_inputs: Vec<super::PackInput> = packs
        .iter()
        .map(|pack| super::PackInput {
            plugin: identity(pack),
            pack: pack.pack.clone(),
        })
        .collect();
    // Only an instance has files for an inspector to open. The fact is always present, with `game`
    // only when there is one, so a rule can require `game` and stay quiet for a loose file without
    // becoming unanchored.
    let instance_attrs = match &input.instance {
        Some(instance) => [(
            "game".to_string(),
            AttrValue::Scalar(Scalar::Text(instance.game_id.to_string())),
        )]
        .into_iter()
        .collect(),
        None => Default::default(),
    };
    let mut engine_facts: Vec<Fact> = vec![Fact {
        kind: "core.instance".to_string(),
        attrs: instance_attrs,
        refs: Vec::new(),
        span: None,
    }];
    engine_facts.extend(extra_facts.iter().cloned());
    let loose_resolver = LooseResolver::from_packs(packs.iter().map(|pack| &pack.pack), &parsed);
    let resolver: &dyn RefResolver = match &input.instance {
        Some(instance) => instance.resolver,
        None => &loose_resolver,
    };
    let no_inspector = NoInspector;
    let inspector: &dyn Inspector = match &input.instance {
        Some(instance) => instance.inspector,
        None => &no_inspector,
    };
    let mut evaluated = evaluate(&EvaluateInput {
        packs: &pack_inputs,
        parsed: &parsed,
        engine_facts: &engine_facts,
        runtime_version: input.runtime_version,
        resolver,
        inspector,
    });
    // A change since the last good launch only ever boosts a suspect another family named.
    evaluated
        .signals
        .extend(super::change::change_signals(changes));
    // A pasted snippet is not a whole log. Say so, and never let the answer read stronger than
    // "possible", whatever the rules found.
    if let Some(source) = snippet_source {
        evaluated.trust_notes.push(TrustNote {
            text: super::english(SNIPPET_TRUST_NOTE),
            source,
        });
        evaluated.level_cap = super::lower_cap(evaluated.level_cap, Level::Possible);
    }
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

    // With an instance and a pack that says what is risky, label each installed-item suspect from
    // the files the resolver can list. Without either, nothing is added: no guessing.
    if input.instance.is_some() {
        if let Some(rules) =
            super::save_risk::merge(packs.iter().filter_map(|pack| pack.pack.save_risk.as_ref()))
        {
            super::save_risk::apply(&mut crash_answer.candidates, &rules, resolver);
        }
    }

    // Nobody was named, but the setup changed since the last good launch: say what changed rather
    // than leave the person with "not sure" and no lead.
    if crash_answer.candidates.is_empty() {
        if let Some(finding) = super::change::change_finding(changes) {
            crash_answer.findings.push(finding);
        }
    }

    let place = super::history::crash_place(
        parsed
            .iter()
            .flat_map(|artifact| artifact.output.facts.iter()),
    );
    let candidates = crash_answer
        .candidates
        .iter()
        .map(|candidate| super::history::HistorySuspect {
            owner: candidate.owner.clone(),
            role: evaluated
                .roles
                .get(&super::owner_key(&candidate.owner))
                .copied(),
        })
        .collect();
    let record = super::history::CrashRecord {
        artifact: input
            .files
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        readable: !parsed.is_empty(),
        candidates,
        place,
    };

    if let Some(instance) = input.instance {
        crash_answer
            .could_not_check
            .extend(instance.could_not_check.iter().cloned());
    }

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
        record,
    })
}

/// Which primary speaks for a file when several recognise it: a built-in pack, then a verified
/// official plugin, then anyone else by plugin id.
fn pack_rank(pack: &LoadedCrashPack) -> u8 {
    if pack.plugin_id == BUILT_IN_PLUGIN_ID {
        0
    } else if pack.official {
        1
    } else {
        2
    }
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
