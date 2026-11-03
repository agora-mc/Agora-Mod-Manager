//! A game instance's own image ("Set image" on its page), the same picture system Minecraft
//! instances have. Thin adapters over `agora_core::icon`, which keeps the image in the instance's
//! Agora-owned folder and applies Minecraft's type and size limits.

use super::error::{GamesError, TASK_FAILED};

/// Copy a picked image in as the instance's image; answers it as a data URL.
#[tauri::command]
pub async fn games_instance_set_icon(
    app: tauri::AppHandle,
    instance_id: String,
    source_path: String,
) -> Result<String, GamesError> {
    let ctx = crate::core_context(&app)?;
    tokio::task::spawn_blocking(move || {
        agora_core::icon::set_game_instance_icon(
            &ctx,
            &instance_id,
            std::path::Path::new(&source_path),
        )
        .map_err(GamesError::from)
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, "The image was not saved."))?
}

/// The instance's image as a data URL, or `null` when it has none (its tile is generated).
#[tauri::command]
pub async fn games_instance_icon(
    app: tauri::AppHandle,
    instance_id: String,
) -> Result<Option<String>, GamesError> {
    let ctx = crate::core_context(&app)?;
    tokio::task::spawn_blocking(move || {
        agora_core::icon::get_game_instance_icon(&ctx, &instance_id).map_err(GamesError::from)
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, "The image could not be read."))?
}
