//! Inventory rows for installed instance content.

use agora_core::helpers::{content_subdir, hash_file_sha256};
use agora_core::models::{InstalledMod, InstanceManifest};
use agora_core::registry::{RegistryItem, RegistryService};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CurationStatus {
    Curated,
    UnderReview,
    Uncurated,
    Archived,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataStatus {
    Complete,
    Partial,
    Unavailable,
}

/// What the file on disk looks like compared with the manifest entry, or a file Agora did not
/// install. Nothing here changes a file; it only tells the user what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// Present and still the bytes the manifest recorded.
    Ok,
    /// Listed in the manifest but not on disk (deleted by hand).
    Missing,
    /// Present, but its bytes are not the recorded ones (replaced or edited by hand).
    Modified,
    /// On disk in a content folder with no manifest entry (added by hand).
    Untracked,
}

/// A manifest entry enriched with local filesystem and cached registry data.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledContentRow {
    pub key: String,
    pub filename: String,
    pub display_name: String,
    pub version: Option<String>,
    pub content_type: String,
    pub enabled: bool,
    pub installed_at: String,
    pub source: String,
    pub source_label: String,
    /// Whether the pack contributed this entry, as opposed to the user adding
    /// it. Distinct from `source`, which mixes origin with acquisition method
    /// and is a display string.
    pub pack_managed: bool,
    /// Whether Agora installed this only to satisfy another mod's dependency.
    /// Drives the "why is this here?" affordance and orphan cleanup.
    pub installed_as_dependency: bool,
    /// Whether the user pinned this entry against updates.
    pub update_pinned: bool,
    pub source_url: Option<String>,
    pub registry_id: Option<String>,
    pub modrinth_id: Option<String>,
    pub mod_jar_id: Option<String>,
    pub loader_mod_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub file_present: bool,
    /// How the file on disk compares with its manifest entry, or `untracked` for a file Agora
    /// did not install. Detection never changes a file.
    pub file_state: FileState,
    pub resolved_path: Option<String>,
    pub author: Option<String>,
    pub categories: Vec<String>,
    pub icon_url: Option<String>,
    pub curation_status: CurationStatus,
    pub agora_score: Option<i64>,
    pub modrinth_downloads: Option<i64>,
    pub metadata_status: MetadataStatus,
    /// Which worlds a data pack is synced into. Present only on data pack rows
    /// (MASTER_SPEC §20.6).
    pub world_sync: Option<crate::datapack_sync::DatapackWorldStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledContentMetadata {
    pub key: String,
    pub display_name: Option<String>,
    pub icon_url: Option<String>,
    pub author: Option<String>,
}

/// Build all rows for an instance. Registry failures degrade to manifest and
/// filesystem data so a stale or missing cache never hides installed content.
pub fn list_installed_content(
    instance_dir: &Path,
    manifest: &InstanceManifest,
    content_type: Option<&str>,
    registry: Option<&RegistryService>,
) -> Vec<InstalledContentRow> {
    let entries = manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
        .chain(manifest.worlds.iter())
        .filter(|entry| content_type_matches(&entry.content_type, content_type));

    let entries: Vec<&InstalledMod> = entries.collect();
    let registry_items = registry
        .map(|service| {
            let ids = entries
                .iter()
                .filter_map(|entry| entry.registry_id.clone())
                .collect::<Vec<_>>();
            service.get_items_by_ids(&ids).unwrap_or_default()
        })
        .unwrap_or_default();
    let categories = registry
        .map(|service| {
            let ids = entries
                .iter()
                .filter_map(|entry| entry.registry_id.clone())
                .collect::<Vec<_>>();
            service.get_item_categories(&ids).unwrap_or_default()
        })
        .unwrap_or_default();
    let authors = registry
        .map(|service| {
            let ids = entries
                .iter()
                .filter_map(|entry| entry.registry_id.clone())
                .collect::<Vec<_>>();
            service.get_item_authors(&ids).unwrap_or_default()
        })
        .unwrap_or_default();

    let worlds = if entries
        .iter()
        .any(|entry| normalize_content_type(&entry.content_type) == "datapack")
    {
        crate::datapack_sync::list_worlds(instance_dir)
    } else {
        Vec::new()
    };

    let mut rows = entries
        .into_iter()
        .map(|entry| {
            let registry_item = entry
                .registry_id
                .as_ref()
                .and_then(|id| registry_items.get(id));
            let item_categories = entry
                .registry_id
                .as_ref()
                .and_then(|id| categories.get(id))
                .cloned()
                .unwrap_or_default();
            let author = entry
                .registry_id
                .as_ref()
                .and_then(|id| authors.get(id))
                .cloned();
            let mut row = build_row(instance_dir, entry, registry_item, item_categories, author);
            if row.content_type == "datapack" {
                row.world_sync = Some(crate::datapack_sync::world_status(manifest, entry, &worlds));
            }
            row
        })
        .collect::<Vec<_>>();
    rows.extend(untracked_rows(instance_dir, manifest, content_type));
    rows
}

