//! LOOT's masterlist for a Creation Engine game (MASTER_SPEC §26.6): fetching it when the user
//! asks, keeping the verified copy under the data root, and sorting an instance's plugins with
//! libloot.
//!
//! Nothing here runs in the background. [`update`] is the only function that reaches the network,
//! and it is called by an explicit "update masterlist" action. [`sort_and_notes`] uses only the
//! cached copy; when there is none the load-order sort falls back to masters alone.
//!
//! The masterlist branch is package data (`plugin_list.loot` in the game's `package.json`), not a
//! constant here. For Skyrim SE it is `loot/skyrimse`, the prelude is `loot/prelude`, and libloot
//! 0.29 expects the `v0.29` branch (LOOT's masterlists repository keeps one branch per libloot
//! minor, https://github.com/loot/loot#masterlists).
//!
//! The libloot facts this file relies on, from `libloot-0.29.7` in the cargo registry:
//! `Game::with_local_path` (`src/game.rs:201`), `Game::set_additional_data_paths`
//! (`src/game.rs:268`), `Game::database` (`src/game.rs:287`), `Game::load_plugin_headers`
//! (`src/game.rs:371`), `Game::sort_plugins` (`src/game.rs:444`), `Database::load_masterlist`
//! (`src/database/mod.rs:71`), `Database::load_masterlist_with_prelude`
//! (`src/database/mod.rs:80`), `Database::plugin_metadata` (`src/database/mod.rs:265`),
//! `MergeMode`/`EvalMode` (`src/database/mod.rs:26` and `:36`), `Message::content`
//! (`src/metadata/message.rs:206`), `select_message_content` (`src/metadata/message.rs:114`),
//! `PluginMetadata::messages` (`src/metadata/plugin_metadata.rs:100`),
//! `PluginMetadata::dirty_info` (`src/metadata/plugin_metadata.rs:110`), and
//! `PluginCleaningData::detail` (`src/metadata/plugin_cleaning_data.rs:118`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agora_game_api::{fold_path_case, GameDefinition, LootRule};
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::game_load_order::{Installed, LoadOrderEntry, LoadOrderError, NoteLevel, PluginNote};
use crate::http_client::{self, ClientCategory, HttpClients};

/// The GitHub contents API response fields used here. GitHub also sends many other fields.
#[derive(Debug, Deserialize)]
struct ContentsResponse {
    sha: String,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    encoding: Option<String>,
}

/// One downloaded and verified masterlist file.
struct VerifiedFile {
    bytes: Vec<u8>,
    blob_sha: String,
    source_url: String,
}

/// The cached masterlist's provenance, stored beside the YAML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasterlistStatus {
    /// When the copy was fetched, RFC 3339.
    pub fetched_at: String,
    /// The contents API URL the masterlist came from.
    pub source_url: String,
    /// The commit the branch pointed at when it was fetched.
    pub commit_sha: String,
    /// The git blob SHA-1 the bytes were checked against.
    pub blob_sha: String,
}

/// The verified files a sort may use.
#[derive(Debug, Clone)]
pub struct CachedMasterlist {
    pub masterlist: PathBuf,
    pub prelude: Option<PathBuf>,
    pub status: MasterlistStatus,
}

/// The facts LOOT found about one plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LootOutcome {
    /// Every plugin that was handed to libloot, in LOOT's order.
    pub order: Vec<String>,
    /// LOOT's messages for the instance's plugins, English text.
    pub notes: Vec<PluginNote>,
}

/// The directory holding a game's cached LOOT files.
fn game_dir(ctx: &Ctx, definition: &GameDefinition) -> PathBuf {
    ctx.paths.root().join("loot").join(definition.id.as_str())
}

fn status_path(ctx: &Ctx, definition: &GameDefinition) -> PathBuf {
    game_dir(ctx, definition).join("masterlist.json")
}

fn loot_rule(definition: &GameDefinition) -> Result<&LootRule, LoadOrderError> {
    definition
        .plugin_list
        .as_ref()
        .and_then(|rule| rule.loot.as_ref())
        .ok_or_else(|| LoadOrderError::NoLootMasterlist(definition.id.as_str().to_string()))
}

/// The masterlist cached for this game, if a previous update left one that still reads.
///
/// A missing status file means "never fetched" and is not an error. A status file that does not
/// parse is an error: the user asked for a verified masterlist and this one cannot be trusted.
pub fn cached(
    ctx: &Ctx,
    definition: &GameDefinition,
) -> Result<Option<CachedMasterlist>, LoadOrderError> {
    let rule = loot_rule(definition)?;
    let dir = game_dir(ctx, definition);
    let status_file = status_path(ctx, definition);
    let raw = match std::fs::read(&status_file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LoadOrderError::Io {
                path: status_file,
                source,
            })
        }
    };
    let status: MasterlistStatus =
        serde_json::from_slice(&raw).map_err(|error| LoadOrderError::Masterlist {
            reason: format!(
                "the saved masterlist record at {} cannot be read: {error}",
                status_file.display()
            ),
        })?;
    let masterlist = dir.join("masterlist.yaml");
    if !masterlist.exists() {
        return Ok(None);
    }
    let prelude = rule
        .prelude_repo
        .as_ref()
        .map(|_| dir.join("prelude.yaml"))
        .filter(|path| path.exists());
    Ok(Some(CachedMasterlist {
        masterlist,
        prelude,
        status,
    }))
}

