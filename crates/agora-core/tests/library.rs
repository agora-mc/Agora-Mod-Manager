//! Offline library tests: saved metadata, no network, catalog descriptions, damaged records,
//! instance joins, importer metadata and icon validation.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agora_core::content_store::{
    add_archive, add_file, get_item, set_item_metadata, ContentError, ItemMetadata,
};
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
            provider: None,
            provider_id: None,
            provider_origin: None,
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
fn the_listing_reply_carries_no_image_bytes() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("example.bin");
    std::fs::write(&file, b"payload").unwrap();
    let item_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    library::save_icon(&ctx, &item_id, b"\x89PNG\r\n\x1a\nicon bytes").unwrap();

    let listing = library::list(&ctx).unwrap();
    let text = serde_json::to_string(&listing).unwrap();
    assert!(
        !text.contains("data:image")
            && !text.contains("iconDataUrl")
            && !text.contains("icon_data_url"),
        "the listing carries only the icon hash, not its bytes: {text}"
    );
    let listed = &listing.items[0];
    let hash = listed.icon_sha256.as_deref().expect("the hash is listed");
    let icon = library::read_icon_by_hash(&ctx, hash)
        .unwrap()
        .expect("the icon reads by hash");
    assert_eq!(icon.bytes, b"\x89PNG\r\n\x1a\nicon bytes");
}

#[test]
fn a_bad_icon_hash_is_refused() {
    let (_tmp, ctx) = test_ctx();
    assert!(matches!(
        library::read_icon_by_hash(&ctx, "not-a-hash"),
        Err(ContentError::InvalidHash(_))
    ));
    assert!(matches!(
        library::read_icon_by_hash(&ctx, &"0".repeat(64)),
        Err(ContentError::MissingObject(_))
    ));
}

#[test]
fn a_read_limit_refuses_a_larger_object_before_reading_it() {
    let (_tmp, ctx) = test_ctx();
    let sha = agora_core::content_store::store_object_bytes(&ctx, b"0123456789").unwrap();
    let error = agora_core::content_store::read_object_bytes_limited(&ctx, &sha, 4).unwrap_err();
    assert!(matches!(error, ContentError::Other(_)), "{error}");
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
            provider: None,
            provider_id: None,
            provider_origin: None,
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

#[test]
fn an_object_sha256_is_preferred_for_the_minecraft_join() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();

    // Two items with different bytes. The manifest records the second file's download hash but
    // names the first item's object hash, so only the first item is the one installed.
    let first_file = work.path().join("first.jar");
    std::fs::write(&first_file, b"first bytes").unwrap();
    let first_id = add_file(&ctx, &first_file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let first_hash = sha256_hex(b"first bytes");

    let second_file = work.path().join("second.jar");
    std::fs::write(&second_file, b"second bytes").unwrap();
    let second_id = add_file(&ctx, &second_file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    let second_hash = sha256_hex(b"second bytes");

    let dir = ctx.paths.instance_dir("mc1").unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 2,
        "instance_id": "mc1",
        "name": "Minecraft One",
        "minecraft_version": "1.20.1",
        "loader": "fabric",
        "loader_version": "0.15.11",
        "mods": [{
            "filename": "second.jar",
            "registry_id": null,
            "modrinth_id": null,
            "source": "manual",
            "source_url": null,
            "version": "1.0.0",
            "sha256": second_hash,
            "object_sha256": first_hash,
            "hash_verified": true,
            "installed_at": "2026-01-01T00:00:00Z"
        }]
    });
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();

    let listing = library::list(&ctx).unwrap();
    let first = listing
        .items
        .iter()
        .find(|item| item.item_id == first_id)
        .unwrap();
    let second = listing
        .items
        .iter()
        .find(|item| item.item_id == second_id)
        .unwrap();
    assert_eq!(first.instances.len(), 1, "the object hash joins the item");
    assert!(second.instances.is_empty(), "the download hash does not");
}

// ---------------------------------------------------------------------------
// 6. Importers save what they know
// ---------------------------------------------------------------------------

