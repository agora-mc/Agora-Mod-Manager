//! Generic game instances for non-Minecraft games (MASTER_SPEC §26.3, §26.4, §26.12, §26.13).

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;

use agora_game_api::{
    BaseMode, BaseReference, DeploymentStrategy, GameDefinition, GameId, InstalledFramework,
    LayerStack, RuntimeIdentity, StoreId,
};
use serde::{Deserialize, Serialize};

use crate::app_paths::AppPaths;
use crate::ctx::Ctx;
use crate::game_base::{BuildOutcome, BuildProgress};
use crate::game_deploy::DeployMode;
use crate::game_discovery::DiscoveryReport;
use crate::game_launch::{LaunchError, LaunchRoots, Launcher, PreparedLaunch, SystemLauncher};
use crate::game_registry::{IdentifiedInstall, RuntimeResolution};

mod clone;
mod session;
pub mod trash;

pub use clone::{clone_instance, CloneOptions, ClonedInstance};
pub use session::{end_session, stop, watch_session, SessionEnd, StopOutcome};
pub use trash::{DeletedEntry, PurgeSummary, RestoreOutcome, TrashFn};

/// A generic instance manifest stored at `<instances_root>/<id>/instance_manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameInstanceManifest {
    pub manifest_version: u32,
    pub game: GameId,
    pub instance_id: String,
    pub name: String,
    pub runtime_identity: Option<RuntimeIdentity>,
    pub base: BaseReference,
    pub frameworks: Vec<InstalledFramework>,
    pub layers: LayerStack,
    /// The deployment rung the user chose for this instance. `None` leaves the choice to the game
    /// definition and the machine, which may step down (announced); a chosen rung never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<crate::game_deploy::DeployMode>,
    /// Whether the instance's saves are the game's shared folder or its own (MASTER_SPEC §26.5).
    /// Absent in older manifests, which read as `Shared`.
    #[serde(default, skip_serializing_if = "SavesChoice::is_shared")]
    pub saves: SavesChoice,
    /// Where the instance was imported from, when it came from another manager's setup (MASTER_SPEC
    /// §26.10). An import refuses to repeat itself unless it is given a new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<crate::game_import::ImportRecord>,
}

/// Where an instance's saves are: the game's shared save folder, or a folder of its own that the
/// game's own setting redirects to (`SaveLocationRule`). Switching never moves save files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavesChoice {
    #[default]
    Shared,
    Own,
}

impl SavesChoice {
    pub fn is_shared(&self) -> bool {
        matches!(self, SavesChoice::Shared)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SavesChoice::Shared => "shared",
            SavesChoice::Own => "own",
        }
    }
}

impl GameInstanceManifest {
    pub fn new(
        game: GameId,
        instance_id: impl Into<String>,
        name: impl Into<String>,
        runtime_identity: Option<RuntimeIdentity>,
        base: BaseReference,
    ) -> Self {
        Self {
            manifest_version: 3,
            game,
            instance_id: instance_id.into(),
            name: name.into(),
            runtime_identity,
            base,
            frameworks: Vec::new(),
            layers: LayerStack::default(),
            deployment: None,
            saves: SavesChoice::Shared,
            imported_from: None,
        }
    }
}

/// A persistent index record from the `game_instances` database table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameInstanceRecord {
    pub instance_id: String,
    pub game: GameId,
    pub name: String,
    pub base: BaseReference,
    pub created_at: String,
    pub last_launched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_outcome: Option<BuildOutcome>,
}

/// A cross-game summary of an instance (Minecraft or generic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub instance_id: String,
    pub game: GameId,
    pub name: String,
    pub runtime: String,
    pub pinned: Option<bool>,
    pub last_launched_at: Option<String>,
}

/// Outcome of deleting an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOutcome {
    pub instance_id: String,
    pub orphaned_base: Option<String>,
    /// A deployed game folder Agora could not prove was this instance's, left where it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_in_place: Option<PathBuf>,
    /// The Recently deleted entry the instance went to, when the delete was recoverable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_entry_id: Option<String>,
}

/// Options for [`delete`].
#[derive(Debug, Clone, Copy)]
pub struct DeleteOptions {
    /// Delete the instance even when its base record cannot be read. The instance's own folder goes;
    /// a deployed game folder Agora cannot prove is the instance's stays, and is named in the
    /// outcome.
    pub force_unreadable_base: bool,
    /// Keep the instance in Recently deleted instead of removing its folder. On by default: a
    /// delete is a player action and its work should be recoverable. A rollback of a half-created
    /// instance passes false.
    pub recoverable: bool,
}

