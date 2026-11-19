//! An instance's save choice (MASTER_SPEC §26.5, *Saves are the user's choice per instance*).
//!
//! An instance keeps the game's shared save folder, or saves of its own. For its own saves, the
//! game's setting (`SLocalSavePath` under `[General]` in `Skyrim.ini`, for Skyrim) is set to a
//! folder named for the instance, relative to the game's `My Games` folder. Switching changes only
//! where the game looks: no save file is moved, copied or deleted.
//!
//! Turning the choice on records the value the setting held before, in `saves_state.json` in the
//! instance's folder. Turning it off puts that value back, or removes the setting when it was
//! absent, and only if the setting still holds the value Agora set. A setting changed by hand since
//! is left as it is, and the user is told.

use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agora_game_api::{
    glob_match, GameDefinition, GamePath, SaveLocationRule, SaveRecognition, StoreId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::content_store::is_reparse_point_or_symlink;
use crate::ctx::Ctx;
use crate::game_deploy::write_instance_manifest_atomic;
use crate::game_ini::{self, IniError, IniSource};
use crate::game_instance::{self, GameInstanceManifest, InstanceError, SavesChoice};
use crate::game_user_files::{
    paths_eq_case_insensitive, resolve_rel_case_insensitive, user_data_root,
};
use crate::instance_runtime::InstanceGuard;

pub(crate) const STATE_FILE: &str = "saves_state.json";
/// The longest plain instance id used as a folder name as it is. Longer ids are shortened and
/// given a hash, so a save path stays well inside the Windows path limit.
const PLAIN_NAME_MAX: usize = 48;
/// The most characters of an unusual instance id kept in its folder name, before the hash.
const CLEANED_NAME_MAX: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum SavesError {
    #[error("instance '{0}' not found")]
    NotFound(String),
    #[error("{game} declares no save location for the '{store}' store, so the instance cannot have saves of its own")]
    NoSaveLocation { game: String, store: String },
    #[error(transparent)]
    Ini(#[from] IniError),
    #[error("{0}")]
    Other(String),
    #[error("cannot write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What Agora changed in the game's setting when the instance took its own saves: the section and
/// key, the value Agora set, and the value the key held before (`None` when it was absent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavesState {
    /// The per-user file holding the setting (an `instance_path`).
    pub ini: String,
    pub section: String,
    pub key: String,
    pub set_to: String,
    pub previous: Option<String>,
}

/// A folder of saves and what is in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveFolder {
    pub path: PathBuf,
    pub exists: bool,
    /// Absent when the package does not say how to recognise a save.
    pub saves: Option<usize>,
    /// The newest save's modification time, RFC 3339 in UTC.
    pub newest: Option<String>,
}

/// The save choice of an instance and the folders involved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavesStatus {
    pub instance_id: String,
    pub choice: SavesChoice,
    /// The folder the game reads and writes for this instance.
    pub in_use: SaveFolder,
    /// The other folder, whose saves stay where they are when the choice changes.
    pub other: Option<SaveFolder>,
    /// The game's setting as the instance sees it now, and where that value came from.
    pub setting_file: String,
    pub setting: String,
    pub setting_value: Option<String>,
    pub setting_source: IniSource,
}

/// What a change of the save choice did to the game's setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SettingChange {
    /// The package declares folders directly and has no INI setting.
    NoSetting,
    /// The setting now names the instance's folder.
    Set { value: String },
    /// The setting already named the instance's folder.
    Unchanged,
    /// The setting was put back to the value it had before.
    Restored { value: String },
    /// The setting was absent before, so it was removed.
    Removed,
    /// The setting no longer holds the value Agora set, so it was left as it is.
    LeftAlone { now: Option<String> },
    /// Nothing was recorded for this instance, so there was nothing to put back.
    NothingRecorded,
}

/// The result of a change of the save choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavesChange {
    pub instance_id: String,
    pub previous: SavesChoice,
    pub choice: SavesChoice,
    pub setting: SettingChange,
    /// The folder the game reads and writes now.
    pub in_use: SaveFolder,
    /// The other folder: its saves were not moved.
    pub other: Option<SaveFolder>,
}

/// The rule that applies to this store, if the game has one.
pub fn rule_for<'a>(
    definition: &'a GameDefinition,
    store: &StoreId,
) -> Option<&'a SaveLocationRule> {
    definition
        .save_location
        .iter()
        .find(|rule| rule.applies_to_store(store))
}

