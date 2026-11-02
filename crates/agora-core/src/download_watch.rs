//! Noticing mod archives the player downloads (MASTER_SPEC §26.7).
//!
//! Agora does not use the Nexus Mods or CurseForge APIs. Players download mods in their browser, and
//! this module recognises those downloads by what is inside them, offline and without extracting.
//! It never imports anything automatically, and it never moves, changes or deletes the file: the
//! player chooses "Add to library" or "Ignore" in the Library.
//!
//! Recognition is per game package: Minecraft knows its own jars and packs, and a declarative game
//! package's `content_layout` says which entries belong to it. Core itself names no game.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use agora_game_api::{ContentLayout, DownloadAdd, DownloadRecognition};
use serde::{Deserialize, Serialize};

use crate::content_store::{self, ContentError, ItemMetadata};
use crate::ctx::Ctx;
use crate::error::LauncherError;
use crate::game_support;
use crate::settings::SettingsService;

/// "Notice mod downloads": off by default, and Agora only watches while this is on.
pub const NOTICE_DOWNLOADS_SETTING: &str = "notice_mod_downloads";
/// The folder to watch. Absent means the user's Downloads folder.
pub const DOWNLOADS_FOLDER_SETTING: &str = "mod_downloads_watch_folder";
/// Whether a batch of new downloads shows one toast. On by default; the player can turn it off.
pub const DOWNLOADS_TOAST_SETTING: &str = "mod_downloads_toast";
/// The SHA-256 of downloads the player chose to ignore.
pub const IGNORED_DOWNLOADS_SETTING: &str = "ignored_mod_download_hashes";
/// The downloads Agora recognised and has not yet acted on.
pub const DOWNLOAD_NOTICES_SETTING: &str = "mod_download_notices";

const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "7z", "rar", "jar", "mrpack"];
const PARTIAL_EXTENSIONS: &[&str] = &["crdownload", "part", "tmp", "download"];

#[derive(Debug, thiserror::Error)]
pub enum DownloadsError {
    #[error("{0}")]
    Settings(#[from] LauncherError),
    #[error("{0}")]
    Content(#[from] ContentError),
    #[error("cannot read the downloads folder '{folder}': {error}")]
    Folder { folder: String, error: String },
    #[error("the stored list of new downloads is damaged: {0}")]
    MalformedNotices(String),
    #[error("the stored list of ignored downloads is damaged: {0}")]
    MalformedIgnored(String),
    #[error("no new download has the hash {0}")]
    NotFound(String),
    #[error("the download '{0}' has changed since Agora saw it; scan again")]
    Changed(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<DownloadsError> for LauncherError {
    fn from(error: DownloadsError) -> Self {
        LauncherError::Generic {
            code: "ERR_DOWNLOADS".into(),
            message: error.to_string(),
        }
    }
}

/// One download Agora recognised, as it is kept between runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDownload {
    pub path: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub game_id: String,
    pub kind: String,
    pub reason: String,
    /// The Nexus mod id a matching file name suggests. A label only: it decides nothing.
    #[serde(default)]
    pub nexus_mod_id: Option<String>,
    pub found_at_unix_ms: i64,
}

/// One recognised download as the Library shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRow {
    pub path: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub game_id: String,
    pub kind: String,
    pub reason: String,
    pub nexus_mod_id: Option<String>,
    /// The store already holds these bytes, so the row is a note, not an offer.
    pub already_in_library: bool,
}

/// What adding a recognised download to the library did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AddDownloadOutcome {
    Added {
        item_id: String,
    },
    /// The archive has a FOMOD installer. The item is stored; the existing wizard asks the
    /// questions and installs the chosen variant.
    NeedsInstaller {
        item_id: String,
        title: String,
    },
}

/// A download classified by a game package, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedDownload {
    pub game_id: String,
    pub kind: String,
    pub reason: String,
    pub add: DownloadAdd,
}

