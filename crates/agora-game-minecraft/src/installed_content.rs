//! Inventory rows for installed instance content.

use agora_core::{
    content_store::place::same_file,
    crash_engine::attribution::{InstanceView, LayerOwner, LayerView},
    ctx::Ctx,
    error::{LauncherError, LauncherResult},
    helpers::{content_subdir, hash_file_sha256},
    models::{InstalledMod, InstanceManifest},
    registry::{RegistryItem, RegistryService},
};
use agora_game_api::crash::{CouldNotCheck, CouldNotCheckReason};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// On disk but cannot be read right now (another program holds it, or the read failed). It is
    /// not missing: the file is there.
    Unreadable,
    /// On disk in a content folder with no manifest entry (added by hand).
    Untracked,
}

/// The size, modification time and hash a file had when it was last verified. Unchanged since
/// then means unchanged bytes, so listing an instance does not read every jar again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedFileState {
    size: u64,
    modified_ms: i64,
    sha256: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileStateCache {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    files: std::collections::BTreeMap<String, CachedFileState>,
}

const FILE_STATE_CACHE_VERSION: u32 = 1;
const FILE_STATE_CACHE_PATH: &str = ".agora/content-state.json";

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
///
/// `content_objects_dir` is the shared store's object folder. A file that is the same inode as
/// its stored object cannot have changed (the object is protected), so it is never hashed. Other
/// files are hashed only when their size or modification time differs from the last verified
/// values in the instance's `.agora/content-state.json`.
pub fn list_installed_content(
    instance_dir: &Path,
    manifest: &InstanceManifest,
    content_type: Option<&str>,
    registry: Option<&RegistryService>,
    content_objects_dir: Option<&Path>,
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

    let mut inspector = FileStateInspector::new(instance_dir, content_objects_dir);
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
            let mut row = build_row(
                instance_dir,
                entry,
                registry_item,
                item_categories,
                author,
                &mut inspector,
            );
            if row.content_type == "datapack" {
                row.world_sync = Some(crate::datapack_sync::world_status(manifest, entry, &worlds));
            }
            row
        })
        .collect::<Vec<_>>();
    inspector.save();
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
    inspector: &mut FileStateInspector<'_>,
) -> InstalledContentRow {
    let (resolved_path, size_bytes) = resolve_file(instance_dir, entry);
    let file_present = resolved_path.is_some();
    let file_state = inspector.inspect(entry, resolved_path.as_deref());
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

/// The per-instance cache that lets an unchanged file avoid being hashed again.
struct FileStateInspector<'a> {
    instance_dir: &'a Path,
    content_objects_dir: Option<&'a Path>,
    cache_path: PathBuf,
    cache: FileStateCache,
    changed: bool,
}

impl<'a> FileStateInspector<'a> {
    fn new(instance_dir: &'a Path, content_objects_dir: Option<&'a Path>) -> Self {
        let cache_path = instance_dir.join(FILE_STATE_CACHE_PATH);
        let (cache, changed) = load_file_state_cache(&cache_path);
        Self {
            instance_dir,
            content_objects_dir,
            cache_path,
            cache,
            changed,
        }
    }

    /// Compare a present file with the hash its manifest entry recorded. A world is a folder, so
    /// it is never hashed and never reported as changed.
    fn inspect(&mut self, entry: &InstalledMod, resolved_path: Option<&str>) -> FileState {
        let Some(path) = resolved_path.map(Path::new) else {
            return FileState::Missing;
        };
        if normalize_content_type(&entry.content_type) == "world" {
            return FileState::Ok;
        }
        let Some(expected) = recorded_hash(entry) else {
            return FileState::Ok;
        };
        let Ok(metadata) = std::fs::metadata(path) else {
            return FileState::Missing;
        };
        if !metadata.is_file() {
            return FileState::Ok;
        }

        // A file that is the stored object itself cannot have been changed in place: the object
        // is protected. Its identity is the cheapest possible "unchanged" answer.
        if self
            .store_object(expected)
            .is_some_and(|object| same_file(path, &object))
        {
            return FileState::Ok;
        }

        let size = metadata.len();
        let modified = modified_ms(&metadata);
        let key = self.cache_key(path);
        if let Some(modified) = modified {
            if let Some(cached) = self.cache.files.get(&key) {
                if cached.size == size && cached.modified_ms == modified {
                    // Unchanged since the last hash: the cached hash answers the question.
                    return if cached.sha256.eq_ignore_ascii_case(expected) {
                        FileState::Ok
                    } else {
                        FileState::Modified
                    };
                }
            }
        }

        let actual = match hash_file_for_state(path) {
            Ok(actual) => actual,
            Err(_) => return FileState::Unreadable,
        };
        if let Some(modified) = modified {
            let verified = CachedFileState {
                size,
                modified_ms: modified,
                sha256: actual.clone(),
            };
            if self.cache.files.get(&key) != Some(&verified) {
                self.cache.files.insert(key, verified);
                self.changed = true;
            }
        }
        if actual.eq_ignore_ascii_case(expected) {
            FileState::Ok
        } else {
            FileState::Modified
        }
    }