/// Fetches bytes for a URL. Tests inject a fake so no test touches the network.
pub trait Fetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// The core HTTP client, used by every production update.
struct CoreFetcher<'a> {
    clients: &'a HttpClients,
}

impl Fetcher for CoreFetcher<'_> {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        http_client::blocking_checked_get_bytes(self.clients, ClientCategory::GitHub, url)
            .map_err(|error| error.to_string())
    }
}

fn contents_url(repo: &str, path: &str, branch: &str) -> String {
    format!(
        "https://api.github.com/repos/{repo}/contents/{}?ref={}",
        urlencoding::encode(path),
        urlencoding::encode(branch)
    )
}

fn commits_url(repo: &str, branch: &str) -> String {
    format!(
        "https://api.github.com/repos/{repo}/commits/{}",
        urlencoding::encode(branch)
    )
}

/// `sha1("blob " + length + "\0" + bytes)`, the way git names a blob.
fn git_blob_sha1(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn fetch_commit(fetcher: &dyn Fetcher, repo: &str, branch: &str) -> Result<String, LoadOrderError> {
    let url = commits_url(repo, branch);
    let bytes = fetcher
        .get(&url)
        .map_err(|reason| LoadOrderError::MasterlistDownload {
            url: url.clone(),
            reason,
        })?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| LoadOrderError::Masterlist {
            reason: format!("GitHub's answer for {repo} is not JSON: {error}"),
        })?;
    value
        .get("sha")
        .and_then(|sha| sha.as_str())
        .map(str::to_string)
        .ok_or_else(|| LoadOrderError::Masterlist {
            reason: format!("GitHub's answer for {repo} names no commit"),
        })
}

fn fetch_file(
    fetcher: &dyn Fetcher,
    repo: &str,
    branch: &str,
    path: &str,
) -> Result<VerifiedFile, LoadOrderError> {
    let api_url = contents_url(repo, path, branch);
    let bytes = fetcher
        .get(&api_url)
        .map_err(|reason| LoadOrderError::MasterlistDownload {
            url: api_url.clone(),
            reason,
        })?;
    let response: ContentsResponse =
        serde_json::from_slice(&bytes).map_err(|error| LoadOrderError::Masterlist {
            reason: format!("GitHub's contents answer for {repo}/{path} is not JSON: {error}"),
        })?;

    let source_url = match response.download_url.clone() {
        Some(url) => {
            let allowed = url.starts_with("https://raw.githubusercontent.com/")
                || url.starts_with("https://api.github.com/");
            if !allowed {
                return Err(LoadOrderError::MasterlistDownload {
                    url,
                    reason: "only GitHub over https is followed for a masterlist".to_string(),
                });
            }
            url
        }
        None => api_url.clone(),
    };
    let content = match (response.encoding.as_deref(), response.content.as_deref()) {
        (Some("base64"), Some(content)) => {
            use base64::{engine::general_purpose::STANDARD, Engine};
            STANDARD
                .decode(content)
                .map_err(|error| LoadOrderError::Masterlist {
                    reason: format!(
                        "GitHub's contents for {repo}/{path} is not valid base64: {error}"
                    ),
                })?
        }
        _ => fetcher
            .get(&source_url)
            .map_err(|reason| LoadOrderError::MasterlistDownload {
                url: source_url.clone(),
                reason,
            })?,
    };

    let actual = git_blob_sha1(&content);
    if !actual.eq_ignore_ascii_case(&response.sha) {
        return Err(LoadOrderError::MasterlistHash {
            repo: repo.to_string(),
            path: path.to_string(),
            expected: response.sha,
            actual,
        });
    }
    Ok(VerifiedFile {
        bytes: content,
        blob_sha: response.sha,
        source_url,
    })
}

