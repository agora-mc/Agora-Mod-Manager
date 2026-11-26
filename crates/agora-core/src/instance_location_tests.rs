use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agora_game_api::{
    BaseMode, BaseReference, GameId, InstallId, Layer, LayerId, LayerSource, LayerStack,
    RuntimeIdentity,
};
use sha2::{Digest, Sha256};

use crate::content_store::{self, ContentError};
use crate::ctx::Ctx;
use crate::game_base::{self, BaseError};
use crate::game_instance::{self, trash, GameInstanceManifest};
use crate::lock_manager::{LockGuard, LockManager, LockResource};
use crate::snapshot::{self, SnapshotAux, SnapshotOrigin};
use rusqlite::OptionalExtension;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanPoint {
    LiveInstances,
    InstanceFolders,
    LiveSnapshots,
    SnapshotFolders,
}

struct PauseHook(Box<dyn Fn(ScanPoint) + Send + Sync>);

pub(crate) fn pause(ctx: &Ctx, point: ScanPoint) {
    if let Some(hook) = ctx.extensions.get::<PauseHook>() {
        (hook.0)(point);
    }
}

fn fixture() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(dir.path().to_path_buf());
    crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    (dir, ctx)
}

fn instance(ctx: &Ctx, id: &str, item: Option<&str>) -> PathBuf {
    let mut manifest = GameInstanceManifest::new(
        GameId::new("test-game").unwrap(),
        id,
        id,
        None,
        BaseReference::Unpinned {
            install: InstallId::new("steam:1").unwrap(),
            reason: "test fixture".into(),
        },
    );
    if let Some(item) = item {
        manifest.layers = LayerStack::new(vec![Layer {
            id: LayerId::new("content-1").unwrap(),
            enabled: true,
            mount_path: Default::default(),
            source_path: Default::default(),
            source: LayerSource::Content {
                content: item.into(),
            },
            whiteouts: Vec::new(),
            own_copy: false,
        }])
        .unwrap();
    }
    let dir = ctx.paths.instance_dir(id).unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("tracked.txt"), b"shared snapshot bytes").unwrap();
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES (?1, ?2, ?1, datetime('now'))",
        rusqlite::params![id, manifest.game.as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO game_instance_bases (instance_id, base_json) VALUES (?1, ?2)",
        rusqlite::params![id, serde_json::to_string(&manifest.base).unwrap()],
    )
    .unwrap();
    dir
}

fn contend(ctx: &Ctx) -> LockGuard {
    LockManager::new(ctx.paths.locks_root())
        .acquire(LockResource::InstanceLocations, "test contender")
        .unwrap()
}

fn snapshot_move_race(restore: bool) {
    let (_dir, ctx) = fixture();
    let a = instance(&ctx, "first", None);
    let b = instance(&ctx, "second", None);
    let first = snapshot::create_snapshot(&a, None, &["tracked.txt"]).unwrap();
    snapshot::create_snapshot(&b, None, &["tracked.txt"]).unwrap();
    let hash = format!("{:x}", Sha256::digest(b"shared snapshot bytes"));
    let blob = snapshot::snapshot_blob_path(&a, &hash);
    assert!(blob.is_file());
    let entry = restore.then(|| {
        game_instance::delete(&ctx, "second")
            .unwrap()
            .trash_entry_id
            .unwrap()
    });
    let pause_root = if restore {
        ctx.paths.instances_root()
    } else {
        ctx.paths.recently_deleted_games_root()
    };
    let fired = AtomicBool::new(false);
    let locations = contend(&ctx);
    snapshot::delete_snapshot_with(
        &a,
        &first.id,
        |dir| {
            let entries = snapshot::list_snapshot_scan_dir(dir);
            if dir == pause_root && !fired.swap(true, Ordering::SeqCst) {
                attempt_move(&ctx, entry.as_deref());
            }
            entries
        },
        &locations,
    )
    .unwrap();
    assert!(fired.load(Ordering::SeqCst), "the scan reached the pause");
    assert!(
        blob.is_file(),
        "cleanup deleted a blob still needed by the moving instance"
    );
    assert_eq!(
        b.is_dir(),
        !restore,
        "the move must wait or refuse while scanning"
    );
}

