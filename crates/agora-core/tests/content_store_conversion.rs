//! Converting an existing instance between shared links and separate copies.
//!
//! The conversion is opt-in and rewrites files the user already has, so every test here is about
//! what it refuses to touch and what it does never lose.

use std::path::{Path, PathBuf};

use agora_core::content_store::convert::{
    convert_all, convert_instance, preview_all, preview_instance, ConvertSkipReason,
};
use agora_core::content_store::place::same_file;
use agora_core::content_store::{
    add_file, place_object_at, placement_mode, set_placement_mode, ContentError, PlacementMode,
    SHARE_CONTENT_SETTING,
};
use agora_core::ctx::Ctx;
use agora_core::models::{InstalledMod, InstanceManifest, CURRENT_MANIFEST_VERSION};
use agora_core::process_identity::ProcessIdentity;
use agora_core::process_session_manager::ProcessSession;
use sha2::{Digest, Sha256};
use std::time::SystemTime;
use tempfile::TempDir;

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(tmp.path().to_path_buf());
    (tmp, ctx)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn entry(filename: &str, sha256: &str, enabled: bool) -> InstalledMod {
    InstalledMod {
        provider: None,
        update_pinned: false,
        pack_managed: false,
        installed_as_dependency: false,
        filename: filename.to_string(),
        registry_id: None,
        modrinth_id: None,
        source: "manual_drag_drop".to_string(),
        source_url: None,
        version: None,
        sha256: sha256.to_string(),
        hash_verified: true,
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        java_packages: Vec::new(),
        mod_jar_id: None,
        provided_mod_ids: Vec::new(),
        enabled,
        content_type: "mod".to_string(),
        depends_on: Vec::new(),
        optional_deps: Vec::new(),
        incompatible_deps: Vec::new(),
        content_item_id: None,
        object_sha256: None,
    }
}

fn manifest_with(entries: Vec<InstalledMod>) -> InstanceManifest {
    InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: "inst".to_string(),
        name: "Test".to_string(),
        created_from_pack: None,
        minecraft_version: "1.21".to_string(),
        loader: "fabric".to_string(),
        loader_version: "1".to_string(),
        is_locked: false,
        mods: entries,
        resourcepacks: Vec::new(),
        shaders: Vec::new(),
        datapacks: Vec::new(),
        worlds: Vec::new(),
        user_preferences: serde_json::Value::Object(Default::default()),
    }
}

fn setup_instance(ctx: &Ctx, manifest: &InstanceManifest) -> PathBuf {
    let dir = ctx.paths.instance_dir("inst").unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    let path = ctx.paths.instance_manifest("inst").unwrap();
    agora_core::helpers::atomic_write_manifest(&path, manifest).unwrap();
    dir
}

fn write_mod(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join("mods").join(name), bytes).unwrap();
}

/// Put bytes into the store as `mods/<name>` and return the object hash.
fn store_bytes(ctx: &Ctx, name: &str, bytes: &[u8]) -> String {
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join(name);
    std::fs::write(&source, bytes).unwrap();
    let outcome = add_file(ctx, &source, Some(name), Some(&format!("mods/{name}"))).unwrap();
    outcome.item().files[0].sha256.clone()
}

// ---------------------------------------------------------------------------
// 1. To shared: matching files become links, changed files stay put and are listed
// ---------------------------------------------------------------------------

#[test]
fn converting_to_shared_links_matching_files_and_leaves_others() {
    let (_tmp, ctx) = test_ctx();
    let matching = b"the matching mod bytes".repeat(16);
    let user_bytes = b"the user edited this jar".repeat(8);
    let manifest = manifest_with(vec![
        entry("matching.jar", &sha256_hex(&matching), true),
        entry(
            "edited.jar",
            &sha256_hex(b"what the manifest recorded"),
            true,
        ),
    ]);
    let dir = setup_instance(&ctx, &manifest);
    write_mod(&dir, "matching.jar", &matching);
    write_mod(&dir, "edited.jar", &user_bytes);

    let summary = convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();
    assert_eq!(summary.files_converted, 1);
    assert_eq!(summary.files_already, 0);
    assert_eq!(summary.bytes, matching.len() as u64);
    assert_eq!(summary.skipped.len(), 1);
    assert_eq!(summary.skipped[0].path, "mods/edited.jar");
    assert_eq!(summary.skipped[0].reason, ConvertSkipReason::Changed);

    let hash = sha256_hex(&matching);
    let object = ctx.paths.content_object_path(&hash);
    let linked = dir.join("mods/matching.jar");
    assert!(object.is_file(), "the file was added to the store");
    assert!(
        same_file(&linked, &object),
        "the instance file is the store object, not a copy"
    );
    assert_eq!(std::fs::read(&linked).unwrap(), matching);

    let edited = dir.join("mods/edited.jar");
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        user_bytes,
        "a file that does not match is the user's and must not change"
    );
    assert!(!same_file(&edited, &object));

    // A second run finds nothing left to do and still leaves the user's file alone.
    let again = convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();
    assert_eq!(again.files_converted, 0);
    assert_eq!(again.files_already, 1);
    assert_eq!(again.skipped.len(), 1);
    assert_eq!(std::fs::read(&edited).unwrap(), user_bytes);
}

