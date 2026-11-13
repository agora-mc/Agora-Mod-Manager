//! A crash pack: everything a plugin tells the crash doctor, as one hand-written JSON file.

use super::{ParserLimits, Rule};
use crate::GamePath;
use serde::{Deserialize, Serialize};

/// The slots. Every list may be empty: a mod's own plugin might ship two rules about its own log,
/// and a game's plugin ships all of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashPack {
    /// [`CRASH_CONTRACT_VERSION`](super::CRASH_CONTRACT_VERSION).
    pub contract: u32,
    /// The game ids this pack applies to. Empty means every game.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<CrashSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parsers: Vec<ParserDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inspectors: Vec<InspectorDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ref_maps: Vec<RefMap>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<RoleRule>,
    /// What makes a mod a risk to an existing save. Absent means the pack says nothing, so every
    /// suspect is `unknown`; it never guesses "safe".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_risk: Option<SaveRisk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

/// Which files make removing a mod dangerous to the player's save.
///
/// A file path is game-relative and `/`-separated. `*` matches within one path segment and `**`
/// matches any number of segments. The first matching `risky` pattern wins; otherwise a file is
/// safe only when a `safe` pattern matches it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRisk {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risky: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safe: Vec<String>,
}

impl SaveRisk {
    /// Whether the pack declares any pattern at all.
    pub fn is_empty(&self) -> bool {
        self.risky.is_empty() && self.safe.is_empty()
    }
}

/// Where a game's crash evidence appears. Agora's sweep finds more on its own; this says what the
/// game is known to write, and how to tell the file belongs to this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashSource {
    pub id: String,
    /// The folder to look in. Core resolves it; a pack never holds an absolute path.
    #[serde(rename = "where")]
    pub location: GamePath,
    #[serde(rename = "match")]
    pub matching: SourceMatch,
    #[serde(default)]
    pub unit: ArtifactUnit,
    /// The parser for it. Absent means detect by content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// How to tie an artifact to the session, strongest first. The first that works is recorded
    /// and shown. A `primary` source needs at least one, or an old log would read as a new crash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind: Vec<SourceBind>,
    #[serde(default)]
    pub role: SourceRole,
    #[serde(default)]
    pub copy: CopyPolicy,
    #[serde(default)]
    pub privacy: Privacy,
    /// Overrides for files inside a folder artifact: a save copy in a crash report, say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMatch {
    /// Case-insensitive, `*` and `?`, over the name only.
    pub glob: String,
    /// A file that says the artifact is finished being written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactUnit {
    #[default]
    File,
    /// The artifact is the folder and everything in it.
    Folder,
}

/// One way to tie an artifact to a session. Each gives a [`MatchedBy`](super::MatchedBy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceBind {
    /// The process id is in the artifact's name; `regex`'s first group captures it.
    PidInName { regex: String },
    /// The parser reports a `core.process` fact with `pid`.
    PidInFacts,
    /// The parser reports a `core.process` fact with `cwd`.
    WorkingDirInFacts,
    /// The file's time falls inside the session, widened by these. Likely, not certain.
    Time { before_s: u32, after_s: u32 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    /// A bound artifact from here means the game crashed, which outranks the exit code.
    Primary,
    /// Read alongside a crash, never proof of one.
    #[default]
    Supplementary,
}

/// How much of an artifact Agora keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPolicy {
    #[default]
    Whole,
    HeadTail,
    /// Parse it where it lies and keep only the facts.
    FactsOnly,
    Never,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Privacy {
    #[default]
    Normal,
    /// May hold a user name, a save, a screenshot. Left out of exports unless asked for.
    Personal,
    /// Never leaves the machine.
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildPolicy {
    pub glob: String,
    #[serde(default)]
    pub copy: CopyPolicy,
    #[serde(default)]
    pub privacy: Privacy,
}

/// A parser. `id` is what a source's `format` names, local to the plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserDecl {
    pub id: String,
    pub entry: CodeEntry,
    /// Whether this parser is the one that speaks for the file, or reads alongside another. One
    /// primary per file is chosen; every extra runs and may emit only facts in a namespace of its
    /// own, never `core.*`.
    #[serde(default)]
    pub reads: Reads,
    /// How to recognise its artifacts. Empty means only a source's `format` selects it.
    #[serde(default, skip_serializing_if = "Detect::is_empty")]
    pub detect: Detect,
    /// Asks for less than the defaults. The host keeps the lower of this and its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ParserLimits>,
}

/// How a parser takes part when more than one recognises the same file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reads {
    /// Speaks for the file. When several primaries claim it, one is chosen (built-in, then
    /// official, then by plugin id) and the rest are reported as "also recognised by".
    #[default]
    Primary,
    /// Reads alongside the primary. Every extra runs, so it must stay in its own namespace.
    Extra,
    /// Reads a fragment the user pasted, when no primary parser claimed it. It never runs on a
    /// file Agora found or the user picked, and its facts are ordinary facts, so the pack's rules
    /// work on them. Its `detect` must be non-empty: a fragment still needs a shape to recognise.
    Snippet,
}

