//! Facts and refs: what a parser says it found, and what those findings name.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A value in a fact. Numbers are `f64` on purpose: that is what a script engine holds. **A value
/// that may not fit in 53 bits, such as a memory address, is written as text**
/// (`"0x7ff6a0001000"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Number(f64),
    Text(String),
}

/// A fact's attribute: a scalar or a short list of them. Nothing nests, so a rule can always reach
/// a value by its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Scalar(Scalar),
    List(Vec<Scalar>),
}

/// One thing a parser found. `kind` is `namespace.name`. `core.*` is the closed vocabulary in
/// [`CORE_FACT_KINDS`]; any other namespace belongs to the plugin that emits it and core never
/// interprets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, AttrValue>,
    /// What this fact names. Attribution works on these, never on attributes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<Ref>,
    /// Where in the artifact it came from, so the answer can quote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// A name inside a fact that some mod, or the game, or nobody, provides. `kind` says what sort of
/// thing it is. It is open, because a community game has its own sorts; the pack's
/// [`RefMap`](super::RefMap)s say how each sort is looked up. [`ref_kind`] lists the common ones so
/// that packs agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ref {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub strength: Strength,
}

/// The ref kinds packs should use for these things. Not a closed list.
pub mod ref_kind {
    /// A native module: `Example.dll`.
    pub const MODULE: &str = "module";
    /// A game plugin in a load order.
    pub const PLUGIN: &str = "plugin";
    /// A file path relative to the game's visible tree.
    pub const FILE: &str = "file";
    /// A game data record, written as the game names it.
    pub const FORM: &str = "form";
    /// A managed class name, dotted.
    pub const CLASS: &str = "class";
    /// A mod's own identifier.
    pub const MOD_ID: &str = "mod_id";
    /// A script by name.
    pub const SCRIPT: &str = "script";
}

/// How closely a ref is tied to the failure. Ordered weakest first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    /// Merely named.
    #[default]
    Mention,
    /// Data the failing code was handling.
    Object,
    /// Was executing, on the stack.
    Exec,
    /// Was executing at the top of the stack.
    ExecTop,
}

/// Where a fact came from. Line numbers start at 1.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpan {
    /// The child artifact, when the parser was reading a folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u64>,
}

/// The neutral vocabulary a parser may emit, with the attributes each kind may carry. It is closed
/// so that a typo in a rule is an error and not a rule that never fires. Who or what a fact is
/// about goes in `refs`: a frame's module, a load verdict's subject.
///
/// - `core.exception`: `category` is required and one of [`EXCEPTION_CATEGORIES`]; `depth` is the
///   position in a cause chain, 0 outermost.
/// - `core.frame`: `index` is required, 0 is the top. `kind` is `probable`, `scan` (found by
///   scanning stack memory, weak) or `managed`.
/// - `core.load_verdict`: `outcome` is `loaded`, `refused`, `failed` or `last_seen`.
/// - `core.dependency_problem`: `state` is `absent`, `disabled` or `wrong_version`.
/// - `core.log_line`: a line kept for rule text search.
pub const CORE_FACT_KINDS: &[(&str, &[&str])] = &[
    (
        "core.exception",
        &[
            "class",
            "category",
            "message",
            "access",
            "fault_address",
            "near_null",
            "pointer_class",
            "depth",
        ],
    ),
    (
        "core.frame",
        &[
            "index",
            "kind",
            "address",
            "rva",
            "symbol_scheme",
            "symbol",
            "symbol_offset",
            "source_file",
            "line",
        ],
    ),
    (
        "core.module",
        &["version", "sha256", "base_address", "size"],
    ),
    (
        "core.process",
        &[
            "pid",
            "cwd",
            "command_line",
            "image",
            "image_version",
            "image_hash",
            "uptime_s",
        ],
    ),
    (
        "core.system",
        &["ram_free_mb", "commit_pct", "gpu", "vram_pct", "os"],
    ),
    ("core.load_verdict", &["outcome", "reason", "requires"]),
    (
        "core.dependency_problem",
        &["requirement", "state", "needed_by"],
    ),
    ("core.log_line", &["level", "text", "section"]),
];

/// Facts only the engine writes, from what it knows beyond the artifact. A rule queries them like
/// any other; a parser that emits one is refused. This is why rules need no special operators for
/// history, changes or ownership.
///
/// - `core.session`: how this session ended.
/// - `core.history`: how often this bucket has happened.
/// - `core.change`: one per content item changed since the last good launch. `change` is `added`,
///   `removed` or `updated`; the item is in `refs` under kind `content`.
/// - `core.attribution`: one per ref, with the content key that owns it.
/// - `core.instance`: the analysis has a game behind it. `game` is its game id, and is absent when
///   the file is loose. It is always present, so a rule can require `game` and stay quiet without an
///   instance without making the rule unanchored.
pub const ENGINE_FACT_KINDS: &[(&str, &[&str])] = &[
    ("core.instance", &["game"]),
    ("core.session", &["verdict", "exit_code", "runtime_ms"]),
    (
        "core.history",
        &[
            "same_bucket_count",
            "sessions_since_first",
            "predates_last_change",
        ],
    ),
    ("core.change", &["change", "from_version", "to_version"]),
    (
        "core.attribution",
        &["ref_kind", "ref_value", "owner", "also_provided_by", "via"],
    ),
];

/// Attributes the engine adds to a parser's fact once the pack's roles are known: usable in rules,
/// refused from a parser. `actionable_index` counts frames after dropping those whose ref has the
/// role `system` or `platform`, so a top frame inside the game itself or the Java standard library
/// doesn't count as the place to look. `novel` marks a log line unseen in the last clean session.
pub const ENGINE_ATTRS: &[(&str, &[&str])] = &[
    ("core.frame", &["actionable_index"]),
    ("core.log_line", &["novel"]),
];

/// Every `core.exception.category`. Anything a parser can't place is `unknown`.
pub const EXCEPTION_CATEGORIES: &[&str] = &[
    "access_violation",
    "fast_fail",
    "managed",
    "oom",
    "assert",
    "load_failure",
    "unknown",
];

fn lookup(
    table: &[(&'static str, &'static [&'static str])],
    kind: &str,
) -> Option<&'static [&'static str]> {
    table.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a)
}

/// The attributes a rule may name on a `core.*` kind, or `None` if core has no such kind.
pub fn core_kind_attrs(kind: &str) -> Option<Vec<&'static str>> {
    let base = lookup(CORE_FACT_KINDS, kind).or_else(|| lookup(ENGINE_FACT_KINDS, kind))?;
    let mut attrs = base.to_vec();
    attrs.extend(lookup(ENGINE_ATTRS, kind).unwrap_or(&[]));
    Some(attrs)
}

/// The attributes a parser may set on a `core.*` kind, or `None` if a parser may not emit it.
pub fn parser_kind_attrs(kind: &str) -> Option<&'static [&'static str]> {
    lookup(CORE_FACT_KINDS, kind)
}

/// Whether `kind` is `namespace.name` in lower-case words.
pub fn is_valid_kind(kind: &str) -> bool {
    let word = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    };
    match kind.split_once('.') {
        Some((namespace, name)) => word(namespace) && name.split('.').all(word),
        None => false,
    }
}