/// One file a one-off scan (the CLI) would offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedDownload {
    pub path: String,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    pub game_id: String,
    pub kind: String,
    pub reason: String,
    pub nexus_mod_id: Option<String>,
    pub already_in_library: bool,
}

/// Whether the player has turned download watching on.
pub fn enabled(ctx: &Ctx) -> Result<bool, DownloadsError> {
    Ok(SettingsService::new(ctx.clone()).get_bool(NOTICE_DOWNLOADS_SETTING)?)
}

/// Whether a batch of new downloads may show a toast.
pub fn toast_enabled(ctx: &Ctx) -> Result<bool, DownloadsError> {
    Ok(SettingsService::new(ctx.clone()).get_bool_or(DOWNLOADS_TOAST_SETTING, true)?)
}

/// The folder to watch: the player's choice, else the platform Downloads folder.
pub fn folder(ctx: &Ctx) -> Result<PathBuf, DownloadsError> {
    let chosen = SettingsService::new(ctx.clone()).get_string(DOWNLOADS_FOLDER_SETTING)?;
    if let Some(chosen) = chosen.filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(chosen));
    }
    default_folder().ok_or_else(|| DownloadsError::Folder {
        folder: "Downloads".into(),
        error: "this system does not report a Downloads folder; choose one in Settings".into(),
    })
}

/// The platform Downloads folder, when the system reports one.
pub fn default_folder() -> Option<PathBuf> {
    dirs::download_dir()
}

/// The files the watcher considers, with the folder it started from.
///
/// The baseline is taken when the watch starts, so files already in the folder are never offered.
/// A file is only offered after two polls see the same size, which leaves a browser still writing
/// it alone.
#[derive(Debug, Clone)]
pub struct WatchState {
    folder: PathBuf,
    baseline: HashSet<String>,
    previous: HashMap<String, (u64, u128)>,
    published: HashSet<String>,
}

impl WatchState {
    /// Start watching the current folder. What is there now is the baseline, not an offer.
    pub fn start(ctx: &Ctx) -> Result<Self, DownloadsError> {
        let folder = folder(ctx)?;
        let baseline = read_folder(&folder)?
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        Ok(Self {
            folder,
            baseline,
            previous: HashMap::new(),
            published: HashSet::new(),
        })
    }

    /// Poll once. Returns the notices this poll added. While the setting is off this does nothing.
    pub fn scan(&mut self, ctx: &Ctx) -> Result<Vec<StoredDownload>, DownloadsError> {
        if !enabled(ctx)? {
            return Ok(Vec::new());
        }

        let folder = folder(ctx)?;
        if folder != self.folder {
            *self = Self::start(ctx)?;
            return Ok(Vec::new());
        }

        let ignored = stored_ignored(ctx)?;
        let mut notices = stored_notices(ctx)?;
        let mut added = Vec::new();

        for entry in read_folder(&self.folder)? {
            if !looks_like_archive(&entry.name) || is_partial(&entry.name) {
                continue;
            }
            if self.baseline.contains(&entry.name) || self.published.contains(&entry.name) {
                continue;
            }
            let Some(marker) = entry.marker else {
                continue;
            };
            if self.previous.get(&entry.name) != Some(&marker) {
                self.previous.insert(entry.name.clone(), marker);
                continue;
            }

            self.published.insert(entry.name.clone());

            let path = PathBuf::from(&entry.path);
            let Ok(sha256) = crate::helpers::hash_file_sha256(&path) else {
                continue;
            };
            if ignored.contains(&sha256) {
                continue;
            }
            let Some(classified) = classify_path(ctx, &path)? else {
                continue;
            };

            let nexus_mod_id = nexus_mod_id(&entry.name);
            let notice = StoredDownload {
                path: entry.path,
                file_name: entry.name,
                size: marker.0,
                sha256,
                game_id: classified.game_id,
                kind: classified.kind,
                reason: classified.reason,
                nexus_mod_id,
                found_at_unix_ms: content_store::now_unix_ms(),
            };
            notices.push(notice.clone());
            added.push(notice);
        }

        if !added.is_empty() {
            save_notices(ctx, &notices)?;
        }
        Ok(added)
    }
}

