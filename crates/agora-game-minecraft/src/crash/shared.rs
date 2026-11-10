//! Line collection and small helpers shared by the built-in Minecraft parsers.

use agora_game_api::crash::{
    AttrValue, BuiltInParserError, Coverage, CoverageState, Fact, LinesReply, ParseNote,
    ParseNoteKind, ParserLimits, ParserReads, Ref, Scalar, SourceSpan, Strength,
};
use std::collections::BTreeMap;

pub(crate) struct Collected {
    pub lines: Vec<String>,
    pub cut: bool,
    pub budget_spent: bool,
    pub stopped: Option<String>,
}

/// Read an artifact through the scoped reads, keeping within the request's limits. A read that
/// fails after some lines is reported, not thrown away: what was read is still used.
pub(crate) fn collect_lines(
    reads: &dyn ParserReads,
    artifact: &str,
    limits: ParserLimits,
) -> Result<Collected, BuiltInParserError> {
    let mut lines = Vec::new();
    let mut from = 0u64;
    let mut cut = false;
    let mut budget_spent = false;
    let mut stopped = None;
    let mut total = 0u64;

    loop {
        let reply: LinesReply = match reads.read_lines(artifact, from, 1024) {
            Ok(reply) => reply,
            Err(error) => {
                if lines.is_empty() {
                    return Err(BuiltInParserError::new(format!(
                        "could not read the artifact: {}",
                        error.message
                    )));
                }
                stopped = Some(format!("reading stopped ({})", error.code));
                break;
            }
        };
        cut |= reply.cut;
        let mut hit_limit = false;
        for line in reply.lines {
            total = total.saturating_add(line.len() as u64 + 1);
            if total > limits.max_total_read_bytes || lines.len() >= limits.max_lines as usize {
                budget_spent = true;
                hit_limit = true;
                break;
            }
            lines.push(line);
        }
        if hit_limit {
            break;
        }
        match reply.next_line {
            Some(next) if next > from => from = next,
            _ => break,
        }
    }

    Ok(Collected {
        lines,
        cut,
        budget_spent,
        stopped,
    })
}

pub(crate) fn text(value: impl Into<String>) -> AttrValue {
    AttrValue::Scalar(Scalar::Text(value.into()))
}

pub(crate) fn number(value: f64) -> AttrValue {
    AttrValue::Scalar(Scalar::Number(value))
}

pub(crate) fn reference(kind: &str, value: impl Into<String>, strength: Strength) -> Ref {
    Ref {
        kind: kind.to_string(),
        value: value.into(),
        strength,
    }
}

pub(crate) fn fact(
    kind: &str,
    attrs: Vec<(&str, AttrValue)>,
    refs: Vec<Ref>,
    line: usize,
    section: &str,
) -> Fact {
    Fact {
        kind: kind.to_string(),
        attrs: attrs
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect::<BTreeMap<_, _>>(),
        refs,
        span: Some(SourceSpan {
            artifact: None,
            section: Some(section.to_string()),
            line_start: Some(line as u64),
            line_end: Some(line as u64),
        }),
    }
}

pub(crate) fn checked(kind: &str) -> Coverage {
    Coverage {
        kind: kind.to_string(),
        state: CoverageState::Checked,
        why: None,
    }
}

pub(crate) fn not_applicable(kind: &str) -> Coverage {
    Coverage {
        kind: kind.to_string(),
        state: CoverageState::NotApplicable,
        why: None,
    }
}

pub(crate) fn absent(kind: &str, why: impl Into<String>) -> Coverage {
    Coverage {
        kind: kind.to_string(),
        state: CoverageState::Absent,
        why: Some(why.into()),
    }
}

/// A kind the parser looked for: checked when the artifact reached the part that holds it, absent
/// with a reason when it did not. A budget that ran out is always "could not check", never a
/// clean answer.
pub(crate) fn looked_for(
    kind: &str,
    reached: bool,
    why_absent: &str,
    budget_spent: bool,
) -> Coverage {
    if reached {
        checked(kind)
    } else if budget_spent {
        absent(
            kind,
            "the file was not read to the end: its read budget ran out",
        )
    } else {
        absent(kind, why_absent)
    }
}

pub(crate) fn notes_for(collected: &Collected, limits: ParserLimits) -> Vec<ParseNote> {
    let mut notes = Vec::new();
    let mut push = |kind: ParseNoteKind, message: &str| {
        if notes.len() < limits.max_notes as usize {
            notes.push(ParseNote {
                kind,
                message: Some(message.to_string()),
            });
        }
    };
    if collected.cut {
        push(
            ParseNoteKind::Truncated,
            "one or more lines were cut at the line-length limit",
        );
    }
    if collected.budget_spent {
        push(
            ParseNoteKind::BudgetSpent,
            "the file was not read to the end: the byte budget ran out",
        );
    }
    if let Some(message) = &collected.stopped {
        push(ParseNoteKind::Other, message);
    }
    notes
}

/// The category a Java exception class falls into, in the contract's closed vocabulary.
pub(crate) fn java_exception_category(class: &str) -> &'static str {
    if class.ends_with("OutOfMemoryError") {
        "oom"
    } else if class.ends_with("ClassNotFoundException")
        || class.ends_with("NoClassDefFoundError")
        || class.ends_with("ExceptionInInitializerError")
    {
        "load_failure"
    } else if class.ends_with("AssertionError") {
        "assert"
    } else {
        "managed"
    }
}

/// A Java source location: `(Thing.java:42)` or `(Unknown Source)`.
pub(crate) fn source_location(invocation: &str) -> (Option<String>, Option<f64>) {
    let Some(open) = invocation.find('(') else {
        return (None, None);
    };
    let inside = invocation[open + 1..].trim_end_matches(')').trim();
    if inside.is_empty() || inside.eq_ignore_ascii_case("native method") {
        return (None, None);
    }
    match inside.rsplit_once(':') {
        Some((file, line)) => (Some(file.trim().to_string()), line.trim().parse().ok()),
        None => (Some(inside.to_string()), None),
    }
}

/// Strip the loader's decoration from a frame's class token. Fabric writes `knot//a.b.C`, Forge
/// writes `TRANSFORMER/name@version/a.b.C`, and a JDK frame writes `java.base/jdk.internal...`.
pub(crate) fn class_from_frame(qualified: &str) -> Option<String> {
    let class_path = qualified.rsplit('/').next().unwrap_or(qualified);
    let class = match class_path.rsplit_once('.') {
        Some((class, _method)) => class,
        None => return None,
    };
    if class.is_empty() || class == "-" {
        None
    } else {
        Some(class.to_string())
    }
}

/// The jar or module a Forge/NeoForge frame annotation names: `~[some.jar%123!/:?]`.
pub(crate) fn jar_from_frame(line: &str) -> Option<String> {
    let start = line.find("~[")? + 2;
    let end = line[start..].find(']')? + start;
    let inside = &line[start..end];
    let name = inside.split('%').next().unwrap_or(inside);
    let name = name.split("!/").next().unwrap_or(name).trim();
    if name.is_empty() || name == "?" || name == "?:?" {
        None
    } else {
        Some(name.to_string())
    }
}
