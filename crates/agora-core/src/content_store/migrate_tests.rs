//! The snapshot-object migration's own rules (MASTER_SPEC §32.8, "Migration").
//!
//! Before B3a, snapshots kept their blobs in a second store, `<root>/snapshot-objects/<xx>/<hash>`.
//! The migration moves each one into the one content store, re-hashed; keeps and reports anything
//! whose bytes do not match its name; resumes after a crash (a journal on the §32.11 envelope); and,
//! once it is done, fallback reads stop until another migration run starts.
//!
//! These tests were written before the code (owner's routing, 2026-11-26). The names below are the
//! API the brief asks for; if one has to change, say why in the report — what each test asserts
//! may not change.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::content_store::collect::CollectRequest;
use crate::content_store::migrate::{self, LegacyStoreState, MigrationOutcome};
use crate::content_store::{self, protect};
use crate::ctx::Ctx;
use crate::lock_manager::{LockManager, LockResource};
use crate::snapshot;

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fixture() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(dir.path().to_path_buf());
    crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    // Collections in these tests must not keep an object only because it is new.
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

fn legacy_root(ctx: &Ctx) -> PathBuf {
    ctx.paths.root().join("snapshot-objects")
}

fn legacy_path(ctx: &Ctx, hash: &str) -> PathBuf {
    legacy_root(ctx).join(&hash[..2]).join(hash)
}

/// Put `bytes` in the legacy store under `name` (normally their own hash), as a pre-B3a build did.
fn plant_legacy(ctx: &Ctx, name: &str, bytes: &[u8]) -> PathBuf {
    let path = legacy_path(ctx, name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

/// Move an object a capture just stored back to where a pre-B3a capture would have put it.
fn demote_to_legacy(ctx: &Ctx, hash: &str) -> PathBuf {
    let stored = ctx.paths.content_object_path(hash);
    protect::unprotect(&stored).unwrap();
    let legacy = legacy_path(ctx, hash);
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::copy(&stored, &legacy).unwrap();
    fs::remove_file(&stored).unwrap();
    legacy
}

fn object_bytes(ctx: &Ctx, hash: &str) -> Option<Vec<u8>> {
    fs::read(ctx.paths.content_object_path(hash)).ok()
}

fn done(outcome: MigrationOutcome) -> migrate::MigrationReport {
    match outcome {
        MigrationOutcome::Done(report) => report,
        other => panic!("expected the migration to finish, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. What a finished migration leaves behind
// ---------------------------------------------------------------------------

#[test]
fn verified_objects_move_into_the_content_store_and_the_legacy_copies_go() {
    let (_dir, ctx) = fixture();
    let a = plant_legacy(&ctx, &sha256_hex(b"alpha"), b"alpha");
    let b = plant_legacy(&ctx, &sha256_hex(b"bravo"), b"bravo");
    let c = plant_legacy(&ctx, &sha256_hex(b"charlie"), b"charlie");

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.moved, 3, "{report:?}");
    assert!(report.mismatched.is_empty(), "{report:?}");
    for (path, bytes) in [(&a, &b"alpha"[..]), (&b, b"bravo"), (&c, b"charlie")] {
        assert!(!path.exists(), "the legacy copy {path:?} is gone");
        let hash = sha256_hex(bytes);
        assert_eq!(object_bytes(&ctx, &hash).as_deref(), Some(bytes));
        // Placed by the store's one writer, so it is protected like any other object.
        assert_eq!(
            protect::protection(&ctx.paths.content_object_path(&hash)).unwrap(),
            protect::Protection::Protected
        );
    }
    assert!(!migrate::legacy_store_active(&ctx.paths));
}

#[test]
fn a_snapshot_captured_before_the_move_restores_from_the_content_store_afterwards() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"legacy bytes");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"legacy bytes");
    let legacy = demote_to_legacy(&ctx, &hash);

    done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert!(!legacy.exists());
    fs::write(dir.join("mods").join("kept.jar"), b"changed later").unwrap();
    snapshot::restore_snapshot(&dir, &snap.id, &["mods"]).unwrap();
    assert_eq!(
        fs::read(dir.join("mods").join("kept.jar")).unwrap(),
        b"legacy bytes"
    );
}

#[test]
fn a_snapshot_with_an_uppercase_manifest_hash_restores_after_migration() {
    let (_dir, ctx) = fixture();
    let bytes = b"legacy bytes";
    let dir = instance_with_file(&ctx, "live", bytes);
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(bytes);
    let upper = hash.to_ascii_uppercase();
    let legacy = demote_to_legacy(&ctx, &hash);
    fs::remove_file(legacy).unwrap();
    let legacy = plant_legacy(&ctx, &upper, bytes);
    let manifest = snapshot::snapshot_manifest_path(&dir, &snap.id);
    let json = fs::read_to_string(&manifest).unwrap();
    assert!(json.contains(&hash));
    fs::write(&manifest, json.replace(&hash, &upper)).unwrap();

    assert_eq!(
        snapshot::snapshot_blob_source(&dir, &upper, bytes.len() as u64).unwrap(),
        Some(legacy.clone())
    );
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert!(!legacy.exists());
    let source = snapshot::snapshot_blob_source(&dir, &upper, bytes.len() as u64).unwrap();
    assert_eq!(source, Some(ctx.paths.content_object_path(&hash)));
    fs::write(dir.join("mods/kept.jar"), b"changed later").unwrap();
    snapshot::restore_snapshot(&dir, &snap.id, &["mods"]).unwrap();
    assert_eq!(fs::read(dir.join("mods/kept.jar")).unwrap(), bytes);
}

#[test]
fn a_legacy_object_whose_bytes_do_not_match_its_name_is_kept_and_reported() {
    let (_dir, ctx) = fixture();
    let claimed = sha256_hex(b"what the name promises");
    let bad = plant_legacy(&ctx, &claimed, b"what is really there");
    let good = plant_legacy(&ctx, &sha256_hex(b"fine"), b"fine");

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(report.mismatched, vec![bad.clone()], "{report:?}");
    assert_eq!(
        fs::read(&bad).unwrap(),
        b"what is really there",
        "kept exactly as it was"
    );
    assert!(!good.exists());
    assert!(
        object_bytes(&ctx, &claimed).is_none(),
        "a mismatched file never enters the store under the name it claims"
    );
}

#[test]
fn a_good_copy_already_in_the_store_is_kept_and_the_legacy_duplicate_goes() {
    let (_dir, ctx) = fixture();
    let hash = content_store::store_object_bytes(&ctx, b"twice").unwrap();
    let legacy = plant_legacy(&ctx, &hash, b"twice");

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.already_present, 1, "{report:?}");
    assert_eq!(report.moved, 0, "{report:?}");
    assert!(!legacy.exists());
    assert_eq!(object_bytes(&ctx, &hash).as_deref(), Some(&b"twice"[..]));
}