/// The save folder name for an instance. A plain id is its own name. Any other id (one with spaces,
/// punctuation, a Windows reserved name, or a length that is too long) becomes a name of safe
/// characters followed by a hash of the whole id, so two ids never share a folder.
pub fn save_folder_name(instance_id: &str) -> String {
    let plain = !instance_id.is_empty()
        && instance_id.len() <= PLAIN_NAME_MAX
        && instance_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !is_reserved_device_name(instance_id);
    if plain {
        return instance_id.to_string();
    }
    let cleaned: String = instance_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(CLEANED_NAME_MAX)
        .collect();
    let cleaned = if cleaned.is_empty() {
        "instance".to_string()
    } else {
        cleaned
    };
    let digest = format!("{:x}", Sha256::digest(instance_id.as_bytes()));
    format!("{cleaned}-{}", &digest[..12])
}

/// Windows refuses these names as a path component, whatever comes after them.
fn is_reserved_device_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base[3..].bytes().all(|b| (b'1'..=b'9').contains(&b)))
}

/// The setting's value for this instance: the rule's `own_value` with the folder name filled in.
pub fn own_value(rule: &SaveLocationRule, instance_id: &str) -> String {
    own_value_scoped(rule, instance_id, None)
}

/// [`own_value`] for one loadout: its own folder sits under the instance's folder, so two mod
/// setups never load each other's saves. The scope is a loadout name, made into a folder name.
pub fn own_value_scoped(rule: &SaveLocationRule, instance_id: &str, scope: Option<&str>) -> String {
    render_own_value(&rule.own_value, instance_id, scope)
}

fn render_own_value(template: &str, instance_id: &str, scope: Option<&str>) -> String {
    let mut folder = save_folder_name(instance_id);
    if let Some(scope) = scope {
        folder.push('\\');
        folder.push_str(&save_folder_name(scope));
    }
    template.replace("{instance}", &folder)
}

fn resolve(path: &GamePath) -> Result<PathBuf, SavesError> {
    let (location, path) = validate_folder_path(path)?;
    let mut folder = user_data_root(location).ok_or_else(|| {
        SavesError::Other(format!("could not find the {location:?} user-data folder"))
    })?;
    for part in path.as_str().split('/').filter(|part| !part.is_empty()) {
        checked_metadata(&folder)?;
        folder = resolve_rel_case_insensitive(&folder, part);
    }
    checked_metadata(&folder)?;
    Ok(folder)
}

