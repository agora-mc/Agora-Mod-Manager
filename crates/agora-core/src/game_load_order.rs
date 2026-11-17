//! Load orders (MASTER_SPEC §26.6, §30.12): the order a game loads an instance's items in, the
//! findings its rules raise against that order, and the sort, move and lock operations that
//! change it.
//!
//! Core owns the ordered list, its locks and its writes ([`crate::game_plugins`]). The meaning
//! belongs to the game: a `script` list's `load_order_rules` hook returns the rules, the badges
//! shown beside items, and any findings about the list as a whole. Core checks the rules, reports
//! cycles, and sorts. It knows nothing about a particular engine's file formats.
//!
//! The order is the rule's `implicit` items (and those its `implicit_list_file` names) that are
//! present, then the instance's own list in its order. Implicit items are never written into
//! the list. Locked and implicit items keep their positions when others are sorted or moved:
//! free items trade places only among the positions that no locked or implicit item holds.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agora_game_api::{
    fold_path_case, glob_match, GameContext, GameDefinition, GamePackage, ListFinding,
    LoadOrderDefinition, LoadOrderLine, LoadOrderRule, LoadOrderRulesReply, OverlapReply,
    SorterEntry, SorterNote, SorterReply, SorterRequest, SorterStatus, UserLoadOrderRule,
};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_deploy::{self, DeployMode, DeploymentPlan};
use crate::game_plugins::{self, PluginEntry, PluginListError};
use crate::lock_manager::LockResource;

/// What a sort says when the list has duplicates, so its sorter cannot judge it.
pub const DUPLICATES_BY_MASTERS_ONLY: &str =
    "Sorted by masters only. A plugin is listed twice, so the sorting tool was not asked.";

/// What a sort says when its sorter has an update hook but no saved data yet. The masters-only
/// sort still runs, so a master below its dependent is moved rather than called unfixable.
pub const SORTER_NEEDS_DATA: &str =
    "Sorted by masters only. There is no masterlist yet; update it for a full sort.";

#[derive(Debug, thiserror::Error)]
pub enum LoadOrderError {
    #[error(transparent)]
    Plugins(#[from] PluginListError),
    #[error("game '{0}' declares no load order rules, so its plugins cannot be sorted")]
    NoRules(String),
    #[error("game '{game}' declares no load order named '{list}'")]
    UnknownList { game: String, list: String },
    #[error("load order '{0}' declares no ordering rules, so it cannot be sorted")]
    NoSort(String),
    #[error("load order '{list}' uses script rules, but they failed: {reason}")]
    RulesScript { list: String, reason: String },
    #[error("load order '{list}' uses a script format, but it failed: {reason}")]
    FormatScript { list: String, reason: String },
    #[error("game '{0}' has no game package to run its load order scripts")]
    NoPackage(String),
    #[error("cannot read the game's files for instance '{instance_id}': {reason}")]
    Files { instance_id: String, reason: String },
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("'{0}' is not in the instance's plugin list")]
    NotListed(String),
    #[error("'{0}' is always loaded first, so it cannot be moved or placed against")]
    AlwaysLoaded(String),
    #[error("'{0}' is locked; unlock it before moving it")]
    Locked(String),
    #[error("'{0}' cannot be placed relative to itself")]
    SelfTarget(String),
    #[error("position {position} is outside the list, which has {last} positions")]
    BadPosition { position: usize, last: usize },
    #[error("position {position} is held by '{holder}', which does not move, so '{plugin}' cannot go there")]
    Held {
        plugin: String,
        position: usize,
        holder: String,
    },
    #[error("'{plugin}' cannot go before or after '{target}': the plugins that do not move leave no free place there")]
    NoRoom { plugin: String, target: String },
    #[error("sort did not settle after {0} moves; nothing was written")]
    DidNotSettle(usize),
    #[error("the sorting tool failed: {reason}")]
    Sorter { list: String, reason: String },
    #[error("the sorting tool did not answer, though the list declares one")]
    NoSorterReply(String),
    #[error("the sorting tool returned an order that {reason}; nothing was written")]
    SorterOrder { reason: String },
    #[error("the sorting tool returned a note Agora cannot read: {reason}")]
    SorterNote { reason: String },
    #[error("the sorting tool update failed: {reason}")]
    SorterUpdate { reason: String },
    #[error("load order '{0}' declares no overlaps support, so overlaps cannot be checked")]
    NoOverlaps(String),
    #[error("the list's overlaps reader failed: {reason}")]
    Overlaps { list: String, reason: String },
    #[error("the list's overlaps reader did not answer, though the list declares one")]
    NoOverlapsReply(String),
    #[error("the list's overlaps reader returned a reply Agora cannot use: {reason}")]
    OverlapsReply { reason: String },
    #[error("the load order rules could not be read: {0}")]
    RulesStore(String),
    #[error("a rule cannot make '{0}' load after itself")]
    RuleSelf(String),
    #[error("'{item}' cannot load after '{after}': '{after}' needs '{item}' as a master")]
    RuleContradictsMaster { item: String, after: String },
    #[error("'{item}' cannot load after '{after}': '{item}' is always loaded first")]
    RuleImplicit { item: String, after: String },
    #[error(
        "'{item}' cannot load after '{after}': that would make a loop with the rules already set"
    )]
    RuleCycle { item: String, after: String },
    #[error("moving '{plugin}' there breaks the rule that '{item}' loads after '{after}'")]
    BreaksRule {
        plugin: String,
        item: String,
        after: String,
    },
}

// ---------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------

/// One plugin the game loads, in load order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOrderEntry {
    /// The plugin's name as the list or the game's own rule writes it.
    pub name: String,
    /// Always loaded first, from the game's rule. Never written into the instance's list.
    pub implicit: bool,
    pub active: bool,
    /// Whether the plugin's file is in the plugin folder the game sees.
    pub present: bool,
    /// Whether Agora added this plugin because a content layer deploys it.
    pub managed: bool,
    /// Whether the user locked this plugin's position in the order.
    pub locked: bool,
    /// Short flags the game's rules show beside the item, e.g. `Master` or `Light`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<String>,
    /// The items this one requires before it, as the game's rules name them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masters: Vec<String>,
}

/// An instance's load order and what the game's rules say about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadOrder {
    /// The list's id within the game.
    #[serde(default)]
    pub list_id: String,
    /// The list's name as a person reads it.
    #[serde(default)]
    pub list_name: String,
    /// The list's `rules.kind`: `none` or `script`.
    #[serde(default)]
    pub rules_kind: String,
    /// A sentence shown above the list, when the game declares one.
    #[serde(default)]
    pub intro: Option<String>,
    /// The column heading for an item's name, when the game declares one.
    #[serde(default)]
    pub item_heading: Option<String>,
    /// Whether the instance has a copy of the plugin list yet.
    pub exists: bool,
    pub entries: Vec<LoadOrderEntry>,
    pub findings: Vec<Finding>,
    /// What the list's sorter says about these plugins. Empty when it has nothing to say.
    #[serde(default)]
    pub notes: Vec<PluginNote>,
    /// True when the saved notes no longer match the list (or the sorter's data changed). The
    /// desktop then asks for fresh notes in the background rather than running the sorter on a
    /// plain read.
    #[serde(default)]
    pub notes_out_of_date: bool,
    /// The saved overlaps report, when the list declares an overlaps reader and one was computed
    /// for this exact order. A plain read never runs the reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlaps: Option<OverlapReport>,
    /// True when the list declares an overlaps reader but no report matches this order, so the
    /// caller asks for one in the background.
    #[serde(default)]
    pub overlaps_out_of_date: bool,
}

/// How important a sorter's message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteLevel {
    Error,
    Warn,
    Info,
}

/// One thing a list's sorter says about a plugin. Plain text, never markup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNote {
    pub plugin: String,
    pub level: NoteLevel,
    pub message: String,
}

/// A problem with the order that the game's rules name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Finding {
    /// An active plugin needs a master that is missing, inactive, or loads after it.
    MasterNotEarlier {
        plugin: String,
        master: String,
        problem: MasterProblem,
        /// Whether a sort could put the master above the plugin. False when the master is locked
        /// or always loaded, so a hint that points at `sort` would be a lie.
        #[serde(default = "default_true")]
        movable: bool,
    },
    /// Plugins whose masters lead back to one another. The engine cannot order them.
    MasterCycle { plugins: Vec<String> },
    /// The same plugin is listed more than once.
    DuplicateListing { plugin: String },
    /// A problem the game's rules report about the list as a whole, with the items it names.
    ListLevel {
        message: String,
        items: Vec<String>,
        refuses: bool,
    },
    /// LOOT would move these plugins to reach the order it recommends (MASTER_SPEC §31.6). A
    /// warning: the check never reorders anything itself.
    SorterMoves {
        list: String,
        instance: String,
        moves: Vec<SortMove>,
    },
    /// A script rule names an item the list does not have, so the rule cannot be applied.
    MissingRuleItem {
        list: String,
        item: String,
        rule: String,
    },
    /// Both items of a `conflicts` rule are active.
    RuleConflict {
        list: String,
        first: String,
        second: String,
    },
    /// The list's rules hook failed, so its order could not be judged.
    RulesFailed { list: String, reason: String },
}

fn default_true() -> bool {
    true
}

/// Why a master does not come before its plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterProblem {
    /// Not in the list, or its file is not installed.
    Missing,
    /// Listed, but inactive.
    Inactive,
    /// Active, but loads after the plugin that needs it.
    Later,
}

impl Finding {
    /// Whether a launch refuses for this finding (MASTER_SPEC §26.6). A master that is missing or
    /// inactive, a master loop, and too many plugins refuse: the game crashes or cannot start.
    /// A late master only warns: a setup that loads a plugin before its master still starts (the
    /// Phase 3 run of p3-modded did), though the plugin's references may resolve wrongly. An
    /// unreadable header and a plugin listed twice only warn too.
    pub fn refuses_launch(&self) -> bool {
        match self {
            Finding::MasterNotEarlier { problem, .. } => *problem != MasterProblem::Later,
            Finding::MasterCycle { .. } => true,
            Finding::ListLevel { refuses, .. } => *refuses,
            Finding::DuplicateListing { .. }
            | Finding::MissingRuleItem { .. }
            | Finding::RuleConflict { .. }
            | Finding::RulesFailed { .. }
            | Finding::SorterMoves { .. } => false,
        }
    }

    /// The finding as one sentence for a person.
    pub fn message(&self) -> String {
        match self {
            Finding::MasterNotEarlier {
                plugin,
                master,
                problem,
                ..
            } => match problem {
                MasterProblem::Missing => format!(
                    "'{plugin}' needs '{master}', which is missing from the list or not installed"
                ),
                MasterProblem::Inactive => {
                    format!("'{plugin}' needs '{master}', which is inactive")
                }
                MasterProblem::Later => format!("'{plugin}' loads before its master '{master}'"),
            },
            Finding::MasterCycle { plugins } => format!(
                "these plugins need one another in a loop: {}",
                plugins.join(", ")
            ),
            Finding::DuplicateListing { plugin } => format!("'{plugin}' is listed more than once"),
            Finding::ListLevel { message, .. } => message.clone(),
            Finding::MissingRuleItem { list, item, rule } => format!(
                "the '{list}' load order's {rule} rule names '{item}', which the list does not have"
            ),
            Finding::RuleConflict {
                list,
                first,
                second,
            } => {
                format!("the '{list}' load order says '{first}' conflicts with '{second}', and both are on")
            }
            Finding::RulesFailed { list, reason } => {
                format!("the '{list}' load order's rules could not be read: {reason}")
            }
            Finding::SorterMoves {
                moves, instance, ..
            } => format!(
                "Warning: LOOT would move {} plugin{}. Run 'agora games instance plugins sort \
                 {instance}' to apply them.",
                moves.len(),
                if moves.len() == 1 { "" } else { "s" },
            ),
        }
    }
}

/// The findings as one line for a person, each problem named, in the order given.
pub fn describe_findings(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(Finding::message)
        .collect::<Vec<_>>()
        .join("; ")
}

/// One move a sort made. Positions are 1-based positions in the list, as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortMove {
    pub plugin: String,
    pub from: usize,
    pub to: usize,
}

/// A plugin that loads above a master the sort could not put before it, so the sort could not fix
/// that master finding. Either the master is locked or always loaded, or moving it would itself
/// put it above one of its own masters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub plugin: String,
    pub master: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortReport {
    pub moves: Vec<SortMove>,
    pub blocked: Vec<Blocked>,
    /// Whether the moves were written to the instance's list (false for a dry run).
    pub written: bool,
    /// What ordered the free plugins: the list's sorter, or only their masters.
    pub mode: SortMode,
    /// A sentence for the user when the full sort could not run.
    pub message: Option<String>,
}

/// What a sort used to order the free plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    /// The list's plugin-provided sorter ordered the free plugins.
    Sorter,
    /// Only the plugins' own masters were used.
    Masters,
}

/// One load-after rule a user set for an instance's list (MASTER_SPEC §31.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadAfterRule {
    pub item: String,
    pub after: String,
}

/// A rule as the list shows it: whether both items are still in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadAfterRuleView {
    pub item: String,
    pub after: String,
    /// The item or its target that the list no longer has. `None` when the rule is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unused: Option<String>,
}

/// One pair of plugins where the winner replaces changes the loser made (MASTER_SPEC §31.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPair {
    pub winner: String,
    pub loser: String,
    /// How many record changes the winner replaces.
    pub count: usize,
    /// Up to [`CONFLICT_LABELS`] record labels that explain the pair.
    pub labels: Vec<String>,
    /// Whether one of the pair's items was added, turned on or moved since the last recorded
    /// launch (MASTER_SPEC §31.5). A session with no recorded entries is not a baseline.
    #[serde(default)]
    pub new_since_last_launch: bool,
}

/// How many record labels one pair lists.
pub const CONFLICT_LABELS: usize = 10;

/// The classification of one list's overlaps (MASTER_SPEC §31.3). A list with no conflicts still
/// has a report: `pairs` is empty and `patched`/`decided`/`implicit_pairs` say what was resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlapReport {
    /// Conflicts grouped by (winner, loser), in the order the first one was found.
    pub pairs: Vec<ConflictPair>,
    /// Changer pairs the winner patches because it lists the changer as a master, directly or
    /// through its masters' masters.
    pub patched: usize,
    /// Changer pairs a load-after rule already decides.
    pub decided: usize,
    /// The pairs a load-after rule decides, so a plugin filter can say what it loses by the
    /// player's choice rather than "neither wins nor loses".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decided_pairs: Vec<ConflictPair>,
    /// Changer pairs between the game's own always-loaded files, which nobody can reorder.
    #[serde(default)]
    pub implicit_pairs: usize,
    /// What the reader said about the files it could not read, or about the list as a whole.
    pub notes: Vec<PluginNote>,
}

impl OverlapReport {
    /// The number of record changes the winners replace, over every pair.
    pub fn conflicts(&self) -> usize {
        self.pairs.iter().map(|pair| pair.count).sum()
    }

    /// The plugins that lose changes, each with how many.
    pub fn losers_of(&self, plugin: &str) -> Vec<(String, usize)> {
        let wanted = key(plugin);
        self.pairs
            .iter()
            .filter(|pair| key(&pair.loser) == wanted)
            .map(|pair| (pair.winner.clone(), pair.count))
            .collect()
    }

    /// The plugins this plugin wins changes from, each with how many.
    pub fn wins_over(&self, plugin: &str) -> Vec<(String, usize)> {
        let wanted = key(plugin);
        self.pairs
            .iter()
            .filter(|pair| key(&pair.winner) == wanted)
            .map(|pair| (pair.loser.clone(), pair.count))
            .collect()
    }

    /// The plugins a load-after rule lets this plugin win over, each with how many changes.
    pub fn decided_wins_over(&self, plugin: &str) -> Vec<(String, usize)> {
        let wanted = key(plugin);
        self.decided_pairs
            .iter()
            .filter(|pair| key(&pair.winner) == wanted)
            .map(|pair| (pair.loser.clone(), pair.count))
            .collect()
    }

    /// The plugins a load-after rule makes this plugin lose to, each with how many changes.
    pub fn decided_losers_of(&self, plugin: &str) -> Vec<(String, usize)> {
        let wanted = key(plugin);
        self.decided_pairs
            .iter()
            .filter(|pair| key(&pair.loser) == wanted)
            .map(|pair| (pair.winner.clone(), pair.count))
            .collect()
    }
}

/// The last report an overlaps reader gave for one list, with what it was computed from.
#[derive(Default)]
struct OverlapsCache {
    entries: Mutex<HashMap<(String, String), CachedOverlaps>>,
}

#[derive(Clone)]
struct CachedOverlaps {
    fingerprint: String,
    data_version: String,
    report: OverlapReport,
}

impl OverlapsCache {
    fn lookup(
        &self,
        game: &str,
        list: &str,
        fingerprint: &str,
        data_version: &str,
    ) -> Option<OverlapReport> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .get(&(game.to_string(), list.to_string()))
            .filter(|cached| {
                cached.fingerprint == fingerprint && cached.data_version == data_version
            })
            .map(|cached| cached.report.clone())
    }

    fn store(
        &self,
        game: &str,
        list: &str,
        fingerprint: String,
        data_version: String,
        report: OverlapReport,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.insert(
            (game.to_string(), list.to_string()),
            CachedOverlaps {
                fingerprint,
                data_version,
                report,
            },
        );
    }
}

