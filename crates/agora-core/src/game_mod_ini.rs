//! INI files that mods ship, edited per instance (MASTER_SPEC §26.5, *Per-user files*).
//!
//! Mods are stored once and shared by every instance. An edit to one of their INI files is kept
//! at `<instance>/overrides/<path>`, a layer that is applied after every other layer, so the
//! user's choice wins without changing the stored mod. This module lists the mod INI files an
//! instance can see, reads them (the override when there is one), writes an override, and removes
//! it again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agora_game_api::{Layer, LayerSource, RelPath};
use serde::Serialize;

use crate::content_store::{self, ContentFile};
use crate::ctx::Ctx;
use crate::error::LauncherError;
use crate::game_ini::IniDocument;
use crate::game_instance::{self, GameInstanceManifest};
use crate::lock_manager::LockResource;

const OVERRIDES: &str = "overrides";

#[derive(Debug, thiserror::Error)]
pub enum ModIniError {
    #[error("instance '{0}' not found")]
    InstanceNotFound(String),
    #[error("'{}' is not an INI file of an enabled mod of this instance", .path)]
    UnknownPath { path: String },
    #[error("'{}' cannot be edited because {reason}", .path)]
    NotEditable { path: String, reason: String },
    #[error("cannot read the instance's overrides folder {}: {source}", path.display())]
    UnreadableOverrides {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is a file, not a folder, so the instance's overrides cannot be read", path.display())]
    OverridesNotAFolder { path: PathBuf },
    #[error("refusing to use {}: it points outside the instance", path.display())]
    EscapesInstance { path: PathBuf },
    #[error("'{0}' is not a text INI file, so it cannot be edited here")]
    Unparseable(String),
    #[error("cannot read {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Lock(#[from] LauncherError),
    #[error("{0}")]
    Other(String),
}

/// One INI file an enabled mod ships, as this instance sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModIni {
    /// The path inside the game folder, with forward slashes.
    pub path: String,
    /// The content item's id. Empty for an override that no mod ships.
    pub mod_id: String,
    /// The mod's name. Empty for an override that no mod ships.
    pub mod_name: String,
    /// Whether the instance has its own copy of this file in `overrides/`.
    pub overridden: bool,
    /// A later enabled mod that ships the same path. The row itself names the winning mod, so
    /// this is only set when a mod other than the winner explains the file.
    pub shadowed_by: Option<String>,
    /// False when the mod that shipped this path is turned off, or when no mod ships it any more.
    /// Such a row can be reset but not edited.
    pub mod_enabled: bool,
}

/// Where the text of a mod INI came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModIniSource {
    /// The instance's own copy in `overrides/`.
    Override,
    /// The winning mod's file in the content store.
    Mod,
}

/// A mod INI file as the instance sees it.
#[derive(Debug)]
pub struct ModIniRead {
    pub path: String,
    pub source: ModIniSource,
    pub document: IniDocument,
}

fn instance_dir(ctx: &Ctx, instance_id: &str) -> Result<PathBuf, ModIniError> {
    ctx.paths
        .instance_dir(instance_id)
        .map_err(|e| ModIniError::Other(e.to_string()))
}

fn manifest(ctx: &Ctx, instance_id: &str) -> Result<GameInstanceManifest, ModIniError> {
    game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        game_instance::InstanceError::NotFound(id) => ModIniError::InstanceNotFound(id),
        other => ModIniError::Other(other.to_string()),
    })
}

fn is_ini(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ini"))
}

/// Where a content item's file lands in the game folder under a layer, the way the deployment
/// planner mounts it. `None` when the layer's source path does not contain the file.
fn mounted_path(layer: &Layer, file: &ContentFile) -> Option<RelPath> {
    let source = layer.source_path.as_str().trim_matches('/');
    let source_parts: Vec<&str> = if source.is_empty() {
        Vec::new()
    } else {
        source.split('/').filter(|part| !part.is_empty()).collect()
    };
    let file_parts: Vec<&str> = file
        .path
        .as_str()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let stripped = if source_parts.is_empty() {
        Some(file.path.as_str().to_string())
    } else if file_parts.len() > source_parts.len()
        && source_parts
            .iter()
            .zip(file_parts.iter())
            .all(|(source, part)| source.eq_ignore_ascii_case(part))
    {
        Some(file_parts[source_parts.len()..].join("/"))
    } else {
        None
    }?;
    let mounted = if layer.mount_path.as_str().is_empty() {
        stripped
    } else {
        format!(
            "{}/{}",
            layer.mount_path.as_str().trim_end_matches('/'),
            stripped.trim_start_matches('/')
        )
    };
    RelPath::new(mounted).ok()
}

/// The path a person typed, with Windows separators made forward slashes.
fn normalize_input(path: &str) -> String {
    path.replace('\\', "/")
}