/// Files sitting in a content folder with no manifest entry. Agora did not install them, so they
/// are shown (and nothing else) rather than hidden or removed.
fn untracked_rows(
    instance_dir: &Path,
    manifest: &InstanceManifest,
    content_type: Option<&str>,
) -> Vec<InstalledContentRow> {
    use std::collections::HashSet;

    let mut tracked = HashSet::new();
    for entry in manifest
        .mods
        .iter()
        .chain(manifest.resourcepacks.iter())
        .chain(manifest.shaders.iter())
        .chain(manifest.datapacks.iter())
    {
        let folder = content_subdir(&entry.content_type);
        tracked.insert(format!("{folder}/{}", entry.filename));
        tracked.insert(format!("{folder}/{}.disabled", entry.filename));
    }

    let mut rows = Vec::new();
    for folder in ["mods", "resourcepacks", "shaderpacks", "datapacks"] {
        let Some(kind) = content_type_for_folder(folder) else {
            continue;
        };
        if !content_type_matches(kind, content_type) {
            continue;
        }
        let dir = instance_dir.join(folder);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // A directory or a reparse point is not a content file Agora can reason about.
            if !std::fs::symlink_metadata(&path)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let rel = format!("{folder}/{name}");
            if tracked.contains(&rel) {
                continue;
            }
            let size = std::fs::metadata(&path).ok().map(|meta| meta.len());
            rows.push(InstalledContentRow {
                key: format!("untracked:{rel}"),
                filename: name.to_string(),
                display_name: filename_display_name(name),
                version: None,
                content_type: kind.to_string(),
                enabled: !name.ends_with(".disabled"),
                installed_at: String::new(),
                source: "manual".to_string(),
                source_label: "Added by hand".to_string(),
                pack_managed: false,
                installed_as_dependency: false,
                update_pinned: false,
                source_url: None,
                registry_id: None,
                modrinth_id: None,
                mod_jar_id: None,
                loader_mod_id: None,
                size_bytes: size,
                file_present: true,
                file_state: FileState::Untracked,
                resolved_path: Some(path.to_string_lossy().into_owned()),
                author: None,
                categories: vec!["Uncategorized".to_string()],
                icon_url: None,
                curation_status: CurationStatus::Unknown,
                agora_score: None,
                modrinth_downloads: None,
                metadata_status: MetadataStatus::Unavailable,
                world_sync: None,
            });
        }
    }
    rows.sort_by(|left, right| {
        left.content_type
            .cmp(&right.content_type)
            .then(left.filename.cmp(&right.filename))
    });
    rows
}

fn content_type_for_folder(folder: &str) -> Option<&'static str> {
    match folder {
        "mods" => Some("mod"),
        "resourcepacks" => Some("resourcepack"),
        "shaderpacks" => Some("shader"),
        "datapacks" => Some("datapack"),
        _ => None,
    }
}

fn content_type_matches(value: &str, requested: Option<&str>) -> bool {
    let Some(requested) = requested else {
        return true;
    };
    normalize_content_type(value) == normalize_content_type(requested)
}

