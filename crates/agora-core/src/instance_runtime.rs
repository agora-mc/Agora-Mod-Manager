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
use crate::lock_manager::{LockGuard, LockRefusal, LockResource};
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

/// Read the current lease, if any. Only a missing file means "no lease": a permission denial, a
/// Windows sharing violation, a directory at the path or any other read failure is "can't tell"
/// and returned as an error, so no caller can proceed as if the instance were idle. A corrupt or
/// future-version lease is likewise reported as occupied — see [`check_idle`].
pub fn read(paths: &AppPaths, instance_id: &str) -> LauncherResult<Option<RuntimeLease>> {
    let path = lease_path(paths, instance_id)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LauncherError::Generic {
                code: "ERR_RUNTIME_LEASE_UNREADABLE".into(),
                message: format!(
                    "The runtime record for '{instance_id}' at {} could not be read ({error}), so \
                     it is not possible to tell whether a game is using it. Close any running game \
                     and try again.",
                    path.display()
                ),
            })
        }
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

/// What to do about a launch that was recorded but never verified. The same words reach the player
/// from the `ERR_INSTANCE_LAUNCH_INDETERMINATE` refusal, from [`game_activity`]'s can't-tell answer
/// and from [`ensure_game_not_running`], so the instruction cannot drift between them.
const INDETERMINATE_GUIDANCE: &str =
    "Make sure the game is closed, then clear the launch state for this instance and try again.";

/// The whole indeterminate refusal for an instance: what was seen, and what to do about it.
fn indeterminate_message(instance_id: &str) -> String {
    format!(
        "A launch of '{instance_id}' was recorded but never finished starting, so it is not \
         possible to tell whether Minecraft is running. {INDETERMINATE_GUIDANCE}"
    )
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
                message: indeterminate_message(instance_id),
            })
        }
    }
}

/// What is known about whether an instance's game is using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameActivity {
    /// A verified live game, or a view/wrap of the instance.
    Running,
    /// A launch was recorded but never verified: the files may be swapped in and the process that
    /// was starting the game has not recorded it. Something holds the instance, but no game is
    /// proven to be running — so a refusal must not tell the player to close one.
    Starting,
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
            // mutation exactly as `check_idle` says it must, but there is no game to close.
            (RuntimeState::Running, None) | (RuntimeState::Starting, _) => {
                return GameActivity::Starting;
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
                return if details.starting {
                    GameActivity::Starting
                } else {
                    GameActivity::Running
                };
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
/// "close the game before ...". Only proven idle passes: running, starting and "could not tell"
/// all refuse.
///
/// The refusal says what the player can do about it: a verified game gets "close the game", while a
/// launch that never finished starting gets the [`INDETERMINATE_GUIDANCE`] the
/// `ERR_INSTANCE_LAUNCH_INDETERMINATE` error gives, because there is no game to close.
pub fn ensure_game_not_running(ctx: &Ctx, instance_id: &str, action: &str) -> Result<(), String> {
    match game_activity(ctx, instance_id) {
        GameActivity::Idle => Ok(()),
        GameActivity::Running => Err(format!(
            "'{instance_id}' is running; close the game before {action}"
        )),
        GameActivity::Starting => Err(format!(
            "{} Refusing to {action}.",
            indeterminate_message(instance_id)
        )),
        GameActivity::CouldNotTell(reason) => Err(format!(
            "could not tell whether '{instance_id}' is running ({reason}); refusing to {action}"
        )),
    }
}

/// One typed answer to "may this process change instance X now?" (MASTER_SPEC §32.13).
///
/// The question used to have four differently shaped answers — an `InstanceGuard` or a
/// `LauncherError` code, a [`GameActivity`], a lock error code and a boolean — so every caller
/// compared code strings to tell "the game is running" from "a record could not be read". This is
/// the one decision: [`may_mutate`] asks it, [`InstanceGuard::for_mutation`] and its siblings are
/// this answer turned back into the guard-or-error the UI matches on, and a caller that needs to
/// know *why* matches a variant instead of a string.
///
/// `CantTell` never permits a change: an unreadable lease, an unreadable user-files session or
/// session store, and a launch that recorded nothing to verify all refuse.
#[derive(Debug)]
pub enum MayMutate {
    /// Nothing is using the instance, and the caller holds its exclusive writer lock. Dropping the
    /// guard releases the lock.
    Yes(InstanceGuard),
    /// Something is using it now. Wait for it to finish; a change would race it.
    Busy {
        cause: BusyCause,
        /// The refusal as the existing callers' error codes describe it, so the UI keeps
        /// `ERR_INSTANCE_RUNNING` / `ERR_LOCK_TIMEOUT` and their messages.
        error: LauncherError,
    },
    /// A record that answers the question could not be read, or a launch was recorded with no
    /// process identity to verify, or the instance lock could not be taken for a reason other
    /// than someone holding it. Nothing is known, so nothing may change.
    CantTell { error: LauncherError },
}

/// What is using the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyCause {
    /// The instance's game: a live runtime lease, a live user-files session, or an open session
    /// whose owner is alive.
    Game,
    /// Another Agora operation holding the instance's writer lock.
    Lock,
}