#[test]
fn snapshot_blobs_survive_restore_between_listings() {
    snapshot_move_race(true);
}

#[test]
fn snapshot_blobs_survive_delete_after_listings() {
    snapshot_move_race(false);
}

#[test]
fn snapshot_delete_refuses_contended_location_lock() {
    let (_dir, ctx) = fixture();
    let a = instance(&ctx, "first", None);
    let first = snapshot::create_snapshot(&a, None, &["tracked.txt"]).unwrap();
    let hash = format!("{:x}", Sha256::digest(b"shared snapshot bytes"));
    let blob = snapshot::snapshot_blob_path(&a, &hash);
    let _held = contend(&ctx);
    let delete = snapshot::delete_snapshot(&a, &first.id);
    assert!(blob.is_file(), "a blocked scan must leave the blob alone");
    assert!(snapshot::list_snapshots(&a)
        .unwrap()
        .iter()
        .any(|s| s.id == first.id));
    assert!(delete.unwrap_err().contains("instance-locations"));
}

#[test]
fn snapshot_prune_refuses_contended_location_lock() {
    let (_dir, ctx) = fixture();
    let a = instance(&ctx, "first", None);
    let first = snapshot::create_snapshot(&a, None, &["tracked.txt"]).unwrap();
    let hash = format!("{:x}", Sha256::digest(b"shared snapshot bytes"));
    let blob = snapshot::snapshot_blob_path(&a, &hash);
    // Make the blob an orphan so the independent prune would delete it.
    fs::remove_file(
        a.join(".agora_snapshots")
            .join(format!("{}.json", first.id)),
    )
    .unwrap();
    assert!(blob.is_file());
    let _held = contend(&ctx);
    let prune = snapshot::prune_unreferenced_objects(&a);
    assert!(blob.is_file(), "a blocked prune must leave the blob alone");
    assert!(prune.unwrap_err().contains("instance-locations"));
}

