//! A snapshot owns what it captured, including a folder the game no longer declares.
//!
//! The reviewers' fixture: capture `characters/hero.sav`, create `characters/new.sav`, stop
//! declaring `characters`, change `hero.sav`, restore (MASTER_SPEC §32.7). Restoring the captured
//! files must not take the files created since with them, and the undo snapshot must cover
//! everything the restore touched.

use agora_core::snapshot::{self, SnapshotAux, SnapshotInputFile, SnapshotOrigin};
use std::fs;
use std::path::Path;

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn read(root: &Path, relative: &str) -> Vec<u8> {
    fs::read(root.join(relative)).unwrap()
}

/// One instance: a folder the game will stop declaring, and one it keeps.
fn instance() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "characters/hero.sav", b"level 1");
    write(dir.path(), "config/a.toml", b"config v1");
    dir
}

fn file(relative_path: &str, contents: &[u8]) -> SnapshotInputFile {
    SnapshotInputFile {
        relative_path: relative_path.to_string(),
        contents: Some(contents.to_vec()),
    }
}

/// The whole-root path: a snapshot of live folders, restored by `restore_snapshot`.
#[test]
fn restoring_a_retired_folder_keeps_files_created_since_the_snapshot() {
    let dir = instance();
    let taken = snapshot::create_snapshot_scoped(
        dir.path(),
        Some("captured both"),
        &["characters", "config"],
    )
    .unwrap();
    write(dir.path(), "characters/new.sav", b"created since");
    write(dir.path(), "characters/hero.sav", b"level 9");

    // The game no longer declares `characters`; `config` is still tracked.
    let outcome = snapshot::restore_snapshot(dir.path(), &taken.id, &["config"]).unwrap();

    assert_eq!(
        read(dir.path(), "characters/hero.sav"),
        b"level 1",
        "the file the snapshot captured comes back"
    );
    assert_eq!(
        read(dir.path(), "characters/new.sav"),
        b"created since",
        "a file created since the snapshot is not swept away with the folder"
    );
    assert_eq!(read(dir.path(), "config/a.toml"), b"config v1");
    assert!(
        outcome
            .restored_roots
            .iter()
            .any(|root| root == "characters"),
        "the retired folder was restored: {:?}",
        outcome.restored_roots
    );
}

/// A folder on the way replaced by a link since the capture is refused, not written through.
///
/// The instance lock makes the check correct: another process can still replace a folder between
/// the check and the write, which is the staged-apply work (C1).
#[cfg(windows)]
#[test]
fn restoring_into_a_folder_a_link_has_replaced_is_refused() {
    use std::process::Command;

    let dir = instance();
    let taken = snapshot::create_snapshot_scoped(
        dir.path(),
        Some("captured both"),
        &["characters", "config"],
    )
    .unwrap();

    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("hero.sav"), b"outside").unwrap();
    fs::remove_dir_all(dir.path().join("characters")).unwrap();
    let link = dir.path().join("characters");
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(outside.path())
        .status()
        .unwrap();
    assert!(status.success(), "the junction was not created");

    // The game no longer declares `characters`, so the restore would write file by file into the
    // folder the junction now stands for.
    let error = snapshot::restore_snapshot(dir.path(), &taken.id, &["config"]).unwrap_err();

    assert!(
        error.contains("goes through the link") || error.contains("refus"),
        "the restore says why it was refused: {error}"
    );
    assert_eq!(
        read(outside.path(), "hero.sav"),
        b"outside",
        "nothing was written outside the instance"
    );
    // Remove the junction before the temporary folder is cleaned up.
    let _ = fs::remove_dir(&link);
}

/// The generic-game path: a data snapshot, restored file by file by `restore_snapshot_data`.
#[test]
fn a_data_snapshot_of_a_folder_the_game_stopped_declaring_keeps_new_files() {
    let dir = instance();
    let taken = snapshot::create_snapshot_from_data(
        dir.path(),
        Some("captured both"),
        &[
            file("characters/hero.sav", b"level 1"),
            file("config/a.toml", b"config v1"),
        ],
        SnapshotOrigin::Automatic,
        &SnapshotAux::default(),
    )
    .unwrap();
    write(dir.path(), "characters/new.sav", b"created since");
    write(dir.path(), "characters/hero.sav", b"level 9");

    snapshot::restore_snapshot_data(dir.path(), &taken.id).unwrap();

    assert_eq!(
        read(dir.path(), "characters/hero.sav"),
        b"level 1",
        "the captured file comes back"
    );
    assert_eq!(
        read(dir.path(), "characters/new.sav"),
        b"created since",
        "a file created since the snapshot is not swept away with the folder"
    );
    assert_eq!(read(dir.path(), "config/a.toml"), b"config v1");
}
