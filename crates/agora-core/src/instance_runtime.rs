//! Durable record of whether a game is currently using an instance.
//!
//! # Why this is not just the instance lock
//!
//! [`LockResource::Instance`](crate::lock_manager::LockResource::Instance) is a
//! short-lived mutex held by whichever process is *mutating* an instance. It
//! cannot express "a game is running", because the game outlives the critical
//! section — and, on the delegated launch path, can outlive the launcher
//! itself. A lock owned by the launcher gets released (or correctly detected as
//! stale, its owner being genuinely dead) while Minecraft is still writing to
//! `saves/`, at which point another process is free to restore a snapshot out
//! from under a live game.
//!
//! So ownership has to track the *game*, not the launcher. The lease records
//! the game's OS-level [`ProcessIdentity`] — pid plus start time plus
//! executable — which
//! [`process_identity::verify`](crate::process_identity::verify) checks
//! fail-closed against pid reuse.
//!
//! # The starting window
//!
//! Between spawning the game and learning its identity there is a window where
//! a crash leaves a `Starting` lease with no identity to verify. That state is
//! deliberately **not** self-clearing: a launcher that died proves nothing
//! about whether a game exists, and neither does a timeout. It blocks mutation
//! until [`clear`] is called explicitly, which is a decision for the user to
//! make once they can see no game is running.
//!
//! Lease files live under the locks root, never inside the instance directory —
//! coordination state must not be something a restore can roll back.

use crate::app_paths::AppPaths;
use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};
use crate::process_identity::{self, ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const RUNTIME_LEASE_SCHEMA_VERSION: u32 = 1;

/// How far along a launch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeState {
    /// Spawned, or about to be, but the game's identity is not yet recorded.
    Starting,
    /// Running, with a verifiable process identity.
    Running,
}

/// The durable "this instance is in use" record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeLease {
    pub schema_version: u32,
    pub instance_id: String,
    /// Identifies this particular launch. Only the holder of a matching
    /// generation may clear the lease, so a late exit handler from a previous
    /// launch cannot release the current one.
    pub generation: String,
    pub state: RuntimeState,
    /// Present once the game has been identified.
    pub identity: Option<ProcessIdentity>,
    pub launcher_pid: u32,
    pub recorded_at: String,
}

fn lease_path(paths: &AppPaths, instance_id: &str) -> LauncherResult<PathBuf> {
    // Reuses the lock path validation, then swaps the extension, so an
    // instance id that is unsafe as a lock name is unsafe here too.
    let lock = paths.instance_lock(instance_id)?;
    Ok(lock.with_extension("runtime.json"))
}

fn write_lease(paths: &AppPaths, lease: &RuntimeLease) -> LauncherResult<()> {
    let path = lease_path(paths, &lease.instance_id)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| LauncherError::Generic {
            code: "ERR_RUNTIME_LEASE".into(),
            message: format!("Could not create the lock directory: {e}"),
        })?;
    }
    let bytes = serde_json::to_vec_pretty(lease).map_err(|e| LauncherError::Generic {
        code: "ERR_RUNTIME_LEASE".into(),
        message: format!("Could not serialize the runtime lease: {e}"),
    })?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes).map_err(|e| LauncherError::Generic {
        code: "ERR_RUNTIME_LEASE".into(),
        message: format!("Could not write the runtime lease: {e}"),
    })?;
    fs::rename(&tmp, &path).map_err(|e| LauncherError::Generic {
        code: "ERR_RUNTIME_LEASE".into(),
        message: format!("Could not publish the runtime lease: {e}"),
    })
}

/// Read the current lease, if any. A corrupt lease is treated as present and
/// unreadable rather than absent — see [`check_idle`].
pub fn read(paths: &AppPaths, instance_id: &str) -> LauncherResult<Option<RuntimeLease>> {
    let path = lease_path(paths, instance_id)?;
    let Ok(bytes) = fs::read(&path) else {
        return Ok(None);
    };
    match serde_json::from_slice::<RuntimeLease>(&bytes) {
        Ok(lease) if lease.schema_version == RUNTIME_LEASE_SCHEMA_VERSION => Ok(Some(lease)),
        // Unreadable or from a future version: report it as occupied. Deleting
        // it would be assuming the instance is idle on the strength of a file
        // we could not parse.
        _ => Err(LauncherError::Generic {
            code: "ERR_RUNTIME_LEASE_UNREADABLE".into(),
            message: format!(
                "The runtime record for '{instance_id}' at {} cannot be read, so it is not \
                 possible to tell whether a game is using it. Close any running game and remove \
                 that file to continue.",
                path.display()
            ),
        }),
    }
}