fn add_item(ctx: &Ctx, root: &Path) -> (String, PathBuf) {
    let file = root.join("stored.txt");
    let bytes = b"stored content";
    fs::write(&file, bytes).unwrap();
    let id = content_store::add_file(ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let object = ctx
        .paths
        .content_object_path(&format!("{:x}", Sha256::digest(bytes)));
    assert!(object.is_file());
    (id, object)
}

fn attempt_move(ctx: &Ctx, entry: Option<&str>) {
    let result = match entry {
        Some(entry) => trash::restore(ctx, entry, None).map(|_| ()),
        None => game_instance::delete(ctx, "second").map(|_| ()),
    };
    if let Err(error) = result {
        assert!(error.to_string().contains("instance-locations"), "{error}");
    }
}

fn content_move_race(restore: bool, snapshot_only: bool) {
    let (dir, ctx) = fixture();
    let (item, object) = add_item(&ctx, dir.path());
    let b = instance(&ctx, "second", (!snapshot_only).then_some(item.as_str()));
    if snapshot_only {
        snapshot::create_snapshot_full(
            &b,
            None,
            &["tracked.txt"],
            SnapshotOrigin::User,
            &SnapshotAux {
                content_refs: vec![item.clone()],
                ..Default::default()
            },
        )
        .unwrap();
    }
    let entry = restore.then(|| {
        game_instance::delete(&ctx, "second")
            .unwrap()
            .trash_entry_id
            .unwrap()
    });
    let move_ctx = Ctx::for_testing(dir.path().to_path_buf());
    let pause_point = match (snapshot_only, restore) {
        (true, true) => ScanPoint::LiveSnapshots,
        (true, false) => ScanPoint::SnapshotFolders,
        (false, true) => ScanPoint::LiveInstances,
        (false, false) => ScanPoint::InstanceFolders,
    };
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_hook = Arc::clone(&fired);
    ctx.extensions.insert(PauseHook(Box::new(move |point| {
        if point == pause_point && !fired_in_hook.swap(true, Ordering::SeqCst) {
            attempt_move(&move_ctx, entry.as_deref());
        }
    })));

    let removal = content_store::remove_item(&ctx, &item);
    assert!(
        fired.load(Ordering::SeqCst),
        "the scan reached the pause: {removal:?}"
    );
    assert!(
        object.is_file(),
        "cleanup deleted content still needed by the moving instance"
    );
    assert!(ctx.paths.content_item_path(&item).is_file());
    assert!(
        matches!(removal, Err(ContentError::InUse { .. })),
        "{removal:?}"
    );
    assert_eq!(
        b.is_dir(),
        !restore,
        "the move must wait or refuse while scanning"
    );
}

#[test]
fn content_live_manifest_survives_restore_between_listings() {
    content_move_race(true, false);
}

#[test]
fn content_live_manifest_survives_delete_after_listings() {
    content_move_race(false, false);
}

#[test]
fn content_snapshot_survives_restore_between_listings() {
    content_move_race(true, true);
}

#[test]
fn content_snapshot_survives_delete_after_listings() {
    content_move_race(false, true);
}

#[test]
fn content_cleanup_refuses_contended_location_lock() {
    let (dir, ctx) = fixture();
    let (item, object) = add_item(&ctx, dir.path());
    let _held = contend(&ctx);
    let removal = content_store::remove_item_with_options(
        &ctx,
        &item,
        content_store::RemoveOptions {
            ignore_unreadable_snapshots: true,
        },
    );
    assert!(ctx.paths.content_item_path(&item).is_file());
    assert!(object.is_file());
    assert!(removal
        .unwrap_err()
        .to_string()
        .contains("instance-locations"));
}

#[test]
fn generic_purge_keeps_entry_when_locations_are_locked() {
    let (_dir, ctx) = fixture();
    instance(&ctx, "second", None);
    let entry = game_instance::delete(&ctx, "second")
        .unwrap()
        .trash_entry_id
        .unwrap();
    let dir = ctx.paths.recently_deleted_games_root().join(&entry);
    let trash: trash::TrashFn =
        Arc::new(|path| fs::remove_dir_all(path).map_err(|e| e.to_string()));
    let _held = contend(&ctx);
    let purge = trash::purge(&ctx, &entry, &trash);
    assert!(dir.join("instance/tracked.txt").is_file());
    assert!(purge
        .unwrap_err()
        .to_string()
        .contains("instance-locations"));
}

/// A base with a real record and folder, so removal can only be refused by the pinning check or
/// the locations lock — never by a malformed fixture.
fn base_fixture(ctx: &Ctx, tmp: &Path, base_id: &str) -> PathBuf {
    let folder = ctx.paths.bases_dir().join(base_id);
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("game.exe"), b"base bytes").unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 1u32,
        "base_id": base_id,
        "runtime": RuntimeIdentity::minecraft("1.0"),
        "mode": BaseMode::Linked,
        "source_install": "steam:1",
        "source_product": "Test Game",
        "source_location": tmp.join("source"),
        "location": folder,
        "created_unix_ms": 0i64,
        "files": [],
        "skipped": [],
    });
    fs::create_dir_all(ctx.paths.base_manifests_dir()).unwrap();
    fs::write(
        ctx.paths.base_manifest_path(base_id),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    folder
}

fn pinned_instance(ctx: &Ctx, id: &str, base_id: &str) -> PathBuf {
    let manifest = GameInstanceManifest::new(
        GameId::new("test-game").unwrap(),
        id,
        id,
        None,
        BaseReference::Pinned {
            id: base_id.into(),
            runtime: RuntimeIdentity::minecraft("1.0"),
            mode: BaseMode::Linked,
        },
    );
    let dir = ctx.paths.instance_dir(id).unwrap();
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("tracked.txt"), b"shared snapshot bytes").unwrap();
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    conn.execute(
        "INSERT INTO instances (instance_id, game, name, created_at)
         VALUES (?1, ?2, ?1, datetime('now'))",
        rusqlite::params![id, manifest.game.as_str()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO game_instance_bases (instance_id, base_json) VALUES (?1, ?2)",
        rusqlite::params![id, serde_json::to_string(&manifest.base).unwrap()],
    )
    .unwrap();
    dir
}

