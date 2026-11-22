//! Guards for the error-code registry (MASTER_SPEC 32.10).
//!
//! The registry is only worth having if nothing can step around it, so these
//! tests read the sources rather than trusting them:
//!
//! 1. [`declarations_are_unique_and_round_trip`] — one row per string, and
//!    `from_code` is the inverse of `as_str`.
//! 2. [`every_rust_code_string_is_declared`] — every code in non-test Rust
//!    (core, the game packages, the CLI, the desktop's Rust side) is declared.
//!    A helper that builds a `Generic` from a `&str` takes its strings through
//!    this door too, and a typo has nowhere to hide.
//! 3. [`every_code_the_ui_and_cli_match_on_is_declared`] — every code the
//!    desktop's TypeScript and the CLI mention is declared, or is on the short
//!    list of codes that never come from Rust. A code the UI compares against
//!    that nothing emits fails here with its `file:line`, which is how
//!    `ERR_MSA_AUTH_EXPIRED` was found: the desktop released a dialog for a
//!    sign-in failure no code reports.
//! 4. [`plugin_and_provider_error_codes_are_declared`] — the two families built
//!    from `PluginErrorCode` are an exhaustive mapping to declared codes, not
//!    `format!` strings.
//!
//! The scanning rules have regression tests of their own
//! ([`test_code_after_a_test_module_is_still_checked`],
//! [`template_literals_are_read`], [`a_code_built_with_format_is_refused`],
//! [`a_message_that_merely_starts_with_err_is_not_a_code`]), because a guard
//! that quietly stops looking is worse than no guard.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agora_core::error_code::ALL;

/// Strings that look like a code but are not one: a family prefix a caller
/// matches against (`code.starts_with(ERR_NETWORK_HOST)`).
const CODE_PREFIXES: &[&str] = &["ERR_NETWORK_HOST"];

/// Codes the desktop makes up itself, before or instead of an error from Rust.
/// Each is emitted by the frontend and never crosses into a `LauncherError`.
const DESKTOP_ONLY_CODES: &[(&str, &str)] = &[
    (
        "ERR_APPLY",
        "desktop/src/pages/Browse.tsx hands a failed install plan to the install \
         dialog as an error payload of its own; nothing in Rust reports it",
    ),
    (
        "ERR_UNKNOWN",
        "desktop/src/lib/tauri.ts's fallback code for an error payload without one",
    ),
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the crate sits two directories under the repository root")
}

