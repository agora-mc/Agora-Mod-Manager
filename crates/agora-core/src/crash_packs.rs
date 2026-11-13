//! Loading crash packs and running their parsers (MASTER_SPEC §30.11).
//!
//! This module holds no game knowledge. It reads the packs a plugin names in
//! its manifest, lints them through the contract, and drives a parser export
//! through [`ScriptHost`] with a scope that reaches exactly one artifact.
//!
//! Two rules shape everything here:
//!
//! - **A bad pack is reported, never silently dropped, and never stops the
//!   others.** Each plugin's packs are independent; a broken file names the
//!   plugin and the file so a person can fix it.
//! - **A parser sees no paths and no file system.** Its only reads are the two
//!   scoped operations the contract defines, against the artifact in the
//!   request.

use crate::error::LauncherResult;
use agora_game_api::crash::{
    lint_pack_json, lint_rule, validate_extra_output, validate_inspector_output,
    validate_parser_output, ActionKind, ActionRequest, ArtifactHandle, AttrMatch, BuiltInPack,
    Cause, CodeEntry, Condition, CoverageState, CrashPack, Detect, Effect, FactQuery, GameHandle,
    InspectorDecl, InspectorOutput, Level, LinesReply, LocalText, MatchOps, ParseNoteKind,
    ParserDecl, ParserLimits, ParserOutput, ParserReadError, ParserReads, ParserRequest,
    ReadRefusal, ReadRefused, Rule, RuleScope, RuleText, Scalar, Scope, TextMatch,
    CRASH_CONTRACT_VERSION, DETECT_HEAD_BYTES,
};
use agora_game_api::GameId;
use agora_plugin_api::host::{ScriptHost, ScriptReadError, ScriptReads};
use agora_plugin_api::manifest::{validate_package_path, PluginManifest};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The most a parser's JSON output may be before it is refused. The contract's
/// per-field limits are the real guard; this catches a well-shaped but enormous
/// result before it is parsed into memory twice.
pub const MAX_PARSER_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Pack loading
// ---------------------------------------------------------------------------

/// One thing wrong with a pack, with the file and (when it has one) the rule id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackProblem {
    pub plugin_id: String,
    /// The manifest-relative path, so a person can open it.
    pub file: String,
    /// `rule example_id`, `parser crash_logger`, or empty for the whole file.
    pub at: String,
    /// The linter's code (or `capability`, `path`, `missing`, `unreadable`).
    pub code: String,
    pub message: String,
}

impl PackProblem {
    fn new(
        plugin_id: &str,
        file: impl Into<String>,
        at: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            file: file.into(),
            at: at.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PackProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.file)?;
        if !self.at.is_empty() {
            write!(f, " ({})", self.at)?;
        }
        write!(f, ": {} [{}]", self.message, self.code)
    }
}

/// The source a built-in pack's findings name: Agora itself, not a plugin.
pub const BUILT_IN_PLUGIN_ID: &str = "agora";

fn is_false(value: &bool) -> bool {
    !*value
}

