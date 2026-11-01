//! The offline library: every item in the content store, its saved metadata, and the instances
//! that use it.
//!
//! Nothing here reaches the network. Descriptions come from the item's own saved metadata, or,
//! for catalog items, from the cached signed catalog in `registry.db`. A missing description stays
//! missing: the UI is told there is none rather than being handed invented text.
//!
//! Instances are read from their manifests on disk. A generic instance names a store item
//! directly (`LayerSource::Content`); a Minecraft instance records the installed file's SHA-256,
//! so an item is joined to it by object hash (and by a recorded item id when one is present).

use std::collections::HashSet;
use std::path::Path;

use agora_game_api::LayerSource;
use serde::{Deserialize, Serialize};

use crate::community_image;
use crate::content_store::{
    self, ContentError, ContentItem, ContentSource, ItemEntry, ItemMetadata,
};
use crate::ctx::Ctx;
use crate::helpers::read_manifest;
use crate::registry::RegistryService;

/// One instance that uses a library item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryInstanceRef {
    pub instance_id: String,
    pub name: String,
    pub game_id: String,
}

/// One row of the library, as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryItem {
    pub item_id: String,
    pub name: String,
    /// The item's own description, or a catalog entry's, or none. Never filled with a guess.
    pub description: Option<String>,
    pub version: Option<String>,
    pub game_id: Option<String>,
    pub page_url: Option<String>,
    /// SHA-256 of the saved icon object, when one is stored.
    pub icon_sha256: Option<String>,
    /// `archive`, `folder`, `fomod`, `thunderstore`, `mo2`, `catalog`, `file` or `unknown`.
    pub source_kind: &'static str,
    /// Where the item came from, in a sentence a person can read.
    pub source_label: String,
    /// The instances that have this item, by name.
    pub instances: Vec<LibraryInstanceRef>,
}

/// A store record that cannot be read. Listed so the user can see it and remove it, never skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DamagedLibraryItem {
    pub item_id: String,
    pub error: String,
}

/// Everything the library has, plus what could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryListing {
    pub items: Vec<LibraryItem>,
    pub damaged: Vec<DamagedLibraryItem>,
    /// Unreadable instance manifests (and catalog lookups that failed for reasons other than a
    /// missing catalog), so a failure is never shown as "nothing there".
    pub warnings: Vec<String>,
}

/// A stored icon whose bytes were identified as an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryIcon {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

impl LibraryIcon {
    pub fn to_data_url(&self) -> String {
        use base64::Engine;
        format!(
            "data:{};base64,{}",
            self.mime,
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

/// Every item in the store with its metadata and using instances. Read-only and network-free.
pub fn list(ctx: &Ctx) -> Result<LibraryListing, ContentError> {
    let entries = content_store::list_items(ctx)?;
    let scan = scan_instances(ctx);
    let mut warnings = scan.warnings.clone();
    let mut registry = RegistryLookup::new(ctx);
    let mut items = Vec::new();
    let mut damaged = Vec::new();

    for entry in entries {
        match entry {
            ItemEntry::Damaged(record) => damaged.push(DamagedLibraryItem {
                item_id: record.item_id,
                error: record.error,
            }),
            ItemEntry::Readable(item) => {
                let using = scan.using(&item);
                let game_id = item
                    .game_id
                    .clone()
                    .or_else(|| catalog_game(&item))
                    .or_else(|| unique_game(&using));
                let description = resolve_description(&item, &mut registry, &mut warnings);
                let (source_kind, source_label) = source_of(&item);
                items.push(LibraryItem {
                    item_id: item.item_id.clone(),
                    name: item.name.clone(),
                    description,
                    version: item.version.clone(),
                    game_id,
                    page_url: item.page_url.clone(),
                    icon_sha256: item.icon_sha256.clone(),
                    source_kind,
                    source_label,
                    instances: using,
                });
            }
        }
    }

    items.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.item_id.cmp(&b.item_id))
    });