    fn store_object(&self, hash: &str) -> Option<PathBuf> {
        let objects = self.content_objects_dir?;
        let hash = hash.to_ascii_lowercase();
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(objects.join(&hash[..2]).join(hash))
    }

    fn cache_key(&self, path: &Path) -> String {
        path.strip_prefix(self.instance_dir)
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
    }

    fn save(mut self) {
        if !self.changed {
            return;
        }
        self.cache.version = FILE_STATE_CACHE_VERSION;
        let Ok(bytes) = serde_json::to_vec(&self.cache) else {
            return;
        };
        if let Some(parent) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temp = self.cache_path.with_extension(format!(
            "json.{}.tmp",
            CACHE_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if std::fs::write(&temp, bytes).is_ok() && std::fs::rename(&temp, &self.cache_path).is_err()
        {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

static CACHE_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn recorded_hash(entry: &InstalledMod) -> Option<&str> {
    entry
        .object_sha256
        .as_deref()
        .or(Some(entry.sha256.as_str()))
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
}

/// A corrupt, old or unreadable cache is ignored and rebuilt; it is never trusted.
fn load_file_state_cache(path: &Path) -> (FileStateCache, bool) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<FileStateCache>(&bytes) {
            Ok(cache) if cache.version == FILE_STATE_CACHE_VERSION => (cache, false),
            _ => (FileStateCache::default(), true),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (FileStateCache::default(), false)
        }
        Err(_) => (FileStateCache::default(), true),
    }
}

fn modified_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).ok(),
        Err(error) => i64::try_from(error.duration().as_millis())
            .ok()
            .map(|millis| -millis),
    }
}

fn hash_file_for_state(path: &Path) -> std::io::Result<String> {
    #[cfg(test)]
    FILE_HASH_CALLS.with(|calls| calls.set(calls.get() + 1));
    hash_file_sha256(path)
}

