//! Generic parsers built into core (MASTER_SPEC §30.2, §30.4, §30.11).
//!
//! Each one speaks the same contract as a script: it takes a
//! [`ParserRequest`](agora_game_api::crash::ParserRequest), reads through
//! [`ScriptReads`], and returns a [`ParserOutput`](agora_game_api::crash::ParserOutput). It is
//! configured by data in the pack, holds no game knowledge, and never invokes a script host.

pub mod key_value;
pub mod minidump;
#[cfg(test)]
mod tests;
pub mod text_sections;

use agora_game_api::crash::{
    is_valid_kind, parser_kind_attrs, ParseNote, ParseNoteKind, ParserLimits, ParserOutput,
    ParserRequest, Producer, Scalar, Strength, CRASH_CONTRACT_VERSION,
};
use agora_plugin_api::host::ScriptReads;
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeSet;

/// Why a generic parser could not produce an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenericErrorKind {
    /// The pack's configuration is wrong.
    Config,
    /// The configured `max_facts` ceiling was reached.
    TooManyFacts,
    /// A read the parse needed was refused.
    #[allow(dead_code)]
    Read,
    /// The parser itself could not continue.
    Internal,
}

#[derive(Debug, Clone)]
pub(crate) struct GenericError {
    pub kind: GenericErrorKind,
    pub message: String,
}

impl GenericError {
    fn new(kind: GenericErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::new(GenericErrorKind::Config, message)
    }

    pub(crate) fn too_many(message: impl Into<String>) -> Self {
        Self::new(GenericErrorKind::TooManyFacts, message)
    }

    #[allow(dead_code)]
    pub(crate) fn read(message: impl Into<String>) -> Self {
        Self::new(GenericErrorKind::Read, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(GenericErrorKind::Internal, message)
    }
}

/// Run a named generic parser over one request.
pub(crate) fn run(
    name: &str,
    config: &serde_json::Value,
    request: &ParserRequest,
    reads: &dyn ScriptReads,
) -> Result<ParserOutput, GenericError> {
    match name {
        "text_sections" => text_sections::run(config, request, reads),
        "key_value" => key_value::run(config, request, reads),
        "minidump" => minidump::run(config, request, reads),
        other => Err(GenericError::config(format!(
            "unknown generic parser `{other}`"
        ))),
    }
}

/// Check a generic parser's config without running it. An unknown name is an error.
pub(crate) fn validate_config(name: &str, config: &serde_json::Value) -> Result<(), String> {
    match name {
        "text_sections" => text_sections::validate(config),
        "key_value" => key_value::validate(config),
        "minidump" => minidump::validate(config),
        other => Err(format!("unknown generic parser `{other}`")),
    }
}

/// The `producer` block every generic parser config may carry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProducerConfig {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// A ref a line rule builds from a named capture group.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefRule {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub strength: Strength,
}

/// Build the producer, defaulting the format to the request's parser id.
pub(crate) fn producer(config_format: Option<String>, request: &ParserRequest) -> Producer {
    Producer {
        format: config_format.unwrap_or_else(|| request.parser.clone()),
        version: None,
    }
}

pub(crate) fn producer_with(config: &Option<ProducerConfig>, default_format: &str) -> Producer {
    let (format, version) = match config {
        Some(producer) => (producer.format.clone(), producer.version.clone()),
        None => (None, None),
    };
    Producer {
        format: format.unwrap_or_else(|| default_format.to_string()),
        version,
    }
}

pub(crate) fn check_producer(producer: &Option<ProducerConfig>) -> Result<(), String> {
    if let Some(producer) = producer {
        if producer.format.as_deref() == Some("") {
            return Err("`producer.format` is empty".to_string());
        }
    }
    Ok(())
}

pub(crate) fn check_kind(kind: &str) -> Result<(), String> {
    if !is_valid_kind(kind) {
        return Err(format!("`{kind}` isn't a fact kind"));
    }
    Ok(())
}

pub(crate) fn check_attr(kind: &str, attr: &str) -> Result<(), String> {
    if kind.starts_with("core.") {
        match parser_kind_attrs(kind) {
            Some(allowed) if allowed.contains(&attr) => Ok(()),
            Some(_) => Err(format!("`{kind}` has no attribute `{attr}`")),
            None => Err(format!("a parser can't emit `{kind}`")),
        }
    } else {
        Ok(())
    }
}

pub(crate) fn capture_names(regex: &Regex) -> BTreeSet<String> {
    regex
        .capture_names()
        .flatten()
        .map(str::to_string)
        .collect()
}

