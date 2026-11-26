//! Worlds and the multiplayer server list of one instance.
//!
//! Both live in files Minecraft owns: `saves/<folder>/level.dat` (gzipped NBT)
//! and `servers.dat` (plain NBT). Agora reads them to show what is there and
//! writes only the few fields the user edits here — a world's display name and
//! the server entries — leaving every other tag exactly as the game wrote it.
//!
//! Every change refuses to run while the game has the instance open, because
//! Minecraft rewrites both files on exit and would silently undo the edit.

use crate::instance_service::TrashFn;
use agora_core as core_api;
use agora_core::ctx::Ctx;
use agora_core::error::{LauncherError, LauncherResult};
use agora_core::instance_runtime::InstanceGuard;
use agora_game_api::{
    classify_entry_name, ArchiveError, ArchiveLimits, EntryName, NamePolicy, SafeArchive, Select,
};
use base64::Engine;
use core_api::app_paths::is_safe_file_name;
use fastnbt::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Icons larger than this are not inlined; the game writes 64×64 PNGs.
const MAX_ICON_BYTES: u64 = 256 * 1024;

/// One world as the Worlds page shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldInfo {
    /// Folder name under `saves/`. The stable identity; Quick Play uses it.
    pub folder: String,
    /// `LevelName` from `level.dat`, or the folder name when it cannot be read.
    pub name: String,
    /// Milliseconds since the epoch, as the game records it.
    pub last_played_ms: Option<i64>,
    /// `survival`, `creative`, `adventure` or `spectator`.
    pub game_mode: Option<String>,
    pub hardcore: bool,
    /// The Minecraft version that last saved the world, when recorded.
    pub version_name: Option<String>,
    pub size_bytes: u64,
    /// `icon.png` as a `data:` URL.
    pub icon_data_url: Option<String>,
}

/// A screenshot in the instance's `screenshots/` folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotInfo {
    pub file: String,
    pub size_bytes: u64,
    /// When the file was last written, RFC 3339.
    pub taken: Option<String>,
    /// Absolute path, for Show in folder.
    pub path: String,
}

/// Screenshots larger than this are not inlined into the page.
const MAX_SCREENSHOT_BYTES: u64 = 32 * 1024 * 1024;

fn is_screenshot(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
}

/// One entry of the in-game multiplayer list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    pub name: String,
    pub address: String,
    /// The server's favicon as the game cached it, as a `data:` URL.
    pub icon_data_url: Option<String>,
}

/// Something played recently, for Home's "Jump back in".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentPlay {
    /// `world` or `server`.
    pub kind: String,
    pub instance_id: String,
    pub instance_name: String,
    /// World folder or server address: what Quick Play needs.
    pub target: String,
    /// World name or server address, for display.
    pub label: String,
    pub last_played_ms: i64,
}

/// Settings key listing servers joined through Agora, newest first.
const RECENT_SERVERS_KEY: &str = "recent_quick_play_servers";
const RECENT_SERVERS_KEPT: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecentServer {
    instance_id: String,
    address: String,
    at_ms: i64,
}

/// Remember that a server was joined through Quick Play.
pub fn record_server_join(conn: &rusqlite::Connection, instance_id: &str, address: &str) {
    let mut recent: Vec<RecentServer> = agora_core::db::get_setting(conn, RECENT_SERVERS_KEY)
        .ok()
        .flatten()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    recent.retain(|entry| !(entry.instance_id == instance_id && entry.address == address));
    recent.insert(
        0,
        RecentServer {
            instance_id: instance_id.to_string(),
            address: address.to_string(),
            at_ms: chrono::Utc::now().timestamp_millis(),
        },
    );
    recent.truncate(RECENT_SERVERS_KEPT);
    let _ = agora_core::db::set_setting(conn, RECENT_SERVERS_KEY, &serde_json::json!(recent));
}

#[derive(Clone)]
pub struct WorldService {
    ctx: Ctx,
    /// The user's explicit answer to a running game: change its world anyway.
    /// The instance lock is still taken.
    allow_while_running: bool,
}

fn error(code: &str, message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: code.into(),
        message: message.into(),
    }
}

fn io_error(code: &str, what: &str, err: impl std::fmt::Display) -> LauncherError {
    error(code, format!("{what}: {err}"))
}

impl WorldService {
    pub fn new(ctx: Ctx) -> Self {
        Self {
            ctx,
            allow_while_running: false,
        }
    }

    /// Allow world changes while the instance's game is running. Only for a
    /// caller that has explicitly confirmed it (the CLI's `--while-running`
    /// flag or a desktop confirmation); the instance lock is still taken.
    pub fn while_running(mut self) -> Self {
        self.allow_while_running = true;
        self
    }

    fn instance_dir(&self, instance_id: &str) -> LauncherResult<(String, PathBuf)> {
        let id = core_api::paths::sanitize_id(instance_id);
        core_api::app_paths::validate_path_component(&id)?;
        let dir = self.ctx.paths.instance_dir(&id)?;
        if !dir.is_dir() {
            return Err(error(
                "ERR_INSTANCE_NOT_FOUND",
                format!("Instance '{instance_id}' not found."),
            ));
        }
        Ok((id, dir))
    }

