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

/// The exit code Agora's own Stop ends a game with (`TerminateJobObject`, or the fallback
/// `TerminateProcess`). Exit code 1 is what a Crash Logger crash exits with, so a Stop must not
/// share it. The code follows the virtual file system's `0xA6F50001` (a refusal to run
/// unprotected): same family, next number. The `stop_requested` flag on the session is the
/// primary record of a Stop; this code is the second witness, and what tells an Agora Stop from a
/// kill in Task Manager (which exits 1).
pub const STOP_EXIT_CODE: u32 = 0xA6F5_0002;

/// A game process Agora holds open: its handle keeps the process object alive, so the pid cannot
/// be reused by a stranger while Agora waits, and the exit code can be read when it ends.
/// Only a process proven to be in the launch's own job is ever held.
#[cfg(windows)]
pub struct HeldProcess {
    handle: windows::Handle,
    pid: u32,
    image: String,
}

/// Holding a process by handle is Windows-only; elsewhere none is ever held.
#[cfg(not(windows))]
pub struct HeldProcess(std::convert::Infallible);

impl HeldProcess {
    /// The process id, valid for as long as this value is held.
    pub fn pid(&self) -> u32 {
        #[cfg(windows)]
        {
            self.pid
        }
        #[cfg(not(windows))]
        {
            match self.0 {}
        }
    }

    /// The process's image file name, such as `SkyrimSE.exe`.
    pub fn image(&self) -> &str {
        #[cfg(windows)]
        {
            &self.image
        }
        #[cfg(not(windows))]
        {
            match self.0 {}
        }
    }

    /// The exit code once the process has ended; `None` while it still runs. A process that
    /// really exits with code 259 (`STILL_ACTIVE`) is told apart by waiting on the handle.
    pub fn exit_code(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            windows::exit_code(&self.handle)
        }
        #[cfg(not(windows))]
        {
            match self.0 {}
        }
    }
}

/// Hold every process in the named job whose image name is one of `images` (compared without
/// regard to case) and whose pid is not in `already`. A process is held only if it is proven to
/// be in the job at the moment it is opened, so a pid that was recycled between listing and
/// opening is refused. Returns nothing when the job is gone or the platform has no jobs.
pub fn hold_game_processes(job: &str, images: &[String], already: &[u32]) -> Vec<HeldProcess> {
    #[cfg(windows)]
    {
        windows::hold_game_processes(job, images, already)
    }
    #[cfg(not(windows))]
    {
        let _ = (job, images, already);
        Vec::new()
    }
}

