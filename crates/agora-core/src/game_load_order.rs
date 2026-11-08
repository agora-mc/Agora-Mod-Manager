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
    LoadOrderDefinition, LoadOrderLine, LoadOrderRule, LoadOrderRulesReply, SorterEntry,
    SorterReply, SorterRequest, SorterStatus,
};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_deploy::{self, DeployMode, DeploymentPlan};
use crate::game_plugins::{self, PluginEntry, PluginListError};
use crate::lock_manager::LockResource;

/// What a sort says when the list has duplicates, so its sorter cannot judge it.
pub const DUPLICATES_BY_MASTERS_ONLY: &str =
    "Sorted by masters only. A plugin is listed twice, so the list's sorter was not asked.";

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
    #[error("the list's sorter failed: {reason}")]
    Sorter { list: String, reason: String },
    #[error("the list's sorter did not answer, though the list declares one")]
    NoSorterReply(String),
    #[error("the list's sorter returned an order that {reason}; nothing was written")]
    SorterOrder { reason: String },
    #[error("the list's sorter returned a note Agora cannot read: {reason}")]
    SorterNote { reason: String },
    #[error("the list's sorter update failed: {reason}")]
    SorterUpdate { reason: String },
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
            | Finding::RulesFailed { .. } => false,
        }
    }

    /// The finding as one sentence for a person.
    pub fn message(&self) -> String {
        match self {
            Finding::MasterNotEarlier {
                plugin,
                master,
                problem,
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

/// A plugin that loads above a master the sort may not move, so the sort could not fix that
/// master finding. The plugin would have to move, and it is locked or always loaded.
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
    rule.rules.kind == "script" || rule.rules.sorter.is_some() || rule.items.folders
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

/// What a package's sorter hooks receive: the list as it stands, plus the rule's free options.
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
    let context = hook_context(ctx, instance_id, definition, rule, entries)?;
    Ok(SorterRequest {
        context,
        list_id: rule.id.clone(),
        options: sorter.options.clone(),
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
    for note in reply.notes.iter().chain(reply.general.iter()) {
        let level = match note.level.as_str() {
            "error" => NoteLevel::Error,
            "warn" => NoteLevel::Warn,
            "info" => NoteLevel::Info,
            other => {
                return Err(LoadOrderError::SorterNote {
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
        LoadOrderError::SorterUpdate {
            reason: error.message,
        }
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

/// [`check`] for one list; `None` names the game's first list.
pub fn check_in(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Vec<Finding>, LoadOrderError> {
    Ok(order_of_with_view(ctx, instance_id, definition, list_id, None, true)?.findings)
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

/// [`check_all`], reading `links` instead of planning again.
pub(crate) fn check_all_with_view(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    links: Option<&DeploymentPlan>,
) -> Result<Vec<Finding>, LoadOrderError> {
    definition
        .load_orders
        .iter()
        .map(|rule| {
            order_of_with_view(ctx, instance_id, definition, Some(&rule.id), links, true)
                .map(|order| order.findings)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|all| all.into_iter().flatten().collect())
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

    let (moves, blocked) = if rule.rules.sorter.is_some() && !duplicated {
        let reply = call_sorter(ctx, instance_id, definition, rule, &entries)?;
        notes = sorter_notes(&reply)?;
        let moves = apply_sorter_order(&mut entries, &reply.order)?;
        let fixed = Fixed::of(&entries);
        let blocked = blocked_of(&entries, &fixed);
        mode = SortMode::Sorter;
        (moves, blocked)
    } else {
        if rule.rules.sorter.is_some() {
            message = Some(DUPLICATES_BY_MASTERS_ONLY.to_string());
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
    Ok(move_inner(ctx, instance_id, definition, None, plugin, target, false)?.0)
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
    Ok(move_inner(ctx, instance_id, definition, list_id, plugin, target, false)?.0)
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
    let (report, order) = move_inner(ctx, instance_id, definition, None, plugin, target, true)?;
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
    let (report, order) = move_inner(ctx, instance_id, definition, list_id, plugin, target, true)?;
    Ok((report, order.expect("the order was asked for")))
}

fn move_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
    plugin: &str,
    target: &MoveTarget,
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
            }]
        );
        let missing = [entry("A.esp", &[])];
        assert_eq!(
            script_findings(&missing, &[requires("A.esp", "Gone.esm")], "plugins"),
            [Finding::MasterNotEarlier {
                plugin: "A.esp".into(),
                master: "Gone.esm".into(),
                problem: MasterProblem::Missing,
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
}
