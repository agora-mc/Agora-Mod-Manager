//! Loading crash packs and running their parsers (MASTER_SPEC §30.11).
//!
//! This module holds no game knowledge. It reads the packs a plugin names in
//! its manifest, lints them through the contract, and drives a parser export
//! through [`ScriptHost`] with a scope that reaches exactly one artifact.
//!
//! Two rules shape everything here:
//!
//! - **A bad pack is reported, never silently dropped, and never stops the
//!   others.** Each plugin's packs are independent; a broken file names the
//!   plugin and the file so a person can fix it.
//! - **A parser sees no paths and no file system.** Its only reads are the two
//!   scoped operations the contract defines, against the artifact in the
//!   request.

use crate::error::LauncherResult;
use agora_game_api::crash::{
    lint_pack_json, validate_parser_output, ArtifactHandle, CodeEntry, CoverageState, CrashPack,
    Detect, GameHandle, LinesReply, ParseNoteKind, ParserDecl, ParserLimits, ParserOutput,
    ParserRequest, ReadRefusal, ReadRefused, CRASH_CONTRACT_VERSION, DETECT_HEAD_BYTES,
};
use agora_plugin_api::host::{ScriptHost, ScriptReadError, ScriptReads};
use agora_plugin_api::manifest::{validate_package_path, PluginManifest};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The most a parser's JSON output may be before it is refused. The contract's
/// per-field limits are the real guard; this catches a well-shaped but enormous
/// result before it is parsed into memory twice.
pub const MAX_PARSER_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Pack loading
// ---------------------------------------------------------------------------

/// One thing wrong with a pack, with the file and (when it has one) the rule id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackProblem {
    pub plugin_id: String,
    /// The manifest-relative path, so a person can open it.
    pub file: String,
    /// `rule example_id`, `parser crash_logger`, or empty for the whole file.
    pub at: String,
    /// The linter's code (or `capability`, `path`, `missing`, `unreadable`).
    pub code: String,
    pub message: String,
}

impl PackProblem {
    fn new(
        plugin_id: &str,
        file: impl Into<String>,
        at: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            file: file.into(),
            at: at.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PackProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.file)?;
        if !self.at.is_empty() {
            write!(f, " ({})", self.at)?;
        }
        write!(f, ": {} [{}]", self.message, self.code)
    }
}

/// A pack that parsed and linted clean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedCrashPack {
    pub plugin_id: String,
    pub file: String,
    pub pack: CrashPack,
}

/// What loading every pack told us. Problems are not fatal to the report:
/// packs that did load stay loaded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashPackReport {
    pub packs: Vec<LoadedCrashPack>,
    pub problems: Vec<PackProblem>,
}

impl CrashPackReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn for_plugin<'a>(&'a self, plugin_id: &'a str) -> impl Iterator<Item = &'a PackProblem> {
        self.problems
            .iter()
            .filter(move |problem| problem.plugin_id == plugin_id)
    }
}

/// One plugin folder to read packs from.
pub struct PluginPackSource<'a> {
    pub plugin_id: &'a str,
    pub root: &'a Path,
    pub manifest: &'a PluginManifest,
}

/// Load packs from several plugins. One plugin's failure never stops another's.
pub fn load_for_plugins(sources: &[PluginPackSource<'_>]) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    for source in sources {
        let one = load_plugin_packs(source.plugin_id, source.root, source.manifest);
        report.packs.extend(one.packs);
        report.problems.extend(one.problems);
    }
    report
}

