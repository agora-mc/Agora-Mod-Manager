//! Minecraft's one door to core's shared content store.
//!
//! Every content file Agora installs is stored once, verified, and placed into the instance by
//! the store's own placement (a hard link by default, or a copy when sharing is off). Keeping
//! the calls here means the Minecraft package gains a single `agora_core` reference, and the
//! install paths use one consistent routine for storing, placing and recognising Agora's files.

use std::path::Path;

use agora_core::{
    content_store as store,
    ctx::Ctx,
    error::{LauncherError, LauncherResult},
    helpers,
    library::{self as core_library},
    lock_manager::LockResource,
    models::{InstalledMod, ProviderOrigin},
    paths, snapshot,
};

#[cfg(test)]
pub use core_library::{list_unused, ProjectDetails, ProjectVersion};
pub use core_library::{EnrichOptions, EnrichTarget, FillReport, MetadataSource};
#[cfg(test)]
pub use store::ContentItem;

#[cfg(test)]
use agora_core::{db, http_client};

/// The core richness pass used by pack imports and the Library action, so both go through one
/// narrow door and the game package's core references stay in this module.
pub fn enrich_store_items(
    ctx: &Ctx,
    source: &dyn MetadataSource,
    targets: &[EnrichTarget],
    options: EnrichOptions<'_>,
) -> LauncherResult<FillReport> {
    core_library::enrich_items(ctx, source, targets, options).map_err(content_error)
}

/// What an install path already knows about the file it is storing.
#[derive(Clone, Copy)]
pub struct StoreDetails<'a> {
    pub content_type: &'a str,
    /// The file's name inside the instance, without a `.disabled` suffix.
    pub filename: &'a str,
    pub version: Option<&'a str>,
    pub page_url: Option<&'a str>,
    pub description: Option<&'a str>,
    pub catalog_id: Option<&'a str>,
    /// Generic provenance kind (`catalog`, `modrinth`, `manual`, `provider`).
    pub provider: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    /// Opaque provider provenance (a JSON string built by [`provider_origin_blob`]).
    pub provider_origin: Option<&'a str>,
}

/// The store item a file was added as, and the object it was placed from.
#[derive(Clone)]
pub struct StoredFile {
    pub item_id: String,
    pub sha256: String,
}

fn content_error(error: store::ContentError) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_CONTENT_STORE".into(),
        message: error.to_string(),
    }
}

/// The opaque provenance blob a provider install records, so an offline add can rebuild the exact
/// `ProviderOrigin` (provider, project and version ids).
pub fn provider_origin_blob(provider_id: &str, project_id: &str, version_id: &str) -> String {
    serde_json::to_string(&ProviderOrigin {
        provider_id: provider_id.to_string(),
        project_id: project_id.to_string(),
        version_id: version_id.to_string(),
    })
    .expect("provider origin is serializable")
}

fn provider_origin_from_blob(blob: &str) -> Option<ProviderOrigin> {
    serde_json::from_str(blob).ok()
}

/// The content type that owns one of the store's instance-side folders.
pub fn content_type_for_subdir(name: &str) -> Option<&'static str> {
    match name {
        "mods" => Some("mod"),
        "resourcepacks" => Some("resourcepack"),
        "shaderpacks" => Some("shader"),
        "datapacks" => Some("datapack"),
        "saves" => Some("world"),
        _ => None,
    }
}

/// The content type of an instance-relative path, for the folders the manifest tracks as
/// content. `saves/` is deliberately absent: a world is a folder of ordinary per-instance
/// files, never a single stored item.
pub fn tracked_content_type(relative_path: &str) -> Option<&'static str> {
    let folder = relative_path.split(['/', '\\']).next().unwrap_or_default();
    match folder {
        "mods" => Some("mod"),
        "resourcepacks" => Some("resourcepack"),
        "shaderpacks" => Some("shader"),
        "datapacks" => Some("datapack"),
        _ => None,
    }
}

fn file_name_of(relative_path: &str) -> &str {
    relative_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative_path)
}

fn write_error(action: &str, path: &Path, error: std::io::Error) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_CONTENT_STORE".into(),
        message: format!("Could not {action} {}: {error}", path.display()),
    }
}