#[test]
fn base_removal_cannot_race_a_restore_between_pinning_reads() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    pinned_instance(&ctx, "pinned", base_id);
    let entry = game_instance::delete(&ctx, "pinned")
        .unwrap()
        .trash_entry_id
        .unwrap();
    let move_ctx = ctx.clone();
    let fired = AtomicBool::new(false);
    let removal = game_base::remove_base_with(
        &ctx.paths,
        base_id,
        || {
            if !fired.swap(true, Ordering::SeqCst) {
                let restore = trash::restore(&move_ctx, &entry, None);
                assert!(
                    restore.is_err(),
                    "a restore completed while base removal held instance-locations"
                );
                assert!(restore
                    .unwrap_err()
                    .to_string()
                    .contains("instance-locations"));
            }
        },
        |path| std::fs::metadata(path),
        |path| std::fs::remove_file(path),
    );
    assert!(fired.load(Ordering::SeqCst), "the pause was reached");
    assert!(
        matches!(removal, Err(BaseError::InUse { .. })),
        "removal must refuse while the trash entry still pins the base: {removal:?}"
    );
    assert!(
        base_dir.join("game.exe").is_file(),
        "the base survived the race"
    );
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
    assert!(
        ctx.paths
            .recently_deleted_games_root()
            .join(&entry)
            .join("instance/tracked.txt")
            .is_file(),
        "the restore was refused, so the entry is still restorable"
    );
}

/// Paused through the restore's debug-only seam, so the test itself is debug-only: a
/// release profile has no seam to arm and nothing to assert.
#[cfg(debug_assertions)]
#[test]
fn base_removal_cannot_delete_a_base_a_restore_is_publishing() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    pinned_instance(&ctx, "pinned", base_id);
    let entry = game_instance::delete(&ctx, "pinned")
        .unwrap()
        .trash_entry_id
        .unwrap();

    // Park the restore in the window between its rename and its database insert.
    ctx.test_seams
        .set_restore_pause_after_rename_for(Some("pinned"));
    let restore_ctx = ctx.clone();
    let restore_entry = entry.clone();
    let restore = std::thread::spawn(move || {
        trash::restore(&restore_ctx, &restore_entry, None).expect("the restore completes")
    });
    let mut reached = false;
    for _ in 0..600 {
        if ctx.test_seams.restore_pause_after_rename_reached() {
            reached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reached, "the restore never reached its rename");

    // A removal whose between-reads hook lets the parked restore finish: its
    // database read happened before the insert, its Recently-deleted read
    // would happen after the entry is tidied — the exact pairing that must
    // never be able to see "no pins". Keeping the base back at the locations
    // lock is a pass too; then the hook never fires.
    let fired = Arc::new(AtomicBool::new(false));
    let gate = fired.clone();
    let entry_dir = ctx
        .paths
        .recently_deleted_games_root()
        .join(&entry)
        .to_path_buf();
    let removal = game_base::remove_base_with(
        &ctx.paths,
        base_id,
        {
            // The seam to release belongs to this test's context, which the closure below takes.
            let seams = ctx.test_seams.clone();
            move || {
                if !gate.swap(true, Ordering::SeqCst) {
                    seams.set_restore_pause_after_rename_for(None);
                    for _ in 0..600 {
                        if !entry_dir.exists() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    panic!("the restore never tidied its entry");
                }
            }
        },
        |path| std::fs::metadata(path),
        |path| std::fs::remove_file(path),
    );
    if !fired.load(Ordering::SeqCst) {
        // The removal was kept back at the locations lock while the restore still
        // held it. Let the restore commit and publish its row.
        ctx.test_seams.set_restore_pause_after_rename_for(None);
    }
    restore.join().unwrap();

    assert!(
        removal.is_err(),
        "the removal must not delete a base while a restore is publishing its pin: {removal:?}"
    );
    assert!(
        base_dir.join("game.exe").is_file(),
        "a base a restored instance pins was removed from under it"
    );
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
    assert!(
        ctx.paths
            .instance_dir("pinned")
            .unwrap()
            .join("tracked.txt")
            .is_file(),
        "the restored instance is whole"
    );
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM game_instance_bases WHERE instance_id = 'pinned'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| panic!("row read failed: {e}"))
        .unwrap();
    assert_eq!(present, Some(1), "the restored instance's row is published");

    // Either the removal read the pair the dangerous way and had to refuse with
    // the pinning refusal, or it never got past the locations lock and the row
    // now answers for the instance.
    let verdict = if fired.load(Ordering::SeqCst) {
        removal
    } else {
        game_base::remove_base(&ctx.paths, base_id)
    };
    assert!(
        matches!(&verdict, Err(BaseError::InUse { instances }) if instances == &vec!["pinned".to_string()]),
        "the pin is visible to a scan that runs entirely after the restore: {verdict:?}"
    );
}

