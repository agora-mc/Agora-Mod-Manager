//! Game-instance actions the CLI had and the desktop did not: making an instance the one Steam
//! starts, deploying and undeploying by hand, deploying a content item from its own copies, and
//! deleting a tool swap's kept Data copy. Each is a thin wrapper over the core function the CLI
//! calls (`scripts/cli_coverage.json` keeps the two in step).

use super::error::{GamesError, CORE_FAILED, TASK_FAILED};
use super::instances::{map_error, parse_deployment};
use agora_core::{game_deploy, game_instance, game_tool_copy, game_wrap};
use serde::Serialize;

async fn blocking<T, F>(work: F) -> Result<T, GamesError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, GamesError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| GamesError::new(TASK_FAILED, "The instance work did not finish."))?
}

fn core_error(error: impl std::fmt::Display) -> GamesError {
    GamesError::new(CORE_FAILED, error.to_string())
}

/// The record and the game definition an action needs, or a plain error.
fn instance_game(
    ctx: &agora_core::ctx::Ctx,
    instance_id: &str,
) -> Result<agora_game_api::GameDefinition, GamesError> {
    let record = game_instance::get(ctx, instance_id)
        .map_err(map_error)?
        .ok_or_else(|| core_error(format!("Instance '{instance_id}' was not found.")))?;
    ctx.games.game(&record.game).cloned().ok_or_else(|| {
        core_error(format!(
            "Agora has no game definition for '{}', so this instance cannot be used.",
            record.game
        ))
    })
}

/// Whether this instance is the one `agora games wrap` starts when Steam launches its game
/// without a pending launch.
#[tauri::command]
pub async fn games_instance_is_default(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<bool, GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        let record = game_instance::get(&ctx, &instance_id)
            .map_err(map_error)?
            .ok_or_else(|| core_error(format!("Instance '{instance_id}' was not found.")))?;
        Ok(
            game_wrap::default_instance(&ctx, &record.game).as_deref()
                == Some(instance_id.as_str()),
        )
    })
    .await
}

/// Make this instance the one Steam starts for its game (`agora games instance default`).
#[tauri::command]
pub async fn games_instance_set_default(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<(), GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        game_wrap::set_default_instance(&ctx, &instance_id).map_err(core_error)
    })
    .await
}

/// What a deploy did, for the page to say.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployedInstance {
    /// The deployment used: virtual, links or copies.
    pub mode: String,
    pub outcome: game_deploy::DeployOutcome,
}

/// Deploy the instance's game folder now, without launching (`agora games instance deploy`).
/// `mode` is automatic, virtual, links or copies; automatic uses the mode a launch would.
#[tauri::command]
pub async fn games_instance_deploy(
    app: tauri::AppHandle,
    instance_id: String,
    mode: Option<String>,
) -> Result<DeployedInstance, GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        let definition = instance_game(&ctx, &instance_id)?;
        let requested = match mode.as_deref() {
            Some(word) => parse_deployment(word)?,
            None => None,
        };
        let mode = game_instance::deploy_mode_for(
            &ctx,
            &instance_id,
            &definition,
            requested,
            &agora_core::game_launch::SystemLauncher,
        )
        .map_err(core_error)?;
        agora_core::instance_runtime::ensure_game_not_running(&ctx, &instance_id, "redeploying it")
            .map_err(core_error)?;
        let outcome =
            game_deploy::deploy(&ctx, &instance_id, &definition, mode).map_err(core_error)?;
        Ok(DeployedInstance {
            mode: mode.to_string(),
            outcome,
        })
    })
    .await
}

/// Take the deployed game folder down, keeping what the game wrote (`agora games instance
/// undeploy`).
#[tauri::command]
pub async fn games_instance_undeploy(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<game_deploy::HarvestReport, GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        game_deploy::undeploy(&ctx, &instance_id).map_err(core_error)
    })
    .await
}

/// Deploy one content item from the instance's own copies of its files, or go back to shared
/// ones (`agora games instance content own-copy`).
#[tauri::command]
pub async fn games_instance_content_set_own_copy(
    app: tauri::AppHandle,
    instance_id: String,
    item_id: String,
    own_copy: bool,
) -> Result<(), GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        game_deploy::set_content_own_copy(&ctx, &instance_id, &item_id, own_copy)
            .map_err(core_error)
    })
    .await
}

/// Delete the instance's kept copy of its Data folder (`agora games tools-swap remove-copy`).
/// Resolves `true` when a copy was deleted, `false` when there was none.
#[tauri::command]
pub async fn games_tools_swap_remove_copy(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<bool, GamesError> {
    blocking(move || {
        let ctx = crate::core_context(&app)?;
        let definition = instance_game(&ctx, &instance_id)?;
        let removed = game_tool_copy::remove_kept_copy(&ctx, &definition, &instance_id)
            .map_err(core_error)?;
        Ok(matches!(removed, game_tool_copy::RemoveCopy::Removed))
    })
    .await
}