/// Load every pack a plugin's manifest names.
///
/// A pack without `crash:contribute` is refused. A path that escapes the
/// plugin folder is refused even when the manifest parser already rejected it,
/// because this is the code that actually opens files.
pub fn load_plugin_packs(
    plugin_id: &str,
    root: &Path,
    manifest: &PluginManifest,
) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    if manifest.contributions.crash_packs.is_empty() {
        return report;
    }

    let allowed = manifest
        .capabilities
        .required
        .iter()
        .any(|name| name == "crash:contribute");
    if !allowed {
        for contribution in &manifest.contributions.crash_packs {
            report.problems.push(PackProblem::new(
                plugin_id,
                contribution.path.clone(),
                "",
                "capability",
                "reading crash evidence needs the `crash:contribute` capability",
            ));
        }
        return report;
    }

    let root_real = root.canonicalize().ok();

    for contribution in &manifest.contributions.crash_packs {
        let relative = contribution.path.clone();
        if let Err(error) = validate_package_path("crash pack path", &relative) {
            report.problems.push(PackProblem::new(
                plugin_id,
                relative,
                "",
                "path",
                error.message,
            ));
            continue;
        }

        let target = root.join(relative.replace('\\', "/"));
        let Ok(target_real) = target.canonicalize() else {
            report.problems.push(PackProblem::new(
                plugin_id,
                relative,
                "",
                "missing",
                "the file named in `contributions.crashPacks` is not there",
            ));
            continue;
        };
        if let Some(root_real) = &root_real {
            if !target_real.starts_with(root_real) {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    relative,
                    "",
                    "path",
                    "the pack path resolves outside the plugin folder",
                ));
                continue;
            }
        }

        let bytes = match std::fs::read(&target_real) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    relative,
                    "",
                    "unreadable",
                    format!("could not read the pack: {error}"),
                ));
                continue;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    relative,
                    "",
                    "unreadable",
                    "the pack is not UTF-8 text",
                ));
                continue;
            }
        };

        match lint_pack_json(&text) {
            Ok(pack) => report.packs.push(LoadedCrashPack {
                plugin_id: plugin_id.to_string(),
                file: relative,
                pack,
            }),
            Err(problems) => {
                for problem in problems {
                    report.problems.push(PackProblem::new(
                        plugin_id,
                        relative.clone(),
                        problem.at,
                        format!("{:?}", problem.code).to_lowercase(),
                        problem.message,
                    ));
                }
            }
        }
    }

    report
}

/// Validate a development folder's packs. Used by `agora plugin validate`.
pub fn validate_plugin_folder(folder: &Path) -> LauncherResult<CrashPackReport> {
    let manifest = crate::plugins::install::read_folder_manifest(folder)?;
    Ok(load_plugin_packs(manifest.id.as_str(), folder, &manifest))
}

// ---------------------------------------------------------------------------
// Scoped reads
// ---------------------------------------------------------------------------

/// The artifact bytes a parser may reach, held in memory.
///
/// Deliberately not a handle to a file: the engine decides what is in scope,
/// and a parser has no way to ask for anything else.
pub struct MemoryArtifact {
    files: BTreeMap<String, Arc<Vec<u8>>>,
    limits: ParserLimits,
    total_read: AtomicU64,
}

impl MemoryArtifact {
    pub fn new(limits: ParserLimits) -> Self {
        Self {
            files: BTreeMap::new(),
            limits,
            total_read: AtomicU64::new(0),
        }
    }

    /// Add an artifact by id. The first id is usually the root.
    pub fn insert(&mut self, id: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.files.insert(id.into(), Arc::new(bytes.into()));
    }

    fn budget(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total_read.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(refused(
                    ReadRefusal::BudgetSpent,
                    "this parse has read its whole byte budget",
                ));
            }
            match self.total_read.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn file(&self, id: &str) -> Result<&Arc<Vec<u8>>, ScriptReadError> {
        self.files.get(id).ok_or_else(|| {
            refused(
                ReadRefusal::OutOfScope,
                format!("`{id}` is not part of this parse"),
            )
        })
    }
}

fn refused(reason: ReadRefusal, message: impl Into<String>) -> ScriptReadError {
    let reply = ReadRefused {
        reason,
        message: message.into(),
    };
    let code = match reply.reason {
        ReadRefusal::OutOfScope => "out_of_scope",
        ReadRefusal::TooLarge => "too_large",
        ReadRefusal::BudgetSpent => "budget_spent",
        ReadRefusal::NotRetained => "not_retained",
        ReadRefusal::Unreadable => "unreadable",
    };
    ScriptReadError::new(code, reply.message)
}

