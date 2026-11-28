//! A snapshot owns what it captured, including a folder the game no longer declares.
//!
//! The reviewers' fixture: capture `characters/hero.sav`, create `characters/new.sav`, stop
//! declaring `characters`, change `hero.sav`, restore (MASTER_SPEC §32.7). A retired folder is
//! restored by the same wholesale mechanism as a tracked one — the folder the snapshot recorded is
//! what comes back — and the undo snapshot covers the recorded scope, so the files the restore
//! replaced come back with it.
//!
//! A data snapshot (generic games) owns the files it lists rather than the folders they live in, so
//! a file created since the capture is not its to remove.

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
///
/// A retired folder is not a second restore mechanism: it is moved aside and the captured folder
/// is promoted in its place, exactly like a tracked one. A file created since the capture goes
/// with the folder it sits in, and the undo snapshot (over the recorded scope) puts it back.
#[test]
fn a_retired_folder_is_restored_like_a_tracked_one() {
    let dir = instance();
    let taken = snapshot::create_snapshot_scoped(
        dir.path(),
        Some("captured both"),
        &["characters", "config"],
    )
    .unwrap();
    // The game writes, then stops declaring the folder: `config` is still tracked.
    write(dir.path(), "characters/new.sav", b"created since");
    write(dir.path(), "characters/hero.sav", b"level 9");

    // The undo point a restore takes before it replaces anything, sized to the snapshot's recorded
    // scope the way `SnapshotService::restore` sizes it.
    let recorded = snapshot::restore_scope(dir.path(), &taken.id).unwrap();
    let recorded_refs: Vec<&str> = recorded.iter().map(String::as_str).collect();
    let undo = snapshot::create_snapshot_scoped(dir.path(), Some("undo"), &recorded_refs).unwrap();

    let outcome = snapshot::restore_snapshot(dir.path(), &taken.id, &["config"]).unwrap();

    assert_eq!(
        read(dir.path(), "characters/hero.sav"),
        b"level 1",
        "the file the snapshot captured comes back"
    );
    assert!(
        !dir.path().join("characters/new.sav").exists(),
        "the retired folder is replaced the way a tracked one is"
    );
    assert_eq!(
        read(dir.path(), "config/a.toml"),
        b"config v1",
        "a tracked folder is restored the same way"
    );
    assert!(
        outcome
            .restored_roots
            .iter()
            .any(|root| root == "characters"),
        "the retired folder was restored: {:?}",
        outcome.restored_roots
    );

    let undo_outcome = snapshot::restore_snapshot(dir.path(), &undo.id, &["config"]).unwrap();
    assert_eq!(
        read(dir.path(), "characters/hero.sav"),
        b"level 9",
        "the undo puts back the file the restore replaced"
    );
    assert_eq!(
        read(dir.path(), "characters/new.sav"),
        b"created since",
        "the undo covers the recorded scope, so a file created since the capture comes back"
    );
    assert!(
        undo_outcome
            .restored_roots
            .iter()
            .any(|root| root == "characters"),
        "the undo restores the folder the restore replaced: {:?}",
        undo_outcome.restored_roots
    );
}

/// A folder on the way replaced by a link since the capture is never written through: the restore
/// replaces the folder itself, so nothing lands outside the instance.
///
/// The instance lock makes the check correct: another process can still replace a folder between
/// the check and the write, which is the staged-apply work (C1).
#[cfg(windows)]
#[test]
fn restoring_over_a_folder_a_link_has_replaced_writes_nothing_outside_the_instance() {
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

    // The game no longer declares `characters`, so the restore has to produce that folder while a
    // junction stands where it belongs.
    snapshot::restore_snapshot(dir.path(), &taken.id, &["config"]).unwrap();

    assert_eq!(
        read(dir.path(), "characters/hero.sav"),
        b"level 1",
        "the captured folder is back, as a folder"
    );
    assert!(
        fs::symlink_metadata(dir.path().join("characters"))
            .unwrap()
            .is_dir(),
        "the link was replaced by a real folder"
    );
    assert_eq!(
        read(outside.path(), "hero.sav"),
        b"outside",
        "nothing was written outside the instance"
    );
    assert!(
        !dir.path().join(".agora_pre_restore").exists(),
        "the junction's old place was cleaned up"
    );
}

/// The generic-game path: a data snapshot, restored file by file by `restore_snapshot_data`.
///
/// This path owns the files the manifest lists, not the folders they live in, so it is unchanged by
/// the wholesale rule above.
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
        &["characters", "config"],
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

/// A data restore writes through the strict path rules: a folder on the way that a link replaced
/// since the capture refuses the write instead of landing outside the instance.
#[cfg(windows)]
#[test]
fn a_data_restore_refuses_to_write_through_a_folder_a_link_replaced() {
    use std::process::Command;

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

    let error = snapshot::restore_snapshot_data(dir.path(), &taken.id).unwrap_err();

    assert!(
        error.contains("goes through the link") || error.contains("refus"),
        "the restore says why it was refused: {error}"
    );
    assert_eq!(
        read(outside.path(), "hero.sav"),
        b"outside",
        "nothing was written outside the instance"
    );
    let _ = fs::remove_dir(&link);
}
