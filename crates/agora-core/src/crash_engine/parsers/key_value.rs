//! INI-style key/value logs: `[Section]`, then `key = value` or `key: value`.
//!
//! Keys compare case-insensitively. A duplicate key replaces the earlier fact, matching
//! [`game_ini`](crate::game_ini)'s "last line wins" convention. A line with no separator is
//! ignored, not an error.

use super::{
    attr_scalar, capture_names, check_attr, check_capture, check_kind, check_producer, output,
    push_note, read_all_lines, truncate_to, GenericError, ProducerConfig, RefRule,
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
struct KeyValueConfig {
    #[serde(default)]
    producer: Option<ProducerConfig>,
    keys: Vec<KeyRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyRule {
    key: String,
    #[serde(default)]
    section: Option<String>,
    kind: String,
    #[serde(default)]
    value_pattern: Option<String>,
    #[serde(default)]
    attrs: BTreeMap<String, String>,
    #[serde(default)]
    refs: Vec<RefRule>,
}

struct CompiledRule {
    key: String,
    section: Option<String>,
    kind: String,
    pattern: Option<Regex>,
    attrs: Vec<(String, String)>,
    refs: Vec<RefRule>,
}

fn load(config: &serde_json::Value) -> Result<KeyValueConfig, String> {
    let parsed: KeyValueConfig = serde_json::from_value(config.clone())
        .map_err(|error| format!("the key_value config is malformed: {error}"))?;
    check_producer(&parsed.producer)?;
    if parsed.keys.is_empty() {
        return Err("a key_value config needs at least one key".to_string());
    }
    for rule in &parsed.keys {
        check_kind(&rule.kind)?;
        let names = match &rule.value_pattern {
            Some(pattern) => {
                let regex = Regex::new(pattern)
                    .map_err(|error| format!("key `{}` value_pattern: {error}", rule.key))?;
                capture_names(&regex)
            }
            None => BTreeSet::new(),
        };
        for (attr, group) in &rule.attrs {
            check_attr(&rule.kind, attr)?;
            check_capture(
                &names,
                rule.value_pattern.as_deref().unwrap_or(""),
                group,
                true,
            )?;
        }
        for reference in &rule.refs {
            check_capture(
                &names,
                rule.value_pattern.as_deref().unwrap_or(""),
                &reference.value,
                true,
            )?;
        }
    }
    Ok(parsed)
}

pub(crate) fn validate(config: &serde_json::Value) -> Result<(), String> {
    load(config).map(|_| ())
}

pub(crate) fn run(
    config: &serde_json::Value,
    request: &ParserRequest,
    reads: &dyn ScriptReads,
) -> Result<ParserOutput, GenericError> {
    let config = load(config).map_err(GenericError::config)?;
    let producer = super::producer_with(&config.producer, &request.parser);
    let mut rules = Vec::with_capacity(config.keys.len());
    for rule in config.keys {
        let pattern = match &rule.value_pattern {
            Some(pattern) => Some(Regex::new(pattern).map_err(|error| {
                GenericError::config(format!("key `{}` value_pattern: {error}", rule.key))
            })?),
            None => None,
        };
        rules.push(CompiledRule {
            pattern,
            key: rule.key,
            section: rule.section,
            kind: rule.kind,
            attrs: rule.attrs.into_iter().collect(),
            refs: rule.refs,
        });
    }
    let limits = request.limits;

    let mut facts: Vec<Fact> = Vec::new();
    let mut current_section: Option<String> = None;
    let mut seen: BTreeMap<(usize, String, String), usize> = BTreeMap::new();

    let scan = read_all_lines(reads, &request.artifact.id, limits, |line_no, raw| {
        let line = if line_no == 1 {
            raw.strip_prefix('\u{feff}').unwrap_or(raw)
        } else {
            raw
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            return Ok(());
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            current_section = Some(name.trim().to_string());
            return Ok(());
        }
        let Some(separator) = line.find(['=', ':']) else {
            return Ok(());
        };
        let key = line[..separator].trim();
        if key.is_empty() {
            return Ok(());
        }
        let value = line[separator + 1..].trim();

        for (rule_index, rule) in rules.iter().enumerate() {
            if !key.eq_ignore_ascii_case(&rule.key) {
                continue;
            }
            if let Some(section) = &rule.section {
                let matches = current_section
                    .as_deref()
                    .is_some_and(|current| current.eq_ignore_ascii_case(section));
                if !matches {
                    continue;
                }
            }
            let captures = match &rule.pattern {
                Some(pattern) => match pattern.captures(value) {
                    Some(captures) => Some(captures),
                    None => continue,
                },
                None => None,
            };

            let mut attrs = BTreeMap::new();
            for (attr, group) in &rule.attrs {
                let text = if group == "value" {
                    Some(value)
                } else {
                    captures
                        .as_ref()
                        .and_then(|captures| captures.name(group))
                        .map(|matched| matched.as_str())
                };
                if let Some(text) = text {
                    let text = truncate_to(text.to_string(), limits.max_attr_len as usize);
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
                let text = if reference.value == "value" {
                    Some(value)
                } else {
                    captures
                        .as_ref()
                        .and_then(|captures| captures.name(&reference.value))
                        .map(|matched| matched.as_str())
                };
                if let Some(text) = text {
                    let value = truncate_to(text.to_string(), limits.max_attr_len as usize);
                    if !value.is_empty() {
                        refs.push(Ref {
                            kind: reference.kind.clone(),
                            value,
                            strength: reference.strength,
                        });
                    }
                }
            }

            let fact = Fact {
                kind: rule.kind.clone(),
                attrs,
                refs,
                span: Some(SourceSpan {
                    artifact: None,
                    section: current_section.clone(),
                    line_start: Some(line_no),
                    line_end: Some(line_no),
                }),
            };
            let section_key = current_section
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            let dedupe = (rule_index, section_key, key.to_ascii_lowercase());
            if let Some(&index) = seen.get(&dedupe) {
                facts[index] = fact;
            } else {
                if facts.len() >= limits.max_facts as usize {
                    return Err(GenericError::too_many(format!(
                        "{} facts, over the limit of {}",
                        facts.len() + 1,
                        limits.max_facts
                    )));
                }
                seen.insert(dedupe, facts.len());
                facts.push(fact);
            }
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

    let incomplete = scan.budget_spent || scan.stopped.is_some();
    let found: BTreeSet<&str> = facts.iter().map(|fact| fact.kind.as_str()).collect();
    let mut kinds = BTreeSet::new();
    for rule in &rules {
        kinds.insert(rule.kind.clone());
    }
    let mut coverage = Vec::new();
    for kind in kinds {
        if incomplete && !found.contains(kind.as_str()) {
            coverage.push(Coverage {
                kind,
                state: CoverageState::Absent,
                why: Some("the file was not read to the end: the byte budget ran out".to_string()),
            });
        } else {
            coverage.push(Coverage {
                kind,
                state: CoverageState::Checked,
                why: None,
            });
        }
    }

    Ok(output(producer, facts, Vec::new(), coverage, notes))
}