/// The folder the game reads and writes for this instance, without surveying its contents.
pub fn in_use_folder(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<PathBuf, SavesError> {
    let manifest = manifest_of(ctx, instance_id)?;
    let store = game_ini::store_of(ctx, instance_id)?;
    let folders = save_folders(
        definition,
        &store,
        instance_id,
        manifest.save_scope.as_deref(),
    )?;
    Ok(folders.in_use(manifest.saves)?.path.clone())
}

/// The game's shared save folder for the rule.
pub fn shared_folder(rule: &SaveLocationRule) -> Result<PathBuf, SavesError> {
    resolve(&rule.shared_dir)
}

/// The instance's own save folder: `own_value` under the rule's `relative_to` folder.
pub fn own_folder(rule: &SaveLocationRule, instance_id: &str) -> Result<PathBuf, SavesError> {
    own_folder_scoped(rule, instance_id, None)
}

/// [`own_folder`] for one loadout's own save folder.
pub fn own_folder_scoped(
    rule: &SaveLocationRule,
    instance_id: &str,
    scope: Option<&str>,
) -> Result<PathBuf, SavesError> {
    validate_own_template(&rule.own_value, &rule.relative_to, &rule.shared_dir)?;
    let folder = resolve(&rule.relative_to)?.join(relative_save_path(&own_value_scoped(
        rule,
        instance_id,
        scope,
    ))?);
    checked_metadata(&folder)?;
    Ok(folder)
}

fn relative_save_path(value: &str) -> Result<PathBuf, SavesError> {
    let trimmed = value.trim_end_matches(['\\', '/']);
    let parts = trimmed.split(['\\', '/']);
    if parts.clone().any(|part| {
        !is_safe_save_component(part) || part.starts_with(' ') || part.contains([';', '#', '='])
    }) {
        return Err(SavesError::Other(format!(
            "unsafe relative save path '{value}'"
        )));
    }
    Ok(parts.collect())
}

pub(crate) fn is_safe_save_component(part: &str) -> bool {
    !part.is_empty()
        && !part.ends_with(['.', ' '])
        && !part.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        && !part.chars().any(char::is_control)
        && !is_reserved_device_name(part)
}

pub(crate) fn validate_folder_path(
    path: &GamePath,
) -> Result<(&agora_game_api::UserDataLocation, &agora_game_api::RelPath), SavesError> {
    let GamePath::UserData { location, path } = path else {
        return Err(SavesError::Other(
            "save folders must use the user_data root".into(),
        ));
    };
    if !path.is_empty()
        && !path
            .as_str()
            .trim_end_matches('/')
            .split('/')
            .all(is_safe_save_component)
    {
        return Err(SavesError::Other(format!(
            "unsafe save folder path '{path}'"
        )));
    }
    Ok((location, path))
}

/// The fixed directory reserved for private saves, before the instance component.
fn private_prefix(template: &str) -> Result<PathBuf, SavesError> {
    let path = relative_save_path(template)?;
    let mut prefix = PathBuf::new();
    let mut found_instance = false;
    for part in &path {
        if part == "{instance}" && !found_instance {
            found_instance = true;
        } else if part.to_string_lossy().contains(['{', '}']) {
            return Err(SavesError::Other(
                "own_value must contain one full {instance} path component".into(),
            ));
        } else if !found_instance {
            prefix.push(part);
        }
    }
    if !found_instance {
        return Err(SavesError::Other(
            "own_value must contain one full {instance} path component".into(),
        ));
    }
    Ok(prefix)
}

fn path_within(path: &Path, root: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| paths_eq_case_insensitive(ancestor, root))
}

pub(crate) fn validate_own_template(
    template: &str,
    base: &GamePath,
    shared: &GamePath,
) -> Result<(), SavesError> {
    let (location, path) = validate_folder_path(base)?;
    let (shared_location, shared_path) = validate_folder_path(shared)?;
    let prefix = private_prefix(template)?;
    relative_save_path(&render_own_value(template, "example-instance", None))?;
    if location == shared_location {
        let private = Path::new(path.as_str()).join(prefix);
        if path_within(Path::new(shared_path.as_str()), &private) {
            return Err(SavesError::Other(
                "private saves need a directory reserved apart from shared saves".into(),
            ));
        }
    }
    Ok(())
}

/// Check every ancestor without following a symbolic link or Windows reparse point.
pub(crate) fn checked_metadata(path: &Path) -> Result<Option<Metadata>, SavesError> {
    let ancestors = path
        .ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .collect::<Vec<_>>();
    let mut result = None;
    for component in ancestors.into_iter().rev() {
        match fs::symlink_metadata(component) {
            Ok(metadata) if is_reparse_point_or_symlink(&metadata) => {
                return Err(SavesError::Other(format!(
                    "{} is a link or reparse point",
                    component.display()
                )));
            }
            Ok(metadata) => result = Some(metadata),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SavesError::Read {
                    path: component.to_path_buf(),
                    source,
                })
            }
        }
    }
    Ok(result)
}

#[derive(Debug)]
pub(crate) struct SaveArea {
    pub path: PathBuf,
    private_root: Option<PathBuf>,
}

impl SaveArea {
    pub(crate) fn excludes(&self, path: &Path) -> bool {
        self.private_root
            .as_ref()
            .is_some_and(|root| path_within(path, root))
    }