/// Who findings from a built-in pack name, with this Agora build's version.
pub fn built_in_identity() -> crate::crash_engine::PluginIdentity {
    crate::crash_engine::PluginIdentity {
        id: BUILT_IN_PLUGIN_ID.to_string(),
        name: "Agora".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

// ---------------------------------------------------------------------------
// Community signatures (the signed catalog's crash_signatures table)
// ---------------------------------------------------------------------------

/// The identity every community signature names. The id is namespaced so it can never collide
/// with a plugin id, and the name is what a finding shows after "from".
pub const COMMUNITY_PACK_PLUGIN_ID: &str = "agora.community";
pub const COMMUNITY_PACK_PLUGIN_NAME: &str = "Agora community catalog";
/// The file a community pack reports, so a problem points at where the row came from.
pub const COMMUNITY_PACK_FILE: &str = "registry.db (crash_signatures)";

/// One community crash signature, format v2. It is data the engine turns into a rule; it never
/// names a suspect and has no parser of its own.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunitySignature {
    pub id: String,
    pub name: String,
    /// The game ids this signature applies to. Required and non-empty.
    pub games: Vec<String>,
    pub regex_pattern: String,
    #[serde(default)]
    pub level: Option<Level>,
    #[serde(default)]
    pub cause: Option<Cause>,
    /// The finding's headline, in plain text.
    pub text: String,
    #[serde(default)]
    pub action: Option<CommunityAction>,
}

/// An optional button a signature offers, from the UI's closed set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityAction {
    pub kind: ActionKind,
    #[serde(default)]
    pub content_key: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Parse one v2 signature file. The caller still runs [`CommunitySignature::to_rule`] and
/// [`lint_rule`], so shape and rule checks are the same for a file and a catalog row.
pub fn parse_community_signature_json(json: &str) -> Result<CommunitySignature, String> {
    serde_json::from_str(json).map_err(|error| error.to_string())
}

impl CommunitySignature {
    /// Turn this signature into one rule. It applies only to the games it names, produces one
    /// finding that accuses nobody, and offers its action. The caller lints the result.
    pub fn to_rule(&self) -> Result<Rule, Vec<String>> {
        let mut errors = Vec::new();
        if self.games.is_empty() {
            errors.push("`games` is a non-empty list of game ids".to_string());
        }
        for game in &self.games {
            if let Err(error) = GameId::new(game.clone()) {
                errors.push(format!("game id `{game}`: {error}"));
            }
        }
        let level = match self.level.unwrap_or(Level::Possible) {
            Level::Possible => Level::Possible,
            Level::Likely => Level::Likely,
            other => {
                errors.push(format!(
                    "`level` is `possible` or `likely`; got `{}`",
                    match other {
                        Level::NotSure => "not_sure",
                        Level::VeryLikely => "very_likely",
                        Level::Possible | Level::Likely => unreachable!(),
                    }
                ));
                Level::Possible
            }
        };
        if self.regex_pattern.trim().is_empty() {
            errors.push("`regex_pattern` is a non-empty string".to_string());
        }
        if self.text.trim().is_empty() {
            errors.push("`text` is a non-empty plain-text headline".to_string());
        }
        let mut actions = Vec::new();
        if let Some(action) = &self.action {
            match action.kind {
                ActionKind::InstallContent => {
                    if action
                        .content_key
                        .as_deref()
                        .is_none_or(|key| key.trim().is_empty())
                    {
                        errors.push(
                            "`action.content_key` is required for `install_content`".to_string(),
                        );
                    }
                }
                _ if action.content_key.is_some() => {
                    errors.push("only `install_content` uses `action.content_key`".to_string());
                }
                _ => {}
            }
            if action
                .label
                .as_deref()
                .is_some_and(|label| label.trim().is_empty())
            {
                errors.push("`action.label` is a non-empty string when present".to_string());
            }
            actions.push(ActionRequest {
                kind: action.kind,
                content_key: action.content_key.clone(),
                label: action
                    .label
                    .as_ref()
                    .map(|label| LocalText::from([("en".to_string(), label.clone())])),
            });
        }
        if !errors.is_empty() {
            return Err(errors);
        }

        let instance_game = |test: MatchOps| {
            Condition::Fact(FactQuery {
                kind: "core.instance".to_string(),
                filter: BTreeMap::from([("game".to_string(), AttrMatch::Test(test))]),
                reference: None,
                bind: BTreeMap::new(),
            })
        };
        // One of its games, or a loose file with no instance at all. A loose file is still kept to
        // the right game: the community packs are split by game set, and the engine only lets a
        // pack read a loose file when one of its games' parsers read it.
        let game_condition = Condition::Any(vec![
            instance_game(MatchOps {
                any_of: Some(self.games.iter().cloned().map(Scalar::Text).collect()),
                ..MatchOps::default()
            }),
            instance_game(MatchOps {
                present: Some(false),
                ..MatchOps::default()
            }),
        ]);
        // The text match is only allowed to stand on its own when the game is known, so the
        // game condition is the anchor the linter asks for.
        let when = Condition::All(vec![
            game_condition,
            Condition::Text(TextMatch {
                regex: self.regex_pattern.clone(),
                ..TextMatch::default()
            }),
        ]);

        Ok(Rule {
            id: self.id.clone(),
            version: 1,
            scope: RuleScope::default(),
            when,
            verify: None,
            effects: vec![Effect::Finding {
                scope: Scope::Mechanism,
                level,
                cause: Some(self.cause.unwrap_or(Cause::Other)),
                candidate: None,
                family: None,
                weight: None,
                text: None,
            }],
            text: RuleText {
                headline: LocalText::from([("en".to_string(), self.text.clone())]),
                why_not_sure: None,
            },
            supersedes: Vec::new(),
            repairs: Vec::new(),
            actions,
            next_step: None,
            fixtures: Vec::new(),
            reference: Some(format!("crash-signatures/{}.json", self.id)),
            max_branches: None,
        })
    }
}

/// Read the community signatures from an already-open, verified `registry.db` and make one loaded
/// pack from the rows that convert and lint clean.
///
/// A missing table means the catalog predates community signatures (or has none), so the pack is
/// simply absent. A row that fails is reported with its id and never stops the others.
pub fn load_community_pack(conn: &Connection) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    let table: Option<String> = match conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'crash_signatures'",
            [],
            |row| row.get(0),
        )
        .optional()
    {
        Ok(table) => table,
        Err(error) => {
            report.problems.push(PackProblem::new(
                COMMUNITY_PACK_PLUGIN_ID,
                COMMUNITY_PACK_FILE,
                "",
                "unreadable",
                format!("could not read the catalog's tables: {error}"),
            ));
            return report;
        }
    };
    if table.is_none() {
        return report;
    }

    let mut statement = match conn.prepare(
        "SELECT id, name, games_json, regex_pattern, level, cause, text,
                action_kind, action_content_key, action_label
           FROM crash_signatures
          ORDER BY id",
    ) {
        Ok(statement) => statement,
        Err(error) => {
            report.problems.push(PackProblem::new(
                COMMUNITY_PACK_PLUGIN_ID,
                COMMUNITY_PACK_FILE,
                "",
                "schema",
                format!(
                    "the catalog's crash_signatures table has no signature format v2 columns; \
                     update the catalog: {error}"
                ),
            ));
            return report;
        }
    };

    let rows = match statement.query_map([], |row| {
        let id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let games: String = row.get(2)?;
        let regex_pattern: String = row.get(3)?;
        let level: Option<String> = row.get(4)?;
        let cause: Option<String> = row.get(5)?;
        let text: String = row.get(6)?;
        let action_kind: Option<String> = row.get(7)?;
        let action_content_key: Option<String> = row.get(8)?;
        let action_label: Option<String> = row.get(9)?;
        Ok((
            id,
            name,
            games,
            regex_pattern,
            level,
            cause,
            text,
            action_kind,
            action_content_key,
            action_label,
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            report.problems.push(PackProblem::new(
                COMMUNITY_PACK_PLUGIN_ID,
                COMMUNITY_PACK_FILE,
                "",
                "unreadable",
                format!("could not read the catalog's crash signatures: {error}"),
            ));
            return report;
        }
    };

    // Rules grouped by the games they name, one pack per group, so the engine's own game filter
    // (an instance's game, or the games whose parsers read a loose file) applies to them.
    let mut rules: BTreeMap<Vec<String>, Vec<Rule>> = BTreeMap::new();
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(error) => {
                report.problems.push(PackProblem::new(
                    COMMUNITY_PACK_PLUGIN_ID,
                    COMMUNITY_PACK_FILE,
                    "",
                    "unreadable",
                    format!("could not read one crash signature row: {error}"),
                ));
                continue;
            }
        };
        let (id, signature) = match community_signature_from_row(row) {
            Ok(pair) => pair,
            Err((id, message)) => {
                report.problems.push(PackProblem::new(
                    COMMUNITY_PACK_PLUGIN_ID,
                    COMMUNITY_PACK_FILE,
                    format!("signature `{id}`"),
                    "signature",
                    format!("could not check this signature: {message}"),
                ));
                continue;
            }
        };
        let rule = match signature.to_rule() {
            Ok(rule) => rule,
            Err(errors) => {
                report.problems.push(PackProblem::new(
                    COMMUNITY_PACK_PLUGIN_ID,
                    COMMUNITY_PACK_FILE,
                    format!("signature `{id}`"),
                    "signature",
                    format!("could not check this signature: {}", errors.join("; ")),
                ));
                continue;
            }
        };
        let problems = lint_rule(&rule);
        if !problems.is_empty() {
            let message = problems
                .iter()
                .map(|problem| problem.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            report.problems.push(PackProblem::new(
                COMMUNITY_PACK_PLUGIN_ID,
                COMMUNITY_PACK_FILE,
                format!("signature `{id}`"),
                "lint",
                format!("could not check this signature: {message}"),
            ));
            continue;
        }
        let mut games = signature.games.clone();
        games.sort();
        games.dedup();
        rules.entry(games).or_default().push(rule);
    }

    for (games, rules) in rules {
        report.packs.push(LoadedCrashPack {
            plugin_id: COMMUNITY_PACK_PLUGIN_ID.to_string(),
            plugin_name: COMMUNITY_PACK_PLUGIN_NAME.to_string(),
            plugin_version: String::new(),
            dependencies: Vec::new(),
            official: false,
            file: COMMUNITY_PACK_FILE.to_string(),
            pack: CrashPack {
                contract: CRASH_CONTRACT_VERSION,
                games,
                sources: Vec::new(),
                parsers: Vec::new(),
                inspectors: Vec::new(),
                ref_maps: Vec::new(),
                roles: Vec::new(),
                save_risk: None,
                rules,
            },
        });
    }

    report
}

type SignatureRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Turn one stored row back into a v2 signature. A malformed cell is a problem with that row's
/// id, never a panic or a silently dropped row.
fn community_signature_from_row(
    (
        id,
        name,
        games,
        regex_pattern,
        level,
        cause,
        text,
        action_kind,
        action_content_key,
        action_label,
    ): SignatureRow,
) -> Result<(String, CommunitySignature), (String, String)> {
    let bad = |message: String| (id.clone(), message);
    let games: Vec<String> = serde_json::from_str(&games).map_err(|error| {
        bad(format!(
            "`games_json` is not a JSON array of game ids: {error}"
        ))
    })?;
    let action = match action_kind {
        None => None,
        Some(kind) => {
            let kind: ActionKind = serde_json::from_value(serde_json::Value::String(kind))
                .map_err(|error| bad(format!("`action_kind`: {error}")))?;
            Some(CommunityAction {
                kind,
                content_key: action_content_key,
                label: action_label,
            })
        }
    };
    let level = match level {
        None => None,
        Some(level) => Some(
            serde_json::from_value::<Level>(serde_json::Value::String(level))
                .map_err(|error| bad(format!("`level`: {error}")))?,
        ),
    };
    let cause = match cause {
        None => None,
        Some(cause) => Some(
            serde_json::from_value::<Cause>(serde_json::Value::String(cause))
                .map_err(|error| bad(format!("`cause`: {error}")))?,
        ),
    };
    Ok((
        id.clone(),
        CommunitySignature {
            id,
            name,
            games,
            regex_pattern,
            level,
            cause,
            text,
            action,
        },
    ))
}

/// Load the community pack from the cached, verified catalog. A missing or unsigned catalog is
/// absent rather than an error, because the catalog has its own update path; anything else is a
/// reported problem.
pub fn load_community_pack_from_registry(app_data_dir: &Path) -> CrashPackReport {
    match crate::registry_sync::open_verified_registry(app_data_dir) {
        Ok(registry) => load_community_pack(registry.connection()),
        Err(crate::error::LauncherError::RegistryMissing)
        | Err(crate::error::LauncherError::RegistrySignatureInvalid) => CrashPackReport::default(),
        Err(error) => {
            let mut report = CrashPackReport::default();
            report.problems.push(PackProblem::new(
                COMMUNITY_PACK_PLUGIN_ID,
                COMMUNITY_PACK_FILE,
                "",
                "unreadable",
                format!("could not open the signed catalog: {error}"),
            ));
            report
        }
    }
}

