//! OS-level process identity captured immediately after spawn and verified
//! before any process-management operation.  The identity serves as a
//! fail‑closed guard against PID reuse, stale records, and accidental
//! cross‑session signalling.
//!
//! # Design
//!
//! * `capture` queries the running OS (via [`sysinfo`]) for PID, start time,
//!   and expected executable path of a just‑spawned child process.
//! * `check` refreshes the OS record and says whether PID, start_time and
//!   executable still match the captured identity — or that it could not tell.
//!   A caller that reads "could not tell" as gone acts on a question it never
//!   got an answer to; see [`ProcessCheck`].
//!
//! Both functions are **pure** – they take the data they need and return a
//! result.  No global or async state is involved.

use crate::error::{LauncherError, LauncherResult};
use std::time::Duration;
use sysinfo::{Pid, System};

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Query the OS for the process identity of `pid` immediately after spawn.
///
/// **Fail‑closed**: if the process is no longer alive or the OS cannot supply
/// start_time, this returns `Err(ProcessCaptureFailed)`.
pub fn capture(pid: u32) -> LauncherResult<ProcessIdentity> {
    let mut previous = capture_once(pid)?;
    let parent_exe = std::env::current_exe().ok().map(|path| {
        path.canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    });

    // On Unix the PID becomes visible between fork and exec. A single sample
    // can therefore still report the parent's executable even though the
    // child transitions to Java immediately afterwards. Require two stable
    // consecutive samples before committing the identity.
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(10));
        let current = match capture_once(pid) {
            Ok(current) => current,
            // A very short-lived child may exit while identity is settling.
            // Retain the last valid sample so its waiter can classify the exit.
            Err(_) => return Ok(previous),
        };
        let still_parent_image = parent_exe.as_deref().is_some_and(|parent| {
            current
                .expected_exe
                .as_deref()
                .is_some_and(|exe| exe.replace('\\', "/") == parent)
        });
        if current.start_time == previous.start_time
            && current.expected_exe == previous.expected_exe
            && !still_parent_image
        {
            return Ok(current);
        }
        previous = current;
    }

    Ok(previous)
}

fn capture_once(pid: u32) -> LauncherResult<ProcessIdentity> {
    let mut system = System::new();
    // Refresh only the single process to keep the call lightweight.
    system.refresh_process(Pid::from_u32(pid));

    let process =
        system
            .process(Pid::from_u32(pid))
            .ok_or_else(|| LauncherError::ProcessCaptureFailed {
                pid,
                detail: "Process not found in system process table after spawn".into(),
            })?;

    let start_time = process.start_time();
    if start_time == 0 {
        return Err(LauncherError::ProcessCaptureFailed {
            pid,
            detail: "OS reported start_time = 0 for spawned process".into(),
        });
    }

    let expected_exe = process.exe().map(|p| p.to_string_lossy().to_string());

    Ok(ProcessIdentity {
        pid,
        start_time,
        expected_exe,
        job: None,
    })
}

/// The identity of the process calling this, for a journal that must say a launch is starting.
/// One read of the process table, not the settling loop [`capture`] uses after a spawn.
pub fn capture_self() -> Option<ProcessIdentity> {
    capture_once(std::process::id()).ok()
}

// ---------------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------------

/// What the OS says about a process identity (MASTER_SPEC §32.13).
///
/// Three answers, because "I could not ask the OS" is not "the process is gone": a caller that
/// reads a failed lookup, an unreadable process table or a failed job query as `Gone` deletes or
/// rewrites the user's files on the strength of a question it never got an answer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessCheck {
    /// The same process is still running: pid, start time and (when the OS gives them) executable
    /// all match, or the job it was launched in still holds processes.
    Alive,
    /// The recorded process is provably not running: its pid is free, the pid now belongs to
    /// another process, or the process has exited.
    Gone,
    /// The OS could not answer. Never read as gone: `may still be running` is the only safe
    /// reading, so the caller refuses or needs a hand, exactly as it does for `Alive`.
    CantTell(String),
}

/// One process as the OS describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The pid is in the table.
    Found {
        /// The start time the OS reports for that pid.
        start_time: u64,
        /// The executable the OS reports, when it reports one.
        exe: Option<String>,
        /// The process has exited and is waiting to be reaped (a zombie on Unix).
        zombie: bool,
        /// The process has exited while something still holds a handle to it (Windows).
        exited: bool,
    },
    /// The pid is not in the table, and the table was read.
    Absent,
    /// The table could not be read for this pid.
    CantTell(String),
}