impl Default for DeleteOptions {
    fn default() -> Self {
        Self {
            force_unreadable_base: false,
            recoverable: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("runtime is unknown or unidentified: {}", .0.join(", "))]
    RuntimeUnidentified(Vec<String>),
    #[error("instance '{0}' already exists")]
    AlreadyExists(String),
    /// A deleted instance's id has been taken again; restoring needs a new name.
    #[error("an instance called '{0}' exists again; choose a new name to restore it")]
    RestoreIdTaken(String),
    /// A recoverable operation refused, with a code the page branches on.
    #[error("{message}")]
    Trash { code: &'static str, message: String },
    #[error("instance '{0}' not found")]
    NotFound(String),
    #[error("instance '{0}' is running; close the game first")]
    Running(String),
    #[error("nothing is running for instance '{0}'")]
    NothingRunning(String),
    /// The session's process was not one Agora started, so it is never killed.
    #[error("the game running for instance '{0}' was not started by Agora; close it yourself")]
    NotStartedByAgora(String),
    #[error("instance '{0}' is a Minecraft instance; use 'agora launch {0}' to launch or 'agora instance delete {0}' to delete")]
    MinecraftInstance(String),
    #[error("invalid instance ID '{0}': {1}")]
    InvalidId(String, String),
    #[error("instance directory '{0}' is invalid: must be directly inside instances root")]
    InvalidInstanceDir(PathBuf),
    #[error("game install '{0}' is no longer installed")]
    InstallNotFound(String),
    #[error("game install directory '{0}' does not exist")]
    InstallDirMissing(PathBuf),
    #[error("pinned base '{0}' not found")]
    BaseNotFound(String),
    #[error(transparent)]
    BaseError(#[from] crate::game_base::BaseError),
    #[error(transparent)]
    LaunchError(#[from] LaunchError),
    /// The virtual file system could not start. Core never decides what happens next: `next` is
    /// the rung a retry would use, for the caller to offer to the user (or take, with
    /// [`VfsFailure::FallBack`]). Nothing has been launched.
    #[error("the virtual file system could not start for instance '{instance_id}': {reason}")]
    VfsUnavailable {
        instance_id: String,
        reason: String,
        next: Option<DeployMode>,
    },
    /// Tools the game says should run before a launch have not, or their output is stale, and the
    /// caller asked to see them instead of answering (MASTER_SPEC §30.12). Nothing was launched.
    #[error("instance '{instance_id}' needs these tools run before launch: {}", .tools.iter().map(|t| format!("{} ({})", t.name, t.reason)).collect::<Vec<_>>().join("; "))]
    BeforeLaunchNeeded {
        instance_id: String,
        tools: Vec<crate::game_tools::BeforeLaunchNeed>,
    },
    /// A before-launch tool ran and failed. Nothing was launched; the caller may launch anyway.
    #[error("{} could not run before launch: {message}", .name)]
    BeforeLaunchFailed {
        tool: String,
        name: String,
        message: String,
    },
    #[error(transparent)]
    Deploy(#[from] crate::game_deploy::DeployError),
    #[error(transparent)]
    UserFiles(#[from] crate::game_user_files::UserFilesError),
    #[error(transparent)]
    Saves(#[from] crate::game_saves::SavesError),
    #[error(transparent)]
    Plugins(#[from] crate::game_plugins::PluginListError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Other(String),
}

/// Generate a valid instance id from a display name:
/// lowercase, `[a-z0-9-]`, runs of `-` collapsed, at most 40 characters,
/// then `-` and 6 random hex characters.
pub fn generate_instance_id_from_name(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            slug.push(lc);
        } else if lc == '-' || !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() {
        "instance"
    } else if slug.len() > 40 {
        slug[..40].trim_end_matches('-')
    } else {
        slug
    };
    let hex = &uuid::Uuid::new_v4().simple().to_string()[..6];
    format!("{slug}-{hex}")
}

fn check_id_exists(
    paths: &AppPaths,
    conn: &rusqlite::Connection,
    candidate: &str,
) -> Result<bool, InstanceError> {
    // Only "no row" means free: a query that fails must not read as "unused".
    for query in [
        "SELECT 1 FROM user_instances WHERE instance_id = ?1",
        "SELECT 1 FROM game_instances WHERE instance_id = ?1",
    ] {
        if conn
            .query_row(query, [candidate], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(true);
        }
    }
    let dir = paths
        .instance_dir(candidate)
        .map_err(|e| InstanceError::InvalidId(candidate.to_string(), e.to_string()))?;
    if dir.exists() {
        return Ok(true);
    }
    Ok(false)
}

/// Create a generic game instance from an identified install.
pub fn create(
    ctx: &Ctx,
    install: &IdentifiedInstall,
    definition: &GameDefinition,
    name: &str,
    id: Option<String>,
    mode: BaseMode,
    progress: &(dyn Fn(BuildProgress) + Send + Sync),
) -> Result<GameInstanceRecord, InstanceError> {
    create_with_options(
        ctx,
        install,
        definition,
        name,
        id,
        mode,
        crate::game_base::BuildOptions::default(),
        progress,
    )
}

/// Create a generic game instance from an identified install with build options.
#[allow(clippy::too_many_arguments)]
pub fn create_with_options(
    ctx: &Ctx,
    install: &IdentifiedInstall,
    definition: &GameDefinition,
    name: &str,
    id: Option<String>,
    mode: BaseMode,
    options: crate::game_base::BuildOptions,
    progress: &(dyn Fn(BuildProgress) + Send + Sync),
) -> Result<GameInstanceRecord, InstanceError> {
    let runtime = match &install.runtime {
        RuntimeResolution::Identified { runtime, .. } => runtime.clone(),
        RuntimeResolution::Unidentified { reasons } => {
            return Err(InstanceError::RuntimeUnidentified(reasons.clone()));
        }
    };

    let instance_name = if name.trim().is_empty() {
        definition.name.as_str()
    } else {
        name.trim()
    };

    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|e| InstanceError::Other(e.to_string()))?;

    let instance_id = if let Some(chosen_id) = id {
        ctx.paths
            .instance_dir(&chosen_id)
            .map_err(|e| InstanceError::InvalidId(chosen_id.clone(), e.to_string()))?;
        if check_id_exists(&ctx.paths, &conn, &chosen_id)? {
            return Err(InstanceError::AlreadyExists(chosen_id));
        }
        chosen_id
    } else {
        let mut candidate = generate_instance_id_from_name(instance_name);
        for _ in 0..100 {
            if !check_id_exists(&ctx.paths, &conn, &candidate)? {
                break;
            }
            candidate = generate_instance_id_from_name(instance_name);
        }
        if check_id_exists(&ctx.paths, &conn, &candidate)? {
            return Err(InstanceError::AlreadyExists(candidate));
        }
        candidate
    };

    // The id is settled before the base is built: a duplicate or invalid id
    // must not cost a full build and hash first.
    let (base_ref, build_outcome) = if install.discovered.capabilities.executables_readable
        && install.discovered.capabilities.relocatable
    {
        let outcome = crate::game_base::build_base(
            &ctx.paths, install, definition, mode, None, options, progress,
        )?;
        let manifest = outcome.manifest();
        let b_ref = BaseReference::Pinned {
            id: manifest.base_id.clone(),
            runtime: manifest.runtime.clone(),
            mode: manifest.mode,
        };
        (b_ref, Some(outcome))
    } else {
        let reason = if !install.discovered.capabilities.executables_readable
            && !install.discovered.capabilities.relocatable
        {
            "executables are not readable and game is not relocatable".to_string()
        } else if !install.discovered.capabilities.executables_readable {
            if install.discovered.store.as_str() == "microsoft-store" {
                "executables are not readable (Microsoft Store)".to_string()
            } else {
                "executables are not readable".to_string()
            }
        } else {
            "game cannot run from outside its store folder".to_string()
        };
        let b_ref = BaseReference::Unpinned {
            install: install.install_id.clone(),
            reason,
        };
        (b_ref, None)
    };

    let instance_dir = ctx
        .paths
        .instance_dir(&instance_id)
        .map_err(|e| InstanceError::InvalidId(instance_id.clone(), e.to_string()))?;

    let manifest = GameInstanceManifest {
        manifest_version: 3,
        game: definition.id.clone(),
        instance_id: instance_id.clone(),
        name: instance_name.to_string(),
        runtime_identity: Some(runtime),
        base: base_ref.clone(),
        frameworks: Vec::new(),
        layers: LayerStack::default(),
        deployment: None,
        saves: SavesChoice::Shared,
        imported_from: None,
    };

    let manifest_path = instance_dir.join("instance_manifest.json");
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let tmp_path = instance_dir.join(format!(
        "instance_manifest.json.{}.tmp",
        uuid::Uuid::new_v4()
    ));

    if let Some(parent) = instance_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Creating the folder is how a create wins its id. A folder that is already there belongs to
    // another create (or an earlier one): this one must not touch it.
    match std::fs::create_dir(&instance_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(InstanceError::AlreadyExists(instance_id));
        }
        Err(e) => return Err(e.into()),
    }

    let write_result: Result<(), InstanceError> = (|| {
        std::fs::write(&tmp_path, &manifest_bytes)?;
        std::fs::rename(&tmp_path, &manifest_path)?;
        let base_json = serde_json::to_string(&base_ref)?;
        conn.execute(
            "INSERT INTO game_instances (instance_id, game, name, base_json, created_at, last_launched_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'), NULL)",
            rusqlite::params![
                &instance_id,
                definition.id.as_str(),
                instance_name,
                &base_json
            ],
        )?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_dir_all(&instance_dir);
        return Err(e);
    }

    let created_at: String = conn.query_row(
        "SELECT created_at FROM game_instances WHERE instance_id = ?1",
        [&instance_id],
        |row| row.get(0),
    )?;

    Ok(GameInstanceRecord {
        instance_id,
        game: definition.id.clone(),
        name: instance_name.to_string(),
        base: base_ref,
        created_at,
        last_launched_at: None,
        build_outcome,
    })
}

/// Retrieve a generic instance record by ID.
pub fn get(ctx: &Ctx, id: &str) -> Result<Option<GameInstanceRecord>, InstanceError> {
    let db_path = ctx.paths.local_state_db();
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let mut stmt = conn.prepare(
        "SELECT instance_id, game, name, base_json, created_at, last_launched_at
         FROM game_instances WHERE instance_id = ?1",
    )?;
    let mut rows = stmt.query([id])?;
    if let Some(row) = rows.next()? {
        let instance_id: String = row.get(0)?;
        let game_str: String = row.get(1)?;
        let name: String = row.get(2)?;
        let base_json: String = row.get(3)?;
        let created_at: String = row.get(4)?;
        let last_launched_at: Option<String> = row.get(5)?;
        let base: BaseReference = serde_json::from_str(&base_json)?;
        let game = GameId::new(&game_str)
            .map_err(|e| InstanceError::Other(format!("invalid game id in db: {e}")))?;
        Ok(Some(GameInstanceRecord {
            instance_id,
            game,
            name,
            base,
            created_at,
            last_launched_at,
            build_outcome: None,
        }))
    } else {
        Ok(None)
    }
}

/// Read the generic instance manifest from disk for an instance ID.
pub fn get_manifest(ctx: &Ctx, id: &str) -> Result<GameInstanceManifest, InstanceError> {
    let manifest_path = ctx
        .paths
        .instance_dir(id)
        .map_err(|e| InstanceError::InvalidId(id.to_string(), e.to_string()))?
        .join("instance_manifest.json");
    if !manifest_path.exists() {
        return Err(InstanceError::NotFound(id.to_string()));
    }
    let content = std::fs::read_to_string(&manifest_path)?;
    let manifest: GameInstanceManifest = serde_json::from_str(&content)?;
    Ok(manifest)
}

/// The launch log an instance's launches append plain-word notes to.
pub fn launch_log_path(ctx: &Ctx, id: &str) -> Result<PathBuf, InstanceError> {
    let dir = ctx
        .paths
        .instance_dir(id)
        .map_err(|e| InstanceError::InvalidId(id.to_string(), e.to_string()))?;
    Ok(dir.join("logs").join("launch.log"))
}

/// Append one line to an instance's launch log.
pub fn append_launch_log(ctx: &Ctx, id: &str, line: &str) -> Result<(), InstanceError> {
    let path = launch_log_path(ctx, id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn write_launch_notes(ctx: &Ctx, id: &str, notes: &[String]) -> Result<(), InstanceError> {
    for note in notes {
        append_launch_log(ctx, id, note)?;
    }
    Ok(())
}

/// List all generic game instances ordered by last launched, then created.
pub fn list(ctx: &Ctx) -> Result<Vec<GameInstanceRecord>, InstanceError> {
    let db_path = ctx.paths.local_state_db();
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let mut stmt = match conn.prepare(
        "SELECT instance_id, game, name, base_json, created_at, last_launched_at
         FROM game_instances
         ORDER BY COALESCE(last_played_at, last_launched_at) DESC NULLS LAST, created_at DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map([], |row| {
        let instance_id: String = row.get(0)?;
        let game_str: String = row.get(1)?;
        let name: String = row.get(2)?;
        let base_json: String = row.get(3)?;
        let created_at: String = row.get(4)?;
        let last_launched_at: Option<String> = row.get(5)?;
        Ok((
            instance_id,
            game_str,
            name,
            base_json,
            created_at,
            last_launched_at,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (instance_id, game_str, name, base_json, created_at, last_launched_at) = row?;
        let base: BaseReference = serde_json::from_str(&base_json)?;
        let game = GameId::new(&game_str)
            .map_err(|e| InstanceError::Other(format!("invalid game id in db: {e}")))?;
        records.push(GameInstanceRecord {
            instance_id,
            game,
            name,
            base,
            created_at,
            last_launched_at,
            build_outcome: None,
        });
    }
    Ok(records)
}

/// Whether an instance ID belongs to a Minecraft instance in `user_instances`.
pub fn is_minecraft_instance(ctx: &Ctx, id: &str) -> bool {
    let db_path = ctx.paths.local_state_db();
    if !db_path.exists() {
        return false;
    }
    let Ok(conn) = crate::db::local_state_connection(&db_path) else {
        return false;
    };
    conn.query_row(
        "SELECT 1 FROM user_instances WHERE instance_id = ?1",
        [id],
        |_| Ok(()),
    )
    .is_ok()
}

/// The store whose user-files session belongs to an instance: the pinned base's runtime, else the
/// identity the manifest kept. Every check for "is this instance running" asks the same question,
/// so a manifest without a runtime identity cannot hide a running game from one of them.
pub(crate) fn session_store(
    base: &BaseReference,
    manifest: &GameInstanceManifest,
) -> Option<agora_game_api::StoreId> {
    match base {
        BaseReference::Pinned { runtime, .. } => Some(runtime.store.clone()),
        BaseReference::Unpinned { .. } => manifest
            .runtime_identity
            .as_ref()
            .map(|runtime| runtime.store.clone()),
    }
}

/// Delete a generic game instance, moving its folder to Recently deleted so it can be restored.
/// Refuses Minecraft instances and folders outside `<instances_root>`.
pub fn delete(ctx: &Ctx, id: &str) -> Result<DeleteOutcome, InstanceError> {
    delete_with_options(ctx, id, DeleteOptions::default())
}

/// [`delete`] with the caller's answer for a base record that cannot be read.
pub fn delete_with_options(
    ctx: &Ctx,
    id: &str,
    options: DeleteOptions,
) -> Result<DeleteOutcome, InstanceError> {
    // A delete changes the instance's records and folders, so it takes the same lock as every other
    // writer. While it is held nothing can deploy or launch the instance out from under it. A tool
    // run holds it for its whole run, so that case fails fast instead of waiting out the timeout.
    let resource = crate::lock_manager::LockResource::Instance(id.to_string());
    if ctx
        .lock_manager
        .current_operation(&resource)
        .is_some_and(|operation| operation.starts_with("tool-"))
    {
        return Err(InstanceError::Other(format!(
            "a tool is running for instance '{id}'; wait for it to finish, then try again"
        )));
    }
    let _lock = ctx
        .lock_manager
        .acquire(resource, "delete")
        .map_err(|e| InstanceError::Other(e.to_string()))?;

    let db_path = ctx.paths.local_state_db();
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;

    // Refuse an id that is in user_instances (Minecraft instance)
    let in_mc = conn
        .query_row(
            "SELECT 1 FROM user_instances WHERE instance_id = ?1",
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if in_mc {
        return Err(InstanceError::MinecraftInstance(id.to_string()));
    }

    // Check if it exists in game_instances
    let base_json: String = match conn.query_row(
        "SELECT base_json FROM game_instances WHERE instance_id = ?1",
        [id],
        |row| row.get(0),
    ) {
        Ok(json) => json,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(InstanceError::NotFound(id.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    let base_ref: BaseReference = serde_json::from_str(&base_json)?;

    let instance_dir = ctx
        .paths
        .instance_dir(id)
        .map_err(|e| InstanceError::InvalidId(id.to_string(), e.to_string()))?;
    if instance_dir.parent() != Some(&ctx.paths.instances_root()) {
        return Err(InstanceError::InvalidInstanceDir(instance_dir));
    }

    // A launch that is swapping or spawning has no game process yet, but it is running: the
    // journal says so. An unreadable journal is not "not running"; it refuses.
    if let Ok(manifest) = get_manifest(ctx, id) {
        if let Some(store) = session_store(&base_ref, &manifest) {
            match crate::game_user_files::status(ctx, &manifest.game, &store) {
                Ok(Some(session)) if session.running && session.instance == id => {
                    return Err(InstanceError::Running(id.to_string()));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(InstanceError::Other(format!(
                        "the user-files session for instance '{id}' cannot be read, so it may be running: {error}"
                    )))
                }
            }
        }
    }

    // The instance's deployed folder is Agora's and is rebuilt on demand, but the game may have
    // written into it. Take it down as a launch end does: harvest those changes first, and refuse
    // (changing nothing) when that cannot run. A game still running from it is refused there.
    let mut base_unreadable = false;
    let deployment_folder = match crate::game_deploy::deployment_dir(ctx, id) {
        Ok(deployed) => deployed.and_then(|game| game.parent().map(std::path::Path::to_path_buf)),
        Err(crate::game_deploy::DeployError::UnreadableBaseManifest(_))
            if options.force_unreadable_base =>
        {
            base_unreadable = true;
            None
        }
        Err(error) => return Err(error.into()),
    };
    // Without the record the farm cannot be checked or harvested. It is left alone, and named.
    let left_in_place = if base_unreadable {
        match find_instance_deployment_folder(&ctx.paths, id)? {
            Some(folder) => Some(move_orphaned_deployment(&folder, id)?),
            None => None,
        }
    } else {
        None
    };
    if instance_dir.join("instance_manifest.json").exists() && !base_unreadable {
        crate::game_deploy::undeploy_locked(ctx, id)?;
    }
    // What is left is the empty folder Agora made for the farm; the instance is going, so it does.
    if let Some(folder) = deployment_folder {
        if folder.is_dir() {
            let _ = std::fs::remove_dir(&folder);
        }
    }

    if options.recoverable {
        // Move the folder into Agora's own holding area before the row goes, so a crash leaves
        // either a restorable entry or a live instance, never a row with no files.
        let row = trash::read_row(&conn, id)?;
        let entry = if instance_dir.exists() {
            let entry = trash::stash(
                &ctx.paths.recently_deleted_games_root(),
                row,
                &instance_dir,
                chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()),
            )?;
            if let Err(error) =
                conn.execute("DELETE FROM game_instances WHERE instance_id = ?1", [id])
            {
                let _ = trash::undo_stash(
                    &ctx.paths.recently_deleted_games_root(),
                    &entry,
                    &instance_dir,
                );
                return Err(error.into());
            }
            Some(entry)
        } else {
            // A row whose folder is already gone has nothing to recover.
            conn.execute("DELETE FROM game_instances WHERE instance_id = ?1", [id])?;
            None
        };
        if let BaseReference::Pinned { id: base_id, .. } = &base_ref {
            let _ = crate::game_tool_copy::remove_deleted_instance_copy(ctx, id, base_id);
        }
        return Ok(DeleteOutcome {
            instance_id: id.to_string(),
            // The base stays while the instance is restorable: suggesting its removal would break
            // a later restore.
            orphaned_base: None,
            left_in_place,
            trash_entry_id: entry.map(|entry| entry.entry_id),
        });
    }

    // The row goes first: a folder left behind by a failed removal is inert,
    // while a row whose folder is gone is a broken instance.
    conn.execute("DELETE FROM game_instances WHERE instance_id = ?1", [id])?;
    // The instance's kept copy of Data goes with it (MASTER_SPEC §26.9). A folder the copy's marker does
    // not name is left in place, and the deletion still succeeds.
    if let BaseReference::Pinned { id: base_id, .. } = &base_ref {
        let _ = crate::game_tool_copy::remove_deleted_instance_copy(ctx, id, base_id);
    }
    if instance_dir.exists() {
        std::fs::remove_dir_all(&instance_dir)?;
    }

    let orphaned_base = if let BaseReference::Pinned { id: base_id, .. } = base_ref {
        // Only suggest removing the base when we can tell nothing pins it.
        let unused = crate::game_base::instances_pinning_base(&ctx.paths, &base_id)
            .map(|pinning| pinning.is_empty())
            .unwrap_or(false);
        if unused {
            Some(base_id)
        } else {
            None
        }
    } else {
        None
    };

    Ok(DeleteOutcome {
        instance_id: id.to_string(),
        orphaned_base,
        left_in_place,
        trash_entry_id: None,
    })
}

/// Remove an instance and its folder for good, without a Recently deleted entry. For rollbacks of
/// something Agora itself just made; a player's delete is recoverable.
pub fn delete_permanently(ctx: &Ctx, id: &str) -> Result<DeleteOutcome, InstanceError> {
    delete_with_options(
        ctx,
        id,
        DeleteOptions {
            force_unreadable_base: false,
            recoverable: false,
        },
    )
}

/// The deployment folder Agora would have made for `id`, searched across the base roots it knows,
/// when the instance's own record cannot say where its base is.
fn find_instance_deployment_folder(
    paths: &AppPaths,
    id: &str,
) -> Result<Option<PathBuf>, InstanceError> {
    Ok(crate::game_base::known_base_roots(paths)?
        .into_iter()
        .map(|root| root.join("deployments").join(id))
        .find(|folder| folder.is_dir()))
}

/// Move a deployment folder Agora cannot prove is the instance's out of the instance-id space, so
/// a later instance created with the same id cannot inherit it. The folder is kept under the base
/// root's `orphaned-deployments`, named with the time it was set aside.
fn move_orphaned_deployment(folder: &Path, id: &str) -> Result<PathBuf, InstanceError> {
    let bases_root = folder
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| {
            InstanceError::Other(format!(
                "cannot tell where to keep the deployed folder '{}'",
                folder.display()
            ))
        })?;
    let orphaned = bases_root.join("orphaned-deployments");
    std::fs::create_dir_all(&orphaned)?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let unique = uuid::Uuid::new_v4().simple();
    let destination = orphaned.join(format!("{id}-{stamp}-{unique}"));
    std::fs::rename(folder, &destination).map_err(|error| {
        InstanceError::Other(format!(
            "the deployed folder '{}' could not be moved aside, so the instance was kept: {error}",
            folder.display()
        ))
    })?;
    Ok(destination)
}

/// Update `last_launched_at` timestamp for a game instance.
pub fn record_launch(ctx: &Ctx, id: &str) -> Result<(), InstanceError> {
    let db_path = ctx.paths.local_state_db();
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let count = conn.execute(
        "UPDATE game_instances SET last_launched_at = datetime('now') WHERE instance_id = ?1",
        [id],
    )?;
    if count == 0 {
        return Err(InstanceError::NotFound(id.to_string()));
    }
    Ok(())
}

/// Give a generic game instance a new name, in the list and in its manifest. The id never changes.
pub fn rename(ctx: &Ctx, id: &str, name: &str) -> Result<(), InstanceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(InstanceError::Other("Give the instance a name.".into()));
    }
    // The manifest is rewritten whole, so this takes the lock every other writer of it takes: a
    // deploy or a delete in flight is waited for rather than overwritten.
    let _lock = ctx
        .lock_manager
        .acquire(
            crate::lock_manager::LockResource::Instance(id.to_string()),
            "rename",
        )
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let db_path = ctx.paths.local_state_db();
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    // The manifest is updated first: a row renamed over an unreadable manifest would be a
    // half-renamed instance. A failed row update puts the manifest back.
    let mut manifest = get_manifest(ctx, id)?;
    let previous = manifest.name.clone();
    manifest.name = name.to_string();
    crate::game_deploy::write_instance_manifest_atomic(ctx, id, &manifest)?;
    let count = conn.execute(
        "UPDATE game_instances SET name = ?1 WHERE instance_id = ?2",
        [name, id],
    )?;
    if count == 0 {
        manifest.name = previous;
        let _ = crate::game_deploy::write_instance_manifest_atomic(ctx, id, &manifest);
        return Err(InstanceError::NotFound(id.to_string()));
    }
    Ok(())
}

/// How long the player has spent in an instance, and when it last ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayStats {
    pub total_play_ms: i64,
    pub last_played_at: Option<String>,
}

/// The instance's recorded play time and last session end.
pub fn play_stats(ctx: &Ctx, id: &str) -> Result<PlayStats, InstanceError> {
    let db_path = ctx.paths.local_state_db();
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    conn.query_row(
        "SELECT total_play_ms, last_played_at FROM game_instances WHERE instance_id = ?1",
        [id],
        |row| {
            Ok(PlayStats {
                total_play_ms: row.get(0)?,
                last_played_at: row.get(1)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| InstanceError::NotFound(id.to_string()))
}

/// Add one session to an instance's play time, ending now. A session that started in the future
/// (a clock jump) adds nothing rather than a negative total.
pub fn record_play_time(
    ctx: &Ctx,
    id: &str,
    started_at: chrono::DateTime<chrono::Utc>,
) -> Result<PlayStats, InstanceError> {
    let now = chrono::DateTime::<chrono::Utc>::from(ctx.clock.now());
    let elapsed = (now - started_at).num_milliseconds().max(0);
    let db_path = ctx.paths.local_state_db();
    let conn = crate::db::local_state_connection(&db_path)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    let count = conn.execute(
        "UPDATE game_instances
            SET total_play_ms = total_play_ms + ?1,
                last_played_at = ?2
          WHERE instance_id = ?3",
        rusqlite::params![elapsed, now.format("%Y-%m-%d %H:%M:%S").to_string(), id],
    )?;
    if count == 0 {
        return Err(InstanceError::NotFound(id.to_string()));
    }
    play_stats(ctx, id)
}

/// What a launch does when the virtual file system (rung 1) cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VfsFailure {
    /// Report it as [`InstanceError::VfsUnavailable`], whether or not the rung was chosen, and
    /// launch nothing: the caller asks the user whether to try the next rung.
    #[default]
    Ask,
    /// The user said yes in advance: step down to the next rung and announce it in the launch's
    /// `notice`, even for a rung they chose.
    FallBack,
}

/// What to do with a tool that should run before the launch (MASTER_SPEC §30.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeforeLaunchAnswer {
    /// Run it before the launch.
    Run,
    /// Leave it and launch anyway. The launch log records the choice.
    LaunchAnyway,
}

/// How a caller answers the tools a launch needs (MASTER_SPEC §30.12). Every answer still lets the
/// launch happen: a protection always has an override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BeforeLaunchChoice {
    /// The non-interactive default: run a `run`-mode tool, and leave an `ask`-mode one unrun
    /// (never run unasked).
    #[default]
    Run,
    /// Launch anyway for every needed tool (the CLI's `--skip-before-launch`).
    Skip,
    /// Do not answer here: return [`InstanceError::BeforeLaunchNeeded`] so the caller can ask.
    Ask,
    /// The player's answer per tool id.
    Answers(std::collections::BTreeMap<String, BeforeLaunchAnswer>),
}

/// Choices for one launch.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// Launch even if the base fails verification.
    pub launch_anyway: bool,
    /// Skip the game's launch alternatives (a framework's loader, for example) and start the
    /// recipe's own executable.
    pub plain: bool,
    /// Use this deployment rung for this launch only, instead of the instance's own choice.
    pub deployment: Option<DeployMode>,
    /// What to do if the virtual file system cannot start. Asks by default.
    pub on_vfs_failure: VfsFailure,
    /// How the tools a launch needs are answered (MASTER_SPEC §30.12).
    pub before_launch: BeforeLaunchChoice,
}

/// The rung a game definition starts from when nobody has chosen one (MASTER_SPEC §26.5).
///
/// Both `VirtualFileSystem` and `Redirect` default to `DeployMode::Virtual` (there is no
/// Redirect backend yet; until there is, the VFS is the rung that never refuses a write).
fn default_deploy_mode(definition: &GameDefinition) -> DeployMode {
    match definition.deployment {
        DeploymentStrategy::VirtualFileSystem | DeploymentStrategy::Redirect => DeployMode::Virtual,
    }
}

/// The deployment mode whose visible files a launch of this instance would judge, as launch
/// picks it: the instance's own choice or the game's default when it deploys, and the mode the
/// instance would use if it is not deployed at all (the same files, with nothing on top).
pub(crate) fn visible_mode(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
) -> Result<DeployMode, InstanceError> {
    let manifest = get_manifest(ctx, id)?;
    Ok(
        deploys_on_launch(&manifest, definition, manifest.deployment)
            .unwrap_or_else(|| default_deploy_mode(definition)),
    )
}

fn vfs_fallback_notice(reason: &str, next: DeployMode) -> String {
    format!(
        "the virtual file system could not start: {reason}; running from {} instead",
        next.plain_name()
    )
}

/// The rung `FallBack` steps down to, with the notice that says so.
struct StepDown {
    mode: DeployMode,
    notice: String,
}

/// The virtual file system cannot start: with `Ask`, the error to report; with `FallBack`, the
/// rung to run from instead.
fn step_down(id: &str, options: &LaunchOptions, reason: String) -> Result<StepDown, InstanceError> {
    let next = DeployMode::Virtual.next_fallback();
    match (options.on_vfs_failure, next) {
        (VfsFailure::FallBack, Some(mode)) => Ok(StepDown {
            mode,
            notice: vfs_fallback_notice(&reason, mode),
        }),
        _ => Err(InstanceError::VfsUnavailable {
            instance_id: id.to_string(),
            reason,
            next,
        }),
    }
}

/// How an instance with deployed content will be run.
struct Rung {
    mode: DeployMode,
    /// The user chose it (it is not the game's default).
    chosen: bool,
    /// `agora_vfs.dll`, when `mode` is `Virtual`.
    vfs_dll: Option<PathBuf>,
    notice: Option<String>,
}

/// The mode `agora games instance deploy` builds: `requested`, else the instance's own choice,
/// else what a launch would run (the game's default, stepping down to links when the virtual
/// file system's DLL cannot be found).
pub fn deploy_mode_for(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    requested: Option<DeployMode>,
    launcher: &dyn Launcher,
) -> Result<DeployMode, InstanceError> {
    if let Some(mode) = requested.or(get_manifest(ctx, id)?.deployment) {
        return Ok(mode);
    }
    Ok(match default_deploy_mode(definition) {
        DeployMode::Virtual if launcher.locate_vfs_dll().is_err() => DeployMode::Links,
        mode => mode,
    })
}

/// The rung a launch of this pinned instance deploys on, or `None` when it runs from its base with
/// nothing deployed. `chosen` is the launch's own override, else the instance's choice.
///
/// A game that needs the virtual file system runs under it even with nothing deployed on top of
/// its base: on a linked base only the VFS keeps its writes out of the store install. Other games
/// with no content run from their base, unless the user chose a rung.
fn deploys_on_launch(
    manifest: &GameInstanceManifest,
    definition: &GameDefinition,
    chosen: Option<DeployMode>,
) -> Option<DeployMode> {
    let has_deployment_layers = manifest.layers.iter().any(|l| {
        (matches!(l.source, agora_game_api::LayerSource::Content { .. }) && l.enabled)
            || (matches!(l.source, agora_game_api::LayerSource::Generated { .. }) && l.enabled)
            || matches!(l.source, agora_game_api::LayerSource::Writable { .. })
    });
    if has_deployment_layers
        || chosen.is_some()
        || default_deploy_mode(definition) == DeployMode::Virtual
    {
        Some(chosen.unwrap_or_else(|| default_deploy_mode(definition)))
    } else {
        None
    }
}

/// The runtime-file findings a launch of this instance would refuse or carry, computed the way
/// launch computes them, without deploying or starting anything.
///
/// An unpinned instance runs from its install folder with nothing to verify, so it has no
/// findings here, the same as at launch.
pub fn runtime_findings(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
) -> Result<Vec<agora_game_api::RuntimeFileFinding>, InstanceError> {
    Ok(runtime_check(ctx, id, definition)?.0)
}

/// The framework findings and passes for a launch of this instance, from one reading of the files
/// the game will see. An unpinned instance has neither.
fn runtime_check(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
) -> Result<
    (
        Vec<agora_game_api::RuntimeFileFinding>,
        Vec<agora_game_api::RuntimeFilePass>,
    ),
    InstanceError,
> {
    let record = get(ctx, id)?.ok_or_else(|| InstanceError::NotFound(id.to_string()))?;
    let BaseReference::Pinned { id: base_id, .. } = record.base else {
        return Ok((Vec::new(), Vec::new()));
    };
    let manifest = get_manifest(ctx, id)?;
    let manifest_path = ctx.paths.base_manifest_path(&base_id);
    if !manifest_path.exists() {
        return Err(InstanceError::BaseNotFound(base_id));
    }
    let content = std::fs::read_to_string(&manifest_path)?;
    let base_manifest: crate::game_base::BaseManifest = serde_json::from_str(&content)?;
    let paths: Vec<String> = match deploys_on_launch(&manifest, definition, manifest.deployment) {
        Some(mode) => crate::game_deploy::visible_paths(ctx, id, definition, mode)?,
        None => base_manifest.files.iter().map(|f| f.path.clone()).collect(),
    };
    Ok((
        crate::game_launch::runtime_findings(definition, &base_manifest.runtime.version, &paths),
        crate::game_launch::runtime_passes(definition, &base_manifest.runtime.version, &paths),
    ))
}

/// A check that ran and passed, in words a page can show. Only checks that actually examined
/// something are here: an empty list means no check applied, never that everything passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PassedCheck {
    /// `framework`, `load_order`, `tool` or `base`, the same values as a finding's kind.
    pub kind: &'static str,
    pub title: String,
    pub detail: String,
}

/// The findings `games instance check` and the instance Overview report, from one place so they
/// cannot disagree. An unpinned instance runs from its install folder and has none.
pub struct LaunchChecks {
    pub runtime: Vec<agora_game_api::RuntimeFileFinding>,
    pub load_order: Vec<crate::game_load_order::Finding>,
    pub outputs: Vec<crate::game_tools::OutputFinding>,
    /// Frameworks the instance's catalog mods need and it does not satisfy. A warning, never a
    /// refusal: the user may have installed the mod past exactly this on purpose.
    pub requirements: Vec<crate::catalog_install::RequirementFinding>,
    /// The checks that ran and passed, for the page to list beside any findings.
    pub passed: Vec<PassedCheck>,
}

/// Which check failed, so each caller keeps the error code it gives that check.
#[derive(Debug, thiserror::Error)]
pub enum LaunchChecksError {
    #[error(transparent)]
    Runtime(#[from] InstanceError),
    #[error(transparent)]
    LoadOrder(#[from] crate::game_load_order::LoadOrderError),
    #[error(transparent)]
    Outputs(#[from] crate::game_tools::ToolError),
}

/// The launch checks of an instance: its framework files, its plugin load order and its tool
/// outputs. A pinned instance's load order and outputs both judge the instance's Links plan, which
/// is planned here once, not once per check and once per generated layer. The framework check's
/// own plan and the Links plan are independent reads, so they are made side by side. The errors
/// keep their order: a framework error is reported before a load-order error.
pub fn launch_checks(
    ctx: &Ctx,
    record: &GameInstanceRecord,
    definition: &GameDefinition,
) -> Result<LaunchChecks, LaunchChecksError> {
    let id = record.instance_id.as_str();
    let pinned = matches!(record.base, BaseReference::Pinned { .. });
    let (runtime, links) = std::thread::scope(|scope| {
        let runtime = scope.spawn(|| runtime_check(ctx, id, definition));
        let links = scope.spawn(|| {
            if pinned {
                crate::game_load_order::plan_for_instance(ctx, id, definition)
            } else {
                Ok(None)
            }
        });
        (join_scoped(runtime), join_scoped(links))
    });
    let (runtime, framework_passes) = runtime?;
    if !pinned {
        return Ok(LaunchChecks {
            runtime,
            load_order: Vec::new(),
            outputs: Vec::new(),
            requirements: Vec::new(),
            passed: Vec::new(),
        });
    }
    let links = links?;
    let orders = crate::game_load_order::orders_with_view(ctx, id, definition, links.as_ref())?;
    let plugin_count: Option<usize> = if definition.load_orders.is_empty() {
        None
    } else {
        Some(orders.iter().map(|order| order.entries.len()).sum())
    };
    let load_order: Vec<crate::game_load_order::Finding> = orders
        .into_iter()
        .flat_map(|order| order.findings)
        .collect();
    let outputs =
        crate::game_tools::output_findings_with_view(ctx, id, definition, links.as_ref())?;
    let requirements = crate::catalog_install::requirement_findings(ctx, id, definition);
    let mut passed = Vec::new();
    for pass in &framework_passes {
        passed.push(PassedCheck {
            kind: "framework",
            title: pass.rule_name.clone(),
            detail: format!("Found {}, which matches this game version.", pass.expected),
        });
    }
    if let Some(count) = plugin_count {
        if load_order.is_empty() {
            passed.push(PassedCheck {
                kind: "load_order",
                title: "Plugin load order".to_string(),
                detail: format!(
                    "{count} {} in a valid order.",
                    if count == 1 { "plugin" } else { "plugins" }
                ),
            });
        }
    }
    Ok(LaunchChecks {
        runtime,
        load_order,
        outputs,
        requirements,
        passed,
    })
}

/// The result of a scoped thread. A panic in the thread is raised again here, not turned into a
/// value.
fn join_scoped<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Answer the tools a launch needs (MASTER_SPEC §30.12): run them through the normal tool pipeline,
/// or record that the player chose to launch anyway. A failed run is reported, not swallowed, and
/// the caller may launch anyway with a later answer.
fn answer_before_launch(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    options: &LaunchOptions,
    launcher: &dyn Launcher,
) -> Result<(), InstanceError> {
    let declares_before_launch = ctx
        .games
        .package_for(&definition.id)
        .is_some_and(|package| {
            package
                .definition()
                .tools
                .iter()
                .any(|tool| tool.game == definition.id && tool.before_launch.is_some())
        });
    if !declares_before_launch {
        return Ok(());
    }
    let facts = crate::game_conditions::InstanceFacts::for_instance(ctx, id, definition);
    let mut notes = Vec::new();
    let needs = crate::game_tools::before_launch_needs(ctx, id, definition, &facts, &mut notes)
        .map_err(|e| InstanceError::Other(e.to_string()))?;
    write_launch_notes(ctx, id, &notes)?;
    if needs.is_empty() {
        return Ok(());
    }
    if matches!(options.before_launch, BeforeLaunchChoice::Ask) {
        return Err(InstanceError::BeforeLaunchNeeded {
            instance_id: id.to_string(),
            tools: needs,
        });
    }
    for need in &needs {
        let answer = match &options.before_launch {
            BeforeLaunchChoice::Skip => BeforeLaunchAnswer::LaunchAnyway,
            BeforeLaunchChoice::Run => {
                if need.mode == agora_game_api::BeforeLaunchMode::Run {
                    BeforeLaunchAnswer::Run
                } else {
                    BeforeLaunchAnswer::LaunchAnyway
                }
            }
            BeforeLaunchChoice::Answers(answers) => {
                answers.get(&need.tool).copied().unwrap_or_else(|| {
                    if need.mode == agora_game_api::BeforeLaunchMode::Run {
                        BeforeLaunchAnswer::Run
                    } else {
                        BeforeLaunchAnswer::LaunchAnyway
                    }
                })
            }
            BeforeLaunchChoice::Ask => unreachable!("handled above"),
        };
        match answer {
            BeforeLaunchAnswer::LaunchAnyway => append_launch_log(
                ctx,
                id,
                &format!(
                    "Before launch: {} was not run ({}); the player chose to launch anyway.",
                    need.name, need.reason
                ),
            )?,
            BeforeLaunchAnswer::Run => {
                let tool_id = agora_game_api::ToolId::new(need.tool.clone())
                    .map_err(|e| InstanceError::Other(e.to_string()))?;
                let cancel = crate::event_sink::CancellationToken::new();
                match crate::game_tools::run(
                    ctx,
                    id,
                    definition,
                    &tool_id,
                    crate::game_tools::CaptureMode::Auto,
                    false,
                    launcher,
                    &cancel,
                ) {
                    Ok(outcome) if outcome.promoted => {
                        append_launch_log(ctx, id, &format!("Before launch: ran {}.", need.name))?
                    }
                    Ok(outcome) => {
                        return Err(InstanceError::BeforeLaunchFailed {
                            tool: need.tool.clone(),
                            name: need.name.clone(),
                            message: format!(
                                "its run was discarded (exit code {})",
                                outcome
                                    .exit_code
                                    .map(|code| code.to_string())
                                    .unwrap_or_else(|| "unknown".into())
                            ),
                        })
                    }
                    Err(error) => {
                        return Err(InstanceError::BeforeLaunchFailed {
                            tool: need.tool.clone(),
                            name: need.name.clone(),
                            message: error.to_string(),
                        })
                    }
                }
            }
        }
    }
    Ok(())
}

/// Prepare launch for a generic game instance.
pub fn prepare_launch(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    launch_anyway: bool,
) -> Result<PreparedLaunch, InstanceError> {
    prepare_launch_with_discovery(ctx, id, definition, launch_anyway, &|| {
        crate::game_discovery::discover_with_folders(&ctx.paths)
    })
}

/// Prepare launch with a custom discovery function (used for testing unpinned launches).
pub fn prepare_launch_with_discovery(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    launch_anyway: bool,
    discover_fn: &dyn Fn() -> DiscoveryReport,
) -> Result<PreparedLaunch, InstanceError> {
    prepare_launch_with(
        ctx,
        id,
        definition,
        LaunchOptions {
            launch_anyway,
            plain: false,
            deployment: None,
            on_vfs_failure: VfsFailure::default(),
            before_launch: BeforeLaunchChoice::default(),
        },
        discover_fn,
        &SystemLauncher,
    )
}

/// Prepare launch, choosing the deployment rung (MASTER_SPEC §26.5).
///
/// The rung is the launch's override, else the instance's own choice, else the game
/// definition's default. If the virtual file system cannot start, `options.on_vfs_failure`
/// decides: `Ask` returns [`InstanceError::VfsUnavailable`] with the next rung, `FallBack` steps
/// down to it and says so in the result's `notice`.
pub fn prepare_launch_with(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    options: LaunchOptions,
    discover_fn: &dyn Fn() -> DiscoveryReport,
    launcher: &dyn Launcher,
) -> Result<PreparedLaunch, InstanceError> {
    let record = get(ctx, id)?.ok_or_else(|| InstanceError::NotFound(id.to_string()))?;
    let manifest = get_manifest(ctx, id)?;
    if matches!(record.base, BaseReference::Pinned { .. }) {
        answer_before_launch(ctx, id, definition, &options, launcher)?;
    }
    match record.base {
        BaseReference::Pinned { id: base_id, .. } => {
            let chosen = options.deployment.or(manifest.deployment);

            if let Some(wanted) = deploys_on_launch(&manifest, definition, chosen) {
                let rung = match wanted {
                    DeployMode::Virtual => match launcher.locate_vfs_dll() {
                        Ok(dll) => Rung {
                            mode: wanted,
                            chosen: chosen.is_some(),
                            vfs_dll: Some(dll),
                            notice: None,
                        },
                        Err(reason) => {
                            let next = step_down(id, &options, reason)?;
                            Rung {
                                mode: next.mode,
                                chosen: chosen.is_some(),
                                vfs_dll: None,
                                notice: Some(next.notice),
                            }
                        }
                    },
                    mode => Rung {
                        mode,
                        chosen: chosen.is_some(),
                        vfs_dll: None,
                        notice: None,
                    },
                };
                prepare_deployed(ctx, id, definition, &options, &base_id, rung)
            } else {
                let manifest_path = ctx.paths.base_manifest_path(&base_id);
                if !manifest_path.exists() {
                    return Err(InstanceError::BaseNotFound(base_id));
                }
                let content = std::fs::read_to_string(&manifest_path)?;
                let base_manifest: crate::game_base::BaseManifest = serde_json::from_str(&content)?;
                // Layers removed since the last deploy take their plugin lines with them.
                crate::game_plugins::sync_without_content(
                    ctx,
                    id,
                    definition,
                    &base_manifest.runtime.store,
                )?;
                let Some(recipe) = &definition.launch else {
                    return Err(LaunchError::NoRecipe.into());
                };
                let mut notes = Vec::new();
                let prepared = if crate::game_launch::needs_instance_facts(
                    definition,
                    recipe,
                    options.plain,
                ) {
                    let facts =
                        crate::game_conditions::InstanceFacts::for_instance(ctx, id, definition);
                    crate::game_launch::prepare_base_launch_with_facts(
                        &base_manifest,
                        definition,
                        options.launch_anyway,
                        options.plain,
                        &facts,
                        &mut notes,
                    )?
                } else {
                    crate::game_launch::prepare_base_launch_with(
                        &base_manifest,
                        definition,
                        options.launch_anyway,
                        options.plain,
                    )?
                };
                write_launch_notes(ctx, id, &notes)?;
                Ok(prepared)
            }
        }
        BaseReference::Unpinned { install, .. } => {
            let has_content = manifest
                .layers
                .iter()
                .any(|l| matches!(l.source, agora_game_api::LayerSource::Content { .. }));
            if has_content {
                return Err(
                    crate::game_deploy::DeployError::UnpinnedInstance(id.to_string()).into(),
                );
            }
            let report = discover_fn();
            let matching = report.installs.iter().find(|discovered| {
                crate::game_registry::make_install_id(&discovered.store, &discovered.product)
                    == install
            });
            let Some(discovered) = matching else {
                return Err(InstanceError::InstallNotFound(install.to_string()));
            };
            if !discovered.location.is_dir() {
                return Err(InstanceError::InstallDirMissing(
                    discovered.location.clone(),
                ));
            }
            let roots = LaunchRoots {
                runtime: discovered.location.clone(),
                install: Some(discovered.location.clone()),
                base: None,
            };
            let Some(recipe) = &definition.launch else {
                return Err(LaunchError::NoRecipe.into());
            };
            let mut notes = Vec::new();
            let (mut resolved, alternative) =
                if crate::game_launch::needs_instance_facts(definition, recipe, options.plain) {
                    let facts =
                        crate::game_conditions::InstanceFacts::for_instance(ctx, id, definition);
                    let (mut resolved, recipe_notes) =
                        crate::game_launch::resolve_recipe_with_facts(recipe, &roots, &facts)?;
                    notes.extend(recipe_notes);
                    let alternative = if options.plain {
                        None
                    } else {
                        crate::game_launch::apply_launch_alternative_with_facts(
                            definition,
                            &roots,
                            &mut resolved,
                            &facts,
                            &mut notes,
                        )?
                    };
                    (resolved, alternative)
                } else {
                    let mut resolved = crate::game_launch::resolve_recipe(recipe, &roots)?;
                    let alternative = if options.plain {
                        None
                    } else {
                        crate::game_launch::apply_launch_alternative(
                            definition,
                            &roots,
                            &mut resolved,
                        )?
                    };
                    (resolved, alternative)
                };
            write_launch_notes(ctx, id, &notes)?;
            if discovered.store.as_str() == "steam" {
                let product = definition
                    .stores
                    .iter()
                    .find(|s| s.store == discovered.store)
                    .map(|s| s.product.as_str())
                    .unwrap_or(discovered.product.as_str());
                resolved
                    .env
                    .insert("SteamAppId".to_string(), std::ffi::OsString::from(product));
                resolved
                    .env
                    .insert("SteamGameId".to_string(), std::ffi::OsString::from(product));
            }
            Ok(PreparedLaunch::undeployed(
                resolved,
                Vec::new(),
                alternative,
            ))
        }
    }
}

/// Deploy a pinned instance on `rung` and resolve its launch from the deployed folder.
fn prepare_deployed(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    options: &LaunchOptions,
    base_id: &str,
    rung: Rung,
) -> Result<PreparedLaunch, InstanceError> {
    let manifest_path = ctx.paths.base_manifest_path(base_id);
    if !manifest_path.exists() {
        return Err(InstanceError::BaseNotFound(base_id.to_string()));
    }
    let content = std::fs::read_to_string(&manifest_path)?;
    let base_manifest: crate::game_base::BaseManifest = serde_json::from_str(&content)?;

    // The launch plans the deployment once. The runtime-file check reads that plan before anything
    // is built, so a refused launch leaves nothing newly built, and the build uses the same plan.
    let mut findings = Vec::new();
    let (outcome, deployed) =
        crate::game_deploy::deploy_checked(ctx, id, definition, rung.mode, |plan| {
            let visible = crate::game_deploy::visible_paths_of(ctx, id, plan)?;
            findings = crate::game_launch::refuse_runtime_mismatch(
                crate::game_launch::runtime_findings(
                    definition,
                    &base_manifest.runtime.version,
                    &visible,
                ),
                options.launch_anyway,
            )?;
            Ok::<(), InstanceError>(())
        })??;

    // The load order reads the Links view of the deployment (the deployed plan itself when it is
    // linked) when the game declares the Creation Engine rules: the only load-order check that
    // plans. Otherwise it needs no plan, and neither do the generated outputs.
    let links_plan;
    let needs_links_view = crate::game_load_order::any_needs_plan(definition);
    let links: Option<&crate::game_deploy::DeploymentPlan> = if !needs_links_view {
        None
    } else if rung.mode == crate::game_deploy::DeployMode::Links {
        Some(&deployed)
    } else {
        links_plan =
            crate::game_deploy::plan(ctx, id, definition, crate::game_deploy::DeployMode::Links)?;
        Some(&links_plan)
    };
    // The deploy has synced the plugin list, so this is the order the game will read. Nothing has
    // been swapped in yet: a refused launch leaves the user's files as they were.
    let load_order_findings = check_load_order(ctx, id, definition, options.launch_anyway, links)?;
    // Stale or unknown tool output is a warning: the launch goes on (MASTER_SPEC §26.9).
    let generated_findings =
        crate::game_tools::output_findings_with_view(ctx, id, definition, links)
            .map_err(|e| InstanceError::Other(e.to_string()))?;

    let game_dir = crate::game_deploy::deployment_dir(ctx, id)?
        .ok_or_else(|| InstanceError::Other("deployed game directory not found".into()))?;

    let ver = crate::game_base::verify_base(
        &base_manifest,
        crate::game_base::VerifyDepth::Quick,
        &|p| definition.is_declared_write(p),
        &|p| definition.is_excluded(p),
    );
    if !ver.problems.is_empty() && !options.launch_anyway {
        return Err(crate::game_launch::LaunchError::BaseDamaged {
            problems: ver.problems,
        }
        .into());
    }
    let warnings = ver.problems;

    let roots = LaunchRoots {
        runtime: game_dir.clone(),
        install: Some(base_manifest.source_location.clone()),
        base: Some(base_manifest.location.clone()),
    };
    let Some(recipe) = &definition.launch else {
        return Err(LaunchError::NoRecipe.into());
    };
    let mut notes = Vec::new();
    let (mut resolved, alternative) =
        if crate::game_launch::needs_instance_facts(definition, recipe, options.plain) {
            let facts = crate::game_conditions::InstanceFacts::for_instance(ctx, id, definition);
            let (mut resolved, recipe_notes) =
                crate::game_launch::resolve_recipe_with_facts(recipe, &roots, &facts)?;
            notes.extend(recipe_notes);
            let alternative = if options.plain {
                None
            } else {
                crate::game_launch::apply_launch_alternative_with_facts(
                    definition,
                    &roots,
                    &mut resolved,
                    &facts,
                    &mut notes,
                )?
            };
            (resolved, alternative)
        } else {
            let mut resolved = crate::game_launch::resolve_recipe(recipe, &roots)?;
            let alternative = if options.plain {
                None
            } else {
                crate::game_launch::apply_launch_alternative(definition, &roots, &mut resolved)?
            };
            (resolved, alternative)
        };
    write_launch_notes(ctx, id, &notes)?;
    if base_manifest.runtime.store.as_str() == "steam" {
        let product = base_manifest.source_product.as_deref().or_else(|| {
            definition
                .stores
                .iter()
                .find(|s| s.store == base_manifest.runtime.store)
                .map(|s| s.product.as_str())
        });
        if let Some(prod) = product {
            resolved
                .env
                .insert("SteamAppId".to_string(), std::ffi::OsString::from(prod));
            resolved
                .env
                .insert("SteamGameId".to_string(), std::ffi::OsString::from(prod));
        }
    }

    // Under the VFS the farm is mounted over itself and the writable layer is the upper layer.
    let vfs = match rung.vfs_dll {
        Some(dll) => {
            let instance_dir = ctx
                .paths
                .instance_dir(id)
                .map_err(|e| InstanceError::Other(e.to_string()))?;
            Some(crate::game_launch::VfsLaunch {
                dll_x86: Some(crate::game_launch::locate_dll_x86(&dll)),
                dll,
                mount: game_dir.clone(),
                upper: instance_dir.join("writable"),
                lowers: vec![game_dir],
                // A game launch shows no alias: its view is the instance's own folder.
                aliases: Vec::new(),
                config_path: instance_dir.join("vfs").join("config.json"),
                log: instance_dir.join("logs").join("vfs.log"),
            })
        }
        None => None,
    };

    Ok(PreparedLaunch {
        resolved,
        warnings,
        deploy_outcome: Some(outcome),
        deployment: Some(rung.mode),
        deployment_chosen: rung.chosen,
        vfs,
        notice: rung.notice,
        alternative,
        runtime_findings: findings,
        load_order_findings,
        generated_findings,
    })
}

/// The load order findings a launch carries, after the plugin list is synced. A refusing finding
/// refuses with [`LaunchError::LoadOrderProblems`] unless `launch_anyway`. A game with no plugin
/// list has no load order to check.
fn check_load_order(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    launch_anyway: bool,
    links: Option<&crate::game_deploy::DeploymentPlan>,
) -> Result<Vec<crate::game_load_order::Finding>, InstanceError> {
    if definition.load_orders.is_empty() {
        return Ok(Vec::new());
    }
    let findings = crate::game_load_order::check_all_with_view(ctx, id, definition, links)
        .map_err(|e| InstanceError::Other(format!("cannot check the plugin load order: {e}")))?;
    Ok(crate::game_launch::refuse_load_order(
        findings,
        launch_anyway,
    )?)
}

/// Start a prepared launch. When it runs under the virtual file system and that cannot start,
/// `options.on_vfs_failure` decides: `Ask` returns [`InstanceError::VfsUnavailable`] (nothing is
/// running; the caller restores whatever it swapped in and may ask the user), `FallBack` deploys
/// the next rung and launches from it, and the returned launch says so.
pub fn spawn_prepared(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    prepared: PreparedLaunch,
    options: LaunchOptions,
    launcher: &dyn Launcher,
) -> Result<(crate::game_launch::LaunchedGame, PreparedLaunch), InstanceError> {
    match launcher.launch(&prepared) {
        Ok(launched) => Ok((launched, prepared)),
        Err(LaunchError::VfsUnavailable { reason }) => {
            let next = step_down(id, &options, reason.clone())?;
            let record = get(ctx, id)?.ok_or_else(|| InstanceError::NotFound(id.to_string()))?;
            let BaseReference::Pinned { id: base_id, .. } = record.base else {
                return Err(LaunchError::VfsUnavailable { reason }.into());
            };
            let fallback = prepare_deployed(
                ctx,
                id,
                definition,
                &options,
                &base_id,
                Rung {
                    mode: next.mode,
                    chosen: false,
                    vfs_dll: None,
                    notice: Some(next.notice),
                },
            )?;
            let launched = launcher.launch(&fallback)?;
            Ok((launched, fallback))
        }
        Err(e) => Err(e.into()),
    }
}

/// A spawned game process with its prepared launch and swap context.
pub struct LaunchedInstance {
    pub launched: crate::game_launch::LaunchedGame,
    pub prepared: PreparedLaunch,
    pub store: StoreId,
    pub running_from: PathBuf,
    /// The session ledger row this launch opened (`session_ledger`), when one could be written.
    /// Pass it to [`session::watch_session`] so the end of the session is recorded against it.
    pub session_id: Option<String>,
}

/// Launch a generic game instance: deploy -> swap_in -> spawn.
/// If spawning fails, restores user files immediately.
pub fn launch(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    launch_anyway: bool,
) -> Result<LaunchedInstance, InstanceError> {
    launch_with_discovery(ctx, id, definition, launch_anyway, &|| {
        crate::game_discovery::discover_with_folders(&ctx.paths)
    })
}

/// Launch with a custom discovery function.
pub fn launch_with_discovery(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    launch_anyway: bool,
    discover_fn: &dyn Fn() -> DiscoveryReport,
) -> Result<LaunchedInstance, InstanceError> {
    launch_with(
        ctx,
        id,
        definition,
        LaunchOptions {
            launch_anyway,
            plain: false,
            deployment: None,
            on_vfs_failure: VfsFailure::default(),
            before_launch: BeforeLaunchChoice::default(),
        },
        discover_fn,
        &SystemLauncher,
    )
}

/// Launch with explicit options, discovery and launcher.
pub fn launch_with(
    ctx: &Ctx,
    id: &str,
    definition: &GameDefinition,
    options: LaunchOptions,
    discover_fn: &dyn Fn() -> DiscoveryReport,
    launcher: &dyn Launcher,
) -> Result<LaunchedInstance, InstanceError> {
    let record = get(ctx, id)?.ok_or_else(|| InstanceError::NotFound(id.to_string()))?;

    // An interrupted tool swap is put back before the launch deploys or swaps anything (MASTER_SPEC §26.9).
    crate::game_tool_swap::recover_game(ctx, definition)
        .map_err(|e| InstanceError::Other(e.to_string()))?;

    // 1. Prepare launch (which performs deployment if needed)
    let prepared =
        prepare_launch_with(ctx, id, definition, options.clone(), discover_fn, launcher)?;

    // 2. Determine store and running_from
    let (store, running_from) = match &record.base {
        BaseReference::Pinned { id: base_id, .. } => {
            let manifest_path = ctx.paths.base_manifest_path(base_id);
            if !manifest_path.exists() {
                return Err(InstanceError::BaseNotFound(base_id.clone()));
            }
            let text = std::fs::read_to_string(&manifest_path)?;
            let base_manifest: crate::game_base::BaseManifest = serde_json::from_str(&text)?;
            let store = base_manifest.runtime.store.clone();
            let running_from = if prepared.deploy_outcome.is_some() {
                crate::game_deploy::deployment_dir(ctx, id)?
                    .unwrap_or_else(|| base_manifest.location.clone())
            } else {
                base_manifest.location.clone()
            };
            (store, running_from)
        }
        BaseReference::Unpinned { install, .. } => {
            let report = discover_fn();
            let matching = report.installs.iter().find(|discovered| {
                crate::game_registry::make_install_id(&discovered.store, &discovered.product)
                    == *install
            });
            let Some(discovered) = matching else {
                return Err(InstanceError::InstallNotFound(install.to_string()));
            };
            (discovered.store.clone(), discovered.location.clone())
        }
    };

    // 3. Swap in user files
    crate::game_user_files::swap_in(ctx, id, definition, &store, &running_from)?;

    // 4. Spawn game process
    let (launched, prepared) = match spawn_prepared(
        ctx, id, definition, prepared, options, launcher,
    ) {
        Ok(l) => l,
        Err(e) => {
            // "If spawning fails, restore immediately." The launch never reported a game, so
            // its starting mark is cleared first. A restore that also fails is reported rather
            // than swallowed: the journal stays, and the next launch or a restore finishes it.
            let put_back = crate::game_user_files::finish_starting(ctx, &definition.id, &store)
                .and_then(|()| crate::game_user_files::restore(ctx, definition, &store));
            return Err(match put_back {
                Ok(_) | Err(crate::game_user_files::UserFilesError::NoSession { .. }) => e,
                Err(restore_error) => InstanceError::Other(format!(
                    "{e}; putting the player's files back also failed: {restore_error}. The journal was kept, and the next launch will finish restoring it."
                )),
            });
        }
    };

    // 5. Record spawned process identity in journal
    let _ = crate::game_user_files::record_process(
        ctx,
        &definition.id,
        &store,
        launched.identity.clone(),
    );

    // 6. Record launch
    let _ = record_launch(ctx, id);

    // 7. Open the session ledger row: what this launch ran with. Best effort by construction;
    //    a launch is never blocked or failed by its own bookkeeping.
    let session_id = crate::session_ledger::record_launch(ctx, id, definition, &store);

    Ok(LaunchedInstance {
        launched,
        prepared,
        store,
        running_from,
        session_id,
    })
}

/// List all instances across all games, including Minecraft.
/// Every game's instances, and a warning for each source that could not be
/// read, so an unreadable table never looks like "no instances".
pub fn list_all(ctx: &Ctx) -> (Vec<InstanceSummary>, Vec<String>) {
    let mut summaries = Vec::new();
    let mut warnings = Vec::new();

    // 1. Generic game instances
    let records = list(ctx).unwrap_or_else(|e| {
        warnings.push(format!("cannot read game instances: {e}"));
        Vec::new()
    });
    {
        for rec in records {
            let (pinned, runtime) = match &rec.base {
                BaseReference::Pinned { runtime, .. } => {
                    (Some(true), format!("{} {}", runtime.store, runtime.version))
                }
                BaseReference::Unpinned { install, .. } => {
                    let rt = ctx
                        .paths
                        .instance_dir(&rec.instance_id)
                        .ok()
                        .map(|d| d.join("instance_manifest.json"))
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .and_then(|c| serde_json::from_str::<GameInstanceManifest>(&c).ok())
                        .and_then(|m| m.runtime_identity)
                        .map(|rt| format!("{} {}", rt.store, rt.version))
                        .unwrap_or_else(|| install.to_string());
                    (Some(false), rt)
                }
            };
            summaries.push(InstanceSummary {
                instance_id: rec.instance_id,
                game: rec.game,
                name: rec.name,
                runtime,
                pinned,
                last_launched_at: rec.last_launched_at,
            });
        }
    }

    // 2. Minecraft instances through ctx.games.instance_backend()
    if let Ok(backend) = ctx.games.instance_backend() {
        let mc_rows = backend.list(ctx).unwrap_or_else(|e| {
            warnings.push(format!("cannot read Minecraft instances: {e}"));
            Vec::new()
        });
        {
            for row in mc_rows {
                let runtime = if row.loader.is_empty() {
                    row.minecraft_version
                } else {
                    format!("{} {}", row.minecraft_version, row.loader)
                };
                summaries.push(InstanceSummary {
                    instance_id: row.instance_id,
                    game: GameId::minecraft(),
                    name: row.name,
                    runtime,
                    pinned: None,
                    last_launched_at: row.last_launched_at,
                });
            }
        }
    }

    summaries.sort_by(|a, b| {
        b.last_launched_at
            .cmp(&a.last_launched_at)
            .then_with(|| a.game.as_str().cmp(b.game.as_str()))
            .then_with(|| a.name.cmp(&b.name))
    });

    (summaries, warnings)
}