/// The last notes a sorter gave for one list, with what they were computed from.
#[derive(Default)]
struct SorterNotesCache {
    entries: Mutex<HashMap<(String, String), CachedSorterNotes>>,
}

#[derive(Clone)]
struct CachedSorterNotes {
    fingerprint: String,
    data_version: String,
    notes: Vec<PluginNote>,
}

impl SorterNotesCache {
    fn lookup(
        &self,
        game: &str,
        list: &str,
        fingerprint: &str,
        data_version: &str,
    ) -> Option<Vec<PluginNote>> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .get(&(game.to_string(), list.to_string()))
            .filter(|cached| {
                cached.fingerprint == fingerprint && cached.data_version == data_version
            })
            .map(|cached| cached.notes.clone())
    }

    fn store(
        &self,
        game: &str,
        list: &str,
        fingerprint: String,
        data_version: String,
        notes: Vec<PluginNote>,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.insert(
            (game.to_string(), list.to_string()),
            CachedSorterNotes {
                fingerprint,
                data_version,
                notes,
            },
        );
    }
}

/// The last LOOT check a sorter gave for one list, with what it was computed from. A check runs
/// the sorter, which is expensive, so a plain list read and a second check for the same order reuse
/// this; it is emptied by a change to the order, the files or the sorter's data.
#[derive(Default)]
struct SorterCheckCache {
    entries: Mutex<HashMap<(String, String), CachedSorterCheck>>,
}

#[derive(Clone)]
struct CachedSorterCheck {
    fingerprint: String,
    data_version: String,
    findings: Vec<Finding>,
}

impl SorterCheckCache {
    fn lookup(
        &self,
        game: &str,
        list: &str,
        fingerprint: &str,
        data_version: &str,
    ) -> Option<Vec<Finding>> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .get(&(game.to_string(), list.to_string()))
            .filter(|cached| {
                cached.fingerprint == fingerprint && cached.data_version == data_version
            })
            .map(|cached| cached.findings.clone())
    }

    fn store(
        &self,
        game: &str,
        list: &str,
        fingerprint: String,
        data_version: String,
        findings: Vec<Finding>,
    ) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.insert(
            (game.to_string(), list.to_string()),
            CachedSorterCheck {
                fingerprint,
                data_version,
                findings,
            },
        );
    }
}

/// What a list's notes are computed from: the order, each present item's active state, and its
/// file's size and modification time. A change to any of them makes the saved notes stale.
fn sorter_fingerprint(entries: &[LoadOrderEntry], installed: &Installed) -> String {
    let mut out = String::new();
    for entry in entries.iter().filter(|entry| entry.present) {
        out.push_str(&key(&entry.name));
        out.push('|');
        out.push(if entry.active { '1' } else { '0' });
        out.push('|');
        if let Some(path) = installed.get(&key(&entry.name)) {
            if let Ok(metadata) = std::fs::metadata(path) {
                out.push_str(&metadata.len().to_string());
                out.push(':');
                if let Ok(modified) = metadata.modified() {
                    if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                        out.push_str(&since.as_nanos().to_string());
                    }
                }
            }
        }
        out.push(';');
    }
    out
}

/// The sorter's data version for a list, from its cheap status hook. An unavailable status is
/// treated as "no version", so the notes simply read as stale rather than failing the list.
fn sorter_data_version(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
) -> String {
    sorter_status(ctx, instance_id, definition, Some(&rule.id))
        .ok()
        .flatten()
        .map(|status| status.data_version)
        .unwrap_or_default()
}

/// Remember notes for the list as it stands now. Sort and Update call this so the next read needs
/// no sorter run.
fn cache_sorter_notes(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
    installed: &Installed,
    notes: &[PluginNote],
) {
    let cache = ctx.extensions.get_or_insert_with(SorterNotesCache::default);
    cache.store(
        definition.id.as_str(),
        &rule.id,
        sorter_fingerprint(entries, installed),
        sorter_data_version(ctx, instance_id, definition, rule),
        notes.to_vec(),
    );
}

/// Where `move` puts a plugin. Positions are 1-based and count the always-loaded plugins too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveTarget {
    Position(usize),
    Before(String),
    After(String),
}

/// The result of moving one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveReport {
    pub plugin: String,
    pub from: usize,
    pub to: usize,
    pub written: bool,
    /// Rules removed because the move broke them and the caller allowed that.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_rules: Vec<LoadAfterRule>,
}

/// The plugin folder as the game sees it: lowercase file name to the file that holds its bytes.
pub type Installed = BTreeMap<String, PathBuf>;

// ---------------------------------------------------------------------------
// Building the order
// ---------------------------------------------------------------------------

fn key(name: &str) -> String {
    fold_path_case(name)
}

/// The list a caller means. `None` means the game's first list; naming an id the game does not
/// declare is an error, never a quiet fall back to the first.
pub fn list_by_id<'a>(
    definition: &'a GameDefinition,
    list_id: Option<&str>,
) -> Result<&'a LoadOrderDefinition, LoadOrderError> {
    match list_id {
        None => definition
            .load_orders
            .first()
            .ok_or_else(|| PluginListError::NoRule(definition.id.clone()).into()),
        Some(id) => definition
            .load_orders
            .iter()
            .find(|rule| rule.id == id)
            .ok_or_else(|| LoadOrderError::UnknownList {
                game: definition.id.as_str().to_string(),
                list: id.to_string(),
            }),
    }
}

/// Whether a list reads the game's files: a script hook, a sorter, or a list of folders.
fn list_needs_plan(rule: &LoadOrderDefinition) -> bool {
    rule.rules.kind == "script"
        || rule.rules.sorter.is_some()
        || rule.rules.overlaps.is_some()
        || rule.items.folders
}

/// Whether any list needs a deployment plan at all.
pub fn any_needs_plan(definition: &GameDefinition) -> bool {
    definition.load_orders.iter().any(list_needs_plan)
}

// ---------------------------------------------------------------------------
// Script hooks
// ---------------------------------------------------------------------------

/// Drive a package hook from core's synchronous code. The hooks only wait on the plugin
/// thread's synchronous bridge, so parking this thread is enough and no Tokio runtime is
/// created here. That also makes the call safe from inside one (the CLI's launch path).
fn block_on_hook<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn package_of<'a>(
    ctx: &'a Ctx,
    definition: &GameDefinition,
) -> Result<&'a std::sync::Arc<dyn GamePackage>, LoadOrderError> {
    ctx.games
        .package_for(&definition.id)
        .ok_or_else(|| LoadOrderError::NoPackage(definition.id.as_str().to_string()))
}

/// The context a load-order hook runs with: the instance, its runtime, layers and the list as it
/// stands before the hook decides anything.
fn hook_context(
    ctx: &Ctx,
    instance_id: &str,
    _definition: &GameDefinition,
    _rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<GameContext, LoadOrderError> {
    let manifest = crate::game_instance::get_manifest(ctx, instance_id).map_err(|error| {
        LoadOrderError::Files {
            instance_id: instance_id.to_string(),
            reason: error.to_string(),
        }
    })?;
    let runtime = manifest
        .runtime_identity
        .clone()
        .ok_or_else(|| LoadOrderError::Files {
            instance_id: instance_id.to_string(),
            reason: "the instance has no runtime yet".to_string(),
        })?;
    let load_order = agora_game_api::LoadOrder {
        entries: entries
            .iter()
            .map(|entry| agora_game_api::LoadOrderEntry {
                id: entry.name.clone(),
                enabled: entry.active,
                locked: entry.locked,
                present: entry.present,
            })
            .collect(),
        rules: Vec::new(),
    };
    Ok(GameContext {
        instance: instance_id.to_string(),
        runtime,
        base: Some(manifest.base.clone()),
        frameworks: manifest.frameworks.clone(),
        layers: manifest.layers.clone(),
        load_order,
        settings: BTreeMap::new(),
    })
}

/// Ask a package for a script list's rules. `None` is a failure, not "no rules".
pub(crate) fn call_load_order_rules(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<LoadOrderRulesReply, String> {
    let package = package_of(ctx, definition).map_err(|error| error.to_string())?;
    let context = hook_context(ctx, instance_id, definition, rule, entries)
        .map_err(|error| error.to_string())?;
    let result = block_on_hook(package.load_order_rules(context, &rule.id))
        .map_err(|error| error.message)?;
    result.ok_or_else(|| {
        format!(
            "the package declares script rules for '{}' but returned none",
            rule.id
        )
    })
}

/// The load-after rules the user set for one instance's list, in the order they were added.
pub fn load_after_rules(
    ctx: &Ctx,
    instance_id: &str,
    list_id: &str,
) -> Result<Vec<LoadAfterRule>, LoadOrderError> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    let mut stmt = conn
        .prepare(
            "SELECT item, after_item FROM plugin_load_order_rules
              WHERE instance_id = ?1 AND list_id = ?2
              ORDER BY id",
        )
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![instance_id, list_id], |row| {
            Ok(LoadAfterRule {
                item: row.get(0)?,
                after: row.get(1)?,
            })
        })
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))
}

/// The rules as the list shows them, naming the item that left the list when a rule is unused.
pub fn load_after_rule_views(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Vec<LoadAfterRuleView>, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let rules = load_after_rules(ctx, instance_id, &rule.id)?;
    let listed = game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id))?;
    let names: HashSet<String> = listed
        .entries
        .iter()
        .map(|entry| key(&entry.name))
        .chain(rule.rules.implicit.iter().map(|name| key(name)))
        .collect();
    Ok(rules
        .into_iter()
        .map(|rule| {
            let unused = if !names.contains(&key(&rule.item)) {
                Some(rule.item.clone())
            } else if !names.contains(&key(&rule.after)) {
                Some(rule.after.clone())
            } else {
                None
            };
            LoadAfterRuleView {
                item: rule.item,
                after: rule.after,
                unused,
            }
        })
        .collect())
}

fn sorter_request(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<SorterRequest, LoadOrderError> {
    let sorter = rule
        .rules
        .sorter
        .as_ref()
        .ok_or_else(|| LoadOrderError::NoSort(rule.id.clone()))?;
    hook_request(
        ctx,
        instance_id,
        definition,
        rule,
        entries,
        sorter.options.clone(),
    )
}

/// Whether `from` reaches `to` by one or more "must come before" edges.
fn reaches(edges: &HashMap<String, Vec<String>>, from: &str, to: &str) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = edges.get(from).cloned().unwrap_or_default();
    while let Some(next) = stack.pop() {
        if next == to {
            return true;
        }
        if !seen.insert(next.clone()) {
            continue;
        }
        if let Some(more) = edges.get(&next) {
            stack.extend(more.iter().cloned());
        }
    }
    false
}

/// Refuse one load-after rule that can never hold, whatever else is stored: it would put an
/// always-loaded plugin after a normal one, contradict a master, or close a loop. `next` maps an
/// item to the items that must come after it, from the masters and the rules being considered.
fn check_load_after_rule(
    entries: &[LoadOrderEntry],
    next: &HashMap<String, Vec<String>>,
    item: &str,
    after: &str,
) -> Result<(), LoadOrderError> {
    let item_key = key(item);
    let after_key = key(after);
    let is_implicit = |name: &str| {
        entries
            .iter()
            .any(|entry| key(&entry.name) == key(name) && entry.implicit)
    };
    // An always-loaded plugin is loaded first by the game, so no rule can put it after a normal
    // plugin; the rule would never hold.
    if is_implicit(item) && !is_implicit(after) {
        return Err(LoadOrderError::RuleImplicit {
            item: item.to_string(),
            after: after.to_string(),
        });
    }
    // The game's own rules come first: a rule that asks an item to load after one of its masters
    // can never hold.
    if let Some(entry) = entries.iter().find(|entry| key(&entry.name) == after_key) {
        if entry.masters.iter().any(|master| key(master) == item_key) {
            return Err(LoadOrderError::RuleContradictsMaster {
                item: item.to_string(),
                after: after.to_string(),
            });
        }
    }
    // Adding `item loads after after` closes a loop when item already reaches after.
    if reaches(next, &item_key, &after_key) {
        return Err(LoadOrderError::RuleCycle {
            item: item.to_string(),
            after: after.to_string(),
        });
    }
    Ok(())
}

/// Add a user's load-after rule (MASTER_SPEC §31.4). Returns whether it was new. A rule that
/// contradicts a master requirement, or that would make a loop with the rules and masters already
/// in place, is refused with the reason.
pub fn add_load_after_rule(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    item: &str,
    after: &str,
) -> Result<bool, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let item = item.trim();
    let after = after.trim();
    if item.is_empty() || after.is_empty() {
        return Err(LoadOrderError::RulesStore(
            "a rule needs both an item and the item it loads after".to_string(),
        ));
    }
    if key(item) == key(after) {
        return Err(LoadOrderError::RuleSelf(item.to_string()));
    }

    let order = order_in(ctx, instance_id, definition, Some(&rule.id))?;
    if let Some(Finding::RulesFailed { reason, .. }) = order
        .findings
        .iter()
        .find(|finding| matches!(finding, Finding::RulesFailed { .. }))
    {
        return Err(LoadOrderError::RulesStore(format!(
            "the list's rules could not be read, so the rule cannot be checked: {reason}"
        )));
    }
    let canonical = |name: &str| {
        order
            .entries
            .iter()
            .find(|entry| key(&entry.name) == key(name))
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| name.to_string())
    };
    let item = canonical(item);
    let after = canonical(after);

    // "next" maps an item to the items that must come after it, from the masters and the rules
    // already set. Adding `item loads after after` closes a loop when item already reaches after.
    let mut next: HashMap<String, Vec<String>> = HashMap::new();
    for entry in &order.entries {
        for master in &entry.masters {
            next.entry(key(master)).or_default().push(key(&entry.name));
        }
    }
    for existing in load_after_rules(ctx, instance_id, &rule.id)? {
        next.entry(key(&existing.after))
            .or_default()
            .push(key(&existing.item));
    }
    check_load_after_rule(&order.entries, &next, &item, &after)?;

    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    let changed = conn
        .execute(
            "INSERT OR IGNORE INTO plugin_load_order_rules (instance_id, list_id, item, after_item)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![instance_id, rule.id, item, after],
        )
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    Ok(changed > 0)
}

/// Remove a user's load-after rule. Returns whether one was there. A rule naming an item the list
/// no longer has can still be removed this way.
pub fn remove_load_after_rule(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    item: &str,
    after: &str,
) -> Result<bool, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    let changed = conn
        .execute(
            "DELETE FROM plugin_load_order_rules
              WHERE instance_id = ?1 AND list_id = ?2 AND item = ?3 AND after_item = ?4",
            rusqlite::params![instance_id, rule.id, item.trim(), after.trim()],
        )
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    Ok(changed > 0)
}

/// The entries of an instance's list when core can find the game's definition. `None` (an
/// unknown instance or game) leaves only the rules themselves to check; a caller that already
/// holds the definition still gets the master checks through [`order_in`].
fn order_entries_for(ctx: &Ctx, instance_id: &str, list_id: &str) -> Option<Vec<LoadOrderEntry>> {
    let record = crate::game_instance::get(ctx, instance_id).ok().flatten()?;
    let definition = ctx.games.game(&record.game)?;
    order_in(ctx, instance_id, definition, Some(list_id))
        .ok()
        .map(|order| order.entries)
}

/// Replace every load-after rule of one list with `rules`, in one transaction.
///
/// A loadout apply and a multi-profile import both restore a whole set of rules
/// at once; removing and adding them one by one would leave a half-set behind
/// if one call failed. A self or blank rule is skipped, and so is any rule the same
/// checks as [`add_load_after_rule`] refuse: an always-loaded item after a normal
/// one, a master contradiction, or a loop in the set, so no path writes a rule
/// that can never hold.
pub fn replace_load_after_rules(
    ctx: &Ctx,
    instance_id: &str,
    list_id: &str,
    rules: &[LoadAfterRule],
) -> Result<(), LoadOrderError> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut proposed: Vec<LoadAfterRule> = Vec::new();
    for rule in rules {
        let item = rule.item.trim();
        let after = rule.after.trim();
        if item.is_empty() || after.is_empty() || key(item) == key(after) {
            continue;
        }
        if !seen.insert((key(item), key(after))) {
            continue;
        }
        proposed.push(LoadAfterRule {
            item: item.to_string(),
            after: after.to_string(),
        });
    }

    // Check the whole set at once against the game's masters and against itself, so a bulk
    // restore runs the same validation as adding one rule at a time. Every proposed rule is an
    // edge, even one later refused, so a loop through it is found too.
    let entries = order_entries_for(ctx, instance_id, list_id).unwrap_or_default();
    let mut next: HashMap<String, Vec<String>> = HashMap::new();
    for entry in &entries {
        for master in &entry.masters {
            next.entry(key(master)).or_default().push(key(&entry.name));
        }
    }
    for rule in &proposed {
        next.entry(key(&rule.after))
            .or_default()
            .push(key(&rule.item));
    }
    let accepted: Vec<&LoadAfterRule> = proposed
        .iter()
        .filter(|rule| check_load_after_rule(&entries, &next, &rule.item, &rule.after).is_ok())
        .collect();

    let mut conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    let tx = conn
        .transaction()
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    tx.execute(
        "DELETE FROM plugin_load_order_rules WHERE instance_id = ?1 AND list_id = ?2",
        rusqlite::params![instance_id, list_id],
    )
    .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    for rule in accepted {
        tx.execute(
            "INSERT INTO plugin_load_order_rules (instance_id, list_id, item, after_item)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![instance_id, list_id, rule.item, rule.after],
        )
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))?;
    }
    tx.commit()
        .map_err(|error| LoadOrderError::RulesStore(error.to_string()))
}