/// End one process by pid with [`STOP_EXIT_CODE`], for a Stop that has no job to end. Returns
/// whether the process was signalled. Elsewhere there is no such code, so it returns `false` and
/// the caller kills the process its own way.
pub fn terminate_process_for_stop(pid: u32) -> bool {
    #[cfg(windows)]
    {
        windows::terminate_process(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

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
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, WAIT_OBJECT_0};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, OpenJobObjectW,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject, CREATE_SUSPENDED, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus, OVERLAPPED,
    };

    // Access rights for a job handle (winnt.h); windows-sys does not export them here.
    const JOB_OBJECT_TERMINATE: u32 = 0x0008;
    const JOB_OBJECT_QUERY: u32 = 0x0004;

    // Job notification messages (`Win32_System_SystemServices`); the numbers are stable.
    const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
    const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;

    /// A message no job sends, used to tell the holder thread to stop.
    const QUIT_KEY: usize = usize::MAX;

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// A Win32 handle closed on drop.
    pub(super) struct Handle(HANDLE);

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
    /// A process a job notification opened before the watcher's next poll. Holding the handle is
    /// the only way its exit code survives a process that ends immediately: Windows destroys the
    /// process object once nothing holds it, and no later call can recover the code.
    struct PreHeld {
        handle: Handle,
        pid: u32,
        image: Option<String>,
    }

    /// The handles that keep a job reachable by name, plus the thread that catches new processes
    /// the moment the job reports them.
    struct JobState {
        _job: Handle,
        port: Handle,
        held: std::sync::Arc<std::sync::Mutex<Vec<PreHeld>>>,
    }

    fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, JobState>> {
        static REGISTRY: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, JobState>>,
        > = std::sync::OnceLock::new();
        REGISTRY.get_or_init(Default::default)
    }

    fn forget(name: &str) {
        if let Ok(mut jobs) = registry().lock() {
            if let Some(state) = jobs.remove(name) {
                // Wake the holder thread; it may be blocked with no more messages coming.
                // SAFETY: the port is live until `state` is dropped at the end of this block.
                unsafe {
                    PostQueuedCompletionStatus(state.port.0, 0, QUIT_KEY, std::ptr::null());
                }
            }
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
        if unsafe { TerminateJobObject(job.0, super::STOP_EXIT_CODE) } == 0 {
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

    /// The pids currently in the job.
    fn job_pids(job: &Handle) -> Vec<u32> {
        let mut capacity = 64usize;
        loop {
            // The header is two u32s; the ids that follow are pointer-sized. A u64 buffer is
            // aligned for both and at least as large as needed.
            let mut buffer = vec![0u64; 2 + capacity];
            // SAFETY: `buffer` is writable for the stated number of bytes.
            let ok = unsafe {
                QueryInformationJobObject(
                    job.0,
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * std::mem::size_of::<u64>()) as u32,
                    std::ptr::null_mut(),
                )
            };
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            if ok == 0 {
                // SAFETY: reads the calling thread's last error.
                let code = unsafe { GetLastError() };
                if code == ERROR_MORE_DATA && capacity < 1 << 16 {
                    // SAFETY: the call wrote the header before failing for size.
                    let needed = unsafe { (*list).NumberOfAssignedProcesses } as usize;
                    capacity = needed.max(capacity * 2);
                    continue;
                }
                return Vec::new();
            }
            // SAFETY: the call succeeded, so the header and `NumberOfProcessIdsInList` ids that
            // follow it are initialised and inside `buffer`.
            return unsafe {
                let count = (*list).NumberOfProcessIdsInList as usize;
                let first = std::ptr::addr_of!((*list).ProcessIdList).cast::<usize>();
                (0..count).map(|i| *first.add(i) as u32).collect()
            };
        }
    }

    /// The image file name of an open process.
    fn image_name(process: &Handle) -> Option<String> {
        let mut buffer = vec![0u16; 1024];
        let mut len = buffer.len() as u32;
        // SAFETY: `buffer` holds `len` u16s; the call writes at most that many and sets `len`.
        let ok = unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut len) };
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }

    /// Catch every process the job reports the moment it is created. Without this a game can be
    /// born and gone between two watcher polls, and Windows leaves no way to ask for its code.
    fn hold_new_processes(port: usize, held: std::sync::Arc<std::sync::Mutex<Vec<PreHeld>>>) {
        std::thread::spawn(move || {
            let port = port as HANDLE;
            loop {
                let mut bytes = 0u32;
                let mut key = 0usize;
                let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
                // SAFETY: the port is kept open by the registry entry for as long as this thread
                // can receive messages; the out-pointers are valid for the call.
                let ok = unsafe {
                    GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut overlapped, INFINITE)
                };
                if key == QUIT_KEY {
                    break;
                }
                if ok == 0 && overlapped.is_null() {
                    break;
                }
                match bytes {
                    JOB_OBJECT_MSG_NEW_PROCESS => {
                        let pid = overlapped as usize as u32;
                        // SAFETY: a plain open by pid; a failure (already ended) returns null.
                        let raw = unsafe {
                            OpenProcess(
                                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                                0,
                                pid,
                            )
                        };
                        if !raw.is_null() {
                            let handle = Handle(raw);
                            let image = image_name(&handle);
                            if let Ok(mut list) = held.lock() {
                                list.push(PreHeld { handle, pid, image });
                            }
                        }
                    }
                    JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => break,
                    _ => {}
                }
            }
        });
    }

    pub(super) fn hold_game_processes(
        job: &str,
        images: &[String],
        already: &[u32],
    ) -> Vec<super::HeldProcess> {
        let wanted: Vec<String> = images.iter().map(|i| i.to_lowercase()).collect();
        let mut taken: Vec<u32> = already.to_vec();
        let mut held = Vec::new();

        // Processes the notification thread caught before they could end.
        let shared = registry()
            .lock()
            .ok()
            .and_then(|jobs| jobs.get(job).map(|state| state.held.clone()));
        if let Some(shared) = shared {
            let waiting: Vec<PreHeld> = match shared.lock() {
                Ok(mut list) => list.drain(..).collect(),
                Err(_) => Vec::new(),
            };
            for pre in waiting {
                if taken.contains(&pre.pid) {
                    continue;
                }
                let Some(image) = pre.image else {
                    continue;
                };
                if wanted.contains(&image.to_lowercase()) {
                    taken.push(pre.pid);
                    held.push(super::HeldProcess {
                        handle: pre.handle,
                        pid: pre.pid,
                        image,
                    });
                }
            }
        }

        let Ok(Some(job)) = open(job) else {
            return held;
        };
        for pid in job_pids(&job) {
            if taken.contains(&pid) {
                continue;
            }
            // SAFETY: a plain open by pid; a failure (the process just ended) returns null.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if raw.is_null() {
                continue;
            }
            let handle = Handle(raw);
            // The pid came from the job's list a moment ago, but a process can end and its pid be
            // reused in between. Ask the kernel whether this handle's process is in the job.
            let mut inside = 0;
            // SAFETY: both handles are live; `inside` receives a BOOL.
            if unsafe { IsProcessInJob(handle.0, job.0, &mut inside) } == 0 || inside == 0 {
                continue;
            }
            let Some(image) = image_name(&handle) else {
                continue;
            };
            if wanted.contains(&image.to_lowercase()) {
                taken.push(pid);
                held.push(super::HeldProcess { handle, pid, image });
            }
        }
        held
    }

    pub(super) fn exit_code(process: &Handle) -> Option<u32> {
        // SAFETY: a live process handle opened with SYNCHRONIZE; a zero wait only polls.
        if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: a live process handle opened with query access; `code` receives the value.
        if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
            return None;
        }
        Some(code)
    }

    pub(super) fn terminate_process(pid: u32) -> bool {
        // SAFETY: a plain open by pid with terminate access.
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if raw.is_null() {
            return false;
        }
        let handle = Handle(raw);
        // SAFETY: a live process handle with terminate access.
        unsafe { TerminateProcess(handle.0, super::STOP_EXIT_CODE) != 0 }
    }

    pub(super) fn attach(child: &Child) -> Option<String> {
        let name = format!("Local\\agora-session-{}", uuid::Uuid::new_v4().simple());
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
        // A completion port tells us about each new process the moment the job gets it, so a game
        // that starts and ends between two polls is still held by its handle.
        // SAFETY: `INVALID_HANDLE_VALUE` creates a new port; no handle is needed for it here.
        let port_raw =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };
        if port_raw.is_null() {
            return None;
        }
        let port = Handle(port_raw);
        let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: std::ptr::null_mut(),
            CompletionPort: port.0,
        };
        // SAFETY: the structure and its size match the information class.
        let associated = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectAssociateCompletionPortInformation,
                (&association as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        };
        if associated == 0 {
            return None;
        }
        // SAFETY: both are live handles; the child's handle is owned by `child`.
        if unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) } == 0 {
            return None;
        }
        // The handle stays open (see the module docs); the job does not have kill-on-close set,
        // so the game outlives Agora, as it always has.
        let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let port_value = port.0 as usize;
        hold_new_processes(port_value, held.clone());
        let mut jobs = registry().lock().ok()?;
        jobs.insert(
            name.clone(),
            JobState {
                _job: job,
                port,
                held,
            },
        );
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
        assert_eq!(active("Local\\agora-session-does-not-exist"), 0);
        assert_eq!(terminate("Local\\agora-session-does-not-exist"), Ok(0));
    }
}
