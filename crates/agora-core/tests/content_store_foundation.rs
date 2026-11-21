//! Foundation tests for single-file items, item metadata, placement and the sharing setting.
//!
//! These cover the shared content store's part of "Minecraft content in the store" (MASTER_SPEC
//! §26.12); no Minecraft install path uses them yet.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use agora_core::content_store::{
    add_file, compute_canonical_item_id, get_item, place_object_at, placement_mode,
    set_item_metadata, set_placement_mode, ContentError, ContentFile, ContentSource, ItemMetadata,
    PlacedAs, PlacementMode, SHARE_CONTENT_SETTING,
};
use agora_core::ctx::Ctx;
use agora_game_api::RelPath;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(tmp.path().to_path_buf());
    (tmp, ctx)
}

fn create_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn object_count(ctx: &Ctx) -> usize {
    let root = ctx.paths.content_objects_dir();
    if !root.exists() {
        return 0;
    }
    std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|shard| {
            std::fs::read_dir(shard.path())
                .map(|entries| entries.count())
                .unwrap_or(0)
        })
        .sum()
}

// ---------------------------------------------------------------------------
// 1. A jar is one file of its item, never its entries
// ---------------------------------------------------------------------------

#[test]
fn a_jar_is_one_file_of_its_item() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let jar = work.path().join("example.jar");
    create_zip(
        &jar,
        &[
            ("pack.mcmeta", b"{\"pack_format\": 15}"),
            ("assets/a.txt", b"a"),
        ],
    );
    let jar_bytes = std::fs::read(&jar).unwrap();

    let outcome = add_file(&ctx, &jar, None, None).unwrap();
    let item = outcome.item();

    assert_eq!(item.files.len(), 1, "a jar is one file, not its entries");
    assert_eq!(item.files[0].path.as_str(), "example.jar");
    assert_eq!(item.files[0].size, jar_bytes.len() as u64);
    assert_eq!(item.files[0].sha256, sha256_hex(&jar_bytes));
    assert_eq!(item.total_size, jar_bytes.len() as u64);
    assert_eq!(object_count(&ctx), 1);
    assert!(matches!(
        item.sources.as_slice(),
        [ContentSource::File { .. }]
    ));
}

// ---------------------------------------------------------------------------
// 2. Identical bytes under two names share one object
// ---------------------------------------------------------------------------

#[test]
fn the_same_bytes_under_two_names_share_one_object() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let first = work.path().join("first.bin");
    let second = work.path().join("second.bin");
    let same = b"the same bytes, under two names".repeat(64);
    std::fs::write(&first, &same).unwrap();
    std::fs::write(&second, &same).unwrap();

    let a = add_file(&ctx, &first, Some("First"), Some("first.bin")).unwrap();
    let b = add_file(&ctx, &second, Some("Second"), Some("second.bin")).unwrap();

    assert_ne!(
        a.item().item_id,
        b.item().item_id,
        "the item path is part of the item's identity"
    );
    assert_eq!(object_count(&ctx), 1);
    let hash = &a.item().files[0].sha256;
    assert_eq!(hash, &b.item().files[0].sha256);
    assert!(ctx.paths.content_object_path(hash).is_file());
}

// ---------------------------------------------------------------------------
// 3. Metadata is optional, round-trips, and old manifests still load
// ---------------------------------------------------------------------------

