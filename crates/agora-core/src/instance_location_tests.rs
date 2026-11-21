use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agora_game_api::{BaseReference, GameId, InstallId, Layer, LayerId, LayerSource, LayerStack};
use sha2::{Digest, Sha256};

use crate::content_store::{self, ContentError};
use crate::ctx::Ctx;
use crate::game_instance::{self, trash, GameInstanceManifest};
use crate::lock_manager::{LockGuard, LockManager, LockResource};
use crate::snapshot::{self, SnapshotAux, SnapshotOrigin};

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
        "INSERT INTO game_instances (instance_id, game, name, base_json, created_at)
         VALUES (?1, ?2, ?1, ?3, datetime('now'))",
        rusqlite::params![
            id,
            manifest.game.as_str(),
            serde_json::to_string(&manifest.base).unwrap()
        ],
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
    snapshot::delete_snapshot_with(&a, &first.id, |dir| {
        let entries = snapshot::list_snapshot_scan_dir(dir);
        if dir == pause_root && !fired.swap(true, Ordering::SeqCst) {
            attempt_move(&ctx, entry.as_deref());
        }
        entries
    })
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