impl Reads {
    /// Whether a parser may emit `core.*` facts. A snippet parser can, because it stands in for
    /// the primary reader of the text it reads.
    pub fn may_emit_core(self) -> bool {
        matches!(self, Self::Primary | Self::Snippet)
    }

    /// Whether this parser only reads text the user pasted.
    pub fn is_snippet(self) -> bool {
        self == Self::Snippet
    }
}

/// What runs. The engine calls all of them through the same request and output shapes
/// ([`ParserRequest`](super::ParserRequest)), and nothing beyond those shapes may depend on which
/// one it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CodeEntry {
    /// A function the plugin's script exports.
    Script { export: String },
    /// A function in a WebAssembly module in the plugin. Declared now; the host comes later.
    Wasm { module: String, export: String },
    /// A parser built into core, which holds no game knowledge, configured by data.
    Generic {
        name: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        config: serde_json::Value,
    },
    /// A parser compiled into the binary, supplied with a built-in pack. Only a game package's
    /// built-in pack may name one; a plugin's pack is refused, because it would run code that is
    /// not in the plugin.
    BuiltIn { name: String },
}

/// Recognising a format from its name and first bytes. Every field given must fit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_glob: Option<String>,
    /// The artifact's first bytes, in hex, for a binary format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magic_hex: Option<String>,
    /// Regexes over the first [`DETECT_HEAD_BYTES`] of text, with `\r\n` already turned into
    /// `\n`. All must match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub all: Vec<String>,
    /// At least one must match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any: Vec<String>,
}

impl Detect {
    pub fn is_empty(&self) -> bool {
        self.name_glob.is_none()
            && self.magic_hex.is_none()
            && self.all.is_empty()
            && self.any.is_empty()
    }
}

/// How much of an artifact detection reads.
pub const DETECT_HEAD_BYTES: usize = 64 * 1024;

/// A deeper check a rule can call by name: open this file, is it sound?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectorDecl {
    pub id: String,
    /// Which files it can look at, by name.
    pub accepts: Vec<String>,
    pub entry: CodeEntry,
}

/// How refs of one kind are traced to what provides them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefMap {
    pub kind: String,
    pub lookup: RefLookup,
    #[serde(rename = "match")]
    pub matching: NameMatch,
    /// Segments that never name the thing being looked up. Only `segment` matching reads it; a
    /// list here keeps a class named `net.*` or `*.client.*` from matching a mod id by accident.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RefLookup {
    /// Who provides this path in the deployed setup. `{value}` is the ref's value; a segment that
    /// is exactly `*` matches one folder on the way to it. `archives` also searches inside archives
    /// the instance view has listed.
    Path {
        patterns: Vec<String>,
        #[serde(default)]
        archives: bool,
    },
    /// A name the instance view has indexed for its items, such as declared mod ids.
    Index(String),
    /// A name from a fact of the same analysis, such as the mod list in the crash report that is
    /// being read. `kind` is the fact kind, `key` the attribute that is matched against the ref,
    /// and `name` the attribute shown to the user (falling back to the key). This needs no
    /// instance, so a loose file can name the mod its own report lists.
    Facts {
        kind: String,
        key: String,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMatch {
    Exact,
    Casefold,
    /// `a.b.c` matches an entry `a.b` at a dot boundary: class names and packages.
    PrefixDot,
    /// The ref value is split at `.`, `$` and `/`; the last part (the class's own name) is dropped
    /// and the rest are compared with the index keys, all normalised to lower-case ASCII letters
    /// and digits. A key under three characters never matches. A `facts` lookup uses this.
    Segment,
}

/// Whether a path pattern uses `*` only as a whole folder segment, and never as the file name.
/// `Ok` is a pattern the resolver can read; `Err` names the rule for the linter.
pub fn check_path_pattern(pattern: &str) -> Result<(), &'static str> {
    let slashed = pattern.replace('\\', "/");
    let segments: Vec<&str> = slashed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for segment in &segments {
        if segment.contains('*') && *segment != "*" {
            return Err(
                "`*` must be a whole path segment, so `**` and `foo*.dll` are not supported",
            );
        }
    }
    if segments.last() == Some(&"*") {
        return Err("`*` cannot be the last segment; the pattern must name the file after it");
    }
    Ok(())
}

/// Says what part a named thing plays, so a stack can be read properly. Works on any ref kind,
/// because a Java frame names a class where a native one names a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRule {
    pub kind: String,
    pub value: String,
    #[serde(rename = "match")]
    pub matching: NameMatch,
    pub role: Role,
}

/// The part a ref plays. Consensus weighs these, so a pack can't invent a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The operating system or a driver. Never a suspect.
    System,
    /// The game itself or its runtime. Crashes often surface here without starting here.
    Platform,
    /// Everything runs through it, so its presence says little.
    Framework,
    /// Hooks engine calls, so it sits on stacks it did not cause.
    HookFramework,
}