fn normalize_content_type(value: &str) -> &str {
    match value {
        "resourcepacks" | "resource_pack" | "resource-packs" => "resourcepack",
        "shaderpacks" | "shaderpack" | "shaders" => "shader",
        "datapacks" | "data_pack" | "data-packs" => "datapack",
        "worlds" | "save" | "saves" => "world",
        _ => value,
    }
}

fn build_row(
    instance_dir: &Path,
    entry: &InstalledMod,
    registry_item: Option<&RegistryItem>,
    categories: Vec<String>,
    author: Option<String>,
) -> InstalledContentRow {
    let (resolved_path, size_bytes) = resolve_file(instance_dir, entry);
    let file_present = resolved_path.is_some();
    let file_state = file_state(entry, resolved_path.as_deref());
    let source_label = match &entry.provider {
        Some(origin) => provider_source_label(&origin.provider_id),
        None => source_label(&entry.source),
    };
    let curation_status = registry_item
        .map(|item| curation_status(&item.status))
        .unwrap_or(CurationStatus::Unknown);
    let metadata_status = if registry_item.is_some() {
        MetadataStatus::Complete
    } else if entry.registry_id.is_some() || entry.modrinth_id.is_some() {
        MetadataStatus::Partial
    } else {
        MetadataStatus::Unavailable
    };
    let display_name = registry_item
        .map(|item| item.name.clone())
        .unwrap_or_else(|| filename_display_name(&entry.filename));

    InstalledContentRow {
        key: content_key(entry),
        filename: entry.filename.clone(),
        display_name,
        version: entry.version.clone(),
        content_type: normalize_content_type(&entry.content_type).to_string(),
        enabled: entry.enabled,
        installed_at: entry.installed_at.clone(),
        source: entry.source.clone(),
        source_label,
        pack_managed: entry.pack_managed,
        installed_as_dependency: entry.installed_as_dependency,
        update_pinned: entry.update_pinned,
        source_url: entry.source_url.clone(),
        registry_id: entry.registry_id.clone(),
        modrinth_id: entry
            .modrinth_id
            .clone()
            .or_else(|| registry_item.and_then(|item| item.modrinth_id.clone())),
        mod_jar_id: entry.mod_jar_id.clone(),
        loader_mod_id: entry.mod_jar_id.clone(),
        size_bytes,
        file_present,
        file_state,
        resolved_path,
        author,
        categories: if categories.is_empty() {
            vec!["Uncategorized".to_string()]
        } else {
            categories
        },
        icon_url: registry_item.and_then(|item| item.icon_url.clone()),
        curation_status,
        agora_score: registry_item.map(|item| item.net_score),
        modrinth_downloads: None,
        metadata_status,
        world_sync: None,
    }
}

fn resolve_file(instance_dir: &Path, entry: &InstalledMod) -> (Option<String>, Option<u64>) {
    if !safe_filename(&entry.filename) {
        return (None, None);
    }
    let base = instance_dir.join(content_subdir(&entry.content_type));
    let candidates = if entry.enabled {
        vec![
            base.join(&entry.filename),
            base.join(format!("{}.disabled", entry.filename)),
        ]
    } else {
        vec![
            base.join(format!("{}.disabled", entry.filename)),
            base.join(&entry.filename),
        ]
    };
    candidates
        .into_iter()
        .find_map(|path| {
            std::fs::metadata(&path)
                .ok()
                .map(|metadata| (path, metadata.len()))
        })
        .map(|(path, size)| (Some(path.to_string_lossy().into_owned()), Some(size)))
        .unwrap_or((None, None))
}

/// Compare a present file with the hash its manifest entry recorded. A world is a folder, so it is
/// never hashed and never reported as changed.
fn file_state(entry: &InstalledMod, resolved_path: Option<&str>) -> FileState {
    let Some(path) = resolved_path else {
        return FileState::Missing;
    };
    if normalize_content_type(&entry.content_type) == "world" {
        return FileState::Ok;
    }
    let expected = entry
        .object_sha256
        .as_deref()
        .or(Some(entry.sha256.as_str()))
        .map(str::trim)
        .filter(|hash| !hash.is_empty());
    let Some(expected) = expected else {
        return FileState::Ok;
    };
    match hash_file_sha256(Path::new(path)) {
        Ok(actual) if actual.eq_ignore_ascii_case(expected) => FileState::Ok,
        Ok(_) => FileState::Modified,
        Err(_) => FileState::Missing,
    }
}