    /// List all eligible files before any copy creates a destination. Include sidecars and files
    /// within folder saves, regardless of how the package counts the saves themselves.
    pub(crate) fn files(&self) -> Result<Vec<(String, u64)>, SavesError> {
        let mut files = Vec::new();
        let mut dirs = vec![(self.path.clone(), String::new())];
        checked_metadata(&self.path)?
            .ok_or_else(|| SavesError::Other(format!("{} does not exist", self.path.display())))?;
        while let Some((dir, relative)) = dirs.pop() {
            for entry in entries(&dir)? {
                let entry = entry.map_err(|source| SavesError::Read {
                    path: dir.clone(),
                    source,
                })?;
                let path = entry.path();
                if self.excludes(&path) {
                    continue;
                }
                let metadata = checked_metadata(&path)?
                    .ok_or_else(|| SavesError::Other(format!("{} disappeared", path.display())))?;
                let name = entry.file_name().into_string().map_err(|_| {
                    SavesError::Other(format!("{} has an unreadable file name", path.display()))
                })?;
                if !is_safe_save_component(&name) {
                    return Err(SavesError::Other(format!(
                        "{} has an unsafe file name",
                        path.display()
                    )));
                }
                let relative = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                if metadata.is_dir() {
                    dirs.push((path, relative));
                } else if metadata.is_file() {
                    files.push((relative, metadata.len()));
                } else {
                    return Err(SavesError::Other(format!(
                        "{} is not a file or folder",
                        path.display()
                    )));
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(files)
    }
}

pub(crate) struct SaveFolders<'a> {
    pub shared: SaveArea,
    pub own: Option<SaveArea>,
    recognition: Option<&'a SaveRecognition>,
    pub ini: Option<&'a SaveLocationRule>,
}

impl SaveFolders<'_> {
    pub(crate) fn in_use(&self, choice: SavesChoice) -> Result<&SaveArea, SavesError> {
        match choice {
            SavesChoice::Shared => Ok(&self.shared),
            SavesChoice::Own => self.own.as_ref().ok_or_else(|| {
                SavesError::Other("the game declares no private save folder".into())
            }),
        }
    }

    fn surveys(&self, choice: SavesChoice) -> Result<(SaveFolder, Option<SaveFolder>), SavesError> {
        let other = match choice {
            SavesChoice::Shared => self.own.as_ref(),
            SavesChoice::Own => Some(&self.shared),
        };
        Ok((
            survey(self.in_use(choice)?, self.recognition)?,
            other
                .map(|area| survey(area, self.recognition))
                .transpose()?,
        ))
    }
}

pub(crate) fn save_folders<'a>(
    definition: &'a GameDefinition,
    store: &StoreId,
    instance_id: &str,
    scope: Option<&str>,
) -> Result<SaveFolders<'a>, SavesError> {
    let ini = rule_for(definition, store);
    let (shared, own, recognition) = match (ini, definition.save_paths.as_slice()) {
        (Some(rule), _) => (
            &rule.shared_dir,
            Some((rule.own_value.as_str(), &rule.relative_to)),
            rule.recognition.as_ref(),
        ),
        (None, [rule]) => (rule.path(), rule.own_layout(), rule.recognition()),
        (None, []) => {
            return Err(SavesError::NoSaveLocation {
                game: definition.id.as_str().into(),
                store: store.as_str().into(),
            })
        }
        (None, _) => {
            return Err(SavesError::Other(
                "could not tell which of the game's declared save paths to use".into(),
            ))
        }
    };
    let shared_path = resolve(shared)?;
    validate_recognition(recognition)?;
    let (own, private_root) = if let Some((template, base)) = own {
        validate_own_template(template, base, shared)?;
        let base = resolve(base)?;
        let private = base.join(private_prefix(template)?);
        if path_within(&shared_path, &private) {
            return Err(SavesError::Other(
                "private saves overlap the shared save folder".into(),
            ));
        }
        let own = base.join(relative_save_path(&render_own_value(
            template,
            instance_id,
            scope,
        ))?);
        checked_metadata(&own)?;
        (
            Some(SaveArea {
                path: own,
                private_root: None,
            }),
            Some(private),
        )
    } else {
        (None, None)
    };
    checked_metadata(&shared_path)?;
    Ok(SaveFolders {
        shared: SaveArea {
            path: shared_path,
            private_root,
        },
        own,
        recognition,
        ini,
    })
}