/// Publish a `Starting` lease and return its generation.
///
/// Call this *before* spawning, while holding the instance lock.
pub fn publish_starting(paths: &AppPaths, instance_id: &str) -> LauncherResult<String> {
    let generation = uuid::Uuid::new_v4().to_string();
    write_lease(
        paths,
        &RuntimeLease {
            schema_version: RUNTIME_LEASE_SCHEMA_VERSION,
            instance_id: instance_id.to_string(),
            generation: generation.clone(),
            state: RuntimeState::Starting,
            identity: None,
            launcher_pid: std::process::id(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        },
    )?;
    Ok(generation)
}

/// Upgrade the lease to `Running` once the game's identity is known.
pub fn publish_running(
    paths: &AppPaths,
    instance_id: &str,
    generation: &str,
    identity: ProcessIdentity,
) -> LauncherResult<()> {
    write_running(paths, instance_id, generation, Some(identity))
}

/// Record that the game is running although its OS identity could not be read.
///
/// The launcher is watching the child handle, so it can still clear the lease
/// when the game exits. After an Agora restart there is no identity to verify:
/// the lease is then treated as an unfinished launch (indeterminate), never as
/// a free instance, until the user clears it.
pub fn publish_running_unverified(
    paths: &AppPaths,
    instance_id: &str,
    generation: &str,
) -> LauncherResult<()> {
    write_running(paths, instance_id, generation, None)
}

fn write_running(
    paths: &AppPaths,
    instance_id: &str,
    generation: &str,
    identity: Option<ProcessIdentity>,
) -> LauncherResult<()> {
    write_lease(
        paths,
        &RuntimeLease {
            schema_version: RUNTIME_LEASE_SCHEMA_VERSION,
            instance_id: instance_id.to_string(),
            generation: generation.to_string(),
            state: RuntimeState::Running,
            identity,
            launcher_pid: std::process::id(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        },
    )
}

/// Release the lease.
///
/// A `generation` clears only a matching lease, so a stale exit handler cannot
/// release a newer launch. `None` forces the release — the explicit escape
/// hatch for a `Starting` lease the user has confirmed is dead.
pub fn clear(paths: &AppPaths, instance_id: &str, generation: Option<&str>) -> LauncherResult<()> {
    let path = lease_path(paths, instance_id)?;
    if let Some(generation) = generation {
        match read(paths, instance_id) {
            Ok(Some(lease)) if lease.generation != generation => return Ok(()),
            // An unreadable lease is not ours to interpret, so leave it.
            Err(_) => return Ok(()),
            _ => {}
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LauncherError::Generic {
            code: "ERR_RUNTIME_LEASE".into(),
            message: format!("Could not clear the runtime lease: {error}"),
        }),
    }
}

/// Whether the runtime lease proves the recorded game is gone. Only a `Running` lease whose
/// process identity no longer verifies is proof; no lease at all is not, because a delegated
/// launch keeps no process identity and the game may still run. Never clears anything.
pub fn game_is_provably_gone(paths: &AppPaths, instance_id: &str) -> bool {
    match read(paths, instance_id) {
        Ok(Some(lease)) => match (lease.state, lease.identity) {
            (RuntimeState::Running, Some(identity)) => process_identity::verify(&identity).is_err(),
            _ => false,
        },
        _ => false,
    }
}

/// Whether this instance has any runtime record at all, readable or not. An unreadable record
/// counts as present: it cannot prove the game is gone.
pub fn game_has_record(paths: &AppPaths, instance_id: &str) -> bool {
    !matches!(read(paths, instance_id), Ok(None))
}

/// Establish that no game is using this instance.
///
/// Call while holding the instance lock, before any mutation.
pub fn check_idle(paths: &AppPaths, instance_id: &str) -> LauncherResult<()> {
    let Some(lease) = read(paths, instance_id)? else {
        return Ok(());
    };
    match (lease.state, &lease.identity) {
        (RuntimeState::Running, Some(identity)) => {
            if process_identity::verify(identity).is_ok() {
                return Err(LauncherError::Generic {
                    code: "ERR_INSTANCE_RUNNING".into(),
                    message: format!(
                        "Minecraft is still running for '{instance_id}' (pid {}). Close the game \
                         before changing this instance.",
                        identity.pid
                    ),
                });
            }
            // The recorded process is provably gone — pid, start time, and
            // executable no longer match — so the lease is stale.
            clear(paths, instance_id, Some(&lease.generation))?;
            Ok(())
        }
        // Recorded as running but with nothing to verify: the same
        // indeterminate case as Starting.
        (RuntimeState::Running, None) | (RuntimeState::Starting, _) => {
            Err(LauncherError::Generic {
                code: "ERR_INSTANCE_LAUNCH_INDETERMINATE".into(),
                message: format!(
                    "A launch of '{instance_id}' was recorded but never finished starting, so it \
                     is not possible to tell whether Minecraft is running. Make sure the game is \
                     closed, then clear the launch state for this instance and try again."
                ),
            })
        }
    }
}

/// What is known about whether an instance's game is using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameActivity {
    /// A live game, a launch between the swap and the spawn, or a view/wrap of the instance.
    Running,
    /// Nothing says a game is using it.
    Idle,
    /// A record that answers the question cannot be read. The risky action must refuse.
    CouldNotTell(String),
}

/// Whether this instance's game is using it, from one place for every caller.
///
/// Game-free: it reads the instance's own user-files journal (which names its game and store), the
/// session ledger's open sessions, and the runtime lease the Minecraft launch publishes. An open
/// session with a live owner is a launch that has not ended, which is the only record a Linux view
/// writes; a game with no journaled files writes no journal either. An unreadable record is
/// [`GameActivity::CouldNotTell`], never `Idle`: a missing or unreadable store must not be read as
/// "no game is running".
pub fn game_activity(ctx: &Ctx, instance_id: &str) -> GameActivity {
    // The runtime lease. Minecraft publishes one; another game has none.
    match read(&ctx.paths, instance_id) {
        Err(error) => return GameActivity::CouldNotTell(error.to_string()),
        Ok(Some(lease)) => match (lease.state, &lease.identity) {
            (RuntimeState::Running, Some(identity))
                if process_identity::verify(identity).is_ok() =>
            {
                return GameActivity::Running;
            }
            // A launch with nothing to verify was recorded but never finished starting. It blocks
            // mutation exactly as `check_idle` says it must.
            (RuntimeState::Running, None) | (RuntimeState::Starting, _) => {
                return GameActivity::Running;
            }
            _ => {}
        },
        Ok(None) => {}
    }

    // The instance's user-files journal: live processes, a running-from folder, or a launch that
    // swapped the files in and has not spawned yet.
    match instance_game_and_store(ctx, instance_id) {
        Ok(Some((game, store))) => match crate::game_user_files::details(ctx, &game, &store) {
            Err(error) => return GameActivity::CouldNotTell(error.to_string()),
            Ok(Some(details)) if details.instance == instance_id && details.running => {
                return GameActivity::Running;
            }
            _ => {}
        },
        Ok(None) => {}
        Err(error) => return GameActivity::CouldNotTell(error),
    }

    // The session ledger's open sessions. A live owner means the session has not ended; this is
    // the only record a Linux view or a game with no journaled files writes.
    match ledger_session_running(ctx, instance_id) {
        Ok(true) => GameActivity::Running,
        Ok(false) => GameActivity::Idle,
        Err(error) => GameActivity::CouldNotTell(error),
    }
}

/// Refuse a change while the instance's game may be using it. `action` completes the sentence
/// "close the game before ...". Only proven idle passes: running and "could not tell" both refuse.
pub fn ensure_game_not_running(ctx: &Ctx, instance_id: &str, action: &str) -> Result<(), String> {
    match game_activity(ctx, instance_id) {
        GameActivity::Idle => Ok(()),
        GameActivity::Running => Err(format!(
            "'{instance_id}' is running; close the game before {action}"
        )),
        GameActivity::CouldNotTell(reason) => Err(format!(
            "could not tell whether '{instance_id}' is running ({reason}); refusing to {action}"
        )),
    }
}

fn instance_game_and_store(
    ctx: &Ctx,
    instance_id: &str,
) -> Result<Option<(agora_game_api::GameId, agora_game_api::StoreId)>, String> {
    // The manifest is the instance's own record and needs no database row. It names the game and
    // store the user-files journal is keyed by.
    match crate::game_instance::get_manifest(ctx, instance_id) {
        Ok(manifest) => Ok(
            crate::game_instance::session_store(&manifest.base, &manifest)
                .map(|store| (manifest.game.clone(), store)),
        ),
        Err(crate::game_instance::InstanceError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn ledger_session_running(ctx: &Ctx, instance_id: &str) -> Result<bool, String> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| format!("the session store could not be read: {error}"))?;
    let open = crate::session_ledger::open_sessions_for_instance(&conn, instance_id)
        .map_err(|error| format!("the session store could not be read: {error}"))?;
    if open.is_empty() {
        return Ok(false);
    }
    for session in &open {
        if let (Some(pid), Some(started)) = (session.owner_pid, session.owner_start) {
            if crate::session_ledger::Owner::is_alive(pid, started as u64) {
                return Ok(true);
            }
        }
    }
    // A gone owner says nothing about the game. With a runtime record that cannot prove the game
    // gone, the session may still be running (a delegated launch keeps no process identity).
    Ok(game_has_record(&ctx.paths, instance_id) && !game_is_provably_gone(&ctx.paths, instance_id))
}

/// Stop the game running an instance, from any process.
///
/// A session this process launched is stopped through the session it tracks. Otherwise the game
/// is found through the runtime lease the launch published, which carries its OS identity, and is
/// stopped through [`ProcessSessionManager::terminate`](crate::process_session_manager::ProcessSessionManager::terminate)
/// with the same pid and identity checks: a pid that now belongs to another program is never
/// killed. A launch that recorded no identity (one that never finished starting, or the official
/// launcher's, which Agora does not track) cannot be stopped here. Returns the stopped pid.
pub fn stop_game(ctx: &Ctx, instance_id: &str) -> LauncherResult<u32> {
    let sessions = &ctx.process_session_manager;
    if let Some(session) = sessions
        .list()
        .into_iter()
        .find(|session| session.instance_id == instance_id)
    {
        sessions.terminate(session.session_id, session.pid)?;
        return Ok(session.pid);
    }
    let not_running = || LauncherError::Generic {
        code: "ERR_GAME_NOT_RUNNING".into(),
        message: format!("Agora has no record of a running game for '{instance_id}'."),
    };
    let lease = read(&ctx.paths, instance_id)?.ok_or_else(not_running)?;
    let identity = match (lease.state, lease.identity) {
        (RuntimeState::Running, Some(identity)) => identity,
        (RuntimeState::Running, None) | (RuntimeState::Starting, _) => {
            return Err(LauncherError::Generic {
                code: "ERR_GAME_IDENTITY_UNKNOWN".into(),
                message: format!(
                    "A launch of '{instance_id}' was recorded without the game's process identity, \
                     so Agora cannot safely tell which process to stop. Close the game yourself."
                ),
            })
        }
    };
    let pid = identity.pid;
    let session_id = uuid::Uuid::new_v4().as_u64_pair().0;
    sessions.register(crate::process_session_manager::ProcessSession {
        instance_id: instance_id.to_string(),
        session_id,
        pid,
        process_identity: identity,
        snapshot_id: String::new(),
        start_time: std::time::SystemTime::now(),
        attached: false,
        user_cancelled: false,
    })?;
    let result = sessions.terminate(session_id, pid);
    // The session existed only to run the checks; nothing in this process waits on its exit.
    sessions.remove(session_id);
    sessions.take_user_stop(session_id);
    result.map(|()| pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (AppPaths, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = AppPaths::from_root(tmp.path().to_path_buf());
        fs::create_dir_all(paths.locks_root()).unwrap();
        (paths, tmp)
    }

    #[test]
    fn no_lease_means_idle() {
        let (paths, _tmp) = paths();
        check_idle(&paths, "demo").unwrap();
    }

    #[test]
    fn a_live_game_blocks_mutation() {
        let (paths, _tmp) = paths();
        // This test process is a live process with a verifiable identity.
        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&paths, "demo").unwrap();
        publish_running(&paths, "demo", &generation, identity).unwrap();

        let error = check_idle(&paths, "demo").unwrap_err();
        assert!(format!("{error:?}").contains("ERR_INSTANCE_RUNNING"));
    }

    /// The launcher dying does not prove the game did, so a half-published
    /// launch must not silently become "idle".
    #[test]
    fn an_unfinished_launch_is_indeterminate_not_idle() {
        let (paths, _tmp) = paths();
        publish_starting(&paths, "demo").unwrap();
        let error = check_idle(&paths, "demo").unwrap_err();
        assert!(format!("{error:?}").contains("ERR_INSTANCE_LAUNCH_INDETERMINATE"));

        // ...and it clears only when something explicitly says so.
        clear(&paths, "demo", None).unwrap();
        check_idle(&paths, "demo").unwrap();
    }

    #[test]
    fn a_stale_generation_cannot_release_a_newer_launch() {
        let (paths, _tmp) = paths();
        let old = publish_starting(&paths, "demo").unwrap();
        let new = publish_starting(&paths, "demo").unwrap();
        clear(&paths, "demo", Some(&old)).unwrap();
        assert!(
            read(&paths, "demo").unwrap().is_some(),
            "the newer lease was released by a stale generation"
        );
        clear(&paths, "demo", Some(&new)).unwrap();
        assert!(read(&paths, "demo").unwrap().is_none());
    }

    #[test]
    fn an_unreadable_lease_is_not_treated_as_idle() {
        let (paths, _tmp) = paths();
        let path = lease_path(&paths, "demo").unwrap();
        fs::write(&path, b"{ this is not a lease").unwrap();
        assert!(check_idle(&paths, "demo").is_err());
    }

    #[test]
    fn only_a_stale_running_lease_proves_the_game_is_gone() {
        let (paths, _tmp) = paths();
        assert!(
            !game_is_provably_gone(&paths, "demo"),
            "no lease is not proof"
        );

        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&paths, "demo").unwrap();
        publish_running(&paths, "demo", &generation, identity.clone()).unwrap();
        assert!(
            !game_is_provably_gone(&paths, "demo"),
            "a live game is not gone"
        );

        let stale = crate::process_identity::ProcessIdentity {
            pid: u32::MAX - 2,
            ..identity
        };
        publish_running(&paths, "demo", &generation, stale).unwrap();
        assert!(game_is_provably_gone(&paths, "demo"));

        publish_starting(&paths, "demo").unwrap();
        assert!(
            !game_is_provably_gone(&paths, "demo"),
            "an unfinished launch is not proof"
        );
    }

    fn stop_ctx() -> (Ctx, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = crate::ctx::CoreContext::for_testing(tmp.path().to_path_buf());
        fs::create_dir_all(ctx.paths.locks_root()).unwrap();
        (ctx, tmp)
    }

    #[test]
    fn stopping_an_instance_with_no_launch_record_is_an_error() {
        let (ctx, _tmp) = stop_ctx();
        let error = stop_game(&ctx, "demo").unwrap_err();
        assert_eq!(error.code(), "ERR_GAME_NOT_RUNNING");
    }

    #[test]
    fn a_launch_without_an_identity_is_never_guessed_at() {
        let (ctx, _tmp) = stop_ctx();
        publish_starting(&ctx.paths, "demo").unwrap();
        let error = stop_game(&ctx, "demo").unwrap_err();
        assert_eq!(error.code(), "ERR_GAME_IDENTITY_UNKNOWN");
        assert!(ctx.process_session_manager.list().is_empty());
    }

    #[test]
    fn a_recorded_game_that_is_gone_is_reported_stale_and_nothing_is_kept() {
        let (ctx, _tmp) = stop_ctx();
        let me = process_identity::capture_self().expect("identity of this test process");
        let gone = ProcessIdentity {
            pid: u32::MAX - 2,
            ..me
        };
        let generation = publish_starting(&ctx.paths, "demo").unwrap();
        publish_running(&ctx.paths, "demo", &generation, gone).unwrap();
        let error = stop_game(&ctx, "demo").unwrap_err();
        assert!(
            matches!(error, LauncherError::ProcessStale { .. }),
            "{error:?}"
        );
        assert!(ctx.process_session_manager.list().is_empty());
    }
}
