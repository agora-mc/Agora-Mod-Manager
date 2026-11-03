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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use agora_game_api::LayerSource;
use serde::{Deserialize, Serialize};

use crate::community_image;
use crate::content_store::{
    self, ContentError, ContentItem, ContentSource, ItemEntry, ItemMetadata,
};
use crate::ctx::Ctx;
use crate::error::LauncherResult;
use crate::helpers::read_manifest;
use crate::lock_manager::LockResource;
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
    /// The one game the item belongs to, when there is exactly one answer. `None` when nothing
    /// ties the item to a game, or when instances of several games use it.
    pub game_id: Option<String>,
    /// Every game the item is tied to, and nothing guessed: the game recorded on the item, the
    /// catalog entry's game, the game of the item an installer or a Thunderstore import made it
    /// from, and the game of each instance that uses it. Empty means "not tied to a game".
    pub game_ids: Vec<String>,
    pub page_url: Option<String>,
    /// SHA-256 of the saved icon object, when one is stored.
    pub icon_sha256: Option<String>,
    /// `archive`, `folder`, `fomod`, `thunderstore`, `mo2`, `catalog`, `file` or `unknown`.
    pub source_kind: &'static str,
    /// Where the item came from, in a sentence a person can read.
    pub source_label: String,
    /// The instances that have this item, by name.
    pub instances: Vec<LibraryInstanceRef>,
    pub file_count: usize,
    pub total_size: u64,
    pub added_at_unix_ms: i64,
    /// Plugin files (by the item's games' plugin-list patterns) at the item's top level or one
    /// folder down, by file name. Empty for a game with no plugin list.
    pub plugin_files: Vec<String>,
    /// The item carries a FOMOD installer (`fomod/ModuleConfig.xml`), which asks its questions
    /// when it is added to an instance.
    pub has_installer: bool,
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

/// An instance manifest that could not be read. Removal refuses while one exists, naming the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadableInstanceManifest {
    pub manifest_path: String,
    pub error: String,
}

/// The instances that use one item, plus any manifest that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstanceUsage {
    pub instances: Vec<LibraryInstanceRef>,
    pub unreadable: Vec<UnreadableInstanceManifest>,
}

/// One item no instance uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnusedItem {
    pub item_id: String,
    pub name: String,
    pub total_size: u64,
    pub game_id: Option<String>,
    /// The games the item is tied to without an instance: recorded, catalog, or the item it was
    /// made from. Empty means "not tied to a game".
    pub game_ids: Vec<String>,
}

/// Items no instance uses, with their total size. Unreadable instance manifests are warnings: the
/// list may be incomplete, and `content_store::remove_item` refuses while one exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnusedListing {
    pub items: Vec<UnusedItem>,
    pub total_size: u64,
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
    let own_games = own_games_by_item(&entries);
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
                let hashes: HashSet<&str> =
                    item.files.iter().map(|file| file.sha256.as_str()).collect();
                let using = scan.using(&item.item_id, &hashes);
                let mut game_ids = games_without_instances(&item, &own_games);
                for instance in &using {
                    push_unique(&mut game_ids, &instance.game_id);
                }
                let game_id = item
                    .game_id
                    .clone()
                    .or_else(|| catalog_game(&item))
                    .or_else(|| (game_ids.len() == 1).then(|| game_ids[0].clone()));
                let description = resolve_description(&item, &mut registry, &mut warnings);
                let (source_kind, source_label) = source_of(&item);
                let plugin_files = plugin_files(ctx, &item, &game_ids);
                items.push(LibraryItem {
                    item_id: item.item_id.clone(),
                    name: item.name.clone(),
                    description,
                    version: item.version.clone(),
                    game_id,
                    game_ids,
                    page_url: item.page_url.clone(),
                    icon_sha256: item.icon_sha256.clone(),
                    source_kind,
                    source_label,
                    instances: using,
                    file_count: item.files.len(),
                    total_size: item.total_size,
                    added_at_unix_ms: item.added_at_unix_ms,
                    plugin_files,
                    has_installer: has_installer(&item),
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
    // Store the object and record it under one lock: a sweep must not see the icon object as
    // unreferenced between the write and the item that names it.
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-icon")?;
    let item = content_store::get_item(ctx, item_id_or_prefix)?;
    let sha256 = content_store::store_object_bytes_locked(ctx, bytes)?;
    let mut metadata = metadata_of(&item);
    metadata.icon_sha256 = Some(sha256);
    content_store::set_item_metadata_locked(ctx, &item.item_id, metadata)?;
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
    // The same one-lock rule as `save_icon`: the object and the records that name it land
    // together, so a sweep cannot delete the icon in between.
    let _lock = ctx
        .lock_manager
        .acquire(LockResource::ContentStore, "content-icons")?;
    let sha256 = content_store::store_object_bytes_locked(ctx, &image.bytes)?;
    let mut saved = 0;
    for item in matches {
        let mut metadata = metadata_of(&item);
        metadata.icon_sha256 = Some(sha256.clone());
        content_store::set_item_metadata_locked(ctx, &item.item_id, metadata)?;
        saved += 1;
    }
    Ok(saved)
}

/// How many project ids or hashes one metadata request may carry. Modrinth's batch endpoints
/// document a per-request id/hash list (see the URLs in the Minecraft metadata source); Agora
/// sends this many at most so a large library becomes a few requests, never one per item.
pub const METADATA_BATCH: usize = 100;

/// What a provider knows about one project, without any install-time details.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectDetails {
    pub project_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub page_url: Option<String>,
    pub icon_url: Option<String>,
}

