//! The offline library: every stored item with its saved description, and which instances use it.
//!
//! Every command here is a thin adapter over `agora_core::library`. The listing reads no network;
//! the icon commands save a shown icon's bytes so it can be shown again offline.

use serde::Serialize;

use agora_core::content_store::ContentError;
use agora_core::ctx::Ctx;
use agora_core::library as core_library;

use super::error::{GamesError, CORE_FAILED, TASK_FAILED};

/// One instance that has a library item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInstanceRow {
    pub instance_id: String,
    pub name: String,
    pub game_id: String,
}

/// One row of the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryRow {
    pub item_id: String,
    pub name: String,
    /// `None` means no description was saved. It is never filled with invented text.
    pub description: Option<String>,
    pub version: Option<String>,
    pub game_id: Option<String>,
    pub page_url: Option<String>,
    pub icon_sha256: Option<String>,
    /// A `data:` URL for the saved icon, when one is stored and readable.
    pub icon_data_url: Option<String>,
    pub source_kind: String,
    pub source_label: String,
    pub instances: Vec<LibraryInstanceRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DamagedLibraryRow {
    pub item_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryListing {
    pub items: Vec<LibraryRow>,
    pub damaged: Vec<DamagedLibraryRow>,
    pub warnings: Vec<String>,
}

/// An instance the library can add an item to, with the game it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInstanceOption {
    pub instance_id: String,
    pub name: String,
    pub game_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInstanceOptions {
    pub instances: Vec<LibraryInstanceOption>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryIconCached {
    pub saved: bool,
    /// How many stored items were given the icon (one for an item, possibly several for a
    /// catalog entry).
    pub matched: usize,
    pub icon_data_url: Option<String>,
}

fn library_error(error: ContentError) -> GamesError {
    GamesError::new(CORE_FAILED, error.to_string())
}

fn read_icon_data_url(ctx: &Ctx, item_id: &str, warnings: &mut Vec<String>) -> Option<String> {
    match core_library::read_icon(ctx, item_id) {
        Ok(icon) => icon.map(|icon| icon.to_data_url()),
        Err(error) => {
            warnings.push(format!(
                "The saved icon for one item could not be read: {error}"
            ));
            None
        }
    }
}

/// Every item in the content store with its saved metadata and the instances that use it. Makes no
/// network request.
#[tauri::command]
pub async fn games_library_list(app: tauri::AppHandle) -> Result<LibraryListing, GamesError> {
    tokio::task::spawn_blocking(move || -> Result<LibraryListing, GamesError> {
        let ctx = crate::core_context(&app)?;
        let listing = core_library::list(&ctx).map_err(library_error)?;
        let mut warnings = listing.warnings;
        let items = listing
            .items
            .into_iter()
            .map(|item| LibraryRow {
                icon_data_url: item
                    .icon_sha256
                    .as_ref()
                    .and_then(|_| read_icon_data_url(&ctx, &item.item_id, &mut warnings)),
                item_id: item.item_id,
                name: item.name,
                description: item.description,
                version: item.version,
                game_id: item.game_id,
                page_url: item.page_url,
                icon_sha256: item.icon_sha256,
                source_kind: item.source_kind.to_string(),
                source_label: item.source_label,
                instances: item
                    .instances
                    .into_iter()
                    .map(|instance| LibraryInstanceRow {
                        instance_id: instance.instance_id,
                        name: instance.name,
                        game_id: instance.game_id,
                    })
                    .collect(),
            })
            .collect();
        Ok(LibraryListing {
            items,
            damaged: listing
                .damaged
                .into_iter()
                .map(|record| DamagedLibraryRow {
                    item_id: record.item_id,
                    message: record.error,
                })
                .collect(),
            warnings,
        })
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, "Reading the library did not finish."))?
}

/// Every instance of every game, so the Add-to-instance picker can offer the ones of the item's
/// game. A source that cannot be read is a warning, not a shorter list the user cannot explain.
#[tauri::command]
pub async fn games_library_instances(
    app: tauri::AppHandle,
) -> Result<LibraryInstanceOptions, GamesError> {
    tokio::task::spawn_blocking(move || -> Result<LibraryInstanceOptions, GamesError> {
        let ctx = crate::core_context(&app)?;
        let (instances, warnings) = agora_core::game_instance::list_all(&ctx);
        Ok(LibraryInstanceOptions {
            instances: instances
                .into_iter()
                .map(|instance| LibraryInstanceOption {
                    instance_id: instance.instance_id,
                    name: instance.name,
                    game_id: instance.game.as_str().to_string(),
                })
                .collect(),
            warnings,
        })
    })
    .await
    .map_err(|_| GamesError::new(TASK_FAILED, "Reading the instances did not finish."))?
}

/// Save an icon shown from `url`. Give `item_id` for one item, or `catalog_id` to save it on every
/// item that came from that catalog entry. A download or image failure is `saved: false`, never an
/// error for the page.
#[tauri::command]
pub async fn games_library_cache_icon(
    app: tauri::AppHandle,
    item_id: Option<String>,
    catalog_id: Option<String>,
    url: String,
) -> Result<LibraryIconCached, GamesError> {
    let item_id = item_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let catalog_id = catalog_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    let url = url.trim().to_string();
    if url.is_empty() || (item_id.is_none() && catalog_id.is_none()) {
        return Err(GamesError::new(
            "invalid_input",
            "An icon needs a URL and an item or catalog entry.",
        ));
    }

    let ctx = crate::core_context(&app)?;
    if let Some(item_id) = item_id {
        let saved = core_library::fetch_icon(&ctx, &item_id, &url)
            .await
            .map_err(library_error)?;
        let mut warnings = Vec::new();
        let icon_data_url = read_icon_data_url(&ctx, &item_id, &mut warnings);
        return Ok(LibraryIconCached {
            saved,
            matched: usize::from(saved),
            icon_data_url,
        });
    }

    let catalog_id = catalog_id.expect("checked above");
    let matched = core_library::fetch_icon_for_catalog(&ctx, &catalog_id, &url)
        .await
        .map_err(library_error)?;
    Ok(LibraryIconCached {
        saved: matched > 0,
        matched,
        icon_data_url: None,
    })
}