impl ScriptReads for MemoryArtifact {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        let data = self.file(artifact)?;
        if len > self.limits.max_read_bytes {
            return Err(refused(
                ReadRefusal::TooLarge,
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let wanted = usize::try_from(len).unwrap_or(usize::MAX);
        let end = start.saturating_add(wanted).min(data.len());
        let slice = &data[start..end];
        self.budget(slice.len() as u64)?;
        Ok(slice.to_vec())
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let data = self.file(artifact)?;
        let max_lines = max_lines.min(self.limits.max_lines);
        let max_len = self.limits.max_line_len as usize;

        // The contract says UTF-8, else the system's code page. A code page is
        // platform-specific; lossy UTF-8 keeps every byte's intent without
        // pretending to know it. Unverified for legacy code pages.
        let text = String::from_utf8_lossy(data);
        let mut lines = Vec::new();
        let mut cut = false;
        let mut index = 0u64;
        let mut more = false;

        // An empty artifact has no lines, and a trailing newline does not
        // invent a final empty one.
        let raw: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            let mut parts: Vec<&str> = text.split('\n').collect();
            if parts.last() == Some(&"") {
                parts.pop();
            }
            parts
        };

        for line in raw {
            if index < from_line {
                index += 1;
                continue;
            }
            if lines.len() >= max_lines as usize {
                more = true;
                break;
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (line, was_cut) = match line.char_indices().nth(max_len) {
                Some((byte, _)) => (&line[..byte], true),
                None if line.len() > max_len => (&line[..max_len], true),
                None => (line, false),
            };
            cut |= was_cut;
            lines.push(line.to_string());
            index += 1;
        }

        let reply = LinesReply {
            lines,
            next_line: more.then_some(index),
            cut,
        };
        serde_json::to_value(reply).map_err(|error| {
            ScriptReadError::new("unreadable", format!("could not encode lines: {error}"))
        })
    }
}

/// Applies a pack's declared lower limits to a reader chosen by the engine.
///
/// The contract says a plugin may ask for less than the host allows, never
/// more. The engine can only know the pack's request here, so the wrapper is
/// where the two meet.
struct LimitedReads {
    inner: Arc<dyn ScriptReads>,
    limits: ParserLimits,
    total: AtomicU64,
}

impl LimitedReads {
    fn new(inner: Arc<dyn ScriptReads>, limits: ParserLimits) -> Self {
        Self {
            inner,
            limits,
            total: AtomicU64::new(0),
        }
    }

    fn spend(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(refused(
                    ReadRefusal::BudgetSpent,
                    "this parse has read its whole byte budget",
                ));
            }
            match self.total.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

impl ScriptReads for LimitedReads {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        if len > self.limits.max_read_bytes {
            return Err(refused(
                ReadRefusal::TooLarge,
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let bytes = self.inner.read_bytes(artifact, offset, len)?;
        self.spend(bytes.len() as u64)?;
        Ok(bytes)
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let max_lines = max_lines.min(self.limits.max_lines);
        let mut reply: LinesReply =
            serde_json::from_value(self.inner.read_lines(artifact, from_line, max_lines)?)
                .map_err(|error| {
                    ScriptReadError::new(
                        "unreadable",
                        format!("the lines reply was malformed: {error}"),
                    )
                })?;
        reply.lines.truncate(max_lines as usize);
        let max_len = self.limits.max_line_len as usize;
        for line in &mut reply.lines {
            if let Some((cut_at, _)) = line.char_indices().nth(max_len) {
                line.truncate(cut_at);
                reply.cut = true;
            }
        }
        serde_json::to_value(reply).map_err(|error| {
            ScriptReadError::new("unreadable", format!("could not encode lines: {error}"))
        })
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Choose the parsers whose `detect` accepts this artifact.
///
/// An empty detect means "only a source's `format` selects me", so it never
/// matches here. A parser whose `detect` is malformed is reported, not ignored:
/// the pack should have been linted, and a silent skip would hide the mistake.
pub fn detect_parsers<'a>(
    pack: &'a CrashPack,
    name: &str,
    head: &[u8],
) -> Result<Vec<&'a ParserDecl>, ParserFailure> {
    let head = &head[..head.len().min(DETECT_HEAD_BYTES)];
    let text = String::from_utf8_lossy(head);
    let mut matched = Vec::new();
    for parser in &pack.parsers {
        if parser.detect.is_empty() {
            continue;
        }
        if detect_matches(&parser.detect, name, head, &text).map_err(|error| ParserFailure {
            plugin_id: String::new(),
            parser_id: parser.id.clone(),
            kind: ParserFailureKind::InvalidDetection,
            message: error,
        })? {
            matched.push(parser);
        }
    }
    Ok(matched)
}

fn detect_matches(detect: &Detect, name: &str, head: &[u8], text: &str) -> Result<bool, String> {
    if let Some(glob) = &detect.name_glob {
        if !glob_matches(glob, name) {
            return Ok(false);
        }
    }
    if let Some(magic) = &detect.magic_hex {
        let magic = magic.to_ascii_lowercase();
        let prefix = &head[..head.len().min(magic.len() / 2)];
        let actual: String = prefix.iter().map(|byte| format!("{byte:02x}")).collect();
        if !magic.starts_with(&actual) || actual.len() != magic.len() {
            return Ok(false);
        }
    }
    for pattern in &detect.all {
        let regex = Regex::new(pattern).map_err(|error| format!("`{pattern}`: {error}"))?;
        if !regex.is_match(text) {
            return Ok(false);
        }
    }
    if !detect.any.is_empty() {
        let mut any = false;
        for pattern in &detect.any {
            let regex = Regex::new(pattern).map_err(|error| format!("`{pattern}`: {error}"))?;
            if regex.is_match(text) {
                any = true;
                break;
            }
        }
        if !any {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Case-insensitive `*` and `?`, over the name only.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

// ---------------------------------------------------------------------------
// Running parsers
// ---------------------------------------------------------------------------

/// Why a parser did not produce a usable output. Names the plugin and parser,
/// because that is the only thing that makes the failure actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParserFailure {
    pub plugin_id: String,
    pub parser_id: String,
    pub kind: ParserFailureKind,
    pub message: String,
}

impl ParserFailure {
    pub fn new(
        plugin_id: impl Into<String>,
        parser_id: impl Into<String>,
        kind: ParserFailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            parser_id: parser_id.into(),
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParserFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "parser `{}` from `{}` could not check this file ({:?}): {}",
            self.parser_id, self.plugin_id, self.kind, self.message
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserFailureKind {
    /// The plugin is not running, or its runtime is gone.
    NotRunning,
    /// The export the pack names does not exist.
    MissingExport,
    /// The script threw.
    ScriptThrew,
    /// The call exceeded the pack's wall-clock limit.
    TimedOut,
    /// The script exceeded its memory ceiling.
    OutOfMemory,
    /// The call was cancelled (user stop, shutdown).
    Cancelled,
    /// This build does not run WebAssembly or generic entries yet.
    UnsupportedEntry,
    /// A `detect` regex is invalid.
    InvalidDetection,
    /// The output was not JSON, or not the contract's shape.
    InvalidOutput,
    /// The output stated another contract version.
    WrongContract,
    /// The output was larger than [`MAX_PARSER_OUTPUT_BYTES`].
    OutputTooLarge,
    /// The host itself failed.
    Internal,
}

/// What one parser said.
#[derive(Debug, Clone, PartialEq)]
pub enum ParserDispatch {
    /// The output is valid and describes this format.
    Parsed(ParserOutput),
    /// The parser looked and said this is not its format; another may try.
    WrongFormat(ParserOutput),
    /// It could not check the file. Never "nothing found".
    Failed(ParserFailure),
}

/// One attempt, under the parser's id.
#[derive(Debug, Clone, PartialEq)]
pub struct ParserAttempt {
    pub parser_id: String,
    pub outcome: ParserDispatch,
}

/// Every attempt made on one artifact.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtifactParse {
    pub attempts: Vec<ParserAttempt>,
    /// No parser's `detect` accepted the artifact. Distinct from failure.
    pub no_parser: bool,
}

impl ArtifactParse {
    /// The first valid, on-format output, if any.
    pub fn parsed(&self) -> Option<&ParserOutput> {
        self.attempts
            .iter()
            .find_map(|attempt| match &attempt.outcome {
                ParserDispatch::Parsed(output) => Some(output),
                _ => None,
            })
    }

    pub fn failures(&self) -> impl Iterator<Item = &ParserFailure> {
        self.attempts
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                ParserDispatch::Failed(failure) => Some(failure),
                _ => None,
            })
    }
}

/// Run the parsers that accept this artifact. A failure in one does not stop
/// the others.
///
/// `format` is the source's `format`, which selects one parser by id and skips
/// detection. Without it, detection reads at most [`DETECT_HEAD_BYTES`].
pub fn parse_artifact(
    host: &dyn ScriptHost,
    plugin_id: &str,
    pack: &CrashPack,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
    format: Option<&str>,
    head: &[u8],
) -> ArtifactParse {
    let selected: Vec<&ParserDecl> = match format {
        Some(id) => match pack.parsers.iter().find(|parser| parser.id == id) {
            Some(parser) => vec![parser],
            None => {
                return ArtifactParse {
                    attempts: vec![ParserAttempt {
                        parser_id: id.to_string(),
                        outcome: ParserDispatch::Failed(ParserFailure::new(
                            plugin_id,
                            id,
                            ParserFailureKind::MissingExport,
                            format!(
                            "the source selects parser `{id}`, which this pack does not declare"
                        ),
                        )),
                    }],
                    no_parser: false,
                }
            }
        },
        None => {
            let head = &head[..head.len().min(DETECT_HEAD_BYTES)];
            match detect_parsers(pack, &request.artifact.name, head) {
                Ok(parsers) => parsers,
                Err(mut failure) => {
                    failure.plugin_id = plugin_id.to_string();
                    return ArtifactParse {
                        attempts: vec![ParserAttempt {
                            parser_id: failure.parser_id.clone(),
                            outcome: ParserDispatch::Failed(failure),
                        }],
                        no_parser: false,
                    };
                }
            }
        }
    };

    if selected.is_empty() {
        return ArtifactParse {
            attempts: Vec::new(),
            no_parser: true,
        };
    }

    let mut attempts = Vec::new();
    for parser in selected {
        let outcome = run_one(host, plugin_id, parser, request, reads.clone());
        attempts.push(ParserAttempt {
            parser_id: parser.id.clone(),
            outcome,
        });
    }
    ArtifactParse {
        attempts,
        no_parser: false,
    }
}

fn run_one(
    host: &dyn ScriptHost,
    plugin_id: &str,
    parser: &ParserDecl,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
) -> ParserDispatch {
    // A pack may ask for less than the engine allows, never more.
    let limits = parser
        .limits
        .map_or(request.limits, |asked| asked.lower_of(request.limits));
    let effective = ParserRequest {
        parser: parser.id.clone(),
        limits,
        ..request.clone()
    };
    let request = &effective;
    let reads: Arc<dyn ScriptReads> = Arc::new(LimitedReads::new(reads, limits));

    let export = match &parser.entry {
        CodeEntry::Script { export } => export.clone(),
        CodeEntry::Wasm { .. } => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::UnsupportedEntry,
                "this build runs script parsers only; a WebAssembly host comes later",
            ))
        }
        CodeEntry::Generic { .. } => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::UnsupportedEntry,
                "generic parsers are not built yet; this pack names a script export instead",
            ))
        }
    };

