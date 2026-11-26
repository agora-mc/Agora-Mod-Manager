//! The object collector's own rules (MASTER_SPEC §32.8): what it keeps, what it frees, and what
//! makes it delete nothing at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::content_store::collect::{CollectRequest, LockRequest};
use crate::content_store::{self, pending};
use crate::ctx::Ctx;
use crate::process_identity::{Lookup, ProcessCheck, ProcessIdentity, ProcessView};
use crate::snapshot::{self, SnapshotAux};

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fixture() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(dir.path().to_path_buf());
    crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    // Every fixture writes its objects a moment before the collection runs, so the production hour
    // of grace would keep all of them. The grace itself is exercised by `an_object_inside_the_grace`.
    ctx.test_seams.set_object_grace(Some(Duration::ZERO));
    (dir, ctx)
}

fn instance_with_file(ctx: &Ctx, id: &str, bytes: &[u8]) -> PathBuf {
    let dir = ctx.paths.instance_dir(id).unwrap();
    fs::create_dir_all(dir.join("mods")).unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 2,
        "instance_id": id,
        "name": id,
        "minecraft_version": "1.20.1",
        "loader": "fabric",
        "loader_version": "0.15.11",
        "mods": [],
        "resourcepacks": [],
        "shaders": [],
        "datapacks": [],
        "worlds": []
    });
    fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("mods").join("kept.jar"), bytes).unwrap();
    dir
}

fn make_folder_link(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        made && fs::symlink_metadata(link)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

fn object_exists(ctx: &Ctx, hash: &str) -> bool {
    ctx.paths.content_object_path(hash).is_file()
}

/// A process table a test decides the answers of, so each branch of the liveness check is reachable.
struct FixedView {
    answer: Lookup,
}

impl ProcessView for FixedView {
    fn lookup(&self, _pid: u32) -> Lookup {
        self.answer.clone()
    }

    fn job_members(&self, _job: &str) -> Result<usize, String> {
        Ok(0)
    }
}

fn pending_record(ctx: &Ctx, hashes: &[String], process: ProcessIdentity) -> PathBuf {
    let dir = ctx.paths.content_pending_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-test.json", process.pid));
    let record = serde_json::json!({
        "schema_version": pending::PENDING_SCHEMA_VERSION,
        "process": process,
        "hashes": hashes,
        "operation": "test operation",
        "created_at": "2026-01-01T00:00:00Z",
    });
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    path
}

fn live_identity() -> ProcessIdentity {
    crate::process_identity::capture_self().expect("this process is in the table")
}

// ---------------------------------------------------------------------------
// 1. An owner location that is a link: nothing is deleted, and the path is named
// ---------------------------------------------------------------------------

#[test]
fn a_linked_snapshot_folder_is_named_and_nothing_is_deleted() {
    let (_dir, ctx) = fixture();
    let live = instance_with_file(&ctx, "live", b"live bytes");
    let other = instance_with_file(&ctx, "other", b"other bytes");
    let live_hash = sha256_hex(b"live bytes");
    let snap = snapshot::create_snapshot(&live, None, &["mods"]).unwrap();

    // The other instance's `.agora_snapshots` is a link. Which objects it holds cannot be judged
    // without following a tree this collection was not asked about.
    let elsewhere = ctx
        .paths
        .recently_deleted_games_root()
        .join("moved-snapshots");
    fs::create_dir_all(&elsewhere).unwrap();
    let link = other.join(".agora_snapshots");
    if !make_folder_link(&elsewhere, &link) {
        panic!(
            "this process cannot create a folder link, so the check must not be skipped silently"
        );
    }

    let report = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(
        report.refused(),
        "a linked owner location stops the collection"
    );
    assert!(
        report
            .refusals
            .iter()
            .any(|why| why.contains(&link.display().to_string())),
        "the refusal names the link: {:?}",
        report.refusals
    );
    assert!(object_exists(&ctx, &live_hash), "nothing may be deleted");
    assert!(snap.id.len() > 10);
}

// ---------------------------------------------------------------------------
// 2. A capture that reuses a blob keeps it while a collection runs
// ---------------------------------------------------------------------------

#[test]
fn a_capture_reusing_a_blob_keeps_it_while_a_collection_runs() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"reused bytes");
    let first = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"reused bytes");
    assert!(object_exists(&ctx, &hash), "the capture stored the blob");
    // The first snapshot's manifest goes, so nothing on disk names the blob any more: only the
    // running capture's pending record stands between it and the collection below. The reuse comes
    // from the live-file index the capture keeps, not from the manifest.
    fs::remove_file(
        dir.join(".agora_snapshots")
            .join(format!("{}.json", first.id)),
    )
    .unwrap();

    // A second capture reuses the blob the first one stored: same size, time and identity. The
    // pause sits after that decision and before the manifest that names it is published.
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_pause = Arc::clone(&fired);
    let collect_ctx = Ctx::for_testing(ctx.paths.root().to_path_buf());
    collect_ctx
        .test_seams
        .set_object_grace(Some(Duration::ZERO));
    let pause: snapshot::CapturePause = Arc::new(move || {
        if fired_in_pause.swap(true, Ordering::SeqCst) {
            return;
        }
        // Nothing names the blob yet, but the capture's pending record does.
        let report = crate::content_store::collect::collect_with_ctx(
            &collect_ctx,
            &CollectRequest {
                store: LockRequest::Wait,
                locations: LockRequest::Wait,
                grace: None,
                ignore_unreadable_snapshots: false,
                pause: None,
            },
        );
        assert!(
            !report.refused(),
            "the collection ran: {:?}",
            report.refusals
        );
    });
    let second = snapshot::create_snapshot_full_with_pause(
        &dir,
        None,
        &["mods"],
        snapshot::SnapshotOrigin::User,
        &SnapshotAux::default(),
        Some(&pause),
    )
    .unwrap();
    assert!(
        fired.load(Ordering::SeqCst),
        "the capture reached the pause, so the window was really exercised"
    );
    assert!(
        object_exists(&ctx, &hash),
        "a collection freed a blob a running capture had registered"
    );
    // And the snapshot really restores.
    fs::write(dir.join("mods").join("kept.jar"), b"changed later").unwrap();
    snapshot::restore_snapshot(&dir, &second.id, &["mods"]).unwrap();
    assert_eq!(
        fs::read(dir.join("mods").join("kept.jar")).unwrap(),
        b"reused bytes"
    );
    assert_ne!(first.id, second.id);
}

