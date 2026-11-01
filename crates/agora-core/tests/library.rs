//! Offline library tests: saved metadata, no network, catalog descriptions, damaged records,
//! instance joins, importer metadata and icon validation.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agora_core::content_store::{add_archive, add_file, get_item, set_item_metadata, ItemMetadata};
use agora_core::ctx::Ctx;
use agora_core::error::LauncherError;
use agora_core::game_instance::{self, GameInstanceManifest, SavesChoice};
use agora_core::library;
use agora_core::network_gate::{self, NetworkGate};
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

fn write_minecraft_instance(ctx: &Ctx, id: &str, name: &str, sha256: &str) {
    let dir = ctx.paths.instance_dir(id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 2,
        "instance_id": id,
        "name": name,
        "minecraft_version": "1.20.1",
        "loader": "fabric",
        "loader_version": "0.15.11",
        "mods": [{
            "filename": "example.jar",
            "registry_id": null,
            "modrinth_id": null,
            "source": "manual",
            "source_url": null,
            "version": "1.0.0",
            "sha256": sha256,
            "hash_verified": true,
            "installed_at": "2026-01-01T00:00:00Z"
        }],
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

fn create_registry_with_game_item(ctx: &Ctx, id: &str, description: &str) {
    let conn = rusqlite::Connection::open(ctx.paths.registry_db()).unwrap();
    conn.execute_batch(
        "CREATE TABLE game_catalog_items (
            id TEXT PRIMARY KEY,
            game TEXT NOT NULL,
            name TEXT NOT NULL,
            author TEXT,
            content_type TEXT NOT NULL,
            download_strategy TEXT NOT NULL,
            source_identifier TEXT NOT NULL,
            sha256 TEXT,
            download_sources_json TEXT NOT NULL DEFAULT '[]',
            game_compatibility_json TEXT NOT NULL DEFAULT '[]',
            description TEXT,
            license_id TEXT,
            page_url TEXT,
            icon_url TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            date_added TEXT
        );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO game_catalog_items
            (id, game, name, content_type, download_strategy, source_identifier,
             download_sources_json, game_compatibility_json, description, status)
         VALUES (?1, 'skyrim-se', 'Catalog Thing', 'mod', 'direct_hash', 'src',
                 '[]', '[]', ?2, 'active')",
        rusqlite::params![id, description],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 1. Items, metadata and icons
// ---------------------------------------------------------------------------

#[test]
fn the_library_lists_items_with_their_saved_metadata() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    let outcome = add_file(&ctx, &file, Some("Example"), None).unwrap();
    let item_id = outcome.item().item_id.clone();

    set_item_metadata(
        &ctx,
        &item_id,
        ItemMetadata {
            description: Some("A thing for testing.".into()),
            page_url: Some("https://example.invalid/thing".into()),
            version: Some("2.4.0".into()),
            game_id: Some("skyrim-se".into()),
            catalog_id: Some("catalog-thing".into()),
            icon_sha256: None,
        },
    )
    .unwrap();
    // A real image is saved as a protected object and recorded on the item.
    assert!(library::save_icon(&ctx, &item_id, b"\x89PNG\r\n\x1a\nicon bytes").unwrap());

    let listing = library::list(&ctx).unwrap();
    let listed = listing
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .expect("the item is listed");
    assert_eq!(listed.name, "Example");
    assert_eq!(listed.description.as_deref(), Some("A thing for testing."));
    assert_eq!(listed.version.as_deref(), Some("2.4.0"));
    assert_eq!(listed.game_id.as_deref(), Some("skyrim-se"));
    assert_eq!(
        listed.page_url.as_deref(),
        Some("https://example.invalid/thing")
    );
    assert!(listed.icon_sha256.is_some(), "the saved icon is recorded");
    assert_eq!(listed.source_kind, "file");
    assert!(listed.instances.is_empty());
    assert!(listing.damaged.is_empty());
}

#[test]
fn an_icon_that_is_not_an_image_is_not_saved() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    let item_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();

    assert!(!library::save_icon(&ctx, &item_id, b"<html>not an image</html>").unwrap());
    assert!(get_item(&ctx, &item_id).unwrap().icon_sha256.is_none());
}

// ---------------------------------------------------------------------------
// 2. No network
// ---------------------------------------------------------------------------

struct CountingGate {
    checks: Arc<AtomicUsize>,
}

impl NetworkGate for CountingGate {
    fn check(
        &self,
        _category: agora_core::http_client::ClientCategory,
    ) -> Result<(), LauncherError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Err(LauncherError::Generic {
            code: "ERR_TEST_NETWORK_ATTEMPT".into(),
            message: "the library must not reach the network".into(),
        })
    }
}