#[test]
fn an_item_written_before_metadata_still_loads() {
    let (_tmp, ctx) = test_ctx();
    let files = vec![ContentFile {
        path: RelPath::new("old.txt").unwrap(),
        size: 5,
        sha256: "ab".repeat(32),
    }];
    let item_id = compute_canonical_item_id(&files);
    let manifest = serde_json::json!({
        "item_id": item_id,
        "name": "Written before metadata",
        "files": files,
        "total_size": 5,
        "sources": [],
        "added_at_unix_ms": 1,
    });
    let items_dir = ctx.paths.content_items_dir();
    std::fs::create_dir_all(&items_dir).unwrap();
    std::fs::write(
        ctx.paths.content_item_path(&item_id),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let item = get_item(&ctx, &item_id).unwrap();
    assert_eq!(item.name, "Written before metadata");
    assert!(item.description.is_none());
    assert!(item.page_url.is_none());
    assert!(item.version.is_none());
    assert!(item.game_id.is_none());
    assert!(item.catalog_id.is_none());
    assert!(item.provider.is_none());
    assert!(item.provider_id.is_none());
    assert!(item.provider_origin.is_none());
    assert!(item.icon_sha256.is_none());

    let serialized = serde_json::to_value(&item).unwrap();
    for field in [
        "description",
        "page_url",
        "version",
        "game_id",
        "catalog_id",
        "provider",
        "provider_id",
        "provider_origin",
        "icon_sha256",
    ] {
        assert!(
            serialized.get(field).is_none(),
            "absent metadata must stay absent, but {field} was written"
        );
    }

    let metadata = ItemMetadata {
        description: Some("A test item".into()),
        page_url: Some("https://example.invalid/item".into()),
        version: Some("1.2.3".into()),
        game_id: Some("minecraft".into()),
        catalog_id: Some("example-mod".into()),
        provider: Some("catalog".into()),
        provider_id: Some("example-mod".into()),
        provider_origin: None,
        icon_sha256: Some("cd".repeat(32)),
    };
    set_item_metadata(&ctx, &item_id, metadata.clone()).unwrap();

    let reloaded = get_item(&ctx, &item_id).unwrap();
    assert_eq!(reloaded.description, metadata.description);
    assert_eq!(reloaded.page_url, metadata.page_url);
    assert_eq!(reloaded.version, metadata.version);
    assert_eq!(reloaded.game_id, metadata.game_id);
    assert_eq!(reloaded.catalog_id, metadata.catalog_id);
    assert_eq!(reloaded.icon_sha256, metadata.icon_sha256);
    // The item's identity did not change with its metadata.
    assert_eq!(reloaded.item_id, item_id);
}

// ---------------------------------------------------------------------------
// 4. Placing: link mode keeps the stored object readable and the link deletable
// ---------------------------------------------------------------------------

// `icacls` and its `/inheritance:` and `/grant:` flags:
// https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/icacls
#[cfg(windows)]
fn restrict_folder_to_modify(dir: &Path) {
    let user = std::env::var("USERNAME").expect("USERNAME");
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(OI)(CI)M"))
        .output()
        .expect("run icacls");
    assert!(
        out.status.success(),
        "icacls could not restrict {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(windows)]
fn restore_inherited_acl(dir: &Path) {
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/inheritance:e")
        .status();
    let _ = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/reset")
        .status();
}

#[cfg(windows)]
fn grant_full_control(dir: &Path) {
    let user = std::env::var("USERNAME").expect("USERNAME");
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .arg("/grant:r")
        .arg(format!("{user}:F"))
        .output()
        .expect("run icacls");
    assert!(
        out.status.success(),
        "icacls could not grant Full Control on {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[cfg(windows)]
fn a_placed_link_can_be_deleted_from_a_folder_without_full_control() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"stored object bytes".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    let folder = work.path().join("inherits-nothing");
    std::fs::create_dir_all(&folder).unwrap();
    restrict_folder_to_modify(&folder);
    let target = folder.join("mod.jar");

    let placed = place_object_at(&ctx, &hash, &target, PlacementMode::Link).unwrap();
    assert_eq!(placed, PlacedAs::Link);
    assert_eq!(std::fs::read(&target).unwrap(), bytes);

    std::fs::remove_file(&target).expect("a placed link must be deletable from its folder");
    assert!(!target.exists());
    restore_inherited_acl(&folder);
}

#[test]
#[cfg(windows)]
fn a_deleted_and_recreated_folder_gets_the_grant_again() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"stored object bytes".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    // A parent that grants only Modify, so a folder recreated under it inherits no
    // FILE_DELETE_CHILD, exactly like an instance folder on a drive with the default ACL.
    let parent = work.path().join("parent");
    std::fs::create_dir_all(&parent).unwrap();
    restrict_folder_to_modify(&parent);

    let folder = parent.join("mods");
    std::fs::create_dir_all(&folder).unwrap();
    let first = folder.join("mod.jar");
    assert_eq!(
        place_object_at(&ctx, &hash, &first, PlacementMode::Link).unwrap(),
        PlacedAs::Link
    );
    std::fs::remove_file(&first).unwrap();

    // Delete the folder and rebuild it at the same path, as a recreated instance would be.
    grant_full_control(&folder);
    std::fs::remove_dir(&folder).unwrap();
    std::fs::create_dir_all(&folder).unwrap();

    let second = folder.join("mod.jar");
    assert_eq!(
        place_object_at(&ctx, &hash, &second, PlacementMode::Link).unwrap(),
        PlacedAs::Link
    );
    std::fs::remove_file(&second).expect("a recreated folder must be granted again");

    restore_inherited_acl(&parent);
    restore_inherited_acl(&folder);
}

#[test]
fn a_placed_link_cannot_change_the_stored_object() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"an object that must not change".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    let folder = work.path().join("mods");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("mod.jar");

    let placed = place_object_at(&ctx, &hash, &target, PlacementMode::Link).unwrap();
    assert_eq!(placed, PlacedAs::Link);

    assert!(
        std::fs::write(&target, b"tampered").is_err(),
        "writing through a link to a protected object must be refused"
    );
    let stored = std::fs::read(ctx.paths.content_object_path(&hash)).unwrap();
    assert_eq!(sha256_hex(&stored), hash);
    assert_eq!(stored, bytes);
}

// ---------------------------------------------------------------------------
// 5. Placing: copy mode is an owned, writable copy
// ---------------------------------------------------------------------------

#[test]
fn copy_mode_places_a_writable_copy() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"the stored bytes".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    let folder = work.path().join("copies");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("mod.jar");

    let placed = place_object_at(&ctx, &hash, &target, PlacementMode::Copy).unwrap();
    assert_eq!(placed, PlacedAs::Copy);

    std::fs::write(&target, b"a local change").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"a local change");
    assert_eq!(
        std::fs::read(ctx.paths.content_object_path(&hash)).unwrap(),
        bytes,
        "writing the copy must not change the stored object"
    );
}