#[test]
fn a_matching_writable_destination_is_protected_before_the_legacy_copy_goes() {
    let (_dir, ctx) = fixture();
    let hash = sha256_hex(b"twice");
    let legacy = plant_legacy(&ctx, &hash, b"twice");
    let destination = ctx.paths.content_object_path(&hash);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(&destination, b"twice").unwrap();

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.already_present, 1);
    assert_eq!(
        protect::protection(&destination).unwrap(),
        protect::Protection::Protected
    );
    assert!(!legacy.exists());
}

#[test]
fn a_destination_that_changes_before_unlink_leaves_the_legacy_copy_and_receipt() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    for change_before_blob_unlink in [false, true] {
        let (_dir, ctx) = fixture();
        let hash = sha256_hex(b"last good copy");
        let legacy = plant_legacy(&ctx, &hash, b"last good copy");
        crate::artifact_receipt::record_verified(&legacy, "sha256", &hash, Some(14)).unwrap();
        let receipt = crate::artifact_receipt::receipt_path(&legacy);
        let destination = ctx.paths.content_object_path(&hash);
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let blob = legacy.clone();
        ctx.test_seams
            .set_snapshot_migration_before_unlink(Arc::new(move |unlink| {
                counter.fetch_add(1, Ordering::Relaxed);
                if (unlink == blob) == change_before_blob_unlink {
                    protect::unprotect(&destination).unwrap();
                    fs::write(&destination, b"short").unwrap();
                }
            }));

        let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();

        assert_eq!(
            hits.load(Ordering::Relaxed),
            if change_before_blob_unlink { 2 } else { 1 }
        );
        assert!(
            matches!(outcome, MigrationOutcome::NeedsHand(migrate::MigrationRefusal::Other(reason)) if reason.contains(&hash))
        );
        assert_eq!(fs::read(&legacy).unwrap(), b"last good copy");
        assert_eq!(receipt.exists(), !change_before_blob_unlink);
        assert_eq!(
            migrate::legacy_store_state(&ctx.paths),
            LegacyStoreState::Active
        );
        let journal = fs::read(migrate::journal_path(&ctx.paths)).unwrap();
        let journal: serde_json::Value = serde_json::from_slice(&journal).unwrap();
        assert_eq!(journal["state"], "running");
    }
}