// ---------------------------------------------------------------------------
// 2. To separate: links become owned copies and the store object survives
// ---------------------------------------------------------------------------

#[test]
fn converting_to_separate_copies_links() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"shared shader bytes".repeat(24);
    let hash = store_bytes(&ctx, "linked.jar", &bytes);
    let manifest = manifest_with(vec![entry("linked.jar", &hash, true)]);
    let dir = setup_instance(&ctx, &manifest);
    place_object_at(
        &ctx,
        &hash,
        &dir.join("mods/linked.jar"),
        PlacementMode::Link,
    )
    .unwrap();
    let target = dir.join("mods/linked.jar");
    let object = ctx.paths.content_object_path(&hash);
    assert!(same_file(&target, &object));

    let summary = convert_instance(&ctx, "inst", PlacementMode::Copy).unwrap();
    assert_eq!(summary.files_converted, 1);
    assert_eq!(summary.bytes, bytes.len() as u64);
    assert!(summary.skipped.is_empty());
    assert!(
        !same_file(&target, &object),
        "the link became an owned copy"
    );
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    assert!(object.is_file(), "the store object is never deleted");

    // A second run sees an owned copy and leaves it alone.
    let again = convert_instance(&ctx, "inst", PlacementMode::Copy).unwrap();
    assert_eq!(again.files_converted, 0);
    assert_eq!(again.files_already, 1);

    // The copy is writable, which is the point of turning sharing off. A change made after the
    // conversion is the user's, so a later run leaves it alone too.
    std::fs::write(&target, b"the user may now change it").unwrap();
    let changed = convert_instance(&ctx, "inst", PlacementMode::Copy).unwrap();
    assert_eq!(changed.files_converted, 0);
    assert_eq!(changed.skipped.len(), 1);
    assert_eq!(changed.skipped[0].reason, ConvertSkipReason::Changed);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"the user may now change it"
    );
}

// ---------------------------------------------------------------------------
// 3. An interruption never loses a file, and a rerun finishes the job
// ---------------------------------------------------------------------------

#[test]
fn a_conversion_interrupted_mid_way_finishes_on_rerun_and_loses_nothing() {
    let (_tmp, ctx) = test_ctx();
    let kept = b"moved out but never replaced".repeat(12);
    let placed = b"replaced but the journal copy remained".repeat(12);
    let conflict_original = b"the original journal copy".repeat(10);
    let conflict_target = b"a different file now at the target".repeat(10);

    let kept_hash = store_bytes(&ctx, "kept.jar", &kept);
    let placed_hash = store_bytes(&ctx, "placed.jar", &placed);
    let manifest = manifest_with(vec![
        entry("kept.jar", &kept_hash, true),
        entry("placed.jar", &placed_hash, true),
        entry("conflict.jar", &sha256_hex(&conflict_original), true),
    ]);
    let dir = setup_instance(&ctx, &manifest);
    let journal = dir.join(".agora/content-convert/mods");
    std::fs::create_dir_all(&journal).unwrap();

    // Interrupted after moving the file out, before placing the object.
    std::fs::write(journal.join("kept.jar"), &kept).unwrap();
    // Interrupted after placing the object, before deleting the journal copy.
    place_object_at(
        &ctx,
        &placed_hash,
        &dir.join("mods/placed.jar"),
        PlacementMode::Link,
    )
    .unwrap();
    std::fs::write(journal.join("placed.jar"), &placed).unwrap();
    // A journal copy that does not match whatever is now at the target.
    std::fs::write(dir.join("mods/conflict.jar"), &conflict_target).unwrap();
    std::fs::write(journal.join("conflict.jar"), &conflict_original).unwrap();

    let summary = convert_instance(&ctx, "inst", PlacementMode::Link).unwrap();

    let kept_target = dir.join("mods/kept.jar");
    let object = ctx.paths.content_object_path(&kept_hash);
    assert_eq!(std::fs::read(&kept_target).unwrap(), kept);
    assert!(
        same_file(&kept_target, &object),
        "the rerun finished the link"
    );
    assert_eq!(
        summary.files_converted, 2,
        "kept and conflict were both recovered and linked"
    );

    let placed_target = dir.join("mods/placed.jar");
    assert!(same_file(&placed_target, &object_path(&ctx, &placed_hash)));
    assert!(
        !journal.join("placed.jar").exists(),
        "the leftover was cleaned up"
    );

    // The target did not match the recorded hash, so it was not the file: it is preserved aside
    // and the intact original is restored, then linked.
    assert_eq!(
        std::fs::read(dir.join("mods/conflict.jar")).unwrap(),
        conflict_original
    );
    let moved = summary
        .skipped
        .iter()
        .find(|skip| skip.reason == ConvertSkipReason::MovedAside)
        .expect("the rejected target is reported");
    let moved_to = moved
        .moved_to
        .as_deref()
        .expect("the file says where it went");
    assert_eq!(std::fs::read(moved_to).unwrap(), conflict_target);
    assert!(!journal.join("conflict.jar").exists());
}