    let Ok(id) = agora_plugin_api::manifest::PluginId::parse(plugin_id) else {
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::Internal,
            "the plugin id is not a valid id",
        ));
    };
    let args = match serde_json::to_value(request) {
        Ok(args) => args,
        Err(error) => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::Internal,
                format!("could not encode the parser request: {error}"),
            ))
        }
    };
    let timeout = Duration::from_millis(request.limits.max_ms);

    let value = match host.invoke_with_reads(&id, &export, args, timeout, reads) {
        Ok(value) => value,
        Err(error) => {
            let kind = match error.code {
                agora_plugin_api::PluginErrorCode::Timeout => ParserFailureKind::TimedOut,
                agora_plugin_api::PluginErrorCode::ResourceExhausted => {
                    ParserFailureKind::OutOfMemory
                }
                agora_plugin_api::PluginErrorCode::Cancelled => ParserFailureKind::Cancelled,
                agora_plugin_api::PluginErrorCode::NotActivated => ParserFailureKind::NotRunning,
                agora_plugin_api::PluginErrorCode::ScriptError
                    if error.message.contains("exports no function named") =>
                {
                    ParserFailureKind::MissingExport
                }
                agora_plugin_api::PluginErrorCode::ScriptError => {
                    if error.message.contains("interrupted") {
                        ParserFailureKind::TimedOut
                    } else {
                        ParserFailureKind::ScriptThrew
                    }
                }
                _ => ParserFailureKind::Internal,
            };
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                kind,
                error.message,
            ));
        }
    };

    let size = match serde_json::to_vec(&value) {
        Ok(encoded) => encoded.len(),
        Err(error) => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::InvalidOutput,
                format!("the output was not JSON: {error}"),
            ))
        }
    };
    if size > MAX_PARSER_OUTPUT_BYTES {
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::OutputTooLarge,
            format!("the output is {size} bytes; the limit is {MAX_PARSER_OUTPUT_BYTES}"),
        ));
    }

    let output: ParserOutput = match serde_json::from_value(value) {
        Ok(output) => output,
        Err(error) => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::InvalidOutput,
                format!("the output is not the crash contract's parser shape: {error}"),
            ))
        }
    };

    if output.contract != CRASH_CONTRACT_VERSION {
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::WrongContract,
            format!(
                "output says contract {}, and this Agora reads {CRASH_CONTRACT_VERSION}",
                output.contract
            ),
        ));
    }

    let problems = validate_parser_output(&output, &request.limits);
    if !problems.is_empty() {
        let message = problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::InvalidOutput,
            message,
        ));
    }

    let wrong_format = output
        .notes
        .iter()
        .any(|note| note.kind == ParseNoteKind::WrongFormat);
    if wrong_format {
        ParserDispatch::WrongFormat(output)
    } else {
        ParserDispatch::Parsed(output)
    }
}