    /// Lock the instance and make sure the game is not using it.
    fn mutate(
        &self,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<(InstanceGuard, PathBuf)> {
        let (id, dir) = self.instance_dir(instance_id)?;
        let guard = if self.allow_while_running {
            InstanceGuard::for_mutation_while_running(&self.ctx, &id, operation)?
        } else {
            InstanceGuard::for_mutation(&self.ctx, &id, operation)?
        };
        Ok((guard, dir))
    }

    fn world_dir(saves: &Path, folder: &str) -> LauncherResult<PathBuf> {
        let dir = saves.join(folder);
        if !is_safe_file_name(folder) || !dir.join("level.dat").is_file() {
            return Err(error(
                "ERR_WORLD_NOT_FOUND",
                format!("This instance has no world named '{folder}'."),
            ));
        }
        Ok(dir)
    }

    /// Every world in the instance, most recently played first.
    pub fn list(&self, instance_id: &str) -> LauncherResult<Vec<WorldInfo>> {
        let (_, dir) = self.instance_dir(instance_id)?;
        let Ok(entries) = std::fs::read_dir(dir.join("saves")) else {
            return Ok(Vec::new());
        };
        let mut worlds: Vec<WorldInfo> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("level.dat").is_file())
            .filter_map(|entry| {
                let folder = entry.file_name().into_string().ok()?;
                Some(read_world(&entry.path(), folder))
            })
            .collect();
        worlds.sort_by(|a, b| {
            b.last_played_ms
                .cmp(&a.last_played_ms)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(worlds)
    }

    /// The most recently played worlds across every instance, and servers
    /// joined through Agora, newest first.
    pub fn recent(&self, limit: usize) -> LauncherResult<Vec<RecentPlay>> {
        let conn = agora_core::db::local_state_connection(&self.ctx.paths.local_state_db())
            .map_err(|e| error("ERR_LOCAL_STATE_FAILED", e.to_string()))?;
        let instances = agora_core::db::list_instances(&conn)
            .map_err(|e| error("ERR_LOCAL_STATE_FAILED", e.to_string()))?;
        let names: HashMap<String, String> = instances
            .iter()
            .map(|row| (row.instance_id.clone(), row.name.clone()))
            .collect();
        let mut recent = Vec::new();
        for row in &instances {
            let Ok(dir) = self.ctx.paths.instance_dir(&row.instance_id) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(dir.join("saves")) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let Ok(folder) = entry.file_name().into_string() else {
                    continue;
                };
                let Some((name, played)) = world_summary(&entry.path(), &folder) else {
                    continue;
                };
                recent.push(RecentPlay {
                    kind: "world".into(),
                    instance_id: row.instance_id.clone(),
                    instance_name: row.name.clone(),
                    target: folder,
                    label: name,
                    last_played_ms: played,
                });
            }
        }
        let servers: Vec<RecentServer> = agora_core::db::get_setting(&conn, RECENT_SERVERS_KEY)
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        for server in servers {
            let Some(instance_name) = names.get(&server.instance_id) else {
                continue;
            };
            recent.push(RecentPlay {
                kind: "server".into(),
                instance_id: server.instance_id,
                instance_name: instance_name.clone(),
                label: server.address.clone(),
                target: server.address,
                last_played_ms: server.at_ms,
            });
        }
        recent.sort_by_key(|entry| std::cmp::Reverse(entry.last_played_ms));
        recent.truncate(limit);
        Ok(recent)
    }

    /// Change the name the game shows. The folder keeps its name, so data
    /// pack choices and Quick Play shortcuts that point at it keep working.
    pub fn rename(&self, instance_id: &str, folder: &str, new_name: &str) -> LauncherResult<()> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(error("ERR_WORLD_NAME", "A world needs a name."));
        }
        let (_guard, dir) = self.mutate(instance_id, "rename-world")?;
        let world = Self::world_dir(&dir.join("saves"), folder)?;
        set_level_name(&world.join("level.dat"), new_name)
    }

    /// Copy a world into a new folder next to it. Returns the new folder name.
    pub fn copy(&self, instance_id: &str, folder: &str) -> LauncherResult<String> {
        let (_guard, dir) = self.mutate(instance_id, "copy-world")?;
        let saves = dir.join("saves");
        let source = Self::world_dir(&saves, folder)?;
        let target_folder = free_folder_name(&saves, &format!("{folder} - Copy"));
        let target = saves.join(&target_folder);
        let staging = saves.join(format!(".{target_folder}.agora-copy"));
        let _ = std::fs::remove_dir_all(&staging);
        let result = copy_tree(&source, &staging).and_then(|()| {
            let name = read_world(&source, folder.to_string()).name;
            set_level_name(&staging.join("level.dat"), &format!("{name} (Copy)"))?;
            std::fs::rename(&staging, &target)
                .map_err(|e| io_error("ERR_WORLD_COPY", "Could not finish the copy", e))
        });
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result.map(|()| target_folder)
    }

    /// Move a world to the system's recycle bin.
    pub fn delete(&self, instance_id: &str, folder: &str, trash: &TrashFn) -> LauncherResult<()> {
        let (_guard, dir) = self.mutate(instance_id, "delete-world")?;
        let world = Self::world_dir(&dir.join("saves"), folder)?;
        trash(&world)
    }

    /// Write a world to a zip archive whose single top-level folder is the
    /// world, the layout the game and every launcher accept back.
    pub fn export(
        &self,
        instance_id: &str,
        folder: &str,
        destination: &Path,
    ) -> LauncherResult<()> {
        let (_guard, dir) = self.mutate(instance_id, "export-world")?;
        let world = Self::world_dir(&dir.join("saves"), folder)?;
        let partial = destination.with_extension("zip.partial");
        let result = write_world_zip(&world, folder, &partial).and_then(|()| {
            std::fs::rename(&partial, destination)
                .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not save the archive", e))
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    }

    /// Add a world from a zip archive. The archive may hold the world at its
    /// root or inside one folder. Returns the new folder name.
    pub fn import(&self, instance_id: &str, archive: &Path) -> LauncherResult<String> {
        let (_guard, dir) = self.mutate(instance_id, "import-world")?;
        let saves = dir.join("saves");
        std::fs::create_dir_all(&saves)
            .map_err(|e| io_error("ERR_WORLD_IMPORT", "Could not create saves/", e))?;
        let mut source = SafeArchive::open(archive, ArchiveLimits::WORLD_ARCHIVE)
            .map_err(|e| archive_error("Could not read the archive", e))?;
        let prefix = world_prefix(&source)?;
        let suggested = prefix
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| is_safe_file_name(name))
            .map(str::to_string)
            .or_else(|| {
                archive
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|name| is_safe_file_name(name))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "Imported World".into());
        let folder = free_folder_name(&saves, &suggested);
        let staging = saves.join(format!(".{folder}.agora-import"));
        let _ = std::fs::remove_dir_all(&staging);
        let world_root = if prefix.is_empty() {
            staging.clone()
        } else {
            staging.join(prefix.trim_end_matches('/'))
        };
        let result = extract_world(&mut source, &prefix, &staging).and_then(|()| {
            std::fs::rename(&world_root, saves.join(&folder))
                .map_err(|e| io_error("ERR_WORLD_IMPORT", "Could not finish the import", e))
        });
        if result.is_err() || world_root != staging {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result.map(|()| folder)
    }

    /// Screenshots, newest first.
    pub fn list_screenshots(&self, instance_id: &str) -> LauncherResult<Vec<ScreenshotInfo>> {
        let (_, dir) = self.instance_dir(instance_id)?;
        let Ok(entries) = std::fs::read_dir(dir.join("screenshots")) else {
            return Ok(Vec::new());
        };
        let mut shots: Vec<(std::time::SystemTime, ScreenshotInfo)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file = entry.file_name().into_string().ok()?;
                let meta = entry.metadata().ok()?;
                if !meta.is_file() || !is_screenshot(&file) {
                    return None;
                }
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                Some((
                    modified,
                    ScreenshotInfo {
                        path: entry.path().to_string_lossy().to_string(),
                        size_bytes: meta.len(),
                        taken: Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339()),
                        file,
                    },
                ))
            })
            .collect();
        shots.sort_by_key(|shot| std::cmp::Reverse(shot.0));
        Ok(shots.into_iter().map(|(_, info)| info).collect())
    }

    fn screenshot_path(&self, instance_id: &str, file: &str) -> LauncherResult<PathBuf> {
        let (_, dir) = self.instance_dir(instance_id)?;
        let path = dir.join("screenshots").join(file);
        if !is_safe_file_name(file) || !is_screenshot(file) || !path.is_file() {
            return Err(error(
                "ERR_SCREENSHOT_NOT_FOUND",
                "That screenshot is no longer there.",
            ));
        }
        Ok(path)
    }

    /// The image as a `data:` URL, for showing it in the page.
    pub fn screenshot_data_url(&self, instance_id: &str, file: &str) -> LauncherResult<String> {
        let path = self.screenshot_path(instance_id, file)?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size > MAX_SCREENSHOT_BYTES {
            return Err(error(
                "ERR_SCREENSHOT_TOO_LARGE",
                "That image is too large to preview.",
            ));
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| io_error("ERR_SCREENSHOT_READ", "Could not read the screenshot", e))?;
        let mime = if file.to_ascii_lowercase().ends_with(".png") {
            "image/png"
        } else {
            "image/jpeg"
        };
        Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }

    /// Rename a screenshot, keeping its extension. Returns the new file name.
    pub fn rename_screenshot(
        &self,
        instance_id: &str,
        file: &str,
        new_name: &str,
    ) -> LauncherResult<String> {
        let path = self.screenshot_path(instance_id, file)?;
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .to_string();
        let stem = new_name.trim();
        let stem = stem.strip_suffix(&format!(".{extension}")).unwrap_or(stem);
        let target_name = format!("{stem}.{extension}");
        if stem.is_empty() || !is_safe_file_name(&target_name) {
            return Err(error(
                "ERR_SCREENSHOT_NAME",
                "That name cannot be used for a file.",
            ));
        }
        let target = path.with_file_name(&target_name);
        if target.exists() {
            return Err(error(
                "ERR_SCREENSHOT_NAME",
                "A screenshot with that name already exists.",
            ));
        }
        std::fs::rename(&path, &target).map_err(|e| {
            io_error(
                "ERR_SCREENSHOT_RENAME",
                "Could not rename the screenshot",
                e,
            )
        })?;
        Ok(target_name)
    }

    /// Move a screenshot to the system recycle bin.
    pub fn delete_screenshot(
        &self,
        instance_id: &str,
        file: &str,
        trash: &TrashFn,
    ) -> LauncherResult<()> {
        let path = self.screenshot_path(instance_id, file)?;
        trash(&path)
    }

    pub fn list_servers(&self, instance_id: &str) -> LauncherResult<Vec<ServerEntry>> {
        let (_, dir) = self.instance_dir(instance_id)?;
        Ok(read_servers(&dir.join("servers.dat"))?
            .iter()
            .map(server_entry)
            .collect())
    }

    /// Add a server to the end of the list, as the game's own Add Server does.
    pub fn add_server(&self, instance_id: &str, name: &str, address: &str) -> LauncherResult<()> {
        let (name, address) = validate_server(name, address)?;
        self.edit_servers(instance_id, |servers| {
            let mut entry = HashMap::new();
            entry.insert("name".into(), Value::String(name));
            entry.insert("ip".into(), Value::String(address));
            servers.push(Value::Compound(entry));
            Ok(())
        })
    }

    /// Change one entry's name and address, keeping its cached icon and any
    /// other tags the game stored on it.
    pub fn update_server(
        &self,
        instance_id: &str,
        index: usize,
        name: &str,
        address: &str,
    ) -> LauncherResult<()> {
        let (name, address) = validate_server(name, address)?;
        self.edit_servers(instance_id, |servers| {
            let Some(Value::Compound(entry)) = servers.get_mut(index) else {
                return Err(missing_server());
            };
            if entry.get("ip").and_then(Value::as_str) != Some(address.as_str()) {
                // The icon belongs to the old address.
                entry.remove("icon");
            }
            entry.insert("name".into(), Value::String(name));
            entry.insert("ip".into(), Value::String(address));
            Ok(())
        })
    }

    pub fn remove_server(&self, instance_id: &str, index: usize) -> LauncherResult<()> {
        self.edit_servers(instance_id, |servers| {
            if index >= servers.len() {
                return Err(missing_server());
            }
            servers.remove(index);
            Ok(())
        })
    }

    /// Move an entry one place up (`-1`) or down (`1`).
    pub fn move_server(&self, instance_id: &str, index: usize, offset: i32) -> LauncherResult<()> {
        self.edit_servers(instance_id, |servers| {
            let target = index as i64 + i64::from(offset);
            if index >= servers.len() || target < 0 || target as usize >= servers.len() {
                return Err(missing_server());
            }
            servers.swap(index, target as usize);
            Ok(())
        })
    }

    fn edit_servers(
        &self,
        instance_id: &str,
        change: impl FnOnce(&mut Vec<Value>) -> LauncherResult<()>,
    ) -> LauncherResult<()> {
        let (_guard, dir) = self.mutate(instance_id, "edit-servers")?;
        let path = dir.join("servers.dat");
        let mut servers = read_servers(&path)?;
        change(&mut servers)?;
        write_servers(&path, servers)
    }
}