/// Add a Thunderstore package with `manifest` and install it into `gen1`.
fn import_thunderstore(
    ctx: &Ctx,
    manifest: serde_json::Value,
) -> agora_core::content_thunderstore::ThunderstoreInstallOutcome {
    let work = tempfile::tempdir().unwrap();
    let zip_path = work.path().join("Thing.zip");
    create_zip(
        &zip_path,
        &[
            ("manifest.json", manifest.to_string().as_bytes()),
            ("plugins/Thing.dll", b"plugin bytes"),
        ],
    );
    let item_id = add_archive(ctx, &zip_path, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    agora_core::content_thunderstore::install_thunderstore(ctx, "gen1", &item_id).unwrap()
}

#[test]
fn a_thunderstore_import_saves_its_manifest_description() {
    let (_tmp, ctx) = test_ctx();
    write_generic_instance(&ctx, "gen1", "Skyrim One", "skyrim-se", &[]);

    // The author's short description, as Thunderstore's manifest defines it (250 chars max).
    let outcome = import_thunderstore(
        &ctx,
        serde_json::json!({
            "name": "Thing",
            "version_number": "1.2.3",
            "description": "A thing that does the thing.",
            "dependencies": []
        }),
    );
    let derived = get_item(&ctx, &outcome.derived_item.item_id).unwrap();
    assert_eq!(
        derived.description.as_deref(),
        Some("A thing that does the thing."),
        "the manifest's own description is saved, not Agora's mapping summary"
    );
    assert_ne!(
        derived.description.as_deref(),
        Some(outcome.summary.as_str())
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

#[test]
fn a_thunderstore_import_without_a_description_saves_none() {
    let (_tmp, ctx) = test_ctx();
    write_generic_instance(&ctx, "gen1", "Skyrim One", "skyrim-se", &[]);

    let outcome = import_thunderstore(
        &ctx,
        serde_json::json!({
            "name": "Thing",
            "version_number": "1.2.3",
            "dependencies": []
        }),
    );
    let derived = get_item(&ctx, &outcome.derived_item.item_id).unwrap();
    assert!(
        derived.description.is_none(),
        "no description is invented when the manifest has none"
    );
    assert_eq!(derived.version.as_deref(), Some("1.2.3"));
}

// ---------------------------------------------------------------------------
// Filling missing details from a provider
// ---------------------------------------------------------------------------

struct FakeMetadataSource {
    available: bool,
    fail_projects: bool,
    project_calls: std::sync::Mutex<Vec<Vec<String>>>,
    hash_calls: std::sync::Mutex<Vec<Vec<String>>>,
    icon_calls: std::sync::Mutex<Vec<String>>,
}

impl FakeMetadataSource {
    fn answering() -> Self {
        Self {
            available: true,
            fail_projects: false,
            project_calls: std::sync::Mutex::new(Vec::new()),
            hash_calls: std::sync::Mutex::new(Vec::new()),
            icon_calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

fn png() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[7u8; 16]);
    bytes
}

impl library::MetadataSource for FakeMetadataSource {
    fn available(&self) -> Result<(), LauncherError> {
        if self.available {
            Ok(())
        } else {
            Err(LauncherError::Generic {
                code: "ERR_TEST_NETWORK_OFF".into(),
                message: "network off".into(),
            })
        }
    }

    fn projects(
        &self,
        project_ids: &[String],
    ) -> Result<Vec<library::ProjectDetails>, LauncherError> {
        self.project_calls
            .lock()
            .unwrap()
            .push(project_ids.to_vec());
        if self.fail_projects {
            return Err(LauncherError::Generic {
                code: "ERR_TEST_BATCH".into(),
                message: "batch unavailable".into(),
            });
        }
        Ok(project_ids
            .iter()
            .map(|id| library::ProjectDetails {
                project_id: id.clone(),
                title: Some(format!("Title {id}")),
                description: Some(format!("Description {id}")),
                page_url: Some(format!("https://modrinth.com/mod/{id}")),
                icon_url: Some(format!("https://cdn.modrinth.com/{id}.png")),
            })
            .collect())
    }

    fn versions_from_hashes(
        &self,
        hashes: &[String],
    ) -> Result<Vec<library::ProjectVersion>, LauncherError> {
        self.hash_calls.lock().unwrap().push(hashes.to_vec());
        Ok(hashes
            .iter()
            .map(|hash| library::ProjectVersion {
                hash: hash.clone(),
                project_id: "project-by-hash".into(),
                version_number: Some("9.9.9".into()),
            })
            .collect())
    }

    fn icon_bytes(&self, url: &str) -> Result<Option<Vec<u8>>, LauncherError> {
        self.icon_calls.lock().unwrap().push(url.to_string());
        Ok(Some(png()))
    }
}

fn add_named_item(ctx: &Ctx, directory: &TempDir, name: &str) -> String {
    let file = directory.path().join(format!("{name}.jar"));
    std::fs::write(&file, format!("bytes of {name}")).unwrap();
    add_file(ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone()
}

fn set_provider(ctx: &Ctx, item_id: &str, provider_id: &str) {
    set_item_metadata(
        ctx,
        item_id,
        ItemMetadata {
            version: Some("1.0.0".into()),
            provider: Some("modrinth".into()),
            provider_id: Some(provider_id.into()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn filling_missing_details_fills_only_what_is_missing_and_batches_the_lookup() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let mut items = Vec::new();
    for index in 0..101 {
        let item_id = add_named_item(&ctx, &work, &format!("raw{index}"));
        set_provider(&ctx, &item_id, &format!("project{index}"));
        items.push(item_id);
    }
    // One item already has a description, page and a name a person chose.
    let kept = &items[0];
    set_item_metadata(
        &ctx,
        kept,
        ItemMetadata {
            description: Some("Keep this.".into()),
            page_url: Some("https://keep.invalid/page".into()),
            version: Some("1.0.0".into()),
            provider: Some("modrinth".into()),
            provider_id: Some("project0".into()),
            ..Default::default()
        },
    )
    .unwrap();
    agora_core::content_store::set_item_name(&ctx, kept, "Chosen Name").unwrap();

    let source = FakeMetadataSource::answering();
    let report = library::fill_missing_details(&ctx, &source).unwrap();

    assert_eq!(report.considered, 101);
    assert_eq!(report.updated, 101, "every item gained an icon and a title");
    let calls = source.project_calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "101 items are two batched requests");
    assert!(calls.iter().all(|ids| ids.len() <= library::METADATA_BATCH));
    assert_eq!(calls[0].len(), 100);
    assert_eq!(calls[1].len(), 1);
    assert!(
        source.hash_calls.lock().unwrap().is_empty(),
        "items with a provider id are not looked up by hash"
    );

    let first = get_item(&ctx, &items[1]).unwrap();
    assert_eq!(first.description.as_deref(), Some("Description project1"));
    assert_eq!(
        first.page_url.as_deref(),
        Some("https://modrinth.com/mod/project1")
    );
    assert_eq!(first.name, "Title project1");
    assert!(first.icon_sha256.is_some());

    let kept = get_item(&ctx, kept).unwrap();
    assert_eq!(
        kept.description.as_deref(),
        Some("Keep this."),
        "a saved description is never overwritten"
    );
    assert_eq!(kept.page_url.as_deref(), Some("https://keep.invalid/page"));
    assert_eq!(
        kept.name, "Chosen Name",
        "a name a person chose is never replaced"
    );
    assert!(kept.icon_sha256.is_some(), "the missing icon is filled");
}

#[test]
fn filling_missing_details_identifies_an_item_by_its_file_hash() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item_id = add_named_item(&ctx, &work, "hashed");
    let item = get_item(&ctx, &item_id).unwrap();
    let object = item.files.first().unwrap().sha256.clone();
    let bytes = agora_core::content_store::read_object_bytes(&ctx, &object).unwrap();
    let expected = agora_core::download::sha512_hex(&bytes);

    let source = FakeMetadataSource::answering();
    let report = library::fill_missing_details(&ctx, &source).unwrap();
    assert_eq!(report.updated, 1);

    let calls = source.hash_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], vec![expected.clone()]);
    drop(calls);
    assert_eq!(source.project_calls.lock().unwrap().len(), 1);

    let item = get_item(&ctx, &item_id).unwrap();
    assert_eq!(item.provider.as_deref(), Some("modrinth"));
    assert_eq!(item.provider_id.as_deref(), Some("project-by-hash"));
    assert_eq!(item.version.as_deref(), Some("9.9.9"));
    assert_eq!(
        item.description.as_deref(),
        Some("Description project-by-hash")
    );
}

#[test]
fn filling_missing_details_makes_no_request_when_the_source_is_off() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item_id = add_named_item(&ctx, &work, "offline");
    set_provider(&ctx, &item_id, "project-offline");

    let mut source = FakeMetadataSource::answering();
    source.available = false;
    let report = library::fill_missing_details(&ctx, &source).unwrap();

    assert_eq!(
        source.project_calls.lock().unwrap().len(),
        0,
        "no request is made when the source is unavailable"
    );
    assert_eq!(source.hash_calls.lock().unwrap().len(), 0);
    assert_eq!(source.icon_calls.lock().unwrap().len(), 0);
    assert_eq!(report.updated, 0);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);

    let item = get_item(&ctx, &item_id).unwrap();
    assert!(item.description.is_none());
}

#[test]
fn a_failed_batch_fetch_is_a_warning_not_a_failure() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    for index in 0..2 {
        let item_id = add_named_item(&ctx, &work, &format!("failing{index}"));
        set_provider(&ctx, &item_id, &format!("project{index}"));
    }

    let mut source = FakeMetadataSource::answering();
    source.fail_projects = true;
    let report = library::fill_missing_details(&ctx, &source).unwrap();

    assert_eq!(report.updated, 0);
    assert_eq!(report.warnings.len(), 1);
    assert!(
        report.warnings[0].contains("batch unavailable"),
        "{:?}",
        report.warnings
    );
}
