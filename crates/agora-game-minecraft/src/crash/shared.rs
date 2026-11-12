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
        || class.ends_with("UnsupportedClassVersionError")
        || class.ends_with("NoSuchMethodError")
        || class.ends_with("NoSuchFieldError")
        || class.ends_with("IllegalAccessError")
        || class.ends_with("AbstractMethodError")
        || class.ends_with("BootstrapMethodError")
        || class.ends_with("UnsatisfiedLinkError")
    {
        "load_failure"
    } else if class.ends_with("AssertionError") {
        "assert"
    } else {
        "managed"
    }
}

/// A file the game needed to create but found in the way, named so the answer can point at it.
pub(crate) fn file_problem(
    exception_class: &str,
    message: &str,
    line: usize,
    section: &str,
) -> Option<Fact> {
    if !exception_class.ends_with("FileAlreadyExistsException") {
        return None;
    }
    let path = message.trim();
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path).trim();
    if name.is_empty() {
        return None;
    }
    Some(fact(
        "minecraft.file_problem",
        vec![
            ("kind", text("already_exists")),
            ("path", text(name.to_string())),
            ("message", text(message.to_string())),
        ],
        vec![reference("file", name.to_string(), Strength::Mention)],
        line,
        section,
    ))
}

/// A class name with the Minecraft colour codes a log may put before it removed (`§7java.lang.X`).
pub(crate) fn plain_class(value: &str) -> &str {
    let mut class = value.trim();
    while let Some(rest) = class.strip_prefix('§') {
        class = match rest.char_indices().next() {
            Some((_, code)) => &rest[code.len_utf8()..],
            None => "",
        };
        class = class.trim();
    }
    class
}

/// The Java version a class-file version needs: class-file 45 is Java 1.1, so 55 is Java 11.
pub(crate) fn class_file_java(message: &str) -> Option<u32> {
    let rest = message.split("class file version ").nth(1)?;
    let number = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    let major: u32 = number.parse().ok()?;
    (major >= 45).then(|| major - 44)
}