/// Replace a game's cached masterlist, prelude and status JSON as one set.
///
/// Everything is written into a fresh staging folder first. Only when all three writes have
/// succeeded is that folder renamed into place (the old one is renamed aside first), so a failed
/// update leaves the previous complete set.
fn install_set(
    ctx: &Ctx,
    definition: &GameDefinition,
    masterlist: &[u8],
    prelude: Option<&[u8]>,
    status: &[u8],
) -> Result<(), LoadOrderError> {
    let dir = game_dir(ctx, definition);
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| LoadOrderError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let stamp = uuid::Uuid::new_v4();
    let staging = parent.join(format!(".staging-{stamp}"));
    let old = parent.join(format!(".old-{stamp}"));
    std::fs::create_dir_all(&staging).map_err(|source| LoadOrderError::Io {
        path: staging.clone(),
        source,
    })?;
    let written = (|| {
        std::fs::write(staging.join("masterlist.yaml"), masterlist).map_err(|source| {
            LoadOrderError::Io {
                path: staging.join("masterlist.yaml"),
                source,
            }
        })?;
        if let Some(prelude) = prelude {
            std::fs::write(staging.join("prelude.yaml"), prelude).map_err(|source| {
                LoadOrderError::Io {
                    path: staging.join("prelude.yaml"),
                    source,
                }
            })?;
        }
        std::fs::write(staging.join("masterlist.json"), status).map_err(|source| {
            LoadOrderError::Io {
                path: staging.join("masterlist.json"),
                source,
            }
        })
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let had_old = dir.exists();
    if had_old {
        std::fs::rename(&dir, &old).map_err(|source| {
            let _ = std::fs::remove_dir_all(&staging);
            LoadOrderError::Io {
                path: dir.clone(),
                source,
            }
        })?;
    }
    if let Err(source) = std::fs::rename(&staging, &dir) {
        if had_old {
            let _ = std::fs::rename(&old, &dir);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(LoadOrderError::Io { path: dir, source });
    }
    if had_old {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// Parse a downloaded masterlist (and prelude) with libloot before it may replace the cache.
fn validate_download(
    rule: &LootRule,
    masterlist: &[u8],
    prelude: Option<&[u8]>,
) -> Result<(), LoadOrderError> {
    let game_type = game_type(&rule.game_type).ok_or_else(|| LoadOrderError::Masterlist {
        reason: format!(
            "Agora does not know how to ask libloot for the game type '{}'",
            rule.game_type
        ),
    })?;
    let temp = TempGame::new()?;
    let game =
        libloot::Game::with_local_path(game_type, &temp.game, &temp.local).map_err(|error| {
            LoadOrderError::Masterlist {
                reason: format!("the downloaded masterlist cannot be checked: {error}"),
            }
        })?;
    let masterlist_path = temp.root.join("downloaded-masterlist.yaml");
    std::fs::write(&masterlist_path, masterlist).map_err(|source| LoadOrderError::Io {
        path: masterlist_path.clone(),
        source,
    })?;
    let prelude_path = match prelude {
        Some(bytes) => {
            let path = temp.root.join("downloaded-prelude.yaml");
            std::fs::write(&path, bytes).map_err(|source| LoadOrderError::Io {
                path: path.clone(),
                source,
            })?;
            Some(path)
        }
        None => None,
    };
    let database = game.database();
    let mut database = database
        .write()
        .map_err(|error| LoadOrderError::Masterlist {
            reason: format!("the downloaded masterlist cannot be checked: {error}"),
        })?;
    match prelude_path {
        Some(prelude) => database
            .load_masterlist_with_prelude(&masterlist_path, &prelude)
            .map_err(|error| LoadOrderError::Masterlist {
                reason: format!("the downloaded masterlist cannot be read: {error}"),
            }),
        None => {
            database
                .load_masterlist(&masterlist_path)
                .map_err(|error| LoadOrderError::Masterlist {
                    reason: format!("the downloaded masterlist cannot be read: {error}"),
                })
        }
    }
}

/// Download and verify the game's masterlist and prelude, then replace the cached copy.
///
/// Nothing is written until every file has been downloaded and its blob SHA-1 matches the one
/// GitHub reports, so a failed update leaves the previous copy in place.
pub fn update(ctx: &Ctx, definition: &GameDefinition) -> Result<MasterlistStatus, LoadOrderError> {
    let fetcher = CoreFetcher {
        clients: &ctx.http_clients,
    };
    update_with(ctx, definition, &fetcher)
}

/// [`update`] with an injected fetcher.
pub fn update_with(
    ctx: &Ctx,
    definition: &GameDefinition,
    fetcher: &dyn Fetcher,
) -> Result<MasterlistStatus, LoadOrderError> {
    let rule = loot_rule(definition)?.clone();
    let masterlist = fetch_file(
        fetcher,
        &rule.masterlist_repo,
        &rule.branch,
        "masterlist.yaml",
    )?;
    let prelude = match &rule.prelude_repo {
        Some(repo) => Some(fetch_file(fetcher, repo, &rule.branch, "prelude.yaml")?),
        None => None,
    };
    let commit_sha = fetch_commit(fetcher, &rule.masterlist_repo, &rule.branch)?;
    validate_download(
        &rule,
        &masterlist.bytes,
        prelude.as_ref().map(|file| file.bytes.as_slice()),
    )?;

    let status = MasterlistStatus {
        fetched_at: chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()).to_rfc3339(),
        source_url: masterlist.source_url,
        commit_sha,
        blob_sha: masterlist.blob_sha,
    };
    let json = serde_json::to_vec_pretty(&status).map_err(|error| LoadOrderError::Masterlist {
        reason: format!("the masterlist record cannot be written: {error}"),
    })?;
    install_set(
        ctx,
        definition,
        &masterlist.bytes,
        prelude.as_ref().map(|file| file.bytes.as_slice()),
        &json,
    )?;
    Ok(status)
}

fn game_type(name: &str) -> Option<libloot::GameType> {
    match name {
        "SkyrimSE" => Some(libloot::GameType::SkyrimSE),
        _ => None,
    }
}

/// A private directory for libloot's game handle. libloot wants a game folder and a local data
/// folder even when every plugin is given by absolute path.
struct TempGame {
    root: PathBuf,
    game: PathBuf,
    local: PathBuf,
}

impl TempGame {
    fn new() -> Result<Self, LoadOrderError> {
        let root = std::env::temp_dir().join(format!(
            "agora-loot-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let game = root.join("game");
        let local = root.join("local");
        std::fs::create_dir_all(game.join("Data")).map_err(|source| LoadOrderError::Io {
            path: game.clone(),
            source,
        })?;
        std::fs::create_dir_all(&local).map_err(|source| LoadOrderError::Io {
            path: local.clone(),
            source,
        })?;
        Ok(Self { root, game, local })
    }
}

impl Drop for TempGame {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn note_level(level: libloot::metadata::MessageType) -> NoteLevel {
    match level {
        libloot::metadata::MessageType::Error => NoteLevel::Error,
        libloot::metadata::MessageType::Warn => NoteLevel::Warn,
        libloot::metadata::MessageType::Say => NoteLevel::Info,
    }
}

/// Sort `entries` with libloot and the cached masterlist, and collect LOOT's messages.
///
/// libloot is pointed at the files the game will actually see: each plugin's absolute path comes
/// from the deployment plan (`installed`), so plugins living in different content layers are all
/// passed to one `Game` handle even though no single folder on disk holds them all. Their parent
/// folders are also given as additional data paths so LOOT's `file()` conditions can see the same
/// layered view. Only the headers are read; the plugin list itself stays Agora's.
/// A folder of links to the plugins the game will see, named as the game names them.
///
/// The deployment plan's sources are content-addressed files (no `.esp` suffix) and can live in
/// several layer folders. libloot validates a plugin's file extension, so the files are staged
/// here under their real names. A hard link is used when the volume allows it and a copy
/// otherwise; the staging folder sits under the data root, beside the cached masterlist.
struct PluginStage {
    dir: PathBuf,
    paths: Vec<PathBuf>,
}

impl PluginStage {
    fn new(
        cached: &CachedMasterlist,
        entries: &[LoadOrderEntry],
        installed: &Installed,
    ) -> Result<Self, LoadOrderError> {
        let parent = cached.masterlist.parent().unwrap_or_else(|| Path::new("."));
        let dir = parent.join(format!(".plugins-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(|source| LoadOrderError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in entries.iter().filter(|entry| entry.present) {
            let lower = fold_path_case(&entry.name);
            if !seen.insert(lower) {
                continue;
            }
            let Some(source) = installed.get(&fold_path_case(&entry.name)) else {
                continue;
            };
            let name = Path::new(&entry.name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(entry.name.as_str());
            let dest = dir.join(name);
            if std::fs::hard_link(source, &dest).is_err() {
                std::fs::copy(source, &dest).map_err(|source| LoadOrderError::Io {
                    path: dest.clone(),
                    source,
                })?;
            }
            paths.push(dest);
        }
        Ok(Self { dir, paths })
    }

    fn refs(&self) -> Vec<&Path> {
        self.paths.iter().map(PathBuf::as_path).collect()
    }
}

impl Drop for PluginStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Prepared {
    game: libloot::Game,
    _temp: TempGame,
    stage: PluginStage,
}

fn prepare_game(
    entries: &[LoadOrderEntry],
    installed: &Installed,
    rule: &LootRule,
    cached: &CachedMasterlist,
) -> Result<Prepared, LoadOrderError> {
    let game_type = game_type(&rule.game_type).ok_or_else(|| LoadOrderError::Masterlist {
        reason: format!(
            "Agora does not know how to ask libloot for the game type '{}'",
            rule.game_type
        ),
    })?;
    let temp = TempGame::new()?;
    let mut game =
        libloot::Game::with_local_path(game_type, &temp.game, &temp.local).map_err(|error| {
            LoadOrderError::Loot {
                reason: error.to_string(),
            }
        })?;

    let stage = PluginStage::new(cached, entries, installed)?;
    let mut parents: BTreeSet<PathBuf> = BTreeSet::new();
    parents.insert(stage.dir.clone());
    for path in &stage.paths {
        if let Some(parent) = path.parent() {
            parents.insert(parent.to_path_buf());
        }
    }
    game.set_additional_data_paths(parents.into_iter().collect())
        .map_err(|error| LoadOrderError::Loot {
            reason: error.to_string(),
        })?;

    {
        let database = game.database();
        let mut database = database.write().map_err(|error| LoadOrderError::Loot {
            reason: error.to_string(),
        })?;
        match &cached.prelude {
            Some(prelude) => database
                .load_masterlist_with_prelude(&cached.masterlist, prelude)
                .map_err(|error| LoadOrderError::Masterlist {
                    reason: format!(
                        "the masterlist at {} cannot be read: {error}",
                        cached.masterlist.display()
                    ),
                })?,
            None => database
                .load_masterlist(&cached.masterlist)
                .map_err(|error| LoadOrderError::Masterlist {
                    reason: format!(
                        "the masterlist at {} cannot be read: {error}",
                        cached.masterlist.display()
                    ),
                })?,
        }
    }
    Ok(Prepared {
        game,
        _temp: temp,
        stage,
    })
}

fn collect_notes(
    game: &libloot::Game,
    plugins: &[String],
) -> Result<Vec<PluginNote>, LoadOrderError> {
    let mut notes = Vec::new();
    let database = game.database();
    let database = database.read().map_err(|error| LoadOrderError::Loot {
        reason: error.to_string(),
    })?;
    for plugin in plugins {
        let metadata = database
            .plugin_metadata(
                plugin,
                libloot::MergeMode::WithoutUserMetadata,
                libloot::EvalMode::Evaluate,
            )
            .map_err(|error| LoadOrderError::Loot {
                reason: error.to_string(),
            })?;
        let Some(metadata) = metadata else { continue };
        for message in metadata.messages() {
            if let Some(content) =
                libloot::metadata::select_message_content(message.content(), "en")
            {
                notes.push(PluginNote {
                    plugin: plugin.clone(),
                    level: note_level(message.message_type()),
                    message: content.text().to_string(),
                });
            }
        }
        for cleaning in metadata.dirty_info() {
            let detail = libloot::metadata::select_message_content(cleaning.detail(), "en")
                .map(|content| content.text().to_string());
            if let Some(detail) = detail {
                notes.push(PluginNote {
                    plugin: plugin.clone(),
                    level: NoteLevel::Warn,
                    message: detail,
                });
            }
        }
    }
    Ok(notes)
}

/// LOOT's messages about the instance's plugins from the saved masterlist. No sorting.
pub fn notes(
    entries: &[LoadOrderEntry],
    installed: &Installed,
    rule: &LootRule,
    cached: &CachedMasterlist,
) -> Result<Vec<PluginNote>, LoadOrderError> {
    let mut prepared = prepare_game(entries, installed, rule, cached)?;
    prepared
        .game
        .load_plugin_headers(&prepared.stage.refs())
        .map_err(|error| LoadOrderError::Loot {
            reason: error.to_string(),
        })?;
    let names: Vec<String> = entries
        .iter()
        .filter(|entry| entry.present)
        .map(|entry| entry.name.clone())
        .collect();
    collect_notes(&prepared.game, &names)
}

/// Sort `entries` with libloot and the cached masterlist, and collect LOOT's messages.
///
/// libloot is pointed at the files the game will actually see: each plugin's absolute path comes
/// from the deployment plan (`installed`), so plugins living in different content layers are all
/// passed to one `Game` handle even though no single folder on disk holds them all. Their parent
/// folders are also given as additional data paths so LOOT's `file()` conditions can see the same
/// layered view. Only the headers are read; the plugin list itself stays Agora's.
pub fn sort_and_notes(
    entries: &[LoadOrderEntry],
    installed: &Installed,
    rule: &LootRule,
    cached: &CachedMasterlist,
) -> Result<LootOutcome, LoadOrderError> {
    let mut prepared = prepare_game(entries, installed, rule, cached)?;
    prepared
        .game
        .load_plugin_headers(&prepared.stage.refs())
        .map_err(|error| LoadOrderError::Loot {
            reason: error.to_string(),
        })?;

    let names: Vec<&str> = entries
        .iter()
        .filter(|entry| entry.present)
        .map(|entry| entry.name.as_str())
        .collect();
    let order = prepared
        .game
        .sort_plugins(&names)
        .map_err(|error| LoadOrderError::Loot {
            reason: error.to_string(),
        })?;

    let notes = collect_notes(&prepared.game, &order)?;
    Ok(LootOutcome { order, notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    struct FakeFetcher {
        responses: HashMap<String, Vec<u8>>,
    }

    impl FakeFetcher {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }
        fn with(mut self, url: &str, bytes: impl Into<Vec<u8>>) -> Self {
            self.responses.insert(url.to_string(), bytes.into());
            self
        }
    }

    impl Fetcher for FakeFetcher {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.responses
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no fake response for {url}"))
        }
    }

    fn contents_json(sha: &str, download_url: &str) -> Vec<u8> {
        serde_json::json!({
            "sha": sha,
            "download_url": download_url,
            "content": null,
            "encoding": "none"
        })
        .to_string()
        .into_bytes()
    }

    fn commit_json(sha: &str) -> Vec<u8> {
        serde_json::json!({ "sha": sha }).to_string().into_bytes()
    }

    fn test_definition() -> GameDefinition {
        use agora_game_api::{DeploymentStrategy, GameId, PluginListRule, RelPath};
        GameDefinition {
            mo2_game_name: None,
            id: GameId::new("loot-test-game").unwrap(),
            name: "LOOT Test Game".into(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::VirtualFileSystem,
            content_rules: vec![],
            content_layout: None,
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
            log_paths: vec![],
            crash_paths: vec![],
            user_files: vec![],
            save_paths: vec![],
            linked_archive_patterns: vec![],
            declared_writes: vec![],
            copy_patterns: vec![],
            excluded_paths: vec![],
            plugin_list: Some(PluginListRule {
                user_file: RelPath::new("user/Plugins.txt").unwrap(),
                plugin_folder: RelPath::new("Data").unwrap(),
                patterns: vec!["*.esp".into(), "*.esm".into()],
                active_prefix: "*".into(),
                header: vec![],
                semantics: Some("creation_engine".into()),
                implicit: vec![],
                implicit_list_file: None,
                loot: Some(LootRule {
                    masterlist_repo: "loot/skyrimse".into(),
                    prelude_repo: Some("loot/prelude".into()),
                    branch: "v0.29".into(),
                    game_type: "SkyrimSE".into(),
                }),
            }),
            launch_alternatives: vec![],
            runtime_files: vec![],
            save_location: vec![],
        }
    }

    #[test]
    fn the_blob_sha_is_the_one_git_uses() {
        assert_eq!(
            git_blob_sha1(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn a_wrong_blob_sha_is_refused_and_the_previous_cache_stays() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let old = b"plugins: []\n";
        let old_status = MasterlistStatus {
            fetched_at: "2026-01-01T00:00:00Z".into(),
            source_url: "old".into(),
            commit_sha: "old".into(),
            blob_sha: git_blob_sha1(old),
        };
        let loot_dir = game_dir(&ctx, &definition);
        std::fs::create_dir_all(&loot_dir).unwrap();
        std::fs::write(loot_dir.join("masterlist.yaml"), old).unwrap();
        std::fs::write(
            loot_dir.join("masterlist.json"),
            serde_json::to_vec(&old_status).unwrap(),
        )
        .unwrap();

        let main_api = contents_url("loot/skyrimse", "masterlist.yaml", "v0.29");
        let main_download = "https://raw.githubusercontent.com/loot/skyrimse/v0.29/masterlist.yaml";
        let prelude_api = contents_url("loot/prelude", "prelude.yaml", "v0.29");
        let prelude_download = "https://raw.githubusercontent.com/loot/prelude/v0.29/prelude.yaml";
        let fake = FakeFetcher::new()
            .with(&main_api, contents_json("0000", main_download))
            .with(main_download, b"bad masterlist\n".to_vec())
            .with(&prelude_api, contents_json("1111", prelude_download))
            .with(prelude_download, b"bad prelude\n".to_vec());

        let error = update_with(&ctx, &definition, &fake).unwrap_err();
        assert!(
            matches!(error, LoadOrderError::MasterlistHash { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(loot_dir.join("masterlist.yaml")).unwrap(),
            old
        );
        assert_eq!(
            cached(&ctx, &definition).unwrap().unwrap().status,
            old_status
        );
    }

    #[test]
    fn a_good_update_verifies_and_records_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let main = b"plugins: []\n";
        let prelude = b"plugins: []\n";
        let main_api = contents_url("loot/skyrimse", "masterlist.yaml", "v0.29");
        let main_download = "https://raw.githubusercontent.com/loot/skyrimse/v0.29/masterlist.yaml";
        let prelude_api = contents_url("loot/prelude", "prelude.yaml", "v0.29");
        let prelude_download = "https://raw.githubusercontent.com/loot/prelude/v0.29/prelude.yaml";
        let commit_api = commits_url("loot/skyrimse", "v0.29");
        let fake = FakeFetcher::new()
            .with(
                &main_api,
                contents_json(&git_blob_sha1(main), main_download),
            )
            .with(main_download, main.to_vec())
            .with(
                &prelude_api,
                contents_json(&git_blob_sha1(prelude), prelude_download),
            )
            .with(prelude_download, prelude.to_vec())
            .with(&commit_api, commit_json("deadbeef"));

        let status = update_with(&ctx, &definition, &fake).unwrap();
        assert_eq!(status.commit_sha, "deadbeef");
        assert_eq!(status.blob_sha, git_blob_sha1(main));
        let loot_dir = game_dir(&ctx, &definition);
        assert_eq!(
            std::fs::read(loot_dir.join("masterlist.yaml")).unwrap(),
            main
        );
        assert_eq!(
            std::fs::read(loot_dir.join("prelude.yaml")).unwrap(),
            prelude
        );
        let saved = cached(&ctx, &definition).unwrap().unwrap();
        assert_eq!(saved.status, status);
    }
    fn seed_cache(ctx: &Ctx, definition: &GameDefinition, bytes: &[u8]) -> MasterlistStatus {
        let status = MasterlistStatus {
            fetched_at: "2026-01-01T00:00:00Z".into(),
            source_url: "old".into(),
            commit_sha: "old".into(),
            blob_sha: git_blob_sha1(bytes),
        };
        let dir = game_dir(ctx, definition);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("masterlist.yaml"), bytes).unwrap();
        std::fs::write(
            dir.join("masterlist.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
        status
    }

    #[test]
    fn a_correctly_hashed_but_unparsable_masterlist_is_refused_and_the_old_cache_stays() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let old = b"plugins: []\n";
        let old_status = seed_cache(&ctx, &definition, old);
        let garbage = b"plugins: [unclosed\n  - name: 'A.esp'\n";

        let main_api = contents_url("loot/skyrimse", "masterlist.yaml", "v0.29");
        let main_download = "https://raw.githubusercontent.com/loot/skyrimse/v0.29/masterlist.yaml";
        let prelude_api = contents_url("loot/prelude", "prelude.yaml", "v0.29");
        let prelude_download = "https://raw.githubusercontent.com/loot/prelude/v0.29/prelude.yaml";
        let commit_api = commits_url("loot/skyrimse", "v0.29");
        let fake = FakeFetcher::new()
            .with(
                &main_api,
                contents_json(&git_blob_sha1(garbage), main_download),
            )
            .with(main_download, garbage.to_vec())
            .with(
                &prelude_api,
                contents_json(&git_blob_sha1(b"plugins: []\n"), prelude_download),
            )
            .with(prelude_download, b"plugins: []\n".to_vec())
            .with(&commit_api, commit_json("deadbeef"));

        let error = update_with(&ctx, &definition, &fake).unwrap_err();
        assert!(
            matches!(error, LoadOrderError::Masterlist { .. }),
            "{error}"
        );
        assert_eq!(
            std::fs::read(game_dir(&ctx, &definition).join("masterlist.yaml")).unwrap(),
            old
        );
        assert_eq!(
            cached(&ctx, &definition).unwrap().unwrap().status,
            old_status
        );
    }

    #[test]
    fn a_prelude_failure_leaves_the_old_masterlist() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let old = b"plugins: []\n";
        let old_status = seed_cache(&ctx, &definition, old);
        let main = b"plugins: []\n";
        let main_api = contents_url("loot/skyrimse", "masterlist.yaml", "v0.29");
        let main_download = "https://raw.githubusercontent.com/loot/skyrimse/v0.29/masterlist.yaml";
        let prelude_api = contents_url("loot/prelude", "prelude.yaml", "v0.29");
        let fake = FakeFetcher::new()
            .with(
                &main_api,
                contents_json(&git_blob_sha1(main), main_download),
            )
            .with(main_download, main.to_vec())
            .with(
                &prelude_api,
                contents_json("1111", "https://raw.example/no"),
            );

        let error = update_with(&ctx, &definition, &fake).unwrap_err();
        assert!(
            matches!(
                error,
                LoadOrderError::MasterlistHash { .. } | LoadOrderError::MasterlistDownload { .. }
            ),
            "{error}"
        );
        assert_eq!(
            std::fs::read(game_dir(&ctx, &definition).join("masterlist.yaml")).unwrap(),
            old
        );
        assert_eq!(
            cached(&ctx, &definition).unwrap().unwrap().status,
            old_status
        );
    }

    #[test]
    fn a_non_github_download_url_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let main_api = contents_url("loot/skyrimse", "masterlist.yaml", "v0.29");
        let fake = FakeFetcher::new().with(
            &main_api,
            contents_json("0000", "https://evil.example/masterlist.yaml"),
        );

        let error = update_with(&ctx, &definition, &fake).unwrap_err();
        assert!(
            matches!(error, LoadOrderError::MasterlistDownload { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("only GitHub over https"));
    }

    /// A Creation Engine plugin's bytes: a TES4 record with an HEDR subrecord and a MAST/DATA pair
    /// per master. Header layout as esplugin reads it for Skyrim SE.
    fn plugin_bytes(masters: &[&str]) -> Vec<u8> {
        fn subrecord(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(kind);
            out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
            out.extend_from_slice(data);
        }
        let mut body = Vec::new();
        subrecord(&mut body, b"HEDR", &[0u8; 12]);
        for master in masters {
            let mut name = master.as_bytes().to_vec();
            name.push(0);
            subrecord(&mut body, b"MAST", &name);
            subrecord(&mut body, b"DATA", &[0u8; 8]);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"TES4");
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&body);
        out
    }

    fn entry(name: &str, implicit: bool, locked: bool) -> LoadOrderEntry {
        LoadOrderEntry {
            name: name.to_string(),
            implicit,
            active: true,
            present: true,
            managed: false,
            locked,
            header_read: false,
            master: false,
            light: false,
            masters: Vec::new(),
            header_error: None,
        }
    }

    fn cached_with(dir: &Path, yaml: &str) -> CachedMasterlist {
        let path = dir.join("masterlist.yaml");
        std::fs::write(&path, yaml).unwrap();
        CachedMasterlist {
            masterlist: path,
            prelude: None,
            status: MasterlistStatus {
                fetched_at: "2026-01-01T00:00:00Z".into(),
                source_url: "fixture".into(),
                commit_sha: "fixture".into(),
                blob_sha: git_blob_sha1(yaml.as_bytes()),
            },
        }
    }

    fn installed(root: &Path, layers: &[(&str, &str)]) -> Installed {
        let mut map = Installed::new();
        for (layer, name) in layers {
            let dir = root.join(layer);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, plugin_bytes(&[])).unwrap();
            map.insert(fold_path_case(name), path);
        }
        map
    }

    fn rule() -> LootRule {
        LootRule {
            masterlist_repo: "loot/skyrimse".into(),
            prelude_repo: None,
            branch: "v0.29".into(),
            game_type: "SkyrimSE".into(),
        }
    }

    #[test]
    fn libloot_orders_plugins_by_a_masterlist_after_rule_in_two_layers() {
        let dir = tempfile::tempdir().unwrap();
        let cached = cached_with(
            dir.path(),
            "plugins:\n  - name: 'A.esp'\n    after:\n      - 'B.esp'\n",
        );
        let installed = installed(
            dir.path(),
            &[("layer-one", "A.esp"), ("layer-two", "B.esp")],
        );
        let entries = vec![entry("A.esp", false, false), entry("B.esp", false, false)];
        let outcome = sort_and_notes(&entries, &installed, &rule(), &cached).unwrap();
        assert_eq!(outcome.order, ["B.esp", "A.esp"]);
    }

    #[test]
    fn a_truncated_masterlist_is_an_error_not_an_empty_set_of_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cached = cached_with(dir.path(), "plugins: [unclosed\n  - name: 'A.esp'\n");
        let installed = installed(dir.path(), &[("layer", "A.esp")]);
        let entries = vec![entry("A.esp", false, false)];
        let error = sort_and_notes(&entries, &installed, &rule(), &cached).unwrap_err();
        assert!(
            matches!(error, LoadOrderError::Masterlist { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_masterlist_message_with_a_script_tag_stays_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let cached = cached_with(
            dir.path(),
            "plugins:\n  - name: 'A.esp'\n    msg:\n      - type: warn\n        content: '<script>alert(1)</script>'\n",
        );
        let installed = installed(dir.path(), &[("layer", "A.esp")]);
        let entries = vec![entry("A.esp", false, false)];
        let outcome = sort_and_notes(&entries, &installed, &rule(), &cached).unwrap();
        assert_eq!(outcome.notes.len(), 1);
        assert_eq!(outcome.notes[0].message, "<script>alert(1)</script>");
        assert_eq!(outcome.notes[0].level, NoteLevel::Warn);
    }

    #[test]
    fn no_saved_masterlist_is_none_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        assert!(cached(&ctx, &definition).unwrap().is_none());
    }

    /// GitHub's contents API wraps `content` at 60 base64 characters with `\n` and ends it with a
    /// newline. `base64::STANDARD.decode` rejects those newlines, so any file small enough to come
    /// inline (the prelude is) makes every update fail with "not valid base64".
    #[test]
    #[ignore = "proves bug: loot contents answers with newline-wrapped base64 are rejected (fetch_file decodes without stripping whitespace)"]
    fn a_newline_wrapped_base64_contents_answer_is_accepted() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        fn wrapped(bytes: &[u8]) -> String {
            let encoded = STANDARD.encode(bytes);
            let mut out = String::new();
            for chunk in encoded.as_bytes().chunks(60) {
                out.push_str(std::str::from_utf8(chunk).unwrap());
                out.push('\n');
            }
            out
        }
        fn inline_json(bytes: &[u8]) -> Vec<u8> {
            serde_json::json!({
                "sha": git_blob_sha1(bytes),
                "download_url": null,
                "content": wrapped(bytes),
                "encoding": "base64"
            })
            .to_string()
            .into_bytes()
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(dir.path().to_path_buf());
        let definition = test_definition();
        let main =
            b"plugins: []\n# padding so the encoded form is longer than one 60 character line\n";
        let fake = FakeFetcher::new()
            .with(
                &contents_url("loot/skyrimse", "masterlist.yaml", "v0.29"),
                inline_json(main),
            )
            .with(
                &contents_url("loot/prelude", "prelude.yaml", "v0.29"),
                inline_json(main),
            )
            .with(
                &commits_url("loot/skyrimse", "v0.29"),
                commit_json("deadbeef"),
            );
        update_with(&ctx, &definition, &fake)
            .expect("a normal GitHub inline (newline-wrapped base64) answer is accepted");
    }
}
