//! Store GC: removal sees Minecraft instances, an unreadable manifest fails closed, and the
//! unused listing only names items no instance uses (both game families).

use agora_core::content_store::{
    add_file, get_item, remove_item, verify_item, ContentError, VerifyDepth,
};
use agora_core::ctx::Ctx;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::library;
use agora_game_api::{
    BaseReference, GameId, InstallId, Layer, LayerId, LayerSource, LayerStack, RelPath,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(tmp.path().to_path_buf());
    (tmp, ctx)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn add_bytes(ctx: &Ctx, name: &str, bytes: &[u8]) -> String {
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join(format!("{name}.jar"));
    std::fs::write(&path, bytes).unwrap();
    add_file(ctx, &path, Some(name), None)
        .unwrap()
        .item()
        .item_id
        .clone()
}

/// A Minecraft instance whose mods record the store fields the real installs write.
fn write_minecraft_instance(ctx: &Ctx, id: &str, name: &str, mods: serde_json::Value) {
    let dir = ctx.paths.instance_dir(id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 2,
        "instance_id": id,
        "name": name,
        "minecraft_version": "1.20.1",
        "loader": "fabric",
        "loader_version": "0.15.11",
        "mods": mods,
        "resourcepacks": [],
        "shaders": [],
        "datapacks": [],
        "worlds": []
    });
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_generic_instance(ctx: &Ctx, id: &str, name: &str, game: &str, item_ids: &[String]) {
    let layers = item_ids
        .iter()
        .enumerate()
        .map(|(index, item_id)| Layer {
            id: LayerId::new(format!("content:{index}")).unwrap(),
            enabled: true,
            mount_path: RelPath::default(),
            source_path: RelPath::default(),
            source: LayerSource::Content {
                content: item_id.clone(),
            },
            whiteouts: Vec::new(),
            own_copy: false,
        })
        .collect();
    let manifest = GameInstanceManifest {
        manifest_version: 3,
        game: GameId::new(game).unwrap(),
        instance_id: id.to_string(),
        name: name.to_string(),
        runtime_identity: None,
        base: BaseReference::Unpinned {
            install: InstallId::new("steam:1").unwrap(),
            reason: "test".to_string(),
        },
        frameworks: Vec::new(),
        layers: LayerStack::new(layers).unwrap(),
        deployment: None,
        saves: SavesChoice::Shared,
        imported_from: None,
    };
    let dir = ctx.paths.instance_dir(id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
}

fn object_path(ctx: &Ctx, hash: &str) -> std::path::PathBuf {
    ctx.paths.content_object_path(hash)
}

// ---------------------------------------------------------------------------
// 1. A Minecraft instance's use blocks removal
// ---------------------------------------------------------------------------

#[test]
fn an_item_a_minecraft_instance_uses_cannot_be_removed() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"a stored minecraft mod";
    let item_id = add_bytes(&ctx, "Stored Mod", bytes);
    let hash = sha256_hex(bytes);
    let obj = object_path(&ctx, &hash);
    assert!(obj.exists(), "the object is stored");

    write_minecraft_instance(
        &ctx,
        "mc1",
        "Survival World",
        serde_json::json!([{
            "filename": "stored.jar",
            "source": "manual",
            "version": "1.0.0",
            "sha256": hash,
            "content_item_id": item_id,
            "object_sha256": hash,
            "installed_at": "2026-01-01T00:00:00Z"
        }]),
    );

    let error = remove_item(&ctx, &item_id).unwrap_err();
    match error {
        ContentError::InUse { instances, .. } => {
            assert!(
                instances.contains("mc1"),
                "the refusal names the Minecraft instance: {instances}"
            );
        }
        other => panic!("expected ContentError::InUse, got {other:?}"),
    }

    assert!(
        ctx.paths.content_item_path(&item_id).exists(),
        "the item is kept"
    );
    assert!(obj.exists(), "its object is kept");
}

// ---------------------------------------------------------------------------
// 2. An unreadable Minecraft manifest fails closed
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_minecraft_manifest_blocks_removal() {
    let (_tmp, ctx) = test_ctx();
    let bytes = b"an item with a broken neighbour";
    let item_id = add_bytes(&ctx, "Kept", bytes);
    let obj = object_path(&ctx, &sha256_hex(bytes));

    let dir = ctx.paths.instance_dir("mc-broken").unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let manifest_path = dir.join("instance_manifest.json");
    std::fs::write(&manifest_path, b"{ this is not a manifest").unwrap();

    let error = remove_item(&ctx, &item_id).unwrap_err();
    match error {
        ContentError::UnreadableManifest { manifest, .. } => {
            assert!(
                manifest.contains("instance_manifest.json"),
                "the refusal names the file: {manifest}"
            );
            assert!(
                manifest.contains("mc-broken"),
                "the refusal names the instance folder: {manifest}"
            );
        }
        other => panic!("expected ContentError::UnreadableManifest, got {other:?}"),
    }

    assert!(
        ctx.paths.content_item_path(&item_id).exists(),
        "nothing is deleted when a manifest cannot be read"
    );
    assert!(obj.exists(), "no object is swept");
}

// ---------------------------------------------------------------------------
// 3. The unused listing only names items no instance uses
// ---------------------------------------------------------------------------

#[test]
fn unused_items_lists_only_items_no_instance_uses() {
    let (_tmp, ctx) = test_ctx();

    // A Minecraft instance uses one item (by the store id, without a matching hash).
    let mc_bytes = b"used by minecraft";
    let mc_item = add_bytes(&ctx, "Minecraft Used", mc_bytes);
    write_minecraft_instance(
        &ctx,
        "mc1",
        "Minecraft One",
        serde_json::json!([{
            "filename": "used.jar",
            "source": "manual",
            "version": "1.0.0",
            "sha256": "0".repeat(64),
            "content_item_id": mc_item,
            "object_sha256": "0".repeat(64),
            "installed_at": "2026-01-01T00:00:00Z"
        }]),
    );

    // A generic instance uses another item through its Content layer.
    let generic_bytes = b"used by skyrim";
    let generic_item = add_bytes(&ctx, "Skyrim Used", generic_bytes);
    write_generic_instance(
        &ctx,
        "gen1",
        "Skyrim One",
        "skyrim-se",
        std::slice::from_ref(&generic_item),
    );

    // Nothing uses this one.
    let unused_bytes = b"nobody uses this";
    let unused_item = add_bytes(&ctx, "Unused", unused_bytes);
    let unused_size = unused_bytes.len() as u64;

    let listing = library::list_unused(&ctx).unwrap();
    let ids: Vec<&str> = listing
        .items
        .iter()
        .map(|item| item.item_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![unused_item.as_str()],
        "only the unused item is listed: {listing:?}"
    );
    assert_eq!(listing.items[0].name, "Unused");
    assert_eq!(listing.items[0].total_size, unused_size);
    assert_eq!(listing.total_size, unused_size);
    assert!(listing.warnings.is_empty(), "{:?}", listing.warnings);
}

// ---------------------------------------------------------------------------
// 4. Removing an unused item keeps every object another item uses
// ---------------------------------------------------------------------------

#[test]
fn removing_an_unused_item_keeps_every_object_another_item_uses() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();

    // Both items contain the same bytes, so they share one object.
    let shared = b"shared bytes";
    let shared_hash = sha256_hex(shared);

    let unused_dir = work.path().join("unused");
    std::fs::create_dir_all(&unused_dir).unwrap();
    std::fs::write(unused_dir.join("shared.txt"), shared).unwrap();
    std::fs::write(
        unused_dir.join("only-unused.txt"),
        b"unique to the unused item",
    )
    .unwrap();
    let unused_id = agora_core::content_store::add_folder(&ctx, &unused_dir, Some("Unused"))
        .unwrap()
        .item()
        .item_id
        .clone();
    let unused_only_hash = sha256_hex(b"unique to the unused item");

    let used_dir = work.path().join("used");
    std::fs::create_dir_all(&used_dir).unwrap();
    std::fs::write(used_dir.join("shared.txt"), shared).unwrap();
    std::fs::write(used_dir.join("only-used.txt"), b"unique to the used item").unwrap();
    let used_id = agora_core::content_store::add_folder(&ctx, &used_dir, Some("Used"))
        .unwrap()
        .item()
        .item_id
        .clone();
    write_generic_instance(
        &ctx,
        "gen1",
        "Skyrim One",
        "skyrim-se",
        std::slice::from_ref(&used_id),
    );

    assert!(object_path(&ctx, &shared_hash).exists());
    assert!(object_path(&ctx, &unused_only_hash).exists());

    remove_item(&ctx, &unused_id).expect("an unused item can be removed");

    assert!(
        object_path(&ctx, &shared_hash).exists(),
        "the shared object is still used by the other item"
    );
    assert!(
        !object_path(&ctx, &unused_only_hash).exists(),
        "the unused item's own object is swept"
    );
    let verification = verify_item(&ctx, &used_id, VerifyDepth::Full).unwrap();
    assert!(verification.problems.is_empty(), "{verification:?}");
    assert!(get_item(&ctx, &used_id).is_ok());
}