/// Whether the message reports a Java failure that belongs to the JVM or the driver rather than
/// to a mod's own code. Each shape is specific enough that it is worth saying even on its own.
pub(crate) fn jvm_problem(
    exception_class: &str,
    message: &str,
    line: usize,
    section: &str,
) -> Option<Fact> {
    let lower = message.to_ascii_lowercase();
    let (kind, detail) = if exception_class.ends_with("UnsupportedClassVersionError") {
        let required = class_file_java(message)?;
        ("class_version", Some(required))
    } else if exception_class.ends_with("UnsatisfiedLinkError") {
        ("native_library", None)
    } else if lower.contains("insufficient memory for the java runtime")
        || lower.contains("the system is out of physical ram or swap space")
        || lower.contains("could not reserve enough space for")
        || lower.contains("invalid maximum heap size")
        || lower.contains("too small maximum heap")
        || lower.contains("specified size exceeds the maximum representable size")
    {
        ("memory_startup", None)
    } else if lower.contains("pixel format not accelerated")
        || lower.contains("glx: failed to create context")
        || lower.contains("couldn't set pixel format")
        || lower.contains("driver does not appear to support opengl")
        || lower.contains("1282: invalid operation")
    {
        ("graphics", None)
    } else if lower.contains("module java.base does not export")
        || lower.contains("nashornscriptenginefactory")
        || lower.contains("lambdametafactory")
        || lower.contains("nosuchfieldexception: ucp")
        || lower.contains(
            "unable to make protected final java.lang.class java.lang.classloader.defineclass",
        )
    {
        ("too_new", None)
    } else if lower.contains("maximum id range exceeded") {
        ("id_limit", None)
    } else {
        return None;
    };

    let mut attrs = vec![("kind", text(kind)), ("message", text(message.to_string()))];
    if let Some(required) = detail {
        attrs.push(("required_java", number(required as f64)));
    }
    Some(fact(
        "minecraft.jvm_problem",
        attrs,
        Vec::new(),
        line,
        section,
    ))
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

/// What a stack frame's `class.method` token names: the text to show as its symbol, and the class
/// that owns it when one survives the decoration around it.
///
/// Fabric writes `knot//a.b.C.m` and a class loader writes `app//a.b.C.m`; Forge writes
/// `TRANSFORMER/name@version/a.b.C.m`; the JDK writes `java.base/java.lang.Thread.run`. A lambda is
/// `a.b.C$$Lambda$12/0x...run` or `a.b.C$$Lambda$12/991528828.apply`, where the part before the
/// `$$Lambda` marker is the class that wrote it. A plain inner class stays `a.b.C$Inner`.
pub(crate) fn frame_symbol_and_class(qualified: &str) -> (String, Option<String>) {
    let qualified = qualified.trim();
    if qualified.is_empty() {
        return (String::new(), None);
    }
    // A class loader is written with a doubled slash; anything before the last one is its prefix.
    let base = qualified
        .rsplit_once("//")
        .map(|(_, rest)| rest)
        .unwrap_or(qualified);
    let lambda = base.find("$$Lambda");
    let symbol = match lambda {
        // The lambda suffix is not a module prefix, so keep the whole token for evidence.
        Some(_) => base.to_string(),
        // A module prefix (`java.base/...`) puts the frame after the last single slash.
        None => base.rsplit('/').next().unwrap_or(base).to_string(),
    };
    let class_part = match lambda {
        Some(at) => &base[..at],
        None => base,
    };
    // A module prefix can sit before a lambda's class too.
    let class_part = class_part.rsplit('/').next().unwrap_or(class_part);
    let class = if lambda.is_some() {
        (!class_part.is_empty() && class_part != "-").then(|| class_part.to_string())
    } else {
        match class_part.rsplit_once('.') {
            Some((class, _method)) if !class.is_empty() && class != "-" => Some(class.to_string()),
            _ => None,
        }
    };
    // A class is never only digits or a bare hex address: those are lambda numbers and addresses,
    // and the frame keeps its symbol without a class ref instead.
    let class = class.filter(|class| !is_numeric_or_hex(class));
    (symbol, class)
}

pub(crate) struct ModCrash {
    phase: &'static str,
    id: String,
    name: Option<String>,
    message: String,
}

impl ModCrash {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn into_fact(self, order: u32, line: usize) -> Fact {
        let mut attrs = vec![
            ("id", text(self.id.clone())),
            ("phase", text(self.phase)),
            ("order", number(order as f64)),
            ("message", text(self.message)),
        ];
        if let Some(name) = self.name {
            attrs.push(("name", text(name)));
        }
        fact(
            "minecraft.mod_crash",
            attrs,
            vec![reference("mod_id", self.id, Strength::Mention)],
            line,
            "mods",
        )
    }
}

/// A loader line that names the mod that threw while it was loading.
pub(crate) fn mod_crash_line(line: &str) -> Option<ModCrash> {
    let trimmed = line.trim_start();
    let message = trimmed.to_string();
    if let Some(rest) = trimmed.strip_prefix("Failure message: ") {
        if let Some((name, id)) = name_and_id(rest.split(" has failed to load correctly").next()?) {
            return Some(ModCrash {
                phase: "load",
                id,
                name: Some(name),
                message,
            });
        }
    }
    if let Some(rest) = trimmed.split("Caught exception from ").nth(1) {
        if let Some((name, id)) = name_and_id(rest) {
            return Some(ModCrash {
                phase: "load",
                id,
                name: Some(name),
                message,
            });
        }
    }
    if let Some(rest) = trimmed.strip_prefix("Failed to create mod instance. ModID: ") {
        let id = rest
            .split_whitespace()
            .next()?
            .trim_end_matches(',')
            .to_string();
        if is_mod_id(&id) {
            return Some(ModCrash {
                phase: "load",
                id,
                name: None,
                message,
            });
        }
    }
    mod_crash_from_message(&message)
}

/// The same lines when they arrive as an exception message, whichever parser read them.
pub(crate) fn mod_crash_from_message(message: &str) -> Option<ModCrash> {
    let trimmed = message.trim();
    let trimmed = trimmed
        .strip_prefix("Suppressed: ")
        .or_else(|| trimmed.strip_prefix("Caused by: "))
        .unwrap_or(trimmed)
        .trim();
    if let Some((stage, id)) = entrypoint_failure(trimmed) {
        return Some(ModCrash {
            phase: "entrypoint",
            id,
            name: Some(stage),
            message: trimmed.to_string(),
        });
    }
    if let Some((name, id)) = trimmed
        .split("Caught exception from ")
        .nth(1)
        .and_then(name_and_id)
    {
        return Some(ModCrash {
            phase: "load",
            id,
            name: Some(name),
            message: trimmed.to_string(),
        });
    }
    if trimmed.contains("has failed to load correctly") {
        if let Some((name, id)) = trimmed
            .split(" has failed to load correctly")
            .next()
            .and_then(name_and_id)
        {
            return Some(ModCrash {
                phase: "load",
                id,
                name: Some(name),
                message: trimmed.to_string(),
            });
        }
    }
    if trimmed.contains("encountered an error during") {
        if let Some((name, id)) = trimmed
            .split("encountered an error during")
            .next()
            .and_then(name_and_id)
        {
            return Some(ModCrash {
                phase: "load",
                id,
                name: Some(name),
                message: trimmed.to_string(),
            });
        }
    }
    None
}

fn entrypoint_failure(message: &str) -> Option<(String, String)> {
    let rest = message
        .split("Could not execute entrypoint stage '")
        .nth(1)?;
    let (stage, rest) = rest.split_once("' due to errors, provided by '")?;
    let (id, _) = rest.split_once('\'')?;
    (is_mod_id(id) && !stage.is_empty()).then(|| (stage.to_string(), id.to_string()))
}

/// `Example Mod (examplemod)`, or a bare name when the loader did not print an id.
fn name_and_id(value: &str) -> Option<(String, String)> {
    let value = value.trim().trim_end_matches(',');
    if let Some((name, id)) = value.rsplit_once(" (") {
        let id = id.trim_end_matches(')').trim();
        if is_mod_id(id) && !name.trim().is_empty() {
            return Some((name.trim().to_string(), id.to_string()));
        }
    }
    None
}

pub(crate) fn is_mod_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// A mixin failure block names the config and, sometimes, the mod that owns it.
pub(crate) fn crash_mixin_line(line: &str, line_no: usize) -> Option<Fact> {
    let rest = line.split("Mixin [").nth(1)?;
    let (inside, _) = rest.split_once(']')?;
    let (config_and_class, named) = match inside.split_once(" from mod ") {
        Some((config, id)) => (config, Some(id.trim().to_string())),
        None => (inside, None),
    };
    let config = config_and_class.split(':').next()?.trim();
    if config.is_empty() {
        return None;
    }
    let owner = named
        .filter(|id| is_mod_id(id))
        .or_else(|| config_mod_id(config));
    Some(fact(
        "minecraft.mixin_failure",
        vec![
            ("severity", text("critical")),
            ("config", text(config.to_string())),
        ],
        owner
            .map(|id| vec![reference("mod_id", id, Strength::Mention)])
            .unwrap_or_default(),
        line_no,
        "mixin",
    ))
}

/// The config's leading token is usually the mod id; skip the generic prefixes a loader writes.
fn config_mod_id(config: &str) -> Option<String> {
    config.split('.').find_map(|token| {
        if matches!(
            token.to_ascii_lowercase().as_str(),
            "mixins" | "mixin" | "fabric" | "forge" | "neoforge" | "quilt" | "json"
        ) {
            None
        } else if is_mod_id(token) {
            Some(token.to_string())
        } else {
            None
        }
    })
}

fn is_numeric_or_hex(text: &str) -> bool {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    !hex.is_empty() && hex.chars().all(|character| character.is_ascii_hexdigit())
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
