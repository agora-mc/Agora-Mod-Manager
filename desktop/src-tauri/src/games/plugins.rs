//! Plugins: an instance's plugin list, its load order, locks and checks (MASTER_SPEC §26.6). Owned
//! by the `plugins` slice. The INI and saves commands (§26.7) are in `ini_saves.rs`.
//!
//! Each command is an adapter over `agora_core::game_load_order` and `game_plugins`: the same
//! functions `agora games instance plugins` calls, so the desktop and the CLI cannot disagree about
//! a plugin's place.
//!
//! A core error reaches the user as its message, which core writes for people already.

use agora_core::ctx::Ctx;
use agora_core::game_load_order::{
    self as load_order, Finding, LoadOrder, LoadOrderEntry, MoveReport, MoveTarget, SortReport,
};
use agora_core::game_plugins;
use agora_game_api::GameDefinition;
use serde::Serialize;

use super::error::{GamesError, CORE_FAILED, TASK_FAILED};

pub(super) fn core_failed(error: impl std::fmt::Display) -> GamesError {
    GamesError::new(CORE_FAILED, error.to_string())
}

/// Binds the instance's game definition to `$def`, or returns the reason there is none. A macro
/// because the definition's type lives in `agora_game_api`, which this crate does not depend on:
/// each command binds it here and passes the binding on to core. `ini_saves` uses it too, so every
/// path here is absolute: a macro's items resolve where it is used, not where it is written.
macro_rules! game_definition {
    ($ctx:expr, $instance_id:expr, $def:ident) => {
        let record = agora_core::game_instance::get($ctx, $instance_id)
            .map_err(crate::games::plugins::core_failed)?
            .ok_or_else(|| {
                crate::games::error::GamesError::new(
                    crate::games::error::CORE_FAILED,
                    format!("Instance '{}' was not found.", $instance_id),
                )
            })?;
        let $def = $ctx.games.game(&record.game).ok_or_else(|| {
            crate::games::error::GamesError::new(
                crate::games::error::CORE_FAILED,
                format!(
                    "Agora does not know the game for instance '{}'.",
                    $instance_id
                ),
            )
        })?;
    };
}
pub(super) use game_definition;

/// Runs `work` on a blocking thread with the core context. Every command here reads or writes
/// files, so none of them runs on the async thread.
pub(super) async fn run_blocking<T, F>(
    app: tauri::AppHandle,
    what: &'static str,
    work: F,
) -> Result<T, GamesError>
where
    T: Send + 'static,
    F: FnOnce(Ctx) -> Result<T, GamesError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || -> Result<T, GamesError> {
        let ctx = crate::core_context(&app)?;
        work(ctx)
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, format!("{what} did not finish.")))?
}

// ---------------------------------------------------------------------------
// Plugin list and load order
// ---------------------------------------------------------------------------

/// An instance's plugins in load order, with what the game's rules say about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsView {
    /// Whether the game keeps a plugin list at all. When false the tab says so, and there is
    /// nothing for Agora to order.
    pub supported: bool,
    /// The list's id within the game.
    pub list_id: String,
    /// The list's name as a person reads it.
    pub list_name: String,
    /// The list's rule kind: `none` or `script`.
    pub rules_kind: String,
    /// This list's intro sentence, when the game declares one.
    pub intro: Option<String>,
    /// The column heading for an item's name, when the game declares one.
    pub item_heading: Option<String>,
    /// Whether this list's sorter has an update hook, so the tab shows an update button.
    pub has_sorter_update: bool,
    /// The update button's label, from the list's sorter.
    pub sorter_update_label: Option<String>,
    /// Whether the game's own rules judge this list, so it can be sorted.
    pub rules_apply: bool,
    /// Whether the instance has a copy of the plugin list yet. It is created at deploy time.
    pub list_exists: bool,
    /// Always-loaded plugins first, then the listed ones, in the order the game reads them.
    pub rows: Vec<PluginRow>,
    pub findings: Vec<PluginFinding>,
    /// The list's sorter's notes, shown in the same problems list.
    pub notes: Vec<PluginFinding>,
    /// True when the saved notes no longer match the list; the tab fetches fresh ones.
    pub notes_out_of_date: bool,
    /// What the list's sorter last saved, when it has saved something.
    pub masterlist: Option<MasterlistView>,
    /// Whether this list declares an overlaps reader, so the tab can show conflicts at all.
    pub has_overlaps_reader: bool,
    /// The saved conflicts report for this exact order, when one was computed. A plain read never
    /// runs the reader, so this can lag the list by one check.
    pub overlaps: Option<OverlapsView>,
    /// True when the list declares an overlaps reader but no report matches this order; the tab
    /// asks for one in the background.
    pub overlaps_out_of_date: bool,
}

/// What the list's sorter last saved, for the tab's "Masterlist from <date>" line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MasterlistView {
    /// RFC 3339, as recorded when the copy was fetched.
    pub fetched_at: String,
    /// The contents API URL the copy came from.
    pub source: String,
}

impl MasterlistView {
    fn from_status(status: &agora_game_api::SorterStatus) -> Self {
        Self {
            fetched_at: status.fetched_at.clone(),
            source: status.source.clone(),
        }
    }
}