/// A pack-relative content path must be a plain relative path under one tracked content folder:
/// no absolute paths, drive prefixes, `..`, `.` or backslashes.
fn validate_relative_path(relative_path: &str) -> LauncherResult<()> {
    use std::path::Component;
    let invalid = || LauncherError::Generic {
        code: "ERR_CONTENT_PATH".into(),
        message: format!("'{relative_path}' is not a safe content path."),
    };
    if relative_path.is_empty() || relative_path.contains('\\') {
        return Err(invalid());
    }
    let mut parts = 0usize;
    for component in Path::new(relative_path).components() {
        match component {
            Component::Normal(_) => parts += 1,
            _ => return Err(invalid()),
        }
    }
    if parts < 2 || tracked_content_type(relative_path).is_none() {
        return Err(invalid());
    }
    Ok(())
}

fn store_details<'a>(
    content_type: &'a str,
    filename: &'a str,
    version: Option<&'a str>,
    page_url: Option<&'a str>,
) -> StoreDetails<'a> {
    StoreDetails {
        content_type,
        filename,
        version,
        page_url,
        description: None,
        catalog_id: None,
        provider: None,
        provider_id: None,
        provider_origin: None,
    }
}

/// Put a stored object at `instance_dir/<relative_path>`, keeping the pack's folder structure.
fn place_at_relative(
    ctx: &Ctx,
    sha256: &str,
    instance_dir: &Path,
    relative_path: &str,
) -> LauncherResult<store::PlacedAs> {
    validate_relative_path(relative_path)?;
    let target = instance_dir.join(relative_path);
    let mode = store::placement_mode(ctx).map_err(content_error)?;
    store::place_object_at(ctx, sha256, &target, mode).map_err(content_error)
}

/// Store a pack file already written at `instance_dir/<relative_path>` and replace that ordinary
/// file with the store placement. Used once a whole import has passed its hash checks, so a
/// stopped import never puts a failing file into the store.
pub fn store_staged_content(
    ctx: &Ctx,
    instance_dir: &Path,
    relative_path: &str,
    source: &Path,
    details: &StoreDetails<'_>,
) -> LauncherResult<Option<StoredFile>> {
    let Some(content_type) = tracked_content_type(relative_path) else {
        return Ok(None);
    };
    validate_relative_path(relative_path)?;
    let details = StoreDetails {
        content_type,
        filename: file_name_of(relative_path),
        ..*details
    };
    let stored = store_file_at(ctx, source, &details, Some(relative_path))?;
    let target = instance_dir.join(relative_path);
    if target.exists() {
        if let Some(folder) = target.parent() {
            ensure_folder_can_delete_children(folder)?;
        }
        std::fs::remove_file(&target)
            .map_err(|error| write_error("replace a staged pack file", &target, error))?;
    }
    place_at_relative(ctx, &stored.sha256, instance_dir, relative_path)?;
    Ok(Some(stored))
}