// ---------------------------------------------------------------------------
// 5b. A damaged (unprotected) object is never placed
// ---------------------------------------------------------------------------

#[test]
fn a_tampered_unprotected_object_is_not_placed() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"the stored bytes".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();
    let object = ctx.paths.content_object_path(&hash);

    agora_core::content_store::protect::unprotect(&object).unwrap();
    let mut tampered = std::fs::read(&object).unwrap();
    tampered[0] ^= 0xff;
    std::fs::write(&object, &tampered).unwrap();

    let folder = work.path().join("mods");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("mod.jar");
    for mode in [PlacementMode::Link, PlacementMode::Copy] {
        let result = place_object_at(&ctx, &hash, &target, mode);
        assert!(
            result.is_err(),
            "a tampered object must not be placed ({mode:?})"
        );
        assert!(
            !target.exists(),
            "a refused placement must leave nothing at the target"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Placing never overwrites
// ---------------------------------------------------------------------------

#[test]
fn placing_over_an_existing_file_is_an_error_and_keeps_the_file() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    std::fs::write(&source, b"stored").unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    let folder = work.path().join("existing");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("mod.jar");
    std::fs::write(&target, b"the user's file").unwrap();

    for mode in [PlacementMode::Link, PlacementMode::Copy] {
        let result = place_object_at(&ctx, &hash, &target, mode);
        assert!(
            matches!(result, Err(ContentError::TargetExists { .. })),
            "placing over an existing file must be an error, got {result:?}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"the user's file");
    }
}

// ---------------------------------------------------------------------------
// 7. A link that cannot be made becomes a copy, and says so
// ---------------------------------------------------------------------------

/// Force the failure with the NTFS link limit that `game_deploy` handles: 1,023 links to one file
/// is the most the file system allows, so a placement past it must be a copy. Windows only: other
/// file systems have much higher limits, so the limit is not reached cheaply there.
#[test]
#[cfg(windows)]
fn a_link_that_cannot_be_made_becomes_a_copy_and_says_so() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("mod.jar");
    let bytes = b"a heavily shared object".repeat(32);
    std::fs::write(&source, &bytes).unwrap();
    let item = add_file(&ctx, &source, None, None).unwrap();
    let hash = item.item().files[0].sha256.clone();

    let object = ctx.paths.content_object_path(&hash);
    let links = work.path().join("links");
    std::fs::create_dir_all(&links).unwrap();
    for i in 0..1023u32 {
        std::fs::hard_link(&object, links.join(format!("link-{i}"))).unwrap();
    }

    let folder = work.path().join("mods");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("mod.jar");
    let placed = place_object_at(&ctx, &hash, &target, PlacementMode::Link).unwrap();
    assert_eq!(
        placed,
        PlacedAs::Copy,
        "a link past the limit must be a copy"
    );
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
}

// ---------------------------------------------------------------------------
// 8. The sharing setting is typed, defaults on, and a malformed value is an error
// ---------------------------------------------------------------------------

#[test]
fn a_malformed_setting_is_an_error() {
    let (_tmp, ctx) = test_ctx();
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let conn = agora_core::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();

    assert_eq!(placement_mode(&ctx).unwrap(), PlacementMode::Link);
    agora_core::db::set_setting(&conn, SHARE_CONTENT_SETTING, &serde_json::json!(false)).unwrap();
    assert_eq!(placement_mode(&ctx).unwrap(), PlacementMode::Copy);
    set_placement_mode(&ctx, PlacementMode::Link).unwrap();
    assert_eq!(placement_mode(&ctx).unwrap(), PlacementMode::Link);

    agora_core::db::set_setting(
        &conn,
        SHARE_CONTENT_SETTING,
        &serde_json::json!("not a boolean"),
    )
    .unwrap();
    assert!(
        placement_mode(&ctx).is_err(),
        "a malformed value must be an error, not the default"
    );

    // Malformed JSON text is also malformed, not "on".
    conn.execute(
        "INSERT INTO user_settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json",
        rusqlite::params![SHARE_CONTENT_SETTING, "not json at all"],
    )
    .unwrap();
    assert!(placement_mode(&ctx).is_err());
}