/// The version a file hash belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectVersion {
    pub hash: String,
    pub project_id: String,
    /// The human version label (`1.3.9`), when the source publishes one.
    pub version_number: Option<String>,
}

/// A provider that can explain store items: batch project details, the version behind a file
/// hash, and an icon's bytes. Implemented by the game package that owns the provider; core keeps
/// the batching and merging so every caller behaves the same.
pub trait MetadataSource: Send + Sync {
    /// Whether the source may be used at all (network permission, endpoint switch). Checked once
    /// before any request, so a caller that is not allowed does not touch the network.
    fn available(&self) -> LauncherResult<()> {
        Ok(())
    }

    /// Details for at most [`METADATA_BATCH`] project ids.
    fn projects(&self, project_ids: &[String]) -> LauncherResult<Vec<ProjectDetails>>;

    /// The version each SHA-512 hash belongs to. Hashes the source does not know are omitted.
    fn versions_from_hashes(&self, hashes: &[String]) -> LauncherResult<Vec<ProjectVersion>>;

    /// An icon's bytes. Returning `None` means the source had nothing usable, not a failure.
    fn icon_bytes(&self, url: &str) -> LauncherResult<Option<Vec<u8>>>;
}

/// What one enrichment pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillReport {
    /// Items that were missing at least one detail and were considered.
    pub considered: usize,
    /// Items that gained at least one field, a title or an icon.
    pub updated: usize,
    /// Items that could not be identified, with the reason.
    pub skipped: Vec<FillSkip>,
    /// A failed batch or icon is reported here and never stops the rest.
    pub warnings: Vec<String>,
}

/// One item a fill pass could not identify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FillSkip {
    pub item_id: String,
    pub name: String,
    pub reason: String,
}

/// One item to enrich, and what the caller already knows about its project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichTarget {
    pub item_id: String,
    pub project_id: Option<String>,
    /// A version label the caller already knows (a pack pins one before any lookup).
    pub version: Option<String>,
}

/// How to fill an item's missing fields.
#[derive(Debug, Clone)]
pub struct EnrichOptions<'a> {
    /// `provider` value to record, e.g. `modrinth`.
    pub provider: &'a str,
    /// `game_id` to record when the item has none.
    pub game_id: Option<&'a str>,
    /// Replace a name that is still the stored file's stem with the project's title.
    pub prefer_title: bool,
    /// Replace a page URL the caller recorded as a guess (a pack's id-only URL) with the
    /// project's canonical page. Off for the fill action, which never overwrites a saved field.
    pub replace_page_url: bool,
}

impl Default for EnrichOptions<'_> {
    fn default() -> Self {
        Self {
            provider: "modrinth",
            game_id: Some("minecraft"),
            prefer_title: true,
            replace_page_url: false,
        }
    }
}