#[test]
fn the_library_makes_no_network_request() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    add_file(&ctx, &file, None, None).unwrap();

    let checks = Arc::new(AtomicUsize::new(0));
    network_gate::install(Arc::new(CountingGate {
        checks: checks.clone(),
    }));

    let listing = library::list(&ctx).unwrap();
    assert_eq!(listing.items.len(), 1);
    assert_eq!(
        checks.load(Ordering::SeqCst),
        0,
        "listing must not attempt a request"
    );
}

// ---------------------------------------------------------------------------
// 3. Catalog description fallback
// ---------------------------------------------------------------------------

#[test]
fn a_catalog_item_without_its_own_description_uses_the_cached_catalog() {
    let (_tmp, ctx) = test_ctx();
    create_registry_with_game_item(&ctx, "catalog-thing", "Words from the catalog.");
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    let item_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();

    set_item_metadata(
        &ctx,
        &item_id,
        ItemMetadata {
            description: None,
            page_url: None,
            version: None,
            game_id: Some("skyrim-se".into()),
            catalog_id: Some("catalog-thing".into()),
            icon_sha256: None,
        },
    )
    .unwrap();

    let listing = library::list(&ctx).unwrap();
    let listed = listing
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .unwrap();
    assert_eq!(
        listed.description.as_deref(),
        Some("Words from the catalog."),
        "the cached catalog supplies the description"
    );
}

// ---------------------------------------------------------------------------
// 4. Damage is reported
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_item_is_reported_not_skipped() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    let good_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();

    let damaged_id = "a".repeat(64);
    let damaged_path = ctx.paths.content_item_path(&damaged_id);
    std::fs::create_dir_all(damaged_path.parent().unwrap()).unwrap();
    std::fs::write(&damaged_path, b"{ this is not an item").unwrap();

    let listing = library::list(&ctx).unwrap();
    assert_eq!(listing.items.len(), 1, "the readable item is listed");
    assert_eq!(listing.items[0].item_id, good_id);
    assert_eq!(
        listing.damaged.len(),
        1,
        "the damaged record is not skipped"
    );
    assert_eq!(listing.damaged[0].item_id, damaged_id);
    assert!(
        listing.damaged[0].error.contains("not a valid item record"),
        "the reason is reported: {}",
        listing.damaged[0].error
    );
}

// ---------------------------------------------------------------------------
// 5. Which instances use an item, for both game kinds
// ---------------------------------------------------------------------------

#[test]
fn which_instances_use_an_item() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.jar");
    let bytes = b"pretend jar bytes";
    std::fs::write(&file, bytes).unwrap();
    let item_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let hash = sha256_hex(bytes);

    write_minecraft_instance(&ctx, "mc1", "Minecraft One", &hash);
    write_generic_instance(
        &ctx,
        "gen1",
        "Skyrim One",
        "skyrim-se",
        std::slice::from_ref(&item_id),
    );

    let listing = library::list(&ctx).unwrap();
    let listed = listing
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .unwrap();
    let names: Vec<&str> = listed
        .instances
        .iter()
        .map(|instance| instance.name.as_str())
        .collect();
    assert_eq!(names, vec!["Minecraft One", "Skyrim One"]);
    assert_eq!(
        listed.game_id.as_deref(),
        None,
        "two games, so no single one"
    );
}

// ---------------------------------------------------------------------------
// 6. Importers save what they know
// ---------------------------------------------------------------------------

#[test]
fn a_thunderstore_import_saves_its_summary() {
    let (_tmp, ctx) = test_ctx();
    write_generic_instance(&ctx, "gen1", "Skyrim One", "skyrim-se", &[]);

    let work = tempfile::tempdir().unwrap();
    let manifest = serde_json::json!({
        "name": "Thing",
        "version_number": "1.2.3",
        "dependencies": []
    });
    let zip_path = work.path().join("Thing.zip");
    create_zip(
        &zip_path,
        &[
            ("manifest.json", manifest.to_string().as_bytes()),
            ("plugins/Thing.dll", b"plugin bytes"),
        ],
    );
    let item_id = add_archive(&ctx, &zip_path, None)
        .unwrap()
        .item()
        .item_id
        .clone();

    let outcome =
        agora_core::content_thunderstore::install_thunderstore(&ctx, "gen1", &item_id).unwrap();
    let derived = get_item(&ctx, &outcome.derived_item.item_id).unwrap();
    assert_eq!(
        derived.description.as_deref(),
        Some(outcome.summary.as_str()),
        "the summary the importer built is saved on the item"
    );
    assert_eq!(derived.version.as_deref(), Some("1.2.3"));

    let manifest = game_instance::get_manifest(&ctx, "gen1").unwrap();
    assert!(
        manifest
            .layers
            .layers()
            .iter()
            .any(|layer| matches!(&layer.source, LayerSource::Content { content } if content == &outcome.derived_item.item_id)),
        "the derived item is added to the instance"
    );
}