fn missing_server() -> LauncherError {
    error(
        "ERR_SERVER_NOT_FOUND",
        "That server is no longer in the list.",
    )
}

fn validate_server(name: &str, address: &str) -> LauncherResult<(String, String)> {
    let address = address.trim();
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return Err(error(
            "ERR_SERVER_ADDRESS",
            "That server address is not valid.",
        ));
    }
    let name = match name.trim() {
        "" => "Minecraft Server",
        name => name,
    };
    Ok((name.to_string(), address.to_string()))
}

fn read_world(dir: &Path, folder: String) -> WorldInfo {
    let data = read_level_dat(&dir.join("level.dat"))
        .ok()
        .and_then(|root| match root {
            Value::Compound(mut root) => match root.remove("Data") {
                Some(Value::Compound(data)) => Some(data),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_default();
    let int = |key: &str| match data.get(key) {
        Some(Value::Byte(v)) => Some(i64::from(*v)),
        Some(Value::Short(v)) => Some(i64::from(*v)),
        Some(Value::Int(v)) => Some(i64::from(*v)),
        Some(Value::Long(v)) => Some(*v),
        _ => None,
    };
    let name = data
        .get("LevelName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| folder.clone());
    let version_name = match data.get("Version") {
        Some(Value::Compound(version)) => version
            .get("Name")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    };
    let game_mode = int("GameType").and_then(|mode| match mode {
        0 => Some("survival".to_string()),
        1 => Some("creative".to_string()),
        2 => Some("adventure".to_string()),
        3 => Some("spectator".to_string()),
        _ => None,
    });
    WorldInfo {
        name,
        last_played_ms: int("LastPlayed"),
        game_mode,
        hardcore: int("hardcore").is_some_and(|value| value != 0),
        version_name,
        size_bytes: tree_size(dir),
        icon_data_url: png_data_url(&dir.join("icon.png")),
        folder,
    }
}

/// Name and last-played time of a world, without measuring it.
pub(crate) fn world_summary(dir: &Path, folder: &str) -> Option<(String, i64)> {
    let root = read_level_dat(&dir.join("level.dat")).ok()?;
    let Value::Compound(mut root) = root else {
        return None;
    };
    let Some(Value::Compound(data)) = root.remove("Data") else {
        return None;
    };
    let played = match data.get("LastPlayed") {
        Some(Value::Long(ms)) => *ms,
        _ => return None,
    };
    let name = data
        .get("LevelName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(folder)
        .to_string();
    Some((name, played))
}

fn png_data_url(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_ICON_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    bytes.starts_with(b"\x89PNG").then(|| {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    })
}

fn tree_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

fn read_level_dat(path: &Path) -> LauncherResult<Value> {
    let file = std::fs::File::open(path)
        .map_err(|e| io_error("ERR_WORLD_READ", "Could not open level.dat", e))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(file)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("ERR_WORLD_READ", "level.dat is not readable", e))?;
    fastnbt::from_bytes(&bytes).map_err(|e| io_error("ERR_WORLD_READ", "level.dat is damaged", e))
}

fn set_level_name(path: &Path, name: &str) -> LauncherResult<()> {
    let mut root = read_level_dat(path)?;
    let Some(Value::Compound(data)) = (match &mut root {
        Value::Compound(root) => root.get_mut("Data"),
        _ => None,
    }) else {
        return Err(error("ERR_WORLD_READ", "level.dat has no Data section."));
    };
    data.insert("LevelName".into(), Value::String(name.to_string()));
    let bytes = fastnbt::to_bytes(&root)
        .map_err(|e| io_error("ERR_WORLD_WRITE", "Could not encode level.dat", e))?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&bytes)
        .map_err(|e| io_error("ERR_WORLD_WRITE", "Could not compress level.dat", e))?;
    let compressed = encoder
        .finish()
        .map_err(|e| io_error("ERR_WORLD_WRITE", "Could not compress level.dat", e))?;
    // The game keeps the previous file as level.dat_old; do the same so a
    // bad write is recoverable the way players already know.
    let _ = std::fs::copy(path, path.with_file_name("level.dat_old"));
    write_atomically(path, &compressed, "ERR_WORLD_WRITE")
}

fn read_servers(path: &Path) -> LauncherResult<Vec<Value>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(io_error(
                "ERR_SERVERS_READ",
                "Could not open servers.dat",
                e,
            ))
        }
    };
    let root: Value = fastnbt::from_bytes(&bytes)
        .map_err(|e| io_error("ERR_SERVERS_READ", "servers.dat is damaged", e))?;
    match root {
        Value::Compound(mut root) => match root.remove("servers") {
            Some(Value::List(servers)) => Ok(servers),
            _ => Ok(Vec::new()),
        },
        _ => Err(error("ERR_SERVERS_READ", "servers.dat is damaged.")),
    }
}

