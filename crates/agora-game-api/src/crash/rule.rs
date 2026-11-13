//! The rule format. Rules are data over facts: they never see an artifact's raw text except through
//! the labelled [`Condition::Text`] escape hatch.
//!
//! # How a rule is evaluated
//!
//! Conditions have three outcomes, [`Truth`], because absent is not clean. A query over a fact kind
//! no parser looked for is `Unknown`, not false. A parser "looked for" a kind if it emitted one or
//! reported it `checked` in its coverage. A rule whose `when` comes out `Unknown` produces a
//! could-not-check note: never a finding, and never silence. A rule therefore needs no list of
//! prerequisites; what it queries is what it requires.
//!
//! Conditions run left to right and carry bindings. A `fact` condition matches every fact that
//! fits, and each match continues as its own branch, so one rule can name several suspects (at most
//! [`MAX_RULE_BRANCHES`]). A name already bound must match the same value, which is how two facts
//! are joined; in `where`, the text `"$name"` means the bound value. Bindings made inside `not` do
//! not leave it, and `count` makes none.
//!
//! The effects fire once per surviving branch. Text may say `{name}` for a bound value and
//! `{owner:name}` for the display name of whatever provides a ref.

use super::{Cause, Family, Level, Role, Scalar, Scope, Strength};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// The most branches one rule may produce unless it asks for more. The engine stops there and
/// says so.
pub const MAX_RULE_BRANCHES: usize = 32;
/// The most a rule may raise its own limit to, with `max_branches`. A rule that checks every item
/// in a list (each script-extender plugin a log lists, say) needs more than the default.
pub const MAX_RULE_BRANCHES_CEILING: usize = 512;
/// The longest regex a rule may hold, in bytes.
pub const MAX_RULE_REGEX_LEN: usize = 256;
/// The compiled size limit for `regex::RegexBuilder`. Rust's `regex` runs in linear time, so this
/// and the length are all a hostile pattern can push against.
pub const RULE_REGEX_SIZE_LIMIT: usize = 1 << 20;
/// The heaviest a rule may weigh a suspect.
pub const MAX_RULE_WEIGHT: u32 = 32;

/// Three-valued logic (Kleene), shared with the platform condition language. The evaluator must use
/// these operations rather than booleans.
pub use crate::condition::Truth;
/// The version gate the platform condition language defines; crash rules keep using it.
pub use crate::condition::VersionGate;

/// One rule. Its `id` is local to the plugin, and the host stamps the source, so a pack can't claim
/// another plugin's name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// Bump when the meaning changes.
    pub version: u32,
    #[serde(default, skip_serializing_if = "RuleScope::is_any")]
    pub scope: RuleScope,
    pub when: Condition,
    /// Optional deeper check of a suspect, whose outcomes hold more effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Verify>,
    /// What this rule says whenever `when` holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    pub text: RuleText,
    /// Rule ids in this pack that this one replaces when both fire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    /// Fixes this rule may offer. The user approves each one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<RepairRequest>,
    /// Buttons this rule offers. Each kind is one the UI already knows how to perform, so a pack
    /// never decides behaviour, only which existing action is useful here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<NextStepKind>,
    /// Sample artifacts in the plugin to run this rule against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixtures: Vec<String>,
    /// Where the knowledge came from, for people reading the pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// How many branches this rule may produce, from [`MAX_RULE_BRANCHES`] up to
    /// [`MAX_RULE_BRANCHES_CEILING`]. Past it the rule does not fire and says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_branches: Option<u32>,
}

impl Rule {
    /// The branch limit the engine applies: the rule's own, never beyond the ceiling.
    pub fn branch_limit(&self) -> usize {
        self.max_branches
            .map(|limit| (limit as usize).clamp(1, MAX_RULE_BRANCHES_CEILING))
            .unwrap_or(MAX_RULE_BRANCHES)
    }
}

/// When the rule applies at all. A version the gate can't judge makes the rule `Unknown`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleScope {
    /// The game's own version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<VersionGate>,
    /// The format the facts came from, and the version of what wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProducerGate>,
}

