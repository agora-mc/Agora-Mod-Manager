//! "Recently deleted" for generic game instances.
//!
//! Deleting an instance does not destroy it. Its folder is renamed (on the same volume, so the
//! move is atomic) into `<data>/recently-deleted-games/<entry-id>/instance`, next to an
//! `entry.json` that holds its `game_instances` row. Restoring puts both back, so the instance is
//! exactly as it was. Entries older than the user's retention window are handed to the OS trash.
//!
//! The on-disk model follows Minecraft's `recently_deleted.rs` (an Agora-owned folder beside a
//! JSON record, a plain same-volume rename, and every step failing closed). It is a separate
//! module because that one is typed around `user_instances` and Minecraft's columns; forcing both
//! shapes through it would have changed Minecraft's delete while this slice is about games. The
//! proven parts (link handling, entry validation, the folder layout) are the same.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::app_paths::validate_path_component;
use crate::ctx::Ctx;
use crate::game_instance::{generate_instance_id_from_name, InstanceError};
use crate::settings::SettingsService;

/// How long an entry stays restorable when the user has not chosen a window.
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
/// A year and a day: a wild setting is clamped rather than accepted as "never purge".
pub const MAX_RETENTION_DAYS: i64 = 366;
/// The setting that holds the retention window, in days.
pub const RETENTION_SETTING: &str = "games_recently_deleted_days";

const ENTRY_FILE: &str = "entry.json";
const INSTANCE_SUBDIR: &str = "instance";
const ENTRY_VERSION: u32 = 1;

/// Sends one Agora-owned folder to the OS trash. The adapter supplies it so core never depends on
/// a platform crate; a failure leaves the entry exactly where it was.
pub type TrashFn = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// One restorable entry, as the Instances page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedEntry {
    pub entry_id: String,
    pub instance_id: String,
    pub game: String,
    pub name: String,
    pub deleted_at: String,
    pub expires_at: String,
    pub size_bytes: u64,
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    pub instance_id: String,
    pub original_instance_id: String,
    /// True when the original id was taken and the instance came back under a new one.
    pub renamed: bool,
}

/// Result of a purge pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeSummary {
    pub purged: usize,
    /// Entries that should have gone but could not; left in place.
    pub failed: usize,
    pub kept: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GameRow {
    pub(crate) instance_id: String,
    game: String,
    name: String,
    base_json: String,
    created_at: String,
    last_launched_at: Option<String>,
    total_play_ms: i64,
    last_played_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEntry {
    version: u32,
    #[serde(flatten)]
    entry: DeletedEntry,
    row: GameRow,
}

fn err(code: &'static str, message: impl Into<String>) -> InstanceError {
    InstanceError::Trash {
        code,
        message: message.into(),
    }
}

fn io_err(code: &'static str, what: &str, error: std::io::Error) -> InstanceError {
    err(code, format!("{what}: {error}"))
}

fn db_err(error: impl std::fmt::Display) -> InstanceError {
    InstanceError::Other(format!("ERR_LOCAL_STATE_FAILED: {error}"))
}

// ---------------------------------------------------------------------------
// Link-safe helpers (the same rules Minecraft's recently deleted area uses)
// ---------------------------------------------------------------------------

fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn remove_link(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if metadata.file_type().is_symlink_dir() {
            return std::fs::remove_dir(path);
        }
    }
    let _ = metadata;
    std::fs::remove_file(path)
}

fn tree_size(root: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

struct RemovedLink {
    path: PathBuf,
    target: PathBuf,
    is_dir: bool,
    junction: bool,
}

fn recreate_link(link: &RemovedLink) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if link.junction {
            return junction::create(&link.target, &link.path);
        }
        if link.is_dir {
            return std::os::windows::fs::symlink_dir(&link.target, &link.path);
        }
        std::os::windows::fs::symlink_file(&link.target, &link.path)
    }
    #[cfg(unix)]
    {
        let _ = (link.is_dir, link.junction);
        std::os::unix::fs::symlink(&link.target, &link.path)
    }
}

fn restore_links(links: &[RemovedLink]) -> usize {
    links.iter().filter(|l| recreate_link(l).is_err()).count()
}

