//! The game's `latest.log`: the loader's own account of what it refused to load.

use super::shared::{
    class_from_frame, collect_lines, fact, jar_from_frame, java_exception_category, looked_for,
    notes_for, number, reference, source_location, text,
};
use agora_game_api::crash::{
    BucketPart, BuiltInParser, BuiltInParserError, Coverage, CoverageState, Fact, ParserOutput,
    ParserReads, ParserRequest, Producer, Strength, CRASH_CONTRACT_VERSION,
};
use std::collections::BTreeSet;

pub struct LatestLogParser;

const FORMAT: &str = "minecraft_latest_log";

impl BuiltInParser for LatestLogParser {
    fn parse(
        &self,
        request: &ParserRequest,
        reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        let collected = collect_lines(reads, &request.artifact.id, request.limits)?;
        if collected.lines.is_empty() {
            return Err(BuiltInParserError::new(
                "the log is empty; there is nothing to read",
            ));
        }

        let mut facts: Vec<Fact> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut game_version: Option<String> = None;
        let mut loader: Option<(String, String)> = None;
        let mut java_version: Option<String> = None;
        let mut java_vendor: Option<String> = None;
        let mut os: Option<String> = None;
        let mut fatal_line: Option<(usize, String)> = None;

        for (offset, line) in collected.lines.iter().enumerate() {
            let line_no = offset + 1;
            let trimmed = line.trim();

            let message = strip_log_prefix(line);
            if let Some(rest) = message.strip_prefix("Loading Minecraft ") {
                if let Some((version, rest)) = rest.split_once(" with ") {
                    if game_version.is_none() {
                        game_version = Some(version.trim().to_string());
                    }
                    let (id, version) = match rest.split_once(' ') {
                        Some((loader, rest)) => {
                            let id = loader.trim_end_matches(" Loader");
                            (id.to_ascii_lowercase(), rest.trim().to_string())
                        }
                        None => (rest.to_ascii_lowercase(), String::new()),
                    };
                    if loader.is_none() {
                        loader = Some((id, version));
                    }
                }
            }
            if let Some(rest) = message.strip_prefix("Loading for game Minecraft ") {
                if let Some(version) = rest.split_whitespace().next() {
                    game_version.get_or_insert_with(|| version.to_string());
                }
            }
            if let Some(pos) = trimmed.find("--fml.mcVersion, ") {
                let version = trimmed[pos + "--fml.mcVersion, ".len()..]
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim();
                game_version.get_or_insert_with(|| version.to_string());
            }
            if let Some(pos) = trimmed.find("--version, ") {
                let token = trimmed[pos + "--version, ".len()..]
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim();
                apply_launcher_version(token, &mut game_version, &mut loader);
            }
            if let Some(rest) = trimmed.split("Found mod file \"").nth(1) {
                if let Some((file, _)) = rest.split_once('"') {
                    if let Some((id, version)) = loader_universal_jar(file) {
                        loader.get_or_insert((id, version));
                    }
                }
            }
            if let Some(pos) = trimmed.find("starting: java version ") {
                let rest = &trimmed[pos + "starting: java version ".len()..];
                if let Some((version, rest)) = rest.split_once(" by ") {
                    java_version.get_or_insert_with(|| version.trim().to_string());
                    let (vendor, os_part) = match rest.split_once("; OS ") {
                        Some((vendor, os)) => (vendor.trim(), Some(os.trim())),
                        None => (rest.trim(), None),
                    };
                    java_vendor.get_or_insert_with(|| vendor.to_string());
                    if let Some(os_text) = os_part {
                        os.get_or_insert_with(|| os_text.to_string());
                    }
                }
            }
            if let Some(rest) = trimmed.strip_prefix("JVM identified as ") {
                let rest = rest.trim();
                if java_version.is_none() {
                    if let Some(index) = rest.find("OpenJDK") {
                        let vendor = rest[..index].trim();
                        java_vendor.get_or_insert_with(|| vendor.to_string());
                        let after = rest[index + "OpenJDK".len()..].trim();
                        if let Some(token) = after.split_whitespace().next() {
                            java_version.get_or_insert_with(|| token.replace('+', "."));
                        }
                    }
                }
            }

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
                                ("needed_by", text(required_by)),
                            ],
                            vec![reference("mod_id", missing, Strength::Mention)],
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
                            ("needed_by", text(required_by)),
                        ],
                        vec![reference("mod_id", missing, Strength::Mention)],
                        line_no,
                        "loader",
                    ));
                }
            }

            if let Some((mod_id, config, target)) = mixin_failure(trimmed) {
                let key = format!(
                    "mixin\0{}\0{config}\0{target}",
                    mod_id.as_deref().unwrap_or("")
                );
                if seen.insert(key) {
                    facts.push(fact(
                        "minecraft.mixin_failure",
                        vec![
                            ("severity", text("critical")),
                            ("config", text(config)),
                            ("target", text(target)),
                        ],
                        mod_id
                            .map(|id| vec![reference("mod_id", id, Strength::ExecTop)])
                            .unwrap_or_default(),
                        line_no,
                        "mixin",
                    ));
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

            if fatal_line.is_none() {
                let message = strip_log_prefix(line);
                if let Some((caused, class, _)) = exception_line(message) {
                    if !caused {
                        fatal_line = Some((offset, message.to_string()));
                        let _ = class;
                    }
                }
            }
        }

        let (exception_facts, bucket) =
            parse_fatal_exception(&collected.lines, fatal_line.as_ref());
        facts.extend(exception_facts);

        // An out-of-memory error anywhere is its own fact, even if the first fatal was something else.
        let oom_line = collected.lines.iter().position(|line| {
            line.contains("java.lang.OutOfMemoryError") || line.contains("OutOfMemoryError")
        });
        let has_oom = facts.iter().any(|fact| {
            fact.kind == "core.exception"
                && matches!(
                    fact.attrs.get("category"),
                    Some(agora_game_api::crash::AttrValue::Scalar(
                        agora_game_api::crash::Scalar::Text(category)
                    )) if category == "oom"
                )
        });
        if let (Some(offset), false) = (oom_line, has_oom) {
            facts.push(fact(
                "core.exception",
                vec![
                    ("class", text("java.lang.OutOfMemoryError")),
                    ("category", text("oom")),
                    ("message", text("the log reports Java ran out of memory")),
                    ("depth", number(0.0)),
                ],
                Vec::new(),
                offset + 1,
                "fatal",
            ));
        }

        if let Some((id, version)) = &loader {
            facts.push(fact(
                "minecraft.loader",
                vec![("id", text(id.clone())), ("version", text(version.clone()))],
                Vec::new(),
                0,
                "loader",
            ));
        }
        if let Some(version) = &game_version {
            facts.push(fact(
                "core.process",
                vec![("image_version", text(version.clone()))],
                Vec::new(),
                0,
                "loader",
            ));
        }
        let mut system_attrs = Vec::new();
        if let Some(version) = &java_version {
            system_attrs.push(("java_version", text(version.clone())));
        }
        if let Some(vendor) = &java_vendor {
            system_attrs.push(("java_vendor", text(vendor.clone())));
        }
        if let Some(os) = &os {
            system_attrs.push(("os", text(os.clone())));
        }
        if !system_attrs.is_empty() {
            facts.push(fact(
                "minecraft.system",
                system_attrs.clone(),
                Vec::new(),
                0,
                "loader",
            ));
        }
        if let Some(os) = &os {
            facts.push(fact(
                "core.system",
                vec![("os", text(os.clone()))],
                Vec::new(),
                0,
                "loader",
            ));
        }

        if let Some((offset, message)) = fatal_line {
            facts.push(fact(
                "core.log_line",
                vec![
                    ("level", text("error")),
                    ("section", text("fatal")),
                    ("text", text(message)),
                ],
                Vec::new(),
                offset + 1,
                "fatal",
            ));
        }

        let found: BTreeSet<&str> = facts.iter().map(|fact| fact.kind.as_str()).collect();
        let budget = collected.budget_spent;
        let mut coverage = vec![
            Coverage {
                kind: "core.exception".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "core.frame".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "core.dependency_problem".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            looked_for(
                "minecraft.loader",
                loader.is_some(),
                "the log does not name a loader",
                budget,
            ),
            looked_for(
                "minecraft.system",
                !system_attrs.is_empty(),
                "the log does not state its Java version or operating system",
                budget,
            ),
            looked_for(
                "core.process",
                game_version.is_some(),
                "the log does not state the game version",
                budget,
            ),
            Coverage {
                kind: "minecraft.mixin_failure".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "minecraft.duplicate_mod".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "minecraft.loader_problem".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "minecraft.mod_error".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "minecraft.config_error".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "minecraft.world_error".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            Coverage {
                kind: "core.log_line".to_string(),
                state: CoverageState::Checked,
                why: None,
            },
            looked_for(
                "minecraft.mod",
                false,
                "latest.log does not list every installed mod",
                budget,
            ),
        ];
        if budget {
            for entry in &mut coverage {
                if !found.contains(entry.kind.as_str()) {
                    entry.state = CoverageState::Absent;
                    entry.why = Some(
                        "the log was not read to the end: its read budget ran out".to_string(),
                    );
                }
            }
        }

        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: FORMAT.to_string(),
                version: None,
            },
            facts,
            bucket,
            coverage,
            notes: notes_for(&collected, request.limits),
        })
    }
}