fn write_servers(path: &Path, servers: Vec<Value>) -> LauncherResult<()> {
    let mut root = HashMap::new();
    root.insert("servers".to_string(), Value::List(servers));
    let bytes = fastnbt::to_bytes(&Value::Compound(root))
        .map_err(|e| io_error("ERR_SERVERS_WRITE", "Could not encode servers.dat", e))?;
    write_atomically(path, &bytes, "ERR_SERVERS_WRITE")
}

fn server_entry(value: &Value) -> ServerEntry {
    let text = |key: &str| match value {
        Value::Compound(entry) => entry.get(key).and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    ServerEntry {
        name: text("name").unwrap_or_default(),
        address: text("ip").unwrap_or_default(),
        icon_data_url: text("icon")
            .filter(|icon| !icon.is_empty() && icon.len() < 512 * 1024)
            .map(|icon| format!("data:image/png;base64,{icon}")),
    }
}

fn write_atomically(path: &Path, bytes: &[u8], code: &str) -> LauncherResult<()> {
    let partial = path.with_extension("agora-partial");
    std::fs::write(&partial, bytes).map_err(|e| io_error(code, "Could not write the file", e))?;
    std::fs::rename(&partial, path).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        io_error(code, "Could not replace the file", e)
    })
}

/// `base`, or `base (2)`, `base (3)`… whichever is free in `saves/`.
fn free_folder_name(saves: &Path, base: &str) -> String {
    let base = base.trim();
    if !saves.join(base).exists() {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|name| !saves.join(name).exists())
        .unwrap_or_else(|| base.to_string())
}