fn object_path(ctx: &Ctx, hash: &str) -> PathBuf {
    ctx.paths.content_object_path(hash)
}

// ---------------------------------------------------------------------------
// 4. A running instance is refused
// ---------------------------------------------------------------------------

#[test]
fn converting_a_running_instance_is_refused() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"a mod the running game has open".repeat(8);
    let manifest = manifest_with(vec![entry("running.jar", &sha256_hex(&bytes), true)]);
    let dir = setup_instance(&ctx, &manifest);
    write_mod(&dir, "running.jar", &bytes);

    ctx.process_session_manager
        .register(ProcessSession {
            instance_id: "inst".to_string(),
            session_id: 1,
            pid: std::process::id(),
            process_identity: ProcessIdentity {
                pid: std::process::id(),
                start_time: 0,
                expected_exe: None,
                job: None,
            },
            snapshot_id: "snapshot".to_string(),
            start_time: SystemTime::now(),
            attached: true,
            user_cancelled: false,
        })
        .unwrap();

    let error = convert_instance(&ctx, "inst", PlacementMode::Link).unwrap_err();
    assert!(
        matches!(error, ContentError::InstanceBusy(ref id) if id == "inst"),
        "unexpected error: {error}"
    );
    let target = dir.join("mods/running.jar");
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    assert!(!same_file(
        &target,
        &ctx.paths.content_object_path(&sha256_hex(&bytes))
    ));
    assert!(!dir.join(".agora/content-convert").exists());
}

// ---------------------------------------------------------------------------
// 5. A malformed setting is an error, never a silent default
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_setting_shows_an_error_not_a_default() {
    let (_tmp, ctx) = test_ctx();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
    agora_core::db::set_setting(
        &conn,
        SHARE_CONTENT_SETTING,
        &serde_json::json!("sometimes"),
    )
    .unwrap();

    let error = placement_mode(&ctx).unwrap_err();
    assert!(
        matches!(error, ContentError::InvalidSetting(_)),
        "a malformed value must be an error, not the default: {error}"
    );

    set_placement_mode(&ctx, PlacementMode::Link).unwrap();
    assert_eq!(placement_mode(&ctx).unwrap(), PlacementMode::Link);
}

// ---------------------------------------------------------------------------
// 6. Preview and bulk conversion count every instance without changing anything
// ---------------------------------------------------------------------------

#[test]
fn preview_and_bulk_conversion_cover_every_instance() {
    let (_tmp, ctx) = test_ctx();
    let first = b"first instance mod".repeat(8);
    let second = b"second instance mod".repeat(8);
    for (id, bytes) in [("one", &first), ("two", &second)] {
        let mut manifest = manifest_with(vec![entry("mod.jar", &sha256_hex(bytes), true)]);
        manifest.instance_id = id.to_string();
        let dir = ctx.paths.instance_dir(id).unwrap();
        std::fs::create_dir_all(dir.join("mods")).unwrap();
        agora_core::helpers::atomic_write_manifest(
            &ctx.paths.instance_manifest(id).unwrap(),
            &manifest,
        )
        .unwrap();
        std::fs::write(dir.join("mods/mod.jar"), bytes).unwrap();
    }

    let preview = preview_all(&ctx, PlacementMode::Link).unwrap();
    assert_eq!(preview.instances, 2);
    assert_eq!(preview.summary.files_converted, 2);
    assert_eq!(preview.summary.bytes, (first.len() + second.len()) as u64);
    for id in ["one", "two"] {
        let dir = ctx.paths.instance_dir(id).unwrap();
        assert!(
            !same_file(
                &dir.join("mods/mod.jar"),
                &ctx.paths.content_object_path(&sha256_hex(if id == "one" {
                    &first
                } else {
                    &second
                }))
            ),
            "preview must not change anything"
        );
    }

    let report = convert_all(&ctx, PlacementMode::Link).unwrap();
    assert_eq!(report.instances, 2);
    assert_eq!(report.summary.files_converted, 2);
    assert!(report.failures.is_empty());
    for id in ["one", "two"] {
        let dir = ctx.paths.instance_dir(id).unwrap();
        let hash = sha256_hex(if id == "one" { &first } else { &second });
        assert!(same_file(
            &dir.join("mods/mod.jar"),
            &ctx.paths.content_object_path(&hash)
        ));
    }

    let single = preview_instance(&ctx, "one", PlacementMode::Copy).unwrap();
    assert_eq!(single.files_converted, 1);
    assert_eq!(single.bytes, first.len() as u64);
}