/// Build the handle a parser request carries for one artifact.
pub fn artifact_handle(
    id: impl Into<String>,
    name: impl Into<String>,
    size: u64,
    children: Vec<ArtifactHandle>,
) -> ArtifactHandle {
    ArtifactHandle {
        id: id.into(),
        name: name.into(),
        size,
        modified_at: None,
        source: None,
        children,
    }
}

/// Build the request handed to a parser.
pub fn parser_request(
    parser_id: impl Into<String>,
    artifact: ArtifactHandle,
    game: GameHandle,
    limits: ParserLimits,
) -> ParserRequest {
    ParserRequest {
        contract: CRASH_CONTRACT_VERSION,
        parser: parser_id.into(),
        artifact,
        game,
        limits,
    }
}

/// A coverage entry saying this parser looked for `kind` and found none.
pub fn checked(kind: impl Into<String>) -> agora_game_api::crash::Coverage {
    agora_game_api::crash::Coverage {
        kind: kind.into(),
        state: CoverageState::Checked,
        why: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_is_case_insensitive_and_bounded() {
        assert!(glob_matches("crash-*.log", "Crash-2024.LOG"));
        assert!(glob_matches("?rash.log", "crash.log"));
        assert!(!glob_matches("crash-*.log", "crash.log"));
        assert!(!glob_matches("crash-*.log", "crash-2024.txt"));
        assert!(!glob_matches("", "anything"));
    }

    #[test]
    fn empty_detect_never_matches_on_its_own() {
        let pack: CrashPack = serde_json::from_str(
            r#"{"contract":1,"parsers":[{"id":"only_by_format","entry":{"script":{"export":"parse"}}}]}"#,
        )
        .unwrap();
        assert!(detect_parsers(&pack, "x.log", b"anything")
            .unwrap()
            .is_empty());
    }
}
