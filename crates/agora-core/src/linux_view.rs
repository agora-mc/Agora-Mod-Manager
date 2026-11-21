//! The Linux game view (MASTER_SPEC §26.5).
//!
//! The virtual-file-system rung on Linux is an overlay mounted over the game folder
//! inside a private user and mount namespace. The lower layer is the instance's own
//! link deployment with the writable layer left out (the same farm Windows builds for
//! the VFS rung), the upper layer is the instance's writable layer, and the work
//! folder is a sibling of the upper layer on the same file system. Writes copy up
//! into the writable layer, so the base, the generated layers and the content store
//! are never the write target.
//!
//! Kernel overlayfs with `userxattr` is tried first, by a real mount (never by kernel
//! version); `fuse-overlayfs` is the second mechanism when it is on `PATH`. When
//! neither can mount, the result is a [`ViewOutcome::SteppedDown`] naming the reason
//! and the rung below through [`DeployMode::next_fallback`] — the same chain Windows
//! uses — so the caller runs from links or copies and says so. Nothing is ever run
//! unprotected: a refused view runs no command.
//!
//! This is a platform primitive: the trait is here, the Linux implementation sits
//! behind `#[cfg(target_os = "linux")]`, and every other platform gets a stub that
//! reports the view as unavailable rather than pretending it ran. No game knowledge
//! is in this module.

use std::path::PathBuf;
use std::process::ExitStatus;

use agora_game_api::GameDefinition;

use crate::ctx::Ctx;
use crate::game_deploy::DeployMode;

/// The Linux tests fail loudly with the reason user namespaces are unavailable unless
/// this marks the host as known-restricted (Ubuntu runners restrict unprivileged user
/// namespaces through AppArmor). Test hook; the product never reads it.
pub const KNOWN_RESTRICTED_ENV: &str = "AGORA_LINUX_VIEW_KNOWN_RESTRICTED";

/// When set to a non-empty value, the overlay mount is refused so the step-down path
/// can be exercised on any Linux host. Test hook; nothing ships that sets it.
pub const REFUSE_OVERLAY_ENV: &str = "AGORA_LINUX_VIEW_REFUSE_OVERLAY";

/// When set to a non-empty value, the kernel overlay mount is skipped as if the kernel refused
/// it, so the `fuse-overlayfs` path can be exercised on a host that has both. Test hook;
/// nothing ships that sets it.
pub const REFUSE_KERNEL_OVERLAY_ENV: &str = "AGORA_LINUX_VIEW_REFUSE_KERNEL_OVERLAY";

/// The command to run inside the namespace. `environment` of `None` inherits the
/// caller's environment, which is what a game wrapper wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub environment: Option<Vec<(String, String)>>,
    pub working_directory: Option<PathBuf>,
}

/// One per-user file mapped for the length of the view: the instance's copy is
/// bind-mounted over the real path, inside the namespace only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerUserMount {
    pub real_path: PathBuf,
    pub instance_copy: PathBuf,
}

/// What to show and what to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRequest {
    pub instance_id: String,
    /// The game folder the overlay is mounted over, inside the namespace.
    pub target: PathBuf,
    pub command: CommandSpec,
    pub per_user_files: Vec<PerUserMount>,
}

/// Which overlay implementation mounted the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayMechanism {
    Kernel,
    Fuse,
}

impl OverlayMechanism {
    pub fn as_str(self) -> &'static str {
        match self {
            OverlayMechanism::Kernel => "kernel overlayfs with userxattr",
            OverlayMechanism::Fuse => "fuse-overlayfs",
        }
    }
}

/// A command that ran inside the view. Its writes are in the instance's writable layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRun {
    pub mechanism: OverlayMechanism,
    pub status: ExitStatus,
    /// The process id of the command Steam asked Agora to run. It is the command's own process,
    /// which for Steam is the launcher that waits for the game, not the game itself.
    pub pid: u32,
}

/// The view could not be mounted, and nothing ran. The caller uses `next` and says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDown {
    pub reason: String,
    pub next: DeployMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewOutcome {
    Ran(ViewRun),
    SteppedDown(StepDown),
}

#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error("the target folder '{}' does not exist or is not a folder", .0.display())]
    InvalidTarget(PathBuf),
    #[error("the command has no program")]
    EmptyProgram,
    #[error("per-user file '{}' is missing or unreadable", .0.display())]
    UnreadablePerUserFile(PathBuf),
    #[error("the instance '{0}' has no link deployment to show")]
    NoDeployment(String),
    #[error("cannot build the link deployment: {0}")]
    Deploy(#[from] crate::game_deploy::DeployError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("the view could not be prepared: {0}")]
    Setup(String),
}

impl ViewError {
    /// True when the step-down env hook caused it, so a test can tell the two apart.
    pub fn is_test_refusal(&self) -> bool {
        matches!(self, ViewError::Setup(message) if message.contains(REFUSE_OVERLAY_ENV))
    }
}

/// The Linux view, as a platform primitive the deployment code calls.
pub trait LinuxView: Send + Sync {
    /// Build the instance's link deployment and run `request.command` with an overlay
    /// mounted over `request.target`, inside a private user and mount namespace. The
    /// call waits for the command and returns its exit status.
    fn view_and_run(
        &self,
        ctx: &Ctx,
        definition: &GameDefinition,
        request: &ViewRequest,
    ) -> Result<ViewOutcome, ViewError>;
}