/// Re-place every manifest entry that recorded a store object, using the current link/copy
/// setting. A rollback restores bytes from a snapshot of copies; this puts the links back.
/// Missing objects and files whose bytes no longer match the record are left alone.
pub fn relink_recorded_content(ctx: &Ctx, instance_dir: &Path) -> LauncherResult<()> {
    let manifest_path = instance_dir.join("instance_manifest.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let manifest = helpers::read_manifest(&manifest_path)?;
    let mode = store::placement_mode(ctx).map_err(content_error)?;
    for entry in manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
    {
        let Some(object) = entry.object_sha256.as_deref() else {
            continue;
        };
        if !ctx.paths.content_object_path(object).is_file() {
            continue;
        }
        let subdir = subdir_for_content_type(&entry.content_type);
        if subdir == "saves" {
            continue;
        }
        let physical = if entry.enabled {
            entry.filename.clone()
        } else {
            format!("{}.disabled", entry.filename)
        };
        let target = instance_dir.join(subdir).join(&physical);
        if target.exists() {
            if !file_matches_recorded(&target, &entry.sha256)? {
                continue;
            }
            if let Some(folder) = target.parent() {
                ensure_folder_can_delete_children(folder)?;
            }
            std::fs::remove_file(&target)
                .map_err(|error| write_error("replace a restored file", &target, error))?;
        }
        store::place_object_at(ctx, object, &target, mode).map_err(content_error)?;
    }
    Ok(())
}

/// Store one file's bytes as a single-file item when `relative_path` is in a tracked content
/// folder, without placing it anywhere yet. Returns `None` for any other path.
pub fn store_bytes(
    ctx: &Ctx,
    relative_path: &str,
    bytes: &[u8],
    version: Option<&str>,
    page_url: Option<&str>,
) -> LauncherResult<Option<StoredFile>> {
    let Some(content_type) = tracked_content_type(relative_path) else {
        return Ok(None);
    };
    validate_relative_path(relative_path)?;
    let incoming = ctx.paths.staging_root().join("pack-incoming");
    std::fs::create_dir_all(&incoming)
        .map_err(|error| write_error("create the store's incoming folder", &incoming, error))?;
    let temp = incoming.join(format!("{}.bin", uuid::Uuid::new_v4()));
    std::fs::write(&temp, bytes).map_err(|error| write_error("stage a pack file", &temp, error))?;
    let stored = store_file_at(
        ctx,
        &temp,
        &store_details(content_type, file_name_of(relative_path), version, page_url),
        Some(relative_path),
    );
    let _ = std::fs::remove_file(&temp);
    Ok(Some(stored?))
}

/// Store one file's bytes and place the object at `instance_dir/<relative_path>` when that path
/// is in a tracked content folder. Returns `None` and writes nothing for any other path, which
/// stays an ordinary per-instance file its owner writes itself.
pub fn store_and_place_bytes(
    ctx: &Ctx,
    instance_dir: &Path,
    relative_path: &str,
    bytes: &[u8],
    version: Option<&str>,
    page_url: Option<&str>,
) -> LauncherResult<Option<StoredFile>> {
    let Some(stored) = store_bytes(ctx, relative_path, bytes, version, page_url)? else {
        return Ok(None);
    };
    place_at_relative(ctx, &stored.sha256, instance_dir, relative_path)?;
    Ok(Some(stored))
}

/// Store an existing file and place the object at `instance_dir/<relative_path>` when that path
/// is in a tracked content folder. Returns `None` (having written nothing) for any other path.
pub fn store_and_place_file(
    ctx: &Ctx,
    instance_dir: &Path,
    relative_path: &str,
    source: &Path,
    version: Option<&str>,
    page_url: Option<&str>,
) -> LauncherResult<Option<StoredFile>> {
    let Some(content_type) = tracked_content_type(relative_path) else {
        return Ok(None);
    };
    validate_relative_path(relative_path)?;
    let stored = store_file_at(
        ctx,
        source,
        &store_details(content_type, file_name_of(relative_path), version, page_url),
        Some(relative_path),
    )?;
    place_at_relative(ctx, &stored.sha256, instance_dir, relative_path)?;
    Ok(Some(stored))
}

/// The folder a content type is installed into.
pub fn subdir_for_content_type(content_type: &str) -> &'static str {
    match content_type {
        "resourcepack" => "resourcepacks",
        "shader" => "shaderpacks",
        "datapack" => "datapacks",
        "world" => "saves",
        _ => "mods",
    }
}

/// Store one verified file as a single-file item. The item records the folder it belongs to in
/// its file path, so an offline add knows where to place it. Identical bytes under the same path
/// reuse the stored object.
pub fn store_file(
    ctx: &Ctx,
    file_path: &Path,
    details: &StoreDetails<'_>,
) -> LauncherResult<StoredFile> {
    store_file_at(ctx, file_path, details, None)
}

fn store_file_at(
    ctx: &Ctx,
    file_path: &Path,
    details: &StoreDetails<'_>,
    relative_path: Option<&str>,
) -> LauncherResult<StoredFile> {
    let rel = match relative_path {
        Some(path) => {
            validate_relative_path(path)?;
            path.to_string()
        }
        None => format!(
            "{}/{}",
            subdir_for_content_type(details.content_type),
            details.filename
        ),
    };
    let name = Path::new(details.filename)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| details.filename.to_string());
    let outcome =
        store::add_file(ctx, file_path, Some(&name), Some(&rel)).map_err(content_error)?;
    let item = outcome.item();
    let file = item
        .files
        .first()
        .ok_or_else(|| content_error(store::ContentError::EmptyItem))?;
    let stored = StoredFile {
        item_id: item.item_id.clone(),
        sha256: file.sha256.clone(),
    };
    // The item may already be in the store (the same bytes, or the same path). Merge rather than
    // replace: a field this install does not know keeps the value the item already carries, so a
    // reinstall never erases a saved icon, description or provider provenance.
    let metadata = store::ItemMetadata {
        description: details
            .description
            .map(str::to_string)
            .or_else(|| item.description.clone()),
        page_url: details
            .page_url
            .map(str::to_string)
            .or_else(|| item.page_url.clone()),
        version: details
            .version
            .map(str::to_string)
            .or_else(|| item.version.clone()),
        game_id: Some("minecraft".to_string()),
        catalog_id: details
            .catalog_id
            .map(str::to_string)
            .or_else(|| item.catalog_id.clone()),
        provider: details
            .provider
            .map(str::to_string)
            .or_else(|| item.provider.clone()),
        provider_id: details
            .provider_id
            .map(str::to_string)
            .or_else(|| item.provider_id.clone()),
        provider_origin: details
            .provider_origin
            .map(str::to_string)
            .or_else(|| item.provider_origin.clone()),
        // The install never knows the icon; only a saved one does.
        icon_sha256: item.icon_sha256.clone(),
    };
    store::set_item_metadata(ctx, &stored.item_id, metadata).map_err(content_error)?;
    Ok(stored)
}