/// Classify one file by its contents against the games the player has enabled. Anything that is
/// not a readable archive, or matches no enabled game, is `None` and is ignored quietly.
pub fn classify_path(ctx: &Ctx, path: &Path) -> Result<Option<ClassifiedDownload>, DownloadsError> {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let entries = match content_store::list_archive_entries(path) {
        Ok(entries) => entries,
        // A file we cannot read as an archive is not content; it is never reported as an error or
        // offered. A genuine I/O failure on the folder is reported by `read_folder`.
        Err(_) => return Ok(None),
    };
    let enabled: HashSet<String> = game_support::list(ctx)
        .map_err(LauncherError::from)?
        .into_iter()
        .filter(|game| game.enabled)
        .map(|game| game.game_id)
        .collect();

    for game in ctx.games.games() {
        if !enabled.contains(game.id.as_str()) {
            continue;
        }
        let Some(package) = ctx.games.package_for(&game.id) else {
            continue;
        };
        if let Some(recognition) = package.recognize_download(&game.id, file_name, &entries) {
            return Ok(Some(ClassifiedDownload {
                game_id: game.id.as_str().to_string(),
                kind: recognition.kind,
                reason: recognition.reason,
                add: recognition.add,
            }));
        }
    }
    Ok(None)
}

/// A declarative game package's recognition, from its declared content layout. This is generic:
/// the layout's markers and data folder are the game's own, and nothing here names a game.
pub fn recognize_with_layout(
    entries: &[String],
    layout: &ContentLayout,
) -> Option<DownloadRecognition> {
    let entries: Vec<String> = entries
        .iter()
        .map(|entry| entry.replace('\\', "/"))
        .collect();
    let mut prefix: Vec<String> = Vec::new();

    for _ in 0..=8 {
        let mut folders = HashSet::new();
        let mut files = Vec::new();
        let mut top_level = Vec::new();
        let mut has_fomod = false;

        for entry in &entries {
            let parts: Vec<&str> = entry.split('/').filter(|part| !part.is_empty()).collect();
            if parts.len() <= prefix.len() {
                continue;
            }
            let matches_prefix = prefix
                .iter()
                .zip(parts.iter())
                .all(|(left, right)| left.eq_ignore_ascii_case(right));
            if !matches_prefix {
                continue;
            }
            let rest = &parts[prefix.len()..];
            if rest.len() == 1 {
                files.push(rest[0]);
            } else {
                folders.insert(rest[0]);
            }
            if rest.len() == 2
                && rest[0].eq_ignore_ascii_case("fomod")
                && rest[1].eq_ignore_ascii_case("ModuleConfig.xml")
            {
                has_fomod = true;
            }
            top_level.push(rest[0]);
        }

        if has_fomod {
            return Some(DownloadRecognition {
                kind: "FOMOD installer".into(),
                reason: "has fomod/ModuleConfig.xml".into(),
                add: DownloadAdd::Archive,
            });
        }

        let data_path = layout.data_path.as_str();
        let root_match = top_level.iter().find(|name| {
            (!data_path.is_empty() && name.eq_ignore_ascii_case(data_path))
                || layout
                    .root_markers
                    .iter()
                    .any(|marker| agora_game_api::glob_match(marker, name))
        });
        let data_match = top_level.iter().find(|name| {
            layout
                .data_markers
                .iter()
                .any(|marker| agora_game_api::glob_match(marker, name))
        });
        if let Some(name) = root_match.or(data_match) {
            return Some(DownloadRecognition {
                kind: "game content".into(),
                reason: format!("`{name}` matches the game's content layout"),
                add: DownloadAdd::Archive,
            });
        }

        if files.is_empty() && folders.len() == 1 {
            prefix.push(folders.into_iter().next().expect("one folder").to_string());
            continue;
        }
        return None;
    }
    None
}

