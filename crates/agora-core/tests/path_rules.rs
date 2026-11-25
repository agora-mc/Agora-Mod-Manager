//! The path rules that need a real folder to judge (MASTER_SPEC §32.3): `join_under` and
//! `resolve_destination`.
//!
//! They live in this crate's test suite rather than beside the code in `agora-game-api` because
//! they need a temporary folder, and the game contract crate's dependency budget is serde,
//! serde_json, semver, thiserror and regex only (`scripts/check_architecture.py`). Core is the
//! consumer these tests speak for.

use agora_game_api::{
    is_link, join_under, resolve_destination, same_spelling, same_target, PathError, RelPath,
};
use std::path::{Path, PathBuf};

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary folder")
}

#[test]
fn joining_under_a_root_returns_the_path_it_checked() {
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();

    let rel = RelPath::strict("mods/a.jar").unwrap();
    assert_eq!(
        join_under(root.path(), &rel).unwrap(),
        root.path().join("mods").join("a.jar")
    );

    // A file that does not exist yet is the ordinary case for a write, and so is a folder below it.
    let deep = RelPath::strict("mods/sub/dir/b.jar").unwrap();
    assert_eq!(
        join_under(root.path(), &deep).unwrap(),
        root.path()
            .join("mods")
            .join("sub")
            .join("dir")
            .join("b.jar")
    );
}

#[test]
fn joining_reports_a_stored_value_the_strict_rule_refuses() {
    // A value written before the strict rule existed: it loads, and it is refused by name when a
    // writer is about to create it, rather than dropped or written as a different file.
    let root = temp_dir();
    let stored = RelPath::new("mods/BuildCraft.").unwrap();
    match join_under(root.path(), &stored) {
        Err(PathError::UnsafePath { path, reason }) => {
            assert_eq!(path, "mods/BuildCraft.");
            assert!(reason.contains("dot"), "{reason}");
        }
        other => panic!("a trailing dot cannot be written as named, got {other:?}"),
    }
}

#[test]
fn a_path_that_is_the_root_is_not_a_file_path() {
    let root = temp_dir();
    let stored = RelPath::new("").unwrap();
    assert!(
        matches!(
            join_under(root.path(), &stored),
            Err(PathError::UnsafePath { .. })
        ),
        "the empty path is the root, so no write may name it"
    );
}

#[cfg(unix)]
#[test]
fn joining_refuses_a_symlink_on_the_way() {
    let root = temp_dir();
    let outside = temp_dir();
    std::os::unix::fs::symlink(outside.path(), root.path().join("mods")).unwrap();

    let rel = RelPath::strict("mods/a.jar").unwrap();
    match join_under(root.path(), &rel) {
        Err(PathError::LinkOnTheWay { path, .. }) => assert_eq!(path, "mods"),
        other => panic!("a symlink on the way must be refused, got {other:?}"),
    }
    assert!(
        !outside.path().join("a.jar").exists(),
        "nothing was written through the link"
    );
}

#[cfg(windows)]
#[test]
fn a_windows_junction_is_a_reparse_point() {
    // What this machine does, kept as the reason `join_under` asks for the reparse-point attribute
    // instead of `file_type().is_symlink()`: std does not report a junction as a symbolic link.
    let root = temp_dir();
    let outside = temp_dir();
    let link = root.path().join("mods");
    make_junction(&link, outside.path());

    let metadata = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        metadata.file_type().is_symlink(),
        "this machine's std reports a junction as a symbolic link (Rust 1.100, Windows 11)"
    );
    assert!(
        !metadata.file_type().is_dir(),
        "and it reports no directory type for it, so asking for a reparse point is precise"
    );
    assert!(is_link(&metadata), "the reparse point is still visible");
}

#[cfg(windows)]
#[test]
fn joining_refuses_a_junction_on_the_way() {
    let root = temp_dir();
    let outside = temp_dir();
    make_junction(&root.path().join("mods"), outside.path());

    let rel = RelPath::strict("mods/a.jar").unwrap();
    match join_under(root.path(), &rel) {
        Err(PathError::LinkOnTheWay { path, .. }) => assert_eq!(path, "mods"),
        other => panic!("a junction on the way must be refused, got {other:?}"),
    }
    assert!(
        !outside.path().join("a.jar").exists(),
        "nothing was written through the junction"
    );
}

#[cfg(windows)]
fn make_junction(link: &Path, target: &Path) {
    // A junction needs no administrator rights, unlike a symbolic link.
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("cmd runs");
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn resolving_a_destination_gives_one_value_to_check_and_write() {
    let root = temp_dir();
    let existing = root.path().join("mods");
    std::fs::create_dir_all(&existing).unwrap();

    let resolved = resolve_destination(&existing.join("a.jar")).unwrap();
    assert!(resolved.is_absolute(), "{}", resolved.display());
    assert!(resolved.ends_with("a.jar"), "{}", resolved.display());
    let parent: PathBuf = resolved
        .parent()
        .expect("a file has a parent")
        .to_path_buf();
    assert!(
        same_target(&parent, &existing.canonicalize().unwrap()),
        "the folder is resolved once, and the file name is appended to it: {}",
        resolved.display()
    );
}

#[test]
fn resolving_refuses_a_path_that_walks_up() {
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();
    // Windows collapses `..` lexically even under a folder that does not exist; Unix does not. One
    // path may not name two targets, so a destination with `..` is refused on both.
    for path in [
        root.path().join("mods").join("..").join("x"),
        root.path()
            .join("mods")
            .join("missing")
            .join("..")
            .join("x"),
    ] {
        match resolve_destination(&path) {
            Err(PathError::Unresolvable { .. }) => {}
            other => panic!("a '..' is refused rather than resolved twice: {other:?}"),
        }
    }
}

#[test]
fn a_resolved_destination_drops_a_dot_component_and_keeps_the_file_name() {
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();
    let resolved = resolve_destination(&root.path().join("mods").join(".").join("a.jar"))
        .expect("a folder that exists resolves");
    assert!(resolved.ends_with("a.jar"), "{}", resolved.display());
    assert_eq!(
        resolved,
        resolve_destination(&root.path().join("mods").join("a.jar")).unwrap(),
        "a '.' component names the same destination"
    );
}

#[cfg(unix)]
#[test]
fn one_folder_reached_through_a_link_is_one_folder() {
    use agora_game_api::same_folder;
    let root = temp_dir();
    let real = root.path().join("real");
    std::fs::create_dir_all(real.join("mods")).unwrap();
    std::os::unix::fs::symlink(&real, root.path().join("alias")).unwrap();
    assert!(
        same_folder(&real.join("mods"), &root.path().join("alias").join("mods")),
        "two spellings of one folder are one folder once the link is resolved"
    );
}

#[test]
fn a_plain_folder_is_not_a_link() {
    // A plain folder is not a link, so the same code path says no.
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();
    let plain = std::fs::symlink_metadata(root.path().join("mods")).unwrap();
    assert!(!is_link(&plain), "a plain directory is not a reparse point");
    assert!(
        !same_spelling(root.path(), &root.path().join("mods")),
        "different folders are different spellings"
    );
}
