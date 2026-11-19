//! What happens when `agora_vfs.dll` is the first import of a process and cannot do its job.
//!
//! Run with:
//! cargo build -p agora-vfs -p agora-vfs-early-import && cargo test -p agora-vfs -- --ignored
//!
//! The fixture program is `agora-early-import-exe.exe` from `crates/agora-vfs/fixtures/early-import`;
//! like `agora_vfs.dll` it is found in the target folder this test runs from. The end-to-end
//! ordering test (a static import's `DllMain` writing into a deployed file before any hook could
//! exist) is `real_injection_loads_the_vfs_before_the_games_own_imports` in
//! `crates/agora-core/tests/game_deploy.rs`.

#![cfg(windows)]

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

/// What `agora_vfs.dll` ends a process it cannot protect with (`DllMain`).
const EXIT_UNPROTECTED: i32 = 0xA6F5_0001_u32 as i32;

fn artifacts() -> (PathBuf, PathBuf) {
    // `target\debug`: test binaries are in `deps` or, in cargo's newer layout, under `build`.
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .ancestors()
        .skip(1)
        .find(|dir| dir.file_name() == Some(std::ffi::OsStr::new("debug")))
        .unwrap_or_else(|| panic!("{} is not under a `debug` folder", exe.display()))
        .to_path_buf();
    let dll = dir.join("agora_vfs.dll");
    let exe = dir.join("agora-early-import-exe.exe");
    let dll_beside_exe = dir.join("agora_early_import.dll");
    for file in [&dll, &exe, &dll_beside_exe] {
        assert!(
            file.exists(),
            "{} not found. Run `cargo build -p agora-vfs -p agora-vfs-early-import` first.",
            file.display()
        );
    }
    (dll, exe)
}

unsafe fn resume(process: HANDLE) {
    type NtResumeProcess = unsafe extern "system" fn(HANDLE) -> i32;
    let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr() as *const u8);
    let proc = GetProcAddress(ntdll, c"NtResumeProcess".as_ptr() as *const u8).unwrap();
    let nt_resume_process: NtResumeProcess = std::mem::transmute(proc);
    let status = nt_resume_process(process);
    assert!(status >= 0, "NtResumeProcess {status:#x}");
}

fn wait_for_exit(child: &mut Child, limit: Duration) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Start the fixture suspended, make the DLL its first import, and let it run.
fn run_with_dll_as_first_import(config: Option<&str>) -> Option<i32> {
    run_with_dll_as_first_import_logging(config, None)
}

/// As [`run_with_dll_as_first_import`], naming the log the launcher would (`AGORA_VFS_LOG`).
fn run_with_dll_as_first_import_logging(
    config: Option<&str>,
    log: Option<&std::path::Path>,
) -> Option<i32> {
    let (dll, exe) = artifacts();
    let mut cmd = Command::new(exe);
    cmd.env_remove("AGORA_VFS_CONFIG");
    cmd.env_remove("AGORA_VFS_LOG");
    if let Some(log) = log {
        cmd.env("AGORA_VFS_LOG", log);
    }
    if let Some(config) = config {
        cmd.env("AGORA_VFS_CONFIG", config);
    }
    run_fixture(cmd, &dll)
}

fn run_fixture(mut cmd: Command, dll: &Path) -> Option<i32> {
    cmd.creation_flags(CREATE_SUSPENDED);
    let mut child = cmd.spawn().unwrap();
    // SAFETY: the handle is the live one `child` owns, and the process is still suspended.
    unsafe {
        agora_vfs_inject::inject_import_table(child.as_raw_handle() as HANDLE, dll)
            .expect("import-table injection");
        resume(child.as_raw_handle() as HANDLE);
    }
    let code = wait_for_exit(&mut child, Duration::from_secs(10));
    if code.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    code
}

