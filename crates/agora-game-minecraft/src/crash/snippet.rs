//! A pasted Minecraft fragment: a stack trace, a loader line, a mixin failure, an out-of-memory
//! line. It reuses the line readers of the whole-file parsers, because the shapes are the same; it
//! only stops requiring the banner, the timestamp or the section that a whole file has.
//!
//! This parser is declared `reads: "snippet"`, so it runs only on text the user pasted and no
//! primary parser claimed. Its output is an ordinary parser output: the same facts, so the pack's
//! rules work on them, and a rule that needs a section a fragment cannot have comes out "could not
//! check".

use super::latest_log::{
    duplicate_mod, exception_line, fabric_dependency, forge_dependency, loader_problem,
    looks_like_config_error, looks_like_mod_error, looks_like_world_error, mod_conflict,
    parse_fatal_exception, strip_log_prefix,
};
use super::shared::{
    collect_lines, fact, loader_dependency_problem, mixin_failure_fact, mixin_failure_line,
    mod_crash_from_message, mod_crash_line, notes_for, number, reference, text,
};
use agora_game_api::crash::{
    AttrValue, BuiltInParser, BuiltInParserError, Fact, ParserOutput, ParserReads, ParserRequest,
    Producer, Scalar, Strength, CRASH_CONTRACT_VERSION,
};
use std::collections::BTreeSet;

pub struct SnippetParser;

/// The full parser whose line handling this fragment uses. A snippet is loose log text, so the
/// latest-log rules (loader problems, mixin failures, dependencies) are the ones that apply.
const FORMAT: &str = "minecraft_latest_log";

impl BuiltInParser for SnippetParser {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        let collected = collect_lines(reads, &request.artifact.id, request.limits)?;
        if collected.lines.is_empty() {
            return Err(BuiltInParserError::new(
                "the pasted text is empty; there is nothing to read",
            ));
        }

        // The whole-file readers expect a log line's `[time] [thread/WARN]: ` prefix gone and an
        // `Exception in thread "..."` header removed. Do that once, then hand the same lines to the
        // readers the whole-file parser uses.
        let lines: Vec<String> = collected
            .lines
            .iter()
            .map(|line| snippet_line(line))
            .collect();

        let mut facts: Vec<Fact> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut mod_crash_order = 0u32;

        // The first exception line, with its `Caused by:` chain and `at` frames. The same reader
        // the latest-log parser uses, so the two never drift.
        let fatal = lines.iter().enumerate().find_map(|(offset, line)| {
            exception_line(line)
                .filter(|(caused, _, _)| !*caused)
                .map(|_| (offset, line.clone()))
        });
        let (exception_facts, bucket) = parse_fatal_exception(&lines, fatal.as_ref());
        facts.extend(exception_facts);

