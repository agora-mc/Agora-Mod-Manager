//! The Game support list in Settings: which games Agora offers, and turning one on or off.
//!
//! Thin adapters over `agora_core::game_support`. Turning a game off hides it from the game
//! switcher and never touches its instances, bases or files.

use agora_core::error::LauncherError;
use agora_core::game_support::{self, GameSupport};

use super::error::{GamesError, TASK_FAILED};

/// Every game Agora knows (built in or from a plugin), with whether the user has it on.
#[tauri::command]
pub async fn games_support_list(app: tauri::AppHandle) -> Result<Vec<GameSupport>, GamesError> {
    tokio::task::spawn_blocking(move || -> Result<Vec<GameSupport>, GamesError> {
        let ctx = crate::core_context(&app)?;
        Ok(game_support::list(&ctx).map_err(LauncherError::from)?)
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, "Reading the game support list did not finish."))?
}

/// Turn a game on or off. Core refuses an id that is not exactly a known game, and refuses to
/// turn Minecraft off; a refusal changes nothing.
#[tauri::command]
pub async fn games_support_set_enabled(
    app: tauri::AppHandle,
    game_id: String,
    enabled: bool,
) -> Result<(), GamesError> {
    tokio::task::spawn_blocking(move || -> Result<(), GamesError> {
        let ctx = crate::core_context(&app)?;
        game_support::set_enabled(&ctx, &game_id, enabled).map_err(LauncherError::from)?;
        Ok(())
    })
    .await
    .map_err(|_| {
        GamesError::new(
            TASK_FAILED,
            "Saving the game support choice did not finish.",
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::super::error::CORE_FAILED;
    use super::*;

    #[test]
    fn a_refused_game_id_reaches_the_screen_with_its_core_code() {
        let error = LauncherError::from(game_support::GameSupportError::UnknownGame {
            game_id: " valheim".to_string(),
        });
        let mapped = GamesError::from(error);
        assert_eq!(mapped.code, CORE_FAILED);
        assert_eq!(
            mapped.core_code.as_deref(),
            Some(game_support::ERR_GAME_SUPPORT_UNKNOWN_GAME)
        );
        assert!(mapped.message.contains("' valheim'"), "{}", mapped.message);
    }

    #[test]
    fn a_damaged_list_reaches_the_screen_with_its_core_code() {
        let mapped = GamesError::from(LauncherError::from(
            game_support::GameSupportError::Malformed,
        ));
        assert_eq!(
            mapped.core_code.as_deref(),
            Some(game_support::ERR_GAME_SUPPORT_MALFORMED)
        );
    }
}
