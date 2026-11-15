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

    use nix::mount::{mount, MsFlags};
    use nix::sched::{unshare, CloneFlags};
    use nix::sys::wait::{waitpid, WaitStatus};
    use nix::unistd::{execve, fork, getegid, geteuid, pipe2, ForkResult, Pid};

    use super::*;

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
            crate::game_deploy::deploy(ctx, &request.instance_id, definition, DeployMode::Virtual)?;
            let lower = crate::game_deploy::deployment_dir(ctx, &request.instance_id)?
                .ok_or_else(|| ViewError::NoDeployment(request.instance_id.clone()))?;
            let instance_dir = ctx
                .paths
                .instance_dir(&request.instance_id)
                .map_err(|e| ViewError::Setup(e.to_string()))?;
            let upper = instance_dir.join("writable");
            let work = instance_dir.join("view-work");
            std::fs::create_dir_all(&upper)?;
            // Overlay needs an empty work folder. One left by a stopped view is not trusted
            // (a half-written copy-up is not something to carry into the next run).
            if work.exists() {
                std::fs::remove_dir_all(&work)?;
            }
            std::fs::create_dir_all(&work)?;

            let refuse =
                std::env::var_os(REFUSE_OVERLAY_ENV).is_some_and(|value| !value.is_empty());
            let prepared = Prepared::new(request, &lower, &upper, &work, refuse)?;
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
            if !file.instance_copy.is_file() {
                return Err(ViewError::UnreadablePerUserFile(file.instance_copy.clone()));
            }
            if !file.real_path.exists() {
                return Err(ViewError::UnreadablePerUserFile(file.real_path.clone()));
            }
        }
        Ok(())
    }

    /// Everything the child needs, allocated before `fork`, so the child only makes
    /// syscalls until it execs.
    struct Prepared {
        refuse: bool,
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
        fuse_data: CString,
        target: CString,
        program: CString,
        argv: Vec<CString>,
        envp: Vec<CString>,
        working_directory: Option<CString>,
        per_user: Vec<(CString, CString)>,
        fuse_program: Option<CString>,
    }

    impl Prepared {
        fn new(
            request: &ViewRequest,
            lower: &Path,
            upper: &Path,
            work: &Path,
            refuse: bool,
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
            Ok(Prepared {
                refuse,
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
                fuse_data: CString::new(fuse_data)
                    .map_err(|_| ViewError::Setup("a view path contains a NUL".into()))?,
                target: path_cstring(&request.target)?,
                program: path_cstring(&request.command.program)?,
                argv,
                envp,
                working_directory: request
                    .command
                    .working_directory
                    .as_deref()
                    .map(path_cstring)
                    .transpose()?,
                per_user,
                fuse_program: find_fuse_overlayfs(),
            })
        }
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
        Ran(OverlayMechanism),
        StepDown(String),
        Failed(String),
    }

    fn run_prepared(prepared: Prepared) -> Result<ViewOutcome, ViewError> {
        let (read_fd, write_fd) =
            pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)?;
        let read_raw = read_fd.as_raw_fd();
        let write_raw = write_fd.as_raw_fd();
        match unsafe { fork() }.map_err(std::io::Error::from)? {
            ForkResult::Child => {
                // The child never returns: it either execs the command or exits after
                // reporting why. Nothing is dropped here, so no destructor runs twice.
                unsafe { nix::libc::close(read_raw) };
                child(&prepared, write_raw);
            }
            ForkResult::Parent { child } => {
                drop(write_fd);
                let message = read_message(read_raw);
                drop(read_fd);
                let status = wait_for(child);
                match message {
                    Ok(Message::Ran(mechanism)) => {
                        let status = status?;
                        if mechanism == OverlayMechanism::Fuse {
                            // Its daemon stayed in the child's process group. The game is
                            // gone, so nothing else needs that group.
                            unsafe { nix::libc::kill(-child.as_raw(), nix::libc::SIGKILL) };
                        }
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

    fn child(prepared: &Prepared, pipe: RawFd) -> ! {
        if let Err(errno) = unshare(CloneFlags::CLONE_NEWUSER) {
            step_down(
                pipe,
                format!("unprivileged user namespaces are unavailable: {errno}"),
            );
        }
        // Denying setgroups is required before an unprivileged gid map. Older kernels
        // have no setgroups file; that is not an error.
        let _ = write_proc(&prepared.proc_setgroups, &prepared.setgroups_deny);
        if let Err(errno) = write_proc(&prepared.proc_uid_map, &prepared.uid_map) {
            step_down(pipe, format!("cannot map the caller's uid: {errno}"));
        }
        if let Err(errno) = write_proc(&prepared.proc_gid_map, &prepared.gid_map) {
            step_down(pipe, format!("cannot map the caller's gid: {errno}"));
        }
        // Now that uid 0 exists inside the namespace, mount changes are permitted.
        if let Err(errno) = unshare(CloneFlags::CLONE_NEWNS) {
            step_down(pipe, format!("cannot create a mount namespace: {errno}"));
        }
        if let Err(errno) = mount::<CStr, CStr, CStr, CStr>(
            None,
            prepared.root.as_c_str(),
            None,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None,
        ) {
            step_down(
                pipe,
                format!("cannot make the mount namespace private: {errno}"),
            );
        }
        if prepared.refuse {
            step_down(
                pipe,
                format!("overlay mounting was refused by {REFUSE_OVERLAY_ENV} (test hook)"),
            );
        }

        let mechanism = match mount::<CStr, CStr, CStr, CStr>(
            Some(prepared.overlay_source.as_c_str()),
            prepared.target.as_c_str(),
            Some(prepared.overlay_fs.as_c_str()),
            MsFlags::empty(),
            Some(prepared.kernel_data.as_c_str()),
        ) {
            Ok(()) => OverlayMechanism::Kernel,
            Err(kernel) => match try_fuse(prepared) {
                Ok(()) => OverlayMechanism::Fuse,
                Err(fuse) => step_down(
                    pipe,
                    format!("kernel overlayfs with userxattr ({kernel}); fuse-overlayfs ({fuse})"),
                ),
            },
        };

        for (source, real) in &prepared.per_user {
            if let Err(errno) = mount::<CStr, CStr, CStr, CStr>(
                Some(source.as_c_str()),
                real.as_c_str(),
                None,
                MsFlags::MS_BIND,
                None,
            ) {
                failed(
                    pipe,
                    format!(
                        "cannot bind the instance copy over '{}': {errno}",
                        real.to_string_lossy()
                    ),
                );
            }
        }

        if let Some(directory) = &prepared.working_directory {
            if unsafe { nix::libc::chdir(directory.as_ptr()) } != 0 {
                let errno = std::io::Error::last_os_error();
                failed(pipe, format!("cannot enter the working folder: {errno}"));
            }
        }

        if write_all(pipe, format!("K\t{}\n", mechanism.as_str()).as_bytes()).is_err() {
            unsafe { nix::libc::_exit(125) };
        }
        // The write end is close-on-exec, so it goes away exactly when the command starts.
        let _ = execve(prepared.program.as_c_str(), &prepared.argv, &prepared.envp);
        let errno = std::io::Error::last_os_error();
        failed(pipe, format!("cannot start the command: {errno}"));
    }

    /// Mount with `fuse-overlayfs`, if it is installed. Its daemon is left in the child's
    /// process group so the parent can end it after the command exits.
    fn try_fuse(prepared: &Prepared) -> Result<(), String> {
        let program = prepared
            .fuse_program
            .as_ref()
            .ok_or_else(|| "not on PATH".to_string())?;
        if unsafe { nix::libc::setpgid(0, 0) } != 0 {
            return Err("cannot make a process group for its daemon".to_string());
        }
        let daemon = unsafe { nix::libc::fork() };
        if daemon < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if daemon == 0 {
            let args = [
                program.clone(),
                CString::new("-o").expect("a fixed value"),
                prepared.fuse_data.clone(),
                prepared.target.clone(),
            ];
            let _ = execve(program.as_c_str(), &args, &prepared.envp);
            unsafe { nix::libc::_exit(126) };
        }
        // The daemon was started with the status pipe open; it must not keep it open.
        // The parent's write fd is close-on-exec and closes in the daemon on exec, but
        // the daemon inherits the same number. Nothing here depends on the pipe ending.
        let target = prepared.target.to_string_lossy();
        for _ in 0..200 {
            if std::fs::read_to_string("/proc/self/mountinfo")
                .map(|info| info.contains(&format!(" {target} ")))
                .unwrap_or(false)
            {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { nix::libc::kill(daemon, nix::libc::SIGKILL) };
        Err("its mount did not appear within 2 seconds".to_string())
    }

    fn write_proc(path: &CStr, data: &CStr) -> Result<(), std::io::Error> {
        let fd =
            unsafe { nix::libc::open(path.as_ptr(), nix::libc::O_WRONLY | nix::libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written =
            unsafe { nix::libc::write(fd, data.as_ptr() as *const _, data.to_bytes().len()) };
        let error = (written < 0).then(std::io::Error::last_os_error);
        unsafe { nix::libc::close(fd) };
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn write_all(fd: RawFd, bytes: &[u8]) -> Result<(), std::io::Error> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let written =
                unsafe { nix::libc::write(fd, remaining.as_ptr() as *const _, remaining.len()) };
            if written < 0 {
                return Err(std::io::Error::last_os_error());
            }
            remaining = &remaining[written as usize..];
        }
        Ok(())
    }

    fn read_message(fd: RawFd) -> Result<Message, String> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let read = unsafe { nix::libc::read(fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
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
            "K" if detail == "kernel overlayfs with userxattr" => {
                Ok(Message::Ran(OverlayMechanism::Kernel))
            }
            "K" if detail == "fuse-overlayfs" => Ok(Message::Ran(OverlayMechanism::Fuse)),
            "S" => Ok(Message::StepDown(detail.to_string())),
            "E" => Ok(Message::Failed(detail.to_string())),
            "" => Err("the view helper exited without reporting a result".to_string()),
            other => Err(format!(
                "the view helper reported an unknown result: {other}"
            )),
        }
    }

    fn step_down(pipe: RawFd, reason: String) -> ! {
        let _ = write_all(
            pipe,
            format!("S\t{}\n", reason.replace(['\n', '\t'], " ")).as_bytes(),
        );
        unsafe { nix::libc::_exit(71) }
    }

    fn failed(pipe: RawFd, reason: String) -> ! {
        let _ = write_all(
            pipe,
            format!("E\t{}\n", reason.replace(['\n', '\t'], " ")).as_bytes(),
        );
        unsafe { nix::libc::_exit(70) }
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
}