pub(crate) fn check_capture(
    names: &BTreeSet<String>,
    pattern: &str,
    group: &str,
    value_is_special: bool,
) -> Result<(), String> {
    if value_is_special && group == "value" {
        return Ok(());
    }
    if names.contains(group) {
        Ok(())
    } else {
        Err(format!(
            "the pattern `{pattern}` has no capture group `{group}`"
        ))
    }
}

/// A fact kind with the attributes each configured rule may set.
#[derive(Debug, Clone, Default)]
pub(crate) struct LineScan {
    /// A line was longer than `max_line_len` and was cut by the host.
    pub cut: bool,
    /// The scan stopped because the read budget was reached.
    pub budget_spent: bool,
    /// A read failed. The scan stopped, but the parse continues.
    pub stopped: Option<(String, String)>,
    /// How many lines the scan saw.
    pub lines: u64,
}

/// Read every line through `read_lines`, in bounded chunks, calling `on_line` with a 1-based line
/// number. Nothing is sized from the artifact: only the lines a reply actually carries are kept.
pub(crate) fn read_all_lines(
    reads: &dyn ScriptReads,
    artifact_id: &str,
    limits: ParserLimits,
    mut on_line: impl FnMut(u64, &str) -> Result<(), GenericError>,
) -> Result<LineScan, GenericError> {
    let chunk = limits.max_lines.clamp(1, 1024);
    let mut from = 0u64;
    let mut scan = LineScan::default();
    let mut total_bytes = 0u64;
    let mut stop = false;

    while !stop {
        let reply = match reads.read_lines(artifact_id, from, chunk) {
            Ok(reply) => reply,
            Err(error) => {
                if error.code == "budget_spent" {
                    scan.budget_spent = true;
                } else {
                    scan.stopped = Some((error.code, error.message));
                }
                break;
            }
        };
        let reply: agora_game_api::crash::LinesReply =
            serde_json::from_value(reply).map_err(|error| {
                GenericError::internal(format!("the lines reply was malformed: {error}"))
            })?;
        scan.cut |= reply.cut;

        for (index, line) in reply.lines.iter().enumerate() {
            let line_no = from + index as u64 + 1;
            total_bytes = total_bytes.saturating_add(line.len() as u64 + 1);
            if total_bytes > limits.max_total_read_bytes {
                scan.budget_spent = true;
                stop = true;
                break;
            }
            on_line(line_no, line)?;
            scan.lines += 1;
        }

        if stop {
            break;
        }
        match reply.next_line {
            Some(next) if next > from => from = next,
            _ => break,
        }
    }

    Ok(scan)
}

/// Convert a captured attribute to the scalar its kind expects. Addresses stay text; small counts
/// and booleans become numbers and bools so rules can compare them.
pub(crate) fn attr_scalar(_kind: &str, attr: &str, text: &str) -> Scalar {
    match attr {
        "index"
        | "depth"
        | "line"
        | "symbol_offset"
        | "pid"
        | "exit_code"
        | "runtime_ms"
        | "uptime_s"
        | "ram_free_mb"
        | "commit_pct"
        | "vram_pct"
        | "same_bucket_count"
        | "sessions_since_first"
        | "size" => text
            .parse::<f64>()
            .map(Scalar::Number)
            .unwrap_or_else(|_| Scalar::Text(text.to_string())),
        "near_null" | "novel" | "predates_last_change" => match text {
            "true" => Scalar::Bool(true),
            "false" => Scalar::Bool(false),
            _ => Scalar::Text(text.to_string()),
        },
        _ => Scalar::Text(text.to_string()),
    }
}

pub(crate) fn push_note(
    notes: &mut Vec<ParseNote>,
    limits: &ParserLimits,
    kind: ParseNoteKind,
    message: impl Into<String>,
) {
    if notes.len() >= limits.max_notes as usize {
        return;
    }
    notes.push(ParseNote {
        kind,
        message: Some(message.into()),
    });
}

/// Cut a string to `max_bytes` without splitting a character.
pub(crate) fn truncate_to(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text
}

pub(crate) fn output(
    producer: Producer,
    facts: Vec<agora_game_api::crash::Fact>,
    bucket: Vec<agora_game_api::crash::BucketPart>,
    coverage: Vec<agora_game_api::crash::Coverage>,
    notes: Vec<ParseNote>,
) -> ParserOutput {
    ParserOutput {
        contract: CRASH_CONTRACT_VERSION,
        producer,
        facts,
        bucket,
        coverage,
        notes,
    }
}