/// The implementation for the platform this build is for.
pub fn native() -> &'static dyn LinuxView {
    #[cfg(target_os = "linux")]
    {
        &linux_impl::NATIVE
    }
    #[cfg(not(target_os = "linux"))]
    {
        &unsupported::UNSUPPORTED
    }
}

/// The rung a Linux view steps down to when it cannot mount. The same chain Windows
/// uses: the virtual rung steps down to linked files. Direct install is never offered.
pub fn step_down_mode() -> DeployMode {
    DeployMode::Virtual
        .next_fallback()
        .unwrap_or(DeployMode::Copies)
}

#[cfg(not(target_os = "linux"))]
mod unsupported {
    use super::*;

    pub(super) struct Unsupported;

    impl LinuxView for Unsupported {
        fn view_and_run(
            &self,
            _ctx: &Ctx,
            _definition: &GameDefinition,
            _request: &ViewRequest,
        ) -> Result<ViewOutcome, ViewError> {
            Ok(ViewOutcome::SteppedDown(StepDown {
                reason: "the Linux game view is only available on Linux".to_string(),
                next: step_down_mode(),
            }))
        }
    }

    pub(super) static UNSUPPORTED: Unsupported = Unsupported;
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::ExitStatus;

    use nix::fcntl::OFlag;
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{fork, getegid, geteuid, pipe2, ForkResult, Pid};

    use super::*;
    use nix::libc as c;

    pub(super) struct Native;

    impl LinuxView for Native {
        fn view_and_run(
            &self,
            ctx: &Ctx,
            definition: &GameDefinition,
            request: &ViewRequest,
        ) -> Result<ViewOutcome, ViewError> {
            validate(request)?;
            // The lower layer is the instance's farm with the writable layer left out.
            // `DeployMode::Virtual` is exactly that plan: the writable layer is shown
            // on top by the view instead of being placed in the farm.
            //
            // The wrapper records its session before this call so a crash during the
            // view is diagnosable, so the deploy must allow the session it opened
            // itself; `wrap` already refused while any other session was live. The
            // instance lock is still taken.
            crate::game_deploy::deploy_while_running(
                ctx,
                &request.instance_id,
                definition,
                DeployMode::Virtual,
                true,
            )?;
            let lower = crate::game_deploy::deployment_dir(ctx, &request.instance_id)?
                .ok_or_else(|| ViewError::NoDeployment(request.instance_id.clone()))?;
            let instance_dir = ctx
                .paths
                .instance_dir(&request.instance_id)
                .map_err(|e| ViewError::Setup(e.to_string()))?;
            let upper = instance_dir.join("writable");
            // One work folder per wrapper process. Wiping a shared one would break a concurrent
            // tool run or a second game session that still has it mounted.
            let work = instance_dir.join(format!("view-work.{}", std::process::id()));
            std::fs::create_dir_all(&upper)?;
            // Overlay needs an empty work folder. One left by a stopped view is not trusted
            // (a half-written copy-up is not something to carry into the next run). Never follow
            // a link here: `remove_dir_all` through it would delete a folder Agora does not own.
            if let Ok(meta) = std::fs::symlink_metadata(&work) {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    std::fs::remove_file(&work)?;
                } else {
                    std::fs::remove_dir_all(&work)?;
                }
            }
            std::fs::create_dir_all(&work)?;