/// Fill missing details on every item the source can identify. Reads and writes the content
/// store only; the source decides whether it may reach the network.
pub fn fill_missing_details(
    ctx: &Ctx,
    source: &dyn MetadataSource,
) -> Result<FillReport, ContentError> {
    let targets: Vec<EnrichTarget> = content_store::list_items(ctx)?
        .into_iter()
        .filter_map(|entry| entry.readable().cloned())
        .filter(needs_details)
        .map(|item| EnrichTarget {
            item_id: item.item_id,
            project_id: item.provider_id.clone(),
            version: item.version.clone(),
        })
        .collect();
    enrich_items(ctx, source, &targets, EnrichOptions::default())
}

fn needs_details(item: &ContentItem) -> bool {
    non_empty(item.description.clone()).is_none()
        || non_empty(item.page_url.clone()).is_none()
        || non_empty(item.version.clone()).is_none()
        || item.icon_sha256.is_none()
}

fn item_provider(item: &ContentItem) -> Option<&str> {
    item.provider.as_deref()
}

/// The file stem an item was named after, when its name still is that stem.
fn name_is_file_stem(item: &ContentItem) -> bool {
    let Some(file) = item.files.first() else {
        return false;
    };
    let stem = Path::new(file.path.as_str())
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned());
    stem.as_deref() == Some(item.name.trim())
}

/// SHA-512 of a stored object, read in chunks so a large file is never loaded whole. Modrinth's
/// hash lookup accepts SHA-1 and SHA-512 (not the store's SHA-256), so this is the value it needs.
fn object_sha512(ctx: &Ctx, sha256: &str) -> Result<String, ContentError> {
    use sha2::{Digest, Sha512};
    use std::io::Read;
    if !sha256
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || sha256.len() != 64
    {
        return Err(ContentError::InvalidHash(sha256.to_string()));
    }
    let path = ctx.paths.content_object_path(sha256);
    let mut file = std::fs::File::open(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ContentError::MissingObject(sha256.to_string())
        } else {
            ContentError::Io(error)
        }
    })?;
    let mut hasher = Sha512::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Apply project details to `targets`, filling only fields that are absent. A failed batch or