/// One plugin, as the load order shows it. `position` is 1-based and counts always-loaded plugins:
/// the numbering `agora games instance plugins` prints and `move --to` takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRow {
    pub position: usize,
    pub name: String,
    /// Always loaded by the game's rule. Not in the instance's list, so it cannot be moved or locked.
    pub implicit: bool,
    pub active: bool,
    /// Whether the plugin's file is installed where the game looks for it.
    pub present: bool,
    /// Whether Agora added this line because a content layer deploys it.
    pub managed: bool,
    pub locked: bool,
    /// Short flags the game's rules show beside the item, such as `Master` or `Light`.
    pub badges: Vec<String>,
    /// The items the game's rules say it needs first.
    pub masters: Vec<String>,
}

/// One problem with the load order, with the plugins it concerns so the row can show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginFinding {
    pub message: String,
    /// Whether a launch refuses to start for this finding. The others are warnings.
    pub refuses_launch: bool,
    /// `finding` when a launch refuses, `warning` for something to read, `info` for a note that
    /// changes nothing. The tab titles the whole card from this.
    pub level: String,
    /// The plugins the finding is about. Empty for a limit that applies to the whole list.
    pub plugins: Vec<String>,
}

/// One pair of plugins where the winner replaces changes the loser made (MASTER_SPEC §31.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictPairView {
    pub winner: String,
    pub loser: String,
    /// How many record changes the winner replaces.
    pub count: usize,
    /// Up to ten record labels that explain the pair.
    pub labels: Vec<String>,
    /// Whether one of the pair's items was added, turned on or moved since the last launch.
    pub new_since_last_launch: bool,
}

/// One list's classified overlaps, as the conflicts card shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlapsView {
    pub pairs: Vec<ConflictPairView>,
    /// Changer pairs the winner patches because it masters the changer.
    pub patched: usize,
    /// Changer pairs a load-after rule already decides.
    pub decided: usize,
    /// Changer pairs between the game's own always-loaded files.
    pub implicit_pairs: usize,
    /// What the reader said about files it could not read, or about the list itself.
    pub notes: Vec<PluginFinding>,
}

fn overlaps_view(report: &agora_core::game_load_order::OverlapReport) -> OverlapsView {
    OverlapsView {
        pairs: report
            .pairs
            .iter()
            .map(|pair| ConflictPairView {
                winner: pair.winner.clone(),
                loser: pair.loser.clone(),
                count: pair.count,
                labels: pair.labels.clone(),
                new_since_last_launch: pair.new_since_last_launch,
            })
            .collect(),
        patched: report.patched,
        decided: report.decided,
        implicit_pairs: report.implicit_pairs,
        notes: report.notes.iter().map(note_view).collect(),
    }
}

/// One load-after rule as the rules section shows it, naming the item that left the list when the
/// rule is unused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleView {
    pub item: String,
    pub after: String,
    /// The item or its target that the list no longer has. `None` while the rule is in use.
    pub unused: Option<String>,
}

fn rule_view(rule: &agora_core::game_load_order::LoadAfterRuleView) -> RuleView {
    RuleView {
        item: rule.item.clone(),
        after: rule.after.clone(),
        unused: rule.unused.clone(),
    }
}

impl PluginsView {
    fn unsupported() -> Self {
        Self {
            supported: false,
            list_id: String::new(),
            list_name: String::new(),
            rules_kind: String::new(),
            intro: None,
            item_heading: None,
            has_sorter_update: false,
            sorter_update_label: None,
            rules_apply: false,
            list_exists: false,
            rows: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            notes_out_of_date: false,
            masterlist: None,
            has_overlaps_reader: false,
            overlaps: None,
            overlaps_out_of_date: false,
        }
    }

    fn from_order(order: &LoadOrder) -> Self {
        Self {
            supported: true,
            list_id: order.list_id.clone(),
            list_name: order.list_name.clone(),
            rules_kind: order.rules_kind.clone(),
            intro: order.intro.clone(),
            item_heading: order.item_heading.clone(),
            has_sorter_update: false,
            sorter_update_label: None,
            rules_apply: order.rules_kind == "script",
            list_exists: order.exists,
            rows: order
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| row_view(index + 1, entry))
                .collect(),
            findings: order.findings.iter().map(finding_view).collect(),
            notes: order.notes.iter().map(note_view).collect(),
            notes_out_of_date: order.notes_out_of_date,
            masterlist: None,
            has_overlaps_reader: false,
            overlaps: order.overlaps.as_ref().map(overlaps_view),
            overlaps_out_of_date: order.overlaps_out_of_date,
        }
    }
}