fn entries(path: &Path) -> Result<fs::ReadDir, SavesError> {
    fs::read_dir(path).map_err(|source| SavesError::Read {
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn validate_recognition(
    recognition: Option<&SaveRecognition>,
) -> Result<(), SavesError> {
    if let Some(SaveRecognition::Files { patterns }) = recognition {
        if patterns.is_empty()
            || patterns.iter().any(|pattern| {
                pattern.trim().is_empty()
                    || pattern.contains(['/', '\\'])
                    || pattern.chars().any(char::is_control)
            })
        {
            return Err(SavesError::Other(
                "save recognition needs nonempty file-name patterns".into(),
            ));
        }
    }
    Ok(())
}

fn survey(
    area: &SaveArea,
    recognition: Option<&SaveRecognition>,
) -> Result<SaveFolder, SavesError> {
    let path = &area.path;
    let mut saves = 0usize;
    let mut newest: Option<SystemTime> = None;
    let exists = checked_metadata(path)?.is_some();
    if exists {
        for entry in entries(path)? {
            let entry = entry.map_err(|source| SavesError::Read {
                path: path.clone(),
                source,
            })?;
            let file = entry.path();
            if area.excludes(&file) {
                continue;
            }
            let metadata = checked_metadata(&file)?
                .ok_or_else(|| SavesError::Other(format!("{} disappeared", file.display())))?;
            let is_save = match recognition {
                Some(SaveRecognition::Folders) => metadata.is_dir(),
                Some(SaveRecognition::Files { patterns }) if metadata.is_file() => {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    patterns
                        .iter()
                        .any(|pattern| glob_match(&pattern.to_lowercase(), &name))
                }
                _ => false,
            };
            if !is_save {
                continue;
            }
            saves += 1;
            if let Ok(modified) = metadata.modified() {
                newest = Some(newest.map_or(modified, |n| n.max(modified)));
            }
        }
    }
    Ok(SaveFolder {
        path: path.to_path_buf(),
        exists,
        saves: recognition.map(|_| saves),
        newest: newest.map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()),
    })
}

fn manifest_of(ctx: &Ctx, instance_id: &str) -> Result<GameInstanceManifest, SavesError> {
    game_instance::get_manifest(ctx, instance_id).map_err(|e| match e {
        InstanceError::NotFound(id) => SavesError::NotFound(id),
        other => SavesError::Other(other.to_string()),
    })
}

fn state_path(ctx: &Ctx, instance_id: &str) -> Result<PathBuf, SavesError> {
    Ok(ctx
        .paths
        .instance_dir(instance_id)
        .map_err(|e| SavesError::Other(e.to_string()))?
        .join(STATE_FILE))
}

pub(crate) fn read_state(path: &Path) -> Result<Option<SavesState>, SavesError> {
    if checked_metadata(path)?.is_none() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|source| SavesError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| SavesError::Other(format!("cannot read {}: {e}", path.display())))
}

fn write_state(path: &Path, state: &SavesState) -> Result<(), SavesError> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| SavesError::Other(format!("cannot record the save choice: {e}")))?;
    game_ini::write_atomic(path, &bytes).map_err(|e| match e {
        IniError::Write { path, source } => SavesError::Write { path, source },
        other => SavesError::Other(other.to_string()),
    })
}