/// A pack that parsed and linted clean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedCrashPack {
    pub plugin_id: String,
    /// The plugin's display name, so an answer can name it to a person. Defaults to the id for a
    /// report written before this field existed.
    #[serde(default)]
    pub plugin_name: String,
    #[serde(default)]
    pub plugin_version: String,
    /// The plugin ids this pack's plugin declares as dependencies. A source may name a parser from
    /// one of them as `"<plugin id>:<parser id>"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// This copy is verified as an official package. It only decides which primary reads a file
    /// when more than one claims it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub official: bool,
    pub file: String,
    pub pack: CrashPack,
}

/// What loading every pack told us. Problems are not fatal to the report:
/// packs that did load stay loaded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashPackReport {
    pub packs: Vec<LoadedCrashPack>,
    pub problems: Vec<PackProblem>,
}

impl CrashPackReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn for_plugin<'a>(&'a self, plugin_id: &'a str) -> impl Iterator<Item = &'a PackProblem> {
        self.problems
            .iter()
            .filter(move |problem| problem.plugin_id == plugin_id)
    }
}

/// One plugin folder to read packs from.
pub struct PluginPackSource<'a> {
    pub plugin_id: &'a str,
    pub root: &'a Path,
    pub manifest: &'a PluginManifest,
    /// This copy is verified as an official package.
    pub official: bool,
}

/// Load packs from several plugins. One plugin's failure never stops another's.
pub fn load_for_plugins(sources: &[PluginPackSource<'_>]) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    for source in sources {
        let one = load_plugin_packs_as(
            source.plugin_id,
            source.root,
            source.manifest,
            source.official,
        );
        report.packs.extend(one.packs);
        report.problems.extend(one.problems);
    }
    report
}

/// Load every pack a plugin's manifest names.
///
/// A pack without `crash:contribute` is refused. A path that escapes the
/// plugin folder is refused even when the manifest parser already rejected it,
/// because this is the code that actually opens files.
pub fn load_plugin_packs(
    plugin_id: &str,
    root: &Path,
    manifest: &PluginManifest,
) -> CrashPackReport {
    load_plugin_packs_as(plugin_id, root, manifest, false)
}

/// Like [`load_plugin_packs`], with whether this copy is verified as an official package. That
/// flag only orders two primaries that claim the same file, and it defaults to false for a
/// development folder.
pub fn load_plugin_packs_as(
    plugin_id: &str,
    root: &Path,
    manifest: &PluginManifest,
    official: bool,
) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    if manifest.contributions.crash_packs.is_empty() {
        return report;
    }

    let allowed = manifest
        .capabilities
        .required
        .iter()
        .any(|name| name == "crash:contribute");
    if !allowed {
        for contribution in &manifest.contributions.crash_packs {
            report.problems.push(PackProblem::new(
                plugin_id,
                contribution.path.clone(),
                "",
                "capability",
                "reading crash evidence needs the `crash:contribute` capability",
            ));
        }
        return report;
    }

    let root_real = match root.canonicalize() {
        Ok(root_real) => root_real,
        Err(error) => {
            // Without the folder's real path the escape check below can't run, so nothing loads.
            for contribution in &manifest.contributions.crash_packs {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    contribution.path.clone(),
                    "",
                    "path",
                    format!("could not resolve the plugin folder: {error}"),
                ));
            }
            return report;
        }
    };

    for contribution in &manifest.contributions.crash_packs {
        let relative = contribution.path.clone();
        if let Err(error) = validate_package_path("crash pack path", &relative) {
            report.problems.push(PackProblem::new(
                plugin_id,
                relative,
                "",
                "path",
                error.message,
            ));
            continue;
        }

        let target = root.join(relative.replace('\\', "/"));
        let Ok(target_real) = target.canonicalize() else {
            report.problems.push(PackProblem::new(
                plugin_id,
                relative,
                "",
                "missing",
                "the file named in `contributions.crashPacks` is not there",
            ));
            continue;
        };
        if !target_real.starts_with(&root_real) {
            report.problems.push(PackProblem::new(
                plugin_id,
                relative,
                "",
                "path",
                "the pack path resolves outside the plugin folder",
            ));
            continue;
        }

        let bytes = match std::fs::read(&target_real) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    relative,
                    "",
                    "unreadable",
                    format!("could not read the pack: {error}"),
                ));
                continue;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                report.problems.push(PackProblem::new(
                    plugin_id,
                    relative,
                    "",
                    "unreadable",
                    "the pack is not UTF-8 text",
                ));
                continue;
            }
        };

        match lint_pack_json(&text) {
            Ok(pack) => {
                let dependencies: Vec<String> = manifest
                    .dependencies
                    .keys()
                    .map(|id| id.as_str().to_string())
                    .collect();
                let config_problems =
                    parser_config_problems(plugin_id, &relative, &pack, false, &dependencies);
                if config_problems.is_empty() {
                    report.packs.push(LoadedCrashPack {
                        plugin_id: plugin_id.to_string(),
                        plugin_name: manifest.name.clone(),
                        plugin_version: manifest.version.to_string(),
                        dependencies: dependencies.clone(),
                        official,
                        file: relative,
                        pack,
                    });
                } else {
                    report.problems.extend(config_problems);
                }
            }
            Err(problems) => {
                for problem in problems {
                    report.problems.push(PackProblem::new(
                        plugin_id,
                        relative.clone(),
                        problem.at,
                        format!("{:?}", problem.code).to_lowercase(),
                        problem.message,
                    ));
                }
            }
        }
    }

    report
}

/// Load the packs a compiled game package supplies. The JSON is linted by the same linter a
/// plugin's pack goes through, and each `built_in` parser must have its code supplied beside it.
pub fn load_built_in_packs(packs: &[BuiltInPack]) -> CrashPackReport {
    let mut report = CrashPackReport::default();
    for built_in in packs {
        match lint_pack_json(&built_in.json) {
            Ok(pack) => {
                let mut problems =
                    parser_config_problems(BUILT_IN_PLUGIN_ID, &built_in.file, &pack, true, &[]);
                for parser in &pack.parsers {
                    if let CodeEntry::BuiltIn { name } = &parser.entry {
                        if !built_in
                            .parsers
                            .iter()
                            .any(|(supplied, _)| supplied == name)
                        {
                            problems.push(PackProblem::new(
                                BUILT_IN_PLUGIN_ID,
                                built_in.file.clone(),
                                format!("parser `{}`", parser.id),
                                "missing_parser",
                                format!("no built-in parser named `{name}` was supplied"),
                            ));
                        }
                    }
                }
                if problems.is_empty() {
                    let identity = built_in_identity();
                    report.packs.push(LoadedCrashPack {
                        plugin_id: identity.id,
                        plugin_name: identity.name,
                        plugin_version: identity.version,
                        dependencies: Vec::new(),
                        official: false,
                        file: built_in.file.clone(),
                        pack,
                    });
                } else {
                    report.problems.extend(problems);
                }
            }
            Err(problems) => {
                for problem in problems {
                    report.problems.push(PackProblem::new(
                        BUILT_IN_PLUGIN_ID,
                        built_in.file.clone(),
                        problem.at,
                        format!("{:?}", problem.code).to_lowercase(),
                        problem.message,
                    ));
                }
            }
        }
    }
    report
}