fn parse_fatal_exception(
    lines: &[String],
    first: Option<&(usize, String)>,
) -> (Vec<Fact>, Vec<BucketPart>) {
    let Some((start, _)) = first else {
        return (Vec::new(), Vec::new());
    };
    let mut facts = Vec::new();
    let mut depth = 0usize;
    let mut index = 0u32;
    let mut bucket = Vec::new();
    let mut started = false;

    for (offset, line) in lines.iter().enumerate().skip(*start) {
        let message = strip_log_prefix(line);
        if !started {
            if let Some((caused, class, text_message)) = exception_line(message) {
                if !caused {
                    started = true;
                    depth = 0;
                    index = 0;
                    if bucket.is_empty() {
                        bucket.push(BucketPart {
                            scheme: "jvm_log".to_string(),
                            value: class.clone(),
                        });
                    }
                    push_exception(&mut facts, &class, text_message, depth, offset + 1);
                }
            }
            continue;
        }

        if let Some((caused, class, text_message)) = exception_line(message) {
            if caused {
                depth += 1;
            } else {
                depth = 0;
            }
            index = 0;
            push_exception(&mut facts, &class, text_message, depth, offset + 1);
            continue;
        }
        if message.trim_start().starts_with("at ") {
            if let Some(frame) = frame_fact(message, index, offset + 1) {
                facts.push(frame);
                index += 1;
            }
            continue;
        }
        let trimmed = message.trim();
        if trimmed.is_empty() || (trimmed.starts_with("... ") && trimmed.ends_with(" more")) {
            continue;
        }
        break;
    }
    (facts, bucket)
}