fn files_under(root: &Path, suffixes: &[&str], skip_tests_dir: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                let generated = matches!(name.as_str(), "target" | "node_modules" | "dist" | "gen");
                let test_dir = skip_tests_dir && name == "tests";
                if !generated && !test_dir {
                    queue.push(path);
                }
            } else if suffixes.iter().any(|suffix| name.ends_with(suffix)) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn relative(path: &Path) -> String {
    let root = repo_root();
    path.strip_prefix(&root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// A literal the lexer found: the offset of its opening quote and its value.
struct Literal {
    offset: usize,
    value: String,
    /// `true` for a string (a `"…"`, `r#"…"#` or `b"…"`), `false` for a char
    /// literal — a char is data, but never a code.
    is_string: bool,
}

/// One lexical pass over Rust source: string literals, char literals and
/// comments, so that no later check has to guess where code ends and data
/// begins. Offsets are byte offsets into `text`.
fn lex_rust(text: &str) -> (Vec<bool>, Vec<Literal>) {
    let bytes = text.as_bytes();
    let mut code = vec![true; bytes.len()];
    let mut literals = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        if rest.starts_with("//") {
            let end = rest.find('\n').map_or(bytes.len(), |at| i + at);
            code[i..end].fill(false);
            i = end;
        } else if rest.starts_with("/*") {
            // Block comments nest in Rust.
            let mut depth = 0usize;
            let mut end = i;
            while end < bytes.len() {
                if text[end..].starts_with("/*") {
                    depth += 1;
                    end += 2;
                } else if text[end..].starts_with("*/") {
                    depth -= 1;
                    end += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    end += text[end..].chars().next().map_or(1, char::len_utf8);
                }
            }
            code[i..end].fill(false);
            i = end;
        } else if let Some((end, is_string, value)) = rust_literal_at(text, i) {
            if is_string {
                literals.push(Literal {
                    offset: i,
                    value,
                    is_string,
                });
            }
            code[i..end].fill(false);
            i = end;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    (code, literals)
}

/// The Rust literal that starts at `at`, if one does: `"…"`, `r#"…"#`, `b"…"`,
/// `b'…'`, `'…'`. Returns its end, whether it is a string, and its contents.
fn rust_literal_at(text: &str, at: usize) -> Option<(usize, bool, String)> {
    let bytes = text.as_bytes();
    let (start, is_string, raw_hashes) = match bytes.get(at)? {
        b'"' => (at, true, None),
        b'\'' => (at, false, None),
        b'r' | b'b' => {
            let mut cursor = at + 1;
            let mut hashes = 0usize;
            while bytes.get(cursor) == Some(&b'#') {
                hashes += 1;
                cursor += 1;
            }
            match bytes.get(cursor) {
                Some(b'"') => (cursor, true, Some(hashes)),
                Some(b'\'') if bytes[at] == b'b' => (cursor, false, None),
                _ => return None,
            }
        }
        _ => return None,
    };
    if let Some(hashes) = raw_hashes {
        // A raw string ends at `"` followed by the same number of `#`.
        let mut cursor = start + 1;
        while cursor < bytes.len() {
            if bytes[cursor] == b'"' {
                let closing = bytes[cursor + 1..]
                    .iter()
                    .take_while(|byte| **byte == b'#')
                    .count();
                if closing >= hashes {
                    let end = cursor + 1 + hashes;
                    return Some((end, is_string, text[start + 1..cursor].to_string()));
                }
            }
            cursor += 1;
        }
        return Some((bytes.len(), is_string, String::new()));
    }
    if !is_string {
        // `'\n'`, `'é'`, `b'x'`: a char literal closes after one char, unless
        // the char is escaped. Anything else — `'a` in a lifetime — is code.
        let inner = &text[start + 1..];
        let first = inner.chars().next()?;
        let after = start + 1 + first.len_utf8();
        if first == '\\' {
            let rest = &text[after..];
            let escaped = rest.chars().next()?;
            let closing = after + escaped.len_utf8();
            if bytes.get(closing) != Some(&b'\'') {
                return None;
            }
            return Some((closing + 1, false, String::new()));
        }
        if bytes.get(after) != Some(&b'\'') {
            return None;
        }
        return Some((after + 1, false, first.to_string()));
    }
    // An ordinary string: escaped quotes and backslashes are content.
    let mut cursor = start + 1;
    let mut value = String::new();
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => {
                let escaped = text[cursor + 1..].chars().next()?;
                value.push(escaped);
                cursor += 1 + escaped.len_utf8();
            }
            b'"' => return Some((cursor + 1, true, value)),
            _ => {
                let ch = text[cursor..].chars().next()?;
                value.push(ch);
                cursor += ch.len_utf8();
            }
        }
    }
    Some((bytes.len(), true, value))
}

/// Byte spans of `#[cfg(test)]` items: code only tests run is not what the
/// registry protects, so it is not held to its rules.
///
/// Only the item the attribute decorates is skipped. Braces are counted in code
/// only — a fixture's JSON, a `'\u{7b}'` or a raw string cannot move the end —
/// and the item after a test module is scanned like any other.
fn test_spans(text: &str) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let (code, _) = lex_rust(text);
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_attribute =
            byte_is_code(&code, i) && bytes[i] == b'#' && bytes[i..].starts_with(ATTR.as_bytes());
        if !at_attribute {
            i += 1;
            continue;
        }
        let after = i + ATTR.len();
        let mut token = after;
        while token < bytes.len()
            && !(byte_is_code(&code, token) && matches!(bytes[token], b'{' | b';'))
        {
            token += 1;
        }
        if token == bytes.len() {
            break;
        }
        if bytes[token] == b';' {
            spans.push((i, token + 1));
            i = token + 1;
            continue;
        }
        let mut depth = 0usize;
        let mut end = token;
        while end < bytes.len() {
            if byte_is_code(&code, end) {
                match bytes[end] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            end += 1;
        }
        spans.push((i, (end + 1).min(bytes.len())));
        i = end + 1;
    }
    spans
}

fn byte_is_code(code: &[bool], at: usize) -> bool {
    code.get(at).copied().unwrap_or(false)
}

fn inside(spans: &[(usize, usize)], offset: usize) -> bool {
    spans
        .iter()
        .any(|(start, end)| *start <= offset && offset < *end)
}

/// How a string literal in non-test Rust is read.
#[derive(Debug, PartialEq, Eq)]
enum CodeLiteral {
    /// A whole code: `"ERR_HASH_MISMATCH"`.
    Code(String),
    /// A `format!` template that builds codes at run time: `"ERR_PLUGIN_{:?}"`.
    /// These are refused — the mapping is enumerated instead, the way
    /// [`agora_core::error_code::ErrorCode::for_plugin_error`] is.
    Family(String),
    /// Text that happens to begin with `ERR_` — a log message, not a code.
    Message,
}