    Ok(LibraryListing {
        items,
        damaged,
        warnings,
    })
}

/// Read an item's saved icon. `None` when no icon was stored; an unreadable or non-image object is
/// an error for the caller to report, not a silently empty icon.
pub fn read_icon(ctx: &Ctx, item_id_or_prefix: &str) -> Result<Option<LibraryIcon>, ContentError> {
    let item = content_store::get_item(ctx, item_id_or_prefix)?;
    match item.icon_sha256.as_deref() {
        Some(sha256) => read_icon_by_hash(ctx, sha256),
        None => Ok(None),
    }
}

/// Read one saved icon by its object hash. The hash is validated, the bytes come only from the
/// store's objects, and the read is capped before any bytes are loaded.
pub fn read_icon_by_hash(ctx: &Ctx, sha256: &str) -> Result<Option<LibraryIcon>, ContentError> {
    let bytes =
        content_store::read_object_bytes_limited(ctx, sha256, community_image::MAX_IMAGE_BYTES)?;
    let Some(mime) = community_image::sniff_image(&bytes) else {
        return Err(ContentError::Other(format!(
            "the saved object {sha256} is not an image"
        )));
    };
    Ok(Some(LibraryIcon { mime, bytes }))
}

/// Save `bytes` as an item's icon if they are an image. Returns whether an icon was saved; bytes
/// that are not an image are not an error for the page, so they simply are not saved.
pub fn save_icon(ctx: &Ctx, item_id_or_prefix: &str, bytes: &[u8]) -> Result<bool, ContentError> {
    if community_image::sniff_image(bytes).is_none() {
        return Ok(false);
    }
    let item = content_store::get_item(ctx, item_id_or_prefix)?;
    let sha256 = content_store::store_object_bytes(ctx, bytes)?;
    let mut metadata = metadata_of(&item);
    metadata.icon_sha256 = Some(sha256);
    content_store::set_item_metadata(ctx, &item.item_id, metadata)?;
    Ok(true)
}

/// Fetch `url` through the checked HTTP client, then save it as the item's icon when it is an
/// image. A failed fetch is reported as "not saved", never as a page error.
pub async fn fetch_icon(
    ctx: &Ctx,
    item_id_or_prefix: &str,
    url: &str,
) -> Result<bool, ContentError> {
    let image = match community_image::fetch(ctx, url).await {
        Ok(image) => image,
        Err(_) => return Ok(false),
    };
    save_icon(ctx, item_id_or_prefix, &image.bytes)
}

/// Fetch `url` once and save it on every readable item that came from the catalog entry
/// `catalog_id`. Returns how many items were given the icon. Used when Browse shows a catalog
/// item's icon, so the bytes are there when the network is not.
pub async fn fetch_icon_for_catalog(
    ctx: &Ctx,
    catalog_id: &str,
    url: &str,
) -> Result<usize, ContentError> {
    let matches: Vec<ContentItem> = content_store::list_items(ctx)?
        .into_iter()
        .filter_map(|entry| entry.readable().cloned())
        .filter(|item| item_catalog_id(item).as_deref() == Some(catalog_id))
        .collect();
    if matches.is_empty() {
        return Ok(0);
    }
    let image = match community_image::fetch(ctx, url).await {
        Ok(image) => image,
        Err(_) => return Ok(0),
    };
    let sha256 = content_store::store_object_bytes(ctx, &image.bytes)?;
    let mut saved = 0;
    for item in matches {
        let mut metadata = metadata_of(&item);
        metadata.icon_sha256 = Some(sha256.clone());
        content_store::set_item_metadata(ctx, &item.item_id, metadata)?;
        saved += 1;
    }
    Ok(saved)
}