fn push_exception(
    facts: &mut Vec<Fact>,
    class: &str,
    message: Option<String>,
    depth: usize,
    line_no: usize,
) {
    facts.push(fact(
        "core.exception",
        vec![
            ("class", text(class.to_string())),
            ("category", text(java_exception_category(class))),
            ("message", text(message.unwrap_or_default())),
            ("depth", number(depth as f64)),
        ],
        vec![reference("class", class.to_string(), Strength::Mention)],
        line_no,
        "fatal",
    ));
}

fn frame_fact(line: &str, index: u32, line_no: usize) -> Option<Fact> {
    let rest = line.trim_start().strip_prefix("at ")?;
    let invocation = rest.split_whitespace().next()?;
    let before_paren = invocation.split('(').next().unwrap_or(invocation);
    let class = class_from_frame(before_paren)?;
    let symbol = before_paren.rsplit('/').next().unwrap_or(before_paren);
    let (source_file, source_line) = source_location(invocation);
    let mut attrs = vec![
        ("index", number(index as f64)),
        ("kind", text("managed")),
        ("symbol_scheme", text("jvm")),
        ("symbol", text(symbol.to_string())),
    ];
    if let Some(file) = &source_file {
        attrs.push(("source_file", text(file.clone())));
    }
    if let Some(line) = source_line {
        attrs.push(("line", number(line)));
    }
    let mut refs = vec![reference("class", class, Strength::Exec)];
    if let Some(jar) = jar_from_frame(line) {
        refs.push(reference("file", jar, Strength::Mention));
    }
    Some(fact("core.frame", attrs, refs, line_no, "fatal"))
}