/// The user rules the order does not satisfy. A rule whose item or target is not in the list is
/// unused and never reported.
fn broken_rules(entries: &[LoadOrderEntry], rules: &[LoadAfterRule]) -> Vec<LoadAfterRule> {
    let position = |name: &str| {
        entries
            .iter()
            .position(|entry| key(&entry.name) == key(name))
    };
    rules
        .iter()
        .filter(|rule| {
            let (Some(item), Some(after)) = (position(&rule.item), position(&rule.after)) else {
                return false;
            };
            item <= after
        })
        .cloned()
        .collect()
}

/// Turn the user's rules into prerequisite edges, so the masters-only sort and the blocked check
/// honour them exactly as they honour a master.
fn apply_load_after_rules(entries: &mut [LoadOrderEntry], rules: &[LoadAfterRule]) {
    for rule in rules {
        let Some(item) = entries
            .iter_mut()
            .find(|entry| key(&entry.name) == key(&rule.item))
        else {
            continue;
        };
        if key(&rule.item) == key(&rule.after) {
            continue;
        }
        if !item
            .masters
            .iter()
            .any(|master| key(master) == key(&rule.after))
        {
            item.masters.push(rule.after.clone());
        }
    }
}

/// What a package's sorter and overlaps hooks receive: the list as it stands, the user's rules,
/// plus the hook's free options.
fn hook_request(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
    options: serde_json::Value,
) -> Result<SorterRequest, LoadOrderError> {
    let context = hook_context(ctx, instance_id, definition, rule, entries)?;
    Ok(SorterRequest {
        context,
        list_id: rule.id.clone(),
        options,
        entries: entries
            .iter()
            .map(|entry| SorterEntry {
                name: entry.name.clone(),
                active: entry.active,
                locked: entry.locked,
                implicit: entry.implicit,
                present: entry.present,
            })
            .collect(),
        active: entries
            .iter()
            .filter(|entry| entry.active)
            .map(|entry| entry.name.clone())
            .collect(),
        locked: entries
            .iter()
            .filter(|entry| entry.locked)
            .map(|entry| entry.name.clone())
            .collect(),
        implicit: entries
            .iter()
            .filter(|entry| entry.implicit)
            .map(|entry| entry.name.clone())
            .collect(),
        rules: load_after_rules(ctx, instance_id, &rule.id)?
            .into_iter()
            .map(|rule| UserLoadOrderRule {
                item: rule.item,
                after: rule.after,
            })
            .collect(),
    })
}

/// Ask the package's sorter for a whole order. A failure is an error, never "nothing changed".
fn call_sorter(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<SorterReply, LoadOrderError> {
    let package = package_of(ctx, definition)?;
    let request = sorter_request(ctx, instance_id, definition, rule, entries)?;
    let result = block_on_hook(package.sort_load_order(request)).map_err(|error| {
        LoadOrderError::Sorter {
            list: rule.id.clone(),
            reason: error.message,
        }
    })?;
    result.ok_or_else(|| LoadOrderError::NoSorterReply(rule.id.clone()))
}

/// A sorter reply's notes as the list shows them. A level Agora does not know is refused, because
/// guessing its severity would be inventing an answer.
fn sorter_notes(reply: &SorterReply) -> Result<Vec<PluginNote>, LoadOrderError> {
    let mut notes = Vec::new();
    for note in reply
        .notes
        .iter()
        .chain(reply.general.iter())
        .chain(reply.checks.iter())
    {
        let level = note_level(&note.level)?;
        notes.push(PluginNote {
            plugin: note.item.clone(),
            level,
            message: note.text.clone(),
        });
    }
    Ok(notes)
}

/// An overlaps reply's notes as the list shows them, with the same strict level check as a
/// sorter's.
fn overlaps_notes(reply: &OverlapReply) -> Result<Vec<PluginNote>, LoadOrderError> {
    let mut notes = Vec::new();
    for note in &reply.notes {
        let level = match note.level.as_str() {
            "error" => NoteLevel::Error,
            "warn" => NoteLevel::Warn,
            "info" => NoteLevel::Info,
            other => {
                return Err(LoadOrderError::OverlapsReply {
                    reason: format!("`{other}` is not error, warn or info"),
                })
            }
        };
        notes.push(PluginNote {
            plugin: note.item.clone(),
            level,
            message: note.text.clone(),
        });
    }
    Ok(notes)
}

/// Ask the package's overlaps reader about the list. A failure is an error, never "no conflicts".
fn call_overlaps(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<OverlapReply, LoadOrderError> {
    let overlaps = rule
        .rules
        .overlaps
        .as_ref()
        .ok_or_else(|| LoadOrderError::NoOverlaps(rule.id.clone()))?;
    let package = package_of(ctx, definition)?;
    let request = hook_request(
        ctx,
        instance_id,
        definition,
        rule,
        entries,
        overlaps.options.clone(),
    )?;
    let result = block_on_hook(package.find_overlaps(request)).map_err(|error| {
        LoadOrderError::Overlaps {
            list: rule.id.clone(),
            reason: error.message,
        }
    })?;
    result.ok_or_else(|| LoadOrderError::NoOverlapsReply(rule.id.clone()))
}

/// Classify one overlaps reply against the current order and the user's load-after rules
/// (MASTER_SPEC §31.3). A reply that names an item the list does not load, repeats an item, or
/// names a key with fewer than two items is refused: it is a reply core cannot trust.
pub fn classify_overlaps(
    entries: &[LoadOrderEntry],
    reply: &OverlapReply,
    rules: &[LoadAfterRule],
) -> Result<OverlapReport, LoadOrderError> {
    let mut positions: HashMap<String, (usize, &LoadOrderEntry)> = HashMap::new();
    for (position, entry) in entries.iter().enumerate().filter(|(_, e)| loads(e)) {
        positions.insert(key(&entry.name), (position, entry));
    }
    // "after" must come before "item", so these edges order the two the way the player asked.
    // A chain of rules decides the pair at its ends, not only a single rule.
    let mut rule_edges: HashMap<String, Vec<String>> = HashMap::new();
    for rule in rules {
        rule_edges
            .entry(key(&rule.after))
            .or_default()
            .push(key(&rule.item));
    }

    let mut report = OverlapReport {
        notes: overlaps_notes(reply)?,
        ..OverlapReport::default()
    };
    let mut seen_keys: HashSet<String> = HashSet::new();

    for key_entry in &reply.keys {
        if key_entry.key.trim().is_empty() {
            return Err(LoadOrderError::OverlapsReply {
                reason: "a record key is empty".to_string(),
            });
        }
        if !seen_keys.insert(key_entry.key.clone()) {
            return Err(LoadOrderError::OverlapsReply {
                reason: format!("record key '{}' is reported twice", key_entry.key),
            });
        }
        if key_entry.items.len() < 2 {
            return Err(LoadOrderError::OverlapsReply {
                reason: format!(
                    "record key '{}' names {} item(s), but an overlap needs two",
                    key_entry.key,
                    key_entry.items.len()
                ),
            });
        }
        let mut holders: Vec<(usize, &str, &str, &LoadOrderEntry)> = Vec::new();
        let mut seen_items: HashSet<String> = HashSet::new();
        for item in &key_entry.items {
            if item.name.trim().is_empty() || item.digest.trim().is_empty() {
                return Err(LoadOrderError::OverlapsReply {
                    reason: format!(
                        "record key '{}' has an item without a name or digest",
                        key_entry.key
                    ),
                });
            }
            let item_key = key(&item.name);
            if !seen_items.insert(item_key.clone()) {
                return Err(LoadOrderError::OverlapsReply {
                    reason: format!("record key '{}' names '{}' twice", key_entry.key, item.name),
                });
            }
            let Some((position, entry)) = positions.get(&item_key) else {
                return Err(LoadOrderError::OverlapsReply {
                    reason: format!(
                        "record key '{}' names '{}', which the list does not load",
                        key_entry.key, item.name
                    ),
                });
            };
            holders.push((*position, item.name.as_str(), item.digest.as_str(), entry));
        }
        holders.sort_by_key(|(position, ..)| *position);

        // The first holder is the definer: its copy is the original, never a change, so it is
        // never a changer and never loses. A later holder whose digest equals the one before it
        // is "identical to master" and is not a changer either.
        let mut changers: Vec<(usize, &str, &str, &LoadOrderEntry)> = Vec::new();
        for index in 1..holders.len() {
            if holders[index].2 != holders[index - 1].2 {
                changers.push(holders[index]);
            }
        }
        if changers.is_empty() {
            continue;
        }
        let (_, winner_name, winner_digest, winner) =
            *holders.last().expect("an overlap has items");
        let label = if key_entry.label.trim().is_empty() {
            key_entry.key.clone()
        } else {
            key_entry.label.clone()
        };
        // Each changer is judged as a pair with the winner, so one changer the winner does not
        // know about never turns the others into conflicts (MASTER_SPEC §31.3).
        for changer in changers {
            if key(changer.1) == key(winner_name) || changer.2 == winner_digest {
                // The winner's own change, or one the winner copies identically: nothing is lost.
                continue;
            }
            if winner_masters(entries, winner, changer.1) {
                report.patched += 1;
                continue;
            }
            if reaches(&rule_edges, &key(changer.1), &key(winner_name)) {
                report.decided += 1;
                record_pair(&mut report.decided_pairs, winner_name, changer.1, &label);
                continue;
            }
            if winner.implicit && changer.3.implicit {
                report.implicit_pairs += 1;
                continue;
            }
            record_pair(&mut report.pairs, winner_name, changer.1, &label);
        }
    }
    Ok(report)
}

/// Add one changer pair to a list, merging a repeat of the same pair and keeping up to
/// [`CONFLICT_LABELS`] labels.
fn record_pair(pairs: &mut Vec<ConflictPair>, winner: &str, loser: &str, label: &str) {
    match pairs
        .iter_mut()
        .find(|pair| key(&pair.winner) == key(winner) && key(&pair.loser) == key(loser))
    {
        Some(pair) => {
            pair.count += 1;
            if pair.labels.len() < CONFLICT_LABELS
                && !pair.labels.iter().any(|existing| existing == label)
            {
                pair.labels.push(label.to_string());
            }
        }
        None => pairs.push(ConflictPair {
            winner: winner.to_string(),
            loser: loser.to_string(),
            count: 1,
            labels: vec![label.to_string()],
            new_since_last_launch: false,
        }),
    }
}

/// Whether `winner` lists `changer` among its masters, directly or through its masters' masters.
/// Only a master the game actually loads can carry a patch: an inactive or missing plugin in the
/// chain patches nothing.
fn winner_masters(entries: &[LoadOrderEntry], winner: &LoadOrderEntry, changer: &str) -> bool {
    let wanted = key(changer);
    let by_name: HashMap<String, &LoadOrderEntry> = entries
        .iter()
        .filter(|entry| loads(entry))
        .map(|entry| (key(&entry.name), entry))
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = winner.masters.iter().map(|name| key(name)).collect();
    while let Some(name) = pending.pop() {
        if name == wanted {
            return true;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(entry) = by_name.get(&name) {
            pending.extend(entry.masters.iter().map(|master| key(master)));
        }
    }
    false
}

/// What an overlaps report is computed from: the same fingerprint as the sorter's notes, plus the
/// user's load-after rules, because adding a rule can turn a conflict into a decided one.
fn overlaps_fingerprint(
    entries: &[LoadOrderEntry],
    installed: &Installed,
    rules: &[LoadAfterRule],
) -> String {
    let mut out = sorter_fingerprint(entries, installed);
    out.push_str("|rules|");
    let mut ordered: Vec<(String, String)> = rules
        .iter()
        .map(|rule| (key(&rule.item), key(&rule.after)))
        .collect();
    ordered.sort();
    for (item, after) in ordered {
        out.push_str(&item);
        out.push('>');
        out.push_str(&after);
        out.push(';');
    }
    out
}

fn cache_overlaps(
    ctx: &Ctx,
    game: &str,
    list: &str,
    fingerprint: String,
    data_version: String,
    report: &OverlapReport,
) {
    let cache = ctx.extensions.get_or_insert_with(OverlapsCache::default);
    cache.store(game, list, fingerprint, data_version, report.clone());
}

/// Ask the overlaps reader now, cache the report for the list as it stands, and return it.
pub fn refresh_overlaps(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<OverlapReport, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    if rule.rules.overlaps.is_none() {
        return Err(LoadOrderError::NoOverlaps(rule.id.clone()));
    }
    let links = plan_for_order_in(ctx, instance_id, definition, Some(&rule.id))?;
    let listed = game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id))?;
    let (installed, names) = installed_files(ctx, instance_id, definition, rule, links.as_ref())?;
    let mut entries = build_order(
        rule,
        &listed.entries,
        Some(&installed),
        &names,
        listed.exists,
    )
    .entries;
    if rule.rules.kind == "script" {
        let reply = call_load_order_rules(ctx, instance_id, definition, rule, &entries).map_err(
            |reason| LoadOrderError::Overlaps {
                list: rule.id.clone(),
                reason: format!("the list's rules could not be read: {reason}"),
            },
        )?;
        set_prerequisites(&mut entries, &reply.rules, &rule.id);
    }
    let rules = load_after_rules(ctx, instance_id, &rule.id)?;
    let reply = call_overlaps(ctx, instance_id, definition, rule, &entries)?;
    let mut report = classify_overlaps(&entries, &reply, &rules)?;
    mark_new_pairs(ctx, instance_id, &rule.id, &mut report, &entries);
    cache_overlaps(
        ctx,
        definition.id.as_str(),
        &rule.id,
        overlaps_fingerprint(&entries, &installed, &rules),
        sorter_data_version(ctx, instance_id, definition, rule),
        &report,
    );
    Ok(report)
}

/// Mark each pair whose winner or loser was added, turned on or moved since the last recorded
/// launch, so the conflicts list can put those first (MASTER_SPEC §31.5). A failed read of the
/// ledger means no baseline, never a made-up "new".
fn mark_new_pairs(
    ctx: &Ctx,
    instance_id: &str,
    list_id: &str,
    report: &mut OverlapReport,
    current: &[LoadOrderEntry],
) {
    let previous: Vec<crate::session_ledger::LoadOrderEntry> =
        crate::db::local_state_connection(&ctx.paths.local_state_db())
            .ok()
            .and_then(|conn| {
                crate::session_ledger::latest_manifest(&conn, instance_id)
                    .ok()
                    .flatten()
            })
            .map(|manifest| {
                manifest
                    .load_order
                    .into_iter()
                    .filter(|entry| entry.list == list_id)
                    .collect()
            })
            .unwrap_or_default();
    for pair in &mut report.pairs {
        let is_new = pair_is_new_since(pair, current, &previous);
        pair.new_since_last_launch = is_new;
    }
}

/// Whether a conflict pair is new since the load order the last session recorded: one of its
/// plugins was added, turned on, or moved. A session with no recorded entries for the list is not
/// a baseline, so no pair is reported as new against it.
fn pair_is_new_since(
    pair: &ConflictPair,
    current: &[LoadOrderEntry],
    previous: &[crate::session_ledger::LoadOrderEntry],
) -> bool {
    if previous.is_empty() {
        return false;
    }
    let previous_by_name: HashMap<String, (usize, bool)> = previous
        .iter()
        .enumerate()
        .map(|(position, entry)| (key(&entry.name), (position, entry.active)))
        .collect();
    let current_by_name: HashMap<String, (usize, bool)> = current
        .iter()
        .filter(|entry| !entry.implicit)
        .enumerate()
        .map(|(position, entry)| (key(&entry.name), (position, entry.active)))
        .collect();
    [&pair.winner, &pair.loser].into_iter().any(|name| {
        let Some((position, active)) = current_by_name.get(&key(name)) else {
            return false;
        };
        match previous_by_name.get(&key(name)) {
            None => true,
            Some((before, was_active)) => (!*was_active && *active) || before != position,
        }
    })
}

/// The overlaps findings the instance check reports (MASTER_SPEC §31.5): the count of conflicts,
/// and a warning for each pair that is new since the load order the last session recorded. A hook
/// that fails or returns a reply core cannot trust is "could not check", never "no conflicts".
pub fn overlap_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    orders: &[LoadOrder],
) -> Vec<Finding> {
    let previous = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .ok()
        .and_then(|conn| {
            crate::session_ledger::latest_manifest(&conn, instance_id)
                .ok()
                .flatten()
        });
    let mut out = Vec::new();
    for rule in &definition.load_orders {
        if rule.rules.overlaps.is_none() {
            continue;
        }
        // A one-list check passes only that list's order; do not report another list's overlaps.
        if !orders.iter().any(|order| order.list_id == rule.id) {
            continue;
        }
        let report = match refresh_overlaps(ctx, instance_id, definition, Some(&rule.id)) {
            Ok(report) => report,
            Err(error) => {
                out.push(Finding::ListLevel {
                    message: format!("Warning: overlaps could not be checked: {error}"),
                    items: Vec::new(),
                    refuses: false,
                });
                continue;
            }
        };
        for note in &report.notes {
            let level = match note.level {
                NoteLevel::Error => "Error",
                NoteLevel::Warn => "Warning",
                NoteLevel::Info => "Info",
            };
            out.push(Finding::ListLevel {
                message: format!("{level} from the overlaps reader: {}", note.message),
                items: if note.plugin.is_empty() {
                    Vec::new()
                } else {
                    vec![note.plugin.clone()]
                },
                refuses: false,
            });
        }
        if report.pairs.is_empty() {
            continue;
        }
        let own = if report.implicit_pairs > 0 {
            format!("; {} between the game's own files", report.implicit_pairs)
        } else {
            String::new()
        };
        out.push(Finding::ListLevel {
            message: format!(
                "Info: {} conflict{} between {} pair{} of plugins{own}. Run \
                 'agora games instance plugins conflicts {instance_id}' to see them.",
                report.conflicts(),
                if report.conflicts() == 1 { "" } else { "s" },
                report.pairs.len(),
                if report.pairs.len() == 1 { "" } else { "s" },
            ),
            items: Vec::new(),
            refuses: false,
        });
        let Some(manifest) = &previous else {
            continue;
        };
        let previous_entries: Vec<crate::session_ledger::LoadOrderEntry> = manifest
            .load_order
            .iter()
            .filter(|entry| entry.list == rule.id)
            .cloned()
            .collect();
        let current: &[LoadOrderEntry] = orders
            .iter()
            .find(|order| order.list_id == rule.id)
            .map(|order| order.entries.as_slice())
            .unwrap_or(&[]);
        for pair in &report.pairs {
            if pair_is_new_since(pair, current, &previous_entries) {
                let label = pair
                    .labels
                    .first()
                    .map(|label| format!(" (e.g. {label})"))
                    .unwrap_or_default();
                out.push(Finding::ListLevel {
                    message: format!(
                        "Warning: '{}' now replaces {} change{} made by '{}'{label}. Keep this \
                         order, or let '{}' win.",
                        pair.winner,
                        pair.count,
                        if pair.count == 1 { "" } else { "s" },
                        pair.loser,
                        pair.loser,
                    ),
                    items: vec![pair.winner.clone(), pair.loser.clone()],
                    refuses: false,
                });
            }
        }
    }
    out
}