#[test]
fn base_removal_refuses_contended_location_lock() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    let _held = contend(&ctx);
    let removal = game_base::remove_base(&ctx.paths, base_id);
    assert!(base_dir.is_dir(), "a busy lock must keep the base folder");
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
    assert!(removal
        .unwrap_err()
        .to_string()
        .contains("instance-locations"));
}

#[test]
fn base_removal_keeps_base_when_state_db_metadata_fails() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    let db = ctx.paths.local_state_db();
    let removal = game_base::remove_base_with(
        &ctx.paths,
        base_id,
        || {},
        |path| {
            if path == db {
                Err(std::io::Error::other("metadata unavailable"))
            } else {
                std::fs::metadata(path)
            }
        },
        |path| std::fs::remove_file(path),
    );
    assert!(
        base_dir.is_dir(),
        "an unreadable state db must keep the base"
    );
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
    assert!(removal
        .unwrap_err()
        .to_string()
        .contains("metadata unavailable"));
}

#[test]
fn base_removal_still_reads_recently_deleted_when_the_state_db_is_missing() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    pinned_instance(&ctx, "pinned", base_id);
    let entry = game_instance::delete(&ctx, "pinned")
        .unwrap()
        .trash_entry_id
        .unwrap();
    let db = ctx.paths.local_state_db();
    let removal = game_base::remove_base_with(
        &ctx.paths,
        base_id,
        || {},
        |path| {
            if path == db {
                // A database that was never created means no live pins; the
                // Recently-deleted scan is independent and must still run.
                Err(std::io::Error::from(std::io::ErrorKind::NotFound))
            } else {
                std::fs::metadata(path)
            }
        },
        |path| std::fs::remove_file(path),
    );
    assert!(
        matches!(&removal, Err(BaseError::InUse { instances }) if instances == &vec!["pinned".to_string()]),
        "a missing database is no live pins, not no pins at all: {removal:?}"
    );
    assert!(
        base_dir.join("game.exe").is_file(),
        "the base pinned by a restorable entry is kept"
    );
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
    assert!(
        ctx.paths
            .recently_deleted_games_root()
            .join(&entry)
            .join("instance/tracked.txt")
            .is_file(),
        "the entry is untouched"
    );
}

fn retention_fixture(ctx: &Ctx) -> PathBuf {
    let dir = instance(ctx, "retention", None);
    for i in 0..7 {
        snapshot::create_snapshot(&dir, Some(&format!("pre-launch-{i}")), &["tracked.txt"])
            .unwrap();
    }
    assert_eq!(snapshot::list_snapshots(&dir).unwrap().len(), 7);
    dir
}

#[test]
fn retention_skips_a_round_when_locations_are_locked() {
    let (_dir, ctx) = fixture();
    let instance_dir = retention_fixture(&ctx);
    let _held = contend(&ctx);
    crate::lkg::run_retention(&instance_dir).expect("a busy lock skips the round, not an error");
    assert_eq!(
        snapshot::list_snapshots(&instance_dir).unwrap().len(),
        7,
        "a skipped round deletes nothing"
    );
}

