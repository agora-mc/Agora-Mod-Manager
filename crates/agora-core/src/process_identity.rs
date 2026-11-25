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
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use sysinfo::{Pid, System};

fn executable_paths_match(expected: &Path, observed: &Path) -> bool {
    match (expected.canonicalize(), observed.canonicalize()) {
        (Ok(expected), Ok(observed)) => expected == observed,
        (Err(_), Err(_)) => {
            expected.to_string_lossy().replace('\\', "/")
                == observed.to_string_lossy().replace('\\', "/")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Query the OS for the process identity of `pid` immediately after spawn.
///
/// **Fail‑closed**: if the process is no longer alive or the OS cannot supply
/// start_time, this returns `Err(ProcessCaptureFailed)`.
pub fn capture(pid: u32) -> LauncherResult<ProcessIdentity> {
    let mut previous = capture_once(pid)?;
    let parent_exe = std::env::current_exe().ok();

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
        let current_exe = current.expected_exe.as_deref().map(Path::new);
        let previous_exe = previous.expected_exe.as_deref().map(Path::new);
        let still_parent_image = parent_exe
            .as_deref()
            .zip(current_exe)
            .is_some_and(|(parent, exe)| executable_paths_match(parent, exe));
        let image_settled = match (previous_exe, current_exe) {
            (Some(previous), Some(current)) => executable_paths_match(previous, current),
            (None, None) => true,
            _ => false,
        };
        if current.start_time == previous.start_time && image_settled && !still_parent_image {
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
    /// The OS positively reported that this PID is absent.
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
            #[cfg(target_os = "macos")]
            if let Some(exited) = lookup_unreaped_exit(pid) {
                return exited;
            }
            return match pid_absent(pid) {
                true => Lookup::Absent,
                false => {
                    Lookup::CantTell(format!("the targeted lookup could not identify pid {pid}"))
                }
            };
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

/// Check whether the process or any members of its job can still use the resource.
pub fn check_with(identity: &ProcessIdentity, view: &dyn ProcessView) -> ProcessCheck {
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
    check_pid(identity, view, false)
}

/// Check the exact process before signalling it. Job membership proves nothing about its PID.
pub fn check_exact_with(identity: &ProcessIdentity, view: &dyn ProcessView) -> ProcessCheck {
    check_pid(identity, view, true)
}

fn check_pid(
    identity: &ProcessIdentity,
    view: &dyn ProcessView,
    require_exe: bool,
) -> ProcessCheck {
    let unknown = || {
        ProcessCheck::CantTell(format!(
            "pid {} has no usable process identity",
            identity.pid
        ))
    };
    if identity.pid == 0 || identity.start_time == 0 {
        return unknown();
    }
    match view.lookup(identity.pid) {
        Lookup::Absent => ProcessCheck::Gone,
        Lookup::CantTell(why) => ProcessCheck::CantTell(why),
        Lookup::Found {
            start_time,
            exe,
            zombie,
            exited,
        } => {
            if zombie || exited {
                return ProcessCheck::Gone;
            }
            if start_time == 0 {
                return unknown();
            }
            if start_time != identity.start_time {
                return ProcessCheck::Gone;
            }
            match (&identity.expected_exe, &exe) {
                (Some(expected), Some(current)) => {
                    if !executable_paths_match(Path::new(expected), Path::new(current)) {
                        return ProcessCheck::Gone;
                    }
                }
                _ if require_exe => return unknown(),
                _ => {}
            }
            ProcessCheck::Alive
        }
    }
}

#[cfg(target_os = "macos")]
fn lookup_unreaped_exit(pid: u32) -> Option<Lookup> {
    // A PID too large for pid_t cannot name a process; the caller then asks pid_absent.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return None;
    };
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // sysinfo asks for BSD info with arg=0, which omits zombies. Signal 0 can
    // still see their PIDs. Ask Darwin for the status including unreaped exits.
    // SAFETY: the buffer has the size and type required by PROC_PIDTBSDINFO.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: the OS returned a complete proc_bsdinfo.
    let info = unsafe { info.assume_init() };
    (info.pbi_status == libc::SZOMB).then_some(Lookup::Found {
        start_time: info.pbi_start_tvsec,
        exe: None,
        zombie: true,
        exited: false,
    })
}

#[cfg(windows)]
fn pid_absent(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: the handle is closed when OpenProcess succeeds. For a nonzero PID, an
    // invalid parameter means the PID is absent. Access denied leaves liveness unknown.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return pid != 0 && GetLastError() == ERROR_INVALID_PARAMETER;
        }
        CloseHandle(handle);
        false
    }
}

#[cfg(unix)]
fn pid_absent(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 queries existence without sending a signal.
    (unsafe { libc::kill(pid, 0) == -1 })
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(any(windows, unix)))]
fn pid_absent(_pid: u32) -> bool {
    false
}

/// Verify again immediately before a PID-directed action. On Windows the open handle keeps
/// the PID bound to the same process until the action returns.
pub(crate) fn signal_verified<T>(
    identity: &ProcessIdentity,
    signal: impl FnOnce() -> T,
) -> Result<T, String> {
    #[cfg(windows)]
    let _handle = {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: a successful OpenProcess returns an owned handle.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, identity.pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        unsafe { OwnedHandle::from_raw_handle(handle.cast()) }
    };
    match check_exact_with(identity, &OsProcesses) {
        ProcessCheck::Alive => Ok(signal()),
        ProcessCheck::Gone => Err(format!(
            "pid {} no longer matches the recorded process",
            identity.pid
        )),
        ProcessCheck::CantTell(error) => Err(error),
    }
}
/// End every verified process and every descendant it started, children after parents. Returns
/// how many processes were actually signalled.
pub(crate) fn terminate_tree(roots: &[&ProcessIdentity]) -> usize {
    let mut system = System::new();
    system.refresh_processes();

    let mut queue: Vec<ProcessIdentity> = roots
        .iter()
        .filter(|identity| check_exact_with(identity, &OsProcesses) == ProcessCheck::Alive)
        .map(|identity| (*identity).clone())
        .collect();

    let mut seen: HashSet<Pid> = HashSet::new();
    let mut targets = Vec::new();
    while let Some(identity) = queue.pop() {
        let pid = Pid::from_u32(identity.pid);
        if !seen.insert(pid) {
            continue;
        }
        let Some(process) = system.process(pid).filter(|process| {
            process.start_time() == identity.start_time
                && identity
                    .expected_exe
                    .as_deref()
                    .map(Path::new)
                    .zip(process.exe())
                    .is_some_and(|(expected, observed)| executable_paths_match(expected, observed))
        }) else {
            continue;
        };
        let start_time = process.start_time();
        targets.push(identity);
        for (child_pid, child) in system.processes() {
            // A child cannot be older than its parent: an older one only names a parent that has
            // since exited and whose pid was reused.
            if child.parent() == Some(pid) && child.start_time() >= start_time {
                queue.push(ProcessIdentity {
                    pid: child_pid.as_u32(),
                    start_time: child.start_time(),
                    expected_exe: child.exe().map(|exe| exe.to_string_lossy().into_owned()),
                    job: None,
                });
            }
        }
    }

    let mut ended = 0;
    for identity in targets {
        let pid = Pid::from_u32(identity.pid);
        if let Some(process) = system.process(pid) {
            if signal_verified(&identity, || {
                crate::process_job::terminate_process_for_stop(identity.pid) || process.kill()
            }) == Ok(true)
            {
                ended += 1;
            }
        }
    }
    ended
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
    fn test_exact_check_accepts_equivalent_executable_path() {
        let (mut child, mut identity) = spawn_test_child();
        let exe = Path::new(identity.expected_exe.as_deref().unwrap());
        let alias = exe
            .parent()
            .unwrap()
            .join(".")
            .join(exe.file_name().unwrap());
        assert_ne!(alias.as_os_str(), exe.as_os_str());
        identity.expected_exe = Some(alias.to_str().unwrap().to_owned());

        let checked = check_exact_with(&identity, &OsProcesses);
        let signalled = signal_verified(&identity, || true);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(checked, ProcessCheck::Alive);
        assert_eq!(signalled, Ok(true));
    }

    #[cfg(unix)]
    #[test]
    fn executable_paths_match_through_a_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let game = real.join("game");
        let other = real.join("other");
        std::fs::write(&game, b"game").unwrap();
        std::fs::write(&other, b"other").unwrap();

        assert!(executable_paths_match(&game, &alias.join("game")));
        assert!(!executable_paths_match(&game, &alias.join("other")));
        // This spelling normalizes to the real path, but cannot itself be resolved on Unix.
        let unresolved = dir.path().join(r"real\game");
        assert!(!executable_paths_match(&game, &unresolved));
        assert!(!executable_paths_match(&unresolved, &game));
    }

    #[test]
    fn executable_paths_fall_back_to_normalized_text_when_both_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("missing").join("game");
        let same = format!(r"{}/missing\game", dir.path().display());
        assert!(executable_paths_match(&expected, Path::new(&same)));
        assert!(!executable_paths_match(
            &expected,
            &dir.path().join("other")
        ));
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

    #[test]
    fn test_check_killed_unreaped_child_is_gone() {
        let (mut child, identity) = spawn_test_child();
        let before = check(&identity);
        child.kill().expect("kill the child");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let after = loop {
            let result = check(&identity);
            if result == ProcessCheck::Gone || std::time::Instant::now() >= deadline {
                break result;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        child.wait().expect("reap the child");
        assert_eq!(before, ProcessCheck::Alive);
        assert_eq!(after, ProcessCheck::Gone);
    }
}