fn copy_tree(source: &Path, target: &Path) -> LauncherResult<()> {
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry.map_err(|e| io_error("ERR_WORLD_COPY", "Could not read the world", e))?;
        let relative = entry.path().strip_prefix(source).unwrap_or(entry.path());
        if relative.as_os_str() == "session.lock" {
            continue;
        }
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination)
                .map_err(|e| io_error("ERR_WORLD_COPY", "Could not create a folder", e))?;
        } else if entry.file_type().is_file() {
            std::fs::copy(entry.path(), &destination)
                .map_err(|e| io_error("ERR_WORLD_COPY", "Could not copy a file", e))?;
        }
    }
    Ok(())
}

fn write_world_zip(world: &Path, folder: &str, destination: &Path) -> LauncherResult<()> {
    let file = std::fs::File::create(destination)
        .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not create the archive", e))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for entry in walkdir::WalkDir::new(world).follow_links(false) {
        let entry =
            entry.map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not read the world", e))?;
        let relative = entry.path().strip_prefix(world).unwrap_or(entry.path());
        if relative.as_os_str().is_empty() || relative.as_os_str() == "session.lock" {
            continue;
        }
        let name = format!("{folder}/{}", relative.to_string_lossy().replace('\\', "/"));
        if entry.file_type().is_dir() {
            zip.add_directory(name, options)
                .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not write the archive", e))?;
        } else if entry.file_type().is_file() {
            zip.start_file(name, options)
                .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not write the archive", e))?;
            let mut source = std::fs::File::open(entry.path())
                .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not read a world file", e))?;
            std::io::copy(&mut source, &mut zip)
                .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not write the archive", e))?;
        }
    }
    zip.finish()
        .map_err(|e| io_error("ERR_WORLD_EXPORT", "Could not finish the archive", e))?;
    Ok(())
}

