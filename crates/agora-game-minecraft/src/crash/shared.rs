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

/// A mixin failure: the config it named, the class it tried to change, and the mod that owns it.
pub(crate) struct MixinFailure {
    pub config: String,
    pub target: Option<String>,
    pub owner: Option<String>,
}

/// A line from any loader's mixin failure shapes. The mod's own config and its target are kept as
/// separate refs so a pack can map either to a mod, and the config's owner ranks first.
pub(crate) fn mixin_failure_line(line: &str) -> Option<MixinFailure> {
    let lower = line.to_ascii_lowercase();
    let is_failure = lower.contains("mixin apply")
        || lower.contains("mixin prepare")
        || lower.contains("mixin [")
        || lower.contains("failed injection")
        || lower.contains("injection failure")
        || lower.contains("was loaded too early")
        || (lower.contains(" from mod ")
            && (lower.contains(".mixins.json") || lower.contains(".mixin.json"))
            && lower.contains("failed"));
    if !is_failure {
        return None;
    }
    let config = mixin_config_token(line)?;
    let target = mixin_target_token(line);
    if lower.contains("was loaded too early") {
        // "Target already loaded" is worth naming only when a second mod is named too. A platform
        // target means the line names one mod, and blaming it there is a guess.
        let target = target.as_deref()?;
        if is_platform_or_framework_class(target) {
            return None;
        }
    }
    let owner = named_mixin_owner(line).or_else(|| config_mod_id(&config));
    Some(MixinFailure {
        config,
        target,
        owner,
    })
}

/// A mixin failure fact. The owner (when one is named) comes first, then the config, then the
/// target class, so a pack can bind any of them.
pub(crate) fn mixin_failure_fact(
    failure: MixinFailure,
    line_no: usize,
    owner_strength: Strength,
) -> Fact {
    let mut attrs = vec![
        ("severity", text("critical")),
        ("config", text(failure.config.clone())),
    ];
    if let Some(target) = &failure.target {
        attrs.push(("target", text(target.clone())));
    }
    let mut refs = Vec::new();
    if let Some(owner) = &failure.owner {
        refs.push(reference("mod_id", owner.clone(), owner_strength));
    }
    refs.push(reference(
        "mixin_config",
        mixin_config_value(&failure.config),
        Strength::Mention,
    ));
    if let Some(target) = &failure.target {
        if looks_like_class(target) {
            refs.push(reference("class", target.clone(), Strength::Mention));
        }
    }
    fact("minecraft.mixin_failure", attrs, refs, line_no, "mixin")
}

/// A mixin failure block names the config and, sometimes, the mod that owns it.
pub(crate) fn crash_mixin_line(line: &str, line_no: usize) -> Option<Fact> {
    Some(mixin_failure_fact(
        mixin_failure_line(line)?,
        line_no,
        Strength::Mention,
    ))
}

/// The first token on the line that looks like a mixin config (`a.mixins.json`, `a-common.mixins.json`,
/// `mixins.a.json`), without its `:MixinClass` suffix.
fn mixin_config_token(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|raw| {
        let token = raw.trim_matches(|character: char| {
            matches!(
                character,
                '[' | ']' | '(' | ')' | ',' | ';' | '\'' | '"' | '<' | '>'
            )
        });
        let base = token.split(':').next().unwrap_or(token);
        is_mixin_config(base).then(|| base.to_string())
    })
}

fn is_mixin_config(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.ends_with(".mixins.json")
        || lower.ends_with(".mixin.json")
        || (lower.starts_with("mixins.") && lower.ends_with(".json"))
        || (lower.starts_with("mixin.") && lower.ends_with(".json"))
}

/// The class a failure names as its target, from either `target <class>` or `-> <class>:`.
fn mixin_target_token(line: &str) -> Option<String> {
    let raw = if let Some(rest) = line.split(" target ").nth(1) {
        rest.split_whitespace().next()?
    } else {
        line.split(" -> ").nth(1)?.split(':').next()?.trim()
    };
    let target = raw.trim_matches(|character: char| {
        matches!(character, '[' | ']' | '(' | ')' | ',' | ';' | '\'' | '"')
    });
    (!target.is_empty() && looks_like_class(target)).then(|| target.to_string())
}

fn named_mixin_owner(line: &str) -> Option<String> {
    let inside_brackets = line
        .split("Mixin [")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .and_then(|inside| inside.split(" from mod ").nth(1));
    let candidate = inside_brackets
        .or_else(|| line.split(" from mod ").nth(1))
        .or_else(|| line.split("Mixin apply for mod ").nth(1))
        .or_else(|| line.split("Mixin prepare for mod ").nth(1))?;
    let id = candidate
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ',' | ']' | ')' | ';')
        })
        .next()?
        .trim();
    is_mod_id(id).then(|| id.to_string())
}

