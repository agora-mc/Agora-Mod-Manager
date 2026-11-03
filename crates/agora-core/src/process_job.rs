//! Agora's own record of "these are the processes I started": a Windows Job Object made at launch.
//!
//! A game is often a chain: `skse64_loader.exe` starts `SkyrimSE.exe` and exits. By the time the
//! player presses Stop, the process Agora spawned is gone and only its child is left, so a pid
//! recorded at launch no longer names anything useful (and a recycled pid could name a stranger).
//! A job object solves both: the launched process is put in a job Agora creates, every process it
//! starts inherits membership, and Stop ends the job. A job can only hold processes that descend
//! from what Agora launched, so nothing else can be ended through it, and its name is a random
//! token rather than a pid, so a reused pid cannot reach it.
//!
//! Agora keeps the job's handle open for as long as the job has processes: a named kernel object
//! stops being reachable by name once its last handle closes, even while processes are still in it.
//! The job does not end the game if Agora exits (no kill-on-close). The cost is that a game left
//! running across an Agora restart is no longer reachable through its job; Stop then falls back to
//! the recorded process and the children it can still be proven to have started. Other platforms
//! have no equivalent here: [`attach`] returns `None` and Stop uses that same fallback.

use std::process::{Child, Command};

/// Spawn `command` so that the process and everything it starts belong to a fresh job, returning
/// the job's name. The job is `None` when the platform has none or the job could not be made;
/// the process runs either way.
///
/// On Windows the process is created suspended, joined to the job, and only then resumed, so it
/// cannot start a child before the job has it.
pub fn spawn_in_job(command: &mut Command) -> std::io::Result<(Child, Option<String>)> {
    #[cfg(windows)]
    {
        windows::spawn_in_job(command)
    }
    #[cfg(not(windows))]
    {
        command.spawn().map(|child| (child, None))
    }
}

/// Put an already-spawned, still-suspended process in a fresh job and return the job's name.
/// For a launch that has its own suspended start-up (the virtual file system's injection).
pub fn attach(child: &Child) -> Option<String> {
    #[cfg(windows)]
    {
        windows::attach(child)
    }
    #[cfg(not(windows))]
    {
        let _ = child;
        None
    }
}