/// The folder inside the archive that holds `level.dat`: the shallowest one,
/// as `""` for the root or `"name/"`.
fn world_prefix(archive: &SafeArchive) -> LauncherResult<String> {
    let mut best: Option<String> = None;
    for entry in archive.file_entries() {
        // The name read the way the extraction reads it: `./MyWorld/level.dat` is
        // `MyWorld/level.dat`, and the selection below is made against that classified path, so a
        // folder found under one spelling and selected under another would take none of the world.
        let EntryName::Writable(rel) = classify_entry_name(&entry.name) else {
            continue;
        };
        let path = rel.as_str();
        // The file has to *be* `level.dat`: `backup-level.dat` ends with it and is not a world, so
        // a level file an archive keeps as a copy cannot pick the folder.
        let (parent, file) = match path.rsplit_once('/') {
            Some((parent, file)) => (parent, file),
            None => ("", path),
        };
        if file != "level.dat" {
            continue;
        }
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        if best
            .as_ref()
            .is_none_or(|current| prefix.matches('/').count() < current.matches('/').count())
        {
            best = Some(prefix);
        }
    }
    best.ok_or_else(|| {
        error(
            "ERR_WORLD_IMPORT",
            "That archive has no Minecraft world in it (no level.dat).",
        )
    })
}

/// Unpack the world under `prefix` into `target`, and nothing else.
///
/// The archive's names are the reader's to check: an entry that escapes the
/// folder, carries a link or names something no disk can hold refuses the whole
/// import with the entry named, rather than writing part of a world.
fn extract_world(archive: &mut SafeArchive, prefix: &str, target: &Path) -> LauncherResult<()> {
    let wanted = prefix.to_string();
    archive
        .extract_where(target, NamePolicy::Refuse, &mut |name| {
            let path = name.replace('\\', "/");
            if path.starts_with(&wanted) && path != wanted {
                Select::Write
            } else {
                // A README beside the world folder, or a second world in the same
                // archive: left out, and reported as left out.
                Select::LeaveOut("it is not part of this world".to_string())
            }
        })
        .map(|_| ())
        .map_err(|e| archive_error("Could not import the world", e))
}

