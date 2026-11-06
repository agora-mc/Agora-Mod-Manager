//! A text log cut into named sections by header lines, with line rules inside each.
//!
//! A section's header may appear many times; every body is scanned. A line matches only inside
//! its own section, and a section that never appears makes its kinds `absent` so rules read
//! unknown rather than false.

use super::{
    attr_scalar, capture_names, check_attr, check_capture, check_kind, check_producer, output,
    producer, push_note, read_all_lines, truncate_to, GenericError, ProducerConfig, RefRule,
};
use agora_game_api::crash::{
    AttrValue, Coverage, CoverageState, Fact, ParseNoteKind, ParserOutput, ParserRequest, Ref,
    SourceSpan,
};
use agora_plugin_api::host::ScriptReads;
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextSectionsConfig {
    #[serde(default)]
    producer: Option<ProducerConfig>,
    sections: Vec<SectionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SectionConfig {
    id: String,
    header: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    absent_why: Option<String>,
    #[serde(default)]
    lines: Vec<LineRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LineRule {
    pattern: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    attrs: BTreeMap<String, String>,
    #[serde(default)]
    refs: Vec<RefRule>,
}

struct CompiledLine {
    regex: Regex,
    kind: String,
    attrs: Vec<(String, String)>,
    refs: Vec<RefRule>,
}

struct CompiledSection {
    id: String,
    header: Regex,
    absent_why: Option<String>,
    kinds: Vec<String>,
    lines: Vec<CompiledLine>,
}

fn load(config: &serde_json::Value) -> Result<TextSectionsConfig, String> {
    let parsed: TextSectionsConfig = serde_json::from_value(config.clone())
        .map_err(|error| format!("the text_sections config is malformed: {error}"))?;
    check_producer(&parsed.producer)?;
    if parsed.sections.is_empty() {
        return Err("a text_sections config needs at least one section".to_string());
    }
    for section in &parsed.sections {
        Regex::new(&section.header)
            .map_err(|error| format!("section `{}` header: {error}", section.id))?;
        if let Some(kind) = &section.kind {
            check_kind(kind)?;
        }
        for rule in &section.lines {
            let regex = Regex::new(&rule.pattern)
                .map_err(|error| format!("section `{}` pattern: {error}", section.id))?;
            let names = capture_names(&regex);
            let kind = rule
                .kind
                .as_deref()
                .or(section.kind.as_deref())
                .ok_or_else(|| {
                    format!(
                        "section `{}` has a line rule with no kind and no section kind",
                        section.id
                    )
                })?;
            check_kind(kind)?;
            for (attr, group) in &rule.attrs {
                check_attr(kind, attr)?;
                check_capture(&names, &rule.pattern, group, false)?;
            }
            for reference in &rule.refs {
                check_capture(&names, &rule.pattern, &reference.value, false)?;
            }
        }
    }
    Ok(parsed)
}

pub(crate) fn validate(config: &serde_json::Value) -> Result<(), String> {
    load(config).map(|_| ())
}

fn compile(config: TextSectionsConfig) -> Result<Vec<CompiledSection>, GenericError> {
    let mut compiled = Vec::with_capacity(config.sections.len());
    for section in config.sections {
        let header = Regex::new(&section.header).map_err(|error| {
            GenericError::config(format!("section `{}` header: {error}", section.id))
        })?;
        let mut kinds = BTreeSet::new();
        if let Some(kind) = &section.kind {
            kinds.insert(kind.clone());
        }
        for line in &section.lines {
            let kind = line
                .kind
                .clone()
                .or_else(|| section.kind.clone())
                .ok_or_else(|| {
                    GenericError::config(format!(
                        "section `{}` has a line rule with no kind and no section kind",
                        section.id
                    ))
                })?;
            kinds.insert(kind);
        }
        let mut lines = Vec::with_capacity(section.lines.len());
        for line in section.lines {
            let kind = line
                .kind
                .clone()
                .or_else(|| section.kind.clone())
                .ok_or_else(|| {
                    GenericError::config(format!(
                        "section `{}` has a line rule with no kind and no section kind",
                        section.id
                    ))
                })?;
            let regex = Regex::new(&line.pattern).map_err(|error| {
                GenericError::config(format!("section `{}` pattern: {error}", section.id))
            })?;
            lines.push(CompiledLine {
                regex,
                kind,
                attrs: line.attrs.into_iter().collect(),
                refs: line.refs,
            });
        }
        compiled.push(CompiledSection {
            id: section.id,
            header,
            absent_why: section.absent_why,
            kinds: kinds.into_iter().collect(),
            lines,
        });
    }
    Ok(compiled)
}

pub(crate) fn run(
    config: &serde_json::Value,
    request: &ParserRequest,
    reads: &dyn ScriptReads,
) -> Result<ParserOutput, GenericError> {
    let config = load(config).map_err(GenericError::config)?;
    let producer = producer(
        config
            .producer
            .as_ref()
            .and_then(|producer| producer.format.clone()),
        request,
    );
    let producer = agora_game_api::crash::Producer {
        version: config.producer.as_ref().and_then(|p| p.version.clone()),
        ..producer
    };
    let sections = compile(config)?;
    let limits = request.limits;

    let mut facts: Vec<Fact> = Vec::new();
    let mut current: Option<usize> = None;
    let mut seen: BTreeSet<usize> = BTreeSet::new();

    let scan = read_all_lines(reads, &request.artifact.id, limits, |line_no, line| {
        if let Some(index) = sections
            .iter()
            .position(|section| section.header.is_match(line))
        {
            current = Some(index);
            seen.insert(index);
            return Ok(());
        }
        let Some(index) = current else {
            return Ok(());
        };
        let section = &sections[index];
        for rule in &section.lines {
            let Some(captures) = rule.regex.captures(line) else {
                continue;
            };
            if facts.len() >= limits.max_facts as usize {
                return Err(GenericError::too_many(format!(
                    "{} facts, over the limit of {}",
                    facts.len() + 1,
                    limits.max_facts
                )));
            }
            let mut attrs = BTreeMap::new();
            for (attr, group) in &rule.attrs {
                if let Some(matched) = captures.name(group) {
                    let text =
                        truncate_to(matched.as_str().to_string(), limits.max_attr_len as usize);
                    attrs.insert(
                        attr.clone(),
                        AttrValue::Scalar(attr_scalar(&rule.kind, attr, &text)),
                    );
                }
            }
            let mut refs = Vec::new();
            for reference in &rule.refs {
                if refs.len() >= limits.max_refs_per_fact as usize {
                    break;
                }
                if let Some(matched) = captures.name(&reference.value) {
                    let value =
                        truncate_to(matched.as_str().to_string(), limits.max_attr_len as usize);
                    if !value.is_empty() {
                        refs.push(Ref {
                            kind: reference.kind.clone(),
                            value,
                            strength: reference.strength,
                        });
                    }
                }
            }
            facts.push(Fact {
                kind: rule.kind.clone(),
                attrs,
                refs,
                span: Some(SourceSpan {
                    artifact: None,
                    section: Some(section.id.clone()),
                    line_start: Some(line_no),
                    line_end: Some(line_no),
                }),
            });
        }
        Ok(())
    })?;

    let mut notes = Vec::new();
    if scan.cut {
        push_note(
            &mut notes,
            &limits,
            ParseNoteKind::Truncated,
            "one or more lines were cut at the line-length limit",
        );
    }
    if scan.budget_spent {
        push_note(
            &mut notes,
            &limits,
            ParseNoteKind::BudgetSpent,
            "the log was not read to the end: the byte budget ran out",
        );
    }
    if let Some((code, message)) = &scan.stopped {
        push_note(
            &mut notes,
            &limits,
            ParseNoteKind::Other,
            format!("reading the log stopped ({code}): {message}"),
        );
    }

    let incomplete = scan.cut || scan.budget_spent || scan.stopped.is_some();
    let mut coverage = Vec::new();
    let mut all_kinds = BTreeSet::new();
    for section in &sections {
        for kind in &section.kinds {
            all_kinds.insert(kind.clone());
        }
    }
    for kind in all_kinds {
        let seen_kind = sections
            .iter()
            .enumerate()
            .any(|(index, section)| seen.contains(&index) && section.kinds.contains(&kind));
        if seen_kind {
            coverage.push(Coverage {
                kind,
                state: CoverageState::Checked,
                why: None,
            });
            continue;
        }
        let Some(section) = sections
            .iter()
            .find(|section| section.kinds.contains(&kind))
        else {
            continue;
        };
        let why = match &section.absent_why {
            Some(why) => why.clone(),
            None if incomplete => format!(
                "the `{}` section was not reached before the log was cut",
                section.id
            ),
            None => format!("the `{}` section is not in this log", section.id),
        };
        coverage.push(Coverage {
            kind,
            state: CoverageState::Absent,
            why: Some(why),
        });
    }

    Ok(output(producer, facts, Vec::new(), coverage, notes))
}
