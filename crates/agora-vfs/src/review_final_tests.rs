//! Final-review probes of the pure mapping and decision functions (no injected process).
//!
//! Each test whose name starts `bug_` proves a bug and is `#[ignore]`d so the suite stays green;
//! run them with `cargo test -p agora-vfs --lib review_final -- --ignored`.

use crate::config::{RuntimeConfig, VfsConfig};
use crate::hooks::{plan_open, Plan};
use crate::nt::*;
use crate::paths::{add_whiteout, resolve, Resolved, Via};
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
#[ignore = "proves bug: a '..' in the rel makes copy-up write a file outside the upper layer"]
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
#[ignore = "proves bug: creating an alternate data stream on a lower file leaves an empty upper file that shadows it"]
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
#[ignore = "proves bug: a directory whiteout does not hide the lower files below it"]
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
#[ignore = "proves bug: the real mount folder is opened with DELETE/write rights and tracked as writable"]
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
#[ignore = "proves bug: the config accepts an upper layer that is also a lower layer"]
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
#[ignore = "proves bug: the config accepts an empty upper layer (writes go to the current directory)"]
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
#[ignore = "proves bug: a panic inside guarded() leaves the thread unprotected (hooks pass through for good)"]
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