#[test]
fn user_snapshot_delete_waits_for_a_lock_released_within_the_wait() {
    let (_dir, ctx) = fixture();
    let instance_dir = instance(&ctx, "waited", None);
    let snap = snapshot::create_snapshot(&instance_dir, None, &["tracked.txt"]).unwrap();
    let held = contend(&ctx);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
    });
    snapshot::delete_snapshot(&instance_dir, &snap.id)
        .expect("a user delete waits briefly instead of failing on a passing collision");
    releaser.join().unwrap();
    assert!(snapshot::list_snapshots(&instance_dir).unwrap().is_empty());
}

/// MASTER_SPEC §32.8: instance-locations is the last lock taken, and nothing is taken while it is
/// held. A manager that holds it and asks for another lock is refused rather than left to wait on
/// a lock another operation is waiting on the locations lock for.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "instance-locations")]
fn taking_a_lock_while_holding_instance_locations_is_refused() {
    let (_dir, ctx) = fixture();
    let manager = LockManager::new(ctx.paths.locks_root());
    let _held = manager.try_instance_locations("test holder").unwrap();
    let _base = manager
        .acquire(LockResource::Base("a-base".into()), "test waiter")
        .unwrap();
}

/// A restore cut off just after its rename: the files are back under `instances/` with the
/// marker the delete wrote, the trash entry is still there, and no row was published. Finishing
/// it publishes the pin (the row) and clears the entry, and a pinning scan reads the live rows
/// and Recently deleted apart: unless recovery holds instance-locations across both, a base
/// removal can read the rows before the insert and the trash after the tidy and delete a base
/// the instance still needs.
#[test]
fn a_restore_recovery_cannot_publish_a_pin_while_a_base_removal_decides() {
    let (dir, ctx) = fixture();
    let base_id = "test-base";
    let base_dir = base_fixture(&ctx, dir.path(), base_id);
    pinned_instance(&ctx, "pinned", base_id);
    let entry = game_instance::delete(&ctx, "pinned")
        .unwrap()
        .trash_entry_id
        .unwrap();
    // The state a crash leaves: the entry marked, the folder moved, no row yet.
    let entry_dir = ctx.paths.recently_deleted_games_root().join(&entry);
    fs::write(
        entry_dir.join("restoring.json"),
        br#"{"new_id":"pinned","new_name":"pinned"}"#,
    )
    .unwrap();
    fs::write(
        entry_dir
            .join("instance")
            .join(".agora-restore-marker.json"),
        serde_json::json!({ "entry_id": entry }).to_string(),
    )
    .unwrap();
    fs::rename(
        entry_dir.join("instance"),
        ctx.paths.instance_dir("pinned").unwrap(),
    )
    .unwrap();

    // The removal's pause between its two pinning reads runs the recovery, exactly where a
    // startup pass of another Agora program would land.
    let recovery_ctx = ctx.clone();
    let fired = AtomicBool::new(false);
    let warnings: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let removal = game_base::remove_base_with(
        &ctx.paths,
        base_id,
        || {
            if fired.swap(true, Ordering::SeqCst) {
                return;
            }
            let (actions, said) = trash::recover_interrupted_reporting(&recovery_ctx);
            assert!(
                actions.is_empty(),
                "recovery published a pin while a base removal held instance-locations: {actions:?}"
            );
            *warnings.lock().unwrap() = said;
        },
        |path| std::fs::metadata(path),
        |path| std::fs::remove_file(path),
    );
    assert!(fired.load(Ordering::SeqCst), "the pause was reached");
    let said = warnings.lock().unwrap().clone();
    assert!(
        said.iter()
            .any(|warning| warning.contains("instance-locations")),
        "recovery must hold instance-locations, and say which lock kept it back: {said:?}"
    );
    assert!(
        matches!(&removal, Err(BaseError::InUse { instances }) if instances == &vec!["pinned".to_string()]),
        "a base the restorable instance still pins was removed: {removal:?}"
    );
    assert!(
        base_dir.join("game.exe").is_file(),
        "the base pinned by the restorable instance was removed"
    );
    assert!(ctx.paths.base_manifest_path(base_id).is_file());
}