/// The liveness half of the answer, before the lock is tried.
enum Liveness {
    /// Nothing says a game is using the instance.
    Free,
    /// The instance's game is using it.
    Busy(LauncherError),
    /// A record could not be read, or a launch has nothing to verify.
    CantTell(LauncherError),
}

impl MayMutate {
    /// This answer as the guard or error [`InstanceGuard`]'s functions have always returned.
    ///
    /// The codes and messages are unchanged on purpose: the desktop UI and the CLI match on them,
    /// and this slice moves the decision, not the user-visible behaviour.
    fn into_result(self) -> LauncherResult<InstanceGuard> {
        match self {
            MayMutate::Yes(guard) => Ok(guard),
            MayMutate::Busy { error, .. } | MayMutate::CantTell { error } => Err(error),
        }
    }
}

/// May this process change `instance_id` now? Takes the instance's writer lock when it may.
///
/// One decision, in this order: the runtime lease, then the game's own session records, then the
/// lock (waiting the lock manager's default), then the lease and the sessions again — a launch
/// that was already in flight can have published its record while this call waited. The game
/// checks come first so a running game is refused at once instead of after a 30-second wait.
pub fn may_mutate(ctx: &Ctx, instance_id: &str, operation: &str) -> MayMutate {
    decide(ctx, instance_id, operation, false, |resource, operation| {
        ctx.lock_manager.acquire_typed(resource, operation)
    })
}

/// [`may_mutate`] with the user's explicit "change it while the game runs" answer (MASTER_SPEC
/// §32.13 override). The lock is still taken: two Agora operations never change one instance at
/// once, whatever the user allowed.
pub fn may_mutate_while_running(ctx: &Ctx, instance_id: &str, operation: &str) -> MayMutate {
    decide(ctx, instance_id, operation, true, |resource, operation| {
        ctx.lock_manager.acquire_typed(resource, operation)
    })
}

/// [`may_mutate`] with a bounded lock wait. Test-only: the production entry points use the lock
/// manager's default wait, and a test must not spend thirty seconds proving it.
#[cfg(test)]
fn may_mutate_within(
    ctx: &Ctx,
    instance_id: &str,
    operation: &str,
    wait: std::time::Duration,
) -> MayMutate {
    decide(ctx, instance_id, operation, false, |resource, operation| {
        ctx.lock_manager
            .acquire_with_timeout_typed(resource, operation, wait, None)
    })
}