/// Check every parser entry that needs code on this side. A plugin may not name a parser compiled
/// into Agora; only a built-in pack may, and then its code must come with it.
fn parser_config_problems(
    plugin_id: &str,
    file: &str,
    pack: &CrashPack,
    allow_built_in: bool,
    dependencies: &[String],
) -> Vec<PackProblem> {
    let mut problems = Vec::new();
    for source in &pack.sources {
        let Some(format) = &source.format else {
            continue;
        };
        if let Some((from_plugin, parser_id)) = format.split_once(':') {
            if from_plugin != plugin_id && !dependencies.iter().any(|id| id == from_plugin) {
                problems.push(PackProblem::new(
                    plugin_id,
                    file,
                    format!("source `{}`", source.id),
                    "parser_dependency",
                    format!(
                        "`{format}` names a parser from `{from_plugin}`, which is not in this \
                         plugin's `dependencies`; add it there, or use a parser from this plugin"
                    ),
                ));
            }
            if parser_id.is_empty() || from_plugin.is_empty() {
                problems.push(PackProblem::new(
                    plugin_id,
                    file,
                    format!("source `{}`", source.id),
                    "parser_dependency",
                    "a parser reference is `\"<plugin id>:<parser id>\"`, with both parts present",
                ));
            }
        }
    }
    for parser in &pack.parsers {
        match &parser.entry {
            CodeEntry::Generic { name, config } => {
                if let Err(message) = crate::crash_engine::parsers::validate_config(name, config) {
                    problems.push(PackProblem::new(
                        plugin_id,
                        file,
                        format!("parser `{}`", parser.id),
                        "parser_config",
                        message,
                    ));
                }
            }
            CodeEntry::BuiltIn { .. } if !allow_built_in => {
                problems.push(PackProblem::new(
                    plugin_id,
                    file,
                    format!("parser `{}`", parser.id),
                    "built_in",
                    "a plugin's pack cannot name a parser compiled into Agora; ship a script or a generic parser",
                ));
            }
            _ => {}
        }
    }
    problems
}

/// A parser reference that resolved: the pack that owns the parser and the parser itself.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedParserRef<'a> {
    pub pack: &'a LoadedCrashPack,
    pub parser: &'a ParserDecl,
}

/// Resolve a source's `format` against the loaded packs. A plain id is local to `owner`; a
/// `"<plugin id>:<parser id>"` reference must name a plugin in `owner`'s declared dependencies.
pub fn resolve_parser_ref<'a>(
    packs: &'a [LoadedCrashPack],
    owner: &'a LoadedCrashPack,
    format: &str,
) -> Result<ResolvedParserRef<'a>, String> {
    let (plugin_id, parser_id) = match format.split_once(':') {
        Some((plugin_id, parser_id)) => (plugin_id, parser_id),
        None => (owner.plugin_id.as_str(), format),
    };
    if plugin_id.is_empty() || parser_id.is_empty() {
        return Err(format!(
            "`{format}` is not a parser reference; it is `\"<parser id>\"` or `\"<plugin id>:<parser id>\"`"
        ));
    }
    let pack = if plugin_id == owner.plugin_id {
        owner
    } else {
        if !owner.dependencies.iter().any(|id| id == plugin_id) {
            return Err(format!(
                "`{format}` names a parser from `{plugin_id}`, which is not a declared dependency of `{}`",
                owner.plugin_id
            ));
        }
        packs
            .iter()
            .find(|pack| pack.plugin_id == plugin_id)
            .ok_or_else(|| format!("no loaded pack belongs to `{plugin_id}`"))?
    };
    let parser = pack
        .pack
        .parsers
        .iter()
        .find(|parser| parser.id == parser_id)
        .ok_or_else(|| format!("`{plugin_id}` has no parser `{parser_id}`"))?;
    Ok(ResolvedParserRef { pack, parser })
}

/// Validate a development folder's packs. Used by `agora plugin validate`.
pub fn validate_plugin_folder(folder: &Path) -> LauncherResult<CrashPackReport> {
    let manifest = crate::plugins::install::read_folder_manifest(folder)?;
    Ok(load_plugin_packs(manifest.id.as_str(), folder, &manifest))
}

// ---------------------------------------------------------------------------
// Scoped reads
// ---------------------------------------------------------------------------

/// The artifact bytes a parser may reach, held in memory.
///
/// Deliberately not a handle to a file: the engine decides what is in scope,
/// and a parser has no way to ask for anything else.
pub struct MemoryArtifact {
    files: BTreeMap<String, Arc<Vec<u8>>>,
    limits: ParserLimits,
    total_read: AtomicU64,
}

impl MemoryArtifact {
    pub fn new(limits: ParserLimits) -> Self {
        Self {
            files: BTreeMap::new(),
            limits,
            total_read: AtomicU64::new(0),
        }
    }

    /// Add an artifact by id. The first id is usually the root.
    pub fn insert(&mut self, id: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.files.insert(id.into(), Arc::new(bytes.into()));
    }

    fn budget(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total_read.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(refused(
                    ReadRefusal::BudgetSpent,
                    "this parse has read its whole byte budget",
                ));
            }
            match self.total_read.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn file(&self, id: &str) -> Result<&Arc<Vec<u8>>, ScriptReadError> {
        self.files.get(id).ok_or_else(|| {
            refused(
                ReadRefusal::OutOfScope,
                format!("`{id}` is not part of this parse"),
            )
        })
    }
}

fn refused(reason: ReadRefusal, message: impl Into<String>) -> ScriptReadError {
    let reply = ReadRefused {
        reason,
        message: message.into(),
    };
    let code = match reply.reason {
        ReadRefusal::OutOfScope => "out_of_scope",
        ReadRefusal::TooLarge => "too_large",
        ReadRefusal::BudgetSpent => "budget_spent",
        ReadRefusal::NotRetained => "not_retained",
        ReadRefusal::Unreadable => "unreadable",
    };
    ScriptReadError::new(code, reply.message)
}

