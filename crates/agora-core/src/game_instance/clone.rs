//! Clone a generic game instance: the same content layers, plugin order, INIs and settings under
//! a new id and folder, never the saves unless the caller asks.
//!
//! The clone is built in a staging folder and renamed into place in one step, so a failure
//! removes the staging folder and leaves nothing behind. Saves live outside the instance folder
//! (in the game's own user-data folders), so the default clone cannot copy them by accident.

use std::path::Path;

use agora_game_api::{GameDefinition, StoreId};

use crate::content_store::is_reparse_point_or_symlink;
use crate::ctx::Ctx;
use crate::game_instance::{
    self, check_id_exists, generate_instance_id_from_name, GameInstanceManifest, InstanceError,
};
use crate::game_saves::{self, SavesError, SavesState, STATE_FILE};
use crate::instance_runtime::{BusyCause, MayMutate};
use crate::save_backup::{copy_save_files, BACKUP_DIR};

/// What a clone is asked to take from its source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloneOptions {
    /// Copy the source's saves into the clone's own save folder. Off by default: a save is the
    /// player's progress, and a clone usually wants its own.
    pub copy_saves: bool,
}

/// The instance a clone made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonedInstance {
    pub instance_id: String,
    pub name: String,
}

/// The manifest gets a new identity. Launch logs, the VFS config and overlay work directory
/// belong to the source's launches. Undo and backup history belong to the source's operations.
/// Carry forward only the validated pre-isolation save setting, when one is needed.
const SKIP_PATHS: &[&str] = &[
    "instance_manifest.json",
    "logs",
    "vfs",
    "view-work",
    ".agora_loadouts/undo",
    BACKUP_DIR,
    STATE_FILE,
];

fn store_of(ctx: &Ctx, instance_id: &str) -> Result<StoreId, InstanceError> {
    let record = game_instance::get(ctx, instance_id)?
        .ok_or_else(|| InstanceError::NotFound(instance_id.to_string()))?;
    let manifest = game_instance::get_manifest(ctx, instance_id)?;
    game_instance::session_store(&record.base, &manifest).ok_or_else(|| {
        InstanceError::Other(format!(
            "'{instance_id}' has never been launched through a store Agora can name."
        ))
    })
}

/// Copy a folder tree, never following a link out of it. A link fails the clone rather than being
/// silently dropped, because the result would not be the instance the player asked for.
fn copy_tree(source: &Path, destination: &Path, relative: &Path) -> Result<(), InstanceError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let relative = relative.join(&name);
        if SKIP_PATHS.iter().any(|skip| {
            crate::game_user_files::paths_eq_case_insensitive(&relative, Path::new(skip))
        }) {
            continue;
        }
        let from = entry.path();
        let to = destination.join(&name);
        let metadata = std::fs::symlink_metadata(&from)?;
        if is_reparse_point_or_symlink(&metadata) {
            return Err(InstanceError::Other(format!(
                "'{}' is a link; Agora will not clone an instance whose files point outside it.",
                from.display()
            )));
        }
        if metadata.is_dir() {
            copy_tree(&from, &to, &relative)?;
        } else if metadata.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Build a copy of `source` in `staging`: files, then the manifest with its new id and name.
fn build_staging(
    ctx: &Ctx,
    source_id: &str,
    new_id: &str,
    new_name: &str,
    staging: &Path,
    source_manifest: &GameInstanceManifest,
    save_state: Option<&SavesState>,
) -> Result<(), InstanceError> {
    let source_dir = ctx
        .paths
        .instance_dir(source_id)
        .map_err(|e| InstanceError::InvalidId(source_id.to_string(), e.to_string()))?;
    game_saves::checked_metadata(&source_dir, ctx.paths.root())?;
    copy_tree(&source_dir, staging, Path::new(""))?;

    let mut manifest = source_manifest.clone();
    manifest.instance_id = new_id.to_string();
    manifest.name = new_name.to_string();
    // The clone gets its own, unscoped folder; it never borrows a loadout's save folder.
    manifest.save_scope = None;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(staging.join("instance_manifest.json"), manifest_bytes)?;

    if let Some(state) = save_state {
        std::fs::write(staging.join(STATE_FILE), serde_json::to_vec_pretty(state)?)?;
    }
    Ok(())
}