/// The new downloads, with files that no longer exist pruned. Reads disk, never the network.
pub fn list(ctx: &Ctx) -> Result<Vec<DownloadRow>, DownloadsError> {
    let mut notices = stored_notices(ctx)?;
    let before = notices.len();
    notices.retain(|notice| Path::new(&notice.path).is_file());
    if notices.len() != before {
        save_notices(ctx, &notices)?;
    }
    let ignored = stored_ignored(ctx)?;

    let mut rows = Vec::new();
    for notice in notices {
        if ignored.contains(&notice.sha256) {
            continue;
        }
        let already_in_library = content_store::contains_hash(ctx, &notice.sha256)?;
        rows.push(DownloadRow {
            path: notice.path,
            file_name: notice.file_name,
            size: notice.size,
            sha256: notice.sha256,
            game_id: notice.game_id,
            kind: notice.kind,
            reason: notice.reason,
            nexus_mod_id: notice.nexus_mod_id,
            already_in_library,
        });
    }
    Ok(rows)
}

/// Forget a download by hash, now and on every future scan.
pub fn ignore(ctx: &Ctx, sha256: &str) -> Result<(), DownloadsError> {
    let sha256 = checked_hash(sha256)?;
    let mut ignored = stored_ignored(ctx)?;
    ignored.insert(sha256.clone());
    save_ignored(ctx, &ignored)?;

    let mut notices = stored_notices(ctx)?;
    notices.retain(|notice| notice.sha256 != sha256);
    save_notices(ctx, &notices)?;
    Ok(())
}

/// Store a recognised download through its game's own add path. The downloaded file is read and
/// never changed, moved or deleted.
pub fn add(ctx: &Ctx, sha256: &str) -> Result<AddDownloadOutcome, DownloadsError> {
    let sha256 = checked_hash(sha256)?;
    let mut notices = stored_notices(ctx)?;
    let notice = notices
        .iter()
        .find(|notice| notice.sha256 == sha256)
        .cloned()
        .ok_or_else(|| DownloadsError::NotFound(sha256.clone()))?;
    let path = PathBuf::from(&notice.path);
    if !path.is_file() {
        return Err(DownloadsError::NotFound(sha256));
    }
    let actual = crate::helpers::hash_file_sha256(&path)?;
    if actual != notice.sha256 {
        return Err(DownloadsError::Changed(notice.path));
    }
    let Some(classified) = classify_path(ctx, &path)? else {
        return Err(DownloadsError::Changed(notice.path));
    };

    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string());
    let outcome = match &classified.add {
        DownloadAdd::File { folder } => content_store::add_file(
            ctx,
            &path,
            name.as_deref(),
            Some(&format!("{folder}/{}", notice.file_name)),
        )?,
        DownloadAdd::Archive => content_store::add_archive(ctx, &path, name.as_deref())?,
    };
    let item = outcome.item();
    let item_id = item.item_id.clone();
    let metadata = ItemMetadata {
        description: item.description.clone(),
        page_url: item.page_url.clone(),
        version: item.version.clone(),
        game_id: Some(classified.game_id.clone()),
        catalog_id: item.catalog_id.clone(),
        provider: item.provider.clone(),
        provider_id: item.provider_id.clone(),
        provider_origin: item.provider_origin.clone(),
        icon_sha256: item.icon_sha256.clone(),
    };
    content_store::set_item_metadata(ctx, &item_id, metadata)?;

    let result = match &classified.add {
        DownloadAdd::Archive => match crate::content_fomod::parse(ctx, &item_id) {
            Ok(installer) => AddDownloadOutcome::NeedsInstaller {
                item_id: item_id.clone(),
                title: installer.module_name,
            },
            Err(crate::content_fomod::FomodError::NoInstaller(_)) => {
                AddDownloadOutcome::Added { item_id }
            }
            // A folder that looks like a FOMOD but cannot be read is a real problem; do not claim
            // the add succeeded as a plain archive.
            Err(error) => {
                return Err(DownloadsError::Content(ContentError::Other(format!(
                    "the installer in {item_id} could not be read: {error}"
                ))))
            }
        },
        DownloadAdd::File { .. } => AddDownloadOutcome::Added { item_id },
    };

    notices.retain(|stored| stored.sha256 != notice.sha256);
    save_notices(ctx, &notices)?;
    Ok(result)
}