impl ScriptReads for MemoryArtifact {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        let data = self.file(artifact)?;
        if len > self.limits.max_read_bytes {
            return Err(refused(
                ReadRefusal::TooLarge,
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let wanted = usize::try_from(len).unwrap_or(usize::MAX);
        let end = start.saturating_add(wanted).min(data.len());
        let slice = &data[start..end];
        self.budget(slice.len() as u64)?;
        Ok(slice.to_vec())
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let data = self.file(artifact)?;
        let max_lines = max_lines.min(self.limits.max_lines);
        let max_len = self.limits.max_line_len as usize;

        // The contract says UTF-8, else the system's code page. A code page is
        // platform-specific; lossy UTF-8 keeps every byte's intent without
        // pretending to know it. Unverified for legacy code pages.
        let text = String::from_utf8_lossy(data);
        let mut lines = Vec::new();
        let mut cut = false;
        let mut index = 0u64;
        let mut more = false;

        // An empty artifact has no lines, and a trailing newline does not
        // invent a final empty one.
        let raw: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            let mut parts: Vec<&str> = text.split('\n').collect();
            if parts.last() == Some(&"") {
                parts.pop();
            }
            parts
        };

        for line in raw {
            if index < from_line {
                index += 1;
                continue;
            }
            if lines.len() >= max_lines as usize {
                more = true;
                break;
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (line, was_cut) = match line.char_indices().nth(max_len) {
                Some((byte, _)) => (&line[..byte], true),
                None => (line, false),
            };
            cut |= was_cut;
            lines.push(line.to_string());
            index += 1;
        }

        let reply = LinesReply {
            lines,
            next_line: more.then_some(index),
            cut,
            gap: None,
        };
        serde_json::to_value(reply).map_err(|error| {
            ScriptReadError::new("unreadable", format!("could not encode lines: {error}"))
        })
    }
}

/// Applies a pack's declared lower limits to a reader chosen by the engine.
///
/// The contract says a plugin may ask for less than the host allows, never
/// more. The engine can only know the pack's request here, so the wrapper is
/// where the two meet.
struct LimitedReads {
    inner: Arc<dyn ScriptReads>,
    limits: ParserLimits,
    total: AtomicU64,
}

impl LimitedReads {
    fn new(inner: Arc<dyn ScriptReads>, limits: ParserLimits) -> Self {
        Self {
            inner,
            limits,
            total: AtomicU64::new(0),
        }
    }

    fn spend(&self, wanted: u64) -> Result<(), ScriptReadError> {
        let mut current = self.total.load(Ordering::SeqCst);
        loop {
            if current.saturating_add(wanted) > self.limits.max_total_read_bytes {
                return Err(refused(
                    ReadRefusal::BudgetSpent,
                    "this parse has read its whole byte budget",
                ));
            }
            match self.total.compare_exchange(
                current,
                current + wanted,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }
}

impl ScriptReads for LimitedReads {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ScriptReadError> {
        if len > self.limits.max_read_bytes {
            return Err(refused(
                ReadRefusal::TooLarge,
                format!(
                    "asked for {len} bytes; one read may return {}",
                    self.limits.max_read_bytes
                ),
            ));
        }
        let bytes = self.inner.read_bytes(artifact, offset, len)?;
        self.spend(bytes.len() as u64)?;
        Ok(bytes)
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<serde_json::Value, ScriptReadError> {
        let max_lines = max_lines.min(self.limits.max_lines);
        let mut reply: LinesReply =
            serde_json::from_value(self.inner.read_lines(artifact, from_line, max_lines)?)
                .map_err(|error| {
                    ScriptReadError::new(
                        "unreadable",
                        format!("the lines reply was malformed: {error}"),
                    )
                })?;
        reply.lines.truncate(max_lines as usize);
        let max_len = self.limits.max_line_len as usize;
        for line in &mut reply.lines {
            if let Some((cut_at, _)) = line.char_indices().nth(max_len) {
                line.truncate(cut_at);
                reply.cut = true;
            }
        }
        serde_json::to_value(reply).map_err(|error| {
            ScriptReadError::new("unreadable", format!("could not encode lines: {error}"))
        })
    }
}

/// The same scoped reads a script sees, handed to a parser compiled into the binary.
struct ReadsForBuiltIn<'a> {
    inner: &'a dyn ScriptReads,
}

impl ParserReads for ReadsForBuiltIn<'_> {
    fn read_bytes(
        &self,
        artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ParserReadError> {
        self.inner
            .read_bytes(artifact, offset, len)
            .map_err(|error| ParserReadError::new(error.code, error.message))
    }

    fn read_lines(
        &self,
        artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<LinesReply, ParserReadError> {
        let value = self
            .inner
            .read_lines(artifact, from_line, max_lines)
            .map_err(|error| ParserReadError::new(error.code, error.message))?;
        serde_json::from_value(value).map_err(|error| {
            ParserReadError::new(
                "unreadable",
                format!("the lines reply was malformed: {error}"),
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Choose the parsers whose `detect` accepts this artifact as a whole file.
///
/// A `snippet` parser is not included: it may read only pasted text, and the analyzer asks for it
/// separately with [`detect_parsers_with_snippets`]. An empty detect means "only a source's
/// `format` selects me", so it never matches here. A parser whose `detect` is malformed is
/// reported, not ignored: the pack should have been linted, and a silent skip would hide the
/// mistake.
pub fn detect_parsers<'a>(
    pack: &'a CrashPack,
    name: &str,
    head: &[u8],
) -> Result<Vec<&'a ParserDecl>, ParserFailure> {
    detect_parsers_for(pack, name, head, false)
}

/// Like [`detect_parsers`], with the `snippet` parsers too. The analyzer uses this to find a
/// fragment fallback for pasted text that no primary parser claimed.
pub fn detect_parsers_with_snippets<'a>(
    pack: &'a CrashPack,
    name: &str,
    head: &[u8],
) -> Result<Vec<&'a ParserDecl>, ParserFailure> {
    detect_parsers_for(pack, name, head, true)
}

fn detect_parsers_for<'a>(
    pack: &'a CrashPack,
    name: &str,
    head: &[u8],
    include_snippets: bool,
) -> Result<Vec<&'a ParserDecl>, ParserFailure> {
    let head = &head[..head.len().min(DETECT_HEAD_BYTES)];
    // Patterns see `\n` line ends whatever the file used: in this regex dialect `$` never matches
    // before `\r`, so `(?m)^banner$` would silently miss every Windows log.
    let text = String::from_utf8_lossy(head).replace("\r\n", "\n");
    let mut matched = Vec::new();
    for parser in &pack.parsers {
        if parser.detect.is_empty() {
            continue;
        }
        if !include_snippets && parser.reads.is_snippet() {
            continue;
        }
        if detect_matches(&parser.detect, name, head, &text).map_err(|error| ParserFailure {
            plugin_id: String::new(),
            parser_id: parser.id.clone(),
            kind: ParserFailureKind::InvalidDetection,
            message: error,
        })? {
            matched.push(parser);
        }
    }
    Ok(matched)
}

fn detect_matches(detect: &Detect, name: &str, head: &[u8], text: &str) -> Result<bool, String> {
    if let Some(glob) = &detect.name_glob {
        if !glob_matches(glob, name) {
            return Ok(false);
        }
    }
    if let Some(magic) = &detect.magic_hex {
        let magic = magic.to_ascii_lowercase();
        let prefix = &head[..head.len().min(magic.len() / 2)];
        let actual: String = prefix.iter().map(|byte| format!("{byte:02x}")).collect();
        if !magic.starts_with(&actual) || actual.len() != magic.len() {
            return Ok(false);
        }
    }
    for pattern in &detect.all {
        let regex = Regex::new(pattern).map_err(|error| format!("`{pattern}`: {error}"))?;
        if !regex.is_match(text) {
            return Ok(false);
        }
    }
    if !detect.any.is_empty() {
        let mut any = false;
        for pattern in &detect.any {
            let regex = Regex::new(pattern).map_err(|error| format!("`{pattern}`: {error}"))?;
            if regex.is_match(text) {
                any = true;
                break;
            }
        }
        if !any {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Case-insensitive `*` and `?`, over the name only.
pub(crate) fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

// ---------------------------------------------------------------------------
// Running parsers
// ---------------------------------------------------------------------------

/// Why a parser did not produce a usable output. Names the plugin and parser,
/// because that is the only thing that makes the failure actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParserFailure {
    pub plugin_id: String,
    pub parser_id: String,
    pub kind: ParserFailureKind,
    pub message: String,
}

impl ParserFailure {
    pub fn new(
        plugin_id: impl Into<String>,
        parser_id: impl Into<String>,
        kind: ParserFailureKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            parser_id: parser_id.into(),
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParserFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "parser `{}` from `{}` could not check this file ({:?}): {}",
            self.parser_id, self.plugin_id, self.kind, self.message
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserFailureKind {
    /// The plugin is not running, or its runtime is gone.
    NotRunning,
    /// The export the pack names does not exist.
    MissingExport,
    /// The script threw.
    ScriptThrew,
    /// The call exceeded the pack's wall-clock limit.
    TimedOut,
    /// The script exceeded its memory ceiling.
    OutOfMemory,
    /// The call was cancelled (user stop, shutdown).
    Cancelled,
    /// This build does not run WebAssembly or generic entries yet.
    UnsupportedEntry,
    /// A `detect` regex is invalid.
    InvalidDetection,
    /// The output was not JSON, or not the contract's shape.
    InvalidOutput,
    /// The output stated another contract version.
    WrongContract,
    /// The output was larger than [`MAX_PARSER_OUTPUT_BYTES`].
    OutputTooLarge,
    /// The host itself failed.
    Internal,
}

/// What one parser said.
#[derive(Debug, Clone, PartialEq)]
pub enum ParserDispatch {
    /// The output is valid and describes this format.
    Parsed(ParserOutput),
    /// The parser looked and said this is not its format; another may try.
    WrongFormat(ParserOutput),
    /// It could not check the file. Never "nothing found".
    Failed(ParserFailure),
}

/// One attempt, under the parser's id.
#[derive(Debug, Clone, PartialEq)]
pub struct ParserAttempt {
    pub parser_id: String,
    pub outcome: ParserDispatch,
}

/// Every attempt made on one artifact.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtifactParse {
    pub attempts: Vec<ParserAttempt>,
    /// No parser's `detect` accepted the artifact. Distinct from failure.
    pub no_parser: bool,
}

impl ArtifactParse {
    /// The first valid, on-format output, if any.
    pub fn parsed(&self) -> Option<&ParserOutput> {
        self.attempts
            .iter()
            .find_map(|attempt| match &attempt.outcome {
                ParserDispatch::Parsed(output) => Some(output),
                _ => None,
            })
    }

    pub fn failures(&self) -> impl Iterator<Item = &ParserFailure> {
        self.attempts
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                ParserDispatch::Failed(failure) => Some(failure),
                _ => None,
            })
    }
}

/// Run the parsers that accept this artifact. A failure in one does not stop
/// the others.
///
/// `format` is the source's `format`, which selects one parser by id and skips
/// detection. Without it, detection reads at most [`DETECT_HEAD_BYTES`].
pub fn parse_artifact(
    host: &dyn ScriptHost,
    plugin_id: &str,
    pack: &CrashPack,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
    format: Option<&str>,
    head: &[u8],
) -> ArtifactParse {
    parse_artifact_with_builtin(host, plugin_id, pack, request, reads, format, head, None)
}

/// Like [`parse_artifact`], for a pack whose parsers are compiled into the binary. `built_in` is
/// the pack that supplied `pack`'s JSON; a plugin's pack passes `None`.
#[allow(clippy::too_many_arguments)]
pub fn parse_artifact_with_builtin(
    host: &dyn ScriptHost,
    plugin_id: &str,
    pack: &CrashPack,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
    format: Option<&str>,
    head: &[u8],
    built_in: Option<&BuiltInPack>,
) -> ArtifactParse {
    let selected: Vec<&ParserDecl> = match format {
        Some(id) => match pack.parsers.iter().find(|parser| parser.id == id) {
            Some(parser) => vec![parser],
            None => {
                return ArtifactParse {
                    attempts: vec![ParserAttempt {
                        parser_id: id.to_string(),
                        outcome: ParserDispatch::Failed(ParserFailure::new(
                            plugin_id,
                            id,
                            ParserFailureKind::MissingExport,
                            format!(
                            "the source selects parser `{id}`, which this pack does not declare"
                        ),
                        )),
                    }],
                    no_parser: false,
                }
            }
        },
        None => {
            let head = &head[..head.len().min(DETECT_HEAD_BYTES)];
            match detect_parsers(pack, &request.artifact.name, head) {
                Ok(parsers) => parsers,
                Err(mut failure) => {
                    failure.plugin_id = plugin_id.to_string();
                    return ArtifactParse {
                        attempts: vec![ParserAttempt {
                            parser_id: failure.parser_id.clone(),
                            outcome: ParserDispatch::Failed(failure),
                        }],
                        no_parser: false,
                    };
                }
            }
        }
    };

    if selected.is_empty() {
        return ArtifactParse {
            attempts: Vec::new(),
            no_parser: true,
        };
    }

    let mut attempts = Vec::new();
    for parser in selected {
        let outcome = run_one(host, plugin_id, parser, request, reads.clone(), built_in);
        attempts.push(ParserAttempt {
            parser_id: parser.id.clone(),
            outcome,
        });
    }
    ArtifactParse {
        attempts,
        no_parser: false,
    }
}

/// Run one parser the caller already chose, through the same host, limits and validator a detected
/// parser gets. Used when the engine picks one primary and every extra for a file itself.
pub fn run_parser_with_builtin(
    host: &dyn ScriptHost,
    plugin_id: &str,
    parser: &ParserDecl,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
    built_in: Option<&BuiltInPack>,
) -> ParserAttempt {
    ParserAttempt {
        parser_id: parser.id.clone(),
        outcome: run_one(host, plugin_id, parser, request, reads, built_in),
    }
}

fn run_one(
    host: &dyn ScriptHost,
    plugin_id: &str,
    parser: &ParserDecl,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
    built_in: Option<&BuiltInPack>,
) -> ParserDispatch {
    // A pack may ask for less than the engine allows, never more.
    let limits = parser
        .limits
        .map_or(request.limits, |asked| asked.lower_of(request.limits));
    let effective = ParserRequest {
        parser: parser.id.clone(),
        limits,
        ..request.clone()
    };
    let request = &effective;
    let reads: Arc<dyn ScriptReads> = Arc::new(LimitedReads::new(reads, limits));

    let value: serde_json::Value = match &parser.entry {
        CodeEntry::Script { export } => {
            let Ok(id) = agora_plugin_api::manifest::PluginId::parse(plugin_id) else {
                return ParserDispatch::Failed(ParserFailure::new(
                    plugin_id,
                    &parser.id,
                    ParserFailureKind::Internal,
                    "the plugin id is not a valid id",
                ));
            };
            let args = match serde_json::to_value(request) {
                Ok(args) => args,
                Err(error) => {
                    return ParserDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &parser.id,
                        ParserFailureKind::Internal,
                        format!("could not encode the parser request: {error}"),
                    ))
                }
            };
            let timeout = Duration::from_millis(request.limits.max_ms);
            match host.invoke_with_reads(&id, export, args, timeout, reads) {
                Ok(value) => value,
                Err(error) => {
                    let kind = match error.code {
                        agora_plugin_api::PluginErrorCode::Timeout => ParserFailureKind::TimedOut,
                        agora_plugin_api::PluginErrorCode::ResourceExhausted => {
                            ParserFailureKind::OutOfMemory
                        }
                        agora_plugin_api::PluginErrorCode::Cancelled => {
                            ParserFailureKind::Cancelled
                        }
                        agora_plugin_api::PluginErrorCode::NotActivated => {
                            ParserFailureKind::NotRunning
                        }
                        agora_plugin_api::PluginErrorCode::ScriptError
                            if error.message.contains("exports no function named") =>
                        {
                            ParserFailureKind::MissingExport
                        }
                        agora_plugin_api::PluginErrorCode::ScriptError => {
                            if error.message.contains("interrupted") {
                                ParserFailureKind::TimedOut
                            } else {
                                ParserFailureKind::ScriptThrew
                            }
                        }
                        _ => ParserFailureKind::Internal,
                    };
                    return ParserDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &parser.id,
                        kind,
                        error.message,
                    ));
                }
            }
        }
        CodeEntry::Wasm { .. } => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::UnsupportedEntry,
                "this build runs script parsers only; a WebAssembly host comes later",
            ))
        }
        CodeEntry::Generic { name, config } => {
            match crate::crash_engine::parsers::run(name, config, request, reads.as_ref()) {
                Ok(output) => match serde_json::to_value(&output) {
                    Ok(value) => value,
                    Err(error) => {
                        return ParserDispatch::Failed(ParserFailure::new(
                            plugin_id,
                            &parser.id,
                            ParserFailureKind::Internal,
                            format!("could not encode the generic parser output: {error}"),
                        ))
                    }
                },
                Err(error) => {
                    let kind = match error.kind {
                        crate::crash_engine::parsers::GenericErrorKind::TooManyFacts => {
                            ParserFailureKind::InvalidOutput
                        }
                        _ => ParserFailureKind::Internal,
                    };
                    return ParserDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &parser.id,
                        kind,
                        format!("generic parser `{name}`: {}", error.message),
                    ));
                }
            }
        }
        CodeEntry::BuiltIn { name } => {
            let found = built_in.and_then(|supplied| {
                supplied
                    .parsers
                    .iter()
                    .find(|(supplied_name, _)| supplied_name == name)
                    .map(|(_, parser)| parser.as_ref())
            });
            let Some(found) = found else {
                return ParserDispatch::Failed(ParserFailure::new(
                    plugin_id,
                    &parser.id,
                    ParserFailureKind::UnsupportedEntry,
                    format!("no built-in parser named `{name}` was supplied for this pack"),
                ));
            };
            let adapter = ReadsForBuiltIn {
                inner: reads.as_ref(),
            };
            match found.parse(request, &adapter) {
                Ok(output) => match serde_json::to_value(&output) {
                    Ok(value) => value,
                    Err(error) => {
                        return ParserDispatch::Failed(ParserFailure::new(
                            plugin_id,
                            &parser.id,
                            ParserFailureKind::Internal,
                            format!("could not encode the built-in parser output: {error}"),
                        ))
                    }
                },
                Err(error) => {
                    return ParserDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &parser.id,
                        ParserFailureKind::Internal,
                        format!("built-in parser `{name}`: {}", error.message),
                    ));
                }
            }
        }
    };

    let size = match serde_json::to_vec(&value) {
        Ok(encoded) => encoded.len(),
        Err(error) => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::InvalidOutput,
                format!("the output was not JSON: {error}"),
            ))
        }
    };
    if size > MAX_PARSER_OUTPUT_BYTES {
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::OutputTooLarge,
            format!("the output is {size} bytes; the limit is {MAX_PARSER_OUTPUT_BYTES}"),
        ));
    }

    let output: ParserOutput = match serde_json::from_value(value) {
        Ok(output) => output,
        Err(error) => {
            return ParserDispatch::Failed(ParserFailure::new(
                plugin_id,
                &parser.id,
                ParserFailureKind::InvalidOutput,
                format!("the output is not the crash contract's parser shape: {error}"),
            ))
        }
    };

    if output.contract != CRASH_CONTRACT_VERSION {
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::WrongContract,
            format!(
                "output says contract {}, and this Agora reads {CRASH_CONTRACT_VERSION}",
                output.contract
            ),
        ));
    }

    let problems = if parser.reads.may_emit_core() {
        validate_parser_output(&output, &request.limits)
    } else {
        validate_extra_output(&output, &request.limits)
    };
    if !problems.is_empty() {
        let message = problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return ParserDispatch::Failed(ParserFailure::new(
            plugin_id,
            &parser.id,
            ParserFailureKind::InvalidOutput,
            message,
        ));
    }

    let wrong_format = output
        .notes
        .iter()
        .any(|note| note.kind == ParseNoteKind::WrongFormat);
    if wrong_format {
        ParserDispatch::WrongFormat(output)
    } else {
        ParserDispatch::Parsed(output)
    }
}

