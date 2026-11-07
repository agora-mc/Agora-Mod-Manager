//! What the engine produces. A plugin's rules feed it; the UI shows it.

use super::{LocalText, MatchedBy, NextStepKind, Privacy, SessionVerdict, SourceSpan};
use serde::{Deserialize, Serialize};

/// How sure Agora is. **There is no "certain" here, on purpose** (MASTER_SPEC §30.5): false
/// positives are always possible, so the strongest thing Agora ever says is "very likely". Ordered
/// weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    NotSure,
    Possible,
    Likely,
    VeryLikely,
}

impl Level {
    /// The words the UI uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotSure => "not sure",
            Self::Possible => "possible",
            Self::Likely => "likely",
            Self::VeryLikely => "very likely",
        }
    }
}

/// What a statement is about. A mechanism ("read a bad pointer") never fills the "who did it" slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    RootCause,
    Mechanism,
    Environment,
}

/// A kind of evidence. Within a family consensus takes the strongest signal rather than the sum, so
/// one fact seen twice counts once. Closed, because the scorer has to know how each behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    /// The loader or the game named it outright.
    DecisiveVerdict,
    /// An inspector opened the file and found it bad.
    VerifiedAsset,
    /// A rule recognised this crash.
    KnownSignature,
    /// A known clash between two things.
    CuratedConflict,
    /// Its code was running on the stack.
    Execution,
    /// Its data was being handled.
    ObjectReference,
    /// It provides a file involved. Boost only.
    ResourceProvider,
    /// It changed since the last good launch. Boost only.
    ChangeCorrelation,
    /// This crash has happened with it before. Boost only.
    HistoryRepeat,
    /// A past fix on this PC confirmed it. Boost only: memory alone never accuses a mod.
    LocalConfirmation,
    /// It is often on stacks like this. Boost only.
    CoCrash,
    /// It logged something new this session. Boost only.
    NovelLogLine,
}

impl Family {
    /// Whether this family can only raise a suspect that other evidence already named. It can never
    /// create a candidate.
    pub fn is_boost_only(self) -> bool {
        matches!(
            self,
            Self::ResourceProvider
                | Self::ChangeCorrelation
                | Self::HistoryRepeat
                | Self::LocalConfirmation
                | Self::CoCrash
                | Self::NovelLogLine
        )
    }
}

/// Who contributed something. The host stamps it, so it can't be claimed from inside a pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAttribution {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    /// The rule, when a rule said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_version: Option<u32>,
    /// The parser, when a parser said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_id: Option<String>,
}

/// Who a ref belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Owner {
    /// An installed item, by its content key.
    Item {
        key: String,
        name: String,
    },
    /// The unmodified game.
    Base,
    /// Output a tool generated.
    Generated {
        tool: String,
    },
    /// A file the user or the game changed in the instance.
    Writable,
    /// A framework, by name.
    Framework {
        name: String,
    },
    System,
    /// It's real, and nothing in this instance provides it.
    NotInInstance,
    Unknown,
}

/// A place in an artifact that supports a statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLine {
    pub artifact: String,
    pub span: SourceSpan,
    /// The text quoted, already scrubbed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

/// One clue against a suspect, after a rule fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    pub family: Family,
    pub candidate: Owner,
    pub weight: u32,
    pub source: SourceAttribution,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceLine>,
}

/// A statement shown to the user. **`source` isn't optional**: every finding names who said it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub scope: Scope,
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<Owner>,
    pub headline: LocalText,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_not_sure: Option<LocalText>,
    pub source: SourceAttribution,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceLine>,
}

/// A suspect after consensus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub owner: Owner,
    pub level: Level,
    pub score: u32,
    pub families: Vec<Family>,
    /// Everyone whose rules contributed, so a noisy plugin can be spotted.
    pub sources: Vec<SourceAttribution>,
    /// Others that also provide what it was blamed through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_provided_by: Vec<String>,
}

/// Something Agora wanted to know and couldn't find out. Shown, never swallowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CouldNotCheck {
    pub reason: CouldNotCheckReason,
    /// The fact kind, rule, parser or version it concerns.
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Who wanted it, or whose code failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceAttribution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CouldNotCheckReason {
    /// No parser looked for this kind of fact.
    NotCovered,
    /// A parser says it could not look.
    AbsentFromArtifact,
    /// A rule's version gate couldn't judge this game or logger version.
    VersionUnknown,
    /// The parser or inspector timed out, threw, ran out of memory or returned something invalid.
    CodeFailed,
    /// A rule named an inspector and nothing provides it.
    InspectorMissing,
    /// Agora did not keep these bytes.
    NotRetained,
    /// A rule hit [`MAX_RULE_BRANCHES`](super::MAX_RULE_BRANCHES).
    TooManyMatches,
}

/// What to do next, with what it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextStep {
    pub kind: NextStepKind,
    /// Who to try first: the suspect for an experiment, the seed for a bisect. Content keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

/// A fix on offer, after the engine resolved its suspect. Core turns it into its repair action and
/// the user approves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairOffer {
    pub request: ResolvedRepair,
    pub source: SourceAttribution,
}

/// A [`RepairRequest`](super::RepairRequest) with its suspect turned into a content key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedRepair {
    DisableContent {
        key: String,
    },
    EnableContent {
        key: String,
    },
    PinContentUpdate {
        key: String,
    },
    UnpinContentUpdate {
        key: String,
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

/// The evidence as Agora holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<String>,
    /// The crash source that found it. Absent means the sweep found it, or the user gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// How it was tied to the session. Absent for evidence that isn't, such as a pasted log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_by: Option<MatchedBy>,
    #[serde(default)]
    pub privacy: Privacy,
    pub retained: Retained,
    /// Who wrote it, when a log can be traced to a mod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributed_to: Option<Owner>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Retained {
    Whole,
    HeadTail { head: u64, tail: u64 },
    FactsOnly,
    NotRetained { why: String },
}

/// A warning about all the evidence, with who raised it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustNote {
    pub text: LocalText,
    pub source: SourceAttribution,
}

/// The answer to "why did my game crash?". Abstaining is an answer: `level` `not_sure`, no
/// candidates, and a next step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashAnswer {
    pub contract: u32,
    pub verdict: SessionVerdict,
    pub headline: LocalText,
    pub level: Level,
    /// Ranked, strongest first. Empty when Agora isn't naming anyone.
    pub candidates: Vec<Candidate>,
    pub findings: Vec<Finding>,
    pub could_not_check: Vec<CouldNotCheck>,
    pub trust_notes: Vec<TrustNote>,
    pub next: NextStep,
    pub repairs: Vec<RepairOffer>,
    pub artifacts: Vec<Artifact>,
}