/// Place a stored object at the instance's content path, using the user's link/copy choice.
pub fn place_file(
    ctx: &Ctx,
    sha256: &str,
    instance_dir: &Path,
    content_type: &str,
    filename: &str,
) -> LauncherResult<store::PlacedAs> {
    let target = instance_dir
        .join(subdir_for_content_type(content_type))
        .join(filename);
    let mode = store::placement_mode(ctx).map_err(content_error)?;
    store::place_object_at(ctx, sha256, &target, mode).map_err(content_error)
}

/// Whether the file on disk still holds the bytes the manifest recorded. A file that does not
/// match is the user's, not Agora's, and must never be deleted.
pub fn file_matches_recorded(path: &Path, recorded_sha256: &str) -> LauncherResult<bool> {
    let actual = helpers::hash_file_sha256(path).map_err(|error| LauncherError::Generic {
        code: "ERR_CONTENT_READ".into(),
        message: format!("Could not read {}: {error}", path.display()),
    })?;
    Ok(actual.eq_ignore_ascii_case(recorded_sha256))
}

/// Make sure a folder can have files deleted from it. A link to a protected object cannot be
/// deleted from a folder that only grants Modify; the store's placement grants the missing right.
pub fn ensure_folder_can_delete_children(folder: &Path) -> LauncherResult<()> {
    if store::protect::grants_delete_child(folder).map_err(|error| LauncherError::Generic {
        code: "ERR_CONTENT_STORE".into(),
        message: error.to_string(),
    })? {
        return Ok(());
    }
    store::protect::grant_delete_child(folder).map_err(|error| LauncherError::Generic {
        code: "ERR_CONTENT_STORE".into(),
        message: error.to_string(),
    })
}

/// Read a stored item by id or unique prefix.
pub fn load_item(ctx: &Ctx, item_id: &str) -> LauncherResult<store::ContentItem> {
    store::get_item(ctx, item_id).map_err(content_error)
}

/// The fields an offline add copies from a stored item into the instance manifest.
pub fn stored_mod_fields(item: &store::ContentItem) -> LauncherResult<(String, String, String)> {
    if item.files.len() != 1 {
        return Err(LauncherError::Generic {
            code: "ERR_CONTENT_ITEM_NOT_SINGLE".into(),
            message: format!(
                "This stored item holds {} files; only a single-file item can be added to a Minecraft instance.",
                item.files.len()
            ),
        });
    }
    let file = &item.files[0];
    let path = file.path.as_str();
    let mut parts = path.split('/');
    let folder = parts.next().unwrap_or_default();
    let content_type = content_type_for_subdir(folder).ok_or_else(|| LauncherError::Generic {
        code: "ERR_CONTENT_ITEM_NOT_MINECRAFT".into(),
        message: format!(
            "'{path}' is not in a Minecraft content folder; this item cannot be added to an instance."
        ),
    })?;
    if content_type == "world" {
        return Err(LauncherError::Generic {
            code: "ERR_CONTENT_ITEM_NOT_SINGLE".into(),
            message: "Worlds are folders, not single files; a world cannot be added to an instance this way."
                .into(),
        });
    }
    let filename = parts
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_CONTENT_ITEM_NOT_MINECRAFT".into(),
            message: format!("'{path}' has no file name."),
        })?;
    if parts.next().is_some() {
        return Err(LauncherError::Generic {
            code: "ERR_CONTENT_ITEM_NOT_MINECRAFT".into(),
            message: format!("'{path}' is nested more deeply than a Minecraft content folder."),
        });
    }
    Ok((
        content_type.to_string(),
        filename.to_string(),
        file.sha256.clone(),
    ))
}