fn strip_log_prefix(line: &str) -> &str {
    let mut rest = line;
    for _ in 0..3 {
        if !rest.starts_with('[') {
            break;
        }
        let Some(pos) = rest.find("]: ") else {
            break;
        };
        rest = &rest[pos + 3..];
    }
    rest
}

fn exception_line(line: &str) -> Option<(bool, String, Option<String>)> {
    let (caused, rest) = match line.strip_prefix("Caused by: ") {
        Some(rest) => (true, rest),
        None => {
            if let Some(rest) = line.strip_prefix("Suppressed: ") {
                (false, rest)
            } else {
                if line.chars().next().is_some_and(|c| c.is_whitespace()) {
                    return None;
                }
                (false, line)
            }
        }
    };
    let (class_part, message) = match rest.split_once(": ") {
        Some((class, message)) => (class, Some(message.to_string())),
        None => match rest.strip_suffix(':') {
            Some(class) => (class, None),
            None => (rest, None),
        },
    };
    let class = class_part.trim();
    if class.is_empty()
        || class.contains(char::is_whitespace)
        || !class.contains('.')
        || !(class.ends_with("Exception")
            || class.ends_with("Error")
            || class.ends_with("Throwable"))
    {
        return None;
    }
    Some((caused, class.to_string(), message))
}

fn apply_launcher_version(
    token: &str,
    game_version: &mut Option<String>,
    loader: &mut Option<(String, String)>,
) {
    let token = token.trim();
    if let Some(rest) = token.strip_prefix("neoforge-") {
        loader.get_or_insert_with(|| ("neoforge".to_string(), rest.to_string()));
    } else if let Some(rest) = token.strip_prefix("forge-") {
        loader.get_or_insert_with(|| ("forge".to_string(), rest.to_string()));
    } else if token.chars().next().is_some_and(|c| c.is_ascii_digit()) && token.contains('.') {
        game_version.get_or_insert_with(|| token.to_string());
    }
}

fn loader_universal_jar(file: &str) -> Option<(String, String)> {
    let (id, rest) = if let Some(rest) = file.strip_prefix("neoforge-") {
        ("neoforge", rest)
    } else {
        ("forge", file.strip_prefix("forge-")?)
    };
    let version = rest.strip_suffix("-universal.jar")?;
    Some((id.to_string(), version.to_string()))
}

fn fabric_dependency(line: &str) -> Option<(String, String, String, String)> {
    let rest = line.trim_start().strip_prefix('-')?.trim_start();
    let rest = rest.strip_prefix("Mod '")?;
    let (_, rest) = rest.split_once("' (")?;
    let (required_by, rest) = rest.split_once(')')?;
    let rest = rest.trim_start();
    let pos = rest.find(" requires ")?;
    let need = rest[pos + " requires ".len()..].trim();

    let (need, state) = if let Some((need, after)) = need.split_once(", which is ") {
        let state = if after.starts_with("missing") {
            "absent"
        } else if after.starts_with("disabled") {
            "disabled"
        } else if after.starts_with("incompatible") {
            "incompatible"
        } else {
            return None;
        };
        (need, state)
    } else if need.contains(", but only the wrong version is present") {
        (need.split(", but only").next()?, "wrong_version")
    } else {
        return None;
    };

    let (requirement, target) = match need.split_once(" of ") {
        Some((requirement, target)) => (requirement.trim(), target.trim()),
        None => ("any version", need.trim()),
    };
    let missing = target_id(target);
    if missing.is_empty() {
        return None;
    }
    Some((
        state.to_string(),
        required_by.trim().to_string(),
        missing,
        requirement.to_string(),
    ))
}