fn safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains('\0')
}

/// Stable identity of one installed entry; also the key of the Modrinth
/// project-metadata cache, so every consumer of that cache must build it here.
pub(crate) fn content_key(entry: &InstalledMod) -> String {
    format!(
        "{}:{}:{}",
        normalize_content_type(&entry.content_type),
        entry.filename,
        entry.sha256
    )
}

/// The name enrichment may show for an installed row. A catalog-backed row
/// keeps the catalog's display name, so the instance view matches Browse. A
/// Modrinth project title (for Iris, "Iris Shaders") only names a row the
/// catalog does not.
pub fn enriched_display_name(
    status: &MetadataStatus,
    modrinth_title: Option<String>,
) -> Option<String> {
    match status {
        MetadataStatus::Complete => None,
        MetadataStatus::Partial | MetadataStatus::Unavailable => modrinth_title,
    }
}

pub(crate) fn filename_display_name(filename: &str) -> String {
    Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(filename)
        .to_string()
}

/// Official providers by name; a plugin's provider by its plugin id, which is
/// what the user saw when they installed it.
fn provider_source_label(provider_id: &str) -> String {
    match provider_id {
        "modrinth" => "Modrinth".to_string(),
        "technic" => "Technic".to_string(),
        other => other.split('/').next().unwrap_or(other).to_string(),
    }
}

fn source_label(source: &str) -> String {
    let normalized = source.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    if normalized == "modrinth" || normalized == "modrinth_raw" {
        return "Modrinth".to_string();
    }
    if normalized == "modrinth_pack" {
        return "Modrinth Pack".to_string();
    }
    if normalized.contains("github") {
        return "GitHub Release".to_string();
    }
    if normalized == "registry" || normalized == "curated" {
        return "Agora Registry".to_string();
    }
    if normalized.contains("manual") || normalized == "local" {
        return "Manual".to_string();
    }
    "Other".to_string()
}