/// A refusal from the one archive reader, as the code the Worlds page already reports.
fn archive_error(context: &str, refusal: ArchiveError) -> LauncherError {
    error("ERR_WORLD_IMPORT", format!("{context}: {refusal}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level_dat(name: &str, last_played: i64, game_type: i32) -> Vec<u8> {
        let mut version = HashMap::new();
        version.insert("Name".to_string(), Value::String("1.21.1".into()));
        let mut data = HashMap::new();
        data.insert("LevelName".to_string(), Value::String(name.into()));
        data.insert("LastPlayed".to_string(), Value::Long(last_played));
        data.insert("GameType".to_string(), Value::Int(game_type));
        data.insert("hardcore".to_string(), Value::Byte(0));
        data.insert("Version".to_string(), Value::Compound(version));
        data.insert("DataVersion".to_string(), Value::Int(3955));
        let mut root = HashMap::new();
        root.insert("Data".to_string(), Value::Compound(data));
        let bytes = fastnbt::to_bytes(&Value::Compound(root)).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn fixture() -> (tempfile::TempDir, WorldService) {
        let root = tempfile::tempdir().unwrap();
        let ctx = Ctx::for_testing(root.path().to_path_buf());
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let dir = ctx.paths.instance_dir("inst").unwrap();
        for (folder, name, played, mode) in
            [("Alpha", "Alpha Base", 100, 0), ("beta", "Beta", 200, 1)]
        {
            let world = dir.join("saves").join(folder);
            std::fs::create_dir_all(world.join("region")).unwrap();
            std::fs::write(world.join("level.dat"), level_dat(name, played, mode)).unwrap();
            std::fs::write(world.join("region/r.0.0.mca"), vec![7u8; 1000]).unwrap();
            std::fs::write(world.join("session.lock"), b"x").unwrap();
        }
        (root, WorldService::new(ctx))
    }

    #[test]
    fn lists_worlds_newest_first_with_their_details() {
        let (_root, service) = fixture();
        let worlds = service.list("inst").unwrap();
        assert_eq!(worlds.len(), 2);
        assert_eq!(worlds[0].folder, "beta");
        assert_eq!(worlds[0].game_mode.as_deref(), Some("creative"));
        assert_eq!(worlds[1].name, "Alpha Base");
        assert_eq!(worlds[1].version_name.as_deref(), Some("1.21.1"));
        assert!(worlds[1].size_bytes >= 1000);
    }

    #[test]
    fn a_delegated_session_blocks_a_world_change_unless_the_user_overrides_it() {
        let (_root, service) = fixture();
        // A delegated launch publishes a lease but keeps no child handle.
        agora_core::instance_runtime::publish_starting(&service.ctx.paths, "inst").unwrap();

        let refused = service.rename("inst", "Alpha", "Blocked");
        assert!(
            refused.is_err(),
            "a delegated session did not block the world change"
        );

        service
            .clone()
            .while_running()
            .rename("inst", "Alpha", "Allowed")
            .unwrap();
        let world = service
            .list("inst")
            .unwrap()
            .into_iter()
            .find(|w| w.folder == "Alpha")
            .unwrap();
        assert_eq!(world.name, "Allowed");
        agora_core::instance_runtime::clear(&service.ctx.paths, "inst", None).unwrap();
    }

    #[test]
    fn rename_changes_the_display_name_and_keeps_other_tags() {
        let (_root, service) = fixture();
        service.rename("inst", "Alpha", "New Name").unwrap();
        let world = service
            .list("inst")
            .unwrap()
            .into_iter()
            .find(|w| w.folder == "Alpha")
            .unwrap();
        assert_eq!(world.name, "New Name");
        assert_eq!(world.last_played_ms, Some(100));
        assert_eq!(world.version_name.as_deref(), Some("1.21.1"));
        assert!(service.rename("inst", "Alpha", "  ").is_err());
        assert!(service.rename("inst", "../Alpha", "x").is_err());
    }

    #[test]
    fn copy_makes_a_separate_world_without_the_session_lock() {
        let (root, service) = fixture();
        let folder = service.copy("inst", "Alpha").unwrap();
        assert_eq!(folder, "Alpha - Copy");
        let saves = root.path().join("instances/inst/saves");
        assert!(!saves.join(&folder).join("session.lock").exists());
        assert!(saves.join(&folder).join("region/r.0.0.mca").is_file());
        assert_eq!(service.copy("inst", "Alpha").unwrap(), "Alpha - Copy (2)");
        let names: Vec<String> = service
            .list("inst")
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert!(names.contains(&"Alpha Base (Copy)".to_string()));
    }

    #[test]
    fn export_then_import_round_trips_a_world() {
        let (root, service) = fixture();
        let archive = root.path().join("alpha.zip");
        service.export("inst", "Alpha", &archive).unwrap();
        let folder = service.import("inst", &archive).unwrap();
        assert_eq!(folder, "Alpha (2)");
        let saves = root.path().join("instances/inst/saves");
        assert!(saves.join(&folder).join("level.dat").is_file());
        assert!(saves.join(&folder).join("region/r.0.0.mca").is_file());
    }

    #[test]
    fn import_refuses_an_archive_that_climbs_out_of_the_world_folder() {
        // The reader's rule, not the importer's: an entry that leaves the folder refuses the whole
        // import with the entry named, where the old loop skipped it silently and imported the rest.
        let (root, service) = fixture();
        let archive = root.path().join("evil.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("MyWorld/level.dat", options).unwrap();
        zip.write_all(&level_dat("Evil", 1, 0)).unwrap();
        zip.start_file("../escape.txt", options).unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();

        let err = service.import("inst", &archive).unwrap_err();
        assert!(err.to_string().contains("escape.txt"), "{err}");
        let saves = root.path().join("instances/inst/saves");
        assert!(
            !saves.join("escape.txt").exists(),
            "nothing may be written outside the world folder"
        );
        assert_eq!(
            std::fs::read_dir(&saves).unwrap().count(),
            2,
            "the two fixture worlds, and no staging folder left behind"
        );
    }

    #[test]
    fn a_level_file_kept_as_a_copy_does_not_pick_the_folder() {
        // `backup-level.dat` at the archive's root ends with `level.dat`, and "shallowest wins"
        // then picked the root: the import took the copy and none of the world.
        let (root, service) = fixture();
        let archive = root.path().join("copies.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("backup-level.dat", options).unwrap();
        zip.write_all(&level_dat("Old", 1, 0)).unwrap();
        zip.start_file("MyWorld/level.dat", options).unwrap();
        zip.write_all(&level_dat("Real", 1, 0)).unwrap();
        zip.start_file("MyWorld/region/r.0.0.mca", options).unwrap();
        zip.write_all(b"region").unwrap();
        zip.finish().unwrap();

        let folder = service.import("inst", &archive).unwrap();
        let saves = root.path().join("instances/inst/saves");
        assert!(
            saves.join(&folder).join("region/r.0.0.mca").is_file(),
            "the world's own folder, not the root the copy sits in"
        );
        assert!(!saves.join(&folder).join("backup-level.dat").exists());
    }

    #[test]
    fn a_world_written_with_a_leading_dot_slash_imports() {
        // `./MyWorld/level.dat` names the same folder as `MyWorld/level.dat`: some archivers write the
        // dot. Discovery and selection have to read that name the same way, or the folder is found
        // and none of the world's files are taken.
        let (root, service) = fixture();
        let archive = root.path().join("dotted.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("./MyWorld/level.dat", options).unwrap();
        zip.write_all(&level_dat("Dotted", 1, 0)).unwrap();
        zip.start_file("./MyWorld/region/r.0.0.mca", options)
            .unwrap();
        zip.write_all(b"region").unwrap();
        zip.finish().unwrap();

        let folder = service.import("inst", &archive).unwrap();
        let saves = root.path().join("instances/inst/saves");
        assert!(
            saves.join(&folder).join("region/r.0.0.mca").is_file(),
            "the world's own folder, dot and all"
        );
    }

    #[test]
    fn import_rejects_an_archive_without_a_world() {
        let (root, service) = fixture();
        let archive = root.path().join("empty.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hi").unwrap();
        zip.finish().unwrap();
        assert!(service.import("inst", &archive).is_err());
    }

    #[test]
    fn delete_hands_the_world_to_the_trash() {
        let (root, service) = fixture();
        let trashed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = trashed.clone();
        let trash: TrashFn = std::sync::Arc::new(move |path: &Path| {
            seen.lock().unwrap().push(path.to_path_buf());
            std::fs::remove_dir_all(path).unwrap();
            Ok(())
        });
        service.delete("inst", "beta", &trash).unwrap();
        assert_eq!(trashed.lock().unwrap().len(), 1);
        assert!(!root.path().join("instances/inst/saves/beta").exists());
        assert!(service.delete("inst", "beta", &trash).is_err());
    }

    #[test]
    fn recent_lists_worlds_and_joined_servers_newest_first() {
        let (_root, service) = fixture();
        let db = service.ctx.paths.local_state_db();
        agora_core::db::init_local_state_db(&db).unwrap();
        let conn = agora_core::db::local_state_connection(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO instances (instance_id, game, name, created_at)
             VALUES ('inst', 'minecraft', 'My Pack', '2026-01-01T00:00:00Z');
             INSERT INTO minecraft_instances
                (instance_id, minecraft_version, loader, loader_version)
             VALUES ('inst', '1.21', 'fabric', '0.16.0');",
        )
        .unwrap();
        record_server_join(&conn, "inst", "play.example.net");
        record_server_join(&conn, "gone", "old.example.net");
        let recent = service.recent(10).unwrap();
        assert_eq!(recent[0].kind, "server", "the join just now is newest");
        assert_eq!(recent[0].instance_name, "My Pack");
        assert_eq!(recent[1].label, "Beta");
        assert_eq!(recent[2].target, "Alpha");
        assert_eq!(recent.len(), 3, "servers of deleted instances are left out");
        assert_eq!(service.recent(1).unwrap().len(), 1);
    }

    #[test]
    fn screenshots_list_rename_and_read() {
        let (root, service) = fixture();
        let shots = root.path().join("instances/inst/screenshots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("2026-01-01_10.00.00.png"), b"\x89PNGdata").unwrap();
        std::fs::write(shots.join("notes.txt"), b"x").unwrap();
        let listed = service.list_screenshots("inst").unwrap();
        assert_eq!(listed.len(), 1);
        let renamed = service
            .rename_screenshot("inst", "2026-01-01_10.00.00.png", "Castle")
            .unwrap();
        assert_eq!(renamed, "Castle.png");
        assert!(service
            .screenshot_data_url("inst", "Castle.png")
            .unwrap()
            .starts_with("data:image/png;base64,"));
        assert!(service.screenshot_data_url("inst", "notes.txt").is_err());
        assert!(service
            .rename_screenshot("inst", "Castle.png", "../x")
            .is_err());
    }

    #[test]
    fn server_list_edits_keep_unknown_tags() {
        let (root, service) = fixture();
        assert!(service.list_servers("inst").unwrap().is_empty());
        service
            .add_server("inst", "Home", "play.example.net")
            .unwrap();
        service
            .add_server("inst", "", "mc.example.org:25570")
            .unwrap();

        // Simulate the game caching an icon and a tag Agora does not know.
        let path = root.path().join("instances/inst/servers.dat");
        let mut servers = read_servers(&path).unwrap();
        if let Value::Compound(entry) = &mut servers[0] {
            entry.insert("icon".into(), Value::String("aWNvbg==".into()));
            entry.insert("acceptTextures".into(), Value::Byte(1));
        }
        write_servers(&path, servers).unwrap();

        service
            .update_server("inst", 0, "Home!", "play.example.net")
            .unwrap();
        service.move_server("inst", 1, -1).unwrap();
        let listed = service.list_servers("inst").unwrap();
        assert_eq!(listed[0].name, "Minecraft Server");
        assert_eq!(listed[1].name, "Home!");
        assert!(listed[1].icon_data_url.is_some());
        let raw = read_servers(&path).unwrap();
        assert!(
            matches!(&raw[1], Value::Compound(e) if e.get("acceptTextures") == Some(&Value::Byte(1)))
        );

        service
            .update_server("inst", 1, "Home", "other.example.net")
            .unwrap();
        assert!(service.list_servers("inst").unwrap()[1]
            .icon_data_url
            .is_none());
        service.remove_server("inst", 0).unwrap();
        assert_eq!(service.list_servers("inst").unwrap().len(), 1);
        assert!(service.remove_server("inst", 5).is_err());
        assert!(service.add_server("inst", "x", "bad host").is_err());
    }
}