fn decide(
    ctx: &Ctx,
    instance_id: &str,
    operation: &str,
    allow_while_running: bool,
    acquire: impl FnOnce(LockResource, &str) -> Result<LockGuard, LockRefusal>,
) -> MayMutate {
    if !allow_while_running {
        if let Some(refusal) = refusal(liveness_answer(ctx, instance_id)) {
            return refusal;
        }
    }
    let lock = match acquire(LockResource::Instance(instance_id.to_string()), operation) {
        Ok(lock) => lock,
        // A held lock is busy: something is using the instance and the change has to wait.
        Err(LockRefusal::Held(error)) => {
            return MayMutate::Busy {
                cause: BusyCause::Lock,
                error,
            }
        }
        // The lock could not be asked for at all (a lock directory that is not a directory, a
        // name that is not a lock name, a cancelled wait): nothing was learned about any holder,
        // so the answer is can't-tell, not "busy".
        Err(LockRefusal::Unavailable(error)) => return MayMutate::CantTell { error },
    };
    if !allow_while_running {
        if let Some(refusal) = refusal(liveness_answer(ctx, instance_id)) {
            return refusal;
        }
    }
    MayMutate::Yes(InstanceGuard {
        instance_id: instance_id.to_string(),
        lock,
        allow_while_running,
    })
}

/// The liveness answer as a refusal, or `None` when a change may proceed.
fn refusal(answer: Liveness) -> Option<MayMutate> {
    match answer {
        Liveness::Free => None,
        Liveness::Busy(error) => Some(MayMutate::Busy {
            cause: BusyCause::Game,
            error,
        }),
        Liveness::CantTell(error) => Some(MayMutate::CantTell { error }),
    }
}

/// The one liveness decision, answered by the records that exist.
fn liveness_answer(ctx: &Ctx, instance_id: &str) -> Liveness {
    match lease_answer(&ctx.paths, instance_id) {
        Liveness::Free => activity_answer(ctx, instance_id),
        refusal => refusal,
    }
}

/// The runtime lease's half of the answer.
///
/// The lease is asked first because it is the record that keeps its own error codes — a live game
/// is `ERR_INSTANCE_RUNNING` and a launch with nothing to verify is `ERR_INSTANCE_LAUNCH_INDETERMINATE`
/// — and because those codes are what the UI shows.
fn lease_answer(paths: &AppPaths, instance_id: &str) -> Liveness {
    let lease = match read(paths, instance_id) {
        Ok(Some(lease)) => lease,
        Ok(None) => return Liveness::Free,
        Err(error) => return Liveness::CantTell(error),
    };
    match (lease.state, &lease.identity) {
        (RuntimeState::Running, Some(identity)) => {
            if process_identity::verify(identity).is_ok() {
                return Liveness::Busy(LauncherError::Generic {
                    code: "ERR_INSTANCE_RUNNING".into(),
                    message: format!(
                        "Minecraft is still running for '{instance_id}' (pid {}). Close the game \
                         before changing this instance.",
                        identity.pid
                    ),
                });
            }
            // The recorded process is provably gone — pid, start time, and executable no longer
            // match — so the lease is stale. If it cannot be released, nothing was proved.
            match clear(paths, instance_id, Some(&lease.generation)) {
                Ok(()) => Liveness::Free,
                Err(error) => Liveness::CantTell(error),
            }
        }
        // Recorded as running but with nothing to verify: the same indeterminate case as Starting.
        (RuntimeState::Running, None) | (RuntimeState::Starting, _) => {
            Liveness::CantTell(LauncherError::Generic {
                code: "ERR_INSTANCE_LAUNCH_INDETERMINATE".into(),
                message: indeterminate_message(instance_id),
            })
        }
    }
}