impl RuleScope {
    pub fn is_any(&self) -> bool {
        self.runtime.is_none() && self.producer.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerGate {
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionGate>,
}

/// A condition, written with one key: `{ "all": [...] }`, `{ "fact": {...} }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
    AtLeast {
        n: u32,
        of: Vec<Condition>,
    },
    /// Some fact fits. Session, history, change and ownership questions are facts too; see
    /// [`ENGINE_FACT_KINDS`](super::ENGINE_FACT_KINDS).
    Fact(FactQuery),
    /// How many facts fit.
    Count(CountQuery),
    /// The escape hatch: a regex over `core.log_line` text. Prefer a fact. The linter refuses one
    /// with no `section` or `source` unless another condition also has to hold.
    Text(TextMatch),
}

impl Condition {
    /// Every fact kind this condition queries.
    pub fn kinds(&self) -> Vec<&str> {
        match self {
            Self::All(list) | Self::Any(list) | Self::AtLeast { of: list, .. } => {
                list.iter().flat_map(Self::kinds).collect()
            }
            Self::Not(inner) => inner.kinds(),
            Self::Fact(query) => vec![query.kind.as_str()],
            Self::Count(query) => vec![query.kind.as_str()],
            Self::Text(_) => vec!["core.log_line"],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactQuery {
    pub kind: String,
    /// Attribute name to test. A bare scalar means "equals".
    #[serde(default, rename = "where", skip_serializing_if = "BTreeMap::is_empty")]
    pub filter: BTreeMap<String, AttrMatch>,
    /// The fact must have a ref like this.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<RefQuery>,
    /// Variable name to attribute name: remember this attribute's value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bind: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefQuery {
    /// The ref's kind. Required, so that its owner can be looked up.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<AttrMatch>,
    /// The ref must be at least this close to the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<Strength>,
    /// The role the pack's [`RoleRule`](super::RoleRule)s give this ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// Remember this ref under a variable. Only ref-bound variables can be suspects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountQuery {
    pub kind: String,
    #[serde(default, rename = "where", skip_serializing_if = "BTreeMap::is_empty")]
    pub filter: BTreeMap<String, AttrMatch>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<RefQuery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_least: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_most: Option<u32>,
}

/// A bounded regex over log lines, compiled with Rust's `regex` only, so it can't backtrack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextMatch {
    /// Only lines the parser placed in this section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Only lines from artifacts of this crash source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub regex: String,
    /// Named capture to ref kind: `(?P<m>...)` binds `m`, and an entry here makes it a ref that can
    /// be a suspect.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refs: BTreeMap<String, String>,
}

/// A test on one attribute: a bare scalar, or an object of tests that must all pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AttrMatch {
    Is(Scalar),
    Test(MatchOps),
}

impl<'de> Deserialize<'de> for AttrMatch {
    // By hand, because `untagged` would hide the unknown-field message the linter promises.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Test)
                .map_err(D::Error::custom),
            serde_json::Value::Null | serde_json::Value::Array(_) => Err(D::Error::custom(
                "an attribute test is a value, or an object of tests such as {\"in\": [...]}",
            )),
            scalar => serde_json::from_value(scalar)
                .map(Self::Is)
                .map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Scalar>,
    #[serde(default, rename = "in", skip_serializing_if = "Option::is_none")]
    pub any_of: Option<Vec<Scalar>>,
    /// Case-insensitive, `*` and `?`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionGate>,
    /// Whether the attribute exists on the fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
}

/// A suspect named by a variable holding a ref: whichever mod provides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerOf {
    pub owner_of: String,
}

/// What a rule says, written with one key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Effect {
    /// A clue against a suspect, for consensus to weigh.
    Signal {
        family: Family,
        candidate: OwnerOf,
        weight: u32,
        /// What kind of cause this clue points at. Absent: the candidate's own default (`mod`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
    },
    /// A statement for the user, in the rule's `text` unless it brings its own. A `root_cause`
    /// finding names a candidate, with a family and weight. A `mechanism` or `environment` one says
    /// what happened and accuses nobody.
    Finding {
        scope: Scope,
        level: Level,
        /// What kind of cause this is. Absent: a root-cause finding about a mod is `mod`, and a
        /// finding that accuses nobody is `other`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate: Option<OwnerOf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        family: Option<Family>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<RuleText>,
    },
    /// The evidence itself is doubtful (a crash suppressor is loaded, the log is cut short), so no
    /// answer for this incident reads stronger than `cap`. The note is shown with its source.
    Trust { cap: Level, text: LocalText },
    /// Treat the ref in this variable as having this role, for this incident.
    Demote { of: String, role: Role },
    /// Something worth saying that blames nobody and states no cause.
    Context { text: LocalText },
}

/// A deeper look at a suspect, through an inspector this or another plugin contributes. `on_unknown`
/// is what happens when it could not look.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verify {
    pub inspector: String,
    /// A ref-bound variable naming the file to inspect.
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_pass: Vec<Effect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_fail: Vec<Effect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_unknown: Vec<Effect>,
}

/// Locale to text. `en` is required.
pub type LocalText = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleText {
    pub headline: LocalText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_not_sure: Option<LocalText>,
}

/// A fix a rule offers, from the same closed set as a plugin's repair proposals. The engine fills
/// in the instance and resolves the suspect, and core's approval path runs it, so a rule can only
/// ever ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RepairRequest {
    DisableContent {
        key: OwnerOf,
    },
    EnableContent {
        key: OwnerOf,
    },
    PinContentUpdate {
        key: OwnerOf,
    },
    UnpinContentUpdate {
        key: OwnerOf,
    },
    SetJvmMemory {
        memory_mb: i64,
    },
    ResetJvmArgs,
    CreateSnapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// What the user should do next. Typed, so that the UI's text never decides behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextStepKind {
    ApplyRepair,
    ConfirmExperiment,
    Bisect,
    Recapture,
    RestoreSave,
    NothingNeeded,
    SecondOpinion,
}

/// A button a rule offers. Closed, because the screen has to know how to perform each one; a pack
/// only says which of those existing actions fits this crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Install the content named by `content_key`.
    InstallContent,
    /// Open the launcher's own console or launch output.
    OpenConsole,
    /// Open the instance's JVM or memory settings.
    OpenJvmSettings,
    /// Open the crash report or log the finding came from.
    OpenCrashLog,
    /// Open the installed-mods view so the user can review the set.
    ReviewMods,
}

impl ActionKind {
    /// Whether this kind needs a content key.
    pub fn needs_content_key(self) -> bool {
        self == Self::InstallContent
    }
}

/// One button a rule offers, in the pack's words and from the UI's closed set of actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    pub kind: ActionKind,
    /// The content key to install. Required for `install_content`, refused for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_key: Option<String>,
    /// The button's words. Absent means the screen uses its own label for the kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<LocalText>,
}