        for (offset, line) in lines.iter().enumerate() {
            let line_no = offset + 1;
            let trimmed = line.trim();

            if let Some((state, required_by, missing, requirement)) = fabric_dependency(trimmed) {
                let key = format!("dep\0{required_by}\0{missing}\0{requirement}\0{state}");
                if seen.insert(key) {
                    if state == "incompatible" {
                        facts.push(fact(
                            "minecraft.loader_problem",
                            vec![
                                ("kind", text("incompatible")),
                                ("message", text(trimmed.to_string())),
                            ],
                            vec![reference("mod_id", missing, Strength::Mention)],
                            line_no,
                            "loader",
                        ));
                    } else {
                        facts.push(fact(
                            "core.dependency_problem",
                            vec![
                                ("requirement", text(requirement)),
                                ("state", text(state)),
                                ("needed_by", text(required_by.clone())),
                            ],
                            vec![
                                reference("mod_id", missing, Strength::Mention),
                                reference("needed_by", required_by, Strength::Mention),
                            ],
                            line_no,
                            "loader",
                        ));
                    }
                }
            } else if let Some((state, required_by, missing, requirement)) =
                forge_dependency(trimmed)
            {
                let key = format!("dep\0{required_by}\0{missing}\0{requirement}");
                if seen.insert(key) {
                    facts.push(fact(
                        "core.dependency_problem",
                        vec![
                            ("requirement", text(requirement)),
                            ("state", text(state)),
                            ("needed_by", text(required_by.clone())),
                        ],
                        vec![
                            reference("mod_id", missing, Strength::Mention),
                            reference("needed_by", required_by, Strength::Mention),
                        ],
                        line_no,
                        "loader",
                    ));
                }
            }

            if let Some(failure) = mixin_failure_line(trimmed) {
                let key = format!(
                    "mixin\0{}\0{}\0{}",
                    failure.owner.as_deref().unwrap_or(""),
                    failure.config,
                    failure.target.as_deref().unwrap_or("")
                );
                if seen.insert(key) {
                    facts.push(mixin_failure_fact(failure, line_no, Strength::ExecTop));
                }
            }

            if let Some(id) = duplicate_mod(trimmed) {
                let key = format!("duplicate\0{id}");
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.duplicate_mod",
                        vec![("id", text(id.clone()))],
                        vec![reference("mod_id", id, Strength::Mention)],
                        line_no,
                        "loader",
                    ));
                }
            }

            if let Some(crash) = mod_crash_line(line).or_else(|| mod_crash_from_message(line)) {
                let key = format!("mod_crash\0{}", crash.id());
                if seen.insert(key) {
                    facts.push(crash.into_fact(mod_crash_order, line_no));
                    mod_crash_order += 1;
                }
            }

            if let Some(conflict) = mod_conflict(trimmed) {
                let key = format!("conflict\0{}\0{}", conflict.0, conflict.1);
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.mod_conflict",
                        vec![
                            ("first", text(conflict.0.clone())),
                            ("second", text(conflict.1.clone())),
                            ("message", text(trimmed.to_string())),
                        ],
                        vec![
                            reference("mod_id", conflict.0, Strength::Mention),
                            reference("mod_id", conflict.1, Strength::Mention),
                        ],
                        line_no,
                        "loader",
                    ));
                }
            }

            if let Some(kind) = loader_problem(trimmed) {
                let key = format!("problem\0{kind}\0{trimmed}");
                if seen.insert(key) {
                    let mut attrs = vec![("kind", text(kind))];
                    if !trimmed.is_empty() {
                        attrs.push(("message", text(trimmed.to_string())));
                    }
                    facts.push(fact(
                        "minecraft.loader_problem",
                        attrs,
                        Vec::new(),
                        line_no,
                        "loader",
                    ));
                }
            }

            if looks_like_mod_error(trimmed) {
                let key = format!("mod_error\0{trimmed}");
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.mod_error",
                        vec![("message", text(trimmed.to_string()))],
                        Vec::new(),
                        line_no,
                        "loader",
                    ));
                }
            }
            if looks_like_config_error(trimmed) {
                let key = format!("config\0{trimmed}");
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.config_error",
                        vec![("message", text(trimmed.to_string()))],
                        Vec::new(),
                        line_no,
                        "loader",
                    ));
                }
            }
            if looks_like_world_error(trimmed) {
                let key = format!("world\0{trimmed}");
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.world_error",
                        vec![("message", text(trimmed.to_string()))],
                        Vec::new(),
                        line_no,
                        "world",
                    ));
                }
            }
        }

        // A Forge loader exception can imply a dependency problem even with no explicit line.
        if !facts
            .iter()
            .any(|fact| fact.kind == "core.dependency_problem")
        {
            if let Some(problem) = loader_dependency_problem(&facts) {
                facts.push(problem);
            }
        }

        // An out-of-memory error anywhere is its own fact, even when it is a `Caused by:` line with
        // nothing before it in the fragment.
        let oom_line = lines
            .iter()
            .position(|line| line.contains("OutOfMemoryError"));
        let has_oom = facts.iter().any(is_oom);
        if let (Some(offset), false) = (oom_line, has_oom) {
            facts.push(fact(
                "core.exception",
                vec![
                    ("class", text("java.lang.OutOfMemoryError")),
                    ("category", text("oom")),
                    (
                        "message",
                        text("the pasted text reports Java ran out of memory"),
                    ),
                    ("depth", number(0.0)),
                ],
                Vec::new(),
                offset + 1,
                "fatal",
            ));
        }

        // A fragment cannot hold the sections a whole file has, so it claims nothing about them.
        // Every kind it did not find stays "could not check" for a rule that asks, which is the
        // truth for a fragment.
        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: FORMAT.to_string(),
                version: None,
            },
            facts,
            bucket,
            coverage: Vec::new(),
            notes: notes_for(&collected, request.limits),
        })
    }
}

/// A log line with the decorations a whole-file reader removes: the `[time] [thread]: ` prefix,
/// and the `Exception in thread "..."` header the JVM prints before an uncaught exception.
fn snippet_line(line: &str) -> String {
    let line = strip_log_prefix(line);
    if let Some(rest) = line.strip_prefix("Exception in thread ") {
        if let Some((_, rest)) = rest.split_once("\" ") {
            return rest.to_string();
        }
    }
    line.to_string()
}

fn is_oom(fact: &Fact) -> bool {
    fact.kind == "core.exception"
        && matches!(
            fact.attrs.get("category"),
            Some(AttrValue::Scalar(Scalar::Text(category))) if category == "oom"
        )
}