/// The game-activity half of the answer: [`game_activity`] as Busy, can't-tell or free.
fn activity_answer(ctx: &Ctx, instance_id: &str) -> Liveness {
    match game_activity(ctx, instance_id) {
        GameActivity::Idle => Liveness::Free,
        GameActivity::Running => Liveness::Busy(LauncherError::Generic {
            code: "ERR_INSTANCE_RUNNING".into(),
            message: format!(
                "The game is running in '{instance_id}'; close it before changing this instance."
            ),
        }),
        // Nothing proves a game is running, so this is not "busy with a game": it is a launch
        // that has to be resolved by hand, the same answer the lease gives for that state.
        GameActivity::Starting => Liveness::CantTell(LauncherError::Generic {
            code: "ERR_INSTANCE_LAUNCH_INDETERMINATE".into(),
            message: indeterminate_message(instance_id),
        }),
        GameActivity::CouldNotTell(reason) => Liveness::CantTell(LauncherError::Generic {
            code: "ERR_INSTANCE_LAUNCH_INDETERMINATE".into(),
            message: format!(
                "could not tell whether the game is running in '{instance_id}' ({reason}); \
                 refusing to change this instance."
            ),
        }),
    }
}

/// Permission to change an instance, from one place for every service.
///
/// Holding this proves two things at once: this process has the instance's
/// exclusive writer lock, and (unless the user explicitly overrode it) the
/// instance's game is not running. Services that change an instance take it
/// instead of calling [`check_idle`] and `LockManager::acquire` themselves;
/// functions they call while it is held take `&InstanceGuard` rather than
/// taking the lock again (it is not re-entrant).
///
/// The game check comes *before* the wait for the lock. A running game holds no
/// lock — the lock covers only the mutation or the launch through spawn — so
/// asking first turns a 30-second timeout into an immediate, named refusal.
/// The check is repeated after the lock is taken, because a launch that was
/// already in flight can have published its lease while this guard waited.
///
/// The decision behind it is [`may_mutate`]; these functions keep the
/// guard-or-error shape and the error codes the UI matches on.
#[derive(Debug)]
pub struct InstanceGuard {
    instance_id: String,
    lock: LockGuard,
    allow_while_running: bool,
}

impl InstanceGuard {
    /// Take the instance lock and make sure no game is using the instance.
    ///
    /// Blocks while another Agora operation holds the lock (the default
    /// timeout). Use [`Self::for_mutation_async`] from async code.
    pub fn for_mutation(ctx: &Ctx, instance_id: &str, operation: &str) -> LauncherResult<Self> {
        may_mutate(ctx, instance_id, operation).into_result()
    }