/// Every pre-launch load-order finding for the instance's lists: each list's own findings, the
/// overlaps report (MASTER_SPEC §31.5) and LOOT's checks (§31.6). The instance check, the launch
/// and `plugins check` all use this one function, so they cannot print different lines.
pub(crate) fn instance_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    orders: &[LoadOrder],
    links: Option<&DeploymentPlan>,
) -> Vec<Finding> {
    let mut out: Vec<Finding> = orders
        .iter()
        .flat_map(|order| order.findings.iter().cloned())
        .collect();
    out.extend(overlap_findings(ctx, instance_id, definition, orders));
    out.extend(sorter_findings(ctx, instance_id, definition, orders, links));
    out
}

/// The LOOT findings for the lists the caller read (MASTER_SPEC §31.6). A sorter with a saved
/// masterlist is asked for its order and checks; the answer is cached like the sorter notes, so a
/// plain list read never runs it. A missing masterlist, a failed sorter and an answer core cannot
/// use are all reported as "could not check", never as a clean list.
fn sorter_findings(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    orders: &[LoadOrder],
    links: Option<&DeploymentPlan>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for rule in &definition.load_orders {
        let Some(sorter) = rule.rules.sorter.as_ref() else {
            continue;
        };
        // A one-list check passes only that list's order; do not report another list's LOOT lines.
        let Some(order) = orders.iter().find(|order| order.list_id == rule.id) else {
            continue;
        };
        let status = match sorter_status(ctx, instance_id, definition, Some(&rule.id)) {
            Ok(status) => status,
            Err(error) => {
                out.push(could_not_check(format!(
                    "LOOT's status could not be read: {error}"
                )));
                continue;
            }
        };
        if sorter.update.is_some() && status.is_none() {
            out.push(no_masterlist_finding(ctx, instance_id));
            continue;
        }
        let installed = match installed_files(ctx, instance_id, definition, rule, links) {
            Ok((installed, _)) => installed,
            Err(error) => {
                out.push(could_not_check(format!(
                    "LOOT could not be checked: {error}"
                )));
                continue;
            }
        };
        // The cached findings name the instance in their message, so the key names it too; two
        // instances of one game must not read each other's "run sort <instance>" line.
        let fingerprint = format!(
            "{instance_id}|{}",
            sorter_fingerprint(&order.entries, &installed)
        );
        let data_version = status
            .as_ref()
            .map(|status| status.data_version.clone())
            .unwrap_or_default();
        let cache = ctx.extensions.get_or_insert_with(SorterCheckCache::default);
        if let Some(findings) = cache.lookup(
            definition.id.as_str(),
            &rule.id,
            &fingerprint,
            &data_version,
        ) {
            out.extend(findings);
            continue;
        }
        let reply = match call_sorter(ctx, instance_id, definition, rule, &order.entries) {
            Ok(reply) => reply,
            Err(error) => {
                out.push(could_not_check(format!(
                    "LOOT could not be checked: {error}"
                )));
                continue;
            }
        };
        let findings = match sorter_reply_findings(instance_id, rule, &order.entries, &reply) {
            Ok(findings) => findings,
            Err(error) => {
                out.push(could_not_check(format!(
                    "LOOT's answer could not be used: {error}"
                )));
                continue;
            }
        };
        cache.store(
            definition.id.as_str(),
            &rule.id,
            fingerprint,
            data_version,
            findings.clone(),
        );
        out.extend(findings);
    }
    out
}

/// A "could not check" finding. A failure is never shown as "nothing found".
fn could_not_check(message: String) -> Finding {
    Finding::ListLevel {
        message: format!("Warning: {message}"),
        items: Vec::new(),
        refuses: false,
    }
}

/// The one line a list with no downloaded masterlist gets, naming the setting when plugin network
/// access is off (MASTER_SPEC §31.6).
fn no_masterlist_finding(ctx: &Ctx, instance_id: &str) -> Finding {
    let mut message = format!(
        "Info: No LOOT masterlist is saved yet. Run 'agora games instance plugins masterlist \
         {instance_id}' to download one."
    );
    if plugin_network_enabled(ctx) == Some(false) {
        message.push_str(
            " Plugin network access is turned off in Settings; turn on 'network_plugins_enabled' \
             (with Lockdown Mode off) to let it download.",
        );
    }
    Finding::ListLevel {
        message,
        items: Vec::new(),
        refuses: false,
    }
}

/// Whether plugin JavaScript may reach the network. `None` when the settings store cannot be read,
/// so the caller does not claim a setting is off without evidence.
fn plugin_network_enabled(ctx: &Ctx) -> Option<bool> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    Some(crate::db::is_network_enabled(
        &conn,
        "network_plugins_enabled",
    ))
}

/// The findings a sorter's reply makes: the moves it would make, its own messages at their level
/// (an `info` message stays in the plugin view), and its checks at their level.
fn sorter_reply_findings(
    instance_id: &str,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
    reply: &SorterReply,
) -> Result<Vec<Finding>, LoadOrderError> {
    let mut out = Vec::new();
    let mut sorted = entries.to_vec();
    let moves = apply_sorter_order(&mut sorted, &reply.order)?;
    if !moves.is_empty() {
        out.push(Finding::SorterMoves {
            list: rule.id.clone(),
            instance: instance_id.to_string(),
            moves,
        });
    }
    for note in reply.notes.iter().chain(reply.general.iter()) {
        let level = note_level(&note.level)?;
        if level != NoteLevel::Info {
            out.push(note_finding(level, note));
        }
    }
    for note in &reply.checks {
        let level = note_level(&note.level)?;
        out.push(note_finding(level, note));
    }
    Ok(out)
}

fn note_level(level: &str) -> Result<NoteLevel, LoadOrderError> {
    match level {
        "error" => Ok(NoteLevel::Error),
        "warn" => Ok(NoteLevel::Warn),
        "info" => Ok(NoteLevel::Info),
        other => Err(LoadOrderError::SorterNote {
            reason: format!("`{other}` is not error, warn or info"),
        }),
    }
}

fn note_finding(level: NoteLevel, note: &SorterNote) -> Finding {
    let prefix = match level {
        NoteLevel::Error => "Error",
        NoteLevel::Warn => "Warning",
        NoteLevel::Info => "Info",
    };
    Finding::ListLevel {
        message: format!("{prefix}: {}", note.text),
        items: if note.item.is_empty() {
            Vec::new()
        } else {
            vec![note.item.clone()]
        },
        refuses: false,
    }
}

/// Ask the sorter for its notes now, cache them for the list as it stands, and return them. The
/// desktop calls this in the background when a plain read says the notes are out of date.
pub fn refresh_sorter_notes(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Vec<PluginNote>, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    if rule.rules.sorter.is_none() {
        return Ok(Vec::new());
    }
    let links = plan_for_order_in(ctx, instance_id, definition, Some(&rule.id))?;
    let listed = game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id))?;
    let (installed, names) = installed_files(ctx, instance_id, definition, rule, links.as_ref())?;
    let entries = build_order(
        rule,
        &listed.entries,
        Some(&installed),
        &names,
        listed.exists,
    )
    .entries;
    let reply = call_sorter(ctx, instance_id, definition, rule, &entries)?;
    let notes = sorter_notes(&reply)?;
    cache_sorter_notes(
        ctx,
        instance_id,
        definition,
        rule,
        &entries,
        &installed,
        &notes,
    );
    Ok(notes)
}