fn is_code_shaped(text: &str) -> bool {
    text.strip_prefix("ERR_").is_some_and(|tail| {
        !tail.is_empty()
            && tail
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    })
}

fn classify_literal(value: &str) -> CodeLiteral {
    if !value.starts_with("ERR_") {
        return CodeLiteral::Message;
    }
    if is_code_shaped(value) {
        return CodeLiteral::Code(value.to_string());
    }
    let stem = value
        .split('{')
        .next()
        .unwrap_or(value)
        .trim_end_matches('_');
    if is_code_shaped(stem) {
        return CodeLiteral::Family(value.to_string());
    }
    CodeLiteral::Message
}

/// Every code, code family and message-shaped literal in non-test code.
fn rust_code_literals(text: &str) -> Vec<(usize, CodeLiteral)> {
    let (_, literals) = lex_rust(text);
    let spans = test_spans(text);
    literals
        .iter()
        .filter(|literal| literal.is_string && !inside(&spans, literal.offset))
        .map(|literal| (literal.offset, classify_literal(&literal.value)))
        .filter(|(_, literal)| !matches!(literal, CodeLiteral::Message))
        .collect()
}

/// Every code-shaped literal in TypeScript, quoted with `"`, `'` or a
/// template literal's backtick.
fn typescript_code_literals(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap_or(' ');
        if !matches!(ch, '"' | '\'' | '`') {
            i += ch.len_utf8();
            continue;
        }
        let start = i;
        let mut value = String::new();
        i += ch.len_utf8();
        while i < text.len() {
            if text[i..].starts_with('\\') {
                i += 1;
                let escaped = text[i..].chars().next().unwrap_or(' ');
                value.push(escaped);
                i += escaped.len_utf8();
                continue;
            }
            let inner = text[i..].chars().next().unwrap_or(' ');
            i += inner.len_utf8();
            if inner == ch {
                break;
            }
            value.push(inner);
        }
        if classify_literal(&value) == CodeLiteral::Code(value.clone()) {
            found.push((start, value));
        }
    }
    found
}

fn declared() -> BTreeSet<&'static str> {
    ALL.iter().map(|code| code.as_str()).collect()
}

#[test]
fn declarations_are_unique_and_round_trip() {
    let mut seen = BTreeSet::new();
    for code in ALL {
        assert!(
            code.as_str().starts_with("ERR_"),
            "{} is not shaped like a machine code",
            code.as_str()
        );
        assert!(
            seen.insert(code.as_str()),
            "{} is declared twice",
            code.as_str()
        );
        assert_eq!(
            agora_core::error_code::ErrorCode::from_code(code.as_str()),
            Some(*code),
            "{} does not round-trip through from_code",
            code.as_str()
        );
        assert_ne!(
            code.exit_code(),
            0,
            "{} claims the exit code of success",
            code.as_str()
        );
    }
    assert_eq!(
        agora_core::error_code::ErrorCode::from_code("ERR_NOT_A_CODE"),
        None,
        "an undeclared string must not resolve to a code"
    );
    assert_eq!(
        agora_core::error_code::ErrorCode::from_code("err_network_offline"),
        None,
        "codes are exact strings, not case-insensitive ones"
    );
}

#[test]
fn every_rust_code_string_is_declared() {
    let root = repo_root();
    let declared = declared();
    let mut files = files_under(&root.join("crates"), &[".rs"], true);
    files.extend(files_under(
        &root.join("desktop").join("src-tauri").join("src"),
        &[".rs"],
        true,
    ));

    let mut undeclared = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        for (offset, literal) in rust_code_literals(&source) {
            match literal {
                CodeLiteral::Code(code) => {
                    if CODE_PREFIXES.contains(&code.as_str()) || declared.contains(&code.as_str()) {
                        continue;
                    }
                    undeclared.push(format!(
                        "{}:{}: `{code}` is not a declared code",
                        relative(&file),
                        line_of(&source, offset)
                    ));
                }
                CodeLiteral::Family(family) => undeclared.push(format!(
                    "{}:{}: `{family}` builds codes with `format!`; enumerate the mapping \
                     (see `ErrorCode::for_plugin_error`) so each code is declared",
                    relative(&file),
                    line_of(&source, offset)
                )),
                CodeLiteral::Message => {}
            }
        }
    }

    assert!(
        undeclared.is_empty(),
        "these code strings are used but declared nowhere; add a row to \
         agora_core::error_code or stop emitting the string:\n{}",
        undeclared.join("\n")
    );
}