fn looks_like_class(value: &str) -> bool {
    value.contains('.') || value.contains('/')
}

fn is_platform_or_framework_class(value: &str) -> bool {
    let value = value.replace('/', ".");
    [
        "java.",
        "javax.",
        "jdk.",
        "sun.",
        "net.minecraft.",
        "net.fabricmc.",
        "net.minecraftforge.",
        "net.neoforged.",
        "com.mojang.",
        "com.google.",
        "org.apache.",
        "org.spongepowered.",
        "org.objectweb.",
        "org.slf4j.",
        "io.netty.",
        "it.unimi.",
        "com.llamalad7.",
        "kotlin.",
        "scala.",
        "org.lwjgl.",
    ]
    .iter()
    .any(|prefix| value.starts_with(prefix))
}

/// A config's stem with the separators a mod id may use turned into dots, so the pack's `segment`
/// matcher sees `examplemod` in `examplemod-common.mixins.json`.
fn mixin_config_value(config: &str) -> String {
    config.replace(['-', '_'], ".")
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

/// Classes an exception's message names, when that is the only place a mod appears. Only shapes
/// specific enough to mean something are read: a mixin error whose message is a class, and a
/// missing-member error, where the message names the class that expected the member.
pub(crate) fn named_message_classes(exception_class: &str, message: &str) -> Vec<String> {
    if exception_class.ends_with("ClassMetadataNotFoundException") {
        return class_tokens(message);
    }
    if exception_class.ends_with("AbstractMethodError") {
        if let Some(rest) = message.split("of abstract class ").nth(1) {
            let class = rest
                .split_whitespace()
                .next()
                .unwrap_or(rest)
                .trim_end_matches('.');
            if looks_like_class(class) {
                return vec![class.to_string()];
            }
        }
    }
    if exception_class.ends_with("NoSuchMethodError")
        || exception_class.ends_with("NoSuchFieldError")
        || exception_class.ends_with("IllegalAccessError")
    {
        return class_tokens(message);
    }
    if exception_class.ends_with("NullPointerException") {
        return quoted_class_paths(message);
    }
    Vec::new()
}

/// Class paths the JVM quotes in a null-pointer message, such as the owner of a null field. The
/// last part is the member, not the class, so it is dropped.
fn quoted_class_paths(message: &str) -> Vec<String> {
    let mut classes = Vec::new();
    for piece in message.split('"') {
        let token = piece.trim().trim_end_matches('.');
        let token = token.split('(').next().unwrap_or(token).trim();
        if !token.contains('.')
            || !token
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_lowercase())
        {
            continue;
        }
        let Some((class, _member)) = token.rsplit_once('.') else {
            continue;
        };
        if class.split('.').count() >= 3 && !is_platform_or_framework_class(class) {
            let class = class.to_string();
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
    }
    classes
}

fn fact_text<'a>(fact: &'a Fact, attr: &str) -> Option<&'a str> {
    match fact.attrs.get(attr) {
        Some(AttrValue::Scalar(Scalar::Text(text))) => Some(text),
        _ => None,
    }
}

fn class_tokens(message: &str) -> Vec<String> {
    let mut classes = Vec::new();
    for raw in message.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '$'))
    }) {
        let token = raw.trim_matches(|character: char| matches!(character, '.' | '$'));
        let segments = token.split('.').count();
        let starts_lower = token
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase());
        if segments >= 3 && starts_lower && !is_platform_or_framework_class(token) {
            let class = token.to_string();
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
    }
    classes
}