/// Clone an instance. Refuses while the source is running, and refuses a source whose folder or
/// manifest cannot be read.
pub fn clone_instance(
    ctx: &Ctx,
    source_id: &str,
    new_name: &str,
    definition: &GameDefinition,
    options: CloneOptions,
) -> Result<ClonedInstance, InstanceError> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(InstanceError::Other("Give the new instance a name.".into()));
    }
    if !game_instance::get(ctx, source_id)?.is_some_and(|record| record.game == definition.id) {
        return Err(InstanceError::NotFound(source_id.to_string()));
    }
    let source_dir = ctx
        .paths
        .instance_dir(source_id)
        .map_err(|error| InstanceError::InvalidId(source_id.to_string(), error.to_string()))?;
    let source_manifest_path = source_dir.join("instance_manifest.json");
    game_saves::checked_metadata(&source_manifest_path, &source_dir)?;
    // A running game is writing the files a clone would copy. The shared guard
    // takes the instance lock and checks the one "is this game running" answer;
    // it fails at once rather than waiting out the lock. The typed answer says
    // which refusal this is, so nothing here compares error codes.
    let _guard = match crate::instance_runtime::may_mutate(ctx, source_id, "clone") {
        MayMutate::Yes(guard) => guard,
        MayMutate::Busy {
            cause: BusyCause::Game,
            ..
        } => return Err(InstanceError::Running(source_id.to_string())),
        MayMutate::Busy { error, .. } | MayMutate::CantTell { error } => {
            return Err(InstanceError::Other(error.to_string()))
        }
    };
    game_saves::checked_metadata(&source_manifest_path, &source_dir)?;
    let source_manifest = game_instance::get_manifest(ctx, source_id)?;
    let store = store_of(ctx, source_id)?;

    // The id is settled before any file is copied, so a taken name costs nothing.
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let mut candidate = generate_instance_id_from_name(new_name);
    for _ in 0..100 {
        if !check_id_exists(&ctx.paths, &conn, &candidate)? {
            break;
        }
        candidate = generate_instance_id_from_name(new_name);
    }
    if check_id_exists(&ctx.paths, &conn, &candidate)? {
        return Err(InstanceError::AlreadyExists(candidate));
    }
    let new_id = candidate;

    // Settle the save story before copying files: a game with no save rule must fail without
    // leaving a half-built clone behind.
    let needs_own_saves =
        options.copy_saves || source_manifest.saves == game_instance::SavesChoice::Own;
    let mut source_saves = None;
    let mut save_state = None;
    let clone_save_folder = if needs_own_saves {
        let scope = source_manifest.save_scope.as_deref();
        let folders = game_saves::save_folders(definition, &store, source_id, scope)?;
        let clone_folders = game_saves::save_folders(definition, &store, &new_id, None)?;
        let destination = &clone_folders.in_use(game_instance::SavesChoice::Own)?.path;
        if game_saves::checked_metadata(
            destination,
            destination.parent().unwrap_or(destination.as_path()),
        )?
        .is_some()
        {
            return Err(InstanceError::Other(format!(
                "the clone's save directory already exists at {}",
                destination.display()
            )));
        }
        if options.copy_saves {
            let area = folders.in_use(source_manifest.saves)?;
            let files = area.files()?;
            if files.is_empty() {
                return Err(InstanceError::Other(
                    "the source has no eligible save files to copy".into(),
                ));
            }
            source_saves = Some((area.path.clone(), files));
        }
        if let Some(rule) = folders
            .ini
            .filter(|_| source_manifest.saves == game_instance::SavesChoice::Own)
        {
            let state = game_saves::read_state(&source_dir.join(STATE_FILE), &source_dir)?
                .ok_or_else(|| {
                    SavesError::Other(
                        "could not tell what the source's save setting was before isolation".into(),
                    )
                })?;
            if state.ini != rule.ini.as_str()
                || state.section != rule.section
                || state.key != rule.key
                || state.set_to != game_saves::own_value_scoped(rule, source_id, scope)
            {
                return Err(SavesError::Other(
                    "the source's recorded save setting differs from its save rule".into(),
                )
                .into());
            }
            game_saves::validate_previous_value(rule, &folders.shared, state.previous.as_deref())?;
            save_state = Some(state);
        }
        Some(destination.clone())
    } else {
        None
    };

    let staging = ctx
        .paths
        .staging_dir(&format!("clone-{}", uuid::Uuid::new_v4()))
        .map_err(|error| InstanceError::Other(error.to_string()))?;
    if let Err(error) = build_staging(
        ctx,
        source_id,
        &new_id,
        new_name,
        &staging,
        &source_manifest,
        save_state.as_ref(),
    ) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let destination = ctx
        .paths
        .instance_dir(&new_id)
        .map_err(|e| InstanceError::InvalidId(new_id.clone(), e.to_string()))?;
    if std::fs::symlink_metadata(&destination).is_ok() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(InstanceError::AlreadyExists(new_id));
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(error) = std::fs::rename(&staging, &destination) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error.into());
    }

    let rollback = |save_folder: Option<&Path>| {
        let _ = std::fs::remove_dir_all(&destination);
        if let Some(folder) = save_folder {
            if game_saves::checked_metadata(folder, folder.parent().unwrap_or(folder))
                .is_ok_and(|metadata| metadata.is_some())
            {
                let _ = std::fs::remove_dir_all(folder);
            }
        }
    };

    if let Some(folder) = clone_save_folder.as_deref() {
        let result = (|| -> Result<(), InstanceError> {
            game_saves::set_choice_inner(
                ctx,
                &new_id,
                definition,
                game_instance::SavesChoice::Own,
                false,
                None,
            )?;
            game_saves::checked_metadata(folder, folder.parent().unwrap_or(folder))?;
            std::fs::create_dir_all(folder.parent().expect("the save folder has a parent"))?;
            std::fs::create_dir(folder)?;
            Ok(())
        })();
        if let Err(error) = result {
            rollback(None);
            return Err(error);
        }
        if let Some((source, files)) = source_saves {
            if let Err(error) = copy_save_files(&files, &source, folder) {
                rollback(Some(folder));
                return Err(InstanceError::Other(error.to_string()));
            }
        }
    }

    let inserted = conn.execute(
        "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, last_launched_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'), NULL)",
        rusqlite::params![
            &new_id,
            definition.id.as_str(),
            new_name,
            serde_json::to_string(&source_manifest.base)?
        ],
    );
    if let Err(error) = inserted {
        rollback(clone_save_folder.as_deref());
        return Err(error.into());
    }

    Ok(ClonedInstance {
        instance_id: new_id,
        name: new_name.to_string(),
    })
}