// ---------------------------------------------------------------------------
// 3. Pending records: unreadable, dead and cannot-tell
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_pending_record_keeps_every_object() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("mod.jar");
    fs::write(&file, b"mod bytes").unwrap();
    let item = content_store::add_file(&ctx, &file, Some("mod"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(b"mod bytes");

    let dir = ctx.paths.content_pending_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("1-broken.json"), b"{ not a record").unwrap();

    content_store::remove_item(&ctx, &item).unwrap();
    assert!(
        object_exists(&ctx, &hash),
        "a pending record that cannot be read may name any object, so nothing is freed"
    );
}

#[test]
fn a_dead_process_pending_record_is_ignored_and_reported() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("mod.jar");
    fs::write(&file, b"dead owner bytes").unwrap();
    let item = content_store::add_file(&ctx, &file, Some("mod"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(b"dead owner bytes");

    // The record names this very object, but its process is provably gone.
    let gone = ProcessIdentity {
        pid: 4_000_000,
        start_time: 1,
        expected_exe: None,
        job: None,
    };
    ctx.test_seams.set_process_view(Some(Arc::new(FixedView {
        answer: Lookup::Absent,
    })));
    let record = pending_record(&ctx, &[hash.clone()], gone);

    content_store::remove_item(&ctx, &item).unwrap();
    assert!(
        !object_exists(&ctx, &hash),
        "a dead operation's record holds nothing: its objects are freed"
    );
    assert!(
        !record.exists(),
        "the record of a dead operation is cleared by the collection"
    );
    assert!(
        ctx.test_seams.process_view_hits() > 0,
        "the liveness check the context carries was consulted"
    );
}

#[test]
fn a_pending_record_that_cannot_be_judged_keeps_its_objects() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("mod.jar");
    fs::write(&file, b"unknown owner bytes").unwrap();
    let item = content_store::add_file(&ctx, &file, Some("mod"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(b"unknown owner bytes");

    ctx.test_seams.set_process_view(Some(Arc::new(FixedView {
        answer: Lookup::CantTell("the process table cannot be read".into()),
    })));
    let record = pending_record(&ctx, &[hash.clone()], live_identity());

    content_store::remove_item(&ctx, &item).unwrap();
    assert!(
        object_exists(&ctx, &hash),
        "cannot tell whether the operation is running, so its objects are kept"
    );
    assert!(record.exists(), "a record that may be in use is left alone");
}

// ---------------------------------------------------------------------------
// 4. A snapshot's blobs, and the grace period
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_blob_survives_until_the_last_snapshot_goes() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"snapshot bytes");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"snapshot bytes");
    assert!(
        object_exists(&ctx, &hash),
        "a capture stores its blobs in the one content store"
    );

    let report = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(!report.refused(), "{:?}", report.refusals);
    assert!(
        object_exists(&ctx, &hash),
        "a snapshot names the object, so a collection keeps it"
    );

    snapshot::delete_snapshot_collecting(&dir, &snap.id, Some(Duration::ZERO)).unwrap();
    assert!(
        !object_exists(&ctx, &hash),
        "deleting the last snapshot that names the object frees it"
    );
}