/// A Forge 1.7-1.12 `LoaderException` that fails because one mod's class is missing from another's
/// package. The loader names the mod that failed and the missing class; this turns that into the
/// same dependency fact an explicit "missing mods" line would produce.
pub(crate) fn loader_dependency_problem(facts: &[Fact]) -> Option<Fact> {
    let exception = |fact: &Fact| {
        fact.kind == "core.exception"
            && fact_text(fact, "class").is_some_and(|class| class.ends_with("LoaderException"))
    };
    let loader = facts.iter().find(|fact| {
        exception(fact)
            && fact_text(fact, "message")
                .is_some_and(|message| message.contains("NoClassDefFoundError"))
    })?;
    let needed_class = fact_text(loader, "message")
        .and_then(|message| message.split("NoClassDefFoundError:").nth(1))
        .and_then(descriptor_class)?;
    let missing = facts
        .iter()
        .filter(|fact| {
            fact.kind == "core.exception"
                && fact_text(fact, "class")
                    .is_some_and(|class| class.ends_with("ClassNotFoundException"))
        })
        .max_by(|a, b| depth(a).total_cmp(&depth(b)))?;
    let missing_class = fact_text(missing, "message")?;
    let missing_id = package_mod_id(missing_class)?;
    let needed_by = package_mod_id(&needed_class)?;
    if missing_id == needed_by {
        return None;
    }
    let line = loader
        .span
        .as_ref()
        .and_then(|span| span.line_start)
        .unwrap_or(0) as usize;
    Some(fact(
        "core.dependency_problem",
        vec![
            ("state", text("absent")),
            ("requirement", text("any version")),
            ("needed_by", text(needed_by.clone())),
        ],
        vec![
            reference("mod_id", missing_id, Strength::Mention),
            reference("needed_by", needed_by, Strength::Mention),
        ],
        line,
        "fatal",
    ))
}

fn descriptor_class(message: &str) -> Option<String> {
    let descriptor = message.split_whitespace().next()?.trim();
    let descriptor = descriptor.strip_prefix('L')?.trim_end_matches(';');
    let class = descriptor.replace('/', ".");
    looks_like_class(&class).then_some(class)
}

fn package_mod_id(class: &str) -> Option<String> {
    let first = class.split('.').next()?.trim();
    let reserved = matches!(
        first.to_ascii_lowercase().as_str(),
        "net" | "com" | "org" | "java" | "javax" | "jdk" | "sun" | "io" | "me" | "cpw" | "lwjgl"
    );
    (is_mod_id(first) && !reserved).then(|| first.to_string())
}

fn depth(fact: &Fact) -> f64 {
    match fact.attrs.get("depth") {
        Some(AttrValue::Scalar(Scalar::Number(depth))) => *depth,
        _ => 0.0,
    }
}

/// A fatal JSON error that names a mod's data namespace. Only the fatal exception chain reaches
/// here, so an unrelated warning earlier in a long log cannot accuse a mod.
pub(crate) fn data_error_fact(
    exception_class: &str,
    message: &str,
    line: usize,
    section: &str,
) -> Option<Fact> {
    let json = exception_class.ends_with("JsonSyntaxException")
        || exception_class.ends_with("JsonParseException")
        || exception_class.ends_with("MalformedJsonException");
    if !json {
        return None;
    }
    let namespace = data_namespace(message)?;
    Some(fact(
        "minecraft.data_error",
        vec![
            ("namespace", text(namespace.clone())),
            ("message", text(message.to_string())),
        ],
        vec![reference("mod_id", namespace, Strength::Mention)],
        line,
        section,
    ))
}

fn data_namespace(message: &str) -> Option<String> {
    if let Some(rest) = message.split("data/").nth(1) {
        if let Some(namespace) = rest.split('/').next() {
            if is_data_namespace(namespace) {
                return Some(namespace.to_string());
            }
        }
    }
    for token in message.split_whitespace() {
        let token = token.trim_matches(|character: char| {
            matches!(
                character,
                '\'' | '"' | ',' | ';' | '(' | ')' | '[' | ']' | '.'
            )
        });
        let Some((namespace, rest)) = token.split_once(':') else {
            continue;
        };
        if rest.contains('/') && is_data_namespace(namespace) {
            return Some(namespace.to_string());
        }
    }
    None
}

fn is_data_namespace(namespace: &str) -> bool {
    is_mod_id(namespace)
        && !matches!(
            namespace.to_ascii_lowercase().as_str(),
            "minecraft" | "forge" | "neoforge"
        )
}

fn is_numeric_or_hex(text: &str) -> bool {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    !hex.is_empty() && hex.chars().all(|character| character.is_ascii_hexdigit())
}

/// One Fabric/Quilt unmet-dependency line: `Mod 'Name' (id) version requires ... of
/// other, which is missing!`. It returns (state, required_by, missing, requirement), the same
/// shape whether the line came from `latest.log` or a crash report's unmet listing.
pub(crate) fn fabric_dependency(line: &str) -> Option<(String, String, String, String)> {
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

/// A loader failure that stopped the game from resolving its mod set. Both a log and a crash
/// report carry these messages, so both readers use this.
pub(crate) fn loader_problem(line: &str) -> Option<&'static str> {
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
    if lower.contains("maximum id range exceeded") {
        return Some("id_limit");
    }
    None
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