fn target_id(target: &str) -> String {
    if let Some((_, id)) = target.rsplit_once(" (") {
        if let Some(id) = id.strip_suffix(')') {
            return id.trim().to_string();
        }
    }
    target
        .trim_matches(|c: char| c == '\'' || c == '"' || c == '.' || c == ',')
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_string()
}

fn forge_dependency(line: &str) -> Option<(String, String, String, String)> {
    let rest = line.split("Mod ID: '").nth(1)?;
    let (id, rest) = rest.split_once("', Requested by: '")?;
    let (by, rest) = rest.split_once("', Expected range: '")?;
    let (range, rest) = rest.split_once("', Actual version: '")?;
    let (actual, _) = rest.split_once('\'')?;
    let state = if actual.trim() == "[MISSING]" || actual.trim().is_empty() {
        "absent"
    } else {
        "wrong_version"
    };
    Some((
        state.to_string(),
        by.trim().to_string(),
        id.trim().to_string(),
        range.trim().to_string(),
    ))
}

fn mixin_failure(line: &str) -> Option<(Option<String>, String, String)> {
    if let Some(rest) = line.split("Mixin apply for mod ").nth(1) {
        let (mod_id, rest) = rest.split_once(" failed ")?;
        let (config_and_mixin, rest) = rest.split_once(" -> ")?;
        let config = config_and_mixin.split(':').next()?.trim();
        let target = rest.split(':').next()?.trim();
        return Some((
            Some(mod_id.trim().to_string()),
            config.to_string(),
            target.to_string(),
        ));
    }
    if let Some(rest) = line.split("Mixin apply failed ").nth(1) {
        let (config_and_mixin, rest) = rest.split_once(" -> ")?;
        let config = config_and_mixin.split(':').next()?.trim();
        let target = rest.split(':').next()?.trim();
        let mod_id = config
            .split('.')
            .next()
            .filter(|token| !token.is_empty())
            .map(str::to_string);
        return Some((mod_id, config.to_string(), target.to_string()));
    }
    None
}

fn duplicate_mod(line: &str) -> Option<String> {
    let rest = line.split("Found a duplicate mod ").nth(1)?;
    let id = rest.split_whitespace().next()?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn loader_problem(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("modresolutionexception") || lower.contains("incompatible mod set") {
        return Some("resolution");
    }
    if lower.contains("incompatible mods found") {
        return Some("incompatible");
    }
    if lower.contains("duplicate asm classes found on classpath")
        || lower.contains("duplicate classes found on classpath")
    {
        return Some("duplicate_classpath");
    }
    if lower.starts_with("missing or unsupported mandatory dependencies") {
        return Some("missing_dependency");
    }
    None
}

fn looks_like_mod_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("mod loading error")
        || lower.contains("failed to load mod")
        || lower.contains("mod file") && lower.contains("failed to load")
        || lower.contains("error loading mod")
}

fn looks_like_config_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("failed to load config")
        || lower.contains("could not load config")
        || lower.contains("error loading config")
        || lower.contains("malformed config")
}

fn looks_like_world_error(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("failed to load chunk")
        || lower.contains("exception loading chunk")
        || (lower.contains("corrupt")
            && (lower.contains("chunk")
                || lower.contains("world")
                || lower.contains("region")
                || lower.contains("save")))
}
