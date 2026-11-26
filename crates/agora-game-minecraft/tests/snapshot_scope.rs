//! What a Minecraft snapshot owns, declared by the package (MASTER_SPEC §32.7).
//!
//! Core reads the declaration instead of holding a folder list of its own, so these tests pin the
//! list core now reads and prove a restore stays inside it.

use agora_core::{db, snapshot, snapshot_service::SnapshotService};
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
fn fill(root: &Path) {
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
}

fn instance() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fill(dir.path());
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

/// The same file set, through the entry point the CLI and the desktop use: a manual capture goes
/// through `SnapshotService`, which reads the package's declaration. The low-level function above
/// proves the folder list; this proves the path a person's "Create snapshot" takes.
#[test]
fn a_manual_capture_through_the_service_captures_the_golden_file_set() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = agora_game_minecraft::testing_context(tmp.path().join("app_data"));
    db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let dir = ctx.paths.instance_dir("golden-instance").unwrap();
    fill(&dir);
    fs::write(
        dir.join("instance_manifest.json"),
        serde_json::json!({
            "instance_id": "golden-instance",
            "name": "Golden",
            "minecraft_version": "1.21.1",
            "loader": "fabric",
            "loader_version": "0.16.0",
            "mods": [],
        })
        .to_string(),
    )
    .unwrap();

    let snapshot = SnapshotService::new(ctx.clone())
        .create("golden-instance", Some("golden"))
        .expect("the service captures what the package declares");

    let mut captured: Vec<String> = snapshot::snapshot_file_entries(&dir, &snapshot.id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.relative_path)
        .collect();
    captured.sort();
    assert_eq!(captured, GOLDEN);
    assert!(
        !captured
            .iter()
            .any(|path| path == "notes.txt" || path.starts_with("undeclared")),
        "the service owns only the declared folders: {captured:?}"
    );
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

/// The pack-update rollback: the snapshot's scope comes from what the merge plans to write, so a
/// folder the package does not track is inside it. A failed update that wrote `scripts/new.zs`
/// must leave the instance as it was, not with the failed update's file beside the restored one
/// (MASTER_SPEC §32.7).
#[test]
fn a_pack_rollback_removes_a_file_written_to_a_folder_the_package_does_not_track() {
    let dir = instance();
    let tracked = agora_game_minecraft::snapshot_scope::tracked_entries();
    // What `apply_merge` does before it touches the instance: the tracked entries plus the roots
    // the plan will write.
    write(dir.path(), "scripts/old.zs", b"shipped before");
    let scope =
        snapshot::recovery_scope_for_writes(tracked, ["scripts/old.zs", "scripts/new.zs"]).unwrap();
    assert!(
        scope.iter().any(|root| root == "scripts"),
        "the plan's writes bring scripts/ into the recovery scope: {scope:?}"
    );
    let scope_refs: Vec<&str> = scope.iter().map(String::as_str).collect();
    let taken =
        snapshot::create_snapshot_scoped(dir.path(), Some("pack-merge"), &scope_refs).unwrap();

    // The merge writes, then fails somewhere later in the protected region.
    write(dir.path(), "scripts/old.zs", b"the update's version");
    write(
        dir.path(),
        "scripts/new.zs",
        b"the failed update's new script",
    );
    write(dir.path(), "mods/a.jar", b"the update's jar");

    snapshot::restore_snapshot(dir.path(), &taken.id, tracked).unwrap();

    assert!(
        !dir.path().join("scripts/new.zs").exists(),
        "the rollback removes the file the failed update created"
    );
    assert_eq!(
        fs::read(dir.path().join("scripts/old.zs")).unwrap(),
        b"shipped before"
    );
    assert_eq!(fs::read(dir.path().join("mods/a.jar")).unwrap(), b"mod a");
}