/// Fill the parts of the view that come from the list's sorter, not from the order itself. Every
/// reply that carries a list does this, so the update button and status line survive a change.
fn fill_sorter(
    view: &mut PluginsView,
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<(), GamesError> {
    let rule = load_order::list_by_id(definition, list_id).map_err(core_failed)?;
    view.has_overlaps_reader = rule.rules.overlaps.is_some();
    if let Some(sorter) = rule.rules.sorter.as_ref() {
        view.has_sorter_update = sorter.update.is_some();
        view.sorter_update_label = sorter.update_label.clone();
    }
    // The status line is informational; a sorter that cannot report it must not turn a saved
    // change or a readable list into a failure. The hook's failure is already in the plugin log.
    if let Ok(status) = load_order::sorter_status(ctx, instance_id, definition, Some(&rule.id)) {
        view.masterlist = status.map(|status| MasterlistView::from_status(&status));
    }
    Ok(())
}

fn row_view(position: usize, entry: &LoadOrderEntry) -> PluginRow {
    PluginRow {
        position,
        name: entry.name.clone(),
        implicit: entry.implicit,
        active: entry.active,
        present: entry.present,
        managed: entry.managed,
        locked: entry.locked,
        badges: entry.badges.clone(),
        masters: entry.masters.clone(),
    }
}

fn note_view(note: &agora_core::game_load_order::PluginNote) -> PluginFinding {
    PluginFinding {
        message: note.message.clone(),
        refuses_launch: false,
        level: match note.level {
            agora_core::game_load_order::NoteLevel::Info => "info",
            agora_core::game_load_order::NoteLevel::Warn
            | agora_core::game_load_order::NoteLevel::Error => "warning",
        }
        .to_string(),
        plugins: if note.plugin.is_empty() {
            Vec::new()
        } else {
            vec![note.plugin.clone()]
        },
    }
}

fn finding_view(finding: &Finding) -> PluginFinding {
    let plugins = match finding {
        Finding::MasterNotEarlier { plugin, .. } | Finding::DuplicateListing { plugin } => {
            vec![plugin.clone()]
        }
        Finding::MasterCycle { plugins } => plugins.clone(),
        Finding::RuleConflict { first, second, .. } => vec![first.clone(), second.clone()],
        Finding::MissingRuleItem { item, .. } => vec![item.clone()],
        Finding::ListLevel { items, .. } => items.clone(),
        Finding::SorterMoves { moves, .. } => moves
            .iter()
            .map(|movement| movement.plugin.clone())
            .collect(),
        Finding::RulesFailed { .. } => Vec::new(),
    };
    let refuses = finding.refuses_launch();
    let level = if refuses {
        "finding"
    } else if matches!(finding, Finding::ListLevel { message, .. } if message.starts_with("Info: "))
    {
        "info"
    } else {
        "warning"
    };
    PluginFinding {
        message: finding.message(),
        refuses_launch: refuses,
        level: level.to_string(),
        plugins,
    }
}

/// A fresh check. `status` is `findings` when a launch would refuse, `warnings` when there is
/// something to read but nothing that stops the game, and `ok` when there is nothing at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckView {
    pub status: String,
    pub findings: Vec<PluginFinding>,
}

fn check_status(findings: &[PluginFinding]) -> &'static str {
    if findings.iter().any(|f| f.refuses_launch) {
        "findings"
    } else if findings.is_empty() {
        "ok"
    } else {
        "warnings"
    }
}

/// A change to one plugin's state, with the list as it is after the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginChange {
    /// Whether the line or the lock actually changed. False when it already had that state.
    pub changed: bool,
    /// The list as it is after the change. `None` only when the change was saved but the re-read
    /// failed; `reload_error` then says why, so a saved change is not reported as a failure.
    pub plugins: Option<PluginsView>,
    /// Why the list could not be read back after a change that was saved.
    pub reload_error: Option<String>,
}

/// The reply after a change that core has already saved, with whatever reading the list back gave.
/// A failed re-read keeps the reply a success over the change, and names why the list is missing.
fn plugin_change_after_save<E: std::fmt::Display>(
    changed: bool,
    order: Result<LoadOrder, E>,
) -> Result<PluginChange, GamesError> {
    match order {
        Ok(order) => Ok(PluginChange {
            changed,
            plugins: Some(PluginsView::from_order(&order)),
            reload_error: None,
        }),
        Err(error) => Ok(PluginChange {
            changed,
            plugins: None,
            reload_error: Some(error.to_string()),
        }),
    }
}

/// A move, with the list as it is after the move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveChange {
    pub report: MoveReport,
    pub plugins: PluginsView,
}

/// A sort, with the list as it is after the sort. A dry run changes nothing, and the report says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SortChange {
    pub report: SortReport,
    pub plugins: PluginsView,
}

/// One load order list a game declares, for the tab row above the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadOrderListRow {
    pub id: String,
    pub name: String,
    /// `none`, `script` or `creation_engine`.
    pub rules_kind: String,
}

/// Every load order list the game declares, in the order it declares them. Empty when the game
/// keeps no list, which is not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadOrderListsView {
    pub lists: Vec<LoadOrderListRow>,
}

#[tauri::command]
pub async fn games_plugins_lists(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<LoadOrderListsView, GamesError> {
    run_blocking(app, "Reading the load order lists", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        Ok(LoadOrderListsView {
            lists: definition
                .load_orders
                .iter()
                .map(|rule| LoadOrderListRow {
                    id: rule.id.clone(),
                    name: rule.name.clone(),
                    rules_kind: rule.rules.kind.clone(),
                })
                .collect(),
        })
    })
    .await
}

/// The instance's plugins in load order, with the findings the game's rules give.
#[tauri::command]
pub async fn games_plugins_list(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<PluginsView, GamesError> {
    run_blocking(app, "Reading the plugin list", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        if definition.load_orders.is_empty() {
            return Ok(PluginsView::unsupported());
        }
        let order = load_order::order_in(&ctx, &instance_id, definition, list_id.as_deref())
            .map_err(core_failed)?;
        let mut view = PluginsView::from_order(&order);
        fill_sorter(
            &mut view,
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
        )?;
        Ok(view)
    })
    .await
}

