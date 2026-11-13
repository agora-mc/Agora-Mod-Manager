//! Game instances: the list, an instance's page, its checks, its launch, create and delete.
//! Owned by the `instances` slice (MASTER_SPEC §26.3, §26.5, §26.14).
//!
//! Each command is a thin adapter over the `agora_core::game_instance` services. The CLI's
//! `agora games instance ...` handlers call the same services, so the two cannot disagree about
//! what an instance is. Launch runs the whole sequence in `launch_with` (deploy, check, swap-in,
//! spawn, record). This file only turns the core's questions into replies the page can show, and
//! watches the game so the user's files are put back when it closes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agora_core::game_base::{BaseProblem, BuildOptions, BuildOutcome, BuildProgress, ProblemKind};
use agora_core::game_deploy::DeployMode;
use agora_core::game_instance::{
    self, GameInstanceManifest, GameInstanceRecord, InstanceError, LaunchOptions, LaunchedInstance,
    PlayStats, VfsFailure,
};
use agora_core::game_launch::{LaunchError, LaunchedGame};
use agora_core::game_user_files::{self, SessionStatus};
use agora_game_api::GameDefinition;
use agora_game_api::{
    BaseMode, BaseReference, GameId, RuntimeFileFinding, RuntimeFileProblem, RuntimeIdentity,
    StoreId,
};
use serde::Serialize;
use tauri::Emitter;

use super::error::{GamesError, CORE_FAILED, TASK_FAILED};

/// No instance has this id.
pub const INSTANCE_NOT_FOUND: &str = "instance_not_found";
/// The instance is running, so the change or the launch has to wait until the game closes.
pub const INSTANCE_RUNNING: &str = "instance_running";
/// The instance's base record cannot be read; deleting it needs the user's answer.
pub const INSTANCE_BASE_UNREADABLE: &str = "instance_base_unreadable";
/// The request names something that cannot exist: a blank name, an unknown mode, a bad game id.
pub const INVALID_REQUEST: &str = "invalid_request";
/// The id belongs to a Minecraft instance, which the Minecraft pages manage.
pub const MINECRAFT_INSTANCE: &str = "minecraft_instance";
/// No install with that id was found on this machine.
pub const INSTALL_NOT_FOUND: &str = "install_not_found";
/// A deleted instance's id has been taken again, so restoring needs a new name.
pub const INSTANCE_ID_TAKEN: &str = "instance_id_taken";
/// Nothing is running for the instance, so there is nothing to stop.
pub const INSTANCE_NOT_RUNNING: &str = "instance_not_running";
/// The game is running but Agora did not start it, so it is not killed.
pub const INSTANCE_NOT_AGORA_PROCESS: &str = "instance_not_agora_process";
/// The install's game version is not one Agora recognises, so it cannot be pinned.
pub const RUNTIME_UNKNOWN: &str = "runtime_unidentified";

/// Progress from `games_instance_create`, sent as this event.
pub const CREATE_PROGRESS_EVENT: &str = "games-instance-create-progress";
/// Sent when a launched game has closed and its user files were put back (or could not be).
pub const INSTANCE_EXITED_EVENT: &str = "games-instance-exited";

/// Progress is sent for every this-many files, and for the last one.
const PROGRESS_EVERY: u64 = 50;

// ---------------------------------------------------------------------------
// Data the frontend reads
// ---------------------------------------------------------------------------

/// The Instances tab: one card per instance of the game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceList {
    pub instances: Vec<InstanceCard>,
}

/// What the page may offer for this game and instance. The UI shows only what works, rather than
/// rendering an action that fails when pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCapabilities {
    pub launch: bool,
    pub stop: bool,
    pub rename: bool,
    pub clone: bool,
    pub trash: bool,
    pub open_folder: bool,
    pub play_time: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCard {
    pub instance_id: String,
    pub game_id: String,
    pub name: String,
    pub store: Option<String>,
    pub version: Option<String>,
    pub pinned: bool,
    /// `virtual`, `links` or `copies` when chosen; `None` means automatic (the game's default).
    pub deployment: Option<String>,
    pub last_launched_at: Option<String>,
    /// When the instance was made (SQLite UTC, no zone), for the card's "Created" line.
    pub created_at: String,
    pub last_played_at: Option<String>,
    pub play_time_ms: i64,
    pub running: bool,
    pub capabilities: InstanceCapabilities,
}

/// An instance's page header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceDetail {
    pub instance_id: String,
    pub game_id: String,
    pub game_name: String,
    pub name: String,
    pub pinned: bool,
    pub store: Option<String>,
    pub version: Option<String>,
    pub build: Option<String>,
    pub base_id: Option<String>,
    pub install_id: Option<String>,
    /// Why the instance runs from its install folder, when it is not pinned.
    pub unpinned_reason: Option<String>,
    pub deployment: Option<String>,
    /// `shared` (the game's save folder) or `own`.
    pub saves: String,
    pub created_at: String,
    pub last_launched_at: Option<String>,
    pub last_played_at: Option<String>,
    pub play_time_ms: i64,
    pub running: bool,
    pub capabilities: InstanceCapabilities,
}

/// One thing a check or a launch found, in the words the page shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckFinding {
    /// `framework`, `load_order`, `tool` or `base`.
    pub kind: &'static str,
    /// True when this stops a launch unless the user launches anyway.
    pub blocks: bool,
    pub title: String,
    /// What is wrong.
    pub detail: String,
    /// Why it matters.
    pub why: String,
    /// What to do about it.
    pub repair: Option<String>,
}

/// An instance's launch checks. A pinned instance is checked; an unpinned one has nothing to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceCheck {
    pub instance_id: String,
    pub checked: bool,
    /// `ok`, `warnings`, or `findings` (a finding that stops the launch).
    pub status: &'static str,
    pub findings: Vec<CheckFinding>,
    /// The checks that ran and passed, never a check that did not run.
    pub passed: Vec<agora_core::game_instance::PassedCheck>,
}

/// What a launch did. `status` says which of the three answers this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchReply {
    /// `launched`: the game started. `refused`: a check stopped it; see `findings`.
    /// `vfs_unavailable`: the virtual file system would not start; `reason` and `next` say what
    /// running from `next` means. Nothing was started; the user is asked first.
    pub status: &'static str,
    pub pid: Option<u32>,
    pub deployment: Option<String>,
    /// How this launch runs, in plain words, when it differs from what was asked.
    pub notice: Option<String>,
    /// For `launched`: what the user launched past. For `refused`: what stopped it.
    pub findings: Vec<CheckFinding>,
    pub reason: Option<String>,
    pub next: Option<String>,
    /// For `before_launch_needed`: the tools that have not run (or whose output is stale).
    pub before_launch: Vec<BeforeLaunchNeedView>,
    /// For `before_launch_failed`: the tool that ran and did not finish.
    pub before_launch_failure: Option<BeforeLaunchFailureView>,
}

/// One tool a launch should run first, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeLaunchNeedView {
    pub tool: String,
    pub name: String,
    /// `run` or `ask`.
    pub mode: String,
    /// Why the tool is needed, in core's words.
    pub reason: String,
}

