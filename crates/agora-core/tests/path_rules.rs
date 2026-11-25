//! The path rules that need a real folder to judge (MASTER_SPEC §32.3): `join_under` and
//! `resolve_destination`.
//!
//! They live in this crate's test suite rather than beside the code in `agora-game-api` because
//! they need a temporary folder, and the game contract crate's dependency budget is serde,
//! serde_json, semver, thiserror and regex only (`scripts/check_architecture.py`). Core is the
//! consumer these tests speak for.

use agora_game_api::{
    compare_folder, is_link, join_under, resolve_destination, same_spelling, same_target,
    PathError, PathRelation, RelPath, ReparseTag,
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
fn a_stored_value_the_strict_rule_refuses_is_reported_by_name() {
    // A value written before the strict rule existed: it loads, and it is refused by name when a
    // writer is about to create it, rather than dropped or written as a different file.
    let stored = RelPath::new("mods/BuildCraft.").unwrap();
    match stored.check_strict() {
        Err(error) => {
            assert_eq!(error.value, "mods/BuildCraft.");
            assert!(error.reason().contains("dot"), "{}", error.reason());
        }
        other => panic!("a trailing dot cannot be written as named, got {other:?}"),
    }
}

#[test]
fn a_path_that_is_the_root_is_not_a_file_path() {
    // The empty path is the root, so no write may name it. It loads as a stored value (the root has
    // always been spelled that way) and the strict rule, which a writer applies, refuses it.
    let stored = RelPath::new("").unwrap();
    assert!(
        stored.check_strict().is_err(),
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
    assert_eq!(
        is_link(&link),
        Ok(true),
        "the reparse point redirects a name, so it is a link"
    );
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
fn a_path_that_cannot_be_resolved_is_not_said_to_be_the_same() {
    // The folder is a file, so nothing below it can be resolved: canonicalize answers
    // "not a directory" (a dangling link, a link loop or a missing permission answers the same way).
    let root = temp_dir();
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let under_a_file = file.join("child");
    match compare_folder(&under_a_file, &under_a_file) {
        PathRelation::CantTell(why) => assert!(
            why.contains("a-file"),
            "the reason says what could not be read: {why}"
        ),
        other => {
            panic!("the same unresolvable path is a question, not a positive identity: {other:?}")
        }
    }
    assert!(matches!(
        compare_folder(&under_a_file, &file),
        PathRelation::CantTell(_)
    ));
}

#[test]
fn two_paths_that_are_genuinely_absent_are_compared_by_spelling() {
    let root = temp_dir();
    let one = root.path().join("gone").join("mods");
    let same = root.path().join("gone").join("MODS");
    // Windows and macOS compare these as one name; Linux does not.
    let folded = cfg!(windows) || cfg!(target_os = "macos");
    assert_eq!(
        compare_folder(&one, &same),
        if folded {
            PathRelation::Same
        } else {
            PathRelation::Different
        }
    );
    assert_eq!(
        compare_folder(&one, &root.path().join("gone").join("other")),
        PathRelation::Different
    );
}

#[test]
fn a_path_that_exists_and_one_that_does_not_are_two_things() {
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();
    assert_eq!(
        compare_folder(&root.path().join("mods"), &root.path().join("gone")),
        PathRelation::Different
    );
}

#[cfg(unix)]
#[test]
fn resolving_refuses_a_dangling_link_on_the_way() {
    let root = temp_dir();
    let gone = root.path().join("gone");
    std::os::unix::fs::symlink(&gone, root.path().join("link")).unwrap();
    match resolve_destination(&root.path().join("link").join("a.jar")) {
        Err(PathError::Unresolvable { .. }) => {}
        other => panic!("a link whose target is gone has no destination: {other:?}"),
    }
}

#[cfg(windows)]
#[test]
fn resolving_refuses_a_dangling_junction_on_the_way() {
    let root = temp_dir();
    let gone = root.path().join("gone");
    make_junction(&root.path().join("link"), &gone);
    match resolve_destination(&root.path().join("link").join("a.jar")) {
        Err(PathError::Unresolvable { .. }) => {}
        other => panic!("a junction whose target is gone has no destination: {other:?}"),
    }
}

#[test]
fn resolving_refuses_a_path_under_a_file() {
    let root = temp_dir();
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    match resolve_destination(&file.join("child").join("a.jar")) {
        Err(PathError::Unresolvable { .. }) => {}
        other => panic!("nothing can be written under a file: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn joining_refuses_a_root_that_is_itself_a_link() {
    let root = temp_dir();
    let real = temp_dir();
    std::os::unix::fs::symlink(real.path(), root.path().join("instance")).unwrap();
    let rel = RelPath::strict("mods/a.jar").unwrap();
    match join_under(&root.path().join("instance"), &rel) {
        Err(PathError::LinkOnTheWay { .. }) => {}
        other => panic!("the root itself is a link, so nothing is written through it: {other:?}"),
    }
}

#[cfg(windows)]
#[test]
fn joining_refuses_a_root_that_is_itself_a_junction() {
    let root = temp_dir();
    let real = temp_dir();
    make_junction(&root.path().join("instance"), real.path());
    let rel = RelPath::strict("mods/a.jar").unwrap();
    match join_under(&root.path().join("instance"), &rel) {
        Err(PathError::LinkOnTheWay { .. }) => {}
        other => {
            panic!("the root itself is a junction, so nothing is written through it: {other:?}")
        }
    }
}

#[test]
fn joining_refuses_a_root_that_is_a_file() {
    let root = temp_dir();
    let file = root.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let rel = RelPath::strict("child/a.jar").unwrap();
    match join_under(&file, &rel) {
        Err(PathError::NotAFolder { .. }) => {}
        other => panic!("a file is not an instance root: {other:?}"),
    }
    // And a file met on the way inside the root is refused too.
    std::fs::write(root.path().join("child"), b"x").unwrap();
    match join_under(root.path(), &rel) {
        Err(PathError::NotAFolder { path, .. }) => assert_eq!(path, "child"),
        other => panic!("nothing can be written under a file: {other:?}"),
    }
}

#[test]
fn a_plain_folder_is_not_a_link() {
    // A plain folder is not a link, so the same code path says no.
    let root = temp_dir();
    std::fs::create_dir_all(root.path().join("mods")).unwrap();
    let plain = root.path().join("mods");
    assert_eq!(
        is_link(&plain),
        Ok(false),
        "a plain directory is not a link"
    );
    assert!(
        !same_spelling(root.path(), &root.path().join("mods")),
        "different folders are different spellings"
    );
}

#[test]
fn only_a_reparse_tag_that_wraps_a_name_redirects_one() {
    // The tags Windows documents. The first three are name surrogates (a symbolic link, a junction,
    // a mount point); the last two carry data and stand for nothing else — a cloud placeholder that
    // keeps a file on OneDrive, and a deduplicated file. A real placeholder cannot be made on this
    // machine (it needs a synced account) and a deduplicated file needs ReFS, so the classification
    // is tested on the tags themselves; the junction above is the end-to-end case.
    for tag in [0xA000_000C, 0xA000_0003, 0xA000_0004] {
        assert!(
            ReparseTag::of(tag).redirects_a_name(),
            "{tag:#010X} is a name surrogate"
        );
    }
    for tag in [0x9000_001A, 0x8000_0013, 0x8000_0017, 0x9000_001C] {
        assert!(
            !ReparseTag::of(tag).redirects_a_name(),
            "{tag:#010X} carries data rather than redirecting a name"
        );
    }
}
