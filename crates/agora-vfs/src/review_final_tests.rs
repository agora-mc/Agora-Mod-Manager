//! Final-review probes of the pure mapping and decision functions (no injected process).
//!
//! Each test whose name starts `bug_` proved a bug and now guards its fix.

use crate::config::{RuntimeConfig, VfsConfig};
use crate::hooks::{plan_open, Plan};
use crate::nt::*;
use crate::paths::{
    add_whiteout, normalize_rel, note_close, rel_of_handle, resolve, upper_for_write, whited_out,
    Resolved, Via,
};
use crate::Tracked;
use std::path::{Path, PathBuf};

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agvfs-final-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn config(mount: &Path, upper: &Path, lowers: &[&Path]) -> RuntimeConfig {
    let json = serde_json::json!({
        "version": 1,
        "mount": mount,
        "upper": upper,
        "lowers": lowers,
        "dll": mount.join("agora_vfs.dll"),
    });
    VfsConfig::from_json_str(&json.to_string()).unwrap()
}

/// Control: a plain write to a lower file is copied up and the lower file is untouched.
#[test]
fn a_plain_write_to_a_lower_file_is_copied_up_and_leaves_the_lower_alone() {
    let root = temp_root("control");
    let game = root.join("game");
    std::fs::create_dir_all(game.join("Data")).unwrap();
    std::fs::write(game.join("Data").join("a.esp"), b"LOWER").unwrap();
    let c = config(&game, &root.join("upper"), &[&game]);
    let plan = plan_open(
        r"Data\a.esp".to_string(),
        Via::Mount,
        FILE_WRITE_DATA,
        FILE_OPEN,
        0,
        &c,
    );
    match plan {
        Plan::Open { path, .. } => assert!(path.starts_with(root.join("upper"))),
        _ => panic!("expected an open of the upper copy"),
    }
    assert_eq!(
        std::fs::read(game.join("Data").join("a.esp")).unwrap(),
        b"LOWER"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `locate` hands `plan_open` a rel that is never normalised, so a `..` in an NT name (a direct
/// NtCreateFile caller; Win32 callers are normalised before they reach NT) walks out of the upper
/// layer. NTFS refuses a `..` in the final open, but the copy-up before it is `fs::copy`, which is
/// Win32 and does resolve `..`: it puts a copy of a lower-side file at any path the `..` reaches on
/// the upper side (and `upper_for_write` creates the folders on the way).
#[test]
fn bug_dotdot_in_rel_copies_a_file_outside_the_upper_layer() {
    let root = temp_root("dotdot");
    // lower is three folders deep, upper two, so the same rel names different files on each side.
    let lower = root.join("p").join("q").join("game");
    let upper = root.join("inst").join("writable");
    std::fs::create_dir_all(&lower).unwrap();
    std::fs::create_dir_all(&upper).unwrap();
    // rel = ..\..\x\f  =>  lower side: <root>\p\x\f   upper side: <root>\x\f
    let source = root.join("p").join("x").join("f");
    let outside = root.join("x").join("f");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"LOWER SIDE FILE").unwrap();

    let c = config(&lower, &upper, &[&lower]);
    let _ = plan_open(
        r"..\..\x\f".to_string(),
        Via::Mount,
        FILE_WRITE_DATA,
        FILE_OPEN,
        0,
        &c,
    );
    assert!(
        !outside.exists(),
        "copy-up created {} outside the upper layer {}",
        outside.display(),
        upper.display()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Opening `Data\a.esp:stream` for write when only `a.esp` exists below: the stream is not found
/// in any lower, so the open is a create in the upper layer. NTFS creates `upper\Data\a.esp` as an
/// empty file carrying the stream, and that empty file now wins over the real `a.esp` below.
#[test]
fn bug_creating_a_stream_on_a_lower_file_shadows_the_file_with_an_empty_one() {
    let root = temp_root("ads");
    let game = root.join("game");
    std::fs::create_dir_all(game.join("Data")).unwrap();
    std::fs::write(game.join("Data").join("a.esp"), b"REAL PLUGIN BYTES").unwrap();
    let c = config(&game, &root.join("upper"), &[&game]);

    let plan = plan_open(
        r"Data\a.esp:Zone.Identifier".to_string(),
        Via::Mount,
        FILE_WRITE_DATA,
        FILE_OPEN_IF,
        0,
        &c,
    );
    let Plan::Open { path, .. } = plan else {
        // Refusing or copying the main stream first would both be fine.
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    // What the hook's NtCreateFile of the redirected path does.
    std::fs::write(&path, b"[ZoneTransfer]").unwrap();

    match resolve(r"Data\a.esp", &c) {
        Resolved::Upper(p) => {
            let len = std::fs::metadata(&p).unwrap().len();
            assert_ne!(
                len, 0,
                "the game now reads an empty a.esp instead of the real one"
            );
        }
        other => assert_eq!(other, Resolved::Lower(game.join("Data").join("a.esp"))),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The whiteout of a directory hides the directory from its parent's listing, but `resolve` only
/// looks for a marker of the exact path, so every file below it is still reachable by path (and the
/// delete hook reports success for a non-empty lower directory). The core's own deployment read
/// of the same markers treats a marker as a prefix (`game_deploy.rs`, "key.starts_with(&prefix)").
#[test]
fn bug_a_directory_whiteout_leaves_the_files_below_it_reachable() {
    let root = temp_root("dirwh");
    let game = root.join("game");
    std::fs::create_dir_all(game.join("Meshes")).unwrap();
    std::fs::write(game.join("Meshes").join("a.nif"), b"x").unwrap();
    let c = config(&game, &root.join("upper"), &[&game]);

    add_whiteout("Meshes", &c.upper);
    assert_eq!(resolve("Meshes", &c), Resolved::WhitedOut);
    assert_eq!(
        resolve(r"Meshes\a.nif", &c),
        Resolved::WhitedOut,
        "the folder is deleted, so the file below it must be gone too"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The mount folder itself is opened for real with the caller's access, and the handle is tracked
/// as an upper (writable) one. A rename or delete through it then passes to the real folder:
/// `nt_set_information_file` only intercepts handles with `lower: true`.
#[test]
fn bug_the_mount_folder_handle_keeps_delete_and_write_rights() {
    let root = temp_root("mount");
    let game = root.join("game");
    std::fs::create_dir_all(&game).unwrap();
    let c = config(&game, &root.join("upper"), &[&game]);
    let plan = plan_open(
        String::new(),
        Via::Mount,
        DELETE_ACCESS | FILE_WRITE_ATTRIBUTES,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
        &c,
    );
    match plan {
        Plan::Open { access, track, .. } => {
            assert!(
                access & (DELETE_ACCESS | WRITE_RIGHTS) == 0 || track.lower,
                "the real game folder is open with access {access:#x} and tracked as writable"
            );
        }
        Plan::Fail(_) => {}
        Plan::Passthrough => panic!("unexpected passthrough"),
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Mapping tables with the upper layer also listed as a lower: writes would land in a layer that is
/// meant to be read-only. The configuration checks aliases against everything, but not the upper
/// against the lowers.
#[test]
fn bug_config_accepts_the_upper_layer_as_a_lower_layer() {
    let json = r#"{"version": 1, "mount": "D:\\Games\\G", "upper": "D:\\Bases\\base",
        "lowers": ["D:\\Games\\G", "D:\\Bases\\base"], "dll": "C:\\d.dll"}"#;
    assert!(
        VfsConfig::from_json_str(json).is_err(),
        "writes to the upper layer would change a lower layer"
    );
}

/// An empty upper layer is accepted: `upper = ""` becomes a relative path, so writes would go to
/// the process's current directory (the mount path, by contrast, is checked for emptiness).
#[test]
fn bug_config_accepts_an_empty_upper_layer() {
    let json = r#"{"version": 1, "mount": "D:\\Games\\G", "upper": "",
        "lowers": ["D:\\Games\\G"], "dll": "C:\\d.dll"}"#;
    assert!(VfsConfig::from_json_str(json).is_err());
}

/// `guarded` sets the per-thread "inside" flag and clears it after the closure, with no guard for
/// a panic. A hook that panics inside `guarded` (any `unwrap` on a bad name or a poisoned lock) is
/// caught by `catch_hook_panic`, which answers that one call with ACCESS_DENIED, but the flag stays
/// set: from then on every hook on that thread sees `is_inside()`, plans nothing and passes the
/// call straight to the real file system, so the thread's writes are no longer redirected.
#[test]
fn bug_a_panic_inside_guarded_leaves_the_thread_unprotected() {
    use crate::util::{catch_hook_panic, guarded, is_inside};
    assert!(!is_inside());
    let status = catch_hook_panic("probe", || {
        guarded(|| -> NTSTATUS { panic!("a hook bug") }).unwrap_or(0)
    });
    assert_eq!(
        status, STATUS_ACCESS_DENIED,
        "the panic is turned into a refusal"
    );
    assert!(
        !is_inside(),
        "after the caught panic the thread still skips every hook"
    );
}

/// A path longer than the old 1024-unit buffer made `GetFinalPathNameByHandleW` fail, so the call
/// passed through to the real file system unredirected. The lookup now grows the buffer (and a
/// write whose path still cannot be read is refused, not passed through; the hook is the only way
/// to prove that part, see the hand-back).
#[test]
fn a_handle_path_over_1023_characters_is_still_located() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let root = temp_root("longpath");
    let mut deep = root.clone();
    for _ in 0..6 {
        deep = deep.join("d".repeat(200));
    }
    std::fs::create_dir_all(&deep).unwrap();
    assert!(deep.as_os_str().len() > 1023);
    let c = config(&root, &temp_root("longpath-upper"), &[&root]);
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
        .open(&deep)
        .unwrap();
    let handle = dir.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let located = unsafe { rel_of_handle(handle, &c) }
        .expect("the handle's path is readable")
        .expect("the path is inside the mount");
    assert_eq!(located.rel.split('\\').count(), 6);
    drop(dir);
    let _ = std::fs::remove_dir_all(&root);
}

/// Delete-on-close of a copied-up file deletes the upper copy at `NtClose`; without a marker the
/// lower original reappears. `note_close` is the decision the hook makes (the hook itself only
/// runs under injection).
#[test]
fn delete_on_close_of_a_copied_up_file_leaves_a_whiteout() {
    let root = temp_root("delclose");
    let game = root.join("game");
    std::fs::create_dir_all(game.join("Data")).unwrap();
    std::fs::write(game.join("Data").join("a.esp"), b"LOWER").unwrap();
    let c = config(&game, &root.join("upper"), &[&game]);
    let upper = upper_for_write(r"Data\a.esp", &c.upper);
    std::fs::write(&upper, b"UPPER COPY").unwrap();
    std::fs::remove_file(&upper).unwrap();
    let t = Tracked {
        rel: r"Data\a.esp".to_string(),
        via: Via::Mount,
        lower: false,
        delete_on_close: true,
        dir: false,
    };
    note_close(&t, &c);
    assert!(whited_out(r"Data\a.esp", &c.upper));
    assert_eq!(resolve(r"Data\a.esp", &c), Resolved::WhitedOut);
    let _ = std::fs::remove_dir_all(&root);
}

/// `..` (above the root), a leading separator, a stream or a device name must never become a path
/// on disk.
#[test]
fn names_that_could_leave_the_upper_layer_are_refused() {
    assert_eq!(
        normalize_rel(r"Data\mod.esp"),
        Some(r"Data\mod.esp".to_string())
    );
    assert_eq!(
        normalize_rel(r".\Data\..\mod.esp"),
        Some(r"mod.esp".to_string())
    );
    for bad in [
        r"..\x",
        r"a\..\..\x",
        r"\x",
        r"/x",
        r"C:x",
        r"Data\a.esp:stream",
        r"CON",
        r"Data\NUL.txt",
    ] {
        assert_eq!(normalize_rel(bad), None, "{bad}");
    }
}