/// The saved state a list's sorter reports for its status line, when it has an update hook.
pub fn sorter_status(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Option<SorterStatus>, LoadOrderError> {
    sorter_update(ctx, instance_id, definition, list_id, false)
}

/// Run a list's sorter update hook, which downloads and verifies whatever the sorter keeps.
/// `None` means the list has no sorter, or its sorter declares no update.
pub fn update_sorter(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Option<SorterStatus>, LoadOrderError> {
    sorter_update(ctx, instance_id, definition, list_id, true)
}

fn sorter_update(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    update: bool,
) -> Result<Option<SorterStatus>, LoadOrderError> {
    let rule = list_by_id(definition, list_id)?;
    let Some(sorter) = rule.rules.sorter.as_ref() else {
        return Ok(None);
    };
    if sorter.update.is_none() {
        return Ok(None);
    }
    let package = package_of(ctx, definition)?;
    let request = sorter_request(ctx, instance_id, definition, rule, &[])?;
    let status = block_on_hook(package.update_sorter(request, update)).map_err(|error| {
        let mut reason = error.message;
        // The network gate names only the request kind. When it is the masterlist download that
        // was refused, say which setting turns plugin downloads back on (MASTER_SPEC §31.6).
        if update && reason.contains("turned off in Settings") {
            reason.push_str(
                " Turn on 'network_plugins_enabled' in Settings (with Lockdown Mode off) to \
                 download the masterlist.",
            );
        } else if update && reason.contains("Lockdown Mode is on") {
            reason.push_str(" Turn off Lockdown Mode in Settings to download the masterlist.");
        }
        LoadOrderError::SorterUpdate { reason }
    })?;
    // A successful update changes the sorter's data, so its saved notes are stale. Refresh them
    // now (the user already waited for the download); a notes failure does not undo the update.
    if update && status.is_some() {
        let _ = refresh_sorter_notes(ctx, instance_id, definition, list_id);
    }
    Ok(status)
}

/// Read a script list through its package's parse hook.
pub(crate) fn call_parse_format(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    bytes: Vec<u8>,
) -> Result<Vec<LoadOrderLine>, String> {
    let package = package_of(ctx, definition).map_err(|error| error.to_string())?;
    let context =
        hook_context(ctx, instance_id, definition, rule, &[]).map_err(|error| error.to_string())?;
    let result = block_on_hook(package.parse_load_order(context, &rule.id, bytes))
        .map_err(|error| error.message)?;
    result.ok_or_else(|| {
        format!(
            "the package declares a script format for '{}' but cannot parse it",
            rule.id
        )
    })
}

/// Render a script list through its package's render hook. The bytes go to the list's declared
/// file; the hook never receives or chooses a path.
pub(crate) fn call_render_format(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    lines: Vec<LoadOrderLine>,
) -> Result<Vec<u8>, String> {
    let package = package_of(ctx, definition).map_err(|error| error.to_string())?;
    let context =
        hook_context(ctx, instance_id, definition, rule, &[]).map_err(|error| error.to_string())?;
    let result = block_on_hook(package.render_load_order(context, &rule.id, lines))
        .map_err(|error| error.message)?;
    result.ok_or_else(|| {
        format!(
            "the package declares a script format for '{}' but cannot render it",
            rule.id
        )
    })
}

/// The plugin names in a game's `implicit_list_file`, one per line, in the file's order.
pub fn parse_implicit_list(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Build the effective order from the instance's list and the folder the game sees.
///
/// `installed` is `None` when the game's files are not known; then every item is taken as present.
/// `implicit_names` are the names read from the rule's `implicit_list_file`. A line in the list
/// that names an always-loaded item is dropped from the order: the game loads that item first
/// regardless. The game's own rules add badges and findings afterwards.
pub fn build_order(
    rule: &LoadOrderDefinition,
    listed: &[PluginEntry],
    installed: Option<&Installed>,
    implicit_names: &[String],
    exists: bool,
) -> LoadOrder {
    let mut implicit_keys = HashSet::new();
    let mut implicit = Vec::new();
    for name in rule.rules.implicit.iter().chain(implicit_names) {
        if implicit_keys.insert(key(name)) {
            implicit.push(name.clone());
        }
    }

    let mut entries = Vec::new();
    let make = |name: &str, is_implicit: bool, active: bool, managed: bool, locked: bool| {
        let k = key(name);
        let present = installed.is_none() || installed.is_some_and(|map| map.contains_key(&k));
        if !present && is_implicit {
            return None;
        }
        Some(LoadOrderEntry {
            name: name.to_string(),
            implicit: is_implicit,
            active: active || is_implicit,
            present,
            managed,
            locked,
            badges: Vec::new(),
            masters: Vec::new(),
        })
    };

    for name in &implicit {
        if let Some(entry) = make(name, true, true, false, false) {
            entries.push(entry);
        }
    }
    for item in listed {
        if implicit_keys.contains(&key(&item.name)) {
            continue;
        }
        if let Some(entry) = make(&item.name, false, item.active, item.managed, item.locked) {
            entries.push(entry);
        }
    }

    let findings = duplicate_findings(&entries);
    LoadOrder {
        list_id: rule.id.clone(),
        list_name: rule.name.clone(),
        rules_kind: rule.rules.kind.clone(),
        intro: rule.intro.clone(),
        item_heading: rule.item_heading.clone(),
        exists,
        entries,
        findings,
        notes: Vec::new(),
        notes_out_of_date: false,
        overlaps: None,
        overlaps_out_of_date: false,
    }
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// Whether the entry loads: active, and its file present.
fn loads(entry: &LoadOrderEntry) -> bool {
    entry.active && entry.present
}

/// Whether some loading item named `master` comes before position `at`.
fn master_loads_before(entries: &[LoadOrderEntry], at: usize, master: &str) -> bool {
    entries[..at]
        .iter()
        .any(|e| loads(e) && key(&e.name) == master)
}

/// Why `master` does not come before the plugin at `at`.
fn master_problem(entries: &[LoadOrderEntry], at: usize, master: &str) -> MasterProblem {
    let mut inactive = false;
    for (index, e) in entries.iter().enumerate() {
        if key(&e.name) != master {
            continue;
        }
        if loads(e) && index > at {
            return MasterProblem::Later;
        }
        if !loads(e) && e.present {
            inactive = true;
        }
    }
    if inactive {
        MasterProblem::Inactive
    } else {
        MasterProblem::Missing
    }
}

/// The groups of loading plugins that reach one another through their masters, as indices into
/// `entries`. Only groups that are a loop are returned: a plugin in one, or one that needs one.
fn cycle_groups(entries: &[LoadOrderEntry]) -> Vec<Vec<usize>> {
    let n = entries.len();
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, e) in entries.iter().enumerate() {
        if loads(e) {
            by_name.entry(key(&e.name)).or_default().push(index);
        }
    }
    let adjacent: Vec<Vec<usize>> = entries
        .iter()
        .map(|e| {
            e.masters
                .iter()
                .filter_map(|m| by_name.get(&key(m)))
                .flatten()
                .copied()
                .collect()
        })
        .collect();

    // reach[i][j]: j is reached from i by one or more master links.
    let mut reach = vec![vec![false; n]; n];
    for (start, row) in reach.iter_mut().enumerate() {
        let mut stack: Vec<usize> = adjacent[start].clone();
        while let Some(node) = stack.pop() {
            if row[node] {
                continue;
            }
            row[node] = true;
            stack.extend(adjacent[node].iter().copied());
        }
    }

    let mut assigned = vec![false; n];
    let mut groups = Vec::new();
    for i in 0..n {
        if assigned[i] || !reach[i][i] {
            continue;
        }
        let group: Vec<usize> = (0..n).filter(|&j| reach[i][j] && reach[j][i]).collect();
        for &j in &group {
            assigned[j] = true;
        }
        groups.push(group);
    }
    groups
}

/// The findings core raises without knowing the game: the same item listed more than once.
pub fn duplicate_findings(entries: &[LoadOrderEntry]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for e in entries.iter().filter(|e| !e.implicit) {
        let count = seen.entry(key(&e.name)).or_insert(0);
        *count += 1;
        if *count == 2 {
            out.push(Finding::DuplicateListing {
                plugin: e.name.clone(),
            });
        }
    }
    out
}

/// Apply a script rule reply to the entries: attach its badges and turn its list findings into
/// core findings, then check the rules themselves.
pub fn apply_reply(
    entries: &mut [LoadOrderEntry],
    reply: &LoadOrderRulesReply,
    list: &str,
) -> Vec<Finding> {
    for entry in entries.iter_mut() {
        if let Some(badges) = reply.badges.get(&entry.name) {
            entry.badges = badges.clone();
        } else {
            // A script may write a badge map with any case for the item's name.
            let wanted = key(&entry.name);
            if let Some((_, badges)) = reply.badges.iter().find(|(name, _)| key(name) == wanted) {
                entry.badges = badges.clone();
            }
        }
    }
    let mut out: Vec<Finding> = reply
        .findings
        .iter()
        .map(|finding: &ListFinding| Finding::ListLevel {
            message: finding.message.clone(),
            items: finding.items.clone(),
            refuses: finding.refuses,
        })
        .collect();
    out.extend(script_findings(entries, &reply.rules, list));
    out
}

// ---------------------------------------------------------------------------
// Script rules
//
// A `script` list returns `LoadOrderRule`s. Core turns them into the same "must come before"
// edges the Creation Engine's masters use, so the ordering and cycle check are shared. Missing
// items and conflicts (which are not an ordering) are findings of their own.
// ---------------------------------------------------------------------------

fn rule_prerequisites(
    entries: &[LoadOrderEntry],
    rules: &[LoadOrderRule],
    list: &str,
) -> (HashMap<String, Vec<String>>, Vec<Finding>) {
    let known = |name: &str| entries.iter().any(|entry| key(&entry.name) == key(name));
    let mut prereqs: HashMap<String, Vec<String>> = HashMap::new();
    let mut findings = Vec::new();
    for rule in rules {
        match rule {
            LoadOrderRule::Requires { item, master, .. } => {
                // A missing master is reported as a missing master below, in the words players
                // know, so only an absent *item* is a rule that cannot be applied.
                if rule.optional() && !(known(item) && known(master)) {
                    continue;
                }
                if !known(item) {
                    findings.push(Finding::MissingRuleItem {
                        list: list.to_string(),
                        item: item.clone(),
                        rule: "requires".to_string(),
                    });
                    continue;
                }
                let edges = prereqs.entry(key(item)).or_default();
                if !edges.iter().any(|edge| key(edge) == key(master)) {
                    edges.push(master.clone());
                }
            }
            LoadOrderRule::Before { first, second, .. } => {
                let both = known(first) && known(second);
                if rule.optional() && !both {
                    continue;
                }
                for name in [first, second] {
                    if !known(name) {
                        findings.push(Finding::MissingRuleItem {
                            list: list.to_string(),
                            item: name.clone(),
                            rule: "before".to_string(),
                        });
                    }
                }
                if both {
                    let edges = prereqs.entry(key(second)).or_default();
                    if !edges.iter().any(|edge| key(edge) == key(first)) {
                        edges.push(first.clone());
                    }
                }
            }
            LoadOrderRule::Conflicts { first, second, .. } => {
                let active = |name: &str| {
                    entries
                        .iter()
                        .any(|entry| key(&entry.name) == key(name) && loads(entry))
                };
                let both = active(first) && active(second);
                if rule.optional() && !both {
                    continue;
                }
                for name in [first, second] {
                    if !known(name) {
                        findings.push(Finding::MissingRuleItem {
                            list: list.to_string(),
                            item: name.clone(),
                            rule: "conflicts".to_string(),
                        });
                    }
                }
                if both {
                    findings.push(Finding::RuleConflict {
                        list: list.to_string(),
                        first: first.clone(),
                        second: second.clone(),
                    });
                }
            }
        }
    }
    (prereqs, findings)
}

/// Whether a sort could put this late master above the item: the master must be free to move, and
/// there must be a free slot above an item that itself cannot move.
fn can_move_master(entries: &[LoadOrderEntry], at: usize, master: &str) -> bool {
    let dependent = &entries[at];
    if (dependent.implicit || dependent.locked)
        && !entries[..at]
            .iter()
            .any(|entry| !entry.implicit && !entry.locked)
    {
        return false;
    }
    entries.iter().enumerate().skip(at + 1).any(|(_, entry)| {
        loads(entry) && key(&entry.name) == master && !entry.implicit && !entry.locked
    })
}

/// The findings a script list's rules raise against its current order: missing items, conflicts,
/// late prerequisites, and cycles. Pure, like [`findings`].
pub fn script_findings(
    entries: &[LoadOrderEntry],
    rules: &[LoadOrderRule],
    list: &str,
) -> Vec<Finding> {
    let (prereqs, mut out) = rule_prerequisites(entries, rules, list);
    for (at, entry) in entries.iter().enumerate() {
        if !loads(entry) {
            continue;
        }
        let Some(masters) = prereqs.get(&key(&entry.name)) else {
            continue;
        };
        let mut reported: HashSet<String> = HashSet::new();
        for master in masters {
            let k = key(master);
            if !reported.insert(k.clone()) || master_loads_before(entries, at, &k) {
                continue;
            }
            out.push(Finding::MasterNotEarlier {
                plugin: entry.name.clone(),
                master: master.clone(),
                problem: master_problem(entries, at, &k),
                movable: can_move_master(entries, at, &k),
            });
        }
    }

    let mut scratch = entries.to_vec();
    for entry in &mut scratch {
        entry.masters = prereqs.get(&key(&entry.name)).cloned().unwrap_or_default();
    }
    for group in cycle_groups(&scratch) {
        out.push(Finding::MasterCycle {
            plugins: group
                .iter()
                .map(|&index| scratch[index].name.clone())
                .collect(),
        });
    }
    out
}

/// Put the rules' prerequisite edges onto the entries, so sorting can see them.
pub fn set_prerequisites(entries: &mut [LoadOrderEntry], rules: &[LoadOrderRule], list: &str) {
    let (prereqs, _) = rule_prerequisites(entries, rules, list);
    for entry in entries.iter_mut() {
        entry.masters = prereqs.get(&key(&entry.name)).cloned().unwrap_or_default();
    }
}

/// Sort a script list by its rules, reusing the same fixed slots and cycle handling. The scratch
/// entries carry the rule edges as masters; the returned entries carry none of that, so the order
/// reads as it did.
pub fn script_sort(
    entries: &mut [LoadOrderEntry],
    rules: &[LoadOrderRule],
    list: &str,
) -> Result<(Vec<SortMove>, Vec<Blocked>), LoadOrderError> {
    let (prereqs, _) = rule_prerequisites(entries, rules, list);
    let mut scratch = entries.to_vec();
    for entry in &mut scratch {
        entry.masters = prereqs.get(&key(&entry.name)).cloned().unwrap_or_default();
    }
    let (moves, blocked) = sort_entries(&mut scratch)?;
    for entry in &mut scratch {
        entry.masters.clear();
    }
    entries.clone_from_slice(&scratch);
    Ok((moves, blocked))
}

// ---------------------------------------------------------------------------
// Sort and move
//
// Positions that a locked or always-loaded plugin holds never change. The other positions are
// the free slots, and a free plugin only trades places among them. `free_slots` lists those
// positions in order, and "slot index" below means an index into that list.
// ---------------------------------------------------------------------------

/// The entries a sort may not move: always loaded, locked, or in a master loop.
struct Fixed {
    cyclic: HashSet<String>,
}

impl Fixed {
    fn of(entries: &[LoadOrderEntry]) -> Self {
        let cyclic = cycle_groups(entries)
            .into_iter()
            .flatten()
            .map(|i| key(&entries[i].name))
            .collect();
        Self { cyclic }
    }

    fn cyclic(&self, e: &LoadOrderEntry) -> bool {
        self.cyclic.contains(&key(&e.name))
    }

    fn fixed(&self, e: &LoadOrderEntry) -> bool {
        e.implicit || e.locked || self.cyclic(e)
    }
}

/// The positions whose entries are not fixed, by `is_fixed`.
fn free_slots_of(
    entries: &[LoadOrderEntry],
    is_fixed: impl Fn(&LoadOrderEntry) -> bool,
) -> Vec<usize> {
    (0..entries.len())
        .filter(|&i| !is_fixed(&entries[i]))
        .collect()
}

/// Move the free entry at position `from` to slot index `place`, shifting the other free entries
/// along the free slots. Fixed entries keep their positions. Returns the position it now holds.
fn place_free(
    entries: &mut [LoadOrderEntry],
    free_slots: &[usize],
    from: usize,
    place: usize,
) -> usize {
    let mut items: Vec<LoadOrderEntry> = free_slots.iter().map(|&s| entries[s].clone()).collect();
    let taken = free_slots.iter().take_while(|&&s| s < from).count();
    let item = items.remove(taken);
    items.insert(place, item);
    for (slot, item) in free_slots.iter().zip(items) {
        entries[*slot] = item;
    }
    free_slots[place]
}

/// Whether moving the entry at `from` to absolute position `target` would put it above one of its
/// own masters, directly or through their masters. A sort must never break a master requirement
/// it is trying to keep; the dependent it was moving for is reported as blocked instead.
fn crosses_own_master(entries: &[LoadOrderEntry], from: usize, target: usize) -> bool {
    if target >= from {
        return false;
    }
    let mut loaded: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if loads(entry) {
            loaded.entry(key(&entry.name)).or_default().push(index);
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<String> = entries[from].masters.iter().map(|m| key(m)).collect();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        for &index in loaded.get(&name).map(Vec::as_slice).unwrap_or_default() {
            if index >= target && index < from {
                return true;
            }
            pending.extend(entries[index].masters.iter().map(|m| key(m)));
        }
    }
    false
}

/// The first place where a plugin needs a master that may move, as (master position, slot index
/// the master takes). A plugin that does not move takes the master just above it; a fixed plugin
/// (locked, or always loaded) takes the slot just above its own position, and if there is none,
/// the master is blocked instead.
fn first_move(
    entries: &[LoadOrderEntry],
    fixed: &Fixed,
    free_slots: &[usize],
) -> Option<(usize, usize)> {
    for (at, dependent) in entries.iter().enumerate() {
        if fixed.cyclic(dependent) {
            continue;
        }
        let above = free_slots.iter().take_while(|&&s| s < at).count();
        let place = if fixed.fixed(dependent) {
            match above.checked_sub(1) {
                Some(place) => place,
                None => continue,
            }
        } else {
            above
        };
        for master in &dependent.masters {
            let k = key(master);
            if master_loads_before(entries, at, &k) {
                continue;
            }
            let later = entries
                .iter()
                .enumerate()
                .skip(at + 1)
                .find(|(_, m)| loads(m) && key(&m.name) == k && !fixed.fixed(m))
                .map(|(index, _)| index);
            if let Some(from) = later {
                if crosses_own_master(entries, from, free_slots[place]) {
                    // Moving that master here would break one of its own masters, so this
                    // dependent stays blocked instead of trading one broken order for another.
                    continue;
                }
                return Some((from, place));
            }
        }
    }
    None
}

/// Sort a load order so every master comes before the plugins that need it, with as few moves as
/// the rules allow: each late master moves up to just before the first plugin that needs it, and
/// everything else keeps its relative order. Locked and always-loaded plugins never move. A plugin
/// that is above a master it cannot get past is reported as blocked. Plugins in a master loop are
/// left alone. Sorting a sorted order makes no moves.
pub fn sort_entries(
    entries: &mut [LoadOrderEntry],
) -> Result<(Vec<SortMove>, Vec<Blocked>), LoadOrderError> {
    let fixed = Fixed::of(entries);
    let free_slots = free_slots_of(entries, |e| fixed.fixed(e));
    let limit = entries
        .len()
        .saturating_mul(entries.len())
        .saturating_add(16);
    let mut moves = Vec::new();
    while let Some((from, place)) = first_move(entries, &fixed, &free_slots) {
        if moves.len() >= limit {
            return Err(LoadOrderError::DidNotSettle(moves.len()));
        }
        let plugin = entries[from].name.clone();
        let to = place_free(entries, &free_slots, from, place) + 1;
        moves.push(SortMove {
            plugin,
            from: from + 1,
            to,
        });
    }

    let blocked = blocked_of(entries, &fixed);
    Ok((moves, blocked))
}

/// The plugins that still load above a master after a sort, because neither can move.
fn blocked_of(entries: &[LoadOrderEntry], fixed: &Fixed) -> Vec<Blocked> {
    let mut blocked: Vec<Blocked> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for (at, dependent) in entries.iter().enumerate() {
        if fixed.cyclic(dependent) {
            continue;
        }
        for master in &dependent.masters {
            let k = key(master);
            if master_loads_before(entries, at, &k) {
                continue;
            }
            let stuck = entries
                .iter()
                .enumerate()
                .skip(at + 1)
                .any(|(_, m)| loads(m) && key(&m.name) == k && !fixed.cyclic(m));
            if stuck && seen.insert((key(&dependent.name), k.clone())) {
                blocked.push(Blocked {
                    plugin: dependent.name.clone(),
                    master: master.clone(),
                });
            }
        }
    }
    blocked
}

/// Reorder the free entries into a sorter's relative order, leaving fixed entries (always loaded,
/// locked, or part of a master loop) exactly where they are. Every free position receives the next
/// free plugin in the sorter's order. Returns the moves that changed a plugin's position.
///
/// The order is checked first: it must name every present item exactly once, case-insensitively,
/// and nothing else. A sorter that drops, adds or repeats an item is refused, so a bad reply can
/// never silently delete a plugin from the list.
fn apply_sorter_order(
    entries: &mut [LoadOrderEntry],
    desired: &[String],
) -> Result<Vec<SortMove>, LoadOrderError> {
    let mut expected: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter().filter(|entry| entry.present) {
        *expected.entry(key(&entry.name)).or_insert(0) += 1;
    }
    let mut remaining = expected.clone();
    for name in desired {
        let k = key(name);
        match remaining.get_mut(&k) {
            Some(count) if *count > 0 => *count -= 1,
            Some(_) => {
                return Err(LoadOrderError::SorterOrder {
                    reason: format!("names '{name}' more often than the list has it"),
                })
            }
            None => {
                return Err(LoadOrderError::SorterOrder {
                    reason: format!("names '{name}', which is not an item the list has"),
                })
            }
        }
    }
    if let Some((missing, _)) = remaining.iter().find(|(_, count)| **count > 0) {
        return Err(LoadOrderError::SorterOrder {
            reason: format!("does not name '{missing}'"),
        });
    }

    let fixed = Fixed::of(entries);
    let free_slots = free_slots_of(entries, |e| fixed.fixed(e));
    let mut wanted: HashMap<String, usize> = HashMap::new();
    for (index, name) in desired.iter().enumerate() {
        wanted.insert(key(name), index);
    }

    let mut free: Vec<(usize, LoadOrderEntry)> = free_slots
        .iter()
        .map(|&slot| (slot, entries[slot].clone()))
        .collect();
    let before: HashMap<String, usize> = free
        .iter()
        .map(|(slot, entry)| (key(&entry.name), *slot))
        .collect();
    free.sort_by_key(|(slot, entry)| {
        (
            wanted.get(&key(&entry.name)).copied().unwrap_or(usize::MAX),
            *slot,
        )
    });

    for (slot, entry) in free_slots.iter().zip(free.iter()) {
        entries[*slot] = entry.1.clone();
    }

    let mut moves = Vec::new();
    for slot in &free_slots {
        let entry = &entries[*slot];
        let from = before[&key(&entry.name)];
        if from != *slot {
            moves.push(SortMove {
                plugin: entry.name.clone(),
                from: from + 1,
                to: slot + 1,
            });
        }
    }
    Ok(moves)
}

/// Move a listed plugin to a position, or before or after another plugin. Returns the plugin's
/// name and its 1-based positions before and after. Locked plugins do not move, and always-loaded
/// plugins cannot be moved or placed against; a position that a locked or always-loaded plugin
/// holds cannot be taken.
pub fn move_entry(
    entries: &mut [LoadOrderEntry],
    plugin: &str,
    target: &MoveTarget,
) -> Result<(String, usize, usize), LoadOrderError> {
    let wanted = key(plugin.trim());
    let from = entries
        .iter()
        .position(|e| key(&e.name) == wanted)
        .ok_or_else(|| LoadOrderError::NotListed(plugin.to_string()))?;
    if entries[from].implicit {
        return Err(LoadOrderError::AlwaysLoaded(entries[from].name.clone()));
    }
    if entries[from].locked {
        return Err(LoadOrderError::Locked(entries[from].name.clone()));
    }
    let free_slots = free_slots_of(entries, |e| e.implicit || e.locked);
    let above = |position: usize| free_slots.iter().take_while(|&&s| s < position).count();
    let slots = free_slots.len();

    let place = match target {
        MoveTarget::Position(position) => {
            if *position == 0 || *position > entries.len() {
                return Err(LoadOrderError::BadPosition {
                    position: *position,
                    last: entries.len(),
                });
            }
            let slot = position - 1;
            match free_slots.iter().position(|&s| s == slot) {
                Some(place) => place,
                None => {
                    return Err(LoadOrderError::Held {
                        plugin: entries[from].name.clone(),
                        position: *position,
                        holder: entries[slot].name.clone(),
                    })
                }
            }
        }
        MoveTarget::Before(other) | MoveTarget::After(other) => {
            let other_key = key(other.trim());
            if other_key == wanted {
                return Err(LoadOrderError::SelfTarget(entries[from].name.clone()));
            }
            let k = entries
                .iter()
                .position(|e| key(&e.name) == other_key)
                .ok_or_else(|| LoadOrderError::NotListed(other.clone()))?;
            let before = matches!(target, MoveTarget::Before(_));
            if before && entries[k].implicit {
                return Err(LoadOrderError::AlwaysLoaded(entries[k].name.clone()));
            }
            let slot_above_k = above(k);
            let no_room = || LoadOrderError::NoRoom {
                plugin: entries[from].name.clone(),
                target: other.clone(),
            };
            if free_slots.contains(&k) {
                // The other plugin moves too: place beside its slot index, counting the moved
                // plugin out of the way when it sat above it.
                let beside = slot_above_k - usize::from(from < k);
                if before {
                    beside
                } else {
                    beside + 1
                }
            } else if before {
                match slot_above_k.checked_sub(1) {
                    Some(place) => place,
                    None => return Err(no_room()),
                }
            } else if slot_above_k < slots {
                slot_above_k
            } else {
                return Err(no_room());
            }
        }
    };
    let name = entries[from].name.clone();
    let to_position = place_free(entries, &free_slots, from, place);
    Ok((name, from + 1, to_position + 1))
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

fn rule_of<'a>(
    definition: &'a GameDefinition,
    list_id: Option<&str>,
) -> Result<&'a LoadOrderDefinition, LoadOrderError> {
    list_by_id(definition, list_id)
}

/// The names a game's `implicit_list_file` holds, as the instance's deployment provides it. Empty
/// when the deployment provides no such file. Sync reads the same names, so it never manages a
/// plugin the game loads itself.
pub fn implicit_list_names(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<String>, LoadOrderError> {
    implicit_list_names_in(ctx, instance_id, definition, None, None)
}

/// [`implicit_list_names`] for one list; `None` names the game's first list.
pub fn implicit_list_names_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    links: Option<&DeploymentPlan>,
) -> Result<Vec<String>, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let (_, names) = installed_files(ctx, instance_id, definition, rule, links)?;
    Ok(names)
}

/// The plugin folder the game sees, from the deployment plan, and the names its
/// `implicit_list_file` holds. The plan includes the writable layer, which the game also sees.
fn installed_files(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    links: Option<&DeploymentPlan>,
) -> Result<(Installed, Vec<String>), LoadOrderError> {
    let planned;
    let plan = match links {
        Some(plan) => plan,
        None => {
            planned = game_deploy::plan(ctx, instance_id, definition, DeployMode::Links).map_err(
                |e| LoadOrderError::Files {
                    instance_id: instance_id.to_string(),
                    reason: e.to_string(),
                },
            )?;
            &planned
        }
    };
    let folder = rule.items.folder.as_str().trim_matches('/');
    let patterns: Vec<String> = rule
        .items
        .patterns
        .iter()
        .map(|p| fold_path_case(p.trim()))
        .filter(|p| !p.is_empty())
        .collect();
    let list_file = rule
        .rules
        .implicit_list_file
        .as_ref()
        .map(|p| fold_path_case(p.as_str()));

    let mut installed = Installed::new();
    let mut list_path = None;
    for file in &plan.files {
        let path = file.path.as_str();
        if list_file.as_deref() == Some(fold_path_case(path).as_str()) {
            list_path = Some(game_deploy::source_path(ctx, &file.source));
            continue;
        }
        if rule.items.folders {
            // A folder is an item when it sits directly under the list's folder.
            let prefix = format!("{folder}/");
            let Some(rest) = path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let Some(name) = rest.split('/').next().filter(|name| !name.is_empty()) else {
                continue;
            };
            let lower = key(name);
            if patterns.iter().any(|p| glob_match(p, &lower)) {
                installed
                    .entry(lower)
                    .or_insert_with(|| game_deploy::source_path(ctx, &file.source));
            }
            continue;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if fold_path_case(parent) != fold_path_case(folder) {
            continue;
        }
        let lower = key(name);
        if patterns.iter().any(|p| glob_match(p, &lower)) {
            installed.insert(lower, game_deploy::source_path(ctx, &file.source));
        }
    }

    let names = match list_path {
        None => Vec::new(),
        Some(path) => {
            let bytes = std::fs::read(&path).map_err(|source| LoadOrderError::Io {
                path: path.clone(),
                source,
            })?;
            parse_implicit_list(&String::from_utf8_lossy(&bytes))
        }
    };
    Ok((installed, names))
}

/// The Links plan an engine game's load order reads its installed plugins from, planned once so a
/// caller can pass it to several `_with_view` reads. `None` for a game whose rules read no plan.
pub fn plan_for_order(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Option<DeploymentPlan>, LoadOrderError> {
    plan_for_order_in(ctx, instance_id, definition, None)
}

/// The Links plan any of the game's lists reads, planned once. `None` when no list reads one,
/// including a game with no lists.
pub fn plan_for_instance(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Option<DeploymentPlan>, LoadOrderError> {
    if !any_needs_plan(definition) {
        return Ok(None);
    }
    game_deploy::plan(ctx, instance_id, definition, DeployMode::Links)
        .map(Some)
        .map_err(|e| LoadOrderError::Files {
            instance_id: instance_id.to_string(),
            reason: e.to_string(),
        })
}

/// [`plan_for_order`] for one list; `None` names the game's first list.
pub fn plan_for_order_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Option<DeploymentPlan>, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    if !list_needs_plan(rule) {
        return Ok(None);
    }
    game_deploy::plan(ctx, instance_id, definition, DeployMode::Links)
        .map(Some)
        .map_err(|e| LoadOrderError::Files {
            instance_id: instance_id.to_string(),
            reason: e.to_string(),
        })
}

fn order_of(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<LoadOrder, LoadOrderError> {
    order_of_with_view(ctx, instance_id, definition, None, None, true)
}

fn order_of_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    links: Option<&DeploymentPlan>,
    read_sorter_notes: bool,
) -> Result<LoadOrder, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let listed = game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id))?;
    if !list_needs_plan(rule) {
        return Ok(build_order(rule, &listed.entries, None, &[], listed.exists));
    }

    let (installed, names) = installed_files(ctx, instance_id, definition, rule, links)?;
    let mut order = build_order(
        rule,
        &listed.entries,
        Some(&installed),
        &names,
        listed.exists,
    );
    if rule.rules.kind == "script" {
        match call_load_order_rules(ctx, instance_id, definition, rule, &order.entries) {
            Ok(reply) => {
                let findings = apply_reply(&mut order.entries, &reply, &rule.id);
                order.findings.extend(findings);
                // Keep the rule edges on the entries, so an overlaps classification can tell a
                // patched key (the winner lists every other changer as a master).
                set_prerequisites(&mut order.entries, &reply.rules, &rule.id);
            }
            Err(reason) => order.findings.push(Finding::RulesFailed {
                list: rule.id.clone(),
                reason,
            }),
        }
    }
    // A plain read never runs the sorter. It uses the notes the last sort or update saved, and
    // says they are out of date when the list or the sorter's data changed since.
    if read_sorter_notes && rule.rules.sorter.is_some() {
        let fingerprint = sorter_fingerprint(&order.entries, &installed);
        let data_version = sorter_data_version(ctx, instance_id, definition, rule);
        let cache = ctx.extensions.get_or_insert_with(SorterNotesCache::default);
        match cache.lookup(
            definition.id.as_str(),
            &rule.id,
            &fingerprint,
            &data_version,
        ) {
            Some(notes) => order.notes = notes,
            None => order.notes_out_of_date = true,
        }
    }
    // A plain read never runs the overlaps reader either. It shows the report the last check or
    // sort saved for this exact order, and says it is out of date when nothing matches.
    if rule.rules.overlaps.is_some() {
        match load_after_rules(ctx, instance_id, &rule.id) {
            Ok(rules) => {
                let fingerprint = overlaps_fingerprint(&order.entries, &installed, &rules);
                let data_version = sorter_data_version(ctx, instance_id, definition, rule);
                let cache = ctx.extensions.get_or_insert_with(OverlapsCache::default);
                match cache.lookup(
                    definition.id.as_str(),
                    &rule.id,
                    &fingerprint,
                    &data_version,
                ) {
                    Some(mut report) => {
                        // The report is cached by order, but "new since the last launch" also
                        // depends on the session ledger, which changes at each launch. Recompute
                        // those flags here so a launch never leaves a stale badge on screen.
                        mark_new_pairs(ctx, instance_id, &rule.id, &mut report, &order.entries);
                        order.overlaps = Some(report);
                    }
                    None => order.overlaps_out_of_date = true,
                }
            }
            Err(_) => order.overlaps_out_of_date = true,
        }
    }
    Ok(order)
}