/// The content store trusts a protected object of the right size without hashing it. The
/// migration is about to delete the only other copy, so it must not: a damaged object of the same
/// size, protected, would otherwise mask the good legacy bytes forever.
#[test]
fn a_damaged_store_object_of_the_same_size_is_replaced_by_the_good_legacy_copy() {
    let (_dir, ctx) = fixture();
    let good: &[u8] = b"0123456789-good";
    let hash = sha256_hex(good);
    let stored = ctx.paths.content_object_path(&hash);
    fs::create_dir_all(stored.parent().unwrap()).unwrap();
    fs::write(&stored, b"0123456789-BAD!").unwrap();
    protect::protect(&stored).unwrap();
    let legacy = plant_legacy(&ctx, &hash, good);

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.replaced_damaged, 1, "{report:?}");
    assert_eq!(object_bytes(&ctx, &hash).as_deref(), Some(good));
    assert!(!legacy.exists());
}

// ---------------------------------------------------------------------------
// 2. Done means the old store is neither read nor swept
// ---------------------------------------------------------------------------

#[test]
fn after_the_migration_the_legacy_store_is_not_read() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"unused");
    fs::create_dir_all(legacy_root(&ctx)).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    // Something put a valid-looking object back in the old store after the migration finished.
    let late = plant_legacy(&ctx, &sha256_hex(b"late"), b"late");
    assert_eq!(
        snapshot::snapshot_blob_source(&dir, &sha256_hex(b"late"), 4).unwrap(),
        None,
        "a finished migration stops the fallback read"
    );
    assert!(late.exists());
}

#[test]
fn the_next_migration_run_moves_an_object_that_arrived_after_done() {
    let (_dir, ctx) = fixture();
    fs::create_dir_all(legacy_root(&ctx)).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    let hash = sha256_hex(b"late");
    let late = plant_legacy(&ctx, &hash, b"late");
    ctx.test_seams
        .set_snapshot_migration_stop_after_place(Some(1));

    assert!(migrate::migrate_snapshot_objects(&ctx).is_err());
    assert_eq!(ctx.test_seams.snapshot_migration_stops(), 1);
    assert!(migrate::legacy_store_active(&ctx.paths));
    assert!(late.exists());

    ctx.test_seams.set_snapshot_migration_stop_after_place(None);
    assert_eq!(
        done(migrate::migrate_snapshot_objects(&ctx).unwrap()).moved,
        1
    );
    assert!(!late.exists());
    assert_eq!(object_bytes(&ctx, &hash).as_deref(), Some(&b"late"[..]));
    assert!(!migrate::legacy_store_active(&ctx.paths));
}