    /// The explicit user override for the running-game check: take the lock and
    /// allow the change while a game is running. The lock itself is never
    /// skipped, so two Agora operations still never change one instance at once.
    pub fn for_mutation_while_running(
        ctx: &Ctx,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<Self> {
        may_mutate_while_running(ctx, instance_id, operation).into_result()
    }

    /// Async counterpart of [`Self::for_mutation`]. The wait for a contested
    /// lock yields to the runtime instead of blocking a worker thread.
    pub async fn for_mutation_async(
        ctx: &Ctx,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<Self> {
        Self::take_async(ctx, instance_id, operation, false)
            .await
            .into_result()
    }

    /// Async counterpart of [`Self::for_mutation_while_running`].
    pub async fn for_mutation_while_running_async(
        ctx: &Ctx,
        instance_id: &str,
        operation: &str,
    ) -> LauncherResult<Self> {
        Self::take_async(ctx, instance_id, operation, true)
            .await
            .into_result()
    }

    /// The same decision as [`may_mutate`] with the lock wait awaited instead of blocking:
    /// [`liveness_answer`] and [`refusal`] are shared, only the wait differs.
    async fn take_async(
        ctx: &Ctx,
        instance_id: &str,
        operation: &str,
        allow_while_running: bool,
    ) -> MayMutate {
        if !allow_while_running {
            if let Some(refusal) = refusal(liveness_answer(ctx, instance_id)) {
                return refusal;
            }
        }
        let lock = match ctx
            .lock_manager
            .acquire_async_typed(LockResource::Instance(instance_id.to_string()), operation)
            .await
        {
            Ok(lock) => lock,
            Err(LockRefusal::Held(error)) => {
                return MayMutate::Busy {
                    cause: BusyCause::Lock,
                    error,
                }
            }
            Err(LockRefusal::Unavailable(error)) => return MayMutate::CantTell { error },
        };
        if !allow_while_running {
            if let Some(refusal) = refusal(liveness_answer(ctx, instance_id)) {
                return refusal;
            }
        }
        MayMutate::Yes(InstanceGuard {
            instance_id: instance_id.to_string(),
            lock,
            allow_while_running,
        })
    }

    /// Whether the user explicitly allowed this change while the game runs. Services
    /// that have their own running-session check use this to skip it as well.
    pub fn allows_running(&self) -> bool {
        self.allow_while_running
    }

    /// The instance this guard authorises changes to.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The underlying lock path, for callers that pass it on.
    pub fn lock(&self) -> &LockGuard {
        &self.lock
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
        Err(error) => {
            // A Minecraft manifest is not a generic instance manifest: it has no generic `base`
            // and therefore no user-files journal keyed by a store. That is "no journal", not an
            // unreadable record. A generic manifest whose base is present but damaged still
            // fails closed.
            let manifest_path = ctx
                .paths
                .instance_manifest(instance_id)
                .map_err(|e| e.to_string())?;
            let Ok(bytes) = std::fs::read(&manifest_path) else {
                return Err(error.to_string());
            };
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            let game_is_minecraft = value
                .get("game")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|game| game == "minecraft");
            match value.get("base") {
                None | Some(serde_json::Value::Null) if game_is_minecraft => Ok(None),
                _ => Err(error.to_string()),
            }
        }
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

    fn init_test_state(ctx: &Ctx) {
        crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    }

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

    /// A lease record that cannot be read at all (a directory at its path, a permission denial or
    /// a sharing violation) must be can't-tell, never a missing lease that reads as idle.
    #[test]
    fn an_io_failure_reading_a_lease_is_not_idle() {
        let (paths, _tmp) = paths();
        let path = lease_path(&paths, "demo").unwrap();
        fs::create_dir(&path).unwrap();

        let error = read(&paths, "demo").unwrap_err();
        assert_eq!(error.code(), "ERR_RUNTIME_LEASE_UNREADABLE");
        assert!(
            error.to_string().contains("try again"),
            "a transient read failure must give a retryable message: {error}"
        );
        let idle = check_idle(&paths, "demo").unwrap_err();
        assert_eq!(idle.code(), "ERR_RUNTIME_LEASE_UNREADABLE");
        assert!(game_has_record(&paths, "demo"));
        assert!(!game_is_provably_gone(&paths, "demo"));
    }

    #[test]
    fn a_running_game_refuses_the_guard_at_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        init_test_state(&ctx);
        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&ctx.paths, "demo").unwrap();
        publish_running(&ctx.paths, "demo", &generation, identity).unwrap();

        let error = InstanceGuard::for_mutation(&ctx, "demo", "test").unwrap_err();
        assert_eq!(error.code(), "ERR_INSTANCE_RUNNING");
    }

    // ------------------------------------------------------------------
    // MayMutate: one typed answer (MASTER_SPEC §32.13)
    // ------------------------------------------------------------------

    fn guard_ctx() -> (Ctx, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        init_test_state(&ctx);
        (ctx, tmp)
    }