#[cfg(test)]
thread_local! {
    static FILE_HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_hash_calls() {
    FILE_HASH_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn hash_calls() -> usize {
    FILE_HASH_CALLS.with(std::cell::Cell::get)
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

// ---------------------------------------------------------------------------
// The crash engine's view of an instance
// ---------------------------------------------------------------------------

/// A Minecraft instance as the crash engine reads it: every installed mod as a layer, plus what
/// the instance itself could not tell attribution.
pub struct CrashInstance {
    /// The game the instance runs, so the caller can pick the packs that apply.
    pub game_id: String,
    /// The game version the instance runs, when the manifest names one.
    pub runtime_version: Option<String>,
    pub view: InstanceView,
    /// Gaps the answer shows instead of reading them as a clean result.
    pub could_not_check: Vec<CouldNotCheck>,
}

/// Build the attribution view for a Minecraft instance, or `None` when `instance_id` is not one.
///
/// Every manifest mod becomes one layer, enabled or not, providing `mods/<filename>` and indexed
/// by its loader ids and Java packages. Missing package metadata is filled from the jars on disk
/// first, so a class can be traced to the mod that ships it; anything still missing is reported
/// once as a count rather than silently left out of the view.
pub fn crash_instance_view(ctx: &Ctx, instance_id: &str) -> LauncherResult<Option<CrashInstance>> {
    let Some(detail) =
        crate::instance_service::InstanceService::new(ctx.clone()).get(instance_id)?
    else {
        return Ok(None);
    };
    let Some(mut manifest) = detail.manifest else {
        return Err(LauncherError::Generic {
            code: "ERR_INSTANCE_MANIFEST_MISSING".into(),
            message: format!(
                "Instance '{instance_id}' has no instance manifest, so its installed mods cannot be listed."
            ),
        });
    };
    let instance_dir = ctx.paths.instance_dir(instance_id)?;
    // The manifest caches what a jar declares, and older instances may not have it. Reading the
    // jars is the existing way to fill it; the parse is cached on disk, so a warm instance is a
    // stat per file.
    crate::dependency_service::refresh_installed_jar_metadata(&instance_dir, &mut manifest.mods)?;

    let display_names = registry_display_names(ctx, &manifest);
    let (layers, mods_without_packages) = layers_for(&manifest, &display_names);
    let could_not_check = (mods_without_packages > 0)
        .then(|| no_packages_note(mods_without_packages))
        .into_iter()
        .collect();
    Ok(Some(CrashInstance {
        game_id: agora_game_api::GameId::minecraft().as_str().to_string(),
        runtime_version: Some(manifest.minecraft_version.clone()),
        view: InstanceView::new(layers),
        could_not_check,
    }))
}

/// One note for every mod whose packages could not be read, so a class from it is never silently
/// missing from attribution.
fn no_packages_note(count: usize) -> CouldNotCheck {
    CouldNotCheck {
        reason: CouldNotCheckReason::InstanceIncomplete,
        subject: format!(
            "{count} installed {}",
            if count == 1 { "mod" } else { "mods" }
        ),
        detail: Some(
            "no Java package is recorded for them, so a class they ship cannot be attributed to them"
                .to_string(),
        ),
        source: None,
    }
}

/// The name the instance view shows for each entry: the catalog's name when it has one, else the
/// file's own name, matching [`list_installed_content`].
fn registry_display_names(ctx: &Ctx, manifest: &InstanceManifest) -> HashMap<String, String> {
    let ids: Vec<String> = manifest
        .mods
        .iter()
        .filter_map(|entry| entry.registry_id.clone())
        .collect();
    let items = RegistryService::new(ctx.clone())
        .get_items_by_ids(&ids)
        .unwrap_or_default();
    manifest
        .mods
        .iter()
        .map(|entry| {
            let name = entry
                .registry_id
                .as_ref()
                .and_then(|id| items.get(id))
                .map(|item| item.name.clone())
                .unwrap_or_else(|| filename_display_name(&entry.filename));
            (content_key(entry), name)
        })
        .collect()
}

fn layers_for(
    manifest: &InstanceManifest,
    display_names: &HashMap<String, String>,
) -> (Vec<LayerView>, usize) {
    let mut layers = Vec::with_capacity(manifest.mods.len());
    let mut without_packages = 0;
    for entry in &manifest.mods {
        if entry.java_packages.is_empty() {
            without_packages += 1;
        }
        let key = content_key(entry);
        let name = display_names
            .get(&key)
            .cloned()
            .unwrap_or_else(|| filename_display_name(&entry.filename));
        let mut layer = LayerView::new(
            format!("mod:{}", entry.filename),
            entry.enabled,
            LayerOwner::Item { key, name },
            vec![format!("mods/{}", entry.filename)],
        );
        let mut mod_ids = Vec::new();
        if let Some(id) = &entry.mod_jar_id {
            mod_ids.push(id.clone());
        }
        mod_ids.extend(entry.provided_mod_ids.iter().cloned());
        if !mod_ids.is_empty() {
            layer = layer.with_index("mod_id", mod_ids);
        }
        if !entry.java_packages.is_empty() {
            layer = layer.with_index("java_package", entry.java_packages.clone());
        }
        layers.push(layer);
    }
    (layers, without_packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::{
        content_store::{add_file, place_object_at, PlacementMode},
        models::CURRENT_MANIFEST_VERSION,
    };
    use sha2::{Digest, Sha256};
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

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn manifest_with(entries: Vec<InstalledMod>) -> InstanceManifest {
        InstanceManifest {
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
            mods: entries,
            resourcepacks: Vec::new(),
            shaders: Vec::new(),
            datapacks: Vec::new(),
            worlds: Vec::new(),
            user_preferences: serde_json::Value::Object(Default::default()),
        }
    }

    fn entry_with_hash(filename: &str, sha256: &str) -> InstalledMod {
        let mut entry = manifest_entry(filename, "mod", true);
        entry.sha256 = sha256.to_string();
        entry
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
        let rows = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].size_bytes, Some(5));
        assert_eq!(rows[1].size_bytes, Some(2));
        assert!(!rows[2].file_present);
        assert_eq!(rows[3].content_type, "resourcepack");
        assert_eq!(rows[3].source_label, "Manual");
        assert_eq!(provider_source_label("acme.cf/shelf"), "acme.cf");
        assert_eq!(provider_source_label("modrinth"), "Modrinth");
        manifest.mods.clear();
        let filtered = list_installed_content(&dir, &manifest, Some("resourcepack"), None, None);
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

        let rows = list_installed_content(&dir, &manifest, None, None, None);
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

        let rows = list_installed_content(&dir, &manifest, Some("mod"), None, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filename, "hand-added.jar");
        assert_eq!(rows[0].file_state, FileState::Untracked);
        assert_eq!(rows[0].source_label, "Added by hand");
        assert_eq!(rows[0].size_bytes, Some(bytes.len() as u64));
        // The content type filter still applies to anything added by hand.
        assert!(list_installed_content(&dir, &manifest, Some("shader"), None, None).is_empty());
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

    #[test]
    fn listing_a_large_unchanged_instance_hashes_nothing() {
        let dir = temp_dir();
        let work = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(work.path().to_path_buf());
        fs::create_dir_all(dir.join("mods")).unwrap();

        let mut entries = Vec::new();
        let mut bytes_written = Vec::new();
        // One placed link and many ordinary copies: only the copies may ever be hashed.
        let linked_bytes = b"a linked mod whose object cannot change".repeat(8);
        let linked_source = work.path().join("linked-source.jar");
        fs::write(&linked_source, &linked_bytes).unwrap();
        let stored = add_file(
            &ctx,
            &linked_source,
            Some("linked"),
            Some("mods/linked.jar"),
        )
        .unwrap();
        let linked_hash = stored.item().files[0].sha256.clone();
        entries.push(entry_with_hash("linked.jar", &linked_hash));
        place_object_at(
            &ctx,
            &linked_hash,
            &dir.join("mods/linked.jar"),
            PlacementMode::Link,
        )
        .unwrap();

        for index in 0..120 {
            let filename = format!("mod-{index}.jar");
            let bytes = format!("the unchanged bytes of mod {index}").into_bytes();
            fs::write(dir.join("mods").join(&filename), &bytes).unwrap();
            entries.push(entry_with_hash(&filename, &sha256_hex(&bytes)));
            bytes_written.push(bytes);
        }
        let manifest = manifest_with(entries);
        let objects = ctx.paths.content_objects_dir();

        reset_hash_calls();
        let first = list_installed_content(&dir, &manifest, None, None, Some(&objects));
        assert_eq!(first.len(), 121);
        assert!(first.iter().all(|row| row.file_state == FileState::Ok));
        assert_eq!(
            hash_calls(),
            120,
            "only the ordinary copies are hashed; the link is known unchanged"
        );

        reset_hash_calls();
        let second = list_installed_content(&dir, &manifest, None, None, Some(&objects));
        assert_eq!(second.len(), 121);
        assert!(second.iter().all(|row| row.file_state == FileState::Ok));
        assert_eq!(hash_calls(), 0, "an unchanged instance re-hashes nothing");
        assert_eq!(bytes_written.len(), 120);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_file_is_hashed_and_reported() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("mods")).unwrap();
        let mut entries = Vec::new();
        for name in ["a.jar", "b.jar", "c.jar"] {
            let bytes = format!("the original bytes of {name}").into_bytes();
            fs::write(dir.join("mods").join(name), &bytes).unwrap();
            entries.push(entry_with_hash(name, &sha256_hex(&bytes)));
        }
        let manifest = manifest_with(entries);

        reset_hash_calls();
        let first = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(hash_calls(), 3, "the first listing verifies each file once");
        assert!(first.iter().all(|row| row.file_state == FileState::Ok));

        // A different size guarantees the change is seen even where the clock is coarse.
        fs::write(
            dir.join("mods/b.jar"),
            b"edited by hand, and longer than before",
        )
        .unwrap();
        reset_hash_calls();
        let second = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(hash_calls(), 1, "only the changed file is hashed");
        assert_eq!(second[1].filename, "b.jar");
        assert_eq!(second[1].file_state, FileState::Modified);
        assert_eq!(second[0].file_state, FileState::Ok);
        assert_eq!(second[2].file_state, FileState::Ok);

        reset_hash_calls();
        let third = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(hash_calls(), 0, "the new state is cached after hashing");
        assert_eq!(third[1].file_state, FileState::Modified);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_cache_is_rebuilt() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("mods")).unwrap();
        fs::create_dir_all(dir.join(".agora")).unwrap();
        fs::write(dir.join(".agora/content-state.json"), b"{ this is not json").unwrap();
        let bytes = b"a mod with only one file".to_vec();
        fs::write(dir.join("mods/only.jar"), &bytes).unwrap();
        let manifest = manifest_with(vec![entry_with_hash("only.jar", &sha256_hex(&bytes))]);

        reset_hash_calls();
        let first = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(hash_calls(), 1, "a corrupt cache is ignored, not trusted");
        assert_eq!(first[0].file_state, FileState::Ok);

        let rebuilt: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(".agora/content-state.json")).unwrap())
                .expect("the rebuilt cache is valid JSON");
        assert_eq!(rebuilt["version"], 1);
        assert!(rebuilt["files"]["mods/only.jar"].is_object());

        reset_hash_calls();
        let second = list_installed_content(&dir, &manifest, None, None, None);
        assert_eq!(hash_calls(), 0, "the rebuilt cache is used");
        assert_eq!(second[0].file_state, FileState::Ok);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_crash_view_makes_one_layer_per_installed_mod() {
        let mut package_mod = manifest_entry("example-brush.jar", "mod", true);
        package_mod.java_packages = vec!["com.example.painting".to_string()];
        package_mod.mod_jar_id = Some("examplebrush".to_string());
        package_mod.provided_mod_ids = vec!["examplebrushalias".to_string()];
        let mut disabled = manifest_entry("example-disabled.jar", "mod", false);
        disabled.java_packages = vec!["com.example.other".to_string()];
        let manifest = manifest_with(vec![package_mod, disabled]);

        let (layers, without_packages) = layers_for(&manifest, &HashMap::new());

        assert_eq!(without_packages, 0);
        assert_eq!(layers.len(), 2, "a turned-off mod still keeps its layer");
        assert_eq!(layers[0].files, ["mods/example-brush.jar"]);
        assert!(layers[0].enabled);
        assert_eq!(layers[1].files, ["mods/example-disabled.jar"]);
        assert!(!layers[1].enabled);
        match &layers[0].owner {
            LayerOwner::Item { key, name } => {
                assert_eq!(key, &content_key(&manifest.mods[0]));
                assert_eq!(name, "example-brush");
            }
            other => panic!("expected an item owner, got {other:?}"),
        }
        assert_eq!(
            layers[0].indexes["mod_id"],
            ["examplebrush", "examplebrushalias"]
        );
        assert_eq!(layers[0].indexes["java_package"], ["com.example.painting"]);
    }

    #[test]
    fn a_catalog_name_replaces_the_file_name_in_the_view() {
        let mut entry = manifest_entry("example-brush.jar", "mod", true);
        entry.registry_id = Some("acme.example-brush".to_string());
        let manifest = manifest_with(vec![entry.clone()]);
        let mut names = HashMap::new();
        names.insert(content_key(&entry), "Example Brush".to_string());

        let (layers, _) = layers_for(&manifest, &names);

        match &layers[0].owner {
            LayerOwner::Item { name, .. } => assert_eq!(name, "Example Brush"),
            other => panic!("expected an item owner, got {other:?}"),
        }
    }

    #[test]
    fn mods_without_packages_are_reported_once_with_their_count() {
        let manifest = manifest_with(vec![
            manifest_entry("known.jar", "mod", true),
            manifest_entry("unknown.jar", "mod", true),
        ]);
        let (_, without_packages) = layers_for(&manifest, &HashMap::new());
        assert_eq!(without_packages, 2);

        let note = no_packages_note(without_packages);
        assert_eq!(note.reason, CouldNotCheckReason::InstanceIncomplete);
        assert_eq!(note.subject, "2 installed mods");
        assert!(note
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("no Java package is recorded"));
    }
}