#[test]
fn reopened_migration_keeps_completed_counts_and_updates_findings() {
    let (_dir, ctx) = fixture();
    plant_legacy(&ctx, &sha256_hex(b"good"), b"good");
    let duplicate = sha256_hex(b"duplicate");
    plant_legacy(&ctx, &duplicate, b"duplicate");
    let damaged = sha256_hex(b"damaged copy");
    plant_legacy(&ctx, &damaged, b"damaged copy");
    for (hash, bytes) in [(&duplicate, &b"duplicate"[..]), (&damaged, &b"bad"[..])] {
        let destination = ctx.paths.content_object_path(hash);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, bytes).unwrap();
    }
    let repair_hash = sha256_hex(b"repaired");
    let repair = plant_legacy(&ctx, &repair_hash, b"wrong");
    let mismatch = plant_legacy(&ctx, &sha256_hex(b"still mismatched"), b"also wrong");
    let stray = legacy_root(&ctx).join("stray.tmp");
    fs::write(&stray, b"left alone").unwrap();

    let first = done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    assert_eq!(
        (first.moved, first.already_present, first.replaced_damaged),
        (1, 1, 1)
    );
    assert_eq!(first.mismatched.len(), 2);
    assert_eq!(first.left_alone.len(), 1);

    assert!(matches!(
        migrate::migrate_snapshot_objects(&ctx).unwrap(),
        MigrationOutcome::NothingToDo
    ));
    let quiet = migrate::migration_status(&ctx.paths).unwrap().unwrap();
    assert_eq!(
        (quiet.moved, quiet.already_present, quiet.replaced_damaged),
        (1, 1, 1)
    );
    assert_eq!(quiet.mismatched, first.mismatched);
    assert_eq!(quiet.left_alone, first.left_alone);

    let new_stray = legacy_root(&ctx).join("new.tmp");
    fs::write(&new_stray, b"also left alone").unwrap();
    fs::write(&repair, b"repaired").unwrap();
    ctx.test_seams
        .set_snapshot_migration_stop_after_place(Some(1));
    assert!(migrate::migrate_snapshot_objects(&ctx).is_err());
    assert_eq!(ctx.test_seams.snapshot_migration_stops(), 1);
    ctx.test_seams.set_snapshot_migration_stop_after_place(None);
    let repaired = done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    assert_eq!(
        (
            repaired.moved,
            repaired.already_present,
            repaired.replaced_damaged
        ),
        (1, 0, 0)
    );
    assert_eq!(repaired.mismatched, vec![mismatch]);
    let mut expected_left_alone = first.left_alone;
    expected_left_alone.push((new_stray, expected_left_alone[0].1.clone()));
    expected_left_alone.sort_unstable();
    assert_eq!(repaired.left_alone, expected_left_alone);
    assert!(!repair.exists());
    let completed = migrate::migration_status(&ctx.paths).unwrap().unwrap();
    assert_eq!(
        (
            completed.moved,
            completed.already_present,
            completed.replaced_damaged
        ),
        (2, 1, 1)
    );
}

#[test]
fn migration_status_distinguishes_an_unreadable_record_from_absence() {
    let (_dir, ctx) = fixture();
    assert!(migrate::migration_status(&ctx.paths).unwrap().is_none());
    fs::create_dir_all(legacy_root(&ctx)).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    let path = migrate::journal_path(&ctx.paths);
    let mut finished: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    finished["report"] = serde_json::Value::Null;
    fs::write(&path, serde_json::to_vec(&finished).unwrap()).unwrap();

    let error =
        migrate::migration_status(&ctx.paths).expect_err("the finished report is unreadable");
    assert!(error.contains(&path.display().to_string()), "{error}");
}

#[test]
fn startup_is_quiet_for_an_unchanged_mismatch_and_migrates_it_after_repair() {
    let (_dir, ctx) = fixture();
    let hash = sha256_hex(b"promised");
    let bad = plant_legacy(&ctx, &hash, b"delivered");
    assert_eq!(
        done(migrate::migrate_snapshot_objects(&ctx).unwrap()).mismatched,
        vec![bad.clone()]
    );

    let path = migrate::journal_path(&ctx.paths);
    let journal_before = fs::read(&path).unwrap();
    let quiet = crate::startup_recovery::run_at_startup(&ctx);
    assert!(
        quiet.actions.is_empty() && quiet.warnings.is_empty(),
        "{quiet:?}"
    );
    assert!(!migrate::legacy_store_active(&ctx.paths));
    assert_eq!(fs::read(&bad).unwrap(), b"delivered");
    assert_eq!(fs::read(&path).unwrap(), journal_before);

    // Change the mtime while keeping the size fixed.
    let older = fs::metadata(&bad).unwrap().modified().unwrap() - Duration::from_secs(60);
    fs::write(&bad, b"bad bytes").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&bad)
        .unwrap()
        .set_modified(older)
        .unwrap();
    let changed = crate::startup_recovery::run_at_startup(&ctx);
    assert!(
        changed.warnings.iter().any(|line| line.contains(&hash)),
        "{changed:?}"
    );
    let quiet = crate::startup_recovery::run_at_startup(&ctx);
    assert!(
        quiet.actions.is_empty() && quiet.warnings.is_empty(),
        "{quiet:?}"
    );

    fs::write(&bad, b"promised").unwrap();
    let repaired = crate::startup_recovery::run_at_startup(&ctx);
    assert!(
        repaired.actions.iter().any(|line| line.contains("1 moved")),
        "{repaired:?}"
    );
    assert!(!bad.exists());
    assert_eq!(object_bytes(&ctx, &hash).as_deref(), Some(&b"promised"[..]));
}