#[test]
#[ignore = "starts a VFS fixture that starts its own child with a custom environment"]
fn a_child_with_an_empty_environment_still_uses_the_view() {
    let (dll, exe) = artifacts();
    let dir = tempfile::tempdir().unwrap();
    let mount = dir.path().join("mount");
    let lower = dir.path().join("lower");
    let upper = dir.path().join("upper");
    for path in [&mount, &lower, &upper] {
        std::fs::create_dir(path).unwrap();
    }
    let parent = mount.join("Parent.exe");
    std::fs::copy(&exe, &parent).unwrap();
    std::fs::copy(&exe, mount.join("Child.exe")).unwrap();
    std::fs::copy(
        exe.with_file_name("agora_early_import.dll"),
        mount.join("agora_early_import.dll"),
    )
    .unwrap();
    let marker = mount.join("early_Child.txt");
    std::fs::write(&marker, b"original lower contents").unwrap();
    std::fs::write(
        lower.join("view-marker.txt"),
        b"visible only through the view",
    )
    .unwrap();
    let config = dir.path().join("config.json");
    let log = dir.path().join("vfs.log");
    let dll_x86 = cfg!(target_arch = "x86").then_some(&dll);
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "version": if dll_x86.is_some() { 2 } else { 1 },
            "mount": mount,
            "upper": upper,
            "lowers": [mount, lower],
            "dll": dll,
            "dll_x86": dll_x86,
            "log": log,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut command = Command::new(parent);
    command
        .env("AGORA_VFS_CONFIG", config)
        .current_dir(&mount)
        .arg("spawn-empty-environment");

    let code = run_fixture(command, &dll);
    assert_eq!(
        code,
        Some(0),
        "the child could not read the merged view: {}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"original lower contents");
    assert_eq!(
        std::fs::read(upper.join("early_Child.txt")).unwrap(),
        b"written by the fixture DllMain"
    );
}

fn run_regression(case: &str, setup: impl FnOnce(&Path, &Path)) -> tempfile::TempDir {
    let (dll, _) = artifacts();
    let dir = tempfile::tempdir().unwrap();
    let mount = dir.path().join("mount");
    let upper = dir.path().join("upper");
    std::fs::create_dir(&mount).unwrap();
    std::fs::create_dir(&upper).unwrap();
    setup(&mount, &upper);
    let config = dir.path().join("config.json");
    let log = dir.path().join("vfs.log");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "mount": mount,
            "upper": upper,
            "lowers": [mount],
            "dll": dll,
            "log": log,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agora-vfs-probe"));
    cmd.env("AGORA_VFS_CONFIG", config)
        .current_dir(&mount)
        .args(["--regression", case]);
    let code = run_fixture(cmd, &dll);
    assert_eq!(
        code,
        Some(0),
        "{case}: {}",
        std::fs::read_to_string(log).unwrap_or_default()
    );
    dir
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn a_failed_create_preserves_the_whiteout() {
    run_regression("failed-create", |mount, upper| {
        std::fs::create_dir(mount.join("blocked")).unwrap();
        std::fs::write(mount.join("blocked/leaf.txt"), b"lower").unwrap();
        std::fs::write(upper.join("blocked"), b"blocks creation").unwrap();
        std::fs::create_dir_all(upper.join(".agvfs-wh/blocked")).unwrap();
        std::fs::write(upper.join(".agvfs-wh/blocked/leaf.txt.wh"), b"").unwrap();
    });
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn a_failed_whiteout_returns_a_delete_error() {
    run_regression("failed-delete", |mount, upper| {
        std::fs::write(mount.join("leaf.txt"), b"lower").unwrap();
        std::fs::write(upper.join(".agvfs-wh"), b"blocks marker creation").unwrap();
    });
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn native_a_relative_rename_targets_the_upper() {
    let root = run_regression("relative-rename", |mount, upper| {
        std::fs::create_dir(mount.join("folder")).unwrap();
        std::fs::write(mount.join("folder/destination.txt"), b"lower").unwrap();
        std::fs::write(upper.join("origin.txt"), b"upper source").unwrap();
    });
    assert_eq!(
        std::fs::read(root.path().join("mount/folder/destination.txt")).unwrap(),
        b"lower"
    );
    assert_eq!(
        std::fs::read(root.path().join("upper/folder/destination.txt")).unwrap(),
        b"upper source"
    );
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn native_a_lower_hard_link_is_denied() {
    let root = run_regression("lower-link", |mount, _| {
        std::fs::write(mount.join("leaf.txt"), b"lower").unwrap();
    });
    assert!(!root.path().join("mount/link.txt").exists());
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn native_a_lower_hard_link_ex_is_denied() {
    let root = run_regression("lower-link-ex", |mount, _| {
        std::fs::write(mount.join("leaf.txt"), b"lower").unwrap();
    });
    assert!(!root.path().join("mount/link.txt").exists());
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn native_delete_leaves_the_lower_file_and_writes_a_whiteout() {
    let root = run_regression("native-delete", |mount, _| {
        std::fs::write(mount.join("leaf.txt"), b"lower").unwrap();
    });
    assert_eq!(
        std::fs::read(root.path().join("mount/leaf.txt")).unwrap(),
        b"lower"
    );
    assert!(root.path().join("upper/.agvfs-wh/leaf.txt.wh").is_file());
}

#[test]
#[ignore = "starts a probe under agora_vfs.dll"]
fn restarting_a_directory_scan_keeps_its_first_pattern() {
    run_regression("listing-restart", |mount, upper| {
        std::fs::create_dir(mount.join("folder")).unwrap();
        std::fs::create_dir(upper.join("folder")).unwrap();
        std::fs::write(mount.join("folder/keep.txt"), b"match").unwrap();
        std::fs::write(upper.join("folder/skip.bin"), b"no match").unwrap();
    });
}

/// A configuration that is named but cannot be read: the process was meant to run under the VFS
/// and cannot, so the DLL ends it before the program's own code runs, instead of letting it run
/// unprotected until a launcher's timeout.
#[test]
#[ignore = "starts a suspended process under agora_vfs.dll"]
fn an_unreadable_configuration_ends_the_process_before_it_runs() {
    let code = run_with_dll_as_first_import(Some(r"C:\no\such\agora-vfs-config.json"));
    assert_eq!(code, Some(EXIT_UNPROTECTED), "the process was not ended");
}

/// The process the DLL ends is named in the log, with the reason, on one line a launcher can parse
/// (`agora_core::game_launch::processes_ended_by_vfs`), even when the configuration that would
/// have named the log is the thing that could not be read.
#[test]
#[ignore = "starts a suspended process under agora_vfs.dll"]
fn an_ended_process_is_named_in_the_log() {
    let dir = std::env::temp_dir().join(format!("agora-vfs-ended-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("vfs.log");
    let _ = std::fs::remove_file(&log);
    let code =
        run_with_dll_as_first_import_logging(Some(r"C:\no\such\agora-vfs-config.json"), Some(&log));
    assert_eq!(code, Some(EXIT_UNPROTECTED), "the process was not ended");
    let text = std::fs::read_to_string(&log).expect("the DLL logged why it ended the process");
    let (_, exe) = artifacts();
    let line = text
        .lines()
        .find(|l| l.contains("ending the process "))
        .unwrap_or_else(|| panic!("no ending line in {text:?}"));
    assert!(line.starts_with('['), "{line}");
    assert!(
        line.ends_with(&format!(
            "ending the process {}: the configuration could not be loaded",
            exe.display()
        )),
        "{line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// No configuration at all (a child started with its own environment block): nothing to
/// enforce, so the DLL installs nothing and the program runs normally.
#[test]
#[ignore = "starts a suspended process under agora_vfs.dll"]
fn no_configuration_installs_nothing_and_the_program_runs() {
    assert_eq!(run_with_dll_as_first_import(None), Some(0));
}

#[test]
#[ignore = "starts a suspended process under agora_vfs.dll"]
fn successful_dll_attach_does_not_write_startup_diagnostics() {
    let (dll, exe) = artifacts();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    let log = dir.path().join("vfs.log");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "mount": dir.path().join("mount"),
            "upper": dir.path().join("upper"),
            "dll": dll,
            "log": log,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut cmd = Command::new(exe);
    cmd.env("AGORA_VFS_CONFIG", config);
    assert_eq!(run_fixture(cmd, &dll), Some(0));
    assert!(
        !log.exists(),
        "attach wrote diagnostics under the loader lock: {:?}",
        std::fs::read_to_string(log)
    );
}