/// Keep every metadata field the item already has, so setting an icon never erases a description.
fn metadata_of(item: &ContentItem) -> ItemMetadata {
    ItemMetadata {
        description: item.description.clone(),
        page_url: item.page_url.clone(),
        version: item.version.clone(),
        game_id: item.game_id.clone(),
        catalog_id: item.catalog_id.clone(),
        icon_sha256: item.icon_sha256.clone(),
    }
}

fn item_catalog_id(item: &ContentItem) -> Option<String> {
    item.catalog_id.clone().or_else(|| {
        item.sources.iter().find_map(|source| match source {
            ContentSource::Catalog { item_id, .. } => Some(item_id.clone()),
            _ => None,
        })
    })
}

fn source_of(item: &ContentItem) -> (&'static str, String) {
    match item.sources.first() {
        None => ("unknown", "No source was recorded for this item.".into()),
        Some(ContentSource::Archive { path, .. }) => {
            ("archive", format!("Archive: {}", last_component(path)))
        }
        Some(ContentSource::Folder { path, .. }) => {
            ("folder", format!("Folder: {}", last_component(path)))
        }
        Some(ContentSource::FomodInstall { from_item, .. }) => (
            "fomod",
            format!("Made by an installer from item {}", short_id(from_item)),
        ),
        Some(ContentSource::Thunderstore {
            package, version, ..
        }) => (
            "thunderstore",
            format!("Thunderstore package {package} {version}"),
        ),
        Some(ContentSource::Mo2Import { mo2_folder, .. }) => {
            ("mo2", format!("Mod Organizer 2 folder: {mo2_folder}"))
        }
        Some(ContentSource::Catalog { game, verified, .. }) => {
            (
                "catalog",
                if *verified {
                    format!("Curated catalog entry for {game}; its source published a hash that matched")
                } else {
                    format!("Curated catalog entry for {game}; its source published no hash")
                },
            )
        }
        Some(ContentSource::File { path, .. }) => {
            ("file", format!("File: {}", last_component(path)))
        }
    }
}