#[test]
fn a_legacy_root_replaced_by_a_file_after_done_needs_a_hand() {
    let (_dir, ctx) = fixture();
    let root = legacy_root(&ctx);
    fs::create_dir_all(&root).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    fs::remove_dir(&root).unwrap();
    fs::write(&root, b"unexpected file").unwrap();

    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();
    assert!(
        matches!(outcome, MigrationOutcome::NeedsHand(_)),
        "{outcome:?}"
    );
}

#[test]
fn a_legacy_root_replaced_by_a_link_after_done_needs_a_hand() {
    let (_dir, ctx) = fixture();
    let root = legacy_root(&ctx);
    fs::create_dir_all(&root).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    fs::remove_dir(&root).unwrap();
    let target = ctx.paths.root().join("replacement");
    fs::create_dir(&target).unwrap();
    assert!(make_folder_link(&target, &root));

    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();
    assert!(
        matches!(outcome, MigrationOutcome::NeedsHand(migrate::MigrationRefusal::LegacyLink(path)) if path == root)
    );
}

#[test]
fn after_the_migration_a_collection_leaves_the_legacy_store_alone() {
    let (_dir, ctx) = fixture();
    let _dir_live = instance_with_file(&ctx, "live", b"unused");
    let bad = plant_legacy(&ctx, &sha256_hex(b"promised"), b"delivered");
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    let orphan_hash = sha256_hex(b"an old orphan");
    let orphan = ctx.paths.content_object_path(&orphan_hash);
    fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    fs::write(&orphan, b"an old orphan").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&orphan)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 3600))
        .unwrap();

    let report = content_store::collect::collect_with_ctx(&ctx, &CollectRequest::default());

    assert!(!report.refused(), "{report:?}");
    assert!(
        report.deleted.contains(&orphan_hash) && !orphan.exists(),
        "{report:?}"
    );
    assert!(
        bad.exists(),
        "a kept mismatch is not swept once the store is retired: {report:?}"
    );
}

#[test]
fn before_the_migration_the_fallback_read_still_works() {
    let (_dir, ctx) = fixture();
    let dir = instance_with_file(&ctx, "live", b"still legacy");
    let snap = snapshot::create_snapshot(&dir, None, &["mods"]).unwrap();
    let hash = sha256_hex(b"still legacy");
    let legacy = demote_to_legacy(&ctx, &hash);

    assert!(migrate::legacy_store_active(&ctx.paths));
    assert_eq!(
        snapshot::snapshot_blob_source(&dir, &hash, b"still legacy".len() as u64).unwrap(),
        Some(legacy)
    );
    fs::write(dir.join("mods").join("kept.jar"), b"changed").unwrap();
    snapshot::restore_snapshot(&dir, &snap.id, &["mods"]).unwrap();
}

#[cfg(windows)]
#[test]
fn an_unreadable_legacy_blob_is_reported_and_kept() {
    use std::os::windows::fs::OpenOptionsExt;

    let (_dir, ctx) = fixture();
    let bytes = b"still legacy";
    let dir = instance_with_file(&ctx, "live", bytes);
    let hash = sha256_hex(bytes);
    let legacy = plant_legacy(&ctx, &hash, bytes);
    let exclusive_handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&legacy)
        .unwrap();

    let error = snapshot::snapshot_blob_source(&dir, &hash, bytes.len() as u64).unwrap_err();
    assert!(error.contains("could not check snapshot object"), "{error}");
    assert!(migrate::migrate_snapshot_objects(&ctx).is_err());
    assert!(migrate::legacy_store_active(&ctx.paths));

    drop(exclusive_handle);
    assert_eq!(
        snapshot::snapshot_blob_source(&dir, &hash, bytes.len() as u64).unwrap(),
        Some(legacy)
    );
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
}