#[test]
fn an_object_inside_the_grace_period_is_kept_and_freed_after_it() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("recent.jar");
    fs::write(&file, b"recent bytes").unwrap();
    let item = content_store::add_file(&ctx, &file, Some("recent"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(b"recent bytes");

    // The production grace: the object was written a moment ago, so it is kept.
    ctx.test_seams.set_object_grace(None);
    content_store::remove_item(&ctx, &item).unwrap();
    assert!(
        object_exists(&ctx, &hash),
        "an object this new is never freed, whatever the records say"
    );

    // The same collection with the wait over frees it.
    ctx.test_seams.set_object_grace(Some(Duration::ZERO));
    let report = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(!report.refused(), "{:?}", report.refusals);
    assert!(
        !object_exists(&ctx, &hash),
        "past the grace, an orphan goes"
    );
}

// ---------------------------------------------------------------------------
// 5. The legacy store: read, restored from, and collected by the same rule
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_whose_blobs_are_only_in_the_legacy_store_still_restores() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"legacy bytes");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"legacy bytes");

    // What a store captured before the move looks like: the blob in `snapshot-objects`, nothing in
    // the content store.
    let legacy = snapshot::snapshot_blob_path(&dir, &hash);
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let stored = ctx.paths.content_object_path(&hash);
    content_store::protect::unprotect(&stored).unwrap();
    fs::copy(&stored, &legacy).unwrap();
    fs::remove_file(&stored).unwrap();

    fs::write(dir.join("mods").join("kept.jar"), b"changed later").unwrap();
    snapshot::restore_snapshot(&dir, &snap.id, &["mods"]).unwrap();
    assert_eq!(
        fs::read(dir.join("mods").join("kept.jar")).unwrap(),
        b"legacy bytes"
    );
}

