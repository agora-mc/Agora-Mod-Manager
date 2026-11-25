//! What a Minecraft snapshot owns, declared by the package (MASTER_SPEC §32.7).
//!
//! Core reads the declaration instead of holding a folder list of its own, so these tests pin the
//! list core now reads and prove a restore stays inside it.

use agora_core::snapshot;
use std::fs;
use std::path::Path;

/// The file set a full Minecraft snapshot captured, as the code before the declaration produced
/// it: the same folders `TRACKED_ENTRIES` named, in the same shape.
const GOLDEN: &[&str] = &[
    "config/a.toml",
    "datapacks/pack.zip",
    "defaultconfigs/b.toml",
    "instance_manifest.json",
    "kubejs/server.js",
    "mods/a.jar",
    "mods/b.jar.disabled",
    "options.txt",
    "resourcepacks/pack.zip",
    "saves/world/level.dat",
    "shaderpacks/shader.zip",
];

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// One instance holding a file in every tracked entry, plus the two files a snapshot must not own:
/// one at the instance root and one in a folder the package does not declare.
fn instance() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "mods/a.jar", b"mod a");
    write(root, "mods/b.jar.disabled", b"mod b");
    write(root, "config/a.toml", b"config");
    write(root, "defaultconfigs/b.toml", b"config");
    write(root, "kubejs/server.js", b"script");
    write(root, "resourcepacks/pack.zip", b"pack");
    write(root, "shaderpacks/shader.zip", b"shader");
    write(root, "datapacks/pack.zip", b"data");
    write(root, "saves/world/level.dat", b"world");
    write(root, "options.txt", b"options");
    write(root, "instance_manifest.json", b"{}");
    write(
        root,
        "notes.txt",
        b"the user's own file, not snapshot state",
    );
    write(root, "undeclared/keep.txt", b"a folder Agora does not own");
    dir
}

#[test]
fn the_declaration_core_reads_is_the_list_the_package_publishes() {
    let declaration = agora_game_minecraft::snapshot_scope::declaration();
    assert_eq!(
        declaration.tracked_entries(),
        agora_game_minecraft::snapshot_scope::tracked_entries()
    );
    assert_eq!(
        declaration.prelaunch_entries(),
        agora_game_minecraft::snapshot_scope::prelaunch_tracked_entries()
    );
}

#[test]
fn a_full_snapshot_captures_exactly_the_golden_file_set() {
    let dir = instance();
    let declaration = agora_game_minecraft::snapshot_scope::declaration();
    let scope = declaration.tracked_entries();
    let taken = snapshot::create_snapshot(dir.path(), Some("golden"), &scope).unwrap();

    let mut captured: Vec<String> = snapshot::snapshot_file_entries(dir.path(), &taken.id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.relative_path)
        .collect();
    captured.sort();
    assert_eq!(captured, GOLDEN);
}

#[test]
fn a_restore_touches_only_the_declared_folders() {
    let dir = instance();
    let declaration = agora_game_minecraft::snapshot_scope::declaration();
    let scope = declaration.tracked_entries();
    let taken = snapshot::create_snapshot(dir.path(), Some("before"), &scope).unwrap();

    // Change a tracked file, and both files the snapshot does not own.
    write(dir.path(), "mods/a.jar", b"changed");
    write(dir.path(), "notes.txt", b"edited since the snapshot");
    write(
        dir.path(),
        "undeclared/keep.txt",
        b"edited since the snapshot",
    );

    let outcome = snapshot::restore_snapshot(dir.path(), &taken.id, &scope).unwrap();

    assert_eq!(fs::read(dir.path().join("mods/a.jar")).unwrap(), b"mod a");
    assert_eq!(
        fs::read(dir.path().join("notes.txt")).unwrap(),
        b"edited since the snapshot",
        "a file at the instance root is not snapshot state"
    );
    assert_eq!(
        fs::read(dir.path().join("undeclared/keep.txt")).unwrap(),
        b"edited since the snapshot",
        "a folder the package does not declare is not snapshot state"
    );

    let mut restored = outcome.restored_roots.clone();
    restored.sort();
    let mut declared: Vec<String> = scope.iter().map(|entry| (*entry).to_string()).collect();
    declared.sort();
    assert_eq!(restored, declared);
    assert!(outcome.preserved_roots.is_empty());
}