// ---------------------------------------------------------------------------
// 3. Nothing to do, busy, and a damaged journal
// ---------------------------------------------------------------------------

#[test]
fn with_no_legacy_store_nothing_is_written() {
    let (_dir, ctx) = fixture();
    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();
    assert!(
        matches!(outcome, MigrationOutcome::NothingToDo),
        "{outcome:?}"
    );
    assert!(!migrate::journal_path(&ctx.paths).exists());
    assert!(!legacy_root(&ctx).exists());
    assert!(!migrate::legacy_store_active(&ctx.paths));
}

#[test]
fn an_empty_legacy_store_finishes_with_nothing_moved() {
    let (_dir, ctx) = fixture();
    fs::create_dir_all(legacy_root(&ctx)).unwrap();
    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    assert_eq!(
        report.moved + report.already_present + report.replaced_damaged,
        0
    );
    assert!(!migrate::legacy_store_active(&ctx.paths));
}

#[test]
fn a_held_store_lock_is_busy_and_moves_nothing() {
    let (_dir, ctx) = fixture();
    let legacy = plant_legacy(&ctx, &sha256_hex(b"wait"), b"wait");

    let locks = ctx.paths.locks_root();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let manager = LockManager::new(locks);
        let _guard = manager
            .acquire(LockResource::ContentStore, "another writer")
            .unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();

    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    assert!(matches!(outcome, MigrationOutcome::Busy), "{outcome:?}");
    assert!(legacy.exists());
    assert!(object_bytes(&ctx, &sha256_hex(b"wait")).is_none());
    assert!(migrate::legacy_store_active(&ctx.paths));
}

#[test]
fn an_unreadable_journal_needs_a_hand_and_the_fallback_stays() {
    let (_dir, ctx) = fixture();
    let instance = instance_with_file(&ctx, "live", b"unused");
    let legacy = plant_legacy(&ctx, &sha256_hex(b"keep reading"), b"keep reading");
    let journal = migrate::journal_path(&ctx.paths);
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(&journal, b"{ not a journal").unwrap();

    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();

    assert!(
        matches!(
            outcome,
            MigrationOutcome::NeedsHand(migrate::MigrationRefusal::UnusableJournal(_))
        ),
        "{outcome:?}"
    );
    assert!(
        legacy.exists(),
        "nothing moves under a journal nobody can read"
    );
    assert!(migrate::legacy_store_active(&ctx.paths));
    assert!(matches!(
        migrate::legacy_store_state(&ctx.paths),
        LegacyStoreState::Unknown(reason) if reason.contains("journal")
    ));
    assert_eq!(
        snapshot::snapshot_blob_source(&instance, &sha256_hex(b"keep reading"), 12).unwrap(),
        Some(legacy)
    );
    assert_eq!(
        fs::read(&journal).unwrap(),
        b"{ not a journal",
        "the damaged journal is evidence"
    );
}

#[test]
fn a_legacy_root_that_is_a_file_is_uncertain_even_after_done() {
    let (_dir, ctx) = fixture();
    let root = legacy_root(&ctx);
    fs::create_dir_all(&root).unwrap();
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    fs::remove_dir(&root).unwrap();
    fs::write(root, b"unexpected file").unwrap();

    assert!(migrate::legacy_store_active(&ctx.paths));
    assert!(matches!(
        migrate::legacy_store_state(&ctx.paths),
        LegacyStoreState::Unknown(reason) if reason.contains("snapshot-objects")
    ));
}