/// What one inspector said.
#[derive(Debug, Clone, PartialEq)]
pub enum InspectorDispatch {
    /// The output is valid and says what the inspector found.
    Inspected(InspectorOutput),
    /// It could not check the file. Never a pass.
    Failed(ParserFailure),
}

/// Run one inspector export through the same host and read scope a parser uses.
///
/// The request's artifact is the one file the rule pointed at, and `reads` reaches only it. A
/// script error, timeout, invalid output or wrong contract becomes [`InspectorDispatch::Failed`],
/// so the caller reports "could not check", naming the plugin.
pub fn run_inspector(
    host: &dyn ScriptHost,
    plugin_id: &str,
    inspector: &InspectorDecl,
    request: &ParserRequest,
    reads: Arc<dyn ScriptReads>,
) -> InspectorDispatch {
    let limits = request.limits;
    let effective = ParserRequest {
        parser: inspector.id.clone(),
        limits,
        ..request.clone()
    };
    let request = &effective;
    let reads: Arc<dyn ScriptReads> = Arc::new(LimitedReads::new(reads, limits));

    let value: serde_json::Value = match &inspector.entry {
        CodeEntry::Script { export } => {
            let Ok(id) = agora_plugin_api::manifest::PluginId::parse(plugin_id) else {
                return InspectorDispatch::Failed(ParserFailure::new(
                    plugin_id,
                    &inspector.id,
                    ParserFailureKind::Internal,
                    "the plugin id is not a valid id",
                ));
            };
            let args = match serde_json::to_value(request) {
                Ok(args) => args,
                Err(error) => {
                    return InspectorDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &inspector.id,
                        ParserFailureKind::Internal,
                        format!("could not encode the inspector request: {error}"),
                    ))
                }
            };
            let timeout = Duration::from_millis(request.limits.max_ms);
            match host.invoke_with_reads(&id, export, args, timeout, reads) {
                Ok(value) => value,
                Err(error) => {
                    let kind = match error.code {
                        agora_plugin_api::PluginErrorCode::Timeout => ParserFailureKind::TimedOut,
                        agora_plugin_api::PluginErrorCode::ResourceExhausted => {
                            ParserFailureKind::OutOfMemory
                        }
                        agora_plugin_api::PluginErrorCode::Cancelled => {
                            ParserFailureKind::Cancelled
                        }
                        agora_plugin_api::PluginErrorCode::NotActivated => {
                            ParserFailureKind::NotRunning
                        }
                        agora_plugin_api::PluginErrorCode::ScriptError
                            if error.message.contains("exports no function named") =>
                        {
                            ParserFailureKind::MissingExport
                        }
                        agora_plugin_api::PluginErrorCode::ScriptError => {
                            if error.message.contains("interrupted") {
                                ParserFailureKind::TimedOut
                            } else {
                                ParserFailureKind::ScriptThrew
                            }
                        }
                        _ => ParserFailureKind::Internal,
                    };
                    return InspectorDispatch::Failed(ParserFailure::new(
                        plugin_id,
                        &inspector.id,
                        kind,
                        error.message,
                    ));
                }
            }
        }
        _ => {
            return InspectorDispatch::Failed(ParserFailure::new(
                plugin_id,
                &inspector.id,
                ParserFailureKind::UnsupportedEntry,
                "this build runs script inspectors only",
            ))
        }
    };

    let size = match serde_json::to_vec(&value) {
        Ok(encoded) => encoded.len(),
        Err(error) => {
            return InspectorDispatch::Failed(ParserFailure::new(
                plugin_id,
                &inspector.id,
                ParserFailureKind::InvalidOutput,
                format!("the output was not JSON: {error}"),
            ))
        }
    };
    if size > MAX_PARSER_OUTPUT_BYTES {
        return InspectorDispatch::Failed(ParserFailure::new(
            plugin_id,
            &inspector.id,
            ParserFailureKind::OutputTooLarge,
            format!("the output is {size} bytes; the limit is {MAX_PARSER_OUTPUT_BYTES}"),
        ));
    }

    let output: InspectorOutput = match serde_json::from_value(value) {
        Ok(output) => output,
        Err(error) => {
            return InspectorDispatch::Failed(ParserFailure::new(
                plugin_id,
                &inspector.id,
                ParserFailureKind::InvalidOutput,
                format!("the output is not the crash contract's inspector shape: {error}"),
            ))
        }
    };
    if output.contract != CRASH_CONTRACT_VERSION {
        return InspectorDispatch::Failed(ParserFailure::new(
            plugin_id,
            &inspector.id,
            ParserFailureKind::WrongContract,
            format!(
                "output says contract {}, and this Agora reads {CRASH_CONTRACT_VERSION}",
                output.contract
            ),
        ));
    }
    let problems = validate_inspector_output(&output, &request.limits);
    if !problems.is_empty() {
        let message = problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return InspectorDispatch::Failed(ParserFailure::new(
            plugin_id,
            &inspector.id,
            ParserFailureKind::InvalidOutput,
            message,
        ));
    }
    InspectorDispatch::Inspected(output)
}