#[test]
fn an_unreferenced_legacy_blob_is_collected_while_a_referenced_one_is_kept() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"referenced bytes");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let referenced = sha256_hex(b"referenced bytes");
    let orphan = sha256_hex(b"nobody's bytes");

    let legacy_referenced = snapshot::snapshot_blob_path(&dir, &referenced);
    let legacy_orphan = snapshot::snapshot_blob_path(&dir, &orphan);
    for path in [&legacy_referenced, &legacy_orphan] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    // The referenced blob exists only in the legacy store; the orphan nowhere else.
    let stored = ctx.paths.content_object_path(&referenced);
    content_store::protect::unprotect(&stored).unwrap();
    fs::copy(&stored, &legacy_referenced).unwrap();
    fs::remove_file(&stored).unwrap();
    fs::write(&legacy_orphan, b"nobody's bytes").unwrap();

    let report = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(!report.refused(), "{:?}", report.refusals);
    assert!(
        legacy_referenced.is_file(),
        "the snapshot still names this blob, so it stays"
    );
    assert!(
        !legacy_orphan.exists(),
        "nothing names this blob, so it goes"
    );
    assert!(snap.id.len() > 10);
}

// ---------------------------------------------------------------------------
// 6. Owner records that cannot be read stop every delete
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_instance_manifest_deletes_nothing() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("orphan.jar");
    fs::write(&file, b"nothing names me").unwrap();
    let item = content_store::add_file(&ctx, &file, Some("orphan"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(b"nothing names me");
    content_store::remove_item(&ctx, &item).unwrap();
    // Put the bytes back into the store with no record naming them, so a healthy collection would
    // free them.
    content_store::store_object_bytes(&ctx, b"nothing names me").unwrap();
    let live = instance_with_file(&ctx, "live", b"live bytes");
    let other = ctx.paths.instance_dir("other").unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("instance_manifest.json"), b"{ not a manifest").unwrap();

    let report = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(
        report.refused(),
        "an unreadable manifest stops the collection"
    );
    assert!(
        report
            .refusals
            .iter()
            .any(|why| why.contains(&other.join("instance_manifest.json").display().to_string())),
        "the refusal names the record: {:?}",
        report.refusals
    );
    assert!(
        object_exists(&ctx, &hash),
        "a collection that cannot read every owner record deletes nothing"
    );
    assert!(live.is_dir());
}

#[test]
fn an_unreadable_snapshot_manifest_stops_the_collection_unless_ignored() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"snapshot bytes");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"snapshot bytes");
    fs::write(
        dir.join(".agora_snapshots")
            .join(format!("{}.json", snap.id)),
        b"{ not a manifest",
    )
    .unwrap();

    let refused = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: false,
            pause: None,
        },
    );
    assert!(refused.refused());
    assert!(object_exists(&ctx, &hash));

    let ignored = crate::content_store::collect::collect_with_ctx(
        &ctx,
        &CollectRequest {
            store: LockRequest::Wait,
            locations: LockRequest::Wait,
            grace: None,
            ignore_unreadable_snapshots: true,
            pause: None,
        },
    );
    assert!(!ignored.refused(), "{:?}", ignored.refusals);
    assert!(
        ignored
            .failures
            .iter()
            .any(|why| why
                .contains("a snapshot that cannot be read may lose files it needs to restore")),
        "the risk is named in the output: {:?}",
        ignored.failures
    );
}

// ---------------------------------------------------------------------------
// 7. The same collection frees an item's objects once its record is gone
// ---------------------------------------------------------------------------

#[test]
fn removing_an_item_frees_only_what_nothing_else_names() {
    let (_dir, ctx) = fixture();
    let work = tempfile::tempdir().unwrap();
    let mine = work.path().join("mine.jar");
    fs::write(&mine, b"mine only").unwrap();
    let theirs = work.path().join("theirs.jar");
    fs::write(&theirs, b"theirs only").unwrap();
    let mine_id = content_store::add_file(&ctx, &mine, Some("mine"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let theirs_id = content_store::add_file(&ctx, &theirs, Some("theirs"), None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let mine_hash = sha256_hex(b"mine only");
    let theirs_hash = sha256_hex(b"theirs only");

    content_store::remove_item(&ctx, &mine_id).unwrap();
    assert!(
        !object_exists(&ctx, &mine_hash),
        "the removed item's object goes"
    );
    assert!(
        object_exists(&ctx, &theirs_hash),
        "another item's object is untouched"
    );
    assert!(ctx.paths.content_item_path(&theirs_id).is_file());
}