/// Remove every link under `root` (the links only, never their targets), putting them back if
/// anything fails part way.
fn detach_links(root: &Path) -> std::io::Result<Vec<RemovedLink>> {
    let mut removed: Vec<RemovedLink> = Vec::new();
    let result = (|| -> std::io::Result<()> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path)?;
                if is_link(&metadata) {
                    #[cfg(windows)]
                    let (target, junction, is_dir) = {
                        use std::os::windows::fs::FileTypeExt;
                        let junction = junction::exists(&path).unwrap_or(false);
                        let target = if junction {
                            junction::get_target(&path)?
                        } else {
                            std::fs::read_link(&path)?
                        };
                        (target, junction, metadata.file_type().is_symlink_dir())
                    };
                    #[cfg(unix)]
                    let (target, junction, is_dir) = (std::fs::read_link(&path)?, false, false);
                    remove_link(&path, &metadata)?;
                    removed.push(RemovedLink {
                        path,
                        target,
                        is_dir,
                        junction,
                    });
                } else if metadata.is_dir() {
                    stack.push(path);
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(removed),
        Err(error) => {
            restore_links(&removed);
            Err(error)
        }
    }
}

fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| !is_link(&m) && m.is_dir())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The retention window in days. A missing or unreadable setting reads as the default, and a
/// wild value is clamped: this is a convenience knob, never a way to purge on the next pass by
/// accident.
pub fn retention_days(ctx: &Ctx) -> i64 {
    let value = SettingsService::new(ctx.clone())
        .get(RETENTION_SETTING)
        .ok()
        .flatten()
        .and_then(|value| match value {
            serde_json::Value::Number(number) => number.as_i64(),
            serde_json::Value::String(text) => text.trim().parse().ok(),
            _ => None,
        });
    value
        .unwrap_or(DEFAULT_RETENTION_DAYS)
        .clamp(1, MAX_RETENTION_DAYS)
}