    /// Write a generic instance manifest whose base names `store`, so the instance has a
    /// user-files journal keyed by it. The id and name are invented; no real user data is used.
    fn write_generic_instance(ctx: &Ctx, id: &str, game: &str, store: &str) {
        let game_id = agora_game_api::GameId::new(game).unwrap();
        let manifest = crate::game_instance::GameInstanceManifest {
            manifest_version: 1,
            game: game_id.clone(),
            instance_id: id.to_string(),
            name: "Demo".to_string(),
            runtime_identity: None,
            base: agora_game_api::BaseReference::Pinned {
                id: "base-1".to_string(),
                runtime: agora_game_api::RuntimeIdentity {
                    game: game_id,
                    store: agora_game_api::StoreId::new(store).unwrap(),
                    version: "1.0".to_string(),
                    build: None,
                },
                mode: agora_game_api::BaseMode::Linked,
            },
            frameworks: Vec::new(),
            layers: agora_game_api::LayerStack::default(),
            deployment: None,
            saves: crate::game_instance::SavesChoice::Shared,
            save_scope: None,
            imported_from: None,
        };
        let dir = ctx.paths.instance_dir(id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("instance_manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn an_idle_instance_may_mutate() {
        let (ctx, _tmp) = guard_ctx();
        match may_mutate(&ctx, "demo", "test") {
            MayMutate::Yes(guard) => assert_eq!(guard.instance_id(), "demo"),
            other => panic!("an idle instance must be allowed: {other:?}"),
        }
    }

    /// A held lock is Busy, not a grant and not can't-tell: the change has to wait, and the
    /// existing `ERR_LOCK_TIMEOUT` answer is what the caller and the UI still see.
    #[test]
    fn a_held_instance_lock_is_busy() {
        let (ctx, _tmp) = guard_ctx();
        let holder = crate::lock_manager::LockManager::new(ctx.paths.locks_root().to_path_buf());
        let _held = holder
            .acquire(LockResource::Instance("demo".into()), "holder")
            .unwrap();

        match may_mutate_within(&ctx, "demo", "test", std::time::Duration::from_millis(150)) {
            MayMutate::Busy { cause, error } => {
                assert_eq!(cause, BusyCause::Lock);
                assert_eq!(error.code(), "ERR_LOCK_TIMEOUT");
            }
            other => panic!("a held lock must be busy: {other:?}"),
        }
    }

    /// A lock that cannot be taken for a reason other than "someone holds it" is can't-tell: it
    /// is unknown whether another operation holds it, so a change may not proceed — and it must
    /// not be reported as busy, which would say a holder exists.
    #[test]
    fn a_lock_that_cannot_be_taken_is_cant_tell() {
        let (ctx, _tmp) = guard_ctx();
        // The lock file's place is a directory, so opening the lock file fails with an I/O error.
        let lock_path = ctx.paths.instance_lock("demo").unwrap();
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        fs::create_dir(&lock_path).unwrap();

        let refusal = match may_mutate(&ctx, "demo", "test") {
            MayMutate::CantTell { error } => error,
            other => panic!("a lock that cannot be taken must be can't-tell: {other:?}"),
        };
        assert_eq!(refusal.code(), "ERR_LOCK_IO");
        // The legacy wrapper keeps the code the UI matches on.
        let error = InstanceGuard::for_mutation(&ctx, "demo", "test").unwrap_err();
        assert_eq!(error.code(), "ERR_LOCK_IO");
    }

    /// A live game is Busy(Game), and the code the callers translate is unchanged.
    #[test]
    fn a_live_lease_is_busy_for_the_game() {
        let (ctx, _tmp) = guard_ctx();
        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&ctx.paths, "demo").unwrap();
        publish_running(&ctx.paths, "demo", &generation, identity).unwrap();

        match may_mutate(&ctx, "demo", "test") {
            MayMutate::Busy { cause, error } => {
                assert_eq!(cause, BusyCause::Game);
                assert_eq!(error.code(), "ERR_INSTANCE_RUNNING");
            }
            other => panic!("a live game must be busy: {other:?}"),
        }
    }

    /// A launch that recorded nothing to verify is can't-tell, not "idle" and not "running".
    #[test]
    fn an_indeterminate_launch_is_cant_tell() {
        let (ctx, _tmp) = guard_ctx();
        publish_starting(&ctx.paths, "demo").unwrap();

        match may_mutate(&ctx, "demo", "test") {
            MayMutate::CantTell { error } => {
                assert_eq!(error.code(), "ERR_INSTANCE_LAUNCH_INDETERMINATE")
            }
            other => panic!("an unfinished launch must be can't-tell: {other:?}"),
        }
    }

    /// A launch that never finished starting is not a game the player can close: the refusal has to
    /// carry the `ERR_INSTANCE_LAUNCH_INDETERMINATE` guidance instead.
    #[test]
    fn a_launch_that_never_finished_starting_gets_the_indeterminate_guidance() {
        let (ctx, _tmp) = guard_ctx();
        publish_starting(&ctx.paths, "demo").unwrap();

        let refusal = ensure_game_not_running(&ctx, "demo", "take a snapshot").unwrap_err();
        assert!(
            refusal.contains("clear the launch state"),
            "an unfinished launch gets the indeterminate guidance: {refusal}"
        );
        assert!(
            !refusal.contains("close the game"),
            "there is no game to close: {refusal}"
        );
        assert!(
            refusal.contains("take a snapshot"),
            "the refusal still names the action: {refusal}"
        );
    }

    /// A verified live game keeps the instruction a player can follow: close it.
    #[test]
    fn a_verified_live_game_is_refused_with_close_the_game() {
        let (ctx, _tmp) = guard_ctx();
        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&ctx.paths, "demo").unwrap();
        publish_running(&ctx.paths, "demo", &generation, identity).unwrap();

        let refusal = ensure_game_not_running(&ctx, "demo", "take a snapshot").unwrap_err();
        assert!(
            refusal.contains("close the game"),
            "a live game gets the instruction it can follow: {refusal}"
        );
        assert!(
            !refusal.contains("clear the launch state"),
            "a live game does not need the launch-state guidance: {refusal}"
        );
    }