/// The instance's load order, with what the game's rules say about it.
pub fn order(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<LoadOrder, LoadOrderError> {
    order_of(ctx, instance_id, definition)
}

/// [`order`] for one list; `None` names the game's first list. An id the game does not declare is
/// an error.
pub fn order_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<LoadOrder, LoadOrderError> {
    order_of_with_view(ctx, instance_id, definition, list_id, None, true)
}

/// Every list the game keeps, in the order it declares them. A game with no lists gives an empty
/// result, not an error.
pub fn orders(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<LoadOrder>, LoadOrderError> {
    definition
        .load_orders
        .iter()
        .map(|rule| order_of_with_view(ctx, instance_id, definition, Some(&rule.id), None, true))
        .collect()
}

/// Every list's order, reading `links` instead of planning once per list. A game with no lists
/// gives an empty result.
pub(crate) fn orders_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    links: Option<&DeploymentPlan>,
) -> Result<Vec<LoadOrder>, LoadOrderError> {
    definition
        .load_orders
        .iter()
        .map(|rule| order_of_with_view(ctx, instance_id, definition, Some(&rule.id), links, true))
        .collect()
}

/// The findings against the instance's order. A launch check calls this and acts on the result.
pub fn check(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<Finding>, LoadOrderError> {
    Ok(order_of(ctx, instance_id, definition)?.findings)
}

/// [`check`] for one list; `None` names the game's first list. It includes the overlaps report and
/// LOOT's checks, so the plugin check prints the same lines as the instance check and a launch.
pub fn check_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Vec<Finding>, LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let links = plan_for_order_in(ctx, instance_id, definition, Some(&rule.id))?;
    let order = order_of_with_view(
        ctx,
        instance_id,
        definition,
        Some(&rule.id),
        links.as_ref(),
        true,
    )?;
    Ok(instance_findings(
        ctx,
        instance_id,
        definition,
        std::slice::from_ref(&order),
        links.as_ref(),
    ))
}

/// The findings against every list the game keeps. A game with no lists has none.
pub fn check_all(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<Vec<Finding>, LoadOrderError> {
    orders(ctx, instance_id, definition)
        .map(|all| all.into_iter().flat_map(|order| order.findings).collect())
}

fn write_entries(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    rule: &LoadOrderDefinition,
    entries: &[LoadOrderEntry],
) -> Result<(), LoadOrderError> {
    let order: Vec<(String, bool)> = entries
        .iter()
        .filter(|e| !e.implicit)
        .map(|e| (e.name.clone(), e.active))
        .collect();
    game_plugins::write_order_locked_in(ctx, instance_id, definition, Some(&rule.id), &order)?;
    Ok(())
}

/// Sort the instance's list so masters come before the plugins that need them. With `dry_run` it
/// reports the moves and writes nothing.
pub fn sort(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    dry_run: bool,
) -> Result<SortReport, LoadOrderError> {
    Ok(sort_inner(ctx, instance_id, definition, None, dry_run, false)?.0)
}

/// [`sort`] for one list; `None` names the game's first list.
pub fn sort_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    dry_run: bool,
) -> Result<SortReport, LoadOrderError> {
    Ok(sort_inner(ctx, instance_id, definition, list_id, dry_run, false)?.0)
}

/// [`sort`], then the instance's load order as it stands afterwards. Both read the installed
/// plugins from one deployment plan, where calling [`sort`] and then [`order`] plans twice.
pub fn sort_and_order(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    dry_run: bool,
) -> Result<(SortReport, LoadOrder), LoadOrderError> {
    let (report, order) = sort_inner(ctx, instance_id, definition, None, dry_run, true)?;
    Ok((report, order.expect("the order was asked for")))
}

/// [`sort_and_order`] for one list; `None` names the game's first list.
pub fn sort_and_order_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    dry_run: bool,
) -> Result<(SortReport, LoadOrder), LoadOrderError> {
    let (report, order) = sort_inner(ctx, instance_id, definition, list_id, dry_run, true)?;
    Ok((report, order.expect("the order was asked for")))
}

fn sort_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    dry_run: bool,
    want_order: bool,
) -> Result<(SortReport, Option<LoadOrder>), LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    if rule.rules.kind == "none" {
        return Err(LoadOrderError::NoSort(rule.id.clone()));
    }
    if rule.rules.kind != "script" {
        return Err(LoadOrderError::NoRules(definition.id.as_str().to_string()));
    }
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "plugins-order",
        )
        .map_err(PluginListError::from)?;
    let links = plan_for_order_in(ctx, instance_id, definition, Some(&rule.id))?;
    let listed = game_plugins::list_in(ctx, instance_id, definition, Some(&rule.id))?;
    let (installed, names) = installed_files(ctx, instance_id, definition, rule, links.as_ref())?;
    let mut entries = build_order(
        rule,
        &listed.entries,
        Some(&installed),
        &names,
        listed.exists,
    )
    .entries;

    // The rules are needed for the fallback sort and for the badges; the list findings are
    // recomputed by the order read back afterwards.
    let reply =
        call_load_order_rules(ctx, instance_id, definition, rule, &entries).map_err(|reason| {
            LoadOrderError::RulesScript {
                list: rule.id.clone(),
                reason,
            }
        })?;
    set_prerequisites(&mut entries, &reply.rules, &rule.id);
    apply_load_after_rules(&mut entries, &load_after_rules(ctx, instance_id, &rule.id)?);

    let mut notes = Vec::new();
    let mut mode = SortMode::Masters;
    let mut message = None;
    let mut counted: HashMap<String, usize> = HashMap::new();
    let mut duplicated = false;
    for entry in entries.iter().filter(|entry| !entry.implicit) {
        let count = counted.entry(key(&entry.name)).or_insert(0);
        *count += 1;
        duplicated |= *count > 1;
    }

    // A sorter with an update hook needs its saved data before it can sort. Without it the
    // script returns the list unchanged, which used to look like a sort that could not move
    // anything; the masters-only sort is the honest fallback.
    let sorter_ready = match rule.rules.sorter.as_ref() {
        None => false,
        Some(sorter) if sorter.update.is_none() => true,
        Some(_) => matches!(
            sorter_status(ctx, instance_id, definition, Some(&rule.id)),
            Ok(Some(_))
        ),
    };
    let (moves, blocked) = if rule.rules.sorter.is_some() && sorter_ready && !duplicated {
        let reply = call_sorter(ctx, instance_id, definition, rule, &entries)?;
        notes = sorter_notes(&reply)?;
        let moves = apply_sorter_order(&mut entries, &reply.order)?;
        let fixed = Fixed::of(&entries);
        let blocked = blocked_of(&entries, &fixed);
        mode = SortMode::Sorter;
        (moves, blocked)
    } else {
        if rule.rules.sorter.is_some() {
            message = Some(
                if duplicated {
                    DUPLICATES_BY_MASTERS_ONLY
                } else {
                    SORTER_NEEDS_DATA
                }
                .to_string(),
            );
        }
        sort_entries(&mut entries)?
    };

    let written = !dry_run && !moves.is_empty();
    if written {
        write_entries(ctx, instance_id, definition, rule, &entries)?;
    }
    if !dry_run && mode == SortMode::Sorter {
        cache_sorter_notes(
            ctx,
            instance_id,
            definition,
            rule,
            &entries,
            &installed,
            &notes,
        );
    }
    let order = if want_order {
        let mut order = order_of_with_view(
            ctx,
            instance_id,
            definition,
            Some(&rule.id),
            links.as_ref(),
            false,
        )?;
        order.notes = notes;
        Some(order)
    } else {
        None
    };
    Ok((
        SortReport {
            moves,
            blocked,
            written,
            mode,
            message,
        },
        order,
    ))
}

/// Move one listed plugin to a position, or before or after another plugin.
pub fn move_plugin(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    plugin: &str,
    target: &MoveTarget,
) -> Result<MoveReport, LoadOrderError> {
    Ok(move_inner(
        ctx,
        instance_id,
        definition,
        None,
        plugin,
        target,
        false,
        false,
    )?
    .0)
}