/// Build the handle a parser request carries for one artifact.
pub fn artifact_handle(
    id: impl Into<String>,
    name: impl Into<String>,
    size: u64,
    children: Vec<ArtifactHandle>,
) -> ArtifactHandle {
    ArtifactHandle {
        id: id.into(),
        name: name.into(),
        size,
        modified_at: None,
        source: None,
        children,
    }
}

/// Build the request handed to a parser.
pub fn parser_request(
    parser_id: impl Into<String>,
    artifact: ArtifactHandle,
    game: GameHandle,
    limits: ParserLimits,
) -> ParserRequest {
    ParserRequest {
        contract: CRASH_CONTRACT_VERSION,
        parser: parser_id.into(),
        artifact,
        game,
        limits,
    }
}

/// A coverage entry saying this parser looked for `kind` and found none.
pub fn checked(kind: impl Into<String>) -> agora_game_api::crash::Coverage {
    agora_game_api::crash::Coverage {
        kind: kind.into(),
        state: CoverageState::Checked,
        why: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_is_case_insensitive_and_bounded() {
        assert!(glob_matches("crash-*.log", "Crash-2024.LOG"));
        assert!(glob_matches("?rash.log", "crash.log"));
        assert!(!glob_matches("crash-*.log", "crash.log"));
        assert!(!glob_matches("crash-*.log", "crash-2024.txt"));
        assert!(!glob_matches("", "anything"));
    }

    #[test]
    fn reading_lines_limits_chars_not_bytes() {
        let mut artifact = MemoryArtifact::new(ParserLimits {
            max_line_len: 5,
            ..ParserLimits::DEFAULT
        });
        artifact.insert(
            "a",
            "ééééé
"
            .as_bytes()
            .to_vec(),
        );
        let reply: LinesReply =
            serde_json::from_value(artifact.read_lines("a", 0, 10).unwrap()).unwrap();
        assert_eq!(reply.lines, vec!["ééééé"]);
        assert!(!reply.cut, "five characters fit the limit");
    }

    #[test]
    fn empty_detect_never_matches_on_its_own() {
        let pack: CrashPack = serde_json::from_str(
            r#"{"contract":1,"parsers":[{"id":"only_by_format","entry":{"script":{"export":"parse"}}}]}"#,
        )
        .unwrap();
        assert!(detect_parsers(&pack, "x.log", b"anything")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_line_anchored_pattern_matches_windows_line_endings() {
        let pack: CrashPack = serde_json::from_str(
            r#"{"contract":1,"parsers":[{"id":"banner","entry":{"script":{"export":"parse"}},
                "detect":{"all":["(?m)^---- Banner ----$"]}}]}"#,
        )
        .unwrap();
        for head in [
            &b"intro\n---- Banner ----\nbody\n"[..],
            &b"intro\r\n---- Banner ----\r\nbody\r\n"[..],
        ] {
            assert_eq!(detect_parsers(&pack, "x.txt", head).unwrap().len(), 1);
        }
        // A lone `\r` is not a line end, so it still blocks `$`.
        assert!(detect_parsers(&pack, "x.txt", b"---- Banner ----\rmore\n")
            .unwrap()
            .is_empty());
    }

    fn community_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE crash_signatures (
                id TEXT PRIMARY KEY,
                name TEXT,
                games_json TEXT NOT NULL,
                regex_pattern TEXT,
                level TEXT,
                cause TEXT,
                text TEXT,
                action_kind TEXT,
                action_content_key TEXT,
                action_label TEXT
            );",
        )
        .unwrap();
    }

    fn insert_signature(conn: &Connection, id: &str, games_json: &str, pattern: &str) {
        conn.execute(
            "INSERT INTO crash_signatures
                (id, name, games_json, regex_pattern, level, cause, text,
                 action_kind, action_content_key, action_label)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, NULL, NULL, NULL)",
            rusqlite::params![id, id, games_json, pattern, format!("{id} happened.")],
        )
        .unwrap();
    }

    #[test]
    fn a_catalog_without_the_table_has_no_community_pack() {
        let conn = Connection::open_in_memory().unwrap();
        let report = load_community_pack(&conn);
        assert!(report.packs.is_empty());
        assert!(report.is_clean(), "{:#?}", report.problems);
    }

    #[test]
    fn a_valid_row_becomes_one_rule_that_names_no_suspect() {
        let conn = Connection::open_in_memory().unwrap();
        community_table(&conn);
        insert_signature(
            &conn,
            "example_signature",
            r#"["skyrim-se"]"#,
            "Unhandled exception",
        );
        conn.execute(
            "UPDATE crash_signatures SET level = 'likely', cause = 'memory',
                action_kind = 'open_jvm_settings', action_label = 'Open JVM Settings'",
            [],
        )
        .unwrap();

        let report = load_community_pack(&conn);
        assert!(report.is_clean(), "{:#?}", report.problems);
        assert_eq!(report.packs.len(), 1);
        let pack = &report.packs[0];
        assert_eq!(pack.plugin_id, COMMUNITY_PACK_PLUGIN_ID);
        assert_eq!(pack.plugin_name, COMMUNITY_PACK_PLUGIN_NAME);
        assert_eq!(pack.pack.rules.len(), 1);
        let rule = &pack.pack.rules[0];
        assert_eq!(rule.id, "example_signature");
        assert_eq!(rule.actions.len(), 1);
        assert_eq!(rule.actions[0].kind, ActionKind::OpenJvmSettings);
        match &rule.effects[0] {
            Effect::Finding {
                scope,
                level,
                cause,
                candidate,
                ..
            } => {
                assert_eq!(*scope, Scope::Mechanism);
                assert_eq!(*level, Level::Likely);
                assert_eq!(*cause, Some(Cause::Memory));
                assert!(candidate.is_none());
            }
            other => panic!("expected a finding, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_row_is_reported_with_its_id_and_never_hides_a_good_one() {
        let conn = Connection::open_in_memory().unwrap();
        community_table(&conn);
        insert_signature(&conn, "bad_games", "[]", "Anything");
        insert_signature(&conn, "bad_regex", r#"["minecraft"]"#, "[not a regex");
        insert_signature(&conn, "good_one", r#"["minecraft"]"#, "A real line");
        let report = load_community_pack(&conn);
        assert_eq!(report.packs.len(), 1);
        assert_eq!(report.packs[0].pack.rules.len(), 1);
        assert_eq!(report.packs[0].pack.rules[0].id, "good_one");
        let at: Vec<&str> = report
            .problems
            .iter()
            .map(|problem| problem.at.as_str())
            .collect();
        assert!(at.contains(&"signature `bad_games`"), "{at:?}");
        assert!(at.contains(&"signature `bad_regex`"), "{at:?}");
        for problem in &report.problems {
            assert!(problem.message.contains("could not check"), "{problem}");
        }
    }

    #[test]
    fn a_level_other_than_possible_or_likely_is_refused() {
        let signature = CommunitySignature {
            id: "too_sure".to_string(),
            name: "Too Sure".to_string(),
            games: vec!["minecraft".to_string()],
            regex_pattern: "Anything".to_string(),
            level: Some(Level::VeryLikely),
            cause: None,
            text: "Something happened.".to_string(),
            action: None,
        };
        let errors = signature.to_rule().unwrap_err();
        assert!(
            errors.iter().any(|error| error.contains("`level`")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_malformed_games_cell_is_a_problem_for_that_row() {
        let conn = Connection::open_in_memory().unwrap();
        community_table(&conn);
        insert_signature(&conn, "broken_games", "not json", "Anything");
        let report = load_community_pack(&conn);
        assert!(report.packs.is_empty());
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].at, "signature `broken_games`");
        assert!(report.problems[0].message.contains("games_json"));
    }

    #[test]
    fn a_signature_json_file_and_a_catalog_row_take_the_same_path() {
        let json = r#"{
          "id": "example_signature",
          "name": "Example",
          "games": ["skyrim-se"],
          "regex_pattern": "Unhandled exception",
          "level": "likely",
          "cause": "memory",
          "text": "The game ran out of memory.",
          "action": { "kind": "open_jvm_settings", "label": "Open JVM Settings" }
        }"#;
        let signature = parse_community_signature_json(json).unwrap();
        let rule = signature.to_rule().unwrap();
        assert!(lint_rule(&rule).is_empty());
        assert_eq!(rule.id, "example_signature");
    }
}