    /// Write the instance's user-files journal: the processes the session recorded, and the launcher
    /// that swapped the files in when there is one. `running_from` does not exist, so the process
    /// scan has nothing to look at and the recorded identities decide.
    fn write_session_journal(
        ctx: &Ctx,
        processes: Vec<ProcessIdentity>,
        starting_launcher: Option<ProcessIdentity>,
    ) {
        let journal = ctx.paths.user_files_journal_path("skyrim-se", "steam");
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        let mut record = serde_json::json!({
            "game": "skyrim-se",
            "store": "steam",
            "instance_id": "demo",
            "created_at": "2026-01-01T00:00:00Z",
            "running_from": ctx.paths.instance_dir("demo").unwrap().join("no-such-folder"),
            "processes": processes,
            "files": [],
        });
        if let Some(launcher) = starting_launcher {
            record["starting_launcher"] = serde_json::to_value(launcher).unwrap();
        }
        std::fs::write(&journal, serde_json::to_vec(&record).unwrap()).unwrap();
    }

    /// A journal whose only live process is the launcher that has not recorded its game yet: the
    /// launch holds the session, but nothing proves a game is running.
    #[test]
    fn a_journal_with_only_a_live_starting_launcher_is_not_a_running_game() {
        let (ctx, _tmp) = guard_ctx();
        write_generic_instance(&ctx, "demo", "skyrim-se", "steam");
        let launcher = process_identity::capture(std::process::id()).unwrap();
        write_session_journal(&ctx, Vec::new(), Some(launcher));

        let refusal = ensure_game_not_running(&ctx, "demo", "take a snapshot").unwrap_err();
        assert!(
            refusal.contains("clear the launch state"),
            "an unfinished launch gets the indeterminate guidance: {refusal}"
        );
        assert!(
            !refusal.contains("close the game"),
            "there is no game to close: {refusal}"
        );
    }

    /// A journal with the game's own live process is a verified live game, not a starting one.
    #[test]
    fn a_journal_with_a_live_game_process_is_a_running_game() {
        let (ctx, _tmp) = guard_ctx();
        write_generic_instance(&ctx, "demo", "skyrim-se", "steam");
        let game = process_identity::capture(std::process::id()).unwrap();
        write_session_journal(&ctx, vec![game], None);

        let refusal = ensure_game_not_running(&ctx, "demo", "take a snapshot").unwrap_err();
        assert!(
            refusal.contains("close the game"),
            "a live game gets the instruction it can follow: {refusal}"
        );
        assert!(
            !refusal.contains("clear the launch state"),
            "a live game does not need the launch-state guidance: {refusal}"
        );
    }

