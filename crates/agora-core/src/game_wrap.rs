//! `agora games wrap`: Steam's launch options, Proton, and the Linux game view (MASTER_SPEC
//! §26.5).
//!
//! Steam starts a game through the command the player puts in its launch options. When that is
//! `agora games wrap %command%`, this module picks the instance to play, opens the session in the
//! ledger, mounts the instance's view over the game's install folder (or steps down to links or
//! copies, announced), waits, closes the session and diagnoses it with the same crash pipeline
//! `games instance launch --wait` uses on Windows.
//!
//! `games instance launch` on Linux writes a [`PendingLaunch`] record and asks Steam to start the
//! game; the wrapper finds that record first. With no record it uses the game's default instance,
//! chosen with `games instance default`. With neither it runs Steam's command untouched and says
//! why: a game with no Agora instance must still start.
//!
//! Nothing here names a game. The Steam app id, the install and the Proton prefix come from the
//! install record; the per-user files come from the game definition.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, UNIX_EPOCH};

use agora_game_api::{BaseReference, GameDefinition, GameId, StoreId, UserFileStrategy};
use serde::{Deserialize, Serialize};

use crate::crash_live::LivePacks;
use crate::ctx::Ctx;
use crate::game_deploy::DeployMode;
use crate::game_discovery::steam;
use crate::game_instance;
use crate::linux_view::{self, CommandSpec, PerUserMount, ViewOutcome, ViewRequest};

/// How long a `games instance launch` record is trusted. Steam starts the wrapper within seconds;
/// two minutes leaves room for the store's own start-up without letting yesterday's launch claim
/// today's.
pub const PENDING_WINDOW: Duration = Duration::from_secs(120);

/// The line every Linux launch prints until a real Proton run has been tested (owner decision,
/// 2026-11-15).
pub const PROTON_UNTESTED: &str =
    "Launching through Proton on Linux has not been tested on real hardware yet.";

/// Points Steam launches at an invented command. Test hook; nothing in the product sets it.
pub const STEAM_COMMAND_ENV: &str = "AGORA_STEAM_COMMAND";