/// The way out for a damaged journal (MASTER_SPEC §32.11: every "needs a hand" has one). The move
/// is safe to start again from nothing: a legacy copy is only removed once a verified copy is in
/// the store. The damaged journal is set aside, not deleted.
#[test]
fn restarting_sets_a_damaged_journal_aside_and_finishes() {
    let (_dir, ctx) = fixture();
    let legacy = plant_legacy(&ctx, &sha256_hex(b"again"), b"again");
    let journal = migrate::journal_path(&ctx.paths);
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(&journal, b"{ not a journal").unwrap();

    let report = done(migrate::restart_snapshot_object_migration(&ctx).unwrap());

    assert_eq!(report.moved, 1, "{report:?}");
    assert!(!legacy.exists());
    let set_aside: Vec<_> = fs::read_dir(journal.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|entry| fs::read(entry.path()).ok().as_deref() == Some(b"{ not a journal"))
        .collect();
    assert_eq!(
        set_aside.len(),
        1,
        "the damaged journal is kept beside the new one"
    );
}

// ---------------------------------------------------------------------------
// 4. A crash halfway through
// ---------------------------------------------------------------------------

/// The seam stops the run right after the n-th object is in the content store and before its
/// legacy copy is removed: the one moment where both copies exist.
#[test]
fn a_run_stopped_halfway_resumes_and_finishes() {
    let (_dir, ctx) = fixture();
    let names: Vec<&[u8]> = vec![b"one", b"two", b"three", b"four", b"five"];
    let planted: Vec<PathBuf> = names
        .iter()
        .map(|bytes| plant_legacy(&ctx, &sha256_hex(bytes), bytes))
        .collect();

    ctx.test_seams
        .set_snapshot_migration_stop_after_place(Some(2));
    let stopped = migrate::migrate_snapshot_objects(&ctx);
    assert!(stopped.is_err(), "the seam stops the run: {stopped:?}");
    assert_eq!(ctx.test_seams.snapshot_migration_stops(), 1);
    assert!(
        migrate::legacy_store_active(&ctx.paths),
        "a stopped run is not done"
    );
    assert!(migrate::journal_path(&ctx.paths).exists());
    // Every object is still readable somewhere, which is the whole point of the order.
    let dir = instance_with_file(&ctx, "live", b"unused");
    for bytes in &names {
        assert!(
            snapshot::snapshot_blob_source(&dir, &sha256_hex(bytes), bytes.len() as u64)
                .unwrap()
                .is_some(),
            "{:?} is readable while the migration is unfinished",
            String::from_utf8_lossy(bytes)
        );
    }

    ctx.test_seams.set_snapshot_migration_stop_after_place(None);
    done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    for (path, bytes) in planted.iter().zip(&names) {
        assert!(!path.exists(), "{path:?}");
        assert_eq!(
            object_bytes(&ctx, &sha256_hex(bytes)).as_deref(),
            Some(*bytes)
        );
    }
    assert!(!migrate::legacy_store_active(&ctx.paths));
}

/// A stopped run's mismatches are still reported when a later run finishes it.
#[test]
fn mismatches_found_before_a_stop_are_still_reported_at_the_end() {
    let (_dir, ctx) = fixture();
    // The "5f" shard sorts before each good object's shard.
    let bad = plant_legacy(&ctx, &sha256_hex(b"0"), b"aaa-delivered");
    for bytes in [&b"x1"[..], b"x2", b"x3"] {
        plant_legacy(&ctx, &sha256_hex(bytes), bytes);
    }
    ctx.test_seams
        .set_snapshot_migration_stop_after_place(Some(1));
    assert!(migrate::migrate_snapshot_objects(&ctx).is_err());
    assert_eq!(ctx.test_seams.snapshot_migration_stops(), 1);
    // The resumed walk cannot rediscover the mismatch.
    fs::remove_file(&bad).unwrap();
    ctx.test_seams.set_snapshot_migration_stop_after_place(None);

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());
    assert_eq!(report.mismatched, vec![bad], "{report:?}");
}

// ---------------------------------------------------------------------------
// 5. What the migration must not touch
// ---------------------------------------------------------------------------

