//! Ending a generic game session: the normal exit, and "Stop" for a game that will not close.
//!
//! A launch journals the game's OS identity in the user-files session. A normal exit restores
//! those files. A stop does the same end, but first ends the recorded process and the children it
//! started — and only those. A process the journal does not name is never touched, so a copy the
//! player started themselves survives.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use agora_game_api::{GameDefinition, StoreId};
use sysinfo::{Pid, System};

use crate::ctx::Ctx;
use crate::game_instance::{self, InstanceError};
use crate::game_user_files;
use crate::process_identity::{self, ProcessIdentity};
use crate::process_job;

/// How long a stop waits for the processes it ended to actually go.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// What ending a session did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    pub instance_id: String,
    /// The instance's total play time after this session.
    pub play_time_ms: i64,
}

/// What a stop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    pub instance_id: String,
    /// The processes ended (the game and the children it started).
    pub processes_ended: usize,
    pub play_time_ms: i64,
}

/// End the game's session the way a normal exit does: put the player's files back and add the
/// session to the instance's play time. No session means nothing to end (`Ok(None)`), so a stop
/// and the exit watcher can both call this and only one records the time.
pub fn end_session(
    ctx: &Ctx,
    definition: &GameDefinition,
    store: &StoreId,
) -> Result<Option<SessionEnd>, InstanceError> {
    let Some(details) = game_user_files::details(ctx, &definition.id, store)? else {
        return Ok(None);
    };
    // A journal whose start time cannot be read still gets its files back; it just adds no play
    // time, rather than leaving the player's files swapped over a timestamp.
    let started_at = chrono::DateTime::parse_from_rfc3339(&details.created_at)
        .map(|at| at.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::DateTime::<chrono::Utc>::from(ctx.clock.now()));
    match game_user_files::restore(ctx, definition, store) {
        Ok(_) => {}
        // Another caller (the exit watcher, when a stop ends the game) finished it first. That is
        // the same end, not a failure.
        Err(game_user_files::UserFilesError::NoSession { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let stats = game_instance::record_play_time(ctx, &details.instance, started_at)?;
    Ok(Some(SessionEnd {
        instance_id: details.instance,
        play_time_ms: stats.total_play_ms,
    }))
}

/// End the game Agora started for this instance, then run the normal session end.
///
/// Refuses when nothing is running. Refuses (and kills nothing) when the session is running only
/// because a process Agora did not start is alive.
pub fn stop(
    ctx: &Ctx,
    instance_id: &str,
    definition: &GameDefinition,
) -> Result<StopOutcome, InstanceError> {
    let record = game_instance::get(ctx, instance_id)?
        .ok_or_else(|| InstanceError::NotFound(instance_id.to_string()))?;
    let manifest = game_instance::get_manifest(ctx, instance_id)?;
    let store = game_instance::session_store(&record.base, &manifest).ok_or_else(|| {
        InstanceError::Other(format!(
            "'{instance_id}' has never been launched through a store Agora can name, so there is no session to stop."
        ))
    })?;

    let details = game_user_files::details(ctx, &definition.id, &store)?
        .ok_or_else(|| InstanceError::NothingRunning(instance_id.to_string()))?;
    if details.instance != instance_id {
        return Err(InstanceError::NothingRunning(instance_id.to_string()));
    }
    if !details.running {
        return Err(InstanceError::NothingRunning(instance_id.to_string()));
    }

    // The job the launch made holds everything the game started, including a game whose launcher
    // has already exited. It is the proof of "Agora started this": nothing outside it is reached.
    let jobs: Vec<&str> = details
        .processes
        .iter()
        .filter_map(|identity| identity.job.as_deref())
        .collect();
    let in_jobs: usize = jobs.iter().map(|job| process_job::active(job)).sum();
    // A launch with no job (another platform, or the job could not be made) is judged by the
    // identity it recorded: the process must still be the one Agora started.
    let alive: Vec<&ProcessIdentity> = details
        .processes
        .iter()
        .filter(|identity| process_identity::verify(identity).is_ok())
        .collect();
    if in_jobs == 0 && alive.is_empty() {
        // The journal says a process runs from the game folder, but not one Agora started.
        return Err(InstanceError::NotStartedByAgora(instance_id.to_string()));
    }

    let mut processes_ended = 0;
    for job in &jobs {
        processes_ended += process_job::terminate(job).map_err(InstanceError::Other)?;
    }
    processes_ended += terminate_tree(&alive);
    wait_until_gone(&alive, &jobs, STOP_TIMEOUT)?;

    let play_time_ms = match end_session(ctx, definition, &store)? {
        Some(end) => {
            if end.instance_id != instance_id {
                return Err(InstanceError::Other(format!(
                    "the session that ended belonged to '{}', not '{instance_id}'",
                    end.instance_id
                )));
            }
            end.play_time_ms
        }
        // The exit watcher ended the session first, because the stop it saw ended the game. The
        // stop is still what happened, so report the play time recorded then.
        None => game_instance::play_stats(ctx, instance_id)?.total_play_ms,
    };
    Ok(StopOutcome {
        instance_id: instance_id.to_string(),
        processes_ended,
        play_time_ms,
    })
}

/// A process to end: its pid and the start time that proves it is the same process.
type Target = (Pid, u64);

/// End every verified process and every descendant it started, children after parents. Returns
/// how many processes were actually signalled.
fn terminate_tree(roots: &[&ProcessIdentity]) -> usize {
    let mut system = System::new();
    system.refresh_processes();

    let mut queue: Vec<Pid> = roots
        .iter()
        .filter(|identity| process_identity::verify(identity).is_ok())
        .map(|identity| Pid::from_u32(identity.pid))
        .collect();

    let mut seen: HashSet<Pid> = HashSet::new();
    let mut targets: Vec<Target> = Vec::new();
    while let Some(pid) = queue.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let Some(process) = system.process(pid) else {
            continue;
        };
        let start_time = process.start_time();
        targets.push((pid, start_time));
        for (child_pid, child) in system.processes() {
            // A child cannot be older than its parent: an older one only names a parent that has
            // since exited and whose pid was reused.
            if child.parent() == Some(pid) && child.start_time() >= start_time {
                queue.push(*child_pid);
            }
        }
    }

    let mut ended = 0;
    for (pid, start_time) in targets {
        // Refresh right before the kill: a pid that was reused by another process is left alone.
        system.refresh_process(pid);
        if let Some(process) = system.process(pid) {
            if process.start_time() == start_time && process.kill() {
                ended += 1;
            }
        }
    }
    ended
}

/// Wait until none of the given identities matches a live process, or the timeout expires.
fn wait_until_gone(
    roots: &[&ProcessIdentity],
    jobs: &[&str],
    timeout: Duration,
) -> Result<(), InstanceError> {
    let deadline = Instant::now() + timeout;
    loop {
        if roots
            .iter()
            .all(|identity| process_identity::verify(identity).is_err())
            && jobs.iter().all(|job| process_job::active(job) == 0)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(InstanceError::Other(
                "The game did not close after being asked to stop. Close it yourself, then try again."
                    .into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