/// Run the list's sorter update hook, replacing whatever it keeps.
#[tauri::command]
pub async fn games_plugins_update_masterlist(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<MasterlistView, GamesError> {
    run_blocking(app, "Updating LOOT's data", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        match load_order::update_sorter(&ctx, &instance_id, definition, list_id.as_deref())
            .map_err(core_failed)?
        {
            Some(status) => Ok(MasterlistView::from_status(&status)),
            None => Err(GamesError::new(
                "NO_SORTER_UPDATE",
                "This list has no LOOT update to run.",
            )),
        }
    })
    .await
}

/// The sorter's notes for one list, fetched in the background after a read said they were stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SorterNotesView {
    pub notes: Vec<PluginFinding>,
    /// Always false: this reply just refreshed them.
    pub notes_out_of_date: bool,
}

/// Ask the list's sorter for its notes now, and remember them for the next read.
#[tauri::command]
pub async fn games_plugins_sorter_notes(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<SorterNotesView, GamesError> {
    run_blocking(app, "Reading LOOT's notes", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let notes =
            load_order::refresh_sorter_notes(&ctx, &instance_id, definition, list_id.as_deref())
                .map_err(core_failed)?;
        Ok(SorterNotesView {
            notes: notes.iter().map(note_view).collect(),
            notes_out_of_date: false,
        })
    })
    .await
}

/// Read one list's conflicts now, filling the cache a later plain read uses. A reader that fails
/// or answers something core cannot trust is an error, never an empty "no conflicts".
#[tauri::command]
pub async fn games_plugins_conflicts(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<OverlapsView, GamesError> {
    run_blocking(app, "Reading the plugins' records", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let report =
            load_order::refresh_overlaps(&ctx, &instance_id, definition, list_id.as_deref())
                .map_err(core_failed)?;
        Ok(overlaps_view(&report))
    })
    .await
}

/// Every load-after rule this instance's list has, with unused rules naming the missing item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RulesView {
    pub rules: Vec<RuleView>,
}

fn rule_views(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> Result<Vec<RuleView>, GamesError> {
    let views = load_order::load_after_rule_views(ctx, instance_id, definition, list_id)
        .map_err(core_failed)?;
    Ok(views.iter().map(rule_view).collect())
}

/// The instance's load order rules, for the collapsed rules section.
#[tauri::command]
pub async fn games_plugins_rules(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<RulesView, GamesError> {
    run_blocking(app, "Reading your load order rules", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        Ok(RulesView {
            rules: rule_views(&ctx, &instance_id, definition, list_id.as_deref())?,
        })
    })
    .await
}

/// A saved rule change: the rules as they now stand, and conflicts read again when the list has
/// a reader. A reader that fails does not turn a saved change into a failure; `overlaps_error`
/// says why the card cannot be updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleChange {
    /// For an add, whether the rule was new; for a remove, whether one was there.
    pub changed: bool,
    pub rules: Vec<RuleView>,
    pub overlaps: Option<OverlapsView>,
    pub overlaps_error: Option<String>,
}

fn refreshed_overlaps(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    list_id: Option<&str>,
) -> (Option<OverlapsView>, Option<String>) {
    let reader = load_order::list_by_id(definition, list_id)
        .map(|rule| rule.rules.overlaps.is_some())
        .unwrap_or(false);
    if !reader {
        return (None, None);
    }
    match load_order::refresh_overlaps(ctx, instance_id, definition, list_id) {
        Ok(report) => (Some(overlaps_view(&report)), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// Add "item loads after after". A rule that contradicts a master or would make a loop is refused
/// by core, with the reason.
#[tauri::command]
pub async fn games_plugins_rule_add(
    app: tauri::AppHandle,
    instance_id: String,
    item: String,
    after: String,
    list_id: Option<String>,
) -> Result<RuleChange, GamesError> {
    run_blocking(app, "Adding the rule", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let changed = load_order::add_load_after_rule(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            &item,
            &after,
        )
        .map_err(core_failed)?;
        let rules = rule_views(&ctx, &instance_id, definition, list_id.as_deref())?;
        let (overlaps, overlaps_error) =
            refreshed_overlaps(&ctx, &instance_id, definition, list_id.as_deref());
        Ok(RuleChange {
            changed,
            rules,
            overlaps,
            overlaps_error,
        })
    })
    .await
}

/// Remove "item loads after after". A rule whose item left the list can still be removed.
#[tauri::command]
pub async fn games_plugins_rule_remove(
    app: tauri::AppHandle,
    instance_id: String,
    item: String,
    after: String,
    list_id: Option<String>,
) -> Result<RuleChange, GamesError> {
    run_blocking(app, "Removing the rule", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let removed = load_order::remove_load_after_rule(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            &item,
            &after,
        )
        .map_err(core_failed)?;
        if !removed {
            return Err(GamesError::new(
                "NO_SUCH_RULE",
                format!("No rule says '{item}' loads after '{after}'."),
            ));
        }
        let rules = rule_views(&ctx, &instance_id, definition, list_id.as_deref())?;
        let (overlaps, overlaps_error) =
            refreshed_overlaps(&ctx, &instance_id, definition, list_id.as_deref());
        Ok(RuleChange {
            changed: true,
            rules,
            overlaps,
            overlaps_error,
        })
    })
    .await
}

/// What one of the conflicts card's two actions left behind: the new order, the rules, and the
/// conflicts read again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictActionChange {
    pub rules: Vec<RuleView>,
    pub plugins: PluginsView,
    pub overlaps: Option<OverlapsView>,
    pub overlaps_error: Option<String>,
}