/// [`move_plugin`] for one list; `None` names the game's first list.
pub fn move_plugin_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    plugin: &str,
    target: &MoveTarget,
) -> Result<MoveReport, LoadOrderError> {
    Ok(move_inner(
        ctx,
        instance_id,
        definition,
        list_id,
        plugin,
        target,
        false,
        false,
    )?
    .0)
}

/// [`move_plugin_in`], but a move that breaks a load-after rule removes that rule instead of being
/// refused (the CLI's `--break-rule`).
pub fn move_plugin_in_breaking_rules(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    plugin: &str,
    target: &MoveTarget,
) -> Result<MoveReport, LoadOrderError> {
    Ok(move_inner(
        ctx,
        instance_id,
        definition,
        list_id,
        plugin,
        target,
        true,
        false,
    )?
    .0)
}

/// [`move_plugin`], then the instance's load order as it stands afterwards. Both read the installed
/// plugins from one deployment plan, where calling [`move_plugin`] and then [`order`] plans twice.
pub fn move_plugin_and_order(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    plugin: &str,
    target: &MoveTarget,
) -> Result<(MoveReport, LoadOrder), LoadOrderError> {
    let (report, order) = move_inner(
        ctx,
        instance_id,
        definition,
        None,
        plugin,
        target,
        false,
        true,
    )?;
    Ok((report, order.expect("the order was asked for")))
}

/// [`move_plugin_and_order`] for one list; `None` names the game's first list.
pub fn move_plugin_and_order_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    plugin: &str,
    target: &MoveTarget,
) -> Result<(MoveReport, LoadOrder), LoadOrderError> {
    let (report, order) = move_inner(
        ctx,
        instance_id,
        definition,
        list_id,
        plugin,
        target,
        false,
        true,
    )?;
    Ok((report, order.expect("the order was asked for")))
}