/// icon is a warning: the rest of the items are still filled.
pub fn enrich_items(
    ctx: &Ctx,
    source: &dyn MetadataSource,
    targets: &[EnrichTarget],
    options: EnrichOptions<'_>,
) -> Result<FillReport, ContentError> {
    let mut report = FillReport {
        considered: targets.len(),
        ..Default::default()
    };
    if targets.is_empty() {
        return Ok(report);
    }
    if let Err(error) = source.available() {
        report.warnings.push(error.to_string());
        return Ok(report);
    }

    let mut items: BTreeMap<String, ContentItem> = BTreeMap::new();
    for target in targets {
        match content_store::get_item(ctx, &target.item_id) {
            Ok(item) => {
                items.insert(target.item_id.clone(), item);
            }
            Err(error) => report.warnings.push(format!(
                "Could not read the item {} to fill in its details: {error}",
                target.item_id
            )),
        }
    }

    // Identify by the provider's own id first, else by the file's SHA-512.
    let mut project_of: BTreeMap<String, String> = BTreeMap::new();
    let mut version_of: BTreeMap<String, String> = BTreeMap::new();
    let mut hash_targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for target in targets {
        let Some(item) = items.get(&target.item_id) else {
            continue;
        };
        let known_project = target.project_id.clone().or_else(|| {
            item_provider(item)
                .filter(|provider| *provider == options.provider)
                .and(item.provider_id.clone())
        });
        let has_known_project = known_project
            .as_deref()
            .is_some_and(|project_id| !project_id.trim().is_empty());
        if let Some(project_id) = known_project.filter(|_| has_known_project) {
            project_of.insert(target.item_id.clone(), project_id);
        }
        // A provider id explains what the item is, but not which version the file is. The
        // version label comes from the hash lookup, so an item still lacking one is hashed even
        // when its project is known.
        let needs_version = non_empty(item.version.clone()).is_none()
            && non_empty(target.version.clone()).is_none();
        if has_known_project && !needs_version {
            continue;
        }
        let Some(file) = item.files.first() else {
            continue;
        };
        match object_sha512(ctx, &file.sha256) {
            Ok(hash) => hash_targets
                .entry(hash)
                .or_default()
                .push(target.item_id.clone()),
            Err(error) => report.skipped.push(FillSkip {
                item_id: item.item_id.clone(),
                name: item.name.clone(),
                reason: format!("its stored file could not be hashed: {error}"),
            }),
        }
    }

    let hashes: Vec<String> = hash_targets.keys().cloned().collect();
    for chunk in hashes.chunks(METADATA_BATCH) {
        match source.versions_from_hashes(chunk) {
            Ok(found) => {
                for version in found {
                    let Some(item_ids) = hash_targets.get(&version.hash) else {
                        continue;
                    };
                    for item_id in item_ids {
                        project_of
                            .entry(item_id.clone())
                            .or_insert_with(|| version.project_id.clone());
                        if let Some(label) = version.version_number.clone() {
                            version_of.insert(item_id.clone(), label);
                        }
                    }
                }
            }
            Err(error) => report
                .warnings
                .push(format!("Could not look up file versions: {error}")),
        }
    }

    let mut by_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (item_id, project_id) in &project_of {
        by_project
            .entry(project_id.clone())
            .or_default()
            .push(item_id.clone());
    }
    let project_ids: Vec<String> = by_project.keys().cloned().collect();
    let mut details: BTreeMap<String, ProjectDetails> = BTreeMap::new();
    for chunk in project_ids.chunks(METADATA_BATCH) {
        match source.projects(chunk) {
            Ok(found) => {
                for project in found {
                    details.insert(project.project_id.clone(), project);
                }
            }
            Err(error) => report
                .warnings
                .push(format!("Could not look up project details: {error}")),
        }
    }

    for (item_id, project_id) in &project_of {
        let Some(mut item) = items.remove(item_id) else {
            continue;
        };
        let Some(project) = details.get(project_id) else {
            report.skipped.push(FillSkip {
                item_id: item.item_id.clone(),
                name: item.name.clone(),
                reason: "its project was not returned by the lookup".into(),
            });
            continue;
        };
        let mut metadata = metadata_of(&item);
        let mut changed = false;
        if metadata.description.is_none() {
            metadata.description = non_empty(project.description.clone());
            changed |= metadata.description.is_some();
        }
        if options.replace_page_url {
            let canonical = non_empty(project.page_url.clone());
            if canonical.is_some() && metadata.page_url != canonical {
                metadata.page_url = canonical;
                changed = true;
            }
        } else if metadata.page_url.is_none() {
            metadata.page_url = non_empty(project.page_url.clone());
            changed |= metadata.page_url.is_some();
        }
        if metadata.version.is_none() {
            let label = version_of.get(item_id).cloned().or_else(|| {
                targets
                    .iter()
                    .find(|t| &t.item_id == item_id)?
                    .version
                    .clone()
            });
            metadata.version = non_empty(label);
            changed |= metadata.version.is_some();
        }
        if metadata.provider.is_none() {
            metadata.provider = Some(options.provider.to_string());
            changed = true;
        }
        if metadata.provider_id.is_none() {
            metadata.provider_id = Some(project_id.clone());
            changed = true;
        }
        if metadata.game_id.is_none() {
            metadata.game_id = options.game_id.map(str::to_string);
            changed |= metadata.game_id.is_some();
        }
        if changed {
            content_store::set_item_metadata(ctx, &item.item_id, metadata.clone())?;
            item = content_store::get_item(ctx, &item.item_id)?;
        }
        if metadata.icon_sha256.is_none() {
            if let Some(url) = project.icon_url.as_deref().filter(|u| !u.trim().is_empty()) {
                match source.icon_bytes(url) {
                    Ok(Some(bytes)) => {
                        if save_icon(ctx, &item.item_id, &bytes)? {
                            changed = true;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => report.warnings.push(format!(
                        "Could not save the icon for {}: {error}",
                        item.name
                    )),
                }
            }
        }
        if options.prefer_title && name_is_file_stem(&item) {
            if let Some(title) = non_empty(project.title.clone()) {
                content_store::set_item_name(ctx, &item.item_id, &title)?;
                changed = true;
            }
        }
        if changed {
            report.updated += 1;
        }
    }

    Ok(report)
}

/// Keep every metadata field the item already has, so setting an icon never erases a description.
fn metadata_of(item: &ContentItem) -> ItemMetadata {
    ItemMetadata {
        description: item.description.clone(),
        page_url: item.page_url.clone(),
        version: item.version.clone(),
        game_id: item.game_id.clone(),
        catalog_id: item.catalog_id.clone(),
        provider: item.provider.clone(),
        provider_id: item.provider_id.clone(),
        provider_origin: item.provider_origin.clone(),
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

/// The games each readable item names by itself: its recorded game and its catalog entry's game.
fn own_games_by_item(entries: &[ItemEntry]) -> BTreeMap<String, Vec<String>> {
    let mut games = BTreeMap::new();
    for item in entries.iter().filter_map(ItemEntry::readable) {
        let mut own = Vec::new();
        if let Some(game) = item.game_id.as_deref() {
            push_unique(&mut own, game);
        }
        for source in &item.sources {
            if let ContentSource::Catalog { game, .. } = source {
                push_unique(&mut own, game);
            }
        }
        games.insert(item.item_id.clone(), own);
    }
    games
}

/// The games an item is tied to without looking at instances: its own, then those of the item a
/// FOMOD installer or a Thunderstore import made it from (that item's own games, one level).
fn games_without_instances(
    item: &ContentItem,
    own_games: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut games = own_games.get(&item.item_id).cloned().unwrap_or_default();
    for source in &item.sources {
        let parent = match source {
            ContentSource::FomodInstall { from_item, .. }
            | ContentSource::Thunderstore { from_item, .. } => from_item,
            _ => continue,
        };
        for game in own_games.get(parent).into_iter().flatten() {
            push_unique(&mut games, game);
        }
    }
    games
}

fn push_unique(games: &mut Vec<String>, game: &str) {
    if !game.is_empty() && !games.iter().any(|known| known == game) {
        games.push(game.to_string());
    }
}

/// Plugin files the item carries for any of `games` (by each game's plugin-list patterns), at the
/// item's top level or one folder down. Deeper files are an installer's options, not plugins the
/// game loads as they are.
fn plugin_files(ctx: &Ctx, item: &ContentItem, games: &[String]) -> Vec<String> {
    let patterns: Vec<String> = games
        .iter()
        .filter_map(|game| agora_game_api::GameId::new(game).ok())
        .filter_map(|game| ctx.games.game(&game).cloned())
        .filter_map(|definition| definition.plugin_list)
        .flat_map(|rule| rule.patterns)
        .map(|pattern| pattern.to_ascii_lowercase())
        .collect();
    if patterns.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<String> = Vec::new();
    for file in &item.files {
        let path = file.path.as_str();
        if path.split('/').count() > 2 {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        let lower = name.to_ascii_lowercase();
        let matches = patterns
            .iter()
            .any(|pattern| agora_game_api::glob_match(pattern, &lower));
        if matches && !found.iter().any(|known| known.eq_ignore_ascii_case(name)) {
            found.push(name.to_string());
        }
    }
    found
}

/// Whether the item holds a FOMOD installer at its top level or inside one wrapper folder.
fn has_installer(item: &ContentItem) -> bool {
    item.files.iter().any(|file| {
        let lower = file.path.as_str().to_ascii_lowercase();
        lower == "fomod/moduleconfig.xml"
            || (lower.ends_with("/fomod/moduleconfig.xml") && lower.split('/').count() == 3)
    })
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
    unreadable: Vec<UnreadableInstanceManifest>,
}

impl InstanceScan {
    /// Instances that use `item_id`: directly by a Content layer or a recorded store id, or, for
    /// Minecraft, by one of `hashes` (the item's object hashes).
    fn using(&self, item_id: &str, hashes: &HashSet<&str>) -> Vec<LibraryInstanceRef> {
        let mut using: Vec<LibraryInstanceRef> = self
            .instances
            .iter()
            .filter(|instance| {
                instance.item_ids.contains(item_id)
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

/// Which instances use one item. Scans every instance manifest (Minecraft and generic alike); an
/// unreadable manifest is returned, never treated as "no instance uses it".
pub fn instances_using(ctx: &Ctx, item_id: &str, object_hashes: &[String]) -> InstanceUsage {
    let scan = scan_instances(ctx);
    let hashes: HashSet<&str> = object_hashes.iter().map(String::as_str).collect();
    InstanceUsage {
        instances: scan.using(item_id, &hashes),
        unreadable: scan.unreadable,
    }
}

/// Whether any readable instance manifest records a stored object hash. Only a Minecraft-style
/// manifest does; a generic instance names items by id. Removal uses this when an item's own
/// record cannot be read: its object hashes are unknown, so it cannot prove that none of those
/// hashes is in use.
pub fn any_instance_records_objects(ctx: &Ctx) -> bool {
    scan_instances(ctx)
        .instances
        .iter()
        .any(|instance| !instance.hashes.is_empty())
}

/// Every item no instance uses, with its size. When an instance manifest cannot be read, nothing
/// is offered: the unreadable instance might use any item, so the listing is empty and the
/// warnings say which instance could not be read (fail closed, like removal).
pub fn list_unused(ctx: &Ctx) -> Result<UnusedListing, ContentError> {
    let entries = content_store::list_items(ctx)?;
    let scan = scan_instances(ctx);
    let own_games = own_games_by_item(&entries);
    if !scan.unreadable.is_empty() {
        return Ok(UnusedListing {
            items: Vec::new(),
            total_size: 0,
            warnings: scan.warnings,
        });
    }
    let mut items = Vec::new();
    let mut total_size = 0u64;
    for entry in entries {
        let Some(item) = entry.readable() else {
            continue;
        };
        let hashes: HashSet<&str> = item.files.iter().map(|file| file.sha256.as_str()).collect();
        if !scan.using(&item.item_id, &hashes).is_empty() {
            continue;
        }
        total_size = total_size.saturating_add(item.total_size);
        items.push(UnusedItem {
            item_id: item.item_id.clone(),
            name: item.name.clone(),
            total_size: item.total_size,
            game_id: item.game_id.clone().or_else(|| catalog_game(item)),
            game_ids: games_without_instances(item, &own_games),
        });
    }
    items.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.item_id.cmp(&b.item_id))
    });
    Ok(UnusedListing {
        items,
        total_size,
        warnings: scan.warnings,
    })
}

/// Read every instance manifest under the data root. An unreadable manifest is a warning, never a
/// vanished instance.
fn scan_instances(ctx: &Ctx) -> InstanceScan {
    let mut scan = InstanceScan {
        instances: Vec::new(),
        warnings: Vec::new(),
        unreadable: Vec::new(),
    };
    let mut dirs = Vec::new();
    collect_instance_dirs(&ctx.paths.instances_root(), None, &mut dirs, &mut scan);
    // An instance in Recently deleted still restores its items, so it still uses them. Both
    // Agora's game trash and Minecraft's keep `<entry>/instance/instance_manifest.json`; a purged
    // entry's folder is gone, so it stops counting on its own.
    for root in [
        ctx.paths.recently_deleted_games_root(),
        ctx.paths.recently_deleted_root(),
    ] {
        collect_instance_dirs(&root, Some("instance"), &mut dirs, &mut scan);
    }

    for dir in dirs {
        let manifest_path = dir.join("instance_manifest.json");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest = match read_manifest(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                let error = error.to_string();
                scan.warnings.push(format!(
                    "Instance '{}' could not be read: {error}",
                    dir.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                scan.unreadable.push(UnreadableInstanceManifest {
                    manifest_path: manifest_path.display().to_string(),
                    error,
                });
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
                    let hash = installed.sha256.trim();
                    (!hash.is_empty()).then_some(hash)
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

/// Every folder under `root`, or every `<folder>/<subdir>` when `subdir` is given. An unreadable
/// root is a warning and a fail-closed unreadable manifest, never a silent "no instances".
fn collect_instance_dirs(
    root: &Path,
    subdir: Option<&str>,
    dirs: &mut Vec<PathBuf>,
    scan: &mut InstanceScan,
) {
    if !root.exists() {
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            let message = format!("Instances could not be listed: {error}");
            scan.warnings.push(message.clone());
            scan.unreadable.push(UnreadableInstanceManifest {
                manifest_path: root.display().to_string(),
                error: message,
            });
            return;
        }
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let dir = match subdir {
            Some(subdir) => dir.join(subdir),
            None => dir,
        };
        if dir.is_dir() {
            dirs.push(dir);
        }
    }
}