fn same_path(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn key(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// One mod that ships a path, and the layer position that decides the winner.
struct Shipping {
    path: String,
    mod_id: String,
    mod_name: String,
    enabled: bool,
    layer_index: usize,
}

/// Every layer's items, listed once per call.
fn shipping_files(
    ctx: &Ctx,
    manifest: &GameInstanceManifest,
) -> Result<Vec<Shipping>, ModIniError> {
    let ids = content_store::item_ids(ctx).map_err(|e| ModIniError::Other(e.to_string()))?;
    let mut out = Vec::new();
    for (layer_index, layer) in manifest.layers.layers().iter().enumerate() {
        let LayerSource::Content { content } = &layer.source else {
            continue;
        };
        let item = content_store::get_item_indexed(ctx, &ids, content)
            .map_err(|e| ModIniError::Other(e.to_string()))?;
        for file in &item.files {
            let Some(rel) = mounted_path(layer, file) else {
                continue;
            };
            if !is_ini(rel.as_str()) {
                continue;
            }
            out.push(Shipping {
                path: rel.as_str().to_string(),
                mod_id: item.item_id.clone(),
                mod_name: item.name.clone(),
                enabled: layer.enabled,
                layer_index,
            });
        }
    }
    Ok(out)
}

/// The override files under `overrides/`, in no particular order. A missing folder has none; an
/// unreadable one, or a file where the folder should be, is an error.
fn override_files(ctx: &Ctx, instance_id: &str) -> Result<Vec<String>, ModIniError> {
    let dir = instance_dir(ctx, instance_id)?;
    let root = dir.join(OVERRIDES);
    let root = match std::fs::symlink_metadata(&root) {
        Ok(meta) => {
            ensure_inside_instance(&dir, &root)?;
            let followed =
                std::fs::metadata(&root).map_err(|source| ModIniError::UnreadableOverrides {
                    path: root.clone(),
                    source,
                })?;
            if !followed.is_dir() {
                return Err(ModIniError::OverridesNotAFolder { path: root });
            }
            if is_reparse_point(&meta) {
                std::fs::canonicalize(&root).map_err(|source| ModIniError::UnreadableOverrides {
                    path: root.clone(),
                    source,
                })?
            } else {
                root
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ModIniError::UnreadableOverrides { path: root, source });
        }
    };
    let mut out = Vec::new();
    walk_overrides(&root, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_overrides(root: &Path, rel: &Path, out: &mut Vec<String>) -> Result<(), ModIniError> {
    let current = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let entries =
        std::fs::read_dir(&current).map_err(|source| ModIniError::UnreadableOverrides {
            path: current.clone(),
            source,
        })?;
    for entry in entries {
        let entry = entry.map_err(|source| ModIniError::UnreadableOverrides {
            path: current.clone(),
            source,
        })?;
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|source| {
            ModIniError::UnreadableOverrides {
                path: entry.path(),
                source,
            }
        })?;
        if is_reparse_point(&meta) {
            continue;
        }
        let child = rel.join(entry.file_name());
        if meta.is_dir() {
            walk_overrides(root, &child, out)?;
        } else if meta.is_file() {
            let path = child.to_string_lossy().replace('\\', "/");
            if is_ini(&path) {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        (meta.file_attributes() & 0x0000_0400) != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Refuse a path under `instance_dir` when a symlink or junction on the way points outside it.
/// Components that do not exist yet are fine: nothing there can point anywhere.
fn ensure_inside_instance(instance_dir: &Path, target: &Path) -> Result<(), ModIniError> {
    let root = std::fs::canonicalize(instance_dir).map_err(|source| ModIniError::Unreadable {
        path: instance_dir.to_path_buf(),
        source,
    })?;
    let relative = target
        .strip_prefix(instance_dir)
        .map_err(|_| ModIniError::EscapesInstance {
            path: target.to_path_buf(),
        })?;
    let mut current = instance_dir.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(ModIniError::Unreadable {
                    path: current,
                    source,
                });
            }
        };
        if is_reparse_point(&meta) {
            let resolved =
                std::fs::canonicalize(&current).map_err(|source| ModIniError::Unreadable {
                    path: current.clone(),
                    source,
                })?;
            if !resolved.starts_with(&root) {
                return Err(ModIniError::EscapesInstance { path: current });
            }
        }
    }
    Ok(())
}

/// Every mod INI file this instance can see, in stack order. A path two enabled mods ship appears
/// once, naming the later (winning) mod. An override whose mod is turned off, or that no mod ships
/// any more, is listed too, so nothing the user cannot see silently changes the game.
pub fn list_mod_inis(ctx: &Ctx, instance_id: &str) -> Result<Vec<ModIni>, ModIniError> {
    let manifest = manifest(ctx, instance_id)?;
    let shipping = shipping_files(ctx, &manifest)?;

    // A path has one winning mod: an enabled mod beats a disabled one, and a later enabled mod
    // beats an earlier one. The map keeps the winner for each lower-cased path.
    let mut winners: BTreeMap<String, Shipping> = BTreeMap::new();
    for ship in shipping {
        let path_key = key(&ship.path);
        match winners.get(&path_key) {
            Some(existing) if existing.enabled && !ship.enabled => continue,
            _ => {
                winners.insert(path_key, ship);
            }
        }
    }

    let mut rows: Vec<(usize, ModIni)> = winners
        .into_values()
        .map(|winner| {
            (
                winner.layer_index,
                ModIni {
                    path: winner.path,
                    mod_id: winner.mod_id,
                    mod_name: winner.mod_name,
                    overridden: false,
                    shadowed_by: None,
                    mod_enabled: winner.enabled,
                },
            )
        })
        .collect();

    for path in override_files(ctx, instance_id)? {
        let path_key = key(&path);
        if let Some((_, row)) = rows.iter_mut().find(|(_, row)| key(&row.path) == path_key) {
            row.overridden = true;
        } else {
            // No mod ships this path any more, so the override is the only copy. It is still the
            // user's file and still deploys; say so instead of hiding it.
            rows.push((
                usize::MAX,
                ModIni {
                    path,
                    mod_id: String::new(),
                    mod_name: String::new(),
                    overridden: true,
                    shadowed_by: None,
                    mod_enabled: false,
                },
            ));
        }
    }

    rows.retain(|(_, row)| row.mod_enabled || row.overridden);
    rows.sort_by(|(left_index, left), (right_index, right)| {
        left_index
            .cmp(right_index)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

fn listed_row(ctx: &Ctx, instance_id: &str, path: &str) -> Result<ModIni, ModIniError> {
    let normalized = normalize_input(path);
    list_mod_inis(ctx, instance_id)?
        .into_iter()
        .find(|row| same_path(&row.path, &normalized))
        .ok_or_else(|| ModIniError::UnknownPath {
            path: path.to_string(),
        })
}

fn override_path(ctx: &Ctx, instance_id: &str, row: &ModIni) -> Result<PathBuf, ModIniError> {
    let dir = instance_dir(ctx, instance_id)?;
    let path = dir.join(OVERRIDES).join(&row.path);
    ensure_inside_instance(&dir, &path)?;
    Ok(path)
}

/// The winning enabled file for a path, read from the content store. Uses the item index once.
fn winning_mod_file(
    ctx: &Ctx,
    manifest: &GameInstanceManifest,
    path: &str,
) -> Result<Option<(String, String, ContentFile)>, ModIniError> {
    let ids = content_store::item_ids(ctx).map_err(|e| ModIniError::Other(e.to_string()))?;
    let mut winner: Option<(String, String, ContentFile)> = None;
    for layer in manifest.layers.layers() {
        if !layer.enabled {
            continue;
        }
        let LayerSource::Content { content } = &layer.source else {
            continue;
        };
        let item = content_store::get_item_indexed(ctx, &ids, content)
            .map_err(|e| ModIniError::Other(e.to_string()))?;
        for file in &item.files {
            let Some(rel) = mounted_path(layer, file) else {
                continue;
            };
            if same_path(rel.as_str(), path) {
                winner = Some((item.item_id.clone(), item.name.clone(), file.clone()));
            }
        }
    }
    Ok(winner)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, ModIniError> {
    std::fs::read(path).map_err(|source| ModIniError::Unreadable {
        path: path.to_path_buf(),
        source,
    })
}

/// Parse one of the instance's mod INI files. The parser keeps everything it does not change, so a
/// text file always parses. A file that is not text (invalid UTF-8 or a NUL byte) is refused rather
/// than shown as a half-read editor that would write back different bytes.
fn parse_ini(path: &str, bytes: &[u8]) -> Result<IniDocument, ModIniError> {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return Err(ModIniError::Unparseable(path.to_string()));
    }
    Ok(IniDocument::parse(bytes))
}

fn load_ini(path: &str, file: &Path) -> Result<IniDocument, ModIniError> {
    parse_ini(path, &read_bytes(file)?)
}

/// Read a mod INI as the instance sees it: the override when one exists, otherwise the winning
/// mod's stored file. Reading never writes anything.
pub fn read_mod_ini(ctx: &Ctx, instance_id: &str, path: &str) -> Result<ModIniRead, ModIniError> {
    let row = listed_row(ctx, instance_id, path)?;
    let override_path = override_path(ctx, instance_id, &row)?;
    if override_path.is_file() {
        return Ok(ModIniRead {
            path: row.path.clone(),
            source: ModIniSource::Override,
            document: load_ini(&row.path, &override_path)?,
        });
    }
    let manifest = manifest(ctx, instance_id)?;
    let Some((_, _, file)) = winning_mod_file(ctx, &manifest, &row.path)? else {
        return Err(ModIniError::NotEditable {
            path: row.path,
            reason: "no enabled mod ships it and the instance has no copy".to_string(),
        });
    };
    Ok(ModIniRead {
        path: row.path.clone(),
        source: ModIniSource::Mod,
        document: load_ini(&row.path, &ctx.paths.content_object_path(&file.sha256))?,
    })
}

/// Set a key in the instance's copy of a mod's INI file. The first edit seeds the copy from the
/// mod's file. Returns whether the file changed. The stored mod is never touched.
pub fn set_mod_ini_value(
    ctx: &Ctx,
    instance_id: &str,
    path: &str,
    section: &str,
    key: &str,
    value: &str,
) -> Result<bool, ModIniError> {
    edit_mod_ini(ctx, instance_id, path, |document| {
        let changed = document.set(section, key, value)?;
        Ok((changed, changed))
    })
}

/// Remove a key from the instance's copy of a mod's INI file, seeding the copy from the mod when
/// it does not exist yet. Returns whether the file changed.
pub fn unset_mod_ini_value(
    ctx: &Ctx,
    instance_id: &str,
    path: &str,
    section: &str,
    key: &str,
) -> Result<bool, ModIniError> {
    edit_mod_ini(ctx, instance_id, path, |document| {
        let changed = document.unset(section, key)?;
        Ok((changed, changed))
    })
}

/// Edit the instance's copy with `f`, which returns its own result and whether it changed the
/// document. The copy is written only when it changed, through a temporary file beside it. The
/// store is read, never written.
fn edit_mod_ini<T>(
    ctx: &Ctx,
    instance_id: &str,
    path: &str,
    f: impl FnOnce(&mut IniDocument) -> Result<(T, bool), crate::game_ini::IniError>,
) -> Result<T, ModIniError> {
    let row = listed_row(ctx, instance_id, path)?;
    if !row.mod_enabled {
        return Err(ModIniError::NotEditable {
            path: row.path,
            reason: if row.mod_name.is_empty() {
                "no enabled mod ships it".to_string()
            } else {
                "its mod is turned off".to_string()
            },
        });
    }
    let target = override_path(ctx, instance_id, &row)?;
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "mod-ini-edit",
    )?;
    let mut document = if target.is_file() {
        load_ini(&row.path, &target)?
    } else {
        let manifest = manifest(ctx, instance_id)?;
        let Some((_, _, file)) = winning_mod_file(ctx, &manifest, &row.path)? else {
            return Err(ModIniError::NotEditable {
                path: row.path,
                reason: "no enabled mod ships it and the instance has no copy".to_string(),
            });
        };
        load_ini(&row.path, &ctx.paths.content_object_path(&file.sha256))?
    };
    let (value, changed) = f(&mut document).map_err(|e| ModIniError::Other(e.to_string()))?;
    if changed {
        std::fs::create_dir_all(target.parent().unwrap_or_else(|| Path::new("."))).map_err(
            |source| ModIniError::Write {
                path: target.clone(),
                source,
            },
        )?;
        // Creating the folders may have followed a link that was not there a moment ago.
        ensure_inside_instance(&instance_dir(ctx, instance_id)?, &target)?;
        write_atomic(&target, &document.render())?;
    }
    Ok(value)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ModIniError> {
    let error = |source| ModIniError::Write {
        path: path.to_path_buf(),
        source,
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(format!(
        "{name}.{}.agora-tmp",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, bytes).map_err(error)?;
    std::fs::rename(&tmp, path).map_err(error)
}

/// Remove the instance's copy of a mod INI, so the mod's file is used again. Only that one file
/// and the folders inside `overrides/` that it leaves empty are removed. A path with no copy is a
/// no-op, not an error.
pub fn reset_mod_ini(ctx: &Ctx, instance_id: &str, path: &str) -> Result<(), ModIniError> {
    let row = listed_row(ctx, instance_id, path)?;
    let target = override_path(ctx, instance_id, &row)?;
    let _lock = ctx.lock_manager.acquire(
        LockResource::Instance(instance_id.to_string()),
        "mod-ini-reset",
    )?;
    match std::fs::remove_file(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ModIniError::Write {
                path: target,
                source,
            });
        }
    }
    remove_empty_parents(&instance_dir(ctx, instance_id)?.join(OVERRIDES), &target);
    Ok(())
}

/// Remove the empty folders above `file`, stopping at `root`, which stays.
fn remove_empty_parents(root: &Path, file: &Path) {
    let mut folder = file.parent();
    while let Some(current) = folder {
        if current == root || !current.starts_with(root) || std::fs::remove_dir(current).is_err() {
            break;
        }
        folder = current.parent();
    }
}