/// A one-off classification pass for the CLI: every recognised file in `folder`, whether or not
/// watching is on. Nothing is stored and nothing is changed.
pub fn scan_folder_now(ctx: &Ctx, folder: &Path) -> Result<Vec<ScannedDownload>, DownloadsError> {
    let ignored = stored_ignored(ctx)?;
    let mut rows = Vec::new();
    for entry in read_folder(folder)? {
        if !looks_like_archive(&entry.name) || is_partial(&entry.name) {
            continue;
        }
        let path = PathBuf::from(&entry.path);
        let Ok(sha256) = crate::helpers::hash_file_sha256(&path) else {
            continue;
        };
        if ignored.contains(&sha256) {
            continue;
        }
        let Some(classified) = classify_path(ctx, &path)? else {
            continue;
        };
        let already_in_library = content_store::contains_hash(ctx, &sha256)?;
        rows.push(ScannedDownload {
            path: entry.path,
            file_name: entry.name.clone(),
            size: entry.marker.map(|marker| marker.0).unwrap_or_default(),
            sha256,
            game_id: classified.game_id,
            kind: classified.kind,
            reason: classified.reason,
            nexus_mod_id: nexus_mod_id(&entry.name),
            already_in_library,
        });
    }
    Ok(rows)
}

/// The Nexus mod id a download name suggests (`<name>-<modId>-<version>-<unixTimestamp>.<ext>`).
/// It is a label only and never decides what the file is.
pub fn nexus_mod_id(file_name: &str) -> Option<String> {
    let stem = Path::new(file_name).file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    if !parts.last()?.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // The rightmost numeric part before the timestamp is the mod id; the parts between it and the
    // timestamp are the version, which may itself contain dashes.
    let mod_index = (1..parts.len() - 2)
        .rev()
        .find(|&index| parts[index].bytes().all(|byte| byte.is_ascii_digit()))?;
    if parts[mod_index].is_empty() || parts[mod_index + 1..parts.len() - 1].is_empty() {
        return None;
    }
    if parts[..mod_index].join("-").is_empty() {
        return None;
    }
    Some(parts[mod_index].to_string())
}

// ---------------------------------------------------------------------------
// Folder reading and stored state
// ---------------------------------------------------------------------------

struct FolderEntry {
    name: String,
    path: String,
    marker: Option<(u64, u128)>,
}

fn read_folder(folder: &Path) -> Result<Vec<FolderEntry>, DownloadsError> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(DownloadsError::Folder {
                folder: folder.display().to_string(),
                error: error.to_string(),
            })
        }
    };
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| DownloadsError::Folder {
            folder: folder.display().to_string(),
            error: error.to_string(),
        })?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // A symlink is not a download Agora created or should follow.
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let marker = std::fs::metadata(entry.path()).ok().map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            (metadata.len(), modified)
        });
        rows.push(FolderEntry {
            name,
            path: entry.path().to_string_lossy().to_string(),
            marker,
        });
    }
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(rows)
}

fn looks_like_archive(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            ARCHIVE_EXTENSIONS
                .iter()
                .any(|allowed| extension.eq_ignore_ascii_case(allowed))
        })
        .unwrap_or(false)
}

fn is_partial(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            PARTIAL_EXTENSIONS
                .iter()
                .any(|partial| extension.eq_ignore_ascii_case(partial))
        })
        .unwrap_or(false)
}

fn stored_notices(ctx: &Ctx) -> Result<Vec<StoredDownload>, DownloadsError> {
    let value = SettingsService::new(ctx.clone()).get(DOWNLOAD_NOTICES_SETTING)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value)
            .map_err(|error| DownloadsError::MalformedNotices(error.to_string())),
    }
}

