//! Guards for the error-code registry (MASTER_SPEC 32.10).
//!
//! The registry is only worth having if nothing can step around it, so these
//! tests read the sources rather than trusting them:
//!
//! 1. [`declarations_are_unique_and_round_trip`] — one row per string, and
//!    `from_code` is the inverse of `as_str`.
//! 2. [`every_rust_code_string_is_declared`] — every `ERR_…` string literal in
//!    non-test Rust (core, the game packages, the CLI, the desktop's Rust side)
//!    is a declared code. A helper that builds a `Generic` from a `&str` takes
//!    its strings through this door too, and a typo has nowhere to hide.
//! 3. [`every_code_the_ui_and_cli_match_on_is_declared`] — every code the
//!    desktop's TypeScript and the CLI mention is declared, or is on the short
//!    list of codes that never come from Rust. A code the UI compares against
//!    that nothing emits fails here with its `file:line`, which is how
//!    `ERR_MSA_AUTH_EXPIRED` was found: the desktop released a dialog for a
//!    sign-in failure no code reports.

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

/// Source with comments replaced by spaces, so a code named in a comment is not
/// mistaken for one in the code. Offsets are preserved, so a match can still be
/// reported at its line.
fn without_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let mut depth = 0;
            while i < bytes.len() {
                if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Which bytes of `text` are code rather than the inside of a string, char
/// literal or raw string. Braces and quotes in data must not move a span.
fn code_mask(text: &str) -> Vec<bool> {
    let bytes = text.as_bytes();
    let mut code = vec![true; bytes.len()];
    let mut i = 0;
    while i < bytes.len() {
        let mut end = i + 1;
        match bytes[i] {
            b'"' => {
                end = i + 1;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                end = (end + 1).min(bytes.len());
            }
            b'\'' => {
                // `'\n'` and `'é'` are char literals; `'a` is a lifetime, and
                // only a char literal may be masked.
                let closing = if bytes.get(i + 1) == Some(&b'\\') {
                    let mut j = i + 2;
                    while j < bytes.len() && j < i + 12 && bytes[j] != b'\'' {
                        j += 1;
                    }
                    bytes.get(j).filter(|byte| **byte == b'\'').map(|_| j)
                } else {
                    // One char, however wide, then the closing quote.
                    text[i + 1..]
                        .chars()
                        .next()
                        .map(|ch| i + 1 + ch.len_utf8())
                        .filter(|after| bytes.get(*after) == Some(&b'\''))
                };
                end = match closing {
                    Some(closing) => closing + 1,
                    None => i + 1,
                };
            }
            b'r' if matches!(bytes.get(i + 1), Some(b'"') | Some(b'#')) => {
                let mut hashes = 0usize;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                end = if bytes.get(j) == Some(&b'"') {
                    j += 1;
                    loop {
                        if j >= bytes.len() {
                            break j;
                        }
                        if bytes[j] == b'"' {
                            let mut closing = 0usize;
                            let mut k = j + 1;
                            while k < bytes.len() && bytes[k] == b'#' && closing < hashes {
                                closing += 1;
                                k += 1;
                            }
                            if closing == hashes {
                                break k;
                            }
                        }
                        j += 1;
                    }
                } else {
                    i + 1
                };
            }
            _ => {}
        }
        // Only a literal is masked: a lifetime or an ordinary byte advances by
        // one and stays code.
        if end > i + 1 {
            for byte in &mut code[i..end.min(bytes.len())] {
                *byte = false;
            }
        }
        i = end.max(i + 1);
    }
    code
}
/// Byte spans of `#[cfg(test)]` items: code only tests run is not what the
/// registry protects, so it is not held to its rules.
///
/// Spans are found from the attribute to the end of the item it decorates, and
/// the brace count skips braces inside strings, char literals and raw strings —
/// a fixture's JSON would otherwise end a test module at the wrong place.
fn test_spans(text: &str) -> Vec<(usize, usize)> {
    const ATTR: &str = "#[cfg(test)]";
    let bytes = text.as_bytes();
    let code = code_mask(text);
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_attribute = code[i] && bytes[i] == b'#' && bytes[i..].starts_with(ATTR.as_bytes());
        if !at_attribute {
            i += 1;
            continue;
        }
        let after = i + ATTR.len();
        let mut token = after;
        while token < bytes.len() && !(code[token] && matches!(bytes[token], b'{' | b';')) {
            token += 1;
        }
        if token == bytes.len() {
            break;
        }
        // A test module runs to the end of the file: it is the last item in its
        // file, and a brace count inside it can be ended early by something
        // this scanner does not know (a nested raw string, a macro's braces).
        let head = &text[after..token];
        if let Some(at) = head.find("mod ") {
            if !head[..at].contains('}') {
                spans.push((i, bytes.len()));
                break;
            }
        }
        if bytes[token] == b';' {
            spans.push((i, token + 1));
            i = token + 1;
            continue;
        }
        let mut depth = 0usize;
        let mut end = token;
        while end < bytes.len() {
            if code[end] {
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

fn inside(spans: &[(usize, usize)], offset: usize) -> bool {
    spans
        .iter()
        .any(|(start, end)| *start <= offset && offset < *end)
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// Every quoted literal in `text`, with the offset of its opening quote.
///
/// `single_quotes` is for the TypeScript side, where `'ERR_…'` is as common as
/// `"ERR_…"`; Rust's `'` starts a lifetime or a char literal, so it is not read
/// as a quote there.
fn string_literals(text: &str, single_quotes: bool) -> Vec<(usize, String)> {
    let bytes = text.as_bytes();
    let mut literals = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let opens = bytes[i] == b'"' || (single_quotes && bytes[i] == b'\'');
        if !opens {
            i += 1;
            continue;
        }
        let close_quote = bytes[i];
        let start = i;
        let mut value = String::new();
        i += 1;
        while i < bytes.len() && bytes[i] != close_quote {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                value.push(bytes[i + 1] as char);
                i += 2;
            } else {
                value.push(bytes[i] as char);
                i += 1;
            }
        }
        i += 1;
        literals.push((start, value));
    }
    literals
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
        let text = without_comments(&source);
        let spans = test_spans(&text);
        for (offset, literal) in string_literals(&text, false) {
            if inside(&spans, offset) || !literal.starts_with("ERR_") {
                continue;
            }
            // A `format!` template builds a family of codes at run time; the
            // families and who owns them are listed in `dynamic_code_families`.
            if literal.contains('{') || CODE_PREFIXES.contains(&literal.as_str()) {
                continue;
            }
            if !declared.contains(literal.as_str()) {
                undeclared.push(format!(
                    "{}:{}: `{literal}` is not a declared code",
                    relative(&file),
                    line_of(&text, offset)
                ));
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

/// Code families built by `format!`, which no table can enumerate. Each is
/// listed with the reason it is not a fixed declaration (MASTER_SPEC 32.10):
///
/// - `ERR_PLUGIN_<KIND>` (`crates/agora-core/src/providers/plugin.rs`,
///   `desktop/src-tauri/src/plugins.rs`, `crates/agora/src/cli_misc_parity.rs`):
///   one code per plugin error kind, named after the kind's `Debug` form. The
///   kinds core itself meets have their own `ERR_PLUGIN_…` rows where core
///   builds them; a plugin's own kinds cannot be known in advance.
/// - `ERR_PROVIDER_<KIND>` (`crates/agora-core/src/providers/plugin.rs`): the
///   same for a content provider's error kinds.
///
/// Also dynamic, and therefore not checkable: a code that arrives as a `String`
/// and goes straight into a `LauncherError::Generic` (`code.into()`,
/// `code.to_string()`, `error.code()`, `error_code.to_string()`). Those carry a
/// code some other site declared; D1 replaces them with typed variants.
const DYNAMIC_CODE_FAMILIES: &[(&str, &str)] = &[
    (
        "crates/agora-core/src/providers/plugin.rs",
        r#"format!("ERR_PROVIDER_{:?}", error.code)"#,
    ),
    (
        "desktop/src-tauri/src/plugins.rs",
        r#"format!("ERR_PLUGIN_{:?}", error.code)"#,
    ),
    (
        "crates/agora/src/cli_misc_parity.rs",
        r#"format!("ERR_PLUGIN_{:?}", error.code)"#,
    ),
];

/// The dynamic families above are the whole list: if one moves or changes
/// shape, this fails so the list is not quietly out of date.
#[test]
fn dynamic_code_families_are_where_the_list_says() {
    let root = repo_root();
    for (path, shape) in DYNAMIC_CODE_FAMILIES {
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("{path} should be readable: {error}"));
        assert!(
            text.contains(shape),
            "{path} no longer builds `{shape}`; update DYNAMIC_CODE_FAMILIES"
        );
    }
}

#[test]
fn every_code_the_ui_and_cli_match_on_is_declared() {
    let root = repo_root();
    let declared = declared();
    let desktop_only: BTreeSet<&str> = DESKTOP_ONLY_CODES.iter().map(|(code, _)| *code).collect();

    // The desktop's TypeScript: every mention of a code, minus its own tests
    // (a mock's invented code proves nothing about what ships).
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
        let text = without_comments(&source);
        let spans = test_spans(&text);
        let is_typescript = file.extension().is_some_and(|ext| ext != "rs");
        for (offset, literal) in string_literals(&text, is_typescript) {
            if !is_typescript && inside(&spans, offset) {
                continue;
            }
            let code = literal.trim();
            if !code.starts_with("ERR_") || code.contains('{') || CODE_PREFIXES.contains(&code) {
                continue;
            }
            if declared.contains(code) || desktop_only.contains(code) {
                continue;
            }
            unknown.push(format!(
                "{}:{}: `{code}` is compared against but nothing declares it",
                relative(&file),
                line_of(&text, offset)
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