#[test]
fn every_code_the_ui_and_cli_match_on_is_declared() {
    let root = repo_root();
    let declared = declared();
    let desktop_only: BTreeSet<&str> = DESKTOP_ONLY_CODES.iter().map(|(code, _)| *code).collect();

    // The desktop's TypeScript, minus its own tests (a mock's invented code
    // proves nothing about what ships).
    let mut sources: Vec<PathBuf> =
        files_under(&root.join("desktop").join("src"), &[".ts", ".tsx"], false)
            .into_iter()
            .filter(|path| {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                !name.contains(".test.") && !name.contains(".spec.")
            })
            .collect();
    // The CLI's checks, in one place for the same reason.
    sources.extend(files_under(
        &root.join("crates").join("agora").join("src"),
        &[".rs"],
        true,
    ));

    let mut unknown = Vec::new();
    for file in sources {
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        let is_typescript = file.extension().is_some_and(|ext| ext != "rs");
        let found: Vec<(usize, String)> = if is_typescript {
            typescript_code_literals(&source)
        } else {
            rust_code_literals(&source)
                .into_iter()
                .filter_map(|(offset, literal)| match literal {
                    CodeLiteral::Code(code) => Some((offset, code)),
                    _ => None,
                })
                .collect()
        };
        for (offset, code) in found {
            if declared.contains(code.as_str())
                || desktop_only.contains(code.as_str())
                || CODE_PREFIXES.contains(&code.as_str())
            {
                continue;
            }
            unknown.push(format!(
                "{}:{}: `{code}` is compared against but nothing declares it",
                relative(&file),
                line_of(&source, offset)
            ));
        }
    }

    assert!(
        unknown.is_empty(),
        "these codes are matched on but no code reports them; either emit and \
         declare one, or stop matching on it:\n{}",
        unknown.join("\n")
    );
}

/// The plugin and provider families come from one enum, so they are one
/// exhaustive mapping each: adding a variant stops this compiling until its
/// code is declared, and the spelling stays what `format!` produced before
/// (`ERR_PLUGIN_NOTACTIVATED`, no separator between the words of a kind).
#[test]
fn plugin_and_provider_error_codes_are_declared() {
    use agora_core::error_code::ErrorCode;
    use agora_plugin_api::error::PluginErrorCode as Kind;

    fn spelled(prefix: &str, kind: Kind) -> String {
        let debug = match kind {
            Kind::InvalidManifest => "InvalidManifest",
            Kind::InvalidPackage => "InvalidPackage",
            Kind::IncompatibleApi => "IncompatibleApi",
            Kind::UnresolvedDependency => "UnresolvedDependency",
            Kind::DependencyCycle => "DependencyCycle",
            Kind::DuplicateContribution => "DuplicateContribution",
            Kind::CapabilityDenied => "CapabilityDenied",
            Kind::UnknownMethod => "UnknownMethod",
            Kind::InvalidArguments => "InvalidArguments",
            Kind::NotActivated => "NotActivated",
            Kind::ScriptError => "ScriptError",
            Kind::Timeout => "Timeout",
            Kind::Cancelled => "Cancelled",
            Kind::ResourceExhausted => "ResourceExhausted",
            Kind::NetworkDenied => "NetworkDenied",
            Kind::Conflict => "Conflict",
            Kind::OperationFailed => "OperationFailed",
            Kind::Internal => "Internal",
            Kind::InvalidResponse => "InvalidResponse",
        };
        format!("{prefix}_{}", debug.to_uppercase())
    }

    let kinds = [
        Kind::InvalidManifest,
        Kind::InvalidPackage,
        Kind::IncompatibleApi,
        Kind::UnresolvedDependency,
        Kind::DependencyCycle,
        Kind::DuplicateContribution,
        Kind::CapabilityDenied,
        Kind::UnknownMethod,
        Kind::InvalidArguments,
        Kind::NotActivated,
        Kind::ScriptError,
        Kind::Timeout,
        Kind::Cancelled,
        Kind::ResourceExhausted,
        Kind::NetworkDenied,
        Kind::Conflict,
        Kind::OperationFailed,
        Kind::Internal,
        Kind::InvalidResponse,
    ];
    for kind in kinds {
        let plugin = ErrorCode::for_plugin_error(kind);
        let provider = ErrorCode::for_provider_error(kind);
        assert_eq!(
            plugin.as_str(),
            spelled("ERR_PLUGIN", kind),
            "a plugin error kind must keep the code it reported before"
        );
        assert_eq!(
            provider.as_str(),
            spelled("ERR_PROVIDER", kind),
            "a provider error kind must keep the code it reported before"
        );
        assert_eq!(
            agora_core::error_code::ErrorCode::from_code(plugin.as_str()),
            Some(plugin),
            "{} is not declared",
            plugin.as_str()
        );
        assert_eq!(
            agora_core::error_code::ErrorCode::from_code(provider.as_str()),
            Some(provider),
            "{} is not declared",
            provider.as_str()
        );
    }
}