/// Add a stored item to a Minecraft instance, with no network, and record it in the manifest.
/// Adding the same item twice returns the existing entry.
pub fn add_item_to_instance(
    ctx: &Ctx,
    instance_id: &str,
    item_id: &str,
) -> LauncherResult<InstalledMod> {
    let sanitized = paths::sanitize_id(instance_id);
    let _guard = ctx.lock_manager.acquire(
        LockResource::Instance(sanitized.clone()),
        "add_stored_content",
    )?;
    let dir = ctx.paths.instance_dir(&sanitized)?;
    let manifest_path = ctx.paths.instance_manifest(&sanitized)?;
    if !manifest_path.exists() {
        return Err(LauncherError::Generic {
            code: "ERR_INSTANCE_NOT_FOUND".into(),
            message: format!("Instance '{instance_id}' not found."),
        });
    }
    let mut manifest = helpers::read_manifest(&manifest_path)?;
    let item = load_item(ctx, item_id)?;
    if let Some(game) = item.game_id.as_deref() {
        if game != "minecraft" {
            return Err(LauncherError::Generic {
                code: "ERR_CONTENT_ITEM_OTHER_GAME".into(),
                message: format!("This item belongs to '{game}', not Minecraft."),
            });
        }
    }
    let (content_type, filename, sha256) = stored_mod_fields(&item)?;
    // An item saved here before (or one whose game was not named) was the user's choice to add.
    let untagged = item.game_id.is_none();

    if let Some(existing) = manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter())
        .find(|entry| entry.content_item_id.as_deref() == Some(item.item_id.as_str()))
        .cloned()
    {
        let placed_name = if existing.enabled {
            existing.filename.clone()
        } else {
            format!("{}.disabled", existing.filename)
        };
        let target = dir
            .join(helpers::content_subdir(&existing.content_type))
            .join(&placed_name);
        if !target.is_file() {
            place_file(ctx, &sha256, &dir, &existing.content_type, &placed_name)?;
            let _ = snapshot::mark_instance_mutated(&dir);
        }
        return Ok(existing);
    }

    place_file(ctx, &sha256, &dir, &content_type, &filename)?;
    let target = dir
        .join(helpers::content_subdir(&content_type))
        .join(&filename);
    let metadata = crate::jar_metadata::parse_jar_metadata_for_loader(&target, &manifest.loader);
    // Restore the provenance the install recorded. A provider's full origin is rebuilt from the
    // opaque blob; without one, an item called "provider" becomes a manual mod rather than claiming
    // an origin that was not recorded. An item with no game id was the user's own choice.
    let origin = item
        .provider_origin
        .as_deref()
        .and_then(provider_origin_from_blob);
    let (provider, registry_id, modrinth_id, source) = if untagged {
        (None, None, None, "manual".to_string())
    } else if let Some(origin) = origin {
        (Some(origin), None, None, "provider".to_string())
    } else {
        match item.provider.as_deref() {
            Some("catalog") => (None, item.provider_id.clone(), None, "registry".to_string()),
            Some("modrinth") => (
                None,
                None,
                item.provider_id.clone(),
                "modrinth_raw".to_string(),
            ),
            Some("provider") => (None, None, None, "manual".to_string()),
            Some("manual") => (None, None, None, "manual".to_string()),
            _ => (
                None,
                item.catalog_id.clone(),
                None,
                "content_store".to_string(),
            ),
        }
    };
    let installed = InstalledMod {
        hash_verified: false,
        provider,
        update_pinned: false,
        pack_managed: false,
        installed_as_dependency: false,
        filename,
        registry_id,
        modrinth_id,
        source,
        source_url: item.page_url.clone(),
        version: item.version.clone(),
        sha256: sha256.clone(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        java_packages: metadata.java_packages,
        mod_jar_id: metadata.mod_jar_id,
        depends_on: metadata.depends_on,
        optional_deps: metadata.optional_deps,
        incompatible_deps: metadata.incompatible_deps,
        provided_mod_ids: metadata
            .provided_mods
            .into_iter()
            .map(|provided| provided.mod_id)
            .collect(),
        enabled: true,
        content_type,
        content_item_id: Some(item.item_id),
        object_sha256: Some(sha256),
    };
    helpers::push_to_content_array(&mut manifest, &installed);
    if let Err(error) = helpers::atomic_write_manifest(&manifest_path, &manifest) {
        // The placement is ours and has no manifest entry yet: take it back.
        let _ = std::fs::remove_file(&target);
        return Err(error);
    }
    let _ = snapshot::mark_instance_mutated(&dir);
    Ok(installed)
}

