//! Which games Agora offers (the Game support list in Settings).
//!
//! Every game the registry knows is listed: the built-in packages and any game a plugin adds. The
//! user can turn a game off, and an off game is not offered in the desktop game switcher. Turning
//! a game off never touches its instances, bases or files; it only hides the game.
//!
//! The choice is stored as the setting [`DISABLED_GAMES_SETTING`]: a JSON array of game ids. A
//! missing setting means nothing is disabled. A stored value that is not an array of strings is an
//! error, never read as "nothing disabled": a damaged list must not quietly switch games back on.
//! An id for a game that is not registered (a plugin that was removed) is kept and ignored, so the
//! choice comes back if that plugin is installed again.
//!
//! Minecraft cannot be turned off yet. The whole app falls back to Minecraft when the active game
//! is unavailable, so turning Minecraft off needs its own design.

use serde::Serialize;

use crate::ctx::CoreContext;
use crate::db;
use crate::error::LauncherError;
use crate::game_registry::PackageSource;

/// The setting that holds the ids of the games the user has turned off.
pub const DISABLED_GAMES_SETTING: &str = "disabled_games";

/// The stored setting is not an array of game ids, so the list cannot be trusted.
pub const ERR_GAME_SUPPORT_MALFORMED: &str = "ERR_GAME_SUPPORT_MALFORMED";
/// No registered game has exactly this id.
pub const ERR_GAME_SUPPORT_UNKNOWN_GAME: &str = "ERR_GAME_SUPPORT_UNKNOWN_GAME";
/// The game cannot be turned off in this version.
pub const ERR_GAME_SUPPORT_CANNOT_DISABLE: &str = "ERR_GAME_SUPPORT_CANNOT_DISABLE";
/// A registered game has no package behind it. Should not happen; refused rather than guessed.
pub const ERR_GAME_SUPPORT_NO_PACKAGE: &str = "ERR_GAME_SUPPORT_NO_PACKAGE";
/// The local state database could not be read or written.
pub const ERR_GAME_SUPPORT_STORAGE: &str = "ERR_GAME_SUPPORT_STORAGE";

#[derive(Debug, thiserror::Error)]
pub enum GameSupportError {
    #[error(
        "Your saved list of turned-off games is damaged. Agora will not guess which games are off."
    )]
    Malformed,
    #[error("No game is known as '{game_id}'.")]
    UnknownGame { game_id: String },
    #[error("Minecraft cannot be turned off yet.")]
    CannotDisable { game_id: String },
    #[error("Game '{game_id}' has no package behind it.")]
    NoPackage { game_id: String },
    #[error("Agora could not read or save the game support list: {message}")]
    Storage { message: String },
}

impl GameSupportError {
    /// The stable `ERR_...` code for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed => ERR_GAME_SUPPORT_MALFORMED,
            Self::UnknownGame { .. } => ERR_GAME_SUPPORT_UNKNOWN_GAME,
            Self::CannotDisable { .. } => ERR_GAME_SUPPORT_CANNOT_DISABLE,
            Self::NoPackage { .. } => ERR_GAME_SUPPORT_NO_PACKAGE,
            Self::Storage { .. } => ERR_GAME_SUPPORT_STORAGE,
        }
    }
}

impl From<GameSupportError> for LauncherError {
    fn from(error: GameSupportError) -> Self {
        LauncherError::Generic {
            code: error.code().to_string(),
            message: error.to_string(),
        }
    }
}

/// Where a game comes from: built into Agora, or added by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GameSource {
    BuiltIn,
    Plugin {
        #[serde(rename = "pluginId")]
        plugin_id: String,
    },
}

/// One registered game, as the Game support list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSupport {
    pub game_id: String,
    pub name: String,
    pub source: GameSource,
    pub enabled: bool,
    /// False for a game the user cannot turn off in this version (Minecraft). Such a game is
    /// always enabled.
    pub can_disable: bool,
}