/// How many processes are still in the named job. A job that no longer exists has none.
pub fn active(name: &str) -> usize {
    #[cfg(windows)]
    {
        windows::active(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        0
    }
}

/// End every process in the named job. Returns how many were in it; a job that is gone, or empty,
/// returns 0. Only processes that joined the job at launch (or descend from one) can be in it.
pub fn terminate(name: &str) -> Result<usize, String> {
    #[cfg(windows)]
    {
        windows::terminate(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Ok(0)
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    // Access rights for a job handle (winnt.h); windows-sys does not export them here.
    const JOB_OBJECT_TERMINATE: u32 = 0x0008;
    const JOB_OBJECT_QUERY: u32 = 0x0004;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// A Win32 handle closed on drop.
    struct Handle(HANDLE);

    // SAFETY: a kernel handle is a process-wide token, usable from any thread.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by this guard and closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Jobs this run of Agora made that still had processes the last time they were asked about.
    /// Holding the handle is what keeps the job reachable by name.
    fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, Handle>> {
        static REGISTRY: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, Handle>>,
        > = std::sync::OnceLock::new();
        REGISTRY.get_or_init(Default::default)
    }

    fn forget(name: &str) {
        if let Ok(mut jobs) = registry().lock() {
            jobs.remove(name);
        }
    }

    fn open(name: &str) -> Result<Option<Handle>, String> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let job =
            unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
        if job.is_null() {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            // The job ends when its last process does and no handle is open: a stopped game.
            if code == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            return Err(format!(
                "the game's job object could not be opened (error {code})"
            ));
        }
        Ok(Some(Handle(job)))
    }

    fn active_in(job: &Handle) -> Option<usize> {
        let mut info = std::mem::MaybeUninit::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>::zeroed();
        // SAFETY: `info` is a correctly sized buffer for this information class.
        let ok = unsafe {
            QueryInformationJobObject(
                job.0,
                JobObjectBasicAccountingInformation,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        // SAFETY: the call succeeded, so the buffer was filled.
        Some(unsafe { info.assume_init() }.ActiveProcesses as usize)
    }

    pub(super) fn active(name: &str) -> usize {
        let count = match open(name) {
            Ok(Some(job)) => active_in(&job).unwrap_or(0),
            _ => 0,
        };
        if count == 0 {
            // Nothing left in it: the job can go.
            forget(name);
        }
        count
    }

    pub(super) fn terminate(name: &str) -> Result<usize, String> {
        let Some(job) = open(name)? else {
            return Ok(0);
        };
        let count = active_in(&job).unwrap_or(0);
        if count == 0 {
            return Ok(0);
        }
        // SAFETY: `job` is a live job handle opened with terminate access.
        if unsafe { TerminateJobObject(job.0, 1) } == 0 {
            // SAFETY: reads the calling thread's last error.
            let code = unsafe { GetLastError() };
            return Err(format!(
                "the game's processes could not be ended (error {code})"
            ));
        }
        // The handle stays registered until `active` sees the job empty, so a stop that is still
        // waiting for the processes to go can keep asking.
        Ok(count)
    }

    pub(super) fn attach(child: &Child) -> Option<String> {
        let name = format!("Local\\agora-game-{}", uuid::Uuid::new_v4().simple());
        let wide_name = wide(&name);
        // SAFETY: a default-security job with a NUL-terminated name.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), wide_name.as_ptr()) };
        if job.is_null() {
            return None;
        }
        let job = Handle(job);
        // A job that already existed is not ours: refuse it rather than join someone else's.
        // SAFETY: reads the calling thread's last error.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return None;
        }
        // A launcher that asks to break away from its job (some launchers and browser engines
        // do) must still be able to start; it simply is not in the job afterwards. Without this
        // flag its CreateProcess would fail, and the game with it. No other limit is set, and
        // in particular nothing ends when Agora exits.
        // SAFETY: the structure is zeroed apart from the one flag, and its size is passed.
        let limits_set = unsafe {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if limits_set == 0 {
            return None;
        }
        // SAFETY: both are live handles; the child's handle is owned by `child`.
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) } == 0 {
            return None;
        }
        // The handle stays open (see the module docs); the job does not have kill-on-close set,
        // so the game outlives Agora, as it always has.
        let mut jobs = registry().lock().ok()?;
        jobs.insert(name.clone(), job);
        Some(name)
    }

    /// Resume every thread of a process created suspended.
    fn resume(child: &Child) -> Result<(), String> {
        type NtResumeProcess = unsafe extern "system" fn(HANDLE) -> i32;
        // SAFETY: ntdll is always loaded; the export is the documented-by-use `NtResumeProcess`
        // and the handle is the live process handle `child` owns.
        unsafe {
            let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
            if ntdll.is_null() {
                return Err("ntdll.dll is not loaded".to_string());
            }
            let Some(proc) = GetProcAddress(ntdll, c"NtResumeProcess".as_ptr() as *const u8) else {
                return Err("NtResumeProcess was not found".to_string());
            };
            let nt_resume_process: NtResumeProcess = std::mem::transmute(proc);
            let status = nt_resume_process(child.as_raw_handle() as HANDLE);
            if status < 0 {
                return Err(format!(
                    "the process could not be resumed (status {status:#x})"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn spawn_in_job(command: &mut Command) -> std::io::Result<(Child, Option<String>)> {
        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let job = attach(&child);
        if let Err(reason) = resume(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other(reason));
        }
        Ok((child, job))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn sleeper() -> Command {
        let mut command = Command::new("cmd.exe");
        command.args(["/c", "ping", "-n", "120", "127.0.0.1>nul"]);
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    #[test]
    fn the_job_holds_the_process_and_ends_it() {
        let (mut child, job) = spawn_in_job(&mut sleeper()).expect("spawned");
        let name = job.expect("a job");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(active(&name) >= 1, "the process is not in its job");
        assert!(terminate(&name).expect("terminated") >= 1);
        let status = child.wait().expect("the child ended");
        assert!(!status.success());
        assert_eq!(terminate(&name).expect("nothing left"), 0);
    }

    #[test]
    fn an_unknown_job_name_holds_nothing_and_ends_nothing() {
        assert_eq!(active("Local\\agora-game-does-not-exist"), 0);
        assert_eq!(terminate("Local\\agora-game-does-not-exist"), Ok(0));
    }
}