fn expires_at(deleted_at: &str, days: i64) -> String {
    chrono::DateTime::parse_from_rfc3339(deleted_at)
        .map(|at| (at + chrono::Duration::days(days)).to_rfc3339())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Entry storage
// ---------------------------------------------------------------------------

fn entry_dir(root: &Path, entry_id: &str) -> Result<PathBuf, InstanceError> {
    validate_path_component(entry_id)
        .map_err(|error| err("ERR_INSTANCE_RESTORE", error.to_string()))?;
    Ok(root.join(entry_id))
}

fn read_entry_file(root: &Path, entry_id: &str) -> Result<StoredEntry, InstanceError> {
    let dir = entry_dir(root, entry_id)?;
    let text = std::fs::read_to_string(dir.join(ENTRY_FILE))
        .map_err(|e| io_err("ERR_INSTANCE_RESTORE", "Cannot read the deleted entry", e))?;
    let stored: StoredEntry = serde_json::from_str(&text)
        .map_err(|e| err("ERR_INSTANCE_RESTORE", format!("Entry is unreadable: {e}")))?;
    if stored.version != ENTRY_VERSION || stored.entry.entry_id != entry_id {
        return Err(err(
            "ERR_INSTANCE_RESTORE",
            "Entry is from a different version or does not match its folder.",
        ));
    }
    if stored.entry.instance_id != stored.row.instance_id {
        return Err(err(
            "ERR_INSTANCE_RESTORE",
            "Entry and its instance row disagree about which instance this is.",
        ));
    }
    validate_path_component(&stored.entry.instance_id)
        .map_err(|error| err("ERR_INSTANCE_RESTORE", error.to_string()))?;
    if agora_game_api::GameId::new(&stored.entry.game).is_err() {
        return Err(err("ERR_INSTANCE_RESTORE", "Entry names an invalid game."));
    }
    if serde_json::from_str::<agora_game_api::BaseReference>(&stored.row.base_json).is_err() {
        return Err(err(
            "ERR_INSTANCE_RESTORE",
            "Entry's base record is unreadable.",
        ));
    }
    Ok(stored)
}

fn read_entry(root: &Path, entry_id: &str) -> Result<StoredEntry, InstanceError> {
    let stored = read_entry_file(root, entry_id)?;
    if !is_plain_dir(&entry_dir(root, entry_id)?.join(INSTANCE_SUBDIR)) {
        return Err(err(
            "ERR_INSTANCE_RESTORE",
            "The deleted instance's files are missing from this entry.",
        ));
    }
    Ok(stored)
}

/// Read the instance's row. A missing row is an error: delete never guesses.
pub(crate) fn read_row(conn: &Connection, instance_id: &str) -> Result<GameRow, InstanceError> {
    conn.query_row(
        "SELECT instance_id, game, name, base_json, created_at, last_launched_at,
                total_play_ms, last_played_at
         FROM game_instances WHERE instance_id = ?1",
        [instance_id],
        |row| {
            Ok(GameRow {
                instance_id: row.get(0)?,
                game: row.get(1)?,
                name: row.get(2)?,
                base_json: row.get(3)?,
                created_at: row.get(4)?,
                last_launched_at: row.get(5)?,
                total_play_ms: row.get(6)?,
                last_played_at: row.get(7)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| InstanceError::NotFound(instance_id.to_string()))
}

/// Move a live instance folder into a new entry. The caller deletes the row afterwards and
/// rolls the move back if that fails. On any failure here nothing is changed.
pub(crate) fn stash(
    root: &Path,
    row: GameRow,
    instance_dir: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<DeletedEntry, InstanceError> {
    if !is_plain_dir(instance_dir) {
        return Err(err(
            "ERR_INSTANCE_DELETE",
            "The instance folder is a link rather than a real folder; nothing was deleted.",
        ));
    }
    let size_bytes = tree_size(instance_dir)
        .map_err(|e| io_err("ERR_INSTANCE_DELETE", "Cannot measure the instance", e))?;
    let entry_id = format!("{}-{}", now.format("%Y%m%dT%H%M%SZ"), uuid::Uuid::new_v4());
    let deleted_at = now.to_rfc3339();
    let entry = DeletedEntry {
        entry_id: entry_id.clone(),
        instance_id: row.instance_id.clone(),
        game: row.game.clone(),
        name: row.name.clone(),
        deleted_at: deleted_at.clone(),
        expires_at: expires_at(&deleted_at, DEFAULT_RETENTION_DAYS),
        size_bytes,
    };
    let stored = StoredEntry {
        version: ENTRY_VERSION,
        entry: entry.clone(),
        row,
    };
    let bytes = serde_json::to_vec_pretty(&stored)
        .map_err(|e| err("ERR_INSTANCE_DELETE", format!("Cannot record entry: {e}")))?;

    let dir = entry_dir(root, &entry_id)?;
    let abandon = |dir: &Path| {
        let _ = std::fs::remove_dir_all(dir);
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| io_err("ERR_INSTANCE_DELETE", "Cannot create Recently deleted", e))?;
    let tmp = dir.join("entry.json.tmp");
    if let Err(e) =
        std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, dir.join(ENTRY_FILE)))
    {
        abandon(&dir);
        return Err(io_err("ERR_INSTANCE_DELETE", "Cannot record entry", e));
    }
    let target = dir.join(INSTANCE_SUBDIR);
    if let Err(e) = std::fs::rename(instance_dir, &target) {
        abandon(&dir);
        return Err(io_err(
            "ERR_INSTANCE_DELETE",
            "Could not move the instance to Recently deleted (is the game running?)",
            e,
        ));
    }
    Ok(entry)
}

/// Put a stashed folder back after its row could not be deleted.
pub(crate) fn undo_stash(
    root: &Path,
    entry: &DeletedEntry,
    instance_dir: &Path,
) -> Result<(), InstanceError> {
    let dir = entry_dir(root, &entry.entry_id)?;
    std::fs::rename(dir.join(INSTANCE_SUBDIR), instance_dir)
        .map_err(|e| io_err("ERR_INSTANCE_DELETE", "Cannot put the instance back", e))?;
    let _ = std::fs::remove_file(dir.join(ENTRY_FILE));
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// All valid entries for one game, newest first. Unreadable or incomplete entries are skipped
/// (and left on disk).
pub fn list(ctx: &Ctx, game_id: &str) -> Vec<DeletedEntry> {
    let root = ctx.paths.recently_deleted_games_root();
    let days = retention_days(ctx);
    let Ok(read) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<DeletedEntry> = read
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|id| read_entry(&root, &id).ok())
        .map(|stored| {
            let mut entry = stored.entry;
            entry.expires_at = expires_at(&entry.deleted_at, days);
            entry
        })
        .filter(|entry| entry.game == game_id)
        .collect();
    out.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    out
}

fn id_is_free(conn: &Connection, instances_root: &Path, id: &str) -> Result<bool, InstanceError> {
    for query in [
        "SELECT 1 FROM game_instances WHERE instance_id = ?1",
        "SELECT 1 FROM user_instances WHERE instance_id = ?1",
    ] {
        if conn
            .query_row(query, [id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
    }
    Ok(std::fs::symlink_metadata(instances_root.join(id)).is_err())
}

fn free_id_for_name(
    conn: &Connection,
    instances_root: &Path,
    name: &str,
) -> Result<String, InstanceError> {
    for _ in 0..100 {
        let candidate = generate_instance_id_from_name(name);
        if id_is_free(conn, instances_root, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(err(
        "ERR_INSTANCE_RESTORE",
        "Could not find a free id to restore this instance under.",
    ))
}

/// Written into an entry before its files are moved out, so a restore cut off part way can be
/// finished (or called off) from the entry alone.
const RESTORING_FILE: &str = "restoring.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Restoring {
    new_id: String,
    new_name: String,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

fn try_lock_instance(
    ctx: &Ctx,
    instance_id: &str,
    operation: &str,
) -> Result<crate::lock_manager::LockGuard, InstanceError> {
    // No waiting: an instance another operation holds is busy, and a pass over the trash moves
    // on rather than queueing behind it.
    ctx.lock_manager
        .acquire_with_timeout(
            crate::lock_manager::LockResource::Instance(instance_id.to_string()),
            operation,
            std::time::Duration::ZERO,
            None,
        )
        .map_err(|error| err("ERR_INSTANCE_BUSY", error.to_string()))
}

fn row_exists(conn: &Connection, id: &str) -> Result<bool, InstanceError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM game_instances WHERE instance_id = ?1",
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Put the manifest's id and name right, then add the row. The files are already in place.
fn finish_restore(
    conn: &Connection,
    dest: &Path,
    stored: &StoredEntry,
    restoring: &Restoring,
) -> Result<(), InstanceError> {
    if restoring.new_id != stored.entry.instance_id || restoring.new_name != stored.row.name {
        let manifest_path = dest.join("instance_manifest.json");
        let bytes = std::fs::read(&manifest_path)
            .map_err(|e| io_err("ERR_INSTANCE_RESTORE", "Cannot read the manifest", e))?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| err("ERR_INSTANCE_RESTORE", format!("Manifest unreadable: {e}")))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| err("ERR_INSTANCE_RESTORE", "Manifest is not an object."))?;
        object.insert(
            "instance_id".into(),
            serde_json::Value::String(restoring.new_id.clone()),
        );
        object.insert(
            "name".into(),
            serde_json::Value::String(restoring.new_name.clone()),
        );
        let out = serde_json::to_vec_pretty(&value)
            .map_err(|e| err("ERR_INSTANCE_RESTORE", e.to_string()))?;
        write_atomic(&manifest_path, &out)
            .map_err(|e| io_err("ERR_INSTANCE_RESTORE", "Cannot update manifest", e))?;
    }
    let row = &stored.row;
    conn.execute(
        "INSERT INTO game_instances
             (instance_id, game, name, base_json, created_at, last_launched_at,
              total_play_ms, last_played_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            &restoring.new_id,
            &row.game,
            &restoring.new_name,
            &row.base_json,
            &row.created_at,
            &row.last_launched_at,
            row.total_play_ms,
            &row.last_played_at,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

fn tidy_entry(root: &Path, entry_id: &str) {
    if let Ok(dir) = entry_dir(root, entry_id) {
        let _ = std::fs::remove_file(dir.join(ENTRY_FILE));
        let _ = std::fs::remove_file(dir.join(RESTORING_FILE));
        let _ = std::fs::remove_dir(&dir);
    }
}

/// Bring one entry's loose ends back to a state a person can act on, with the entry's original
/// instance id locked by the caller:
/// * a delete cut off after the folder moved but before the row went leaves a row with no folder;
///   the entry holds the files, so the row is dropped and the entry is restorable;
/// * a restore cut off after the move leaves the folder in place and no row; it is finished;
/// * a restore cut off before the move is called off.
fn recover_entry(
    ctx: &Ctx,
    conn: &Connection,
    root: &Path,
    stored: &StoredEntry,
) -> Result<(), InstanceError> {
    let entry_id = &stored.entry.entry_id;
    let dir = entry_dir(root, entry_id)?;
    let instances_root = ctx.paths.instances_root();
    let source = dir.join(INSTANCE_SUBDIR);

    let restoring_path = dir.join(RESTORING_FILE);
    if restoring_path.is_file() {
        let restoring: Option<Restoring> = std::fs::read_to_string(&restoring_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let Some(restoring) = restoring else {
            return Ok(());
        };
        validate_path_component(&restoring.new_id)
            .map_err(|error| err("ERR_INSTANCE_RESTORE", error.to_string()))?;
        if is_plain_dir(&source) {
            // The files never left: the restore did not get far enough to matter.
            let _ = std::fs::remove_file(&restoring_path);
        } else if is_plain_dir(&instances_root.join(&restoring.new_id))
            && !row_exists(conn, &restoring.new_id)?
        {
            finish_restore(
                conn,
                &instances_root.join(&restoring.new_id),
                stored,
                &restoring,
            )?;
            tidy_entry(root, entry_id);
        }
        return Ok(());
    }

    if is_plain_dir(&source)
        && row_exists(conn, &stored.entry.instance_id)?
        && std::fs::symlink_metadata(instances_root.join(&stored.entry.instance_id)).is_err()
    {
        conn.execute(
            "DELETE FROM game_instances WHERE instance_id = ?1",
            [&stored.entry.instance_id],
        )?;
    }
    Ok(())
}

/// Finish or call off whatever a crash left half done in Recently deleted. Safe to run at any
/// time: an entry whose instance is busy is skipped, and nothing is touched unless an entry's
/// own record says what happened.
pub fn recover_interrupted(ctx: &Ctx) {
    let root = ctx.paths.recently_deleted_games_root();
    let Ok(read) = std::fs::read_dir(&root) else {
        return;
    };
    let Ok(conn) = crate::db::local_state_connection(&ctx.paths.local_state_db()) else {
        return;
    };
    for id in read
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
    {
        let Ok(stored) = read_entry_file(&root, &id) else {
            continue;
        };
        let Ok(_lock) = try_lock_instance(ctx, &stored.entry.instance_id, "recover") else {
            continue;
        };
        let _ = recover_entry(ctx, &conn, &root, &stored);
    }
}

/// Restore an entry. With no `new_name`, the original id is restored and a taken id is refused
/// with [`InstanceError::RestoreIdTaken`]; with a name, a fresh id comes from it and the manifest
/// and row are rewritten together.
///
/// The entry is marked before anything moves, so a crash at any point leaves either the entry
/// whole or an instance that [`recover_interrupted`] can finish.
pub fn restore(
    ctx: &Ctx,
    entry_id: &str,
    new_name: Option<&str>,
) -> Result<RestoreOutcome, InstanceError> {
    let root = ctx.paths.recently_deleted_games_root();
    let stored = read_entry_file(&root, entry_id)?;
    let original = stored.entry.instance_id.clone();

    // Serialize with a delete or another writer of the same original id: the id is what both
    // operations judge as taken or free.
    let _lock = ctx
        .lock_manager
        .acquire(
            crate::lock_manager::LockResource::Instance(original.clone()),
            "restore",
        )
        .map_err(|error| err("ERR_INSTANCE_RESTORE", error.to_string()))?;

    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let instances_root = ctx.paths.instances_root();

    // A cut-off delete left a row behind with no folder; it is not a real holder of the id.
    recover_entry(ctx, &conn, &root, &stored)?;
    let stored = read_entry(&root, entry_id)?;
    let source = entry_dir(&root, entry_id)?.join(INSTANCE_SUBDIR);

    let (new_id, new_name, renamed) = match new_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => {
            let id = free_id_for_name(&conn, &instances_root, name)?;
            (id, name.to_string(), true)
        }
        None => {
            if !id_is_free(&conn, &instances_root, &original)? {
                return Err(InstanceError::RestoreIdTaken(original));
            }
            (original.clone(), stored.entry.name.clone(), false)
        }
    };

    std::fs::create_dir_all(&instances_root)
        .map_err(|e| io_err("ERR_INSTANCE_RESTORE", "Cannot prepare instances folder", e))?;
    let dest = instances_root.join(&new_id);
    if std::fs::symlink_metadata(&dest).is_ok() {
        return Err(InstanceError::RestoreIdTaken(new_id));
    }

    let restoring = Restoring {
        new_id: new_id.clone(),
        new_name: new_name.clone(),
    };
    let restoring_path = entry_dir(&root, entry_id)?.join(RESTORING_FILE);
    let marker =
        serde_json::to_vec(&restoring).map_err(|e| err("ERR_INSTANCE_RESTORE", e.to_string()))?;
    write_atomic(&restoring_path, &marker)
        .map_err(|e| io_err("ERR_INSTANCE_RESTORE", "Cannot mark the entry", e))?;

    if let Err(e) = std::fs::rename(&source, &dest) {
        let _ = std::fs::remove_file(&restoring_path);
        return Err(io_err(
            "ERR_INSTANCE_RESTORE",
            "Cannot move the files back",
            e,
        ));
    }

    if let Err(error) = finish_restore(&conn, &dest, &stored, &restoring) {
        // Put the files back where they were found, so the entry is whole again.
        if std::fs::rename(&dest, &source).is_ok() {
            let _ = std::fs::remove_file(&restoring_path);
        }
        return Err(error);
    }

    // Committed. Tidy the (now empty) entry; if that fails, listing skips it because its files
    // are gone.
    tidy_entry(&root, entry_id);
    Ok(RestoreOutcome {
        instance_id: new_id,
        original_instance_id: original,
        renamed,
    })
}

fn purge_one(ctx: &Ctx, root: &Path, entry_id: &str, trash: &TrashFn) -> Result<(), InstanceError> {
    // Only something Agora wrote is Agora's to send away: a folder with no valid record is left
    // for whoever put it there.
    let stored = read_entry_file(root, entry_id)?;
    let _lock = try_lock_instance(ctx, &stored.entry.instance_id, "purge")?;
    let dir = entry_dir(root, entry_id)?;
    if !is_plain_dir(&dir) {
        return Err(err(
            "ERR_INSTANCE_DELETE",
            "Entry is not a plain folder; leaving it alone.",
        ));
    }
    let links = detach_links(&dir).map_err(|e| {
        io_err(
            "ERR_INSTANCE_DELETE",
            "Cannot prepare entry for the trash",
            e,
        )
    })?;
    if let Err(error) = trash(&dir) {
        let lost = restore_links(&links);
        if lost > 0 {
            return Err(err(
                "ERR_INSTANCE_DELETE",
                format!("{error} ({lost} link(s) in the entry could not be put back.)"),
            ));
        }
        return Err(err("ERR_INSTANCE_DELETE", error));
    }
    Ok(())
}

/// Send one entry to the OS trash now.
pub fn purge(ctx: &Ctx, entry_id: &str, trash: &TrashFn) -> Result<(), InstanceError> {
    purge_one(
        ctx,
        &ctx.paths.recently_deleted_games_root(),
        entry_id,
        trash,
    )
}

/// Send every entry of one game to the OS trash. Another game's entries are not touched, and
/// neither is anything in the folder that is not an entry Agora wrote. Failures are counted and
/// left in place.
pub fn purge_all(ctx: &Ctx, game_id: &str, trash: &TrashFn) -> PurgeSummary {
    let root = ctx.paths.recently_deleted_games_root();
    let Ok(read) = std::fs::read_dir(&root) else {
        return PurgeSummary::default();
    };
    let mut summary = PurgeSummary::default();
    let mut ids: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    for id in ids {
        let Ok(stored) = read_entry_file(&root, &id) else {
            summary.kept += 1;
            continue;
        };
        if stored.entry.game != game_id {
            continue;
        }
        match purge_one(ctx, &root, &id, trash) {
            Ok(()) => summary.purged += 1,
            Err(_) => summary.failed += 1,
        }
    }
    summary
}

/// Purge entries past the retention window. Failures are counted, never raised: one unreadable
/// entry must not stop the rest from being cleaned up.
pub fn cleanup(ctx: &Ctx, now: chrono::DateTime<chrono::Utc>, trash: &TrashFn) -> PurgeSummary {
    recover_interrupted(ctx);
    let root = ctx.paths.recently_deleted_games_root();
    let days = retention_days(ctx);
    let Ok(read) = std::fs::read_dir(&root) else {
        return PurgeSummary::default();
    };
    // Empty folders hold nothing; `remove_dir` refuses anything else.
    let mut ids: Vec<String> = Vec::new();
    for item in read.filter_map(Result::ok) {
        let path = item.path();
        if is_plain_dir(&path) {
            let _ = std::fs::remove_dir(&path);
        }
        if let Ok(id) = item.file_name().into_string() {
            ids.push(id);
        }
    }
    ids.sort();
    let mut summary = PurgeSummary::default();
    for id in ids {
        let Ok(stored) = read_entry(&root, &id) else {
            continue;
        };
        // An entry whose date cannot be read is kept, never purged on a guess.
        let expired =
            chrono::DateTime::parse_from_rfc3339(&expires_at(&stored.entry.deleted_at, days))
                .map(|at| at <= now)
                .unwrap_or(false);
        if !expired {
            summary.kept += 1;
            continue;
        }
        match purge_one(ctx, &root, &id, trash) {
            Ok(()) => summary.purged += 1,
            Err(_) => summary.failed += 1,
        }
    }
    summary
}