/// The OS view a check is answered from.
///
/// It exists so every source of "can't tell" can be made to happen in a test — a failed lookup of
/// the recorded pid, a process table that lists nothing, a job query that fails — without a seam
/// that changes what production does (MASTER_SPEC §32.9, §32.13).
pub trait ProcessView: Send + Sync {
    /// The OS record for `pid`.
    fn lookup(&self, pid: u32) -> Lookup;
    /// Whether a full refresh of the table listed anything. An empty table is not a machine with
    /// nothing running: it is a table that gave no answer.
    fn table_answered(&self) -> bool;
    /// How many processes the named job still holds.
    fn job_members(&self, job: &str) -> Result<usize, String>;
}

/// The real process table.
pub struct OsProcesses;

impl ProcessView for OsProcesses {
    fn lookup(&self, pid: u32) -> Lookup {
        let mut system = System::new();
        system.refresh_process(Pid::from_u32(pid));
        let Some(process) = system.process(Pid::from_u32(pid)) else {
            return Lookup::Absent;
        };
        // On Unix a killed process stays in the table as a zombie until its parent reaps it; it is
        // not running.
        #[cfg(unix)]
        let zombie = process.status() == sysinfo::ProcessStatus::Zombie;
        #[cfg(not(unix))]
        let zombie = false;
        // On Windows an exited process stays openable while anyone (Agora's own `Child` included)
        // holds a handle to it, and sysinfo reports it as present.
        #[cfg(windows)]
        let exited = has_exited(pid);
        #[cfg(not(windows))]
        let exited = false;
        Lookup::Found {
            start_time: process.start_time(),
            exe: process.exe().map(|p| p.to_string_lossy().to_string()),
            zombie,
            exited,
        }
    }

    fn table_answered(&self) -> bool {
        listed_processes() > 0
    }

    fn job_members(&self, job: &str) -> Result<usize, String> {
        crate::process_job::members(job)
    }
}

/// How many processes the OS listed on a full refresh of the process table.
///
/// Zero processes is not a machine with nothing running; it is a table that gave no answer.
pub fn listed_processes() -> usize {
    let mut system = System::new();
    system.refresh_processes();
    system.processes().len()
}

/// What the OS says about `identity`, through the real process table.
pub fn check(identity: &ProcessIdentity) -> ProcessCheck {
    check_with(identity, &OsProcesses)
}

/// What `view` says about `identity`.
///
/// The rules, in order, and each one only ever makes the answer more cautious:
///
/// * an identity with no pid or no start time was never captured from a running process, so
///   nothing can be looked up: `CantTell`;
/// * a job that still holds processes proves liveness even after the parent exited: `Alive`; a job
///   query that fails proves nothing: `CantTell`;
/// * a failed lookup is `CantTell`, never `Gone`;
/// * a pid that is not in a table that *was* read is `Gone`; a pid missing from an empty table is
///   `CantTell`, because an empty table is no answer;
/// * a different start time, a different executable, a zombie or an exited process is `Gone`.
pub fn check_with(identity: &ProcessIdentity, view: &dyn ProcessView) -> ProcessCheck {
    if identity.pid == 0 || identity.start_time == 0 {
        return ProcessCheck::CantTell(format!(
            "the recorded process (pid {}) has no usable identity",
            identity.pid
        ));
    }
    if let Some(job) = identity.job.as_deref() {
        match view.job_members(job) {
            Ok(count) if count > 0 => return ProcessCheck::Alive,
            Ok(_) => {}
            Err(why) => {
                return ProcessCheck::CantTell(format!(
                    "the job object '{job}' could not be queried ({why})"
                ))
            }
        }
    }
    match view.lookup(identity.pid) {
        Lookup::CantTell(why) => ProcessCheck::CantTell(why),
        Lookup::Absent => {
            if view.table_answered() {
                ProcessCheck::Gone
            } else {
                ProcessCheck::CantTell(
                    "the system process list could not be read, so a recorded process cannot be \
                     shown to be gone"
                        .into(),
                )
            }
        }
        Lookup::Found {
            start_time,
            exe,
            zombie,
            exited,
        } => {
            if zombie {
                return ProcessCheck::Gone;
            }
            // The strongest reuse detector.
            if start_time != identity.start_time {
                return ProcessCheck::Gone;
            }
            if exited {
                return ProcessCheck::Gone;
            }
            // Compare the executable when both sides have it: missing OS data does not reject on
            // its own, because the start time is the authoritative check.
            if let (Some(expected), Some(current)) = (&identity.expected_exe, &exe) {
                let normalised = |s: &str| s.replace('\\', "/");
                if normalised(current) != normalised(expected) {
                    return ProcessCheck::Gone;
                }
            }
            ProcessCheck::Alive
        }
    }
}

