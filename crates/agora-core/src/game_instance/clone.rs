//! Clone a generic game instance: the same content layers, plugin order, INIs and settings under
//! a new id and folder, never the saves unless the caller asks.
//!
//! The clone is built in a staging folder and renamed into place in one step, so a failure
//! removes the staging folder and leaves nothing behind. Saves live outside the instance folder
//! (in the game's own user-data folders), so the default clone cannot copy them by accident.

use std::path::{Path, PathBuf};

use agora_game_api::{GameDefinition, StoreId};

use crate::ctx::Ctx;
use crate::game_instance::{
    self, check_id_exists, generate_instance_id_from_name, GameInstanceManifest, InstanceError,
};
use crate::game_saves::{self, SavesError};
use crate::game_user_files;

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

/// Top-level entries a clone never copies. The manifest is rewritten, and logs and the VFS's own
/// config are per-launch state rather than the instance's content.
const SKIP_TOP_LEVEL: &[&str] = &["instance_manifest.json", "logs", "vfs"];

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
fn copy_tree(
    source: &Path,
    destination: &Path,
    skip_top_level: &[&str],
) -> Result<(), InstanceError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_top_level.iter().any(|skip| name == *skip) {
            continue;
        }
        let from = entry.path();
        let to = destination.join(&name);
        let metadata = std::fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            return Err(InstanceError::Other(format!(
                "'{}' is a link; Agora will not clone an instance whose files point outside it.",
                from.display()
            )));
        }
        if metadata.is_dir() {
            copy_tree(&from, &to, &[])?;
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
) -> Result<(), InstanceError> {
    let source_dir = ctx
        .paths
        .instance_dir(source_id)
        .map_err(|e| InstanceError::InvalidId(source_id.to_string(), e.to_string()))?;
    copy_tree(&source_dir, staging, SKIP_TOP_LEVEL)?;

    let mut manifest = source_manifest.clone();
    manifest.instance_id = new_id.to_string();
    manifest.name = new_name.to_string();
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(staging.join("instance_manifest.json"), manifest_bytes)?;

    // The source's save setting names the source's folder. The clone's choice is recorded fresh
    // (when it has its own saves) after the move, so a stale record must not be carried over.
    let spent_state = staging.join("saves_state.json");
    if spent_state.exists() {
        std::fs::remove_file(&spent_state)?;
    }
    Ok(())
}

/// Point a clone at its own save folder: rewrite the copied setting through the same service the
/// Saves page uses, create the folder, and copy the source's saves into it when `source` is named.
/// Returns the folder Agora created or filled.
fn reuse_own_saves(
    ctx: &Ctx,
    new_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    source: Option<&Path>,
) -> Result<PathBuf, InstanceError> {
    let rule = game_saves::rule_for(definition, store).ok_or_else(|| {
        InstanceError::Saves(SavesError::NoSaveLocation {
            game: definition.id.as_str().to_string(),
            store: store.as_str().to_string(),
        })
    })?;
    let folder = game_saves::own_folder(rule, new_id)?;
    game_saves::set_choice_inner(
        ctx,
        new_id,
        definition,
        game_instance::SavesChoice::Own,
        false,
    )?;
    std::fs::create_dir_all(&folder)?;
    if let Some(source) = source {
        if source.is_dir() {
            copy_tree(source, &folder, &[])?;
        }
    }
    Ok(folder)
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
    let source_manifest = game_instance::get_manifest(ctx, source_id)?;
    let store = store_of(ctx, source_id)?;

    let _lock = ctx
        .lock_manager
        .acquire(
            crate::lock_manager::LockResource::Instance(source_id.to_string()),
            "clone",
        )
        .map_err(|e| InstanceError::Other(e.to_string()))?;

    // A running game is writing the files a clone would copy: wait for it to close. Judged with
    // the lock held, because a launch that was starting when the clone was asked holds the same
    // lock, and its game is running by the time the clone gets it.
    if let Some(session) = game_user_files::status(ctx, &definition.id, &store)? {
        if session.running && session.instance == source_id {
            return Err(InstanceError::Running(source_id.to_string()));
        }
    }

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
    let rule = game_saves::rule_for(definition, &store).cloned();
    let source_saves = if options.copy_saves {
        let rule = rule.as_ref().ok_or_else(|| {
            InstanceError::Saves(SavesError::NoSaveLocation {
                game: definition.id.as_str().to_string(),
                store: store.as_str().to_string(),
            })
        })?;
        Some(match source_manifest.saves {
            game_instance::SavesChoice::Own => game_saves::own_folder(rule, source_id)?,
            game_instance::SavesChoice::Shared => game_saves::shared_folder(rule)?,
        })
    } else {
        None
    };
    if needs_own_saves && rule.is_none() {
        return Err(InstanceError::Saves(SavesError::NoSaveLocation {
            game: definition.id.as_str().to_string(),
            store: store.as_str().to_string(),
        }));
    }
    let clone_save_folder = match (&rule, needs_own_saves) {
        (Some(rule), true) => Some(game_saves::own_folder(rule, &new_id)?),
        _ => None,
    };

    let staging = ctx
        .paths
        .staging_dir(&format!("clone-{}", uuid::Uuid::new_v4()))
        .map_err(|error| InstanceError::Other(error.to_string()))?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    if let Err(error) = build_staging(
        ctx,
        source_id,
        &new_id,
        new_name,
        &staging,
        &source_manifest,
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

    let rollback = |destination: &Path, save_folder: &Option<PathBuf>| {
        let _ = std::fs::remove_dir_all(destination);
        if let Some(folder) = save_folder {
            // A folder for a brand-new id is Agora's, so it may go when the clone fails.
            let _ = std::fs::remove_dir_all(folder);
        }
    };

    if needs_own_saves {
        if let Err(error) =
            reuse_own_saves(ctx, &new_id, definition, &store, source_saves.as_deref())
        {
            rollback(&destination, &clone_save_folder);
            return Err(error);
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
        rollback(&destination, &clone_save_folder);
        return Err(error.into());
    }

    Ok(ClonedInstance {
        instance_id: new_id,
        name: new_name.to_string(),
    })
}