// The public wrappers each name one combination of the two options, so their signatures stay
// small; this one carries both without a builder of its own.
#[allow(clippy::too_many_arguments)]
fn move_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    plugin: &str,
    target: &MoveTarget,
    break_rule: bool,
    want_order: bool,
) -> Result<(MoveReport, Option<LoadOrder>), LoadOrderError> {
    let rule = rule_of(definition, list_id)?;
    let _lock = ctx
        .lock_manager
        .acquire(
            LockResource::Instance(instance_id.to_string()),
            "plugins-order",
        )
        .map_err(PluginListError::from)?;
    let links = plan_for_order_in(ctx, instance_id, definition, Some(&rule.id))?;
    let mut entries = order_of_with_view(
        ctx,
        instance_id,
        definition,
        Some(&rule.id),
        links.as_ref(),
        false,
    )?
    .entries;
    let (name, from, to) = move_entry(&mut entries, plugin, target)?;
    let mut removed_rules = Vec::new();
    let broken = broken_rules(&entries, &load_after_rules(ctx, instance_id, &rule.id)?);
    if let Some(first) = broken.first() {
        if !break_rule {
            return Err(LoadOrderError::BreaksRule {
                plugin: name,
                item: first.item.clone(),
                after: first.after.clone(),
            });
        }
        for broken in &broken {
            remove_load_after_rule(
                ctx,
                instance_id,
                definition,
                Some(rule.id.as_str()),
                &broken.item,
                &broken.after,
            )?;
            removed_rules.push(broken.clone());
        }
    }
    let written = from != to;
    if written {
        write_entries(ctx, instance_id, definition, rule, &entries)?;
    }
    let order = if want_order {
        Some(order_of_with_view(
            ctx,
            instance_id,
            definition,
            Some(&rule.id),
            links.as_ref(),
            true,
        )?)
    } else {
        None
    };
    Ok((
        MoveReport {
            plugin: name,
            from,
            to,
            written,
            removed_rules,
        },
        order,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, masters: &[&str]) -> LoadOrderEntry {
        LoadOrderEntry {
            name: name.to_string(),
            implicit: false,
            active: true,
            present: true,
            managed: false,
            locked: false,
            badges: Vec::new(),
            masters: masters.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn implicit(name: &str) -> LoadOrderEntry {
        LoadOrderEntry {
            implicit: true,
            ..entry(name, &[])
        }
    }

    fn locked(name: &str, masters: &[&str]) -> LoadOrderEntry {
        LoadOrderEntry {
            locked: true,
            ..entry(name, masters)
        }
    }

    fn requires(item: &str, master: &str) -> LoadOrderRule {
        LoadOrderRule::Requires {
            item: item.into(),
            master: master.into(),
            optional: false,
        }
    }

    fn names(entries: &[LoadOrderEntry]) -> Vec<String> {
        entries.iter().map(|e| e.name.clone()).collect()
    }

    fn position_of(entries: &[LoadOrderEntry], name: &str) -> usize {
        entries
            .iter()
            .position(|entry| key(&entry.name) == key(name))
            .expect("the list holds the name")
    }

    fn plugin_line_of(name: &str, active: bool) -> PluginEntry {
        PluginEntry {
            name: name.to_string(),
            active,
            managed: false,
            locked: false,
        }
    }

    fn script_rule() -> LoadOrderDefinition {
        LoadOrderDefinition {
            id: "plugins".into(),
            name: "Plugins".into(),
            file: agora_game_api::RelPath::new("user/Plugins.txt").unwrap(),
            destination: Default::default(),
            items: agora_game_api::LoadOrderItems {
                folder: agora_game_api::RelPath::new("Data").unwrap(),
                patterns: vec!["*.esm".into(), "*.esl".into(), "*.esp".into()],
                folders: false,
            },
            format: agora_game_api::LoadOrderFormat::Lines {
                active_prefix: "*".into(),
                header: Vec::new(),
            },
            rules: agora_game_api::LoadOrderRules {
                kind: "script".into(),
                entry: None,
                implicit: vec!["Skyrim.esm".into(), "Dawnguard.esm".into()],
                implicit_list_file: Some(agora_game_api::RelPath::new("Skyrim.ccc").unwrap()),
                sorter: None,
                overlaps: None,
            },
            intro: Some("Read this list from the top down.".into()),
            item_heading: Some("Plugin".into()),
        }
    }

    #[test]
    fn the_implicit_list_file_skips_blanks_and_comments_and_keeps_order() {
        let text = "\u{feff}# header\r\n\r\nccBGSSSE001-Fish.esm\r\n  ccbgssse037-curios.esl  \r\n";
        assert_eq!(
            parse_implicit_list(text),
            ["ccBGSSSE001-Fish.esm", "ccbgssse037-curios.esl"]
        );
    }

    #[test]
    fn a_late_master_is_a_finding_and_names_its_problem() {
        let late = [entry("A.esp", &[]), entry("B.esm", &[])];
        assert_eq!(
            script_findings(&late, &[requires("A.esp", "B.esm")], "plugins"),
            [Finding::MasterNotEarlier {
                plugin: "A.esp".into(),
                master: "B.esm".into(),
                problem: MasterProblem::Later,
                movable: true,
            }]
        );
    }

    #[test]
    fn a_missing_or_inactive_master_is_a_finding() {
        let mut inactive = entry("B.esm", &[]);
        inactive.active = false;
        let inactive_list = [inactive, entry("A.esp", &[])];
        assert_eq!(
            script_findings(&inactive_list, &[requires("A.esp", "B.esm")], "plugins"),
            [Finding::MasterNotEarlier {
                plugin: "A.esp".into(),
                master: "B.esm".into(),
                problem: MasterProblem::Inactive,
                movable: false,
            }]
        );
        let missing = [entry("A.esp", &[])];
        assert_eq!(
            script_findings(&missing, &[requires("A.esp", "Gone.esm")], "plugins"),
            [Finding::MasterNotEarlier {
                plugin: "A.esp".into(),
                master: "Gone.esm".into(),
                problem: MasterProblem::Missing,
                movable: false,
            }]
        );
    }

    #[test]
    fn a_master_listed_but_not_installed_is_missing_not_inactive() {
        let mut absent = entry("B.esm", &[]);
        absent.present = false;
        let list = [absent, entry("A.esp", &[])];
        assert!(matches!(
            script_findings(&list, &[requires("A.esp", "B.esm")], "plugins").as_slice(),
            [Finding::MasterNotEarlier {
                problem: MasterProblem::Missing,
                ..
            }]
        ));
    }

    #[test]
    fn a_rule_whose_item_is_absent_cannot_be_applied() {
        let list = [entry("A.esp", &[])];
        assert!(matches!(
            script_findings(&list, &[requires("Gone.esp", "B.esm")], "plugins").as_slice(),
            [Finding::MissingRuleItem { item, .. }] if item == "Gone.esp"
        ));
    }

    #[test]
    fn a_master_cycle_is_reported_and_sorting_it_terminates_without_moving_it() {
        let list = [entry("A.esp", &["B.esp"]), entry("B.esp", &["A.esp"])];
        assert!(script_findings(
            &list,
            &[requires("A.esp", "B.esp"), requires("B.esp", "A.esp")],
            "plugins"
        )
        .contains(&Finding::MasterCycle {
            plugins: vec!["A.esp".into(), "B.esp".into()],
        }));
        let mut sorted = list.to_vec();
        let (moves, _) = sort_entries(&mut sorted).unwrap();
        assert!(moves.is_empty());
        assert_eq!(sorted, list);
    }

    #[test]
    fn a_plugin_listed_twice_is_a_finding_once() {
        let list = [entry("A.esp", &[]), entry("a.ESP", &[])];
        assert_eq!(
            duplicate_findings(&list),
            [Finding::DuplicateListing {
                plugin: "a.ESP".into()
            }]
        );
    }

    #[test]
    fn names_compare_case_insensitively_for_masters() {
        let list = [entry("B.ESM", &[]), entry("A.esp", &[])];
        assert!(script_findings(&list, &[requires("A.esp", "b.esm")], "plugins").is_empty());
    }

    #[test]
    fn a_reply_attaches_badges_and_list_findings_with_their_refusal() {
        let mut list = [entry("Skyrim.esm", &[]), entry("Mod.esp", &[])];
        let reply = LoadOrderRulesReply {
            rules: Vec::new(),
            badges: [("skyrim.ESM".to_string(), vec!["Master".to_string()])]
                .into_iter()
                .collect(),
            findings: vec![ListFinding {
                message: "300 full plugins are active; the game loads at most 254".into(),
                items: Vec::new(),
                refuses: true,
            }],
        };
        let findings = apply_reply(&mut list, &reply, "plugins");
        assert_eq!(list[0].badges, ["Master"]);
        assert!(list[1].badges.is_empty());
        assert_eq!(findings.len(), 1);
        assert!(findings[0].refuses_launch());
        assert_eq!(
            findings[0].message(),
            "300 full plugins are active; the game loads at most 254"
        );
    }

    #[test]
    fn sort_fixes_a_late_master_with_one_move_and_keeps_the_rest() {
        let mut list = vec![
            entry("A.esp", &["B.esm"]),
            entry("C.esp", &[]),
            entry("B.esm", &[]),
            entry("D.esp", &[]),
        ];
        let (moves, blocked) = sort_entries(&mut list).unwrap();
        assert_eq!(
            moves,
            [SortMove {
                plugin: "B.esm".into(),
                from: 3,
                to: 1
            }]
        );
        assert!(blocked.is_empty());
        assert_eq!(names(&list), ["B.esm", "A.esp", "C.esp", "D.esp"]);
    }

    #[test]
    fn a_chain_of_late_masters_is_sorted_and_the_second_sort_does_nothing() {
        let mut list = vec![
            entry("A.esp", &["B.esm"]),
            entry("B.esm", &["C.esm"]),
            entry("C.esm", &["D.esm"]),
            entry("D.esm", &[]),
        ];
        let (first, _) = sort_entries(&mut list).unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(names(&list), ["D.esm", "C.esm", "B.esm", "A.esp"]);
        let settled = list.clone();
        let (second, _) = sort_entries(&mut list).unwrap();
        assert!(second.is_empty());
        assert_eq!(list, settled);
    }

    #[test]
    fn a_sort_does_not_pull_a_master_above_the_chain_of_its_own_masters() {
        // A needs B, B needs C, and C needs the locked M.esm. A is above B, so the sort would like
        // to move B up; that would put B above C (which B needs) and C above M.esm (which C needs).
        let mut list = vec![
            entry("A.esp", &["B.esp"]),
            locked("M.esm", &[]),
            entry("C.esp", &["M.esm"]),
            entry("B.esp", &["C.esp"]),
        ];
        let (moves, blocked) = sort_entries(&mut list).unwrap();
        assert!(moves.is_empty(), "the sort moved a plugin above its own master: {moves:?}");
        assert!(
            position_of(&list, "M.esm") < position_of(&list, "C.esp"),
            "C.esp was pulled above its locked master M.esm: {list:?}"
        );
        assert!(
            position_of(&list, "C.esp") < position_of(&list, "B.esp"),
            "B.esp was pulled above its master C.esp: {list:?}"
        );
        assert_eq!(
            blocked,
            [Blocked {
                plugin: "A.esp".into(),
                master: "B.esp".into()
            }]
        );
    }

    #[test]
    fn a_sort_does_not_pull_a_master_above_a_master_it_only_reaches_indirectly() {
        // B's direct master D is above the slot B would take, so the direct lookup sees nothing
        // wrong; but D needs M.esm, which is below that slot. Moving B up would cross M.esm.
        let mut list = vec![
            entry("X.esp", &[]),
            entry("D.esp", &["M.esm"]),
            entry("A.esp", &["B.esp"]),
            locked("M.esm", &[]),
            entry("B.esp", &["D.esp"]),
        ];
        let (moves, _) = sort_entries(&mut list).unwrap();
        assert!(moves.is_empty(), "the sort moved a plugin above an indirect master: {moves:?}");
        assert!(
            position_of(&list, "M.esm") < position_of(&list, "B.esp"),
            "B.esp was pulled above an indirect master M.esm: {list:?}"
        );
    }

    #[test]
    fn sort_is_idempotent() {
        let mut list = vec![
            entry("A.esp", &["C.esm"]),
            entry("B.esp", &["D.esm"]),
            entry("C.esm", &[]),
            entry("D.esm", &["C.esm"]),
            entry("E.esp", &[]),
        ];
        let (first, _) = sort_entries(&mut list).unwrap();
        assert!(!first.is_empty());
        let once = list.clone();
        let (second, _) = sort_entries(&mut list).unwrap();
        assert!(second.is_empty());
        assert_eq!(list, once);
    }

    #[test]
    fn a_locked_master_below_its_dependent_is_blocked_and_nothing_moves() {
        let mut list = vec![entry("A.esp", &["B.esm"]), locked("B.esm", &[])];
        let (moves, blocked) = sort_entries(&mut list).unwrap();
        assert!(moves.is_empty());
        assert_eq!(
            blocked,
            [Blocked {
                plugin: "A.esp".into(),
                master: "B.esm".into()
            }]
        );
        assert_eq!(names(&list), ["A.esp", "B.esm"]);
    }

    #[test]
    fn always_loaded_plugins_are_never_moved_and_stay_first() {
        let mut list = vec![
            implicit("Skyrim.esm"),
            entry("A.esp", &["Skyrim.esm"]),
            implicit("Dawnguard.esm"),
        ];
        let (moves, _) = sort_entries(&mut list).unwrap();
        assert!(moves.is_empty());
        assert_eq!(names(&list), ["Skyrim.esm", "A.esp", "Dawnguard.esm"]);
    }

    #[test]
    fn move_to_a_position_before_and_after() {
        let base = || {
            vec![
                implicit("Skyrim.esm"),
                entry("A.esp", &[]),
                entry("B.esp", &[]),
                entry("C.esp", &[]),
            ]
        };
        let mut list = base();
        let moved = move_entry(&mut list, "C.esp", &MoveTarget::Position(2)).unwrap();
        assert_eq!(moved, ("C.esp".to_string(), 4, 2));
        assert_eq!(names(&list), ["Skyrim.esm", "C.esp", "A.esp", "B.esp"]);

        let mut list = base();
        let moved = move_entry(&mut list, "A.esp", &MoveTarget::Before("C.esp".into())).unwrap();
        assert_eq!(moved, ("A.esp".to_string(), 2, 3));
        assert_eq!(names(&list), ["Skyrim.esm", "B.esp", "A.esp", "C.esp"]);

        let mut list = base();
        move_entry(&mut list, "C.esp", &MoveTarget::Before("A.esp".into())).unwrap();
        assert_eq!(names(&list), ["Skyrim.esm", "C.esp", "A.esp", "B.esp"]);
    }

    #[test]
    fn move_refuses_what_it_must() {
        let base = || {
            vec![
                implicit("Skyrim.esm"),
                entry("A.esp", &[]),
                entry("B.esp", &[]),
            ]
        };
        let mut list = base();
        assert!(matches!(
            move_entry(&mut list, "Skyrim.esm", &MoveTarget::Position(3)),
            Err(LoadOrderError::AlwaysLoaded(_))
        ));
        let mut list = base();
        assert!(matches!(
            move_entry(&mut list, "A.esp", &MoveTarget::Position(1)),
            Err(LoadOrderError::Held { .. })
        ));
        let mut list = base();
        assert!(matches!(
            move_entry(&mut list, "A.esp", &MoveTarget::Before("Skyrim.esm".into())),
            Err(LoadOrderError::AlwaysLoaded(_))
        ));
        let mut list = base();
        assert!(matches!(
            move_entry(&mut list, "A.esp", &MoveTarget::After("A.esp".into())),
            Err(LoadOrderError::SelfTarget(_))
        ));
    }

    #[test]
    fn the_order_puts_present_implicit_plugins_first_and_drops_listed_copies_of_them() {
        let installed: Installed = ["Skyrim.esm", "ccbgssse037-curios.esl", "Mod.esp"]
            .iter()
            .map(|name| (fold_path_case(name), PathBuf::from(name)))
            .collect();
        let names = vec![
            "ccBGSSSE001-Fish.esm".to_string(),
            "ccBGSSSE037-Curios.esl".to_string(),
        ];
        let listed = [
            plugin_line_of("Skyrim.esm", true),
            plugin_line_of("Mod.esp", true),
        ];
        let order = build_order(&script_rule(), &listed, Some(&installed), &names, true);

        let order_names: Vec<&str> = order.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            order_names,
            ["Skyrim.esm", "ccBGSSSE037-Curios.esl", "Mod.esp"]
        );
        assert!(order.entries[0].implicit && order.entries[1].implicit);
        assert!(!order.entries[2].implicit);
        assert_eq!(
            order.intro.as_deref(),
            Some("Read this list from the top down.")
        );
        assert_eq!(order.item_heading.as_deref(), Some("Plugin"));
        assert!(order.findings.is_empty(), "{:?}", order.findings);
    }

    #[test]
    fn a_plugin_that_is_listed_but_not_installed_is_shown_missing() {
        let installed: Installed = [("Skyrim.esm", PathBuf::from("Skyrim.esm"))]
            .into_iter()
            .map(|(name, path)| (fold_path_case(name), path))
            .collect();
        let listed = [plugin_line_of("Gone.esp", true)];
        let order = build_order(&script_rule(), &listed, Some(&installed), &[], true);
        let gone = order.entries.iter().find(|e| e.name == "Gone.esp").unwrap();
        assert!(!gone.present);
        assert!(order.findings.is_empty());
    }

    #[test]
    fn a_game_without_rules_lists_its_items_with_no_findings() {
        let mut rule = script_rule();
        rule.rules.kind = "none".into();
        let listed = [plugin_line_of("A.esp", true), plugin_line_of("B.esp", true)];
        let order = build_order(&rule, &listed, None, &[], true);
        assert!(order.entries.iter().all(|e| e.present));
        assert!(order.findings.is_empty());
    }

    #[test]
    fn a_script_order_reads_the_game_files_and_a_none_order_does_not() {
        let script = script_rule();
        assert!(list_needs_plan(&script));
        let mut none = script;
        none.rules.kind = "none".into();
        none.items.folders = false;
        none.rules.sorter = None;
        assert!(!list_needs_plan(&none));
    }

    #[test]
    fn a_sorter_order_maps_onto_free_slots_and_leaves_fixed_ones() {
        let mut list = vec![
            implicit("Skyrim.esm"),
            entry("A.esp", &[]),
            locked("L.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let desired = vec![
            "C.esp".to_string(),
            "B.esp".to_string(),
            "A.esp".to_string(),
            "L.esp".to_string(),
            "Skyrim.esm".to_string(),
        ];
        let moves = apply_sorter_order(&mut list, &desired).unwrap();
        assert_eq!(
            names(&list),
            ["Skyrim.esm", "C.esp", "L.esp", "B.esp", "A.esp"]
        );
        assert!(list[0].implicit && list[2].locked);
        assert_eq!(
            moves.iter().map(|m| m.plugin.as_str()).collect::<Vec<_>>(),
            ["C.esp", "A.esp"]
        );
        assert!(apply_sorter_order(&mut list, &desired).unwrap().is_empty());
    }

    #[test]
    fn an_empty_or_single_plugin_list_sorts_to_itself() {
        let mut empty: Vec<LoadOrderEntry> = Vec::new();
        let (moves, blocked) = sort_entries(&mut empty).unwrap();
        assert!(moves.is_empty() && blocked.is_empty());
        assert!(empty.is_empty());

        let mut single = vec![entry("Only.esp", &[])];
        let (moves, blocked) = sort_entries(&mut single).unwrap();
        assert!(moves.is_empty() && blocked.is_empty());
        assert_eq!(names(&single), ["Only.esp"]);
    }

    fn overlap_item(name: &str, digest: &str) -> agora_game_api::OverlapItem {
        agora_game_api::OverlapItem {
            name: name.to_string(),
            digest: digest.to_string(),
        }
    }

    fn overlap_key(key: &str, label: &str, items: &[(&str, &str)]) -> agora_game_api::OverlapKey {
        agora_game_api::OverlapKey {
            key: key.to_string(),
            label: label.to_string(),
            items: items
                .iter()
                .map(|(name, digest)| overlap_item(name, digest))
                .collect(),
        }
    }

    fn overlap_reply(keys: Vec<agora_game_api::OverlapKey>) -> agora_game_api::OverlapReply {
        agora_game_api::OverlapReply {
            keys,
            notes: Vec::new(),
        }
    }

    fn after(item: &str, after: &str) -> LoadAfterRule {
        LoadAfterRule {
            item: item.to_string(),
            after: after.to_string(),
        }
    }

    #[test]
    fn the_last_provider_wins_and_each_other_changer_loses_its_change() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("B.esp", "middle"),
                ("C.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(
            report.pairs,
            [
                ConflictPair {
                    winner: "C.esp".into(),
                    loser: "A.esp".into(),
                    count: 1,
                    labels: vec!["GMST fJumpHeightMin".into()],
                    new_since_last_launch: false,
                },
                ConflictPair {
                    winner: "C.esp".into(),
                    loser: "B.esp".into(),
                    count: 1,
                    labels: vec!["GMST fJumpHeightMin".into()],
                    new_since_last_launch: false,
                },
            ]
        );
        assert_eq!(report.conflicts(), 2);
    }

    #[test]
    fn the_definer_never_loses_when_a_later_plugin_changes_the_record() {
        // An earlier holder whose copy is the original is the record's definer. Overriding it is
        // what a mod is for, so it is never reported as a loser (MASTER_SPEC §31.2).
        let list = [entry("Base.esm", &[]), entry("Mod.esp", &[])];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[("Base.esm", "base"), ("Mod.esp", "changed")],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert!(report.pairs.is_empty());
        assert_eq!(report.patched, 0);
        assert_eq!(report.decided, 0);
        assert_eq!(report.implicit_pairs, 0);
    }

    #[test]
    fn an_item_whose_digest_equals_the_previous_holder_is_not_a_changer() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("B.esp", "old"),
                ("C.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(
            report.pairs,
            [ConflictPair {
                winner: "C.esp".into(),
                loser: "A.esp".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: false,
            }]
        );
    }

    #[test]
    fn when_every_plugin_agrees_there_is_nothing_to_show() {
        let list = [entry("A.esp", &[]), entry("B.esp", &[])];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[("A.esp", "same"), ("B.esp", "same")],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert!(report.pairs.is_empty());
        assert_eq!(report.patched, 0);
        assert_eq!(report.decided, 0);
        assert_eq!(report.implicit_pairs, 0);
    }

    #[test]
    fn a_reply_that_leaves_out_agreeing_keys_reports_only_the_conflict() {
        // The reader returns only keys with two or more changing digests. A key every plugin
        // copies identically is simply absent, which is not a failure and not a conflict.
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[("Base.esm", "original"), ("A.esp", "old"), ("B.esp", "new")],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].winner, "B.esp");
        assert_eq!(report.pairs[0].loser, "A.esp");
    }

    #[test]
    fn a_winner_that_lists_a_changer_as_a_master_patches_that_pair() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("Patch.esp", &["Base.esm", "A.esp"]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("Patch.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert!(report.pairs.is_empty());
        assert_eq!(report.patched, 1);
    }

    #[test]
    fn one_changer_the_winner_does_not_master_does_not_turn_the_others_into_conflicts() {
        // The winner patches A on purpose, but nothing says it knows about C. Each changer is
        // judged on its own, so C loses while A stays resolved.
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("C.esp", &[]),
            entry("Patch.esp", &["Base.esm", "A.esp"]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("C.esp", "other"),
                ("Patch.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(report.patched, 1);
        assert_eq!(
            report.pairs,
            [ConflictPair {
                winner: "Patch.esp".into(),
                loser: "C.esp".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: false,
            }]
        );
    }

    #[test]
    fn a_winner_does_not_patch_through_an_inactive_master() {
        // Middle.esp is listed but switched off, so the game does not load it and it cannot carry
        // Patch.esp's patch of A.esp. The pair is a real conflict, not a patch.
        let mut middle = entry("Middle.esp", &["Base.esm", "A.esp"]);
        middle.active = false;
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            middle,
            entry("Patch.esp", &["Middle.esp"]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("Patch.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(report.patched, 0, "an inactive master patched the pair: {report:?}");
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].winner, "Patch.esp");
        assert_eq!(report.pairs[0].loser, "A.esp");
    }

    #[test]
    fn a_winner_does_not_patch_through_a_master_that_is_not_listed() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("Patch.esp", &["Gone.esp"]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("Patch.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(report.patched, 0, "a missing master patched the pair: {report:?}");
        assert_eq!(report.pairs.len(), 1);
    }

    #[test]
    fn a_winner_that_masters_a_changer_through_an_intermediate_master_is_patched() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("Middle.esp", &["Base.esm", "A.esp"]),
            entry("Patch.esp", &["Middle.esp"]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("Patch.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert!(report.pairs.is_empty());
        assert_eq!(report.patched, 1);
    }

    #[test]
    fn a_pair_between_the_games_own_files_is_counted_and_not_listed() {
        let list = [
            implicit("Skyrim.esm"),
            implicit("Dawnguard.esm"),
            implicit("HearthFires.esm"),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Skyrim.esm", "original"),
                ("Dawnguard.esm", "first"),
                ("HearthFires.esm", "second"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert!(report.pairs.is_empty());
        assert_eq!(report.implicit_pairs, 1);
        assert_eq!(report.conflicts(), 0);
    }

    #[test]
    fn a_pair_between_the_games_own_files_and_a_mod_is_still_a_conflict() {
        let list = [
            implicit("Skyrim.esm"),
            implicit("Dawnguard.esm"),
            entry("Mod.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Skyrim.esm", "original"),
                ("Dawnguard.esm", "first"),
                ("Mod.esp", "second"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[]).unwrap();
        assert_eq!(report.implicit_pairs, 0);
        assert_eq!(
            report.pairs,
            [ConflictPair {
                winner: "Mod.esp".into(),
                loser: "Dawnguard.esm".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: false,
            }]
        );
    }

    #[test]
    fn a_load_after_rule_decides_the_pair_it_orders() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("B.esp", "middle"),
                ("C.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[after("C.esp", "A.esp")]).unwrap();
        assert_eq!(report.decided, 1);
        assert_eq!(
            report.decided_pairs,
            [ConflictPair {
                winner: "C.esp".into(),
                loser: "A.esp".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: false,
            }]
        );
        assert_eq!(
            report.decided_losers_of("A.esp"),
            [("C.esp".to_string(), 1)]
        );
        assert_eq!(
            report.decided_wins_over("C.esp"),
            [("A.esp".to_string(), 1)]
        );
        assert_eq!(
            report.pairs,
            [ConflictPair {
                winner: "C.esp".into(),
                loser: "B.esp".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: false,
            }]
        );
    }

    #[test]
    fn a_transitive_load_after_chain_decides_the_pair_it_orders() {
        // The player ordered B after A and C after B. B has no record of its own, so only the
        // chain of rules orders C after A.
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("C.esp", "new"),
            ],
        )]);
        let rules = [after("B.esp", "A.esp"), after("C.esp", "B.esp")];
        let report = classify_overlaps(&list, &reply, &rules).unwrap();
        assert_eq!(report.decided, 1, "{report:?}");
        assert!(report.pairs.is_empty(), "{report:?}");
    }

    #[test]
    fn a_load_after_chain_that_does_not_reach_the_changer_does_not_decide() {
        // C loads after B, but nothing puts B after A, so C is not ordered after A.
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "old"),
                ("C.esp", "new"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[after("C.esp", "B.esp")]).unwrap();
        assert_eq!(report.decided, 0, "{report:?}");
        assert_eq!(report.pairs.len(), 1, "{report:?}");
    }

    #[test]
    fn a_rule_deciding_one_pair_of_three_leaves_the_other_two_alone() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
            entry("D.esp", &[]),
        ];
        let reply = overlap_reply(vec![overlap_key(
            "Game:000001",
            "GMST fJumpHeightMin",
            &[
                ("Base.esm", "original"),
                ("A.esp", "first"),
                ("B.esp", "second"),
                ("C.esp", "third"),
                ("D.esp", "fourth"),
            ],
        )]);
        let report = classify_overlaps(&list, &reply, &[after("D.esp", "B.esp")]).unwrap();
        assert_eq!(report.decided, 1);
        assert_eq!(report.pairs.len(), 2);
        assert_eq!(report.pairs[0].loser, "A.esp");
        assert_eq!(report.pairs[1].loser, "C.esp");
    }

    #[test]
    fn an_empty_overlaps_reply_is_an_empty_report_not_a_failure() {
        let list = [entry("A.esp", &[])];
        let report = classify_overlaps(&list, &overlap_reply(Vec::new()), &[]).unwrap();
        assert_eq!(report, OverlapReport::default());
    }

    #[test]
    fn a_malformed_overlaps_reply_is_refused_and_says_why() {
        let list = [entry("A.esp", &[]), entry("B.esp", &[])];
        for (keys, reason) in [
            (
                vec![overlap_key("Game:1", "", &[("A.esp", "x")])],
                "an overlap needs two",
            ),
            (
                vec![overlap_key(
                    "Game:1",
                    "",
                    &[("A.esp", "x"), ("Gone.esp", "y")],
                )],
                "which the list does not load",
            ),
            (
                vec![overlap_key("Game:1", "", &[("A.esp", "x"), ("a.ESP", "y")])],
                "twice",
            ),
            (
                vec![overlap_key("", "", &[("A.esp", "x"), ("B.esp", "y")])],
                "a record key is empty",
            ),
        ] {
            let error = classify_overlaps(&list, &overlap_reply(keys), &[]).unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "unexpected error: {error}"
            );
            assert!(matches!(error, LoadOrderError::OverlapsReply { .. }));
        }
    }

    #[test]
    fn conflicts_group_by_pair_and_list_at_most_ten_labels() {
        let list = [
            entry("Base.esm", &[]),
            entry("A.esp", &[]),
            entry("B.esp", &[]),
        ];
        let keys: Vec<_> = (0..12)
            .map(|index| {
                overlap_key(
                    &format!("Game:{index}"),
                    &format!("Record {index}"),
                    &[("Base.esm", "original"), ("A.esp", "old"), ("B.esp", "new")],
                )
            })
            .collect();
        let report = classify_overlaps(&list, &overlap_reply(keys), &[]).unwrap();
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].count, 12);
        assert_eq!(report.pairs[0].labels.len(), CONFLICT_LABELS);
    }

    fn session_entry(name: &str, active: bool) -> crate::session_ledger::LoadOrderEntry {
        crate::session_ledger::LoadOrderEntry {
            list: "plugins".into(),
            name: name.to_string(),
            active,
        }
    }

    #[test]
    fn a_pair_is_new_when_its_loser_was_added_turned_on_or_moved() {
        let pair = ConflictPair {
            winner: "C.esp".into(),
            loser: "B.esp".into(),
            count: 1,
            labels: vec!["GMST fJumpHeightMin".into()],
            new_since_last_launch: false,
        };
        let current = [
            entry("A.esp", &[]),
            entry("B.esp", &[]),
            entry("C.esp", &[]),
        ];
        let previous = [
            session_entry("A.esp", true),
            session_entry("B.esp", true),
            session_entry("C.esp", true),
        ];
        // The same order and active set: nothing happened since the last launch.
        assert!(!pair_is_new_since(&pair, &current, &previous));

        // B was turned off before the last launch and is on now.
        let off_before = [
            session_entry("A.esp", true),
            session_entry("B.esp", false),
            session_entry("C.esp", true),
        ];
        assert!(pair_is_new_since(&pair, &current, &off_before));

        // B was not in the last launch's list at all.
        let without_b = [session_entry("A.esp", true), session_entry("C.esp", true)];
        assert!(pair_is_new_since(&pair, &current, &without_b));

        // Both were listed, but B sat above A last time.
        let moved = [
            session_entry("B.esp", true),
            session_entry("A.esp", true),
            session_entry("C.esp", true),
        ];
        assert!(pair_is_new_since(&pair, &current, &moved));
    }

    #[test]
    fn no_recorded_launch_is_not_a_baseline_so_nothing_is_new() {
        let pair = ConflictPair {
            winner: "C.esp".into(),
            loser: "B.esp".into(),
            count: 1,
            labels: vec!["GMST fJumpHeightMin".into()],
            new_since_last_launch: false,
        };
        let current = [entry("B.esp", &[]), entry("C.esp", &[])];
        assert!(!pair_is_new_since(&pair, &current, &[]));
    }
}