fn last_component(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn catalog_game(item: &ContentItem) -> Option<String> {
    item.sources.iter().find_map(|source| match source {
        ContentSource::Catalog { game, .. } => Some(game.clone()),
        _ => None,
    })
}

fn unique_game(instances: &[LibraryInstanceRef]) -> Option<String> {
    let mut games = instances.iter().map(|instance| instance.game_id.as_str());
    let first = games.next()?;
    if games.all(|game| game == first) {
        Some(first.to_string())
    } else {
        None
    }
}

/// An item's description: its own, then the catalog entry it came from. A missing catalog is not
/// an error (the user may never have downloaded one); an unreadable one is reported once.
struct RegistryLookup {
    service: RegistryService,
    /// Whether the cached catalog can be read: `None` until first asked.
    available: Option<bool>,
}

impl RegistryLookup {
    fn new(ctx: &Ctx) -> Self {
        Self {
            service: RegistryService::new(ctx.clone()),
            available: None,
        }
    }

    fn available(&mut self, warnings: &mut Vec<String>) -> bool {
        if self.available.is_none() {
            match self.service.connection() {
                Ok(_) => self.available = Some(true),
                Err(error) => {
                    // A catalog that was never downloaded is not a failure; a damaged one is
                    // reported, because its descriptions are silently missing otherwise.
                    if error.code() != "ERR_REGISTRY_MISSING" {
                        warnings.push(format!(
                            "The cached catalog could not be read, so some descriptions are missing: {error}"
                        ));
                    }
                    self.available = Some(false);
                }
            }
        }
        self.available == Some(true)
    }

    fn item_description(&mut self, warnings: &mut Vec<String>, id: &str) -> Option<String> {
        if !self.available(warnings) {
            return None;
        }
        self.service
            .get_item_by_id(id)
            .ok()
            .flatten()
            .and_then(|item| non_empty(item.description))
    }

    fn game_description(&mut self, warnings: &mut Vec<String>, id: &str) -> Option<String> {
        if !self.available(warnings) {
            return None;
        }
        self.service
            .get_game_item(id)
            .ok()
            .flatten()
            .and_then(|item| non_empty(item.description))
    }
}

fn resolve_description(
    item: &ContentItem,
    registry: &mut RegistryLookup,
    warnings: &mut Vec<String>,
) -> Option<String> {
    if let Some(description) = non_empty(item.description.clone()) {
        return Some(description);
    }
    if let Some(catalog_id) = &item.catalog_id {
        if let Some(description) = registry.item_description(warnings, catalog_id) {
            return Some(description);
        }
        if let Some(description) = registry.game_description(warnings, catalog_id) {
            return Some(description);
        }
    }
    for source in &item.sources {
        if let ContentSource::Catalog { item_id, .. } = source {
            if let Some(description) = registry.game_description(warnings, item_id) {
                return Some(description);
            }
            if let Some(description) = registry.item_description(warnings, item_id) {
                return Some(description);
            }
        }
    }
    None
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

// ---------------------------------------------------------------------------
// Instance scan
// ---------------------------------------------------------------------------

struct ScannedInstance {
    reference: LibraryInstanceRef,
    /// SHA-256 of every file the manifest records (Minecraft).
    hashes: HashSet<String>,
    /// Store item ids the manifest records directly (generic layers, and any per-entry item id).
    item_ids: HashSet<String>,
}

struct InstanceScan {
    instances: Vec<ScannedInstance>,
    warnings: Vec<String>,
}

impl InstanceScan {
    fn using(&self, item: &ContentItem) -> Vec<LibraryInstanceRef> {
        let hashes: HashSet<&str> = item.files.iter().map(|file| file.sha256.as_str()).collect();
        let mut using: Vec<LibraryInstanceRef> = self
            .instances
            .iter()
            .filter(|instance| {
                instance.item_ids.contains(&item.item_id)
                    || (instance.reference.game_id == "minecraft"
                        && instance
                            .hashes
                            .iter()
                            .any(|hash| hashes.contains(hash.as_str())))
            })
            .map(|instance| instance.reference.clone())
            .collect();
        using.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.instance_id.cmp(&b.instance_id))
        });
        using
    }
}

/// Read every instance manifest under the data root. An unreadable manifest is a warning, never a
/// vanished instance.
fn scan_instances(ctx: &Ctx) -> InstanceScan {
    let mut scan = InstanceScan {
        instances: Vec::new(),
        warnings: Vec::new(),
    };
    let root = ctx.paths.instances_root();
    if !root.exists() {
        return scan;
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            scan.warnings
                .push(format!("Instances could not be listed: {error}"));
            return scan;
        }
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("instance_manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = match read_manifest(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                scan.warnings.push(format!(
                    "Instance '{}' could not be read: {error}",
                    dir.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                continue;
            }
        };

        let game_id = manifest.game_data.game.as_str().to_string();
        let mut item_ids = HashSet::new();
        for layer in manifest.game_data.layers.iter() {
            if let LayerSource::Content { content } = &layer.source {
                item_ids.insert(content.clone());
            }
        }
        let mut hashes = HashSet::new();
        for installed in manifest
            .mods
            .iter()
            .chain(manifest.resourcepacks.iter())
            .chain(manifest.shaders.iter())
            .chain(manifest.datapacks.iter())
            .chain(manifest.worlds.iter())
        {
            if let Some(id) = installed
                .content_item_id
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                item_ids.insert(id.to_string());
            }
            // The store object hash is the file's real identity; `sha256` is the download's.
            if let Some(hash) = installed
                .object_sha256
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    Some(installed.sha256.as_str())
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                })
            {
                hashes.insert(hash.to_ascii_lowercase());
            }
        }

        scan.instances.push(ScannedInstance {
            reference: LibraryInstanceRef {
                instance_id: manifest.instance_id.clone(),
                name: manifest.name.clone(),
                game_id,
            },
            hashes,
            item_ids,
        });
    }
    scan
}