/// A folder link inside the old store points somewhere the migration was not asked about. It is
/// named, nothing behind it is read or removed, and the store is not retired while it is there.
#[test]
fn a_link_inside_the_legacy_store_is_named_and_left_alone() {
    let (dir, ctx) = fixture();
    let outside = dir.path().join("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let hash = sha256_hex(b"outside bytes");
    fs::write(outside.join(&hash), b"outside bytes").unwrap();
    fs::create_dir_all(legacy_root(&ctx)).unwrap();
    let link = legacy_root(&ctx).join(&hash[..2]);
    assert!(
        make_folder_link(&outside, &link),
        "this process cannot create a folder link, so the check must not be skipped silently"
    );
    let ordinary = plant_legacy(&ctx, &sha256_hex(b"zz-ordinary"), b"zz-ordinary");

    let outcome = migrate::migrate_snapshot_objects(&ctx).unwrap();

    match &outcome {
        MigrationOutcome::NeedsHand(migrate::MigrationRefusal::LegacyLink(path)) => {
            assert_eq!(path, &link, "the refusal names the link");
        }
        other => panic!("a link in the store must not let the migration finish: {other:?}"),
    }
    assert_eq!(fs::read(outside.join(&hash)).unwrap(), b"outside bytes");
    assert!(migrate::legacy_store_active(&ctx.paths));
    // The ordinary objects may move; what matters is that none is lost either way.
    let ordinary_hash = sha256_hex(b"zz-ordinary");
    assert!(ordinary.exists() || object_bytes(&ctx, &ordinary_hash).is_some());
}

/// Files that are not objects (an old receipt, a temp file, a stray name) are not objects to move,
/// and they are not the migration's to delete either — except a receipt of an object it moved,
/// which describes a file that is no longer there.
#[test]
fn files_that_are_not_objects_are_left_and_reported() {
    let (_dir, ctx) = fixture();
    let hash = sha256_hex(b"with receipt");
    let object = plant_legacy(&ctx, &hash, b"with receipt");
    crate::artifact_receipt::record_verified(&object, "sha256", &hash, Some(12)).unwrap();
    let stray = legacy_root(&ctx).join("ab").join("not-a-hash.tmp");
    fs::create_dir_all(stray.parent().unwrap()).unwrap();
    fs::write(&stray, b"half a write").unwrap();

    let report = done(migrate::migrate_snapshot_objects(&ctx).unwrap());

    assert_eq!(report.moved, 1, "{report:?}");
    assert!(stray.exists(), "a stray file is not deleted");
    assert!(
        report.left_alone.iter().any(|(path, _)| path == &stray),
        "and it is reported: {report:?}"
    );
    let receipt = object.with_file_name(format!("{hash}.agora-verified.json"));
    assert!(
        !receipt.exists(),
        "the moved object's own receipt goes with it"
    );
}

// ---------------------------------------------------------------------------
// 6. Where it runs
// ---------------------------------------------------------------------------

#[test]
fn the_startup_pass_runs_the_migration_and_says_so() {
    let (_dir, ctx) = fixture();
    let legacy = plant_legacy(&ctx, &sha256_hex(b"at startup"), b"at startup");

    let report = crate::startup_recovery::run_at_startup(&ctx);

    assert!(!legacy.exists());
    assert!(
        report
            .actions
            .iter()
            .any(|line| line.to_lowercase().contains("snapshot")),
        "the pass reports the move: {report:?}"
    );
}

#[test]
fn the_startup_pass_reports_a_busy_store_and_carries_on() {
    let (_dir, ctx) = fixture();
    let legacy = plant_legacy(&ctx, &sha256_hex(b"later"), b"later");
    let locks = ctx.paths.locks_root();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let manager = LockManager::new(locks);
        let _guard = manager
            .acquire(LockResource::ContentStore, "another writer")
            .unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();

    let started = std::time::Instant::now();
    let report = crate::startup_recovery::run_at_startup(&ctx);
    let waited = started.elapsed();
    release_tx.send(()).unwrap();
    holder.join().unwrap();

    assert!(legacy.exists());
    assert!(
        waited < Duration::from_secs(5),
        "the startup pass never waits on a lock: {waited:?}"
    );
    assert!(
        report
            .warnings
            .iter()
            .chain(report.actions.iter())
            .any(|line| line.to_lowercase().contains("snapshot")),
        "a busy store is reported, not silent: {report:?}"
    );
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