/// Let the loser win (add the rule and move it after the winner), or keep this order (add the
/// rule only). The pair must still be in the current report; a reply that names a stale pair is
/// refused rather than writing a rule nobody asked for.
#[tauri::command]
pub async fn games_plugins_conflict_action(
    app: tauri::AppHandle,
    instance_id: String,
    winner: String,
    loser: String,
    let_loser_win: bool,
    list_id: Option<String>,
) -> Result<ConflictActionChange, GamesError> {
    run_blocking(app, "Changing the plugin conflict", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let report =
            load_order::refresh_overlaps(&ctx, &instance_id, definition, list_id.as_deref())
                .map_err(core_failed)?;
        let exists = report.pairs.iter().any(|pair| {
            pair.winner.eq_ignore_ascii_case(&winner) && pair.loser.eq_ignore_ascii_case(&loser)
        });
        if !exists {
            return Err(GamesError::new(
                "NO_SUCH_CONFLICT",
                format!(
                    "There is no conflict where '{winner}' replaces changes made by '{loser}'."
                ),
            ));
        }
        let (item, after) = if let_loser_win {
            (loser.as_str(), winner.as_str())
        } else {
            (winner.as_str(), loser.as_str())
        };
        let added = load_order::add_load_after_rule(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            item,
            after,
        )
        .map_err(core_failed)?;
        if let_loser_win {
            if let Err(error) = load_order::move_plugin_in(
                &ctx,
                &instance_id,
                definition,
                list_id.as_deref(),
                &loser,
                &MoveTarget::After(winner.clone()),
            ) {
                // A move that was refused leaves the order as it was; do not keep a rule that
                // only made sense with the move.
                if added {
                    let _ = load_order::remove_load_after_rule(
                        &ctx,
                        &instance_id,
                        definition,
                        list_id.as_deref(),
                        item,
                        after,
                    );
                }
                return Err(GamesError::new(
                    CORE_FAILED,
                    format!("The rule was not kept, because the move failed: {error}"),
                ));
            }
        }
        let order = load_order::order_in(&ctx, &instance_id, definition, list_id.as_deref())
            .map_err(core_failed)?;
        let mut view = PluginsView::from_order(&order);
        fill_sorter(
            &mut view,
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
        )?;
        let rules = rule_views(&ctx, &instance_id, definition, list_id.as_deref())?;
        let (overlaps, overlaps_error) =
            refreshed_overlaps(&ctx, &instance_id, definition, list_id.as_deref());
        Ok(ConflictActionChange {
            rules,
            plugins: view,
            overlaps,
            overlaps_error,
        })
    })
    .await
}

/// Turn a plugin on or off in the instance's list. Refused while the game is running.
#[tauri::command]
pub async fn games_plugins_set_active(
    app: tauri::AppHandle,
    instance_id: String,
    plugin: String,
    active: bool,
    list_id: Option<String>,
    while_running: Option<bool>,
) -> Result<PluginChange, GamesError> {
    let while_running = while_running.unwrap_or(false);
    run_blocking(app, "Changing the plugin", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let changed = game_plugins::set_active_in_while_running(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            &plugin,
            active,
            while_running,
        )
        .map_err(core_failed)?;
        // The change is saved: a failed re-read must not turn it into a reported failure.
        let mut change = plugin_change_after_save(
            changed,
            load_order::order_in(&ctx, &instance_id, definition, list_id.as_deref()),
        )?;
        if let Some(view) = change.plugins.as_mut() {
            fill_sorter(view, &ctx, &instance_id, definition, list_id.as_deref())?;
        }
        Ok(change)
    })
    .await
}

/// Lock or unlock a plugin's place, so sort and move leave it where it is.
#[tauri::command]
pub async fn games_plugins_set_locked(
    app: tauri::AppHandle,
    instance_id: String,
    plugin: String,
    locked: bool,
    list_id: Option<String>,
    while_running: Option<bool>,
) -> Result<PluginChange, GamesError> {
    let while_running = while_running.unwrap_or(false);
    run_blocking(app, "Changing the lock", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let changed = game_plugins::set_locked_in_while_running(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            &plugin,
            locked,
            while_running,
        )
        .map_err(core_failed)?;
        // The change is saved: a failed re-read must not turn it into a reported failure.
        let mut change = plugin_change_after_save(
            changed,
            load_order::order_in(&ctx, &instance_id, definition, list_id.as_deref()),
        )?;
        if let Some(view) = change.plugins.as_mut() {
            fill_sorter(view, &ctx, &instance_id, definition, list_id.as_deref())?;
        }
        Ok(change)
    })
    .await
}