            let refuse =
                std::env::var_os(REFUSE_OVERLAY_ENV).is_some_and(|value| !value.is_empty());
            let refuse_kernel =
                std::env::var_os(REFUSE_KERNEL_OVERLAY_ENV).is_some_and(|value| !value.is_empty());
            let prepared = Prepared::new(request, &lower, &upper, &work, refuse, refuse_kernel)?;
            run_prepared(prepared)
        }
    }

    pub(super) static NATIVE: Native = Native;

    fn validate(request: &ViewRequest) -> Result<(), ViewError> {
        if request.command.program.as_os_str().is_empty() {
            return Err(ViewError::EmptyProgram);
        }
        let target = std::fs::metadata(&request.target)
            .map_err(|_| ViewError::InvalidTarget(request.target.clone()))?;
        if !target.is_dir() {
            return Err(ViewError::InvalidTarget(request.target.clone()));
        }
        for file in &request.per_user_files {
            // The mount binds these paths into the namespace. A link could point anywhere, so
            // inspect the entry itself, never what it resolves to.
            if !plain_file(&file.instance_copy) {
                return Err(ViewError::UnreadablePerUserFile(file.instance_copy.clone()));
            }
            if !plain_file(&file.real_path) {
                return Err(ViewError::UnreadablePerUserFile(file.real_path.clone()));
            }
        }
        Ok(())
    }

    /// A regular file and not a symlink, checked without following the entry.
    fn plain_file(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
    }

    /// Everything the child needs, allocated before `fork`, so the child only makes
    /// syscalls until it execs. No `format!`, `String`, `Vec` or std file API may run in the
    /// child: the process may have other threads holding a lock the child would inherit.
    struct Prepared {
        refuse: bool,
        refuse_kernel: bool,
        proc_setgroups: CString,
        proc_uid_map: CString,
        proc_gid_map: CString,
        setgroups_deny: CString,
        uid_map: CString,
        gid_map: CString,
        root: CString,
        overlay_source: CString,
        overlay_fs: CString,
        kernel_data: CString,
        target: CString,
        program: CString,
        /// The `CString` owners; `argv_ptr` points into these buffers and must not outlive them.
        #[allow(dead_code)]
        argv: Vec<CString>,
        argv_ptr: Vec<*const c::c_char>,
        /// The `CString` owners; `envp_ptr` points into these buffers and must not outlive them.
        #[allow(dead_code)]
        envp: Vec<CString>,
        envp_ptr: Vec<*const c::c_char>,
        working_directory: Option<CString>,
        per_user: Vec<(CString, CString)>,
        fuse_program: Option<CString>,
        fuse_foreground: CString,
        fuse_dash_o: CString,
        fuse_data: CString,
        fuse_mountinfo_probe: Vec<u8>,
        fuse_message_prefix: Vec<u8>,
        kernel_message: Vec<u8>,
    }

    impl Prepared {
        fn new(
            request: &ViewRequest,
            lower: &Path,
            upper: &Path,
            work: &Path,
            refuse: bool,
            refuse_kernel: bool,
        ) -> Result<Self, ViewError> {
            let kernel_data = format!(
                "lowerdir={},upperdir={},workdir={},userxattr",
                escaped(lower)?,
                escaped(upper)?,
                escaped(work)?
            );
            let fuse_data = format!(
                "lowerdir={},upperdir={},workdir={}",
                escaped(lower)?,
                escaped(upper)?,
                escaped(work)?
            );
            let uid = geteuid().as_raw();
            let gid = getegid().as_raw();
            let mut argv = Vec::with_capacity(request.command.args.len() + 1);
            argv.push(path_cstring(&request.command.program)?);
            for arg in &request.command.args {
                argv.push(
                    CString::new(arg.as_str()).map_err(|_| {
                        ViewError::Setup("a command argument contains a NUL".into())
                    })?,
                );
            }
            let envp = match &request.command.environment {
                Some(environment) => environment
                    .iter()
                    .map(|(key, value)| {
                        CString::new(format!("{key}={value}")).map_err(|_| {
                            ViewError::Setup("an environment value contains a NUL".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                None => std::env::vars_os()
                    .map(|(key, value)| {
                        let mut bytes = key.as_bytes().to_vec();
                        bytes.push(b'=');
                        bytes.extend_from_slice(value.as_bytes());
                        CString::new(bytes).map_err(|_| {
                            ViewError::Setup("an environment value contains a NUL".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            };
            let per_user = request
                .per_user_files
                .iter()
                .map(|file| {
                    Ok((
                        path_cstring(&file.instance_copy)?,
                        path_cstring(&file.real_path)?,
                    ))
                })
                .collect::<Result<Vec<_>, ViewError>>()?;
            let argv_ptr = pointers(&argv);
            let envp_ptr = pointers(&envp);
            let target = path_cstring(&request.target)?;
            // The fuse daemon is the only mount helper, and the parent finds its mount by the
            // exact mount point field. `mountinfo` escapes spaces and friends, so the probe uses
            // the same escaping.
            let probe = mountinfo_escaped(request.target.as_os_str().as_bytes());
            Ok(Prepared {
                refuse,
                refuse_kernel,
                proc_setgroups: CString::new("/proc/self/setgroups").expect("a fixed path"),
                proc_uid_map: CString::new("/proc/self/uid_map").expect("a fixed path"),
                proc_gid_map: CString::new("/proc/self/gid_map").expect("a fixed path"),
                setgroups_deny: CString::new("deny").expect("a fixed value"),
                uid_map: CString::new(format!("0 {uid} 1\n")).expect("no NUL in a uid map"),
                gid_map: CString::new(format!("0 {gid} 1\n")).expect("no NUL in a gid map"),
                root: CString::new("/").expect("a fixed path"),
                overlay_source: CString::new("overlay").expect("a fixed value"),
                overlay_fs: CString::new("overlay").expect("a fixed value"),
                kernel_data: CString::new(kernel_data)
                    .map_err(|_| ViewError::Setup("a view path contains a NUL".into()))?,
                target,
                program: path_cstring(&request.command.program)?,
                argv,
                argv_ptr,
                envp,
                envp_ptr,
                working_directory: request
                    .command
                    .working_directory
                    .as_deref()
                    .map(path_cstring)
                    .transpose()?,
                per_user,
                fuse_program: find_fuse_overlayfs(),
                fuse_foreground: CString::new("-f").expect("a fixed value"),
                fuse_dash_o: CString::new("-o").expect("a fixed value"),
                fuse_data: CString::new(fuse_data)
                    .map_err(|_| ViewError::Setup("a view path contains a NUL".into()))?,
                fuse_mountinfo_probe: probe,
                fuse_message_prefix: b"K\tfuse-overlayfs\t".to_vec(),
                kernel_message: b"K\tkernel overlayfs with userxattr\n".to_vec(),
            })
        }
    }

    /// A NUL-terminated pointer array for `execve`, built before the fork.
    fn pointers(strings: &[CString]) -> Vec<*const c::c_char> {
        let mut pointers: Vec<*const c::c_char> = strings.iter().map(|s| s.as_ptr()).collect();
        pointers.push(std::ptr::null());
        pointers
    }

    fn path_cstring(path: &Path) -> Result<CString, ViewError> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| ViewError::Setup(format!("a path contains a NUL: {}", path.display())))
    }

    /// Overlay mount options separate layers with `:` and options with `,`, and take a
    /// backslash as an escape. Escape those bytes so a path survives the option string.
    fn escaped(path: &Path) -> Result<String, ViewError> {
        let mut bytes = Vec::with_capacity(path.as_os_str().as_bytes().len());
        for &byte in path.as_os_str().as_bytes() {
            if matches!(byte, b'\\' | b',' | b':') {
                bytes.push(b'\\');
            }
            bytes.push(byte);
        }
        String::from_utf8(bytes)
            .map_err(|_| ViewError::Setup(format!("a view path is not UTF-8: {}", path.display())))
    }

    /// `/proc/self/mountinfo` escapes space, tab, newline and backslash as three-digit octal
    /// (for example a space is `\040`). Escaping the path the same way is how the fuse mount
    /// is found when the game folder has a space in it.
    fn mountinfo_escaped(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            match byte {
                b' ' => out.extend_from_slice(b"\\040"),
                b'\t' => out.extend_from_slice(b"\\011"),
                b'\n' => out.extend_from_slice(b"\\012"),
                b'\\' => out.extend_from_slice(b"\\134"),
                other => out.push(other),
            }
        }
        out
    }

    fn find_fuse_overlayfs() -> Option<CString> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("fuse-overlayfs");
            if let Ok(meta) = std::fs::metadata(&candidate) {
                use std::os::unix::fs::PermissionsExt;
                if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                    return CString::new(candidate.as_os_str().as_bytes()).ok();
                }
            }
        }
        None
    }

    enum Message {
        Ran {
            mechanism: OverlayMechanism,
            /// The `fuse-overlayfs` daemon Agora started, so only it is ended.
            daemon: Option<i32>,
        },
        StepDown(String),
        Failed(String),
    }

    fn run_prepared(prepared: Prepared) -> Result<ViewOutcome, ViewError> {
        let (status_read, status_write) = pipe2(OFlag::O_CLOEXEC).map_err(std::io::Error::from)?;
        // A separate close-on-exec pipe proves the command actually started: the child writes to
        // it only when `execve` fails, and EOF on it means the exec succeeded. The status pipe is
        // written before the exec (the `K` line), so stopping at its first newline used to hide
        // an `execve` failure behind a normal-looking run.
        let (error_read, error_write) = pipe2(OFlag::O_CLOEXEC).map_err(std::io::Error::from)?;
        let status_read_raw = status_read.as_raw_fd();
        let status_write_raw = status_write.as_raw_fd();
        let error_read_raw = error_read.as_raw_fd();
        let error_write_raw = error_write.as_raw_fd();
        match unsafe { fork() }.map_err(std::io::Error::from)? {
            ForkResult::Child => {
                // The child never returns: it either execs the command or exits after
                // reporting why. Nothing is dropped here, so no destructor runs twice.
                unsafe {
                    c::close(status_read_raw);
                    c::close(error_read_raw);
                }
                child(&prepared, status_write_raw, error_write_raw);
            }
            ForkResult::Parent { child } => {
                drop(status_write);
                drop(error_write);
                let message = read_message(status_read_raw);
                drop(status_read);
                let exec_error = read_exec_error(error_read_raw)?;
                drop(error_read);
                let status = wait_for(child);
                // Only the `fuse-overlayfs` daemon Agora started is ended. The game's own
                // process group may hold a wineserver that belongs to the prefix and that
                // another launch (or the player) still uses, so the group is never signalled.
                if let Ok(Message::Ran {
                    mechanism: OverlayMechanism::Fuse,
                    daemon: Some(daemon),
                }) = &message
                {
                    stop_fuse_daemon(*daemon);
                }
                if let Some(errno) = exec_error {
                    let detail = std::io::Error::from_raw_os_error(errno);
                    return Err(ViewError::Setup(format!(
                        "cannot start the command '{}': {detail}",
                        prepared.program.to_string_lossy()
                    )));
                }
                match message {
                    Ok(Message::Ran { mechanism, .. }) => {
                        let status = status?;
                        Ok(ViewOutcome::Ran(ViewRun {
                            mechanism,
                            status,
                            pid: child.as_raw() as u32,
                        }))
                    }
                    Ok(Message::StepDown(reason)) => Ok(ViewOutcome::SteppedDown(StepDown {
                        reason,
                        next: step_down_mode(),
                    })),
                    Ok(Message::Failed(reason)) => Err(ViewError::Setup(reason)),
                    Err(reason) => Err(ViewError::Setup(format!(
                        "{reason} (the view helper exited as {:?})",
                        status.map(|s| s.code()).unwrap_or(None)
                    ))),
                }
            }
        }
    }

    /// End the `fuse-overlayfs` daemon Agora started, and only it. A polite SIGTERM unmounts; a
    /// daemon that does not go is killed. Nothing else in the game's process group is touched.
    fn stop_fuse_daemon(daemon: i32) {
        if daemon <= 0 {
            return;
        }
        unsafe { c::kill(daemon, c::SIGTERM) };
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if unsafe { c::kill(daemon, 0) } != 0 {
                return;
            }
        }
        unsafe { c::kill(daemon, c::SIGKILL) };
    }

    // -----------------------------------------------------------------------
    // The child: syscalls only, no allocation, from here to `execve`.
    // -----------------------------------------------------------------------

    /// A piece of a status line. Static text, or a number rendered on the stack.
    enum Piece<'a> {
        Text(&'a [u8]),
        Number(i32),
    }

    const STEP_DOWN_EXIT: i32 = 71;
    const FAILED_EXIT: i32 = 70;

    fn last_errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// Retry an operation that reported an interrupted system call. Taking the attempt as a
    /// closure lets the retry itself be tested without sending signals to the test process.
    fn retry_eintr<T>(mut attempt: impl FnMut() -> T, interrupted: impl Fn(&T) -> bool) -> T {
        loop {
            let result = attempt();
            if !interrupted(&result) {
                return result;
            }
        }
    }

    fn write_raw(fd: RawFd, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            let written = unsafe { c::write(fd, bytes.as_ptr() as *const _, bytes.len()) };
            if written < 0 {
                if last_errno() == c::EINTR {
                    continue;
                }
                return false;
            }
            bytes = &bytes[written as usize..];
        }
        true
    }

    fn write_number(fd: RawFd, value: i32) -> bool {
        if value < 0 && !write_raw(fd, b"-") {
            return false;
        }
        let mut number = value.unsigned_abs();
        let mut digits = [0u8; 10];
        let mut start = digits.len();
        if number == 0 {
            return write_raw(fd, b"0");
        }
        while number > 0 {
            start -= 1;
            digits[start] = b'0' + (number % 10) as u8;
            number /= 10;
        }
        write_raw(fd, &digits[start..])
    }

    fn report(pipe: RawFd, kind: u8, exit_code: i32, pieces: &[Piece<'_>]) -> ! {
        let _ = write_raw(pipe, &[kind, b'\t']);
        for piece in pieces {
            let _ = match piece {
                Piece::Text(text) => write_raw(pipe, text),
                Piece::Number(number) => write_number(pipe, *number),
            };
        }
        let _ = write_raw(pipe, b"\n");
        unsafe { c::_exit(exit_code) }
    }

    fn step_down(pipe: RawFd, pieces: &[Piece<'_>]) -> ! {
        report(pipe, b'S', STEP_DOWN_EXIT, pieces)
    }

    fn failed(pipe: RawFd, pieces: &[Piece<'_>]) -> ! {
        report(pipe, b'E', FAILED_EXIT, pieces)
    }

    /// Write a fixed file under `/proc/self` with a raw syscall. No allocation.
    fn write_proc(path: &CStr, data: &CStr) -> i32 {
        let fd = unsafe { c::open(path.as_ptr(), c::O_WRONLY | c::O_CLOEXEC) };
        if fd < 0 {
            return last_errno();
        }
        let mut remaining = data.to_bytes();
        let mut error = 0;
        while !remaining.is_empty() {
            let written = unsafe { c::write(fd, remaining.as_ptr() as *const _, remaining.len()) };
            if written < 0 {
                if last_errno() == c::EINTR {
                    continue;
                }
                error = last_errno();
                break;
            }
            remaining = &remaining[written as usize..];
        }
        unsafe { c::close(fd) };
        error
    }

    fn child(prepared: &Prepared, status: RawFd, error_fd: RawFd) -> ! {
        if unsafe { c::unshare(c::CLONE_NEWUSER) } != 0 {
            step_down(
                status,
                &[
                    Piece::Text(b"unprivileged user namespaces are unavailable: errno "),
                    Piece::Number(last_errno()),
                ],
            );
        }
        // Denying setgroups is required before an unprivileged gid map. Older kernels
        // have no setgroups file; that is not an error.
        let _ = write_proc(&prepared.proc_setgroups, &prepared.setgroups_deny);
        let error = write_proc(&prepared.proc_uid_map, &prepared.uid_map);
        if error != 0 {
            step_down(
                status,
                &[
                    Piece::Text(b"cannot map the caller's uid: errno "),
                    Piece::Number(error),
                ],
            );
        }
        let error = write_proc(&prepared.proc_gid_map, &prepared.gid_map);
        if error != 0 {
            step_down(
                status,
                &[
                    Piece::Text(b"cannot map the caller's gid: errno "),
                    Piece::Number(error),
                ],
            );
        }
        // uid 0 here is the player's own host uid mapped into the namespace, so the game gets
        // no privilege over real files. It is kept as 0 (rather than mapping the player's uid
        // directly) because overlay copy-up keeps a lower layer's read-only mode: content
        // objects are 0444, and an unprivileged uid cannot write the copy-up, so a game could
        // not change a mod's config. uid 0 keeps CAP_DAC_OVERRIDE inside the namespace only.
        // Steam's pressure-vessel runtime has not been run against this; try a real launch before
        // changing the mapping.
        if unsafe { c::unshare(c::CLONE_NEWNS) } != 0 {
            step_down(
                status,
                &[
                    Piece::Text(b"cannot create a mount namespace: errno "),
                    Piece::Number(last_errno()),
                ],
            );
        }
        if unsafe {
            c::mount(
                std::ptr::null(),
                prepared.root.as_ptr(),
                std::ptr::null(),
                c::MS_REC | c::MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            step_down(
                status,
                &[
                    Piece::Text(b"cannot make the mount namespace private: errno "),
                    Piece::Number(last_errno()),
                ],
            );
        }
        if prepared.refuse {
            step_down(
                status,
                &[Piece::Text(
                    b"overlay mounting was refused by AGORA_LINUX_VIEW_REFUSE_OVERLAY (test hook)",
                )],
            );
        }

        // Capture the kernel attempt's errno before `try_fuse` makes its own syscalls.
        let kernel_error = if prepared.refuse_kernel {
            c::ENOTSUP
        } else {
            let mounted = unsafe {
                c::mount(
                    prepared.overlay_source.as_ptr(),
                    prepared.target.as_ptr(),
                    prepared.overlay_fs.as_ptr(),
                    0,
                    prepared.kernel_data.as_ptr() as *const _,
                )
            };
            if mounted == 0 {
                0
            } else {
                last_errno()
            }
        };
        // The fuse daemon is Agora's own. Remember it so every later failure ends it instead of
        // leaking a mount helper that outlives the view.
        let daemon = if kernel_error == 0 {
            None
        } else {
            match try_fuse(prepared) {
                Ok(daemon) => Some(daemon),
                Err(fuse) => step_down(
                    status,
                    &[
                        Piece::Text(b"kernel overlayfs with userxattr (errno "),
                        Piece::Number(kernel_error),
                        Piece::Text(b"); fuse-overlayfs ("),
                        Piece::Text(fuse.reason()),
                        Piece::Text(b")"),
                    ],
                ),
            }
        };

        for (source, real) in &prepared.per_user {
            if unsafe {
                c::mount(
                    source.as_ptr(),
                    real.as_ptr(),
                    std::ptr::null(),
                    c::MS_BIND,
                    std::ptr::null(),
                )
            } != 0
            {
                kill_daemon(daemon);
                failed(
                    status,
                    &[
                        Piece::Text(b"cannot bind the instance copy over '"),
                        Piece::Text(real.to_bytes()),
                        Piece::Text(b"': errno "),
                        Piece::Number(last_errno()),
                    ],
                );
            }
        }

        if let Some(directory) = &prepared.working_directory {
            if unsafe { c::chdir(directory.as_ptr()) } != 0 {
                kill_daemon(daemon);
                failed(
                    status,
                    &[
                        Piece::Text(b"cannot enter the working folder: errno "),
                        Piece::Number(last_errno()),
                    ],
                );
            }
        }

        match daemon {
            None => {
                if !write_raw(status, &prepared.kernel_message) {
                    unsafe { c::_exit(125) };
                }
            }
            Some(daemon) => {
                let wrote = write_raw(status, &prepared.fuse_message_prefix)
                    && write_number(status, daemon)
                    && write_raw(status, b"\n");
                if !wrote {
                    kill_daemon(Some(daemon));
                    unsafe { c::_exit(125) };
                }
            }
        }
        unsafe {
            c::execve(
                prepared.program.as_ptr(),
                prepared.argv_ptr.as_ptr(),
                prepared.envp_ptr.as_ptr(),
            )
        };
        // `execve` only returns on failure. The status pipe already carries the `K` line, so the
        // errno goes on the separate close-on-exec pipe: EOF there is proof the command started.
        let errno = last_errno();
        let _ = write_raw(error_fd, b"E\t");
        let _ = write_number(error_fd, errno);
        let _ = write_raw(error_fd, b"\n");
        kill_daemon(daemon);
        unsafe { c::_exit(FAILED_EXIT) };
    }

    fn kill_daemon(daemon: Option<i32>) {
        if let Some(daemon) = daemon {
            unsafe {
                c::kill(daemon, c::SIGKILL);
            }
        }
    }

    /// Mount calls return -1 and set errno; a non-zero `error` from the kernel attempt is that
    /// errno (or ENOTSUP when the test hook skips it).
    enum FuseFailure {
        NotOnPath,
        Fork,
        Timeout,
    }

    impl FuseFailure {
        fn reason(&self) -> &'static [u8] {
            match self {
                FuseFailure::NotOnPath => b"not on PATH",
                FuseFailure::Fork => b"cannot start its daemon",
                FuseFailure::Timeout => b"its mount did not appear within 2 seconds",
            }
        }
    }

    /// Mount with `fuse-overlayfs`, if it is installed. The daemon is forked here, in the mount
    /// namespace, and its pid is returned so the parent ends exactly that process after the
    /// command exits. It also dies with the process that execs the command, so a force-stopped
    /// wrapper cannot leave it behind.
    fn try_fuse(prepared: &Prepared) -> Result<i32, FuseFailure> {
        let program = prepared
            .fuse_program
            .as_ref()
            .ok_or(FuseFailure::NotOnPath)?;
        let daemon = unsafe { c::fork() };
        if daemon < 0 {
            return Err(FuseFailure::Fork);
        }
        if daemon == 0 {
            // If the process that would become the game is already gone, this daemon has no
            // reason to live. Otherwise the kernel kills it when that process dies.
            if unsafe { c::getppid() } == 1 {
                unsafe { c::_exit(127) };
            }
            unsafe { c::prctl(c::PR_SET_PDEATHSIG, c::SIGKILL) };
            // fuse-overlayfs(1): `-f`, `--foreground` keeps the daemon in the foreground instead
            // of detaching. Without it the pid below is the short-lived wrapper, so both
            // PR_SET_PDEATHSIG and the later stop signal bind to the wrong process and the real
            // mount helper outlives the view.
            let argv: [*const c::c_char; 6] = [
                program.as_ptr(),
                prepared.fuse_foreground.as_ptr(),
                prepared.fuse_dash_o.as_ptr(),
                prepared.fuse_data.as_ptr(),
                prepared.target.as_ptr(),
                std::ptr::null(),
            ];
            unsafe { c::execve(program.as_ptr(), argv.as_ptr(), prepared.envp_ptr.as_ptr()) };
            unsafe { c::_exit(126) };
        }
        // A mount that was already there (a stale mount from a failed daemon, or the target
        // named as another line's root) must not be mistaken for this daemon's mount.
        let before = mount_count(&prepared.fuse_mountinfo_probe);
        // The daemon was started with the status pipe open; it must not keep it open.
        // The parent's write fd is close-on-exec and closes in the daemon on exec, but
        // the daemon inherits the same number. Nothing here depends on the pipe ending.
        for _ in 0..200 {
            if mount_count(&prepared.fuse_mountinfo_probe) > before {
                return Ok(daemon);
            }
            let delay = c::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            unsafe { c::nanosleep(&delay, std::ptr::null_mut()) };
        }
        unsafe { c::kill(daemon, c::SIGKILL) };
        // The daemon is a child of this process until the command is exec'd, so a daemon that
        // never mounted would otherwise be left as a zombie. Reap it before reporting the
        // step-down.
        let _ = waitpid(Pid::from_raw(daemon), None);
        Err(FuseFailure::Timeout)
    }

    /// How many mounts of `target` (already escaped as in `mountinfo`) the child's own mount
    /// table has. Reads `/proc/self/mountinfo` with raw syscalls into a stack buffer. A line that
    /// spans two reads is found by keeping the tail. Comparing this before and after starting the
    /// daemon proves a NEW mount appeared.
    fn mount_count(target: &[u8]) -> usize {
        let path = b"/proc/self/mountinfo\0";
        let fd = unsafe { c::open(path.as_ptr() as *const _, c::O_RDONLY | c::O_CLOEXEC) };
        if fd < 0 {
            return 0;
        }
        let mut buffer = [0u8; 64 * 1024];
        let mut filled = 0;
        let mut count = 0;
        loop {
            if filled >= buffer.len() {
                break;
            }
            let read = retry_eintr(
                || unsafe {
                    c::read(
                        fd,
                        buffer[filled..].as_mut_ptr() as *mut _,
                        buffer.len() - filled,
                    )
                },
                |read| *read < 0 && last_errno() == c::EINTR,
            );
            if read < 0 {
                break;
            }
            if read == 0 {
                break;
            }
            filled += read as usize;
            // Count only complete lines now, and slide the last partial line to the front so a
            // mount entry split across two reads is counted once, on the next pass.
            let complete = buffer[..filled]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|at| at + 1)
                .unwrap_or(0);
            count += count_mount_lines(&buffer[..complete], target);
            let tail = filled - complete;
            if tail > 0 {
                unsafe {
                    std::ptr::copy(buffer[complete..].as_ptr(), buffer.as_mut_ptr(), tail);
                }
            }
            filled = tail;
        }
        count += count_mount_lines(&buffer[..filled], target);
        unsafe { c::close(fd) };
        count
    }

    /// Count lines whose mount point is `target` and whose file system type is an overlay the
    /// wrapper could have mounted. A name in the root field, or a different file system, is not
    /// the daemon's mount even when the bytes are otherwise identical.
    fn count_mount_lines(text: &[u8], target: &[u8]) -> usize {
        if target.is_empty() {
            return 0;
        }
        text.split(|byte| *byte == b'\n')
            .filter(|line| mount_line_matches(line, target))
            .count()
    }

    fn mount_line_matches(line: &[u8], target: &[u8]) -> bool {
        if target.is_empty() {
            return false;
        }
        // `/proc/self/mountinfo` is space separated; fields holding a path escape their spaces
        // as `\040`, so splitting on a literal space is safe.
        let mut fields = line.split(|byte| *byte == b' ');
        // Field 5 (index 4) is the mount point. A target in field 4 (the root) is unrelated.
        if fields.nth(4) != Some(target) {
            return false;
        }
        // The fstype is the field after the bare `-` separator.
        let fstype = loop {
            match fields.next() {
                Some(b"-") => break fields.next(),
                Some(_) => continue,
                None => return false,
            }
        };
        fstype.is_some_and(|fs| {
            fs == b"fuse.fuse-overlayfs" || fs == b"fuse.overlay" || fs == b"overlay"
        })
    }

    /// Read the close-on-exec error pipe to EOF. `None` means the command started; `Some(errno)`
    /// is what `execve` failed with. An unreadable or unexpected report is an error, never
    /// permission to claim a run happened.
    fn read_exec_error(fd: RawFd) -> Result<Option<i32>, ViewError> {
        let mut bytes = [0u8; 32];
        let mut filled = 0;
        loop {
            let read = retry_eintr(
                || unsafe {
                    c::read(
                        fd,
                        bytes[filled..].as_mut_ptr() as *mut _,
                        bytes.len() - filled,
                    )
                },
                |read| *read < 0 && last_errno() == c::EINTR,
            );
            if read < 0 {
                return Err(ViewError::Setup(format!(
                    "cannot confirm the command started: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if read == 0 {
                break;
            }
            filled += read as usize;
            if bytes[..filled].contains(&b'\n') || filled == bytes.len() {
                break;
            }
        }
        let text = String::from_utf8_lossy(&bytes[..filled]);
        let line = text.lines().next().unwrap_or_default();
        if line.is_empty() {
            return Ok(None);
        }
        match line.strip_prefix("E\t") {
            Some(detail) => detail.trim().parse::<i32>().map(Some).map_err(|_| {
                ViewError::Setup(format!(
                    "the view helper reported an unreadable exec error: {line}"
                ))
            }),
            None => Err(ViewError::Setup(format!(
                "the view helper reported an unexpected exec error: {line}"
            ))),
        }
    }

    fn read_message(fd: RawFd) -> Result<Message, String> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let read = retry_eintr(
                || unsafe { c::read(fd, chunk.as_mut_ptr() as *mut _, chunk.len()) },
                |read| *read < 0 && last_errno() == c::EINTR,
            );
            if read < 0 {
                return Err(format!(
                    "cannot read the view helper's report: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read as usize]);
            if bytes.contains(&b'\n') || bytes.len() > 8192 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        let line = text.lines().next().unwrap_or_default();
        let (kind, detail) = line.split_once('\t').unwrap_or((line, ""));
        match kind {
            "K" if detail == "kernel overlayfs with userxattr" => Ok(Message::Ran {
                mechanism: OverlayMechanism::Kernel,
                daemon: None,
            }),
            "K" => match detail.split_once('\t') {
                Some(("fuse-overlayfs", pid)) => Ok(Message::Ran {
                    mechanism: OverlayMechanism::Fuse,
                    daemon: pid.parse().ok(),
                }),
                _ => Err(format!(
                    "the view helper reported an unknown result: {line}"
                )),
            },
            "S" => Ok(Message::StepDown(detail.to_string())),
            "E" => Ok(Message::Failed(detail.to_string())),
            "" => Err("the view helper exited without reporting a result".to_string()),
            other => Err(format!(
                "the view helper reported an unknown result: {other}"
            )),
        }
    }

    fn wait_for(child: Pid) -> Result<ExitStatus, ViewError> {
        loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => {
                    return Ok(ExitStatus::from_raw(code << 8));
                }
                Ok(WaitStatus::Signaled(_, signal, _)) => {
                    return Ok(ExitStatus::from_raw(signal as i32));
                }
                Ok(_) => continue,
                Err(errno) => {
                    return Err(ViewError::Setup(format!(
                        "cannot wait for the view helper: {errno}"
                    )));
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_read_interrupted_by_a_signal_is_retried() {
            let mut calls = 0;
            let read = retry_eintr(
                || {
                    calls += 1;
                    if calls < 3 {
                        -1
                    } else {
                        12
                    }
                },
                |result| *result == -1,
            );
            assert_eq!(read, 12);
            assert_eq!(calls, 3);
        }

        #[test]
        fn mountinfo_escapes_every_character_the_kernel_escapes() {
            assert_eq!(
                mountinfo_escaped(b"/games/A Game\\x\ty\nz"),
                b"/games/A\\040Game\\134x\\011y\\012z"
            );
        }

        #[test]
        fn a_pre_existing_or_wrong_fstype_mount_is_not_counted() {
            let target = b"/games/A\\040Game";
            // A stale fuse mount left by a failed daemon still has the target as its mount point.
            let stale = b"36 35 98:0 / /games/A\\040Game rw - fuse.fuse-overlayfs lowerdir=x rw\n";
            // The target named as another line's ROOT is not a mount of the target.
            let root = b"37 35 98:0 /games/A\\040Game /elsewhere rw - ext4 /dev/sda1 rw\n";
            // Same mount point, but a different file system: not the fuse daemon's mount.
            let other_fs = b"38 35 98:0 / /games/A\\040Game rw - ext4 /dev/sda1 rw\n";
            let before = [stale.as_slice(), root.as_slice(), other_fs.as_slice()].concat();
            assert_eq!(
                count_mount_lines(&before, target),
                1,
                "only the stale fuse mount of the target counts"
            );
            let mut after = before.clone();
            after.extend_from_slice(
                b"39 35 0:47 / /games/A\\040Game rw - fuse.fuse-overlayfs lowerdir=y rw\n",
            );
            assert_eq!(
                count_mount_lines(&after, target),
                2,
                "a new fuse mount of the target adds exactly one"
            );
        }

        #[test]
        fn a_folder_with_a_space_is_found_by_its_escaped_mountinfo_field() {
            // The format `/proc/self/mountinfo` writes, with the mount point in field 5.
            let mountinfo =
                b"36 35 98:0 / /games/A\\040Game rw,relatime - fuse.overlay /games/A\\040Game rw\n";
            assert_eq!(count_mount_lines(mountinfo, b"/games/A\\040Game"), 1);
            // The unescaped path is what the old check looked for, and it never matches.
            assert_eq!(count_mount_lines(mountinfo, b"/games/A Game"), 0);
            // A sibling whose name merely starts the same is not the mount.
            assert_eq!(count_mount_lines(mountinfo, b"/games/A\\040Game Two"), 0);
        }
    }
}