    /// A lease record that cannot be read is can't-tell, never a grant.
    #[test]
    fn an_unreadable_lease_is_cant_tell() {
        let (ctx, _tmp) = guard_ctx();
        let path = lease_path(&ctx.paths, "demo").unwrap();
        fs::create_dir_all(&path).unwrap();

        let refusal = match may_mutate(&ctx, "demo", "test") {
            MayMutate::CantTell { error } => error,
            other => panic!("an unreadable lease must be can't-tell: {other:?}"),
        };
        assert_eq!(refusal.code(), "ERR_RUNTIME_LEASE_UNREADABLE");
        // One decision: the existing guard function refuses with the same error, not its own.
        let error = InstanceGuard::for_mutation(&ctx, "demo", "test").unwrap_err();
        assert_eq!(error.code(), refusal.code());
    }

    /// An unreadable user-files journal is a record that answers the question, so it is
    /// can't-tell, not "no session".
    #[test]
    fn an_unreadable_user_files_journal_is_cant_tell() {
        let (ctx, _tmp) = guard_ctx();
        write_generic_instance(&ctx, "demo", "skyrim-se", "steam");
        let journal = ctx.paths.user_files_journal_path("skyrim-se", "steam");
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        // A parse failure must not read as "no session": that is the class of bug this closes.
        fs::write(&journal, b"{ this is not a journal").unwrap();

        match may_mutate(&ctx, "demo", "test") {
            MayMutate::CantTell { error } => assert!(
                error.to_string().contains("journal"),
                "the refusal must name the unreadable record: {error}"
            ),
            other => panic!("an unreadable journal must be can't-tell: {other:?}"),
        }
    }

    #[test]
    fn the_running_game_override_still_takes_the_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        init_test_state(&ctx);
        let identity = process_identity::capture(std::process::id()).unwrap();
        let generation = publish_starting(&ctx.paths, "demo").unwrap();
        publish_running(&ctx.paths, "demo", &generation, identity).unwrap();

        let guard = InstanceGuard::for_mutation_while_running(&ctx, "demo", "test").unwrap();
        assert_eq!(guard.instance_id(), "demo");
        // Another operation still cannot take the same lock, override or not.
        let other = crate::lock_manager::LockManager::new(ctx.paths.locks_root().to_path_buf());
        assert!(other
            .acquire_with_timeout(
                LockResource::Instance("demo".into()),
                "other",
                std::time::Duration::ZERO,
                None,
            )
            .is_err());
    }

    #[test]
    fn an_idle_instance_yields_the_guard_and_blocks_a_second_writer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        init_test_state(&ctx);
        let guard = InstanceGuard::for_mutation(&ctx, "demo", "first").unwrap();
        let other = crate::lock_manager::LockManager::new(ctx.paths.locks_root().to_path_buf());
        assert!(other
            .acquire_with_timeout(
                LockResource::Instance("demo".into()),
                "second",
                std::time::Duration::ZERO,
                None,
            )
            .is_err());
        drop(guard);
        InstanceGuard::for_mutation(&ctx, "demo", "third").unwrap();
    }

    /// The async form must wait with the runtime, not block its only thread.
    #[test]
    fn an_async_guard_does_not_block_a_current_thread_runtime() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = Ctx::for_testing(tmp.path().to_path_buf());
        init_test_state(&ctx);
        let holder = crate::lock_manager::LockManager::new(ctx.paths.locks_root().to_path_buf());
        let _held = holder
            .acquire(LockResource::Instance("demo".into()), "holder")
            .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // If the guard blocked the thread instead of awaiting, the timer
            // could not fire and this test would hang instead of failing.
            let waited = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                InstanceGuard::for_mutation_async(&ctx, "demo", "waiter"),
            )
            .await;
            assert!(waited.is_err(), "the async guard should have yielded");
            drop(_held);
            InstanceGuard::for_mutation_async(&ctx, "demo", "after")
                .await
                .unwrap();
        });
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