/// One `games instance launch` waiting for Steam to start the wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLaunch {
    pub instance_id: String,
    pub game: String,
    pub created_unix_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum WrapError {
    #[error("no command was given; Steam supplies one as %command%")]
    NoCommand,
    #[error("{0}")]
    Instance(#[from] game_instance::InstanceError),
    #[error("{0}")]
    UserFiles(#[from] crate::game_user_files::UserFilesError),
    #[error("{0}")]
    Deploy(#[from] crate::game_deploy::DeployError),
    #[error("the game's definition could not be read: {0}")]
    Definition(String),
    #[error("the instance '{0}' is not a Steam install, so Steam cannot launch it")]
    NotSteam(String),
    #[error("the Steam install for '{0}' could not be found")]
    InstallNotFound(String),
    #[error("Steam is not installed, or its 'steam' command is not on PATH")]
    SteamMissing,
    #[error("Steam could not be asked to start the game: {0}")]
    SteamStart(String),
    #[error("the instance's view could not run: {0}")]
    View(#[from] crate::linux_view::ViewError),
    #[error("the per-user file '{}' cannot be used: {reason}", path.display())]
    PerUserFile { path: PathBuf, reason: String },
    #[error("the command could not be started: {0}")]
    Command(String),
    #[error("the instance's default could not be saved: {0}")]
    Settings(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Paths(#[from] crate::error::LauncherError),
}

/// What a wrapper run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapOutcome {
    /// The command ran with the instance's view (or its announced step-down).
    Ran {
        instance_id: String,
        mechanism: String,
        exit_code: Option<i32>,
        stepped_down: Option<String>,
        /// Why putting the player's files back after a step-down failed, if it did. The files
        /// stay swapped in and the next launch or a restore finishes the job.
        restore_error: Option<String>,
    },
    /// No instance was chosen, so Steam's command ran untouched and the reason says why.
    PassedThrough {
        reason: String,
        exit_code: Option<i32>,
    },
}

impl WrapOutcome {
    /// The exit code the wrapper process should return.
    pub fn exit_code(&self) -> i32 {
        match self {
            WrapOutcome::Ran { exit_code, .. } | WrapOutcome::PassedThrough { exit_code, .. } => {
                exit_code.unwrap_or(1)
            }
        }
    }
}

/// What Steam's launch options look like for one instance.
pub fn launch_status(ctx: &Ctx, instance_id: &str) -> Result<steam::SteamLaunchStatus, WrapError> {
    let (_, app_id) = instance_install(ctx, instance_id)?;
    let roots = crate::game_discovery::platform::find_steam_roots();
    Ok(steam::launch_status(&roots, &app_id))
}

/// Ask Steam to start an instance: record the pending launch, then open `steam://rungameid/<app>`.
/// The wrapper Steam starts finds the record and runs the instance.
pub fn start_via_steam(
    ctx: &Ctx,
    instance_id: &str,
) -> Result<steam::SteamLaunchStatus, WrapError> {
    let (_, app_id) = instance_install(ctx, instance_id)?;
    let record = game_instance::get(ctx, instance_id)?
        .ok_or_else(|| game_instance::InstanceError::NotFound(instance_id.to_string()))?;
    write_pending(
        ctx,
        &PendingLaunch {
            instance_id: instance_id.to_string(),
            game: record.game.as_str().to_string(),
            created_unix_ms: now_ms(ctx),
        },
    )?;
    let roots = crate::game_discovery::platform::find_steam_roots();
    let flatpak_root = roots.iter().any(|root| is_flatpak_root(root));
    let launcher = match std::env::var_os(STEAM_COMMAND_ENV) {
        Some(program) => Some(SteamLauncher::Native(PathBuf::from(program))),
        None => launcher_in(std::env::var_os("PATH").as_deref(), flatpak_root),
    };
    let Some(launcher) = launcher else {
        clear_pending(ctx);
        return Err(WrapError::SteamMissing);
    };
    let started = launcher
        .command(&format!("steam://rungameid/{app_id}"))
        .spawn();
    if let Err(error) = started {
        // Nothing will pick the record up, so it must not claim a later launch.
        clear_pending(ctx);
        if error.kind() == std::io::ErrorKind::NotFound {
            return Err(WrapError::SteamMissing);
        }
        return Err(WrapError::SteamStart(error.to_string()));
    }
    let mut status = steam::launch_status(&roots, &app_id);
    status.launcher = launcher.describe().to_string();
    Ok(status)
}

/// How Steam can be asked to start a game on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SteamLauncher {
    /// The store's own `steam` command.
    Native(PathBuf),
    /// A Flatpak install has no `steam` on `PATH`; its command names the sandbox.
    Flatpak(PathBuf),
}

impl SteamLauncher {
    fn command(&self, url: &str) -> Command {
        match self {
            SteamLauncher::Native(program) => {
                let mut command = Command::new(program);
                command.arg(url);
                command
            }
            SteamLauncher::Flatpak(program) => {
                let mut command = Command::new(program);
                command.args(["run", "com.valvesoftware.Steam", url]);
                command
            }
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            SteamLauncher::Native(_) => "steam",
            SteamLauncher::Flatpak(_) => "flatpak run com.valvesoftware.Steam",
        }
    }
}

fn is_flatpak_root(root: &Path) -> bool {
    root.components()
        .any(|part| part.as_os_str() == "com.valvesoftware.Steam")
}

/// `steam` when it is on `PATH`; `flatpak` when only a Flatpak Steam root exists. A machine with
/// neither cannot be asked to start a game, which is an error, never a silent no-op.
fn launcher_in(path: Option<&OsStr>, flatpak_root: bool) -> Option<SteamLauncher> {
    if let Some(steam) = find_executable(path, "steam") {
        return Some(SteamLauncher::Native(steam));
    }
    if flatpak_root {
        if let Some(flatpak) = find_executable(path, "flatpak") {
            return Some(SteamLauncher::Flatpak(flatpak));
        }
    }
    None
}

fn find_executable(path: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if is_executable(&with_exe) {
                return Some(with_exe);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Run Steam's command through the chosen instance, or untouched when there is none.
pub fn wrap(ctx: &Ctx, live: &LivePacks<'_>, command: &[String]) -> Result<WrapOutcome, WrapError> {
    let Some(program) = command.first().map(PathBuf::from) else {
        return Err(WrapError::NoCommand);
    };
    let args: Vec<String> = command.iter().skip(1).cloned().collect();

    let Some(instance_id) = choose_instance(ctx, &program)? else {
        return Ok(run_passthrough(
            &program,
            &args,
            "no pending launch and no default instance for this game",
        ));
    };

    let record = game_instance::get(ctx, &instance_id)?
        .ok_or_else(|| game_instance::InstanceError::NotFound(instance_id.clone()))?;
    let definition = ctx
        .games
        .game(&record.game)
        .ok_or_else(|| WrapError::Definition(record.game.as_str().to_string()))?
        .clone();

    let (install, app_id) = instance_install(ctx, &instance_id)?;
    let prefix = steam::proton_prefix(&install, &app_id);
    let store = game_instance::session_store(
        &record.base,
        &game_instance::get_manifest(ctx, &instance_id)?,
    )
    .unwrap_or_else(StoreId::steam);

    if !cfg!(target_os = "linux") {
        return Ok(run_passthrough(
            &program,
            &args,
            "the Linux game view is only available on Linux",
        ));
    }

    let mounts = per_user_mounts(ctx, &instance_id, &definition, &prefix)?;
    let request = ViewRequest {
        instance_id: instance_id.clone(),
        target: install.clone(),
        command: CommandSpec {
            program: program.clone(),
            args: args.clone(),
            environment: None,
            // Steam starts the command with the game folder as its working directory. Re-enter
            // that path inside the namespace after the overlay is mounted, so a relative read or
            // write goes through the view instead of the folder underneath it.
            working_directory: Some(install.clone()),
        },
        per_user_files: mounts,
    };

    let session_id = crate::session_ledger::record_launch(ctx, &instance_id, &definition, &store);
    let started = std::time::Instant::now();
    let outcome = linux_view::native().view_and_run(ctx, &definition, &request);

    match outcome {
        Ok(ViewOutcome::Ran(run)) => {
            finish_session(
                ctx,
                live,
                &definition,
                session_id.as_deref(),
                &program,
                run.pid,
                Some(&run.status),
                started.elapsed(),
            );
            Ok(WrapOutcome::Ran {
                instance_id,
                mechanism: run.mechanism.as_str().to_string(),
                exit_code: run.status.code(),
                stepped_down: None,
                restore_error: None,
            })
        }
        Ok(ViewOutcome::SteppedDown(step)) => {
            eprintln!(
                "The Linux game view could not mount over the game folder ({}). Running from {} instead; writes go into that deployment.",
                step.reason,
                step.next.plain_name()
            );
            run_step_down(
                ctx,
                live,
                &instance_id,
                &definition,
                &store,
                &install,
                &program,
                &args,
                step.next,
                &step.reason,
                session_id.as_deref(),
                started,
            )
        }
        Err(crate::linux_view::ViewError::Deploy(
            crate::game_deploy::DeployError::UnpinnedInstance(_),
        )) => {
            if let Some(session) = session_id.as_deref() {
                finish_session(
                    ctx,
                    live,
                    &definition,
                    Some(session),
                    &program,
                    0,
                    None,
                    started.elapsed(),
                );
            }
            Ok(run_passthrough(
                &program,
                &args,
                "the instance is not pinned to a base, so its content cannot be shown",
            ))
        }
        Err(error) => {
            if let Some(session) = session_id.as_deref() {
                finish_session(
                    ctx,
                    live,
                    &definition,
                    Some(session),
                    &program,
                    0,
                    None,
                    started.elapsed(),
                );
            }
            Err(WrapError::View(error))
        }
    }
}

/// Record which instance is an instance's game's default, so the wrapper can pick it when Steam
/// starts the game without a pending launch.
pub fn set_default_instance(ctx: &Ctx, instance_id: &str) -> Result<(), WrapError> {
    let record = game_instance::get(ctx, instance_id)?
        .ok_or_else(|| game_instance::InstanceError::NotFound(instance_id.to_string()))?;
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| WrapError::Settings(error.to_string()))?;
    crate::db::set_setting(
        &conn,
        &default_key(&record.game),
        &serde_json::Value::String(instance_id.to_string()),
    )
    .map_err(|error| WrapError::Settings(error.to_string()))?;
    Ok(())
}

/// The instance the wrapper plays for a game when Steam starts it directly.
pub fn default_instance(ctx: &Ctx, game: &GameId) -> Option<String> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).ok()?;
    let value = crate::db::get_setting(&conn, &default_key(game))
        .ok()
        .flatten()?;
    value.as_str().map(str::to_string)
}

fn default_key(game: &GameId) -> String {
    format!("game_instance_default.{}", game.as_str())
}

/// The fresh pending launch, removed as it is read. Expired records are dropped too.
pub fn take_pending(ctx: &Ctx) -> Option<PendingLaunch> {
    let path = ctx.paths.pending_launch_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let pending: PendingLaunch = serde_json::from_str(&text).ok()?;
    let age = now_ms(ctx).saturating_sub(pending.created_unix_ms);
    (age >= 0 && age <= PENDING_WINDOW.as_millis() as i64).then_some(pending)
}

/// Write a pending launch, replacing any older one.
pub fn write_pending(ctx: &Ctx, pending: &PendingLaunch) -> Result<(), WrapError> {
    let path = ctx.paths.pending_launch_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| WrapError::Settings(error.to_string()))?;
    }
    let bytes = serde_json::to_vec_pretty(pending)
        .map_err(|error| WrapError::Settings(error.to_string()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|error| WrapError::Settings(error.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|error| WrapError::Settings(error.to_string()))?;
    Ok(())
}

/// Remove the pending launch without reading it.
pub fn clear_pending(ctx: &Ctx) {
    let _ = std::fs::remove_file(ctx.paths.pending_launch_path());
}

fn now_ms(ctx: &Ctx) -> i64 {
    ctx.clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The instance to play: the pending launch, else the default for the game the command belongs to.
fn choose_instance(ctx: &Ctx, program: &Path) -> Result<Option<String>, WrapError> {
    let app_id = std::env::var("SteamAppId").ok();
    let identified = identify_game(ctx, program, app_id.as_deref());

    if let Some(pending) = take_pending(ctx) {
        if let Some(game) = identified.as_ref() {
            if pending.game != game.as_str() {
                eprintln!(
                    "The pending launch names '{}', but Steam is starting '{}'; using the default instance instead.",
                    pending.game,
                    game.as_str()
                );
            } else {
                return Ok(Some(pending.instance_id));
            }
        } else {
            return Ok(Some(pending.instance_id));
        }
    }

    let Some(game) = identified else {
        return Ok(None);
    };
    Ok(default_instance(ctx, &game))
}

/// Which game a Steam command belongs to: its app id names one in the registry, else the install
/// folder that holds the command does.
fn identify_game(ctx: &Ctx, program: &Path, app_id: Option<&str>) -> Option<GameId> {
    let steam = StoreId::steam();
    if let Some(app_id) = app_id {
        if let Some(game) = ctx.games.game_for_store_product(&steam, app_id) {
            return Some(game.clone());
        }
    }
    let report = crate::game_discovery::discover_with_folders(&ctx.paths);
    let program = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let install = report.installs.iter().find(|install| {
        let location =
            std::fs::canonicalize(&install.location).unwrap_or_else(|_| install.location.clone());
        program.starts_with(&location)
    });
    if let Some(install) = install {
        if let Some(game) = ctx
            .games
            .game_for_store_product(&install.store, &install.product)
        {
            return Some(game.clone());
        }
    }
    let folder = program.parent()?;
    ctx.games.game_for_folder(folder)
}

/// The install folder and Steam app id an instance runs from.
fn instance_install(ctx: &Ctx, instance_id: &str) -> Result<(PathBuf, String), WrapError> {
    let manifest = game_instance::get_manifest(ctx, instance_id)?;
    match &manifest.base {
        BaseReference::Pinned { id, .. } => {
            let text = std::fs::read_to_string(ctx.paths.base_manifest_path(id))?;
            let base: crate::game_base::BaseManifest = serde_json::from_str(&text)?;
            let app_id = base
                .source_product
                .clone()
                .ok_or_else(|| WrapError::NotSteam(instance_id.to_string()))?;
            Ok((base.source_location, app_id))
        }
        BaseReference::Unpinned { install, .. } => {
            let (store, product) = install
                .as_str()
                .split_once(':')
                .ok_or_else(|| WrapError::InstallNotFound(instance_id.to_string()))?;
            if store != "steam" {
                return Err(WrapError::NotSteam(instance_id.to_string()));
            }
            let report = crate::game_discovery::discover_with_folders(&ctx.paths);
            let found = report
                .installs
                .iter()
                .find(|discovered| {
                    crate::game_registry::make_install_id(&discovered.store, &discovered.product)
                        == *install
                })
                .ok_or_else(|| WrapError::InstallNotFound(instance_id.to_string()))?;
            Ok((found.location.clone(), product.to_string()))
        }
    }
}

fn per_user_mounts(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
    prefix: &Option<PathBuf>,
) -> Result<Vec<PerUserMount>, WrapError> {
    let instance_dir = ctx.paths.instance_dir(instance_id)?;
    let mut mounts = Vec::new();
    let store = StoreId::steam();
    for mapping in &definition.user_files {
        if mapping.strategy != UserFileStrategy::JournaledSwap || !mapping.applies_to_store(&store)
        {
            continue;
        }
        let real = crate::game_user_files::resolve_user_file_source_in(
            prefix.as_deref(),
            &mapping.source,
        )?;
        let instance_copy = instance_dir.join(mapping.instance_path.as_str());
        // A bind mount needs a file at both ends, but the real prefix is the player's and is
        // never written by the wrapper: no empty placeholder, no first-run blanking. A file the
        // player does not have is not bound at all, so the game creates it itself, as it would
        // without Agora.
        match std::fs::metadata(&real) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(WrapError::PerUserFile {
                    path: real,
                    reason: "it is not a regular file".to_string(),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(WrapError::PerUserFile {
                    path: real,
                    reason: error.to_string(),
                })
            }
        }
        // The instance's first launch keeps the player's settings, as Windows does: with no copy
        // of its own the instance starts from the player's file.
        if !instance_copy.is_file() {
            if instance_copy.exists() {
                return Err(WrapError::PerUserFile {
                    path: instance_copy,
                    reason: "it is not a regular file".to_string(),
                });
            }
            seed_instance_copy(&real, &instance_copy)?;
        }
        mounts.push(PerUserMount {
            real_path: real,
            instance_copy,
        });
    }
    Ok(mounts)
}

/// Copy the player's file into the instance folder, through a temporary name, so a first Linux
/// launch reads their settings and an interrupted copy never becomes the instance's copy.
fn seed_instance_copy(real: &Path, instance_copy: &Path) -> Result<(), WrapError> {
    let parent = instance_copy
        .parent()
        .ok_or_else(|| WrapError::PerUserFile {
            path: instance_copy.to_path_buf(),
            reason: "it has no parent folder".to_string(),
        })?;
    std::fs::create_dir_all(parent).map_err(|error| WrapError::PerUserFile {
        path: parent.to_path_buf(),
        reason: error.to_string(),
    })?;
    let temporary = instance_copy.with_file_name(format!(
        ".agora-seed-{}-{}",
        std::process::id(),
        instance_copy
            .file_name()
            .map(|name| name.to_string_lossy().replace(['/', '\\'], "_"))
            .unwrap_or_default()
    ));
    std::fs::copy(real, &temporary).map_err(|error| WrapError::PerUserFile {
        path: real.to_path_buf(),
        reason: error.to_string(),
    })?;
    std::fs::rename(&temporary, instance_copy).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        WrapError::PerUserFile {
            path: instance_copy.to_path_buf(),
            reason: error.to_string(),
        }
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_step_down(
    ctx: &Ctx,
    live: &LivePacks<'_>,
    instance_id: &str,
    definition: &GameDefinition,
    store: &StoreId,
    install: &Path,
    program: &Path,
    args: &[String],
    mode: DeployMode,
    reason: &str,
    session_id: Option<&str>,
    started: std::time::Instant,
) -> Result<WrapOutcome, WrapError> {
    // Steam's Stop sends SIGTERM. Install the handler before the swap, so a stop at any point
    // after the player's files change still puts them back before this process exits.
    let _stop = install_step_down_stop();

    crate::game_deploy::deploy(ctx, instance_id, definition, mode)?;
    let farm = crate::game_deploy::deployment_dir(ctx, instance_id)?.ok_or_else(|| {
        crate::game_deploy::DeployError::Other(format!(
            "the deployment for '{instance_id}' was not built"
        ))
    })?;
    let swapped = crate::game_user_files::swap_in(ctx, instance_id, definition, store, &farm);
    if let Err(error) = swapped {
        // Nothing ran, so the session that was opened must not stay open as if it had.
        if let Some(session) = session_id {
            finish_session(
                ctx,
                live,
                definition,
                Some(session),
                program,
                0,
                None,
                started.elapsed(),
            );
        }
        return Err(WrapError::UserFiles(error));
    }

    // A stop that arrived during the deploy or the swap must still put the player's files back.
    if step_down_stop_requested() {
        let restore_error = end_step_down(
            ctx, live, definition, store, session_id, program, None, started,
        );
        return Ok(WrapOutcome::Ran {
            instance_id: instance_id.to_string(),
            mechanism: mode.plain_name().to_string(),
            exit_code: Some(STOP_EXIT_CODE),
            stepped_down: Some(reason.to_string()),
            restore_error,
        });
    }

    let mode_program = remap(program, install, &farm);
    let mode_args: Vec<String> = args
        .iter()
        .map(|arg| remap_arg(arg, install, &farm))
        .collect();
    let mut child = match Command::new(&mode_program)
        .args(&mode_args)
        .current_dir(&farm)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            end_step_down(
                ctx, live, definition, store, session_id, program, None, started,
            );
            return Err(WrapError::Command(error.to_string()));
        }
    };
    set_step_down_child(Some(child.id()));
    let wait = child.wait();
    set_step_down_child(None);
    let status = match wait {
        Ok(status) => status,
        Err(error) => {
            end_step_down(
                ctx, live, definition, store, session_id, program, None, started,
            );
            return Err(WrapError::Command(error.to_string()));
        }
    };
    if step_down_stop_requested() {
        // The player asked for this; the session ledger says a stop ended it.
        crate::session_ledger::record_stop(ctx, instance_id);
    }
    let restore_error = end_step_down(
        ctx,
        live,
        definition,
        store,
        session_id,
        program,
        Some(&status),
        started,
    );
    Ok(WrapOutcome::Ran {
        instance_id: instance_id.to_string(),
        mechanism: mode.plain_name().to_string(),
        exit_code: status.code(),
        stepped_down: Some(reason.to_string()),
        restore_error,
    })
}

/// The exit code a wrapper yields when Steam's Stop asked it to end: 128 + SIGTERM.
const STOP_EXIT_CODE: i32 = 143;

/// Put the player's per-user files back after a step-down, report a failure instead of
/// discarding it, and record the session's end. Returns the failure, if any.
#[allow(clippy::too_many_arguments)]
fn end_step_down(
    ctx: &Ctx,
    live: &LivePacks<'_>,
    definition: &GameDefinition,
    store: &StoreId,
    session_id: Option<&str>,
    program: &Path,
    status: Option<&ExitStatus>,
    started: std::time::Instant,
) -> Option<String> {
    let restore_error = match crate::game_instance::end_session(ctx, definition, store) {
        Ok(_) => None,
        Err(error) => {
            let reason = error.to_string();
            eprintln!(
                "agora games wrap: {}",
                crate::game_user_files::restore_failure_message(
                    &reason,
                    definition.id.as_str(),
                    store.as_str()
                )
            );
            Some(reason)
        }
    };
    finish_session(
        ctx,
        live,
        definition,
        session_id,
        program,
        0,
        status,
        started.elapsed(),
    );
    restore_error
}

#[cfg(target_os = "linux")]
static STEP_DOWN_CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
#[cfg(target_os = "linux")]
static STEP_DOWN_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Installs a SIGTERM handler for the step-down command's lifetime and restores the old one.
#[cfg(target_os = "linux")]
struct StepDownStop {
    previous: nix::sys::signal::SigAction,
}

#[cfg(target_os = "linux")]
extern "C" fn on_step_down_signal(_signal: i32) {
    use std::sync::atomic::Ordering;
    let child = STEP_DOWN_CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // Forward to the command only. Its own group is left alone, so a wineserver that
        // belongs to the prefix and is shared with another launch is never signalled.
        unsafe { nix::libc::kill(child, nix::libc::SIGTERM) };
    }
    STEP_DOWN_STOP.store(true, Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
fn install_step_down_stop() -> StepDownStop {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    use std::sync::atomic::Ordering;
    STEP_DOWN_CHILD.store(0, Ordering::SeqCst);
    STEP_DOWN_STOP.store(false, Ordering::SeqCst);
    let action = SigAction::new(
        SigHandler::Handler(on_step_down_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    let previous = unsafe { sigaction(Signal::SIGTERM, &action) }
        .unwrap_or_else(|_| SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()));
    StepDownStop { previous }
}

#[cfg(target_os = "linux")]
impl Drop for StepDownStop {
    fn drop(&mut self) {
        let _ = unsafe {
            nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTERM, &self.previous)
        };
    }
}

#[cfg(target_os = "linux")]
fn set_step_down_child(pid: Option<u32>) {
    use std::sync::atomic::Ordering;
    STEP_DOWN_CHILD.store(pid.map(|pid| pid as i32).unwrap_or(0), Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
fn step_down_stop_requested() -> bool {
    STEP_DOWN_STOP.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(target_os = "linux"))]
struct StepDownStop;

#[cfg(not(target_os = "linux"))]
fn install_step_down_stop() -> StepDownStop {
    StepDownStop
}

#[cfg(not(target_os = "linux"))]
fn set_step_down_child(_pid: Option<u32>) {}

#[cfg(not(target_os = "linux"))]
fn step_down_stop_requested() -> bool {
    false
}

fn remap(path: &Path, install: &Path, farm: &Path) -> PathBuf {
    match path.strip_prefix(install) {
        Ok(relative) => farm.join(relative),
        Err(_) => path.to_path_buf(),
    }
}

fn remap_arg(arg: &str, install: &Path, farm: &Path) -> String {
    let path = Path::new(arg);
    if path.is_absolute() && path.starts_with(install) {
        remap(path, install, farm).to_string_lossy().to_string()
    } else {
        arg.to_string()
    }
}

fn run_passthrough(program: &Path, args: &[String], reason: &str) -> WrapOutcome {
    eprintln!(
        "agora games wrap: {reason}. Starting the game untouched, without this instance's mods."
    );
    let status = Command::new(program).args(args).status();
    match status {
        Ok(status) => WrapOutcome::PassedThrough {
            reason: reason.to_string(),
            exit_code: status.code(),
        },
        Err(error) => {
            eprintln!(
                "agora games wrap: the command '{}' could not be started: {error}",
                program.display()
            );
            WrapOutcome::PassedThrough {
                reason: reason.to_string(),
                exit_code: None,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_session(
    ctx: &Ctx,
    live: &LivePacks<'_>,
    definition: &GameDefinition,
    session_id: Option<&str>,
    program: &Path,
    pid: u32,
    status: Option<&ExitStatus>,
    elapsed: Duration,
) {
    let image = program
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| program.display().to_string());
    let exit = match status.map(ExitStatus::code) {
        Some(Some(code)) => agora_game_api::crash::ExitObservation::Code {
            pid,
            image,
            code: code as u32,
        },
        Some(None) => agora_game_api::crash::ExitObservation::unavailable(
            "the command ended without an exit code (a signal)",
        ),
        None => agora_game_api::crash::ExitObservation::unavailable("the command did not run"),
    };
    let report = crate::game_launch::SessionExitReport {
        processes: Vec::new(),
        relaunched_outside: false,
        exit,
        game_runtime_ms: Some(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX) as u64),
    };
    let Some(session_id) = session_id else {
        return;
    };
    crate::session_ledger::record_end(
        ctx,
        session_id,
        definition,
        &report,
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
    );
    let _ = crate::crash_live::diagnose_session(
        ctx,
        session_id,
        LivePacks {
            packs: live.packs,
            built_in: live.built_in,
            host: live.host,
            instances: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_pending_launch_is_not_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().join("data"));
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let clock = std::sync::Arc::new(crate::ctx::TestClock::new(
            std::time::SystemTime::UNIX_EPOCH,
        ));
        let ctx = ctx.with_clock(clock.clone());
        write_pending(
            &ctx,
            &PendingLaunch {
                instance_id: "old".into(),
                game: "a-game".into(),
                created_unix_ms: 0,
            },
        )
        .unwrap();
        clock.advance(PENDING_WINDOW + Duration::from_secs(1));
        assert!(take_pending(&ctx).is_none());
        // Taking it removed the file, even though it was stale.
        assert!(!ctx.paths.pending_launch_path().exists());
    }

    #[test]
    fn a_fresh_pending_launch_is_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().join("data"));
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        write_pending(
            &ctx,
            &PendingLaunch {
                instance_id: "fresh".into(),
                game: "a-game".into(),
                created_unix_ms: now_ms(&ctx),
            },
        )
        .unwrap();
        assert_eq!(take_pending(&ctx).unwrap().instance_id, "fresh");
        assert!(take_pending(&ctx).is_none());
    }

    #[test]
    fn steam_is_preferred_and_flatpak_is_used_only_when_steam_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let path = tmp.path().join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_string()
            });
            std::fs::write(
                &path,
                b"#!/bin/sh
exit 0
",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        };
        let path = std::env::join_paths([tmp.path()]).unwrap();

        let steam = make("steam");
        let flatpak = make("flatpak");
        assert_eq!(
            launcher_in(Some(&path), true),
            Some(SteamLauncher::Native(steam))
        );

        std::fs::remove_file(
            tmp.path()
                .join(if cfg!(windows) { "steam.exe" } else { "steam" }),
        )
        .unwrap();
        assert_eq!(
            launcher_in(Some(&path), true),
            Some(SteamLauncher::Flatpak(flatpak))
        );
        assert_eq!(launcher_in(Some(&path), false), None);
        assert_eq!(launcher_in(None, true), None);
    }

    #[test]
    fn a_flatpak_root_is_recognised_from_its_sandbox_folder() {
        assert!(is_flatpak_root(Path::new(
            "/home/player/.var/app/com.valvesoftware.Steam/.local/share/Steam"
        )));
        assert!(!is_flatpak_root(Path::new(
            "/home/player/.local/share/Steam"
        )));
    }

    #[test]
    fn only_a_wrapper_with_the_passthrough_counts() {
        assert!(steam::runs_wrapper("agora games wrap %command%"));
        assert!(steam::runs_wrapper("FOO=1 agora games wrap %command% -x"));
        assert!(!steam::runs_wrapper("agora games wrap"));
        assert!(!steam::runs_wrapper("%command%"));
    }

    #[test]
    fn a_command_under_the_install_is_remapped_into_the_farm() {
        let root = std::env::temp_dir();
        let install = root.join("games").join("A Game");
        let farm = root.join("agora/deployments/i1/game");
        let game = install.join("Game.exe");
        assert_eq!(
            remap_arg(&game.display().to_string(), &install, &farm),
            farm.join("Game.exe").display().to_string()
        );
        assert_eq!(remap_arg("-windowed", &install, &farm), "-windowed");
        let elsewhere = root.join("elsewhere/tool");
        assert_eq!(
            remap_arg(&elsewhere.display().to_string(), &install, &farm),
            elsewhere.display().to_string()
        );
    }
}