fn curation_status(status: &str) -> CurationStatus {
    match status.trim().to_ascii_lowercase().as_str() {
        "active" | "curated" => CurationStatus::Curated,
        "under_review" | "under-review" => CurationStatus::UnderReview,
        "archived" => CurationStatus::Archived,
        "uncurated" => CurationStatus::Uncurated,
        _ => CurationStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::models::CURRENT_MANIFEST_VERSION;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn manifest_entry(filename: &str, content_type: &str, enabled: bool) -> InstalledMod {
        InstalledMod {
            provider: None,
            update_pinned: false,
            pack_managed: false,
            installed_as_dependency: false,
            filename: filename.to_string(),
            registry_id: None,
            modrinth_id: None,
            source: "manual_drag_drop".to_string(),
            source_url: None,
            version: None,
            sha256: "hash".to_string(),
            hash_verified: true,
            installed_at: "not-a-timestamp".to_string(),
            java_packages: Vec::new(),
            mod_jar_id: None,
            provided_mod_ids: Vec::new(),
            enabled,
            content_type: content_type.to_string(),
            depends_on: Vec::new(),
            optional_deps: Vec::new(),
            incompatible_deps: Vec::new(),
            content_item_id: None,
            object_sha256: None,
        }
    }

    fn temp_dir() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agora-installed-content-{suffix}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn resolves_enabled_and_disabled_files_and_reports_missing_rows() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("mods")).unwrap();
        fs::create_dir_all(dir.join("resourcepacks")).unwrap();
        fs::write(dir.join("mods/enabled.jar"), b"12345").unwrap();
        fs::write(dir.join("mods/disabled.jar.disabled"), b"12").unwrap();
        fs::write(dir.join("resourcepacks/pack.zip"), b"x").unwrap();

        let mut manifest = InstanceManifest {
            manifest_version: CURRENT_MANIFEST_VERSION,
            game_data: Default::default(),
            pack_origin: None,
            instance_id: "test".to_string(),
            name: "Test".to_string(),
            created_from_pack: None,
            minecraft_version: "1.21".to_string(),
            loader: "fabric".to_string(),
            loader_version: "1".to_string(),
            is_locked: false,
            mods: vec![
                manifest_entry("enabled.jar", "mod", true),
                manifest_entry("disabled.jar", "mod", false),
                manifest_entry("missing.jar", "mod", true),
            ],
            resourcepacks: vec![manifest_entry("pack.zip", "resourcepack", true)],
            shaders: Vec::new(),
            datapacks: Vec::new(),
            worlds: Vec::new(),
            user_preferences: serde_json::Value::Object(Default::default()),
        };
        let rows = list_installed_content(&dir, &manifest, None, None);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].size_bytes, Some(5));
        assert_eq!(rows[1].size_bytes, Some(2));
        assert!(!rows[2].file_present);
        assert_eq!(rows[3].content_type, "resourcepack");
        assert_eq!(rows[3].source_label, "Manual");
        assert_eq!(provider_source_label("acme.cf/shelf"), "acme.cf");
        assert_eq!(provider_source_label("modrinth"), "Modrinth");
        manifest.mods.clear();
        let filtered = list_installed_content(&dir, &manifest, Some("resourcepack"), None);
        assert_eq!(filtered.len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mod_deleted_by_hand_shows_as_missing() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("mods")).unwrap();
        let manifest = InstanceManifest {
            manifest_version: CURRENT_MANIFEST_VERSION,
            game_data: Default::default(),
            pack_origin: None,
            instance_id: "test".to_string(),
            name: "Test".to_string(),
            created_from_pack: None,
            minecraft_version: "1.21".to_string(),
            loader: "fabric".to_string(),
            loader_version: "1".to_string(),
            is_locked: false,
            mods: vec![manifest_entry("deleted.jar", "mod", true)],
            resourcepacks: Vec::new(),
            shaders: Vec::new(),
            datapacks: Vec::new(),
            worlds: Vec::new(),
            user_preferences: serde_json::Value::Object(Default::default()),
        };

        let rows = list_installed_content(&dir, &manifest, None, None);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].file_present);
        assert_eq!(rows[0].file_state, FileState::Missing);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_jar_added_by_hand_shows_as_untracked() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("mods")).unwrap();
        let bytes = b"a jar Agora never installed";
        fs::write(dir.join("mods/hand-added.jar"), bytes).unwrap();
        let manifest = InstanceManifest {
            manifest_version: CURRENT_MANIFEST_VERSION,
            game_data: Default::default(),
            pack_origin: None,
            instance_id: "test".to_string(),
            name: "Test".to_string(),
            created_from_pack: None,
            minecraft_version: "1.21".to_string(),
            loader: "fabric".to_string(),
            loader_version: "1".to_string(),
            is_locked: false,
            mods: Vec::new(),
            resourcepacks: Vec::new(),
            shaders: Vec::new(),
            datapacks: Vec::new(),
            worlds: Vec::new(),
            user_preferences: serde_json::Value::Object(Default::default()),
        };

        let rows = list_installed_content(&dir, &manifest, Some("mod"), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "hand-added.jar");
        assert_eq!(rows[0].file_state, FileState::Untracked);
        assert_eq!(rows[0].source_label, "Added by hand");
        assert_eq!(rows[0].size_bytes, Some(bytes.len() as u64));
        // The content type filter still applies to anything added by hand.
        assert!(list_installed_content(&dir, &manifest, Some("shader"), None).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_catalog_named_row_keeps_its_catalog_name_over_modrinth_title() {
        assert_eq!(
            enriched_display_name(&MetadataStatus::Complete, Some("Iris Shaders".into())),
            None
        );
    }

    #[test]
    fn a_row_the_catalog_does_not_name_takes_the_modrinth_title() {
        assert_eq!(
            enriched_display_name(&MetadataStatus::Partial, Some("Sodium".into())),
            Some("Sodium".into())
        );
        assert_eq!(
            enriched_display_name(&MetadataStatus::Unavailable, Some("Sodium".into())),
            Some("Sodium".into())
        );
        assert_eq!(
            enriched_display_name(&MetadataStatus::Unavailable, None),
            None
        );
    }
}
