//! The input and output shapes around the pure engine.

use agora_game_api::crash::{CrashPack, Fact, Owner, ParserOutput, Ref, Role, SourceAttribution};

/// Who contributed a pack. The host stamps this; a pack can never claim another plugin's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginIdentity {
    pub id: String,
    pub name: String,
    pub version: String,
}

impl PluginIdentity {
    /// The attribution of a parser that this plugin contributes.
    pub fn parser_source(&self, parser_id: &str) -> SourceAttribution {
        SourceAttribution {
            plugin_id: self.id.clone(),
            plugin_name: self.name.clone(),
            plugin_version: self.version.clone(),
            rule_id: None,
            rule_version: None,
            parser_id: Some(parser_id.to_string()),
        }
    }
}

/// One plugin's crash pack.
#[derive(Debug, Clone, PartialEq)]
pub struct PackInput {
    pub plugin: PluginIdentity,
    pub pack: CrashPack,
}

/// What one parser made of one artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedArtifact {
    pub plugin: PluginIdentity,
    pub parser_id: String,
    /// The artifact's name, for evidence lines.
    pub artifact: String,
    /// The crash source that found it. `None` for a swept or pasted artifact.
    pub source: Option<String>,
    pub output: ParserOutput,
}

/// What the resolver knows about a ref. Attribution is a later slice; tests use a fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub owner: Owner,
    pub role: Option<Role>,
}

/// Answers "who owns this ref, and what role has it".
pub trait RefResolver {
    fn resolve(&self, reference: &Ref) -> Resolved;
}

/// One inspector call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectionRequest<'a> {
    pub inspector: &'a str,
    pub target: &'a Ref,
}

/// What an inspector returned. `Missing` and `Failed` mean the inspector did not look, so they
/// take the rule's `on_unknown` effects and are never treated as a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    Pass,
    Fail,
    Unknown,
    Missing,
    Failed(String),
}

/// The deeper check a rule can ask for.
pub trait Inspector {
    fn inspect(&self, request: &InspectionRequest<'_>) -> Inspection;
}

/// Facts only the engine knows, such as the session verdict. They are always covered when present.
pub type EngineFacts = Vec<Fact>;