/// Point this context's HTTP clients at a closed local port. Any request made during a test that
/// installed this helper fails, so a test can prove a path made none.
#[cfg(test)]
pub fn forbid_network(ctx: &mut Ctx) {
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all("http://127.0.0.1:9").expect("a local proxy address is valid"))
        .timeout(std::time::Duration::from_millis(250))
        .build()
        .expect("test HTTP client");
    ctx.http_clients = http_client::HttpClients::for_testing(client);
}

/// Turn file sharing on (`true`) or off (`false`) for this context's placements.
#[cfg(test)]
pub fn set_sharing(ctx: &Ctx, share: bool) -> LauncherResult<()> {
    store::set_placement_mode(
        ctx,
        if share {
            store::PlacementMode::Link
        } else {
            store::PlacementMode::Copy
        },
    )
    .map_err(content_error)
}

/// Disable or enable an artifact through core's crash service.
#[cfg(test)]
pub fn set_artifact_enabled(
    ctx: &Ctx,
    instance_id: &str,
    filename: &str,
    enabled: bool,
) -> LauncherResult<()> {
    crate::datapack_sync::set_artifact_enabled(ctx, instance_id, filename, enabled)
}

/// Create the local state database a test context needs for settings.
#[cfg(test)]
pub fn init_local_state(ctx: &Ctx) {
    db::init_local_state_db(&ctx.paths.local_state_db()).expect("test local state");
}

/// How many items the content store holds. Test support only.
#[cfg(test)]
pub fn store_item_count(ctx: &Ctx) -> usize {
    store::list_items(ctx).expect("list store items").len()
}

/// Whether a folder's own DACL grants the current user delete-child. Test support only.
/// Only Windows has a folder ACL to ask about; on Unix the link's folder needs no extra right.
#[cfg(all(test, windows))]
pub fn folder_grants_delete_child(folder: &Path) -> bool {
    store::protect::grants_delete_child(folder).expect("read the folder ACL")
}

/// Store one file with review metadata and return its item id. Test support only.
#[cfg(test)]
pub fn store_review_item(ctx: &Ctx, source: &Path, rel: &str, game_id: Option<&str>) -> String {
    let outcome =
        store::add_file(ctx, source, Some("review"), Some(rel)).expect("store review item");
    let item = outcome.item().clone();
    store::set_item_metadata(
        ctx,
        &item.item_id,
        store::ItemMetadata {
            description: None,
            page_url: None,
            version: Some("1.0.0".into()),
            game_id: game_id.map(str::to_string),
            catalog_id: None,
            provider: None,
            provider_id: None,
            provider_origin: None,
            icon_sha256: None,
        },
    )
    .expect("set review metadata");
    item.item_id
}

#[cfg(test)]
mod review_store_metadata {
    use super::*;

    /// A PNG signature plus filler: enough for the library's image sniffing.
    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[3u8; 32]);
        bytes
    }

    #[test]
    fn storing_the_same_file_again_keeps_the_saved_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        let work = tempfile::tempdir().unwrap();
        let jar = work.path().join("cool.jar");
        std::fs::write(&jar, b"jar bytes for the review").unwrap();

        let details = StoreDetails {
            content_type: "mod",
            filename: "cool.jar",
            version: Some("1.0.0"),
            page_url: None,
            description: Some("Adds cool things"),
            catalog_id: None,
            provider: None,
            provider_id: None,
            provider_origin: None,
        };
        let first = store_file(&ctx, &jar, &details).unwrap();
        let icon = store::store_object_bytes(&ctx, &png()).unwrap();
        store::set_item_metadata(
            &ctx,
            &first.item_id,
            store::ItemMetadata {
                icon_sha256: Some(icon),
                ..Default::default()
            },
        )
        .unwrap();
        let before = store::get_item(&ctx, &first.item_id).unwrap();
        assert!(before.icon_sha256.is_some());

        // The same file installed again (second instance, or a reinstall) from a source that
        // does not know the icon.
        let again = StoreDetails {
            description: None,
            ..details
        };
        let second = store_file(&ctx, &jar, &again).unwrap();
        assert_eq!(
            first.item_id, second.item_id,
            "identical bytes are one item"
        );

        let after = store::get_item(&ctx, &first.item_id).unwrap();
        assert_eq!(
            after.icon_sha256, before.icon_sha256,
            "the saved icon was erased by re-storing the same file"
        );
    }
}