/// The scanner skips the item a `#[cfg(test)]` decorates and nothing else: a
/// test module in the middle of a file must not hide the code after it.
#[test]
fn test_code_after_a_test_module_is_still_checked() {
    let source = r#"
fn before() -> LauncherError { LauncherError::Generic { code: "ERR_BEFORE_THE_TESTS".into(), message: String::new() } }

#[cfg(test)]
mod tests {
    fn a_test() {
        assert_eq!(
            LauncherError::Generic { code: "ERR_ONLY_A_TEST".into(), message: String::new() }.code(),
            "ERR_ONLY_A_TEST"
        );
    }
}

fn after() -> LauncherError { LauncherError::Generic { code: "ERR_AFTER_THE_TESTS".into(), message: String::new() } }
"#;
    let codes: Vec<String> = rust_code_literals(source)
        .into_iter()
        .filter_map(|(_, literal)| match literal {
            CodeLiteral::Code(code) => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(
        codes,
        vec![
            "ERR_BEFORE_THE_TESTS".to_string(),
            "ERR_AFTER_THE_TESTS".to_string()
        ],
        "only the test module's own body may be skipped"
    );
}

/// A test module's own braces must be found even when its body holds data with
/// braces in it — a fixture, a `'{'`, a raw string.
#[test]
fn test_span_survives_braces_in_test_data() {
    let source = r##"
#[cfg(test)]
mod tests {
    fn a_test() {
        let json = r#"{"a": 1, "b": "}"}"#;
        let brace = '{';
        let nested = "{{}}";
        assert!(json.contains(&brace.to_string()) || !nested.is_empty());
    }
}

fn after() -> LauncherError { LauncherError::Generic { code: "ERR_AFTER_BRACES".into(), message: String::new() } }
"##;
    let codes: Vec<String> = rust_code_literals(source)
        .into_iter()
        .filter_map(|(_, literal)| match literal {
            CodeLiteral::Code(code) => Some(code),
            _ => None,
        })
        .collect();
    assert_eq!(
        codes,
        vec!["ERR_AFTER_BRACES".to_string()],
        "the test module must end where its braces end, whatever its data holds"
    );
}

/// The desktop writes codes in backticks as well as quotes.
#[test]
fn template_literals_are_read() {
    let source = r#"
const a = parsed.code === `ERR_IN_TICKS`;
const b = parsed.code === 'ERR_IN_SINGLE';
const c = parsed.code === "ERR_IN_DOUBLE";
const d = `not a code: error in ticks`;
"#;
    let codes: Vec<String> = typescript_code_literals(source)
        .into_iter()
        .map(|(_, code)| code)
        .collect();
    assert_eq!(
        codes,
        vec![
            "ERR_IN_TICKS".to_string(),
            "ERR_IN_SINGLE".to_string(),
            "ERR_IN_DOUBLE".to_string()
        ]
    );
}

/// A `format!` that builds a code is refused rather than skipped: the mapping
/// has to be enumerated so every code it can produce is declared.
#[test]
fn a_code_built_with_format_is_refused() {
    let source = r#"
fn build(kind: Kind) -> LauncherError {
    LauncherError::Generic { code: format!("ERR_PLUGIN_{:?}", kind).to_uppercase(), message: String::new() }
}
"#;
    let found = rust_code_literals(source);
    assert_eq!(found.len(), 1, "the template must be reported, not skipped");
    assert_eq!(
        found[0].1,
        CodeLiteral::Family("ERR_PLUGIN_{:?}".to_string())
    );
}

/// A message that merely begins with `ERR_` is text, not a code: the scan looks
/// for whole codes, so logging a code-shaped word is not a code site.
#[test]
fn a_message_that_merely_starts_with_err_is_not_a_code() {
    let source = r#"
fn log_it() {
    eprintln!("ERR_HASH_MISMATCH while installing sodium.jar");
    eprintln!("ERR_ while we do not know the code yet");
}
"#;
    assert!(
        rust_code_literals(source).is_empty(),
        "a sentence that starts with a code is a message"
    );
}