/// Move a plugin to a 1-based position that counts always-loaded plugins, as `move --to` does.
#[tauri::command]
pub async fn games_plugins_move(
    app: tauri::AppHandle,
    instance_id: String,
    plugin: String,
    to: usize,
    break_rule: Option<bool>,
    list_id: Option<String>,
) -> Result<MoveChange, GamesError> {
    run_blocking(app, "Moving the plugin", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        let (report, order) = if break_rule.unwrap_or(false) {
            let report = load_order::move_plugin_in_breaking_rules(
                &ctx,
                &instance_id,
                definition,
                list_id.as_deref(),
                &plugin,
                &MoveTarget::Position(to),
            )
            .map_err(move_failed)?;
            let order = load_order::order_in(&ctx, &instance_id, definition, list_id.as_deref())
                .map_err(core_failed)?;
            (report, order)
        } else {
            // One deployment plan serves the move and the order it returns.
            load_order::move_plugin_and_order_in(
                &ctx,
                &instance_id,
                definition,
                list_id.as_deref(),
                &plugin,
                &MoveTarget::Position(to),
            )
            .map_err(move_failed)?
        };
        let mut view = PluginsView::from_order(&order);
        fill_sorter(
            &mut view,
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
        )?;
        Ok(MoveChange {
            report,
            plugins: view,
        })
    })
    .await
}

/// A move that breaks a rule is a question, not a failure: the details carry the rule so the tab
/// can ask before changing anything.
fn move_failed(error: agora_core::game_load_order::LoadOrderError) -> GamesError {
    if let agora_core::game_load_order::LoadOrderError::BreaksRule { item, after, .. } = &error {
        return GamesError::new("BREAKS_RULE", error.to_string()).with_details(serde_json::json!({
            "item": item,
            "after": after,
        }));
    }
    core_failed(error)
}

/// Sort the list so every master comes before the plugins that need it, by the game's master
/// rules. `dry_run` reports the moves and writes nothing, so the user can read them first.
#[tauri::command]
pub async fn games_plugins_sort(
    app: tauri::AppHandle,
    instance_id: String,
    dry_run: bool,
    list_id: Option<String>,
) -> Result<SortChange, GamesError> {
    run_blocking(app, "Sorting the plugins", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        // One deployment plan serves the sort and the order it returns.
        let (report, order) = load_order::sort_and_order_in(
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
            dry_run,
        )
        .map_err(core_failed)?;
        let mut view = PluginsView::from_order(&order);
        fill_sorter(
            &mut view,
            &ctx,
            &instance_id,
            definition,
            list_id.as_deref(),
        )?;
        Ok(SortChange {
            report,
            plugins: view,
        })
    })
    .await
}