/// The instance's save choice, its folders, and the game's setting as the instance sees it.
pub fn status(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<SavesStatus, SavesError> {
    let manifest = manifest_of(ctx, instance_id)?;
    let store = game_ini::store_of(ctx, instance_id)?;
    let folders = save_folders(
        definition,
        &store,
        instance_id,
        manifest.save_scope.as_deref(),
    )?;
    let (in_use, other) = folders.surveys(manifest.saves)?;
    let (setting_file, setting, setting_value, setting_source) = if let Some(rule) = folders.ini {
        let read = game_ini::read(ctx, instance_id, definition, rule.ini.as_str())?;
        (
            rule.ini.as_str().to_string(),
            format!("{}/{}", rule.section, rule.key),
            read.document.get(&rule.section, &rule.key),
            read.source,
        )
    } else {
        (String::new(), String::new(), None, IniSource::Nothing)
    };
    Ok(SavesStatus {
        instance_id: instance_id.to_string(),
        choice: manifest.saves,
        in_use,
        other,
        setting_file,
        setting,
        setting_value,
        setting_source,
    })
}

/// Make the instance's save choice `choice`. Refused while the game's session is running, like any
/// edit of the instance's INI files. Never moves, copies or deletes a save.
pub fn set_choice(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    choice: SavesChoice,
) -> Result<SavesChange, SavesError> {
    set_choice_while_running(ctx, instance_id, definition, choice, false)
}

/// [`set_choice`], with the user's explicit answer to a running game.
pub fn set_choice_while_running(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    choice: SavesChoice,
    allow_while_running: bool,
) -> Result<SavesChange, SavesError> {
    let scope = manifest_of(ctx, instance_id)?.save_scope;
    set_choice_with(
        ctx,
        instance_id,
        definition,
        choice,
        true,
        scope.as_deref(),
        allow_while_running,
    )
}

/// [`set_choice`]; with `create_folder` false it does not make the instance's own save folder. That
/// folder is under the player's Documents, so an import that was not asked to copy saves leaves it
/// alone. The setting still points at it.
pub(crate) fn set_choice_inner(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    choice: SavesChoice,
    create_folder: bool,
    scope: Option<&str>,
) -> Result<SavesChange, SavesError> {
    set_choice_with(
        ctx,
        instance_id,
        definition,
        choice,
        create_folder,
        scope,
        false,
    )
}

fn set_choice_with(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    choice: SavesChoice,
    create_folder: bool,
    scope: Option<&str>,
    allow_while_running: bool,
) -> Result<SavesChange, SavesError> {
    let store = game_ini::store_of(ctx, instance_id)?;
    let guard = game_ini::guard_for_ini(ctx, instance_id, definition, &store, allow_while_running)?;
    set_choice_locked(
        ctx,
        &guard,
        definition,
        &store,
        choice,
        create_folder,
        scope,
    )
}

/// [`set_choice_inner`] for a caller that already holds the instance guard (a loadout apply
/// holds it for the whole change).
pub(crate) fn set_choice_locked(
    ctx: &Ctx,
    guard: &InstanceGuard,
    definition: &GameDefinition,
    store: &StoreId,
    choice: SavesChoice,
    create_folder: bool,
    scope: Option<&str>,
) -> Result<SavesChange, SavesError> {
    let instance_id = guard.instance_id();
    let manifest = manifest_of(ctx, instance_id)?;
    let folders = save_folders(definition, store, instance_id, scope)?;
    let (mut in_use, other) = folders.surveys(choice)?;
    if choice == SavesChoice::Own && create_folder {
        fs::create_dir_all(&in_use.path).map_err(|source| SavesError::Write {
            path: in_use.path.clone(),
            source,
        })?;
        in_use.exists = true;
    }
    let setting = edit_save_setting(ctx, guard, definition, store, choice, scope, &folders)?;

    if manifest.saves != choice || manifest.save_scope.as_deref() != scope {
        let mut updated = manifest.clone();
        updated.saves = choice;
        updated.save_scope = scope.map(str::to_string);
        write_instance_manifest_atomic(ctx, instance_id, &updated)
            .map_err(|e| SavesError::Other(e.to_string()))?;
    }

    Ok(SavesChange {
        instance_id: instance_id.to_string(),
        previous: manifest.saves,
        choice,
        setting,
        in_use,
        other,
    })
}

pub(crate) fn validate_previous_value(
    rule: &SaveLocationRule,
    shared: &SaveArea,
    previous: Option<&str>,
) -> Result<(), SavesError> {
    if let Some(value) = previous.filter(|value| !value.is_empty()) {
        let path = resolve(&rule.relative_to)?.join(relative_save_path(value)?);
        if shared.excludes(&path) {
            return Err(SavesError::Other(
                "the previous save setting names private saves".into(),
            ));
        }
        checked_metadata(&path)?;
    }
    Ok(())
}

fn edit_save_setting(
    ctx: &Ctx,
    guard: &InstanceGuard,
    definition: &GameDefinition,
    store: &StoreId,
    choice: SavesChoice,
    scope: Option<&str>,
    folders: &SaveFolders<'_>,
) -> Result<SettingChange, SavesError> {
    let Some(rule) = folders.ini else {
        return Ok(SettingChange::NoSetting);
    };
    let instance_id = guard.instance_id();
    let state_file = state_path(ctx, instance_id)?;
    let recorded = read_state(&state_file)?;

    Ok(match choice {
        SavesChoice::Own => {
            let own = own_value_scoped(rule, instance_id, scope);
            let (before, changed) = game_ini::edit_with_guard(
                ctx,
                guard,
                definition,
                store,
                rule.ini.as_str(),
                |doc| {
                    let before = doc.get(&rule.section, &rule.key);
                    let changed = doc.set(&rule.section, &rule.key, &own)?;
                    Ok(((before, changed), changed))
                },
            )?;
            // A second `own` keeps the value from before the first one, so toggling is stable.
            // When a different loadout's folder replaces it, track the new value so returning to
            // shared still restores the player's own setting.
            match &recorded {
                None => write_state(
                    &state_file,
                    &SavesState {
                        ini: rule.ini.as_str().to_string(),
                        section: rule.section.clone(),
                        key: rule.key.clone(),
                        set_to: own.clone(),
                        previous: before,
                    },
                )?,
                Some(state) if state.set_to != own => write_state(
                    &state_file,
                    &SavesState {
                        set_to: own.clone(),
                        ..state.clone()
                    },
                )?,
                Some(_) => {}
            }
            if changed {
                SettingChange::Set { value: own }
            } else {
                SettingChange::Unchanged
            }
        }
        SavesChoice::Shared => match recorded {
            None => SettingChange::NothingRecorded,
            Some(state) => {
                if state.ini != rule.ini.as_str()
                    || state.section != rule.section
                    || state.key != rule.key
                {
                    return Err(SavesError::Other(
                        "the recorded save setting differs from the game's rule".into(),
                    ));
                }
                let change =
                    game_ini::edit_with_guard(ctx, guard, definition, store, &state.ini, |doc| {
                        let now = doc.get(&state.section, &state.key);
                        if now.as_deref() != Some(state.set_to.as_str()) {
                            return Ok((SettingChange::LeftAlone { now }, false));
                        }
                        validate_previous_value(rule, &folders.shared, state.previous.as_deref())
                            .map_err(|error| IniError::Other(error.to_string()))?;
                        match &state.previous {
                            Some(value) => {
                                let changed = doc.set(&state.section, &state.key, value)?;
                                Ok((
                                    SettingChange::Restored {
                                        value: value.clone(),
                                    },
                                    changed,
                                ))
                            }
                            None => {
                                let changed = doc.unset(&state.section, &state.key)?;
                                Ok((SettingChange::Removed, changed))
                            }
                        }
                    })?;
                // The record is spent either way: a later `own` records afresh.
                if state_file.exists() {
                    std::fs::remove_file(&state_file).map_err(|source| SavesError::Write {
                        path: state_file.clone(),
                        source,
                    })?;
                }
                change
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_id_is_its_own_folder_name() {
        assert_eq!(save_folder_name("survival-run"), "survival-run");
        assert_eq!(save_folder_name("p3_modded"), "p3_modded");
    }

    #[test]
    fn unusual_ids_get_safe_distinct_names() {
        let long = "long".repeat(40);
        let ids = [
            "a:b",
            "a_b",
            "a b",
            "a=b;c#d",
            "CON",
            "con",
            "NUL.txt",
            "COM1",
            "LPT9",
            "trailing.",
            "  leading",
            "ünïcode ✓",
            "x\"y<z>|w?*",
            "a\u{0}b",
            "Survival run #2 (x=y); ok",
            long.as_str(),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            let name = save_folder_name(id);
            assert!(!name.is_empty());
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{id:?} gave {name:?}"
            );
            assert!(!is_reserved_device_name(&name), "{id:?} gave {name:?}");
            assert!(name.len() <= CLEANED_NAME_MAX + 13, "{id:?} gave {name:?}");
            assert!(seen.insert(name.clone()), "{id:?} collides at {name:?}");
        }
    }

    #[test]
    fn the_own_value_names_no_separator_or_setting_character() {
        let rule = SaveLocationRule {
            ini: agora_game_api::RelPath::new("user/Skyrim.ini").unwrap(),
            section: "General".into(),
            key: "SLocalSavePath".into(),
            own_value: "Saves\\Agora\\{instance}\\".into(),
            shared_dir: GamePath::Runtime {
                path: agora_game_api::RelPath::default(),
            },
            relative_to: GamePath::Runtime {
                path: agora_game_api::RelPath::default(),
            },
            recognition: None,
            stores: Vec::new(),
        };
        let value = own_value(&rule, "Run #2; x=y: \"ok\" .");
        assert!(value.starts_with("Saves\\Agora\\"));
        assert!(value.ends_with('\\'));
        let folder = value
            .trim_start_matches("Saves\\Agora\\")
            .trim_end_matches('\\');
        assert!(!folder.contains(['\\', '/', ':', ';', '#', '=', '"', ' ', '\r', '\n']));
        assert!(!folder.is_empty());
    }
}