/// A before-launch run that failed, in the words the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeLaunchFailureView {
    pub tool: String,
    pub name: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedInstance {
    pub instance_id: String,
    pub name: String,
    /// `pinned` (a base was built or reused) or `unpinned` (it runs from the install folder).
    pub base: &'static str,
    /// True when an identical base already existed and was reused rather than built.
    pub reused_base: bool,
    pub base_id: Option<String>,
    pub unpinned_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedInstance {
    pub instance_id: String,
    /// The base the deleted instance pinned, when nothing else pins it any more.
    pub orphaned_base: Option<String>,
    /// A deployed game folder Agora could not prove was the instance's, left where it was.
    pub left_in_place: Option<String>,
    /// The Recently deleted entry it can be restored from.
    pub trash_entry_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamedInstance {
    pub instance_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClonedInstance {
    pub instance_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoppedInstance {
    pub instance_id: String,
    pub processes_ended: usize,
    pub play_time_ms: i64,
}

/// One entry in Recently deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedEntry {
    pub entry_id: String,
    pub instance_id: String,
    pub game_id: String,
    pub name: String,
    pub deleted_at: String,
    pub expires_at: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedList {
    pub entries: Vec<DeletedEntry>,
    /// The player's retention window, in days.
    pub retention_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoredInstance {
    pub instance_id: String,
    pub original_instance_id: String,
    pub renamed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgedDeleted {
    pub purged: usize,
    pub failed: usize,
    pub kept: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceDeployment {
    pub instance_id: String,
    pub deployment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_hashed: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceExited {
    pub instance_id: String,
    /// The reason the user's files could not be put back, or `None` when they were.
    pub restore_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The generic instances of one game, for its Instances tab.
#[tauri::command]
pub async fn games_instance_list(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<InstanceList, GamesError> {
    blocking(move || list_instances(&app, &game_id)).await
}

/// One instance, for its page's header.
#[tauri::command]
pub async fn games_instance_get(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<InstanceDetail, GamesError> {
    blocking(move || instance_detail(&app, &instance_id)).await
}

/// Create an instance from an install of a game. Builds the pinned base (or reuses one that
/// exists) and reports progress as `games-instance-create-progress`.
#[tauri::command]
pub async fn games_instance_create(
    app: tauri::AppHandle,
    game_id: String,
    install_id: String,
    name: String,
    base_mode: String,
    deployment: Option<String>,
) -> Result<CreatedInstance, GamesError> {
    let chosen = match deployment {
        Some(word) => parse_deployment(&word)?,
        None => None,
    };
    blocking(move || create_instance(&app, &game_id, &install_id, &name, &base_mode, chosen)).await
}

/// Delete an instance and its folder. Refused while the game is running.
#[tauri::command]
pub async fn games_instance_delete(
    app: tauri::AppHandle,
    instance_id: String,
    delete_anyway: bool,
) -> Result<DeletedInstance, GamesError> {
    blocking(move || delete_instance(&app, &instance_id, delete_anyway)).await
}

/// Give an instance a new display name. Its id and folder do not change.
#[tauri::command]
pub async fn games_instance_rename(
    app: tauri::AppHandle,
    instance_id: String,
    name: String,
) -> Result<RenamedInstance, GamesError> {
    blocking(move || rename_instance(&app, &instance_id, &name)).await
}

/// Copy an instance's content, plugin order, INIs and settings into a new one. Saves are copied
/// only when `copy_saves` is true.
#[tauri::command]
pub async fn games_instance_clone(
    app: tauri::AppHandle,
    instance_id: String,
    name: String,
    copy_saves: bool,
) -> Result<ClonedInstance, GamesError> {
    blocking(move || clone_instance(&app, &instance_id, &name, copy_saves)).await
}

/// Stop the game Agora started for an instance, then run the normal session end.
#[tauri::command]
pub async fn games_instance_stop(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<StoppedInstance, GamesError> {
    blocking(move || stop_instance(&app, &instance_id)).await
}

/// The instances of one game in Recently deleted, newest first. Runs the retention pass first.
#[tauri::command]
pub async fn games_instance_deleted_list(
    app: tauri::AppHandle,
    game_id: String,
) -> Result<DeletedList, GamesError> {
    blocking(move || deleted_list(&app, &game_id)).await
}

/// Restore a deleted instance. Without a name its original id is used and a taken id is refused;
/// with a name it comes back under a new id.
#[tauri::command]
pub async fn games_instance_restore_deleted(
    app: tauri::AppHandle,
    entry_id: String,
    name: Option<String>,
) -> Result<RestoredInstance, GamesError> {
    blocking(move || restore_deleted(&app, &entry_id, name.as_deref())).await
}

/// Send one deleted entry to the OS trash, or every entry of `game_id` when no `entry_id` is
/// given. "All" is scoped to the game whose page asked: another game's entries are not its to
/// empty.
#[tauri::command]
pub async fn games_instance_purge_deleted(
    app: tauri::AppHandle,
    game_id: String,
    entry_id: Option<String>,
) -> Result<PurgedDeleted, GamesError> {
    blocking(move || purge_deleted(&app, &game_id, entry_id.as_deref())).await
}

/// The launch checks for an instance, without starting anything.
#[tauri::command]
pub async fn games_instance_check(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<InstanceCheck, GamesError> {
    blocking(move || check_instance(&app, &instance_id)).await
}

/// Choose how an instance deploys and runs: `auto`, `virtual`, `links` or `copies`.
#[tauri::command]
pub async fn games_instance_set_deployment(
    app: tauri::AppHandle,
    instance_id: String,
    mode: String,
) -> Result<InstanceDeployment, GamesError> {
    blocking(move || set_deployment(&app, &instance_id, &mode)).await
}

/// Launch an instance. `deployment` runs this launch only (`None` uses the instance's choice);
/// `launch_anyway` starts the game past the checks that would stop it.
#[tauri::command]
pub async fn games_instance_launch(
    app: tauri::AppHandle,
    instance_id: String,
    launch_anyway: bool,
    deployment: Option<String>,
    skip_before_launch: Option<bool>,
    before_launch: Option<std::collections::BTreeMap<String, String>>,
) -> Result<LaunchReply, GamesError> {
    let chosen = match deployment {
        Some(word) => parse_deployment(&word)?,
        None => None,
    };
    let before = if skip_before_launch.unwrap_or(false) {
        game_instance::BeforeLaunchChoice::Skip
    } else if let Some(answers) = before_launch {
        let mut parsed = std::collections::BTreeMap::new();
        for (tool, answer) in answers {
            let answer = match answer.as_str() {
                "run" => game_instance::BeforeLaunchAnswer::Run,
                "launch_anyway" | "launch-anyway" => {
                    game_instance::BeforeLaunchAnswer::LaunchAnyway
                }
                other => {
                    return Err(GamesError::new(
                        "ERR_BEFORE_LAUNCH_ANSWER",
                        format!(
                            "Unknown before-launch answer '{other}'. Use run or launch_anyway."
                        ),
                    ))
                }
            };
            parsed.insert(tool, answer);
        }
        game_instance::BeforeLaunchChoice::Answers(parsed)
    } else {
        // The page asks before a protected launch: nothing runs unasked.
        game_instance::BeforeLaunchChoice::Ask
    };
    blocking(move || launch_instance(&app, &instance_id, launch_anyway, chosen, before)).await
}

async fn blocking<T, F>(work: F) -> Result<T, GamesError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, GamesError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| GamesError::new(TASK_FAILED, "The instance work did not finish."))?
}

// ---------------------------------------------------------------------------
// The work
// ---------------------------------------------------------------------------

fn list_instances(app: &tauri::AppHandle, game_id: &str) -> Result<InstanceList, GamesError> {
    let game = parse_game_id(game_id)?;
    let ctx = crate::core_context(app)?;
    // A delete or restore cut off by a crash is finished or called off before the list is read,
    // so the page never shows an instance whose folder is in Recently deleted.
    game_instance::trash::recover_interrupted(&ctx);
    let records = game_instance::list(&ctx).map_err(map_error)?;
    let mut instances = Vec::new();
    for record in records
        .iter()
        .filter(|record| record.game.as_str() == game.as_str())
    {
        let definition = ctx
            .games
            .game(&record.game)
            .ok_or_else(|| game_missing(record.game.as_str()))?;
        let manifest = game_instance::get_manifest(&ctx, &record.instance_id).map_err(map_error)?;
        let running = is_running(&ctx, record, &manifest)?;
        let stats = game_instance::play_stats(&ctx, &record.instance_id).map_err(map_error)?;
        let folder_exists = ctx
            .paths
            .instance_dir(&record.instance_id)
            .map(|dir| dir.is_dir())
            .unwrap_or(false);
        instances.push(card_for(
            definition,
            record,
            &manifest,
            &stats,
            running,
            folder_exists,
        ));
    }
    Ok(InstanceList { instances })
}

fn instance_detail(
    app: &tauri::AppHandle,
    instance_id: &str,
) -> Result<InstanceDetail, GamesError> {
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let manifest = game_instance::get_manifest(&ctx, instance_id).map_err(map_error)?;
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| game_missing(record.game.as_str()))?;
    let running = is_running(&ctx, &record, &manifest)?;
    let stats = game_instance::play_stats(&ctx, instance_id).map_err(map_error)?;
    let folder_exists = ctx
        .paths
        .instance_dir(instance_id)
        .map(|dir| dir.is_dir())
        .unwrap_or(false);
    let runtime = runtime_of(&record, &manifest);
    Ok(InstanceDetail {
        instance_id: record.instance_id.clone(),
        game_id: record.game.as_str().to_string(),
        game_name: definition.name.clone(),
        name: record.name.clone(),
        pinned: is_pinned(&record),
        store: runtime.as_ref().map(|r| r.store.as_str().to_string()),
        version: runtime.as_ref().map(|r| r.version.clone()),
        build: runtime.as_ref().and_then(|r| r.build.clone()),
        base_id: match &record.base {
            BaseReference::Pinned { id, .. } => Some(id.clone()),
            BaseReference::Unpinned { .. } => None,
        },
        install_id: match &record.base {
            BaseReference::Unpinned { install, .. } => Some(install.to_string()),
            BaseReference::Pinned { .. } => None,
        },
        unpinned_reason: match &record.base {
            BaseReference::Unpinned { reason, .. } => Some(reason.clone()),
            BaseReference::Pinned { .. } => None,
        },
        deployment: manifest.deployment.map(|mode| mode.as_str().to_string()),
        saves: manifest.saves.as_str().to_string(),
        created_at: record.created_at.clone(),
        last_launched_at: record.last_launched_at.clone(),
        last_played_at: stats.last_played_at,
        play_time_ms: stats.total_play_ms,
        running,
        capabilities: capabilities_for(definition, &record, &manifest, folder_exists),
    })
}

fn create_instance(
    app: &tauri::AppHandle,
    game_id: &str,
    install_id: &str,
    name: &str,
    base_mode: &str,
    deployment: Option<DeployMode>,
) -> Result<CreatedInstance, GamesError> {
    let game = parse_game_id(game_id)?;
    let mode = parse_base_mode(base_mode)?;
    let name = clean_name(name)?;
    let ctx = crate::core_context(app)?;

    // The same discovery and identification the Setup page and `agora games list` use.
    let inventory = agora_core::game_registry::installed_games(&ctx);
    let install = inventory
        .installs
        .iter()
        .find(|install| install.install_id.as_str() == install_id)
        .ok_or_else(|| {
            GamesError::new(
                INSTALL_NOT_FOUND,
                format!("No install '{install_id}' was found. Rescan the installs in Settings, on the game's Installs page, and try again."),
            )
        })?;
    if install.game.as_str() != game.as_str() {
        return Err(GamesError::new(
            INVALID_REQUEST,
            format!("Install '{install_id}' is not a {game_id} install."),
        ));
    }
    let definition = ctx
        .games
        .game(&install.game)
        .ok_or_else(|| game_missing(install.game.as_str()))?;

    let progress_app = app.clone();
    let record = game_instance::create_with_options(
        &ctx,
        install,
        definition,
        &name,
        None,
        mode,
        BuildOptions::default(),
        &move |progress: BuildProgress| {
            if progress.files_done.is_multiple_of(PROGRESS_EVERY)
                || progress.files_done == progress.files_total
            {
                // Progress is cosmetic: a failed emit must not fail the create.
                let _ = progress_app.emit(
                    CREATE_PROGRESS_EVENT,
                    CreateProgress {
                        files_done: progress.files_done,
                        files_total: progress.files_total,
                        bytes_hashed: progress.bytes_hashed,
                        bytes_total: progress.bytes_total,
                    },
                );
            }
        },
    )
    .map_err(map_error)?;

    // Apply the defaults the user chose for this game. A failure here removes the instance Agora
    // just made (nothing the user owned), so a half-set-up instance is never left behind.
    let preferences: Result<(), GamesError> = (|| {
        if let Some(mode) = deployment {
            agora_core::game_deploy::set_deployment(&ctx, &record.instance_id, Some(mode))
                .map_err(core_failed)?;
        }
        Ok(())
    })();
    if let Err(error) = preferences {
        let _ = game_instance::delete_permanently(&ctx, &record.instance_id);
        return Err(error);
    }

    Ok(created_for(
        &record,
        name,
        reused_base(record.build_outcome.as_ref()),
    ))
}

fn delete_instance(
    app: &tauri::AppHandle,
    instance_id: &str,
    delete_anyway: bool,
) -> Result<DeletedInstance, GamesError> {
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let manifest = game_instance::get_manifest(&ctx, instance_id).map_err(map_error)?;
    if is_running(&ctx, &record, &manifest)? {
        return Err(running_error(instance_id));
    }
    let outcome = game_instance::delete_with_options(
        &ctx,
        instance_id,
        game_instance::DeleteOptions {
            force_unreadable_base: delete_anyway,
            recoverable: true,
        },
    )
    .map_err(map_error)?;
    Ok(DeletedInstance {
        instance_id: outcome.instance_id,
        orphaned_base: outcome.orphaned_base,
        left_in_place: outcome
            .left_in_place
            .map(|folder| folder.display().to_string()),
        trash_entry_id: outcome.trash_entry_id,
    })
}

fn rename_instance(
    app: &tauri::AppHandle,
    instance_id: &str,
    name: &str,
) -> Result<RenamedInstance, GamesError> {
    let name = clean_name(name)?;
    let ctx = crate::core_context(app)?;
    load_record(&ctx, instance_id)?;
    game_instance::rename(&ctx, instance_id, &name).map_err(map_error)?;
    Ok(RenamedInstance {
        instance_id: instance_id.to_string(),
        name,
    })
}

fn clone_instance(
    app: &tauri::AppHandle,
    instance_id: &str,
    name: &str,
    copy_saves: bool,
) -> Result<ClonedInstance, GamesError> {
    let name = clean_name(name)?;
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| game_missing(record.game.as_str()))?;
    let cloned = game_instance::clone_instance(
        &ctx,
        instance_id,
        &name,
        definition,
        game_instance::CloneOptions { copy_saves },
    )
    .map_err(map_error)?;
    Ok(ClonedInstance {
        instance_id: cloned.instance_id,
        name: cloned.name,
    })
}

fn stop_instance(app: &tauri::AppHandle, instance_id: &str) -> Result<StoppedInstance, GamesError> {
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| game_missing(record.game.as_str()))?;
    let stopped = game_instance::stop(&ctx, instance_id, definition).map_err(map_error)?;
    Ok(StoppedInstance {
        instance_id: stopped.instance_id,
        processes_ended: stopped.processes_ended,
        play_time_ms: stopped.play_time_ms,
    })
}

fn trash_adapter() -> game_instance::TrashFn {
    Arc::new(|path: &Path| {
        trash::delete(path)
            .map_err(|error| format!("Could not move it to the system trash: {error}"))
    })
}

fn deleted_list(app: &tauri::AppHandle, game_id: &str) -> Result<DeletedList, GamesError> {
    let game = parse_game_id(game_id)?;
    let ctx = crate::core_context(app)?;
    // The retention pass runs when someone looks, so a user who never deletes anything still has
    // old entries cleaned up after the window they chose.
    let _ = game_instance::trash::cleanup(
        &ctx,
        chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()),
        &trash_adapter(),
    );
    let entries = game_instance::trash::list(&ctx, game.as_str())
        .into_iter()
        .map(|entry| DeletedEntry {
            entry_id: entry.entry_id,
            instance_id: entry.instance_id,
            game_id: entry.game,
            name: entry.name,
            deleted_at: entry.deleted_at,
            expires_at: entry.expires_at,
            size_bytes: entry.size_bytes,
        })
        .collect();
    Ok(DeletedList {
        entries,
        retention_days: game_instance::trash::retention_days(&ctx),
    })
}

fn restore_deleted(
    app: &tauri::AppHandle,
    entry_id: &str,
    name: Option<&str>,
) -> Result<RestoredInstance, GamesError> {
    let ctx = crate::core_context(app)?;
    let restored = game_instance::trash::restore(&ctx, entry_id, name).map_err(map_error)?;
    Ok(RestoredInstance {
        instance_id: restored.instance_id,
        original_instance_id: restored.original_instance_id,
        renamed: restored.renamed,
    })
}

fn purge_deleted(
    app: &tauri::AppHandle,
    game_id: &str,
    entry_id: Option<&str>,
) -> Result<PurgedDeleted, GamesError> {
    let game = parse_game_id(game_id)?;
    let ctx = crate::core_context(app)?;
    let trash = trash_adapter();
    let summary = match entry_id {
        Some(entry_id) => {
            game_instance::trash::purge(&ctx, entry_id, &trash).map_err(map_error)?;
            game_instance::trash::PurgeSummary {
                purged: 1,
                failed: 0,
                kept: 0,
            }
        }
        None => game_instance::trash::purge_all(&ctx, game.as_str(), &trash),
    };
    Ok(PurgedDeleted {
        purged: summary.purged,
        failed: summary.failed,
        kept: summary.kept,
    })
}

fn check_instance(app: &tauri::AppHandle, instance_id: &str) -> Result<InstanceCheck, GamesError> {
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| game_missing(record.game.as_str()))?;
    let pinned = is_pinned(&record);

    // The same checks `agora games instance check` runs, from the same core call, in the same order.
    let checks =
        game_instance::launch_checks(&ctx, &record, definition).map_err(|error| match error {
            game_instance::LaunchChecksError::Runtime(inner) => map_error(inner),
            other => core_failed(other),
        })?;
    let mut findings = Vec::new();
    for finding in &checks.runtime {
        findings.push(framework_finding(finding));
    }
    for finding in &checks.load_order {
        findings.push(load_order_finding(finding));
    }
    for output in &checks.outputs {
        findings.push(tool_finding(output));
    }
    for requirement in &checks.requirements {
        findings.push(requirement_finding(requirement));
    }
    Ok(InstanceCheck {
        instance_id: instance_id.to_string(),
        checked: pinned,
        status: status_for(&findings),
        findings,
        passed: checks.passed,
    })
}

fn set_deployment(
    app: &tauri::AppHandle,
    instance_id: &str,
    mode: &str,
) -> Result<InstanceDeployment, GamesError> {
    let chosen = parse_deployment(mode)?;
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let manifest = game_instance::get_manifest(&ctx, instance_id).map_err(map_error)?;
    if is_running(&ctx, &record, &manifest)? {
        return Err(running_error(instance_id));
    }
    agora_core::game_deploy::set_deployment(&ctx, instance_id, chosen).map_err(core_failed)?;
    Ok(InstanceDeployment {
        instance_id: instance_id.to_string(),
        deployment: chosen.map(|mode| mode.as_str().to_string()),
    })
}

fn launch_instance(
    app: &tauri::AppHandle,
    instance_id: &str,
    launch_anyway: bool,
    deployment: Option<DeployMode>,
    before_launch: game_instance::BeforeLaunchChoice,
) -> Result<LaunchReply, GamesError> {
    let ctx = crate::core_context(app)?;
    let record = load_record(&ctx, instance_id)?;
    let manifest = game_instance::get_manifest(&ctx, instance_id).map_err(map_error)?;
    if is_running(&ctx, &record, &manifest)? {
        return Err(running_error(instance_id));
    }
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| game_missing(record.game.as_str()))?;

    let options = LaunchOptions {
        launch_anyway,
        plain: false,
        deployment,
        // Ask, never step down on our own: the page asks the user and launches again if they say yes.
        on_vfs_failure: VfsFailure::Ask,
        before_launch,
    };
    let launched = match game_instance::launch_with(
        &ctx,
        instance_id,
        definition,
        options,
        &|| agora_core::game_discovery::discover_with_folders(&ctx.paths),
        &agora_core::game_launch::SystemLauncher,
    ) {
        Ok(launched) => launched,
        Err(InstanceError::VfsUnavailable { reason, next, .. }) => {
            return Ok(vfs_question(reason, next));
        }
        Err(InstanceError::BeforeLaunchNeeded { tools, .. }) => {
            return Ok(before_launch_question(tools));
        }
        Err(InstanceError::BeforeLaunchFailed {
            tool,
            name,
            message,
        }) => {
            return Ok(before_launch_failed(tool, name, message));
        }
        Err(InstanceError::LaunchError(LaunchError::RuntimeMismatch { findings })) => {
            return Ok(refused(findings.iter().map(framework_finding).collect()));
        }
        Err(InstanceError::LaunchError(LaunchError::LoadOrderProblems { findings })) => {
            return Ok(refused(findings.iter().map(load_order_finding).collect()));
        }
        Err(InstanceError::LaunchError(LaunchError::BaseDamaged { problems })) => {
            return Ok(refused(
                problems.iter().map(|p| base_finding(p, true)).collect(),
            ));
        }
        Err(other) => return Err(map_error(other)),
    };

    let LaunchedInstance {
        launched: game,
        prepared,
        store,
        running_from,
        session_id,
    } = launched;
    let mut findings: Vec<CheckFinding> = Vec::new();
    findings.extend(prepared.warnings.iter().map(|p| base_finding(p, false)));
    findings.extend(prepared.runtime_findings.iter().map(framework_finding));
    findings.extend(prepared.load_order_findings.iter().map(load_order_finding));
    findings.extend(prepared.generated_findings.iter().map(tool_finding));
    let reply = LaunchReply {
        status: "launched",
        pid: Some(game.pid()),
        deployment: prepared.deployment.map(|mode| mode.as_str().to_string()),
        notice: prepared.notice.clone(),
        findings,
        reason: None,
        next: None,
        before_launch: Vec::new(),
        before_launch_failure: None,
    };

    // The user's saves and settings are swapped in for this session. They go back when the game
    // closes, the same as `agora games instance launch --wait`; the page hears about it either way.
    spawn_exit_watch(
        app.clone(),
        ctx,
        instance_id.to_string(),
        game,
        SessionWatch {
            running_from,
            store,
            game_id: record.game.clone(),
            session_id,
        },
    );
    Ok(reply)
}

/// What the exit watcher needs to know about the session it watches.
struct SessionWatch {
    running_from: PathBuf,
    store: StoreId,
    game_id: GameId,
    /// The session ledger row the launch opened, when one could be written.
    session_id: Option<String>,
}

/// Waits for the launched game to close, then puts the user's files back and tells the page.
/// A restore that fails is reported, never dropped: the files stay swapped until someone looks.
fn spawn_exit_watch(
    app: tauri::AppHandle,
    ctx: agora_core::ctx::Ctx,
    instance_id: String,
    mut game: LaunchedGame,
    watch: SessionWatch,
) {
    let SessionWatch {
        running_from,
        store,
        game_id,
        session_id,
    } = watch;
    std::thread::spawn(move || {
        // Waits for the game's own process (not the loader that started it) and records how it
        // ended in the session ledger.
        match ctx.games.game(&game_id) {
            Some(definition) => {
                let _ = game_instance::watch_session(
                    &ctx,
                    definition,
                    session_id.as_deref(),
                    &running_from,
                    &mut game,
                    Duration::from_millis(250),
                    Duration::from_secs(5),
                );
            }
            // No definition to read the game's process from: wait for the launched program alone.
            None => {
                let _ = agora_core::game_launch::wait_for_exit(
                    &running_from,
                    &mut game,
                    Duration::from_millis(250),
                    Duration::from_secs(5),
                );
            }
        }
        // The session is recorded. Diagnose it now, before a logger can rotate its evidence away.
        // Every failure is stored as a "could not analyze" answer, never raised to the page.
        if let Some(session) = session_id.as_deref() {
            let host: Arc<dyn agora_plugin_api::host::ScriptHost> =
                Arc::new(agora_plugin_host::QuickJsHost::new());
            match agora_core::crash_live::load_installed_packs(&ctx, host.clone()) {
                Ok(installed) => {
                    let built_in = agora_game_minecraft::crash::built_in_packs();
                    let _ = agora_core::crash_live::diagnose_session(
                        &ctx,
                        session,
                        agora_core::crash_live::LivePacks {
                            packs: &installed.packs,
                            built_in: &built_in,
                            host: host.as_ref(),
                        },
                    );
                }
                Err(error) => {
                    let _ = agora_core::crash_live::record_could_not_analyze(
                        &ctx,
                        session,
                        format!("the crash packs could not be loaded: {error}"),
                    );
                }
            }
        }
        // The same end a stop runs: put the player's files back and add this session to the
        // instance's play time.
        let restore_error = match ctx.games.game(&game_id) {
            Some(definition) => match game_instance::end_session(&ctx, definition, &store) {
                Ok(_) => None,
                Err(error) => Some(error.to_string()),
            },
            None => Some(game_missing(game_id.as_str()).message),
        };
        // The event is for the page to refresh; the restore result above is the real outcome.
        let _ = app.emit(
            INSTANCE_EXITED_EVENT,
            InstanceExited {
                instance_id,
                restore_error,
            },
        );
    });
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn load_record(
    ctx: &agora_core::ctx::Ctx,
    instance_id: &str,
) -> Result<GameInstanceRecord, GamesError> {
    game_instance::get(ctx, instance_id)
        .map_err(map_error)?
        .ok_or_else(|| not_found(instance_id))
}

fn is_pinned(record: &GameInstanceRecord) -> bool {
    matches!(record.base, BaseReference::Pinned { .. })
}

/// The runtime an instance was built for: its pinned base's, else the identity its manifest kept.
fn runtime_of(
    record: &GameInstanceRecord,
    manifest: &GameInstanceManifest,
) -> Option<RuntimeIdentity> {
    match &record.base {
        BaseReference::Pinned { runtime, .. } => Some(runtime.clone()),
        BaseReference::Unpinned { .. } => manifest.runtime_identity.clone(),
    }
}

/// Whether the game is running from this instance right now, from the user-file session. An
/// unreadable session is an error, not "not running": the page shows it instead of guessing.
fn is_running(
    ctx: &agora_core::ctx::Ctx,
    record: &GameInstanceRecord,
    manifest: &GameInstanceManifest,
) -> Result<bool, GamesError> {
    match runtime_of(record, manifest) {
        Some(runtime) => Ok(running_from_session(
            game_user_files::status(ctx, &record.game, &runtime.store).map_err(core_failed)?,
            &record.instance_id,
        )),
        // An instance with no recorded runtime has no store to look a session up under. It has
        // never been launched through a store this build can name, so no session can be ours.
        None => Ok(false),
    }
}

/// What this game and instance support. `launch` is the game's (a definition with no launch
/// recipe cannot start); `stop` needs a store whose session Agora can find; `openFolder` needs a
/// folder that exists.
fn capabilities_for(
    definition: &GameDefinition,
    record: &GameInstanceRecord,
    manifest: &GameInstanceManifest,
    folder_exists: bool,
) -> InstanceCapabilities {
    InstanceCapabilities {
        launch: definition.launch.is_some(),
        stop: runtime_of(record, manifest).is_some(),
        rename: true,
        clone: true,
        trash: true,
        open_folder: folder_exists,
        play_time: true,
    }
}

fn card_for(
    definition: &GameDefinition,
    record: &GameInstanceRecord,
    manifest: &GameInstanceManifest,
    stats: &PlayStats,
    running: bool,
    folder_exists: bool,
) -> InstanceCard {
    let runtime = runtime_of(record, manifest);
    InstanceCard {
        instance_id: record.instance_id.clone(),
        game_id: record.game.as_str().to_string(),
        name: record.name.clone(),
        store: runtime.as_ref().map(|r| r.store.as_str().to_string()),
        version: runtime.as_ref().map(|r| r.version.clone()),
        pinned: is_pinned(record),
        deployment: manifest.deployment.map(|mode| mode.as_str().to_string()),
        last_launched_at: record.last_launched_at.clone(),
        created_at: record.created_at.clone(),
        last_played_at: stats.last_played_at.clone(),
        play_time_ms: stats.total_play_ms,
        running,
        capabilities: capabilities_for(definition, record, manifest, folder_exists),
    }
}

fn created_for(record: &GameInstanceRecord, name: String, reused: bool) -> CreatedInstance {
    let (base, base_id, unpinned_reason) = match &record.base {
        BaseReference::Pinned { id, .. } => ("pinned", Some(id.clone()), None),
        BaseReference::Unpinned { reason, .. } => ("unpinned", None, Some(reason.clone())),
    };
    CreatedInstance {
        instance_id: record.instance_id.clone(),
        name,
        base,
        reused_base: reused,
        base_id,
        unpinned_reason,
    }
}

/// True when the build found an identical base already on disk and reused it.
fn reused_base(outcome: Option<&BuildOutcome>) -> bool {
    matches!(outcome, Some(BuildOutcome::Existing(_)))
}

/// True only when the session belongs to this instance and the game is still running from it.
/// No session at all means nothing is running.
pub fn running_from_session(session: Option<SessionStatus>, instance_id: &str) -> bool {
    matches!(session, Some(s) if s.running && s.instance == instance_id)
}

/// The three states a check can be in: nothing found, something to review, or a finding that stops
/// the launch.
pub fn status_for(findings: &[CheckFinding]) -> &'static str {
    if findings.iter().any(|f| f.blocks) {
        "findings"
    } else if findings.is_empty() {
        "ok"
    } else {
        "warnings"
    }
}

fn vfs_question(reason: String, next: Option<DeployMode>) -> LaunchReply {
    LaunchReply {
        status: "vfs_unavailable",
        pid: None,
        deployment: None,
        notice: None,
        findings: Vec::new(),
        reason: Some(reason),
        next: next.map(|mode| mode.as_str().to_string()),
        before_launch: Vec::new(),
        before_launch_failure: None,
    }
}

fn refused(findings: Vec<CheckFinding>) -> LaunchReply {
    LaunchReply {
        status: "refused",
        pid: None,
        deployment: None,
        notice: None,
        findings,
        reason: None,
        next: None,
        before_launch: Vec::new(),
        before_launch_failure: None,
    }
}

/// The launch needs these tools first. Nothing was started; the page asks.
fn before_launch_question(tools: Vec<agora_core::game_tools::BeforeLaunchNeed>) -> LaunchReply {
    LaunchReply {
        status: "before_launch_needed",
        pid: None,
        deployment: None,
        notice: None,
        findings: Vec::new(),
        reason: None,
        next: None,
        before_launch: tools
            .into_iter()
            .map(|need| BeforeLaunchNeedView {
                tool: need.tool,
                name: need.name,
                mode: match need.mode {
                    agora_game_api::BeforeLaunchMode::Run => "run".to_string(),
                    agora_game_api::BeforeLaunchMode::Ask => "ask".to_string(),
                },
                reason: need.reason,
            })
            .collect(),
        before_launch_failure: None,
    }
}

/// A before-launch tool ran and did not finish. Nothing was started; the page offers launching
/// anyway, as every protection does.
fn before_launch_failed(tool: String, name: String, message: String) -> LaunchReply {
    LaunchReply {
        status: "before_launch_failed",
        pid: None,
        deployment: None,
        notice: None,
        findings: Vec::new(),
        reason: None,
        next: None,
        before_launch: Vec::new(),
        before_launch_failure: Some(BeforeLaunchFailureView {
            tool,
            name,
            message,
        }),
    }
}

/// A framework a catalog mod in the instance needs. It never stops a launch: the user may have
/// installed the mod past exactly this, so the page keeps saying so instead of asking again.
pub fn requirement_finding(
    finding: &agora_core::catalog_install::RequirementFinding,
) -> CheckFinding {
    use agora_core::catalog_install::RequirementFinding;
    match finding {
        RequirementFinding::Unmet { unmet, .. } => CheckFinding {
            kind: "framework",
            blocks: false,
            title: unmet.name.clone(),
            detail: finding.summary(),
            why: "A mod that is missing what it needs usually does nothing, or stops the game from starting.".to_string(),
            repair: Some(match &unmet.catalog_entry {
                Some(entry) => format!("Install {} from \"Browse\".", entry.name),
                None => format!("Add {} to this instance.", unmet.name),
            }),
        },
        RequirementFinding::NotChecked { .. } => CheckFinding {
            kind: "framework",
            blocks: false,
            title: "Frameworks your mods need".to_string(),
            detail: finding.summary(),
            why: "Agora cannot say whether these mods have what they need.".to_string(),
            repair: None,
        },
    }
}

pub fn framework_finding(finding: &RuntimeFileFinding) -> CheckFinding {
    let blocks = finding.refuses_launch();
    let (detail, why) = match &finding.problem {
        RuntimeFileProblem::WrongVersion => (
            format!(
                "It needs {}, but the game has {}.",
                finding.expected,
                if finding.found.is_empty() {
                    "no file of its family".to_string()
                } else {
                    finding.found.join(", ")
                }
            ),
            "A framework built for another game version often stops the game from starting, or makes it crash."
                .to_string(),
        ),
        RuntimeFileProblem::CannotCheck { reason } => (
            format!("This rule cannot be checked: {reason}."),
            "Agora could not compare this framework with the game, so it cannot say whether they match.".to_string(),
        ),
    };
    CheckFinding {
        kind: "framework",
        blocks,
        title: finding.rule_name.clone(),
        detail,
        why,
        repair: non_empty(&finding.repair),
    }
}

pub fn load_order_finding(finding: &agora_core::game_load_order::Finding) -> CheckFinding {
    let blocks = finding.refuses_launch();
    CheckFinding {
        kind: "load_order",
        blocks,
        title: "Plugin load order".to_string(),
        detail: finding.message(),
        why: if blocks {
            "The game refuses plugins that load before the plugins they depend on, and can crash while it starts."
                .to_string()
        } else {
            "The game may still load these plugins, but the order can cause odd behaviour."
                .to_string()
        },
        repair: Some(
            "Open this instance's Plugins tab and choose Sort, then check again.".to_string(),
        ),
    }
}

pub fn tool_finding(output: &agora_core::game_tools::OutputFinding) -> CheckFinding {
    CheckFinding {
        kind: "tool",
        blocks: false,
        title: output.tool.clone(),
        detail: output.message.clone(),
        why: "Tool output made from older files, or that conflicts with another tool, may not match the mods now installed."
            .to_string(),
        repair: Some("Run the tool again from this instance's Tools tab.".to_string()),
    }
}

fn base_finding(problem: &BaseProblem, blocks: bool) -> CheckFinding {
    let linked = if problem.linked_to_store {
        " The store install changed too."
    } else {
        ""
    };
    CheckFinding {
        kind: "base",
        blocks,
        title: "Base file changed".to_string(),
        detail: format!("{}: {}.{linked}", problem.path, problem_text(&problem.kind)),
        why: "The game's files differ from what Agora recorded when it built the base, so the game may behave oddly or crash."
            .to_string(),
        repair: Some("Verify this base on the game's Bases page in Settings, or launch anyway at your own risk.".to_string()),
    }
}

pub fn problem_text(kind: &ProblemKind) -> String {
    match kind {
        ProblemKind::Missing => "the file is missing".to_string(),
        ProblemKind::SizeChanged { expected, actual } => {
            format!("its size changed (expected {expected} bytes, found {actual})")
        }
        ProblemKind::ContentChanged => "its contents changed".to_string(),
        ProblemKind::Unexpected => "an unexpected file is in the base".to_string(),
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `auto` is no choice (the game's default). Anything else must be a deployment name, in any case.
pub fn parse_deployment(word: &str) -> Result<Option<DeployMode>, GamesError> {
    if word.trim().eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    DeployMode::parse(word).map(Some).ok_or_else(|| {
        GamesError::new(
            INVALID_REQUEST,
            format!("'{word}' is not a deployment. Use automatic, virtual, links or copies."),
        )
    })
}

fn parse_base_mode(word: &str) -> Result<BaseMode, GamesError> {
    word.parse::<BaseMode>()
        .map_err(|message| GamesError::new(INVALID_REQUEST, message))
}

fn parse_game_id(game_id: &str) -> Result<GameId, GamesError> {
    if game_id.trim().is_empty() {
        return Err(GamesError::new(INVALID_REQUEST, "No game was named."));
    }
    if game_id == GameId::MINECRAFT {
        return Err(GamesError::new(
            INVALID_REQUEST,
            "Minecraft instances are listed under My Instances, not here.",
        ));
    }
    GameId::new(game_id).map_err(|error| {
        GamesError::new(
            INVALID_REQUEST,
            format!("'{game_id}' is not a game: {error}"),
        )
    })
}

/// An instance name as the user typed it, trimmed. A blank name is refused rather than replaced
/// with the game's name, so the card never shows something the user did not choose.
pub fn clean_name(name: &str) -> Result<String, GamesError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(GamesError::new(
            INVALID_REQUEST,
            "Give the instance a name.",
        ));
    }
    Ok(trimmed.to_string())
}

/// Maps a core refusal to the code the page branches on. Errors that have no page of their own
/// keep the core's message under `core_failed`.
pub fn map_error(error: InstanceError) -> GamesError {
    match error {
        InstanceError::NotFound(id) => not_found(&id),
        InstanceError::Running(id) => running_error(&id),
        InstanceError::NothingRunning(id) => GamesError::new(
            INSTANCE_NOT_RUNNING,
            format!("Nothing is running for '{id}'."),
        ),
        InstanceError::NotStartedByAgora(id) => GamesError::new(
            INSTANCE_NOT_AGORA_PROCESS,
            format!(
                "'{id}' is running, but Agora did not start that process, so it will not stop it. Close the game yourself."
            ),
        ),
        InstanceError::RestoreIdTaken(id) => GamesError::new(
            INSTANCE_ID_TAKEN,
            format!("An instance called '{id}' exists again. Choose a new name to restore it."),
        ),
        InstanceError::AlreadyExists(id) => GamesError::new(
            "instance_exists",
            format!("An instance called '{id}' already exists."),
        ),
        InstanceError::Trash { code, message } => GamesError::new(code, message),
        InstanceError::Deploy(agora_core::game_deploy::DeployError::UnreadableBaseManifest(
            detail,
        )) => GamesError::new(
            INSTANCE_BASE_UNREADABLE,
            format!("Agora cannot read this instance's base record: {detail}"),
        ),
        InstanceError::MinecraftInstance(id) => GamesError::new(
            MINECRAFT_INSTANCE,
            format!("'{id}' is a Minecraft instance. Manage it under My Instances."),
        ),
        InstanceError::RuntimeUnidentified(reasons) => GamesError::new(
            RUNTIME_UNKNOWN,
            format!(
                "Agora does not recognise this game's version, so it cannot pin it: {}.",
                reasons.join(", ")
            ),
        ),
        InstanceError::InstallNotFound(id) => GamesError::new(
            INSTALL_NOT_FOUND,
            format!("The install '{id}' is no longer on this machine."),
        ),
        other => GamesError::new(CORE_FAILED, other.to_string()),
    }
}

fn core_failed(error: impl std::fmt::Display) -> GamesError {
    GamesError::new(CORE_FAILED, error.to_string())
}

fn not_found(instance_id: &str) -> GamesError {
    GamesError::new(
        INSTANCE_NOT_FOUND,
        format!("No instance called '{instance_id}' was found. It may have been deleted."),
    )
}

fn running_error(instance_id: &str) -> GamesError {
    GamesError::new(
        INSTANCE_RUNNING,
        format!("'{instance_id}' is running. Close the game first, then try again."),
    )
}

fn game_missing(game_id: &str) -> GamesError {
    GamesError::new(
        CORE_FAILED,
        format!("Agora has no game definition for '{game_id}', so this instance cannot be used."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::game_load_order::Finding;

    #[test]
    fn an_unmet_mod_framework_is_reported_and_does_not_stop_a_launch() {
        use agora_core::catalog_install::{
            CatalogEntryRef, FrameworkProblem, RequirementFinding, UnmetFramework,
        };
        let unmet = RequirementFinding::Unmet {
            needed_by: vec!["CrashLogger".into()],
            unmet: UnmetFramework {
                framework: "skse".into(),
                name: "Skyrim Script Extender".into(),
                min_version: None,
                problem: FrameworkProblem::Absent,
                catalog_entry: Some(CatalogEntryRef {
                    id: "skse64".into(),
                    name: "SKSE64".into(),
                }),
                message: String::new(),
            },
        };
        let finding = requirement_finding(&unmet);
        assert_eq!(finding.kind, "framework");
        assert!(!finding.blocks);
        assert_eq!(finding.title, "Skyrim Script Extender");
        assert!(finding
            .detail
            .starts_with("CrashLogger needs Skyrim Script Extender"));
        assert_eq!(
            finding.repair.as_deref(),
            Some("Install SKSE64 from \"Browse\".")
        );

        let unchecked = requirement_finding(&RequirementFinding::NotChecked {
            needed_by: vec!["CrashLogger".into()],
            reason: "the catalog is missing".into(),
        });
        assert!(!unchecked.blocks);
        assert!(unchecked.detail.contains("could not check"));
    }
    use agora_core::game_tools::{OutputFinding, OutputStatus};

    fn runtime(version: &str) -> RuntimeIdentity {
        RuntimeIdentity {
            game: GameId::new("skyrim-se").expect("a valid game id"),
            store: StoreId::new("steam").expect("a valid store id"),
            version: version.to_string(),
            build: None,
        }
    }

    fn pinned_record(id: &str, version: &str) -> GameInstanceRecord {
        GameInstanceRecord {
            instance_id: id.to_string(),
            game: GameId::new("skyrim-se").expect("a valid game id"),
            name: "Grounded".to_string(),
            base: BaseReference::Pinned {
                id: "base-1".to_string(),
                runtime: runtime(version),
                mode: BaseMode::Linked,
            },
            created_at: "2026-10-01 10:00:00".to_string(),
            last_launched_at: None,
            build_outcome: None,
        }
    }

    fn manifest_for(
        record: &GameInstanceRecord,
        deployment: Option<DeployMode>,
    ) -> GameInstanceManifest {
        let mut manifest = GameInstanceManifest::new(
            record.game.clone(),
            record.instance_id.clone(),
            record.name.clone(),
            Some(runtime("1.6.1170")),
            record.base.clone(),
        );
        manifest.deployment = deployment;
        manifest
    }

    fn definition_with_launch(has_launch: bool) -> GameDefinition {
        let launch = if has_launch {
            serde_json::json!({
                "executable": { "root": "runtime", "path": "" },
                "arguments": [],
                "environment": {},
                "working_directory": { "root": "runtime", "path": "" }
            })
        } else {
            serde_json::Value::Null
        };
        serde_json::from_value(serde_json::json!({
            "id": "skyrim-se",
            "name": "Skyrim Special Edition",
            "stores": [],
            "version_sources": [],
            "deployment": "virtual_file_system",
            "content_rules": [],
            "framework_ids": [],
            "tool_ids": [],
            "launch": launch,
            "log_paths": [],
            "crash_paths": [],
            "user_files": [],
            "save_paths": [],
            "linked_archive_patterns": []
        }))
        .expect("a game definition the core reads")
    }

    fn stats() -> PlayStats {
        PlayStats {
            total_play_ms: 7_200_000,
            last_played_at: Some("2026-10-02 09:00:00".into()),
        }
    }

    fn blocking_finding() -> CheckFinding {
        CheckFinding {
            kind: "framework",
            blocks: true,
            title: "t".into(),
            detail: "d".into(),
            why: "w".into(),
            repair: None,
        }
    }

    fn warning_finding() -> CheckFinding {
        CheckFinding {
            blocks: false,
            ..blocking_finding()
        }
    }

    fn wrong_version(found: Vec<&str>) -> RuntimeFileFinding {
        RuntimeFileFinding {
            rule_id: "skse".into(),
            rule_name: "Script extender".into(),
            expected: "skse64_1_6_1170.dll".into(),
            found: found.into_iter().map(str::to_string).collect(),
            repair: "Install the matching extender.".into(),
            problem: RuntimeFileProblem::WrongVersion,
        }
    }

    #[test]
    fn deployment_words_parse_in_any_case_and_auto_means_no_choice() {
        assert_eq!(parse_deployment("auto").expect("auto parses"), None);
        assert_eq!(parse_deployment(" Auto ").expect("auto parses"), None);
        assert_eq!(
            parse_deployment(" Virtual ").expect("virtual parses"),
            Some(DeployMode::Virtual)
        );
        assert_eq!(
            parse_deployment("LINKS").expect("links parses"),
            Some(DeployMode::Links)
        );
        assert_eq!(
            parse_deployment("copies").expect("copies parses"),
            Some(DeployMode::Copies)
        );
    }

    #[test]
    fn hostile_deployment_words_are_refused_with_a_reason() {
        for word in [
            "",
            "   ",
            "vfs",
            "virtual;links",
            "auto virtual",
            "copies\u{0}",
            "../links",
        ] {
            let error = parse_deployment(word).expect_err("a word that is not a mode is refused");
            assert_eq!(error.code, INVALID_REQUEST, "for {word:?}");
            assert!(
                error
                    .message
                    .contains("automatic, virtual, links or copies"),
                "for {word:?}"
            );
        }
    }

    #[test]
    fn base_mode_accepts_linked_and_copied_only() {
        assert_eq!(
            parse_base_mode("Linked").expect("linked parses"),
            BaseMode::Linked
        );
        assert_eq!(
            parse_base_mode("copied").expect("copied parses"),
            BaseMode::Copied
        );
        for word in ["", "both", "linked copied", "link"] {
            let error = parse_base_mode(word).expect_err("an unknown base mode is refused");
            assert_eq!(error.code, INVALID_REQUEST, "for {word:?}");
        }
    }

    #[test]
    fn a_name_is_trimmed_and_a_blank_one_is_refused() {
        assert_eq!(clean_name("  My Skyrim  ").expect("a name"), "My Skyrim");
        assert_eq!(
            clean_name("Skyrim ✨").expect("a name with emoji"),
            "Skyrim ✨"
        );
        for blank in ["", "   ", "\t\n", "\u{00a0}"] {
            let error = clean_name(blank).expect_err("a blank name is refused");
            assert_eq!(error.code, INVALID_REQUEST, "for {blank:?}");
        }
    }

    #[test]
    fn game_ids_refuse_blank_and_minecraft() {
        assert_eq!(
            parse_game_id("skyrim-se").expect("a game").as_str(),
            "skyrim-se"
        );
        for bad in ["", "   ", "minecraft"] {
            let error = parse_game_id(bad).expect_err("not a generic game");
            assert_eq!(error.code, INVALID_REQUEST, "for {bad:?}");
        }
    }

    #[test]
    fn core_refusals_keep_the_codes_the_page_branches_on() {
        let not_found_error = map_error(InstanceError::NotFound("grounded-1".into()));
        assert_eq!(not_found_error.code, INSTANCE_NOT_FOUND);
        assert!(not_found_error.message.contains("grounded-1"));

        let minecraft = map_error(InstanceError::MinecraftInstance("mc-1".into()));
        assert_eq!(minecraft.code, MINECRAFT_INSTANCE);

        let unknown = map_error(InstanceError::RuntimeUnidentified(vec![
            "no version file".into()
        ]));
        assert_eq!(unknown.code, RUNTIME_UNKNOWN);
        assert!(unknown.message.contains("no version file"));

        let other = map_error(InstanceError::Other("disk full".into()));
        assert_eq!(other.code, CORE_FAILED);
        assert!(other.message.contains("disk full"));
    }

    #[test]
    fn a_check_is_findings_only_when_a_finding_blocks() {
        assert_eq!(status_for(&[]), "ok");
        assert_eq!(status_for(&[warning_finding()]), "warnings");
        assert_eq!(
            status_for(&[warning_finding(), blocking_finding()]),
            "findings"
        );
    }

    #[test]
    fn a_framework_built_for_another_version_blocks_and_says_what_it_found() {
        let blocking = framework_finding(&wrong_version(vec!["skse64_1_5_97.dll"]));
        assert_eq!(blocking.kind, "framework");
        assert!(blocking.blocks);
        assert!(blocking.detail.contains("skse64_1_6_1170.dll"));
        assert!(blocking.detail.contains("skse64_1_5_97.dll"));
        assert_eq!(
            blocking.repair.as_deref(),
            Some("Install the matching extender.")
        );

        let empty = framework_finding(&wrong_version(Vec::new()));
        assert!(empty.detail.contains("no file of its family"));
    }

    #[test]
    fn a_rule_that_cannot_be_checked_only_warns() {
        let finding = RuntimeFileFinding {
            problem: RuntimeFileProblem::CannotCheck {
                reason: "bad template".into(),
            },
            repair: "   ".into(),
            ..wrong_version(Vec::new())
        };
        let shown = framework_finding(&finding);
        assert!(
            !shown.blocks,
            "a definition bug is a warning, not a refusal"
        );
        assert!(shown.detail.contains("bad template"));
        assert_eq!(shown.repair, None, "a blank repair is no repair");
    }

    #[test]
    fn only_refusing_plugin_findings_block() {
        let cycle = load_order_finding(&Finding::MasterCycle {
            plugins: vec!["a.esm".into(), "b.esm".into()],
        });
        assert!(cycle.blocks);
        let unreadable = load_order_finding(&Finding::ListLevel {
            message: "'c.esp' has a header that cannot be read: truncated".into(),
            items: vec!["c.esp".into()],
            refuses: false,
        });
        assert!(
            !unreadable.blocks,
            "a header that cannot be read is a warning"
        );
        assert_eq!(unreadable.kind, "load_order");
    }

    #[test]
    fn tool_output_never_blocks_a_launch() {
        let stale = tool_finding(&OutputFinding {
            tool: "Nemesis".into(),
            generation: "gen-3".into(),
            status: OutputStatus::Stale,
            message: "Nemesis output is older than the mods".into(),
        });
        assert!(!stale.blocks);
        assert_eq!(stale.kind, "tool");
        assert_eq!(stale.title, "Nemesis");
    }

    #[test]
    fn a_session_only_counts_as_running_for_its_own_instance() {
        let session = |instance: &str, running: bool| SessionStatus {
            instance: instance.to_string(),
            running,
            files: Vec::new(),
        };
        assert!(
            !running_from_session(None, "grounded-1"),
            "no session is not running"
        );
        assert!(!running_from_session(
            Some(session("other", true)),
            "grounded-1"
        ));
        assert!(!running_from_session(
            Some(session("grounded-1", false)),
            "grounded-1"
        ));
        assert!(running_from_session(
            Some(session("grounded-1", true)),
            "grounded-1"
        ));
    }

    #[test]
    fn every_base_problem_kind_has_its_own_plain_words() {
        let texts = [
            problem_text(&ProblemKind::Missing),
            problem_text(&ProblemKind::SizeChanged {
                expected: 10,
                actual: 12,
            }),
            problem_text(&ProblemKind::ContentChanged),
            problem_text(&ProblemKind::Unexpected),
        ];
        for (i, text) in texts.iter().enumerate() {
            assert!(!text.is_empty());
            for other in &texts[i + 1..] {
                assert_ne!(text, other);
            }
        }
        assert!(problem_text(&ProblemKind::SizeChanged {
            expected: 10,
            actual: 12
        })
        .contains("expected 10"));
    }

    #[test]
    fn a_card_shows_the_pinned_runtime_and_automatic_deployment() {
        let record = pinned_record("grounded-1", "1.6.1170");
        let manifest = manifest_for(&record, None);
        let definition = definition_with_launch(true);
        let card = card_for(&definition, &record, &manifest, &stats(), false, true);
        assert!(card.pinned);
        assert_eq!(card.store.as_deref(), Some("steam"));
        assert_eq!(card.version.as_deref(), Some("1.6.1170"));
        assert_eq!(card.deployment, None, "no choice recorded means automatic");
        assert!(!card.running);
        assert_eq!(card.play_time_ms, 7_200_000);
        assert_eq!(card.last_played_at.as_deref(), Some("2026-10-02 09:00:00"));

        let chosen = card_for(
            &definition,
            &record,
            &manifest_for(&record, Some(DeployMode::Links)),
            &stats(),
            true,
            true,
        );
        assert_eq!(chosen.deployment.as_deref(), Some("links"));
        assert!(chosen.running);
    }

    #[test]
    fn capabilities_follow_the_game_and_this_instance() {
        let record = pinned_record("grounded-1", "1.6.1170");
        let manifest = manifest_for(&record, None);

        let full = capabilities_for(&definition_with_launch(true), &record, &manifest, true);
        assert!(full.launch && full.stop && full.rename && full.clone && full.trash);
        assert!(full.open_folder && full.play_time);

        // A game with no launch recipe cannot be launched; a missing folder cannot be opened.
        let no_launch = capabilities_for(&definition_with_launch(false), &record, &manifest, false);
        assert!(!no_launch.launch);
        assert!(!no_launch.open_folder);
        assert!(no_launch.clone, "cloning does not need a launch recipe");

        // No store is known, so there is no session to find and nothing to stop.
        let mut bare = record.clone();
        bare.base = BaseReference::Unpinned {
            install: agora_game_api::InstallId::new("steam:489830").expect("an install id"),
            reason: "test".into(),
        };
        let mut no_runtime = manifest.clone();
        no_runtime.runtime_identity = None;
        let unknown_store =
            capabilities_for(&definition_with_launch(true), &bare, &no_runtime, true);
        assert!(!unknown_store.stop);
    }

    #[test]
    fn a_reused_base_is_told_apart_from_a_fresh_build() {
        assert!(!reused_base(None), "an unpinned instance built no base");
        let manifest: agora_core::game_base::BaseManifest = serde_json::from_value(serde_json::json!({
            "manifest_version": 1,
            "base_id": "skyrim-se_steam_1.6.1170",
            "runtime": { "game": "skyrim-se", "store": "steam", "version": "1.6.1170", "build": null },
            "mode": "linked",
            "source_install": "steam-489830",
            "source_location": "D:/Games/Skyrim Special Edition",
            "location": "D:/Agora/bases/skyrim-se",
            "created_unix_ms": 0,
            "files": [],
            "skipped": []
        }))
        .expect("a base manifest in the shape the core writes");
        assert!(reused_base(Some(&BuildOutcome::Existing(manifest.clone()))));
        assert!(!reused_base(Some(&BuildOutcome::Built {
            manifest,
            linked_bytes: 0,
            copied_bytes: 0,
        })));
    }

    #[test]
    fn the_exit_event_uses_the_page_field_names() {
        let value = serde_json::to_value(InstanceExited {
            instance_id: "grounded-1".into(),
            restore_error: Some("hash mismatch".into()),
        })
        .expect("the event serializes");
        assert_eq!(value["instanceId"], "grounded-1");
        assert_eq!(value["restoreError"], "hash mismatch");
    }

    /// The page (`features/games/instances/api.ts`) reads these names. A renamed field fails here
    /// before it becomes a page that quietly shows nothing.
    #[test]
    fn the_replies_use_the_names_the_page_reads() {
        let record = pinned_record("grounded-1", "1.6.1170");
        let definition = definition_with_launch(true);
        let card = serde_json::to_value(card_for(
            &definition,
            &record,
            &manifest_for(&record, None),
            &stats(),
            false,
            true,
        ))
        .expect("a card serializes");
        for key in [
            "instanceId",
            "gameId",
            "name",
            "store",
            "version",
            "pinned",
            "deployment",
            "lastLaunchedAt",
            "createdAt",
            "lastPlayedAt",
            "playTimeMs",
            "running",
            "capabilities",
        ] {
            assert!(card.get(key).is_some(), "card is missing {key}");
        }
        for key in [
            "launch",
            "stop",
            "rename",
            "clone",
            "trash",
            "openFolder",
            "playTime",
        ] {
            assert!(
                card["capabilities"].get(key).is_some(),
                "capabilities are missing {key}"
            );
        }

        let finding = serde_json::to_value(framework_finding(&wrong_version(Vec::new())))
            .expect("a finding serializes");
        for key in ["kind", "blocks", "title", "detail", "why", "repair"] {
            assert!(finding.get(key).is_some(), "finding is missing {key}");
        }

        let reply = serde_json::to_value(refused(Vec::new())).expect("a reply serializes");
        for key in [
            "status",
            "pid",
            "deployment",
            "notice",
            "findings",
            "reason",
            "next",
        ] {
            assert!(reply.get(key).is_some(), "launch reply is missing {key}");
        }
        assert_eq!(reply["status"], "refused");
    }

    #[test]
    fn a_refused_launch_names_what_stopped_it() {
        let reply = refused(vec![framework_finding(&wrong_version(Vec::new()))]);
        assert_eq!(reply.status, "refused");
        assert_eq!(reply.pid, None);
        assert_eq!(status_for(&reply.findings), "findings");

        let base = base_finding(
            &BaseProblem {
                path: "Data/Skyrim.esm".into(),
                kind: ProblemKind::ContentChanged,
                linked_to_store: true,
            },
            true,
        );
        assert!(base.blocks);
        assert!(base.detail.contains("Data/Skyrim.esm"));
        assert!(base.detail.contains("store install changed too"));
    }

    #[test]
    fn a_vfs_failure_asks_and_names_the_next_rung_without_starting_anything() {
        let reply = vfs_question("the driver is missing".into(), Some(DeployMode::Links));
        assert_eq!(reply.status, "vfs_unavailable");
        assert_eq!(reply.pid, None);
        assert_eq!(reply.next.as_deref(), Some("links"));
        assert_eq!(reply.reason.as_deref(), Some("the driver is missing"));
        assert!(reply.findings.is_empty());
    }
}