/// Check the load order again and report its findings. Read only.
#[tauri::command]
pub async fn games_plugins_check(
    app: tauri::AppHandle,
    instance_id: String,
    list_id: Option<String>,
) -> Result<CheckView, GamesError> {
    run_blocking(app, "Checking the plugins", move |ctx| {
        game_definition!(&ctx, &instance_id, definition);
        if definition.load_orders.is_empty() {
            return Ok(CheckView {
                status: "ok".to_string(),
                findings: Vec::new(),
            });
        }
        let findings: Vec<PluginFinding> =
            load_order::check_in(&ctx, &instance_id, definition, list_id.as_deref())
                .map_err(core_failed)?
                .iter()
                .map(finding_view)
                .collect();
        Ok(CheckView {
            status: check_status(&findings).to_string(),
            findings,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::game_load_order::MasterProblem;
    use agora_core::game_plugins::PluginListError;

    fn entry(name: &str) -> LoadOrderEntry {
        LoadOrderEntry {
            name: name.to_string(),
            implicit: false,
            active: true,
            present: true,
            managed: false,
            locked: false,
            badges: Vec::new(),
            masters: Vec::new(),
        }
    }

    fn order(entries: Vec<LoadOrderEntry>, findings: Vec<Finding>) -> LoadOrder {
        LoadOrder {
            list_id: "plugins".into(),
            list_name: "Plugins".into(),
            rules_kind: "script".into(),
            intro: None,
            item_heading: None,
            exists: true,
            entries,
            findings,
            notes: Vec::new(),
            notes_out_of_date: false,
            overlaps: None,
            overlaps_out_of_date: false,
        }
    }

    #[test]
    fn rows_are_numbered_from_one_and_keep_the_order_given() {
        let mut skyrim = entry("Skyrim.esm");
        skyrim.badges = vec!["Master".into()];
        let mut implicit = entry("Update.esm");
        implicit.implicit = true;
        let view = PluginsView::from_order(&order(
            vec![implicit, skyrim, entry("SkyUI.esp")],
            Vec::new(),
        ));

        assert!(view.supported && view.rules_apply && view.list_exists);
        assert_eq!(view.list_id, "plugins");
        assert_eq!(view.list_name, "Plugins");
        assert_eq!(view.rules_kind, "script");
        let numbered: Vec<(usize, &str)> = view
            .rows
            .iter()
            .map(|r| (r.position, r.name.as_str()))
            .collect();
        assert_eq!(
            numbered,
            vec![(1, "Update.esm"), (2, "Skyrim.esm"), (3, "SkyUI.esp")]
        );
        assert!(view.rows[0].implicit);
        assert!(view.rows[1].badges.iter().any(|b| b == "Master"));
    }

    #[test]
    fn a_late_master_warns_and_names_the_plugin_that_needs_it() {
        let finding = Finding::MasterNotEarlier {
            plugin: "SkyUI.esp".to_string(),
            master: "Skyrim.esm".to_string(),
            problem: MasterProblem::Later,
            movable: true,
        };
        let view = PluginsView::from_order(&order(vec![entry("SkyUI.esp")], vec![finding]));

        assert_eq!(view.findings.len(), 1);
        assert!(!view.findings[0].refuses_launch);
        assert_eq!(view.findings[0].plugins, vec!["SkyUI.esp".to_string()]);
        assert!(view.findings[0]
            .message
            .contains("loads before its master 'Skyrim.esm'"));
        assert_eq!(check_status(&view.findings), "warnings");
    }

    #[test]
    fn a_missing_master_refuses_and_a_limit_names_no_plugin() {
        let missing = Finding::MasterNotEarlier {
            plugin: "SkyUI.esp".to_string(),
            master: "Skyrim.esm".to_string(),
            problem: MasterProblem::Missing,
            movable: false,
        };
        let limit = Finding::ListLevel {
            message: "300 full plugins are active; the game loads at most 254".into(),
            items: Vec::new(),
            refuses: true,
        };
        let view = PluginsView::from_order(&order(Vec::new(), vec![missing, limit]));

        assert!(view.findings[0].refuses_launch);
        assert!(view.findings[1].refuses_launch);
        assert!(view.findings[1].plugins.is_empty());
        assert_eq!(check_status(&view.findings), "findings");
    }

    #[test]
    fn a_cycle_names_every_plugin_in_it() {
        let cycle = Finding::MasterCycle {
            plugins: vec!["A.esp".to_string(), "B.esp".to_string()],
        };
        let finding = finding_view(&cycle);
        assert_eq!(
            finding.plugins,
            vec!["A.esp".to_string(), "B.esp".to_string()]
        );
        assert!(finding.refuses_launch);
    }

    #[test]
    fn a_game_without_a_plugin_list_is_unsupported_not_an_empty_success() {
        let view = PluginsView::unsupported();
        assert!(!view.supported);
        assert!(view.rows.is_empty());
        assert!(view.findings.is_empty());
    }

    #[test]
    fn an_empty_check_is_ok() {
        assert_eq!(check_status(&[]), "ok");
    }

    #[test]
    fn a_missing_list_reads_as_no_list_yet_not_as_an_error() {
        let mut no_list = order(Vec::new(), Vec::new());
        no_list.exists = false;
        let view = PluginsView::from_order(&no_list);
        assert!(view.supported);
        assert!(!view.list_exists);
    }

    #[test]
    fn a_saved_change_whose_re_read_fails_is_reported_as_saved() {
        let failure = agora_core::game_load_order::LoadOrderError::NoRules("skyrim-se".into());
        let change = plugin_change_after_save(true, Err(failure))
            .expect("a saved change is not a failed change");

        assert!(change.changed);
        assert!(change.plugins.is_none());
        assert!(
            change
                .reload_error
                .as_deref()
                .unwrap_or_default()
                .contains("declares no load order rules"),
            "{:?}",
            change.reload_error
        );
        let json = serde_json::to_value(&change).unwrap();
        assert!(json["plugins"].is_null());
        assert!(json["reloadError"].as_str().is_some());
    }

    #[test]
    fn a_saved_change_with_a_readable_order_reports_the_new_list() {
        let change = plugin_change_after_save(
            true,
            Ok::<_, agora_core::game_load_order::LoadOrderError>(order(
                vec![entry("SkyUI.esp")],
                Vec::new(),
            )),
        )
        .expect("a saved change with a readable order is a success");

        assert!(change.changed);
        assert_eq!(
            change.plugins.as_ref().map(|plugins| plugins.rows.len()),
            Some(1)
        );
        assert!(change.reload_error.is_none());
    }

    #[test]
    fn a_core_error_reaches_the_user_as_its_message() {
        let error = core_failed(PluginListError::NotInList("SkyUI.esp".to_string()));
        assert_eq!(error.code, CORE_FAILED);
        assert!(error
            .message
            .contains("'SkyUI.esp' is not in the instance's plugin list"));
        assert_eq!(error.core_code, None);
    }

    #[test]
    fn a_running_game_is_refused_with_the_reason_core_gives() {
        let error = core_failed(PluginListError::InstanceRunning("abc".to_string()));
        assert!(error.message.contains("close the game"));
    }

    #[test]
    fn a_conflict_pair_keeps_its_new_flag_and_labels_for_the_card() {
        let report = agora_core::game_load_order::OverlapReport {
            pairs: vec![agora_core::game_load_order::ConflictPair {
                winner: "Heavy Boots.esp".into(),
                loser: "Moon Jump.esp".into(),
                count: 1,
                labels: vec!["GMST fJumpHeightMin".into()],
                new_since_last_launch: true,
            }],
            patched: 2,
            decided: 1,
            decided_pairs: Vec::new(),
            implicit_pairs: 3,
            notes: Vec::new(),
        };
        let view = overlaps_view(&report);
        assert_eq!(view.pairs.len(), 1);
        assert!(view.pairs[0].new_since_last_launch);
        assert_eq!(view.pairs[0].winner, "Heavy Boots.esp");
        assert_eq!(view.pairs[0].labels, vec!["GMST fJumpHeightMin"]);
        assert_eq!((view.patched, view.decided, view.implicit_pairs), (2, 1, 3));
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["pairs"][0]["newSinceLastLaunch"], true);
        assert_eq!(json["implicitPairs"], 3);
    }

    #[test]
    fn a_sorter_note_keeps_its_level_so_the_card_can_be_titled_honestly() {
        let info = agora_core::game_load_order::PluginNote {
            plugin: String::new(),
            level: agora_core::game_load_order::NoteLevel::Info,
            message: "Nothing to change.".into(),
        };
        let warn = agora_core::game_load_order::PluginNote {
            plugin: "SkyUI.esp".into(),
            level: agora_core::game_load_order::NoteLevel::Warn,
            message: "A master is missing.".into(),
        };
        assert_eq!(note_view(&info).level, "info");
        assert_eq!(note_view(&warn).level, "warning");
        assert_eq!(note_view(&warn).plugins, vec!["SkyUI.esp".to_string()]);
    }

    #[test]
    fn an_info_finding_is_not_a_warning_and_still_names_no_plugin() {
        let info = Finding::ListLevel {
            message: "Info: 2 conflicts between 1 pair of plugins.".into(),
            items: Vec::new(),
            refuses: false,
        };
        let view = finding_view(&info);
        assert_eq!(view.level, "info");
        assert!(!view.refuses_launch);
        assert!(view.plugins.is_empty());
    }

    #[test]
    fn a_move_that_breaks_a_rule_carries_the_rule_as_details() {
        let error = move_failed(agora_core::game_load_order::LoadOrderError::BreaksRule {
            plugin: "Heavy Boots.esp".into(),
            item: "Heavy Boots.esp".into(),
            after: "Moon Jump.esp".into(),
        });
        assert_eq!(error.code, "BREAKS_RULE");
        let details = error.details.expect("the rule is in the details");
        assert_eq!(details["item"], "Heavy Boots.esp");
        assert_eq!(details["after"], "Moon Jump.esp");
    }

    /// Read-only check against a real MO2 import. Run by hand:
    /// `cargo test -p agora-desktop --lib real_profile_read_only -- --ignored --nocapture`.
    ///
    /// Runs the same core calls as `games_plugins_list` and `games_plugins_check`, and writes no
    /// plugin, list or state file: no sort, move, lock or set. The data root is opened with
    /// `CoreContext::for_testing` rather than `initialize`, because `initialize` creates the
    /// required folders and migrates `local_state.db`. Opening `local_state.db` (already in WAL
    /// mode) can still create and remove its `-wal` and `-shm` sidecars, which changes the data
    /// root's folder timestamp but no file in it.
    #[test]
    #[ignore = "reads the p3-done data root; run by hand, read only"]
    fn real_profile_read_only() {
        let root = std::path::PathBuf::from(r"D:\Agora-bench\p3-done");
        let instance_id = "grounded-apocalypse-mo2-import-6d97f2";
        let ctx = agora_core::ctx::CoreContext::for_testing(root)
            .with_games(std::sync::Arc::new(crate::build_game_registry().build()));
        let record = agora_core::game_instance::get(&ctx, instance_id)
            .expect("the instance record reads")
            .expect("the instance exists");
        let definition = ctx.games.game(&record.game).expect("the game is known");

        let started = std::time::Instant::now();
        agora_core::game_deploy::plan(
            &ctx,
            instance_id,
            definition,
            agora_core::game_deploy::DeployMode::Links,
        )
        .expect("the deployment plan reads");
        let plan_time = started.elapsed();

        let started = std::time::Instant::now();
        let order = load_order::order(&ctx, instance_id, definition).expect("the load order reads");
        let view = PluginsView::from_order(&order);
        let order_time = started.elapsed();

        let started = std::time::Instant::now();
        let findings: Vec<PluginFinding> = load_order::check(&ctx, instance_id, definition)
            .expect("the check reads")
            .iter()
            .map(finding_view)
            .collect();
        let check_time = started.elapsed();

        let rows = &view.rows;
        let count = |f: fn(&PluginRow) -> bool| rows.iter().filter(|r| f(r)).count();
        println!(
            "supported={} rules_apply={} list_exists={}",
            view.supported, view.rules_apply, view.list_exists
        );
        println!(
            "rows={} implicit={} active={} present={} managed={} locked={} masters={} badges={}",
            rows.len(),
            count(|r| r.implicit),
            count(|r| r.active),
            count(|r| r.present),
            count(|r| r.managed),
            count(|r| r.locked),
            count(|r| !r.masters.is_empty()),
            count(|r| !r.badges.is_empty()),
        );
        println!(
            "plan: {:?}  order: {:?}  check: {:?}",
            plan_time, order_time, check_time
        );
        println!("check status: {}", check_status(&findings));
        let refusing = findings.iter().filter(|f| f.refuses_launch).count();
        println!(
            "findings: {} total, {} refuse launch",
            findings.len(),
            refusing
        );
        for finding in findings.iter().take(15) {
            println!(
                "  [{}] {} {:?}",
                if finding.refuses_launch {
                    "refuses"
                } else {
                    "warns"
                },
                finding.message,
                finding.plugins
            );
        }
        for row in rows
            .iter()
            .filter(|r| r.badges.is_empty() && !r.masters.is_empty())
            .take(10)
        {
            println!("  needs {}: {}", row.name, row.masters.join(", "));
        }
        assert!(
            view.supported && view.list_exists,
            "the real profile has a plugin list"
        );
        assert!(!rows.is_empty(), "the real profile lists plugins");
    }
}