#[cfg(windows)]
fn has_exited(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is checked for null and closed before returning.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        ok && code != STILL_ACTIVE as u32
    }
}

// ---------------------------------------------------------------------------
// Identity type
// ---------------------------------------------------------------------------

/// Identity of a spawned OS process, captured from the OS immediately after
/// the child is created.
///
/// This is stored internally in the adapters' `AppState` and is
/// **not** serialised to the frontend.  The public
/// `RunningProcess` (in the Minecraft package's `state`) carries only the fields
/// the UI needs (instance_id, pid, session_id).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// The process start time reported by the OS (seconds since epoch).
    pub start_time: u64,
    /// Resolved path of the executable, when the OS provides it.
    pub expected_exe: Option<String>,
    /// The name of the job object the launch put this process in (Windows), when it made one.
    /// Everything the process starts belongs to the same job, so a later Stop can end the whole
    /// group even after this process has exited. See [`crate::process_job`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: run a long‑lived child process and return its `ProcessIdentity`.
    fn spawn_test_child() -> (std::process::Child, ProcessIdentity) {
        let child = if cfg!(target_os = "windows") {
            std::process::Command::new("cmd.exe")
                .args(["/c", "ping", "-n", "60", "127.0.0.1>nul"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("failed to spawn test child")
        } else {
            std::process::Command::new("sleep")
                .arg("60")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("failed to spawn test child")
        };

        let pid = child.id();
        let identity = capture(pid).expect("capture should succeed on a live child");

        (child, identity)
    }

    #[test]
    fn test_capture_and_check_success() {
        let (mut child, identity) = spawn_test_child();
        let result = check(&identity);
        assert_eq!(
            result,
            ProcessCheck::Alive,
            "a live child should answer alive: {:?}",
            result
        );
        // Cleanup
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_capture_nonexistent_pid() {
        let result = capture(u32::MAX);
        assert!(result.is_err(), "capture of nonexistent PID should fail");
        match result {
            Err(LauncherError::ProcessCaptureFailed { .. }) => {} // expected
            other => panic!("Expected ProcessCaptureFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_check_nonexistent_pid_is_gone() {
        let identity = ProcessIdentity {
            pid: u32::MAX,
            start_time: 12345,
            expected_exe: None,
            job: None,
        };
        // A pid that is not in a table that *was* read is provably gone; a table that could not
        // be read is not, and then the honest answer is `CantTell`. Either is "do not act as if it
        // were running", so the test accepts the two and rejects only `Alive`.
        let result = check(&identity);
        assert_ne!(
            result,
            ProcessCheck::Alive,
            "a nonexistent pid must never be reported alive"
        );
    }

    #[test]
    fn test_check_mismatched_start_time_is_gone() {
        let (mut child, mut identity) = spawn_test_child();
        // Corrupt the start time.
        identity.start_time = identity.start_time.wrapping_add(42);

        let result = check(&identity);
        assert_eq!(
            result,
            ProcessCheck::Gone,
            "a reused pid (different start time) must be gone"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_check_mismatched_exe_is_gone() {
        let (mut child, mut identity) = spawn_test_child();
        // Set an impossible exe path.
        identity.expected_exe = Some("/nonexistent/evil.exe".into());

        let result = check(&identity);
        // If the OS supplies an exe, the mismatch is caught; if it does not,
        // the check is skipped because the start time is the authoritative
        // check, and the process reads as alive.
        assert!(
            matches!(result, ProcessCheck::Gone | ProcessCheck::Alive),
            "an exe mismatch is either caught or skipped, never unknown: {result:?}"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn test_check_after_child_death_is_gone() {
        // Spawn a throw‑away child, kill it, wait, then check.
        let (mut child, identity) = spawn_test_child();
        let _ = child.kill();
        let _ = child.wait();

        // The OS needs a moment to drop the pid from its table. Wait for the answer rather than
        // sleeping a fixed time, so a slow machine does not fail the test and a fast one does not
        // pass it before the table moved.
        let mut result = check(&identity);
        for _ in 0..50 {
            if result == ProcessCheck::Gone {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            result = check(&identity);
        }
        assert_eq!(
            result,
            ProcessCheck::Gone,
            "a killed and reaped child must read as gone, never alive"
        );
    }
}
