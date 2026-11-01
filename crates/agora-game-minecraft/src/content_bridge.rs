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
    lock_manager::LockResource,
    models::InstalledMod,
    paths, snapshot,
};

/// What an install path already knows about the file it is storing.
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
}

/// The store item a file was added as, and the object it was placed from.
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
    let rel = format!(
        "{}/{}",
        subdir_for_content_type(details.content_type),
        details.filename
    );
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
    store::set_item_metadata(
        ctx,
        &stored.item_id,
        store::ItemMetadata {
            description: details.description.map(str::to_string),
            page_url: details.page_url.map(str::to_string),
            version: details.version.map(str::to_string),
            game_id: Some("minecraft".to_string()),
            catalog_id: details.catalog_id.map(str::to_string),
            provider: details.provider.map(str::to_string),
            provider_id: details.provider_id.map(str::to_string),
            icon_sha256: None,
        },
    )
    .map_err(content_error)?;
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
    if let Some(existing) = manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter())
        .find(|entry| entry.content_item_id.as_deref() == Some(item.item_id.as_str()))
    {
        return Ok(existing.clone());
    }
    let (content_type, filename, sha256) = stored_mod_fields(&item)?;
    place_file(ctx, &sha256, &dir, &content_type, &filename)?;
    let target = dir
        .join(helpers::content_subdir(&content_type))
        .join(&filename);
    let metadata = crate::jar_metadata::parse_jar_metadata_for_loader(&target, &manifest.loader);
    // Restore the provenance the install recorded, so a catalog item comes back as a catalog item
    // and a raw Modrinth item as a raw Modrinth item.
    let (registry_id, modrinth_id, source) = match item.provider.as_deref() {
        Some("catalog") => (item.provider_id.clone(), None, "registry"),
        Some("modrinth") => (None, item.provider_id.clone(), "modrinth_raw"),
        Some("provider") => (item.provider_id.clone(), None, "provider"),
        Some("manual") => (None, None, "manual"),
        _ => (item.catalog_id.clone(), None, "content_store"),
    };
    let installed = InstalledMod {
        hash_verified: false,
        provider: None,
        update_pinned: false,
        pack_managed: false,
        installed_as_dependency: false,
        filename,
        registry_id,
        modrinth_id,
        source: source.to_string(),
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
    helpers::atomic_write_manifest(&manifest_path, &manifest)?;
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
    ctx.http_clients = agora_core::http_client::HttpClients::for_testing(client);
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
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).expect("test local state");
}