/// Every registered game, in the registry's order (by game id), with whether it is on.
pub fn list(ctx: &CoreContext) -> Result<Vec<GameSupport>, GameSupportError> {
    let disabled = stored_disabled(ctx)?;
    ctx.games
        .games()
        .map(|game| {
            let source = match ctx.games.source_for(&game.id) {
                Some(PackageSource::Compiled { .. }) => GameSource::BuiltIn,
                Some(PackageSource::Plugin { plugin_id }) => GameSource::Plugin {
                    plugin_id: plugin_id.clone(),
                },
                None => {
                    return Err(GameSupportError::NoPackage {
                        game_id: game.id.to_string(),
                    })
                }
            };
            let can_disable = !game.id.is_minecraft();
            let enabled = !can_disable || !disabled.iter().any(|id| id == game.id.as_str());
            Ok(GameSupport {
                game_id: game.id.to_string(),
                name: game.name.clone(),
                source,
                enabled,
                can_disable,
            })
        })
        .collect()
}

/// Turn a game on or off. The id must be exactly a registered game's id. Idempotent: setting a
/// game to the state it already has leaves the same set of stored ids.
pub fn set_enabled(
    ctx: &CoreContext,
    game_id: &str,
    enabled: bool,
) -> Result<(), GameSupportError> {
    let game = ctx
        .games
        .games()
        .find(|game| game.id.as_str() == game_id)
        .ok_or_else(|| GameSupportError::UnknownGame {
            game_id: game_id.to_string(),
        })?;
    if !enabled && game.id.is_minecraft() {
        return Err(GameSupportError::CannotDisable {
            game_id: game_id.to_string(),
        });
    }

    let mut disabled = stored_disabled(ctx)?;
    disabled.retain(|id| id != game_id);
    if !enabled {
        disabled.push(game_id.to_string());
    }

    let conn = open_state(ctx)?;
    db::set_setting(
        &conn,
        DISABLED_GAMES_SETTING,
        &serde_json::Value::Array(
            disabled
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    )
    .map_err(storage)
}

/// The ids stored as turned off. A missing setting is an empty list; anything that is not an
/// array of strings is [`GameSupportError::Malformed`].
fn stored_disabled(ctx: &CoreContext) -> Result<Vec<String>, GameSupportError> {
    let conn = open_state(ctx)?;
    let Some(value) = db::get_setting(&conn, DISABLED_GAMES_SETTING).map_err(storage)? else {
        return Ok(Vec::new());
    };
    let serde_json::Value::Array(items) = value else {
        return Err(GameSupportError::Malformed);
    };
    items
        .into_iter()
        .map(|item| match item {
            serde_json::Value::String(id) => Ok(id),
            _ => Err(GameSupportError::Malformed),
        })
        .collect()
}

fn open_state(ctx: &CoreContext) -> Result<rusqlite::Connection, GameSupportError> {
    db::local_state_connection(&ctx.paths.local_state_db()).map_err(storage)
}

fn storage(error: impl std::fmt::Display) -> GameSupportError {
    GameSupportError::Storage {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_paths::AppPaths;
    use crate::game_registry::{test_support, GameRegistry};
    use serde_json::json;

    /// A context whose registry holds the named games, each from the given source.
    fn context(root: &std::path::Path, games: &[(PackageSource, &str)]) -> CoreContext {
        let mut builder = GameRegistry::builder();
        for (source, game_id) in games {
            builder
                .add(source.clone(), test_support::package(game_id))
                .expect("a test game registers");
        }
        let (ctx, _) = CoreContext::initialize(AppPaths::from_root(root.to_path_buf()), builder)
            .expect("the context initializes");
        ctx
    }

    fn compiled(game_id: &str) -> (PackageSource, &str) {
        (
            PackageSource::Compiled {
                crate_name: "agora-test".to_string(),
            },
            game_id,
        )
    }

    fn plugin<'a>(game_id: &'a str, plugin_id: &str) -> (PackageSource, &'a str) {
        (
            PackageSource::Plugin {
                plugin_id: plugin_id.to_string(),
            },
            game_id,
        )
    }

    /// Minecraft, Skyrim and a plugin's Valheim: the usual mix.
    fn standard(root: &std::path::Path) -> CoreContext {
        context(
            root,
            &[
                compiled("minecraft"),
                compiled("skyrim-se"),
                plugin("valheim", "agora-tracer.valheim"),
            ],
        )
    }

    fn stored(ctx: &CoreContext) -> Option<serde_json::Value> {
        let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        db::get_setting(&conn, DISABLED_GAMES_SETTING).unwrap()
    }

    fn store(ctx: &CoreContext, value: &serde_json::Value) {
        let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        db::set_setting(&conn, DISABLED_GAMES_SETTING, value).unwrap();
    }

    fn store_text(ctx: &CoreContext, text: &str) {
        let conn = db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
        conn.execute(
            "INSERT INTO user_settings (key, value_json) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
            rusqlite::params![DISABLED_GAMES_SETTING, text],
        )
        .unwrap();
    }

    fn row<'a>(rows: &'a [GameSupport], game_id: &str) -> &'a GameSupport {
        rows.iter()
            .find(|row| row.game_id == game_id)
            .unwrap_or_else(|| panic!("no row for {game_id}"))
    }

    #[test]
    fn lists_every_registered_game_in_id_order_with_its_source() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        let rows = list(&ctx).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.game_id.as_str()).collect();
        assert_eq!(ids, ["minecraft", "skyrim-se", "valheim"]);
        assert_eq!(rows[0].source, GameSource::BuiltIn);
        assert_eq!(rows[1].source, GameSource::BuiltIn);
        assert_eq!(
            rows[2].source,
            GameSource::Plugin {
                plugin_id: "agora-tracer.valheim".to_string()
            }
        );
        assert!(rows.iter().all(|r| r.enabled));
        assert!(!row(&rows, "minecraft").can_disable);
        assert!(row(&rows, "skyrim-se").can_disable);
        assert!(row(&rows, "valheim").can_disable);
    }

    #[test]
    fn the_rows_serialise_with_the_source_shape_the_screen_reads() {
        let root = tempfile::tempdir().unwrap();
        let rows = list(&standard(root.path())).unwrap();
        let built_in = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(built_in["source"], json!({ "kind": "builtIn" }));
        assert_eq!(built_in["gameId"], "skyrim-se");
        assert_eq!(built_in["canDisable"], true);
        let plugin_row = serde_json::to_value(&rows[2]).unwrap();
        assert_eq!(
            plugin_row["source"],
            json!({ "kind": "plugin", "pluginId": "agora-tracer.valheim" })
        );
    }

    #[test]
    fn a_missing_setting_means_nothing_is_disabled() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        assert_eq!(stored(&ctx), None);
        assert!(list(&ctx).unwrap().iter().all(|r| r.enabled));
    }

    #[test]
    fn disabling_a_game_hides_it_in_the_list_and_stores_its_id() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        set_enabled(&ctx, "valheim", false).unwrap();
        assert!(!row(&list(&ctx).unwrap(), "valheim").enabled);
        assert_eq!(stored(&ctx), Some(json!(["valheim"])));
    }

    #[test]
    fn disabling_twice_stores_the_id_once() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        set_enabled(&ctx, "valheim", false).unwrap();
        set_enabled(&ctx, "valheim", false).unwrap();
        assert_eq!(stored(&ctx), Some(json!(["valheim"])));
        assert!(!row(&list(&ctx).unwrap(), "valheim").enabled);
    }

    #[test]
    fn enabling_removes_the_id_and_enabling_twice_is_harmless() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        set_enabled(&ctx, "valheim", false).unwrap();
        set_enabled(&ctx, "valheim", true).unwrap();
        set_enabled(&ctx, "valheim", true).unwrap();
        assert_eq!(stored(&ctx), Some(json!([])));
        assert!(row(&list(&ctx).unwrap(), "valheim").enabled);
    }

    #[test]
    fn an_id_that_is_not_exactly_a_registered_game_is_refused_and_nothing_changes() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        set_enabled(&ctx, "valheim", false).unwrap();
        let hostile = [
            "",
            " ",
            " valheim",
            "valheim ",
            "valheim\n",
            "Valheim",
            "VALHEIM",
            "valheim2",
            "unknown-game",
        ];
        for id in hostile {
            for enabled in [true, false] {
                let error = set_enabled(&ctx, id, enabled)
                    .expect_err("an id that is not a registered game is refused");
                assert_eq!(error.code(), ERR_GAME_SUPPORT_UNKNOWN_GAME, "id {id:?}");
            }
        }
        assert_eq!(stored(&ctx), Some(json!(["valheim"])));
    }

    #[test]
    fn minecraft_cannot_be_turned_off_but_can_be_turned_on() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        let error = set_enabled(&ctx, "minecraft", false).expect_err("Minecraft is refused");
        assert_eq!(error.code(), ERR_GAME_SUPPORT_CANNOT_DISABLE);
        assert_eq!(stored(&ctx), None, "a refused change stores nothing");
        set_enabled(&ctx, "minecraft", true).expect("turning Minecraft on is allowed");
    }

    #[test]
    fn a_stored_minecraft_id_does_not_switch_minecraft_off() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        store(&ctx, &json!(["minecraft"]));
        let rows = list(&ctx).unwrap();
        let minecraft = row(&rows, "minecraft");
        assert!(minecraft.enabled, "a hand-edited id is still always on");
        assert!(!minecraft.can_disable);
    }

    #[test]
    fn a_stored_value_that_is_not_an_array_of_strings_is_an_error_not_nothing_disabled() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        let damaged = [
            json!("valheim"),
            json!([1, 2]),
            json!(["valheim", 3]),
            json!(null),
            json!({ "valheim": false }),
            json!(true),
        ];
        for value in damaged {
            store(&ctx, &value);
            assert_eq!(
                list(&ctx).expect_err("a damaged list is refused").code(),
                ERR_GAME_SUPPORT_MALFORMED,
                "stored {value}"
            );
            assert_eq!(
                set_enabled(&ctx, "valheim", true)
                    .expect_err("a damaged list is not written over")
                    .code(),
                ERR_GAME_SUPPORT_MALFORMED,
                "stored {value}"
            );
            assert_eq!(stored(&ctx), Some(value.clone()), "nothing was rewritten");
        }
        store_text(&ctx, "not json at all");
        assert_eq!(
            list(&ctx).expect_err("unparsable text is refused").code(),
            ERR_GAME_SUPPORT_MALFORMED
        );
    }

    #[test]
    fn a_stored_id_for_an_unregistered_game_is_ignored_and_kept() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        store(&ctx, &json!(["valheim", "removed-plugin-game"]));
        let rows = list(&ctx).unwrap();
        assert_eq!(rows.len(), 3, "an unregistered id adds no row");
        assert!(!row(&rows, "valheim").enabled);
        set_enabled(&ctx, "skyrim-se", false).unwrap();
        assert_eq!(
            stored(&ctx),
            Some(json!(["valheim", "removed-plugin-game", "skyrim-se"])),
            "the id of a game that is not installed stays, so the choice returns with it"
        );
    }

    #[test]
    fn a_disabled_plugin_game_does_not_disable_its_neighbours() {
        let root = tempfile::tempdir().unwrap();
        let ctx = standard(root.path());
        set_enabled(&ctx, "valheim", false).unwrap();
        let rows = list(&ctx).unwrap();
        assert!(row(&rows, "skyrim-se").enabled);
        assert!(!row(&rows, "valheim").enabled);
    }

    #[test]
    fn errors_keep_their_stable_code_when_they_reach_the_core_error() {
        let error: LauncherError = GameSupportError::Malformed.into();
        assert_eq!(error.code(), ERR_GAME_SUPPORT_MALFORMED);
        let no_package: LauncherError = GameSupportError::NoPackage {
            game_id: "x".to_string(),
        }
        .into();
        assert_eq!(no_package.code(), ERR_GAME_SUPPORT_NO_PACKAGE);
    }
}