fn save_notices(ctx: &Ctx, notices: &[StoredDownload]) -> Result<(), DownloadsError> {
    let value = serde_json::to_value(notices)
        .map_err(|error| DownloadsError::MalformedNotices(error.to_string()))?;
    SettingsService::new(ctx.clone()).set(DOWNLOAD_NOTICES_SETTING, &value)?;
    Ok(())
}

fn stored_ignored(ctx: &Ctx) -> Result<HashSet<String>, DownloadsError> {
    let value = SettingsService::new(ctx.clone()).get(IGNORED_DOWNLOADS_SETTING)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(HashSet::new()),
        Some(serde_json::Value::Array(values)) => values
            .into_iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    DownloadsError::MalformedIgnored("expected a list of SHA-256 strings".into())
                })
            })
            .collect(),
        Some(_) => Err(DownloadsError::MalformedIgnored(
            "expected a list of SHA-256 strings".into(),
        )),
    }
}

fn save_ignored(ctx: &Ctx, ignored: &HashSet<String>) -> Result<(), DownloadsError> {
    let mut values: Vec<&str> = ignored.iter().map(String::as_str).collect();
    values.sort_unstable();
    let value = serde_json::json!(values);
    SettingsService::new(ctx.clone()).set(IGNORED_DOWNLOADS_SETTING, &value)?;
    Ok(())
}

fn checked_hash(sha256: &str) -> Result<String, DownloadsError> {
    let sha256 = sha256.trim().to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DownloadsError::NotFound(sha256));
    }
    Ok(sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nexus_label_reads_name_mod_id_version_and_timestamp() {
        assert_eq!(
            nexus_mod_id("JourneyMap-32274-5.9.7-1699999999.jar").as_deref(),
            Some("32274")
        );
        assert_eq!(
            nexus_mod_id("Some Mod-1234-1.0.0-beta-1700000000.zip").as_deref(),
            Some("1234")
        );
        assert_eq!(nexus_mod_id("my-mod.zip"), None);
        assert_eq!(nexus_mod_id("Alpha-42.7z"), None);
    }

    #[test]
    fn only_common_mod_extensions_are_looked_at() {
        for name in ["a.zip", "a.7Z", "a.rar", "a.jar", "a.mrpack"] {
            assert!(looks_like_archive(name), "{name}");
        }
        for name in ["a.txt", "a.exe", "a.zip.part", "a.zip.crdownload", "a"] {
            assert!(!looks_like_archive(name) || is_partial(name), "{name}");
        }
    }

    #[test]
    fn declarative_layout_recognises_fomod_and_markers() {
        let layout = ContentLayout {
            data_path: agora_game_api::RelPath::new("Data").unwrap(),
            data_markers: vec![
                "*.esp".into(),
                "*.esm".into(),
                "*.esl".into(),
                "*.bsa".into(),
                "meshes".into(),
                "textures".into(),
            ],
            root_markers: vec!["*.exe".into()],
            thunderstore_bepinex: false,
        };
        let fomod = recognize_with_layout(
            &["fomod/ModuleConfig.xml".into(), "meshes/x.nif".into()],
            &layout,
        )
        .expect("fomod");
        assert_eq!(fomod.kind, "FOMOD installer");
        assert_eq!(fomod.add, DownloadAdd::Archive);

        let modp = recognize_with_layout(&["Cool.esp".into(), "meshes/x.nif".into()], &layout)
            .expect("esp");
        assert_eq!(modp.kind, "game content");
        assert!(modp.reason.contains("Cool.esp"));

        let wrapped = recognize_with_layout(&["Cool/meshes/x.nif".into()], &layout)
            .expect("single wrapper folder is descended");
        assert_eq!(wrapped.kind, "game content");

        assert!(
            recognize_with_layout(&["readme.txt".into(), "photo.jpg".into()], &layout).is_none()
        );
    }
}
