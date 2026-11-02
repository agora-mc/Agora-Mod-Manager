//! Imports into a fresh instance through the shared content store (brief slice
//! `store-packs`). Kept apart from `import`'s own tests so pack-store behaviour reads on its
//! own.

use super::*;
use crate::content_bridge::{MetadataSource, ProjectDetails, ProjectVersion};
use std::io::Write;

fn store_ctx() -> (Ctx, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = crate::testing_context(tmp.path().join("data"));
    ctx.paths.create_required_dirs().unwrap();
    crate::content_bridge::init_local_state(&ctx);
    (ctx, tmp)
}

fn read_test_manifest(root: &Path, id: &str) -> InstanceManifest {
    // allow-raw-instance-manifest
    serde_json::from_slice(&fs::read(root.join(id).join("instance_manifest.json")).unwrap())
        .unwrap()
}

fn import_mrpack_with(
    ctx: Option<&Ctx>,
    path: &Path,
    root: &Path,
    name: Option<&str>,
    install_anyway: bool,
) -> LauncherResult<ImportResult> {
    import_mrpack_inner(
        path,
        root,
        false,
        None,
        None,
        None,
        OverridePolicy::Standard,
        name,
        false,
        install_anyway,
        ctx,
        None,
    )
}

fn both_hashes(bytes: &[u8]) -> String {
    format!(
        r#"{{"sha1":"{}","sha512":"{}"}}"#,
        sha1_hex(bytes),
        sha512_hex(bytes)
    )
}

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// A real-shaped `modrinth.index.json` pack whose files are embedded (no network).
fn write_hashed_mrpack(dir: &Path, files: &[(&str, &[u8], String)]) -> PathBuf {
    let path = dir.join("store-pack.mrpack");
    let listed: Vec<String> = files
        .iter()
        .map(|(rel, bytes, hashes)| {
            format!(
                r#"{{"path":"{rel}","hashes":{hashes},"env":{{"client":"required","server":"required"}},"downloads":[],"fileSize":{}}}"#,
                bytes.len()
            )
        })
        .collect();
    let index = format!(
        r#"{{"formatVersion":1,"game":"minecraft","versionId":"1.0.0","name":"Store Pack","summary":"","files":[{}],
            "dependencies":{{"minecraft":"1.20.4","fabric-loader":"0.15.0"}}}}"#,
        listed.join(",")
    );
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    writer.start_file("modrinth.index.json", opts).unwrap();
    writer.write_all(index.as_bytes()).unwrap();
    for (rel, bytes, _) in files {
        writer.start_file(*rel, opts).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    path
}

fn assert_mod_is_stored(
    ctx: &Ctx,
    instance_dir: &Path,
    filename: &str,
    linked: bool,
) -> InstalledMod {
    let id = instance_dir.file_name().unwrap().to_str().unwrap();
    let root = instance_dir.parent().unwrap();
    let manifest = read_test_manifest(root, id);
    let entry = manifest
        .mods
        .iter()
        .find(|entry| entry.filename == filename)
        .unwrap_or_else(|| panic!("no manifest entry for {filename}"))
        .clone();
    let item = entry
        .content_item_id
        .clone()
        .expect("the manifest records the store item");
    let object = entry
        .object_sha256
        .clone()
        .expect("the manifest records the store object");
    assert!(ctx.paths.content_object_path(&object).is_file());
    assert!(crate::content_bridge::load_item(ctx, &item).is_ok());
    assert_eq!(entry.sha256, object);
    // Link counts are a Unix concept; Windows checks placement through the store instead.
    #[cfg(not(unix))]
    let _ = linked;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let links = fs::metadata(instance_dir.join("mods").join(filename))
            .unwrap()
            .nlink();
        if linked {
            assert!(
                links >= 2,
                "the instance file is a link to the stored object"
            );
        } else {
            assert_eq!(links, 1, "sharing off places an ordinary copy");
        }
    }
    entry
}

#[test]
fn an_mrpack_import_links_its_mods_from_the_store() {
    let (ctx, tmp) = store_ctx();
    let jar = b"linked mod bytes".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/a.jar", &jar, both_hashes(&jar))]);
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap();
    assert_eq!(imported.imported_mods, 1);
    let instance_dir = root.join(&imported.instance_id);
    assert_eq!(fs::read(instance_dir.join("mods/a.jar")).unwrap(), jar);
    assert_mod_is_stored(&ctx, &instance_dir, "a.jar", true);
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn an_mrpack_import_copies_when_sharing_is_off() {
    let (ctx, tmp) = store_ctx();
    crate::content_bridge::set_sharing(&ctx, false).unwrap();
    let jar = b"copied mod bytes".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/a.jar", &jar, both_hashes(&jar))]);
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap();
    let instance_dir = root.join(&imported.instance_id);
    assert_mod_is_stored(&ctx, &instance_dir, "a.jar", false);
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn a_pack_config_stays_an_ordinary_file() {
    let (ctx, tmp) = store_ctx();
    let jar = b"mod with config".to_vec();
    let config = b"key = value".to_vec();
    let path = write_hashed_mrpack(
        tmp.path(),
        &[
            ("mods/a.jar", &jar, both_hashes(&jar)),
            ("config/settings.toml", &config, both_hashes(&config)),
        ],
    );
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap();
    let instance_dir = root.join(&imported.instance_id);
    let config_path = instance_dir.join("config/settings.toml");
    assert_eq!(fs::read(&config_path).unwrap(), config);
    // The config is the player's: it stays writable and is not a stored item.
    fs::write(&config_path, b"edited").unwrap();
    let manifest = read_test_manifest(&root, &imported.instance_id);
    assert!(!manifest
        .mods
        .iter()
        .any(|entry| entry.filename == "settings.toml"));
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn two_instances_from_the_same_pack_share_objects() {
    let (ctx, tmp) = store_ctx();
    let jar = b"shared mod bytes".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/a.jar", &jar, both_hashes(&jar))]);
    let root = ctx.paths.instances_root();
    let first = import_mrpack_with(Some(&ctx), &path, &root, Some("pack-one"), false).unwrap();
    let second = import_mrpack_with(Some(&ctx), &path, &root, Some("pack-two"), false).unwrap();
    let first_entry = assert_mod_is_stored(&ctx, &root.join(&first.instance_id), "a.jar", true);
    let second_entry = assert_mod_is_stored(&ctx, &root.join(&second.instance_id), "a.jar", true);
    assert_eq!(first_entry.content_item_id, second_entry.content_item_id);
    assert_eq!(first_entry.object_sha256, second_entry.object_sha256);
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn a_prism_import_links_its_mods_from_the_store() {
    let (ctx, tmp) = store_ctx();
    let zip_path = tmp.path().join("prism.zip");
    let file = fs::File::create(&zip_path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    writer.start_file("instance.cfg", opts).unwrap();
    writer.write_all(b"name=Prism Store\n").unwrap();
    writer.start_file("mmc-pack.json", opts).unwrap();
    writer
        .write_all(
            br#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.fabricmc.fabric-loader","version":"0.15.0"}]}"#,
        )
        .unwrap();
    writer.start_file("minecraft/mods/mod.jar", opts).unwrap();
    writer.write_all(b"prism mod").unwrap();
    writer.finish().unwrap();

    let root = ctx.paths.instances_root();
    let imported = import_prism_zip_named(&zip_path, &root, false, None, Some(&ctx)).unwrap();
    let instance_dir = root.join(&imported.instance_id);
    assert_eq!(
        fs::read(instance_dir.join("mods/mod.jar")).unwrap(),
        b"prism mod"
    );
    assert_mod_is_stored(&ctx, &instance_dir, "mod.jar", true);
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn a_technic_zip_import_links_its_mods_from_the_store() {
    let (ctx, _tmp) = store_ctx();
    let instance_dir = ctx.paths.instance_dir("technic-store").unwrap();
    fs::create_dir_all(instance_dir.join("mods")).unwrap();
    let bytes = b"technic mod".to_vec();
    let zipped = zip_bytes(&[("mods/tech.jar", &bytes)]);
    let mut archive = zip::ZipArchive::new(io::Cursor::new(zipped)).unwrap();
    let imported = extract_technic_zip_entries(
        &mut archive,
        &instance_dir.join("mods"),
        OverridePolicy::Standard,
        Some(&ctx),
        &instance_dir,
    )
    .unwrap();
    assert_eq!(imported, 1);
    assert_eq!(fs::read(instance_dir.join("mods/tech.jar")).unwrap(), bytes);
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

#[test]
fn an_unverifiable_pack_file_is_handled_as_before() {
    let (ctx, tmp) = store_ctx();
    let jar = b"no hashes at all".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/a.jar", &jar, "{}".to_string())]);
    let root = ctx.paths.instances_root();
    // The stop and the "install anyway" choice are unchanged.
    let stop = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap_err();
    assert_eq!(stop.code(), MRPACK_HASH_STOP_CODE);
    assert!(!root.exists() || fs::read_dir(&root).unwrap().next().is_none());
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, true).unwrap();
    let manifest = read_test_manifest(&root, &imported.instance_id);
    let entry = manifest
        .mods
        .iter()
        .find(|entry| entry.filename == "a.jar")
        .expect("an unverifiable file is installed anyway and recorded");
    assert_eq!(entry.source, "modrinth-pack");
    // It is still tracked content, so it goes through the store like any other mod.
    assert!(entry.content_item_id.is_some());
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
}

// ---- Review proofs (store-packs review) ----------------------------------------------------

/// An mrpack whose index lists `files` (embedded, hashed) and which also ships `overrides`.
fn write_mrpack_with_overrides(
    dir: &Path,
    files: &[(&str, &[u8])],
    overrides: &[(&str, &[u8])],
) -> PathBuf {
    let path = dir.join("override-pack.mrpack");
    let listed: Vec<String> = files
        .iter()
        .map(|(rel, bytes)| {
            format!(
                r#"{{"path":"{rel}","hashes":{},"env":{{"client":"required","server":"required"}},"downloads":[],"fileSize":{}}}"#,
                both_hashes(bytes),
                bytes.len()
            )
        })
        .collect();
    let index = format!(
        r#"{{"formatVersion":1,"game":"minecraft","versionId":"1.0.0","name":"Override Pack","summary":"","files":[{}],
            "dependencies":{{"minecraft":"1.20.4","fabric-loader":"0.15.0"}}}}"#,
        listed.join(",")
    );
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    writer.start_file("modrinth.index.json", opts).unwrap();
    writer.write_all(index.as_bytes()).unwrap();
    for (rel, bytes) in files {
        writer.start_file(*rel, opts).unwrap();
        writer.write_all(bytes).unwrap();
    }
    for (rel, bytes) in overrides {
        writer.start_file(format!("overrides/{rel}"), opts).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    path
}

#[test]
fn a_nested_pack_path_is_placed_where_the_pack_put_it() {
    let (ctx, tmp) = store_ctx();
    let jar = b"nested mod bytes".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/sub/x.jar", &jar, both_hashes(&jar))]);
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap();
    let instance_dir = root.join(&imported.instance_id);
    assert!(
        instance_dir.join("mods/sub/x.jar").is_file(),
        "the pack asked for mods/sub/x.jar (what an import without the store writes)"
    );
    assert!(
        !instance_dir.join("mods/x.jar").exists(),
        "the store placement must not invent mods/x.jar"
    );
}

#[test]
fn same_named_files_in_different_pack_folders_do_not_collide() {
    let (ctx, tmp) = store_ctx();
    let one = b"first x".to_vec();
    let two = b"second x".to_vec();
    let path = write_hashed_mrpack(
        tmp.path(),
        &[
            ("resourcepacks/a/x.zip", &one, both_hashes(&one)),
            ("resourcepacks/b/x.zip", &two, both_hashes(&two)),
        ],
    );
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false)
        .expect("different folders are different files");
    let instance_dir = root.join(&imported.instance_id);
    assert_eq!(
        fs::read(instance_dir.join("resourcepacks/a/x.zip")).unwrap(),
        one
    );
    assert_eq!(
        fs::read(instance_dir.join("resourcepacks/b/x.zip")).unwrap(),
        two
    );
}

#[test]
fn an_override_replaces_an_index_file_of_the_same_name() {
    let (ctx, tmp) = store_ctx();
    let indexed = b"index version".to_vec();
    let overridden = b"override version".to_vec();
    let path = write_mrpack_with_overrides(
        tmp.path(),
        &[("resourcepacks/pack.zip", &indexed)],
        &[("resourcepacks/pack.zip", &overridden)],
    );
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false)
        .expect("a pack override over an index file is a normal .mrpack");
    let instance_dir = root.join(&imported.instance_id);
    assert_eq!(
        fs::read(instance_dir.join("resourcepacks/pack.zip")).unwrap(),
        overridden,
        "overrides are applied after the index files and win"
    );
}

#[test]
fn a_hash_stopped_import_leaves_nothing_in_the_store() {
    let (ctx, tmp) = store_ctx();
    let good = b"good bytes".to_vec();
    let bad = b"bytes that do not match".to_vec();
    let wrong = both_hashes(b"something else entirely");
    let path = write_hashed_mrpack(
        tmp.path(),
        &[
            ("mods/good.jar", &good, both_hashes(&good)),
            ("mods/bad.jar", &bad, wrong),
        ],
    );
    let root = ctx.paths.instances_root();
    let stop = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap_err();
    assert_eq!(stop.code(), MRPACK_HASH_STOP_CODE);
    assert!(!root.exists() || fs::read_dir(&root).unwrap().next().is_none());
    // The bad file failed its published hash; it must never become store content.
    assert!(
        !ctx.paths.content_object_path(&sha256_hex(&bad)).exists(),
        "the mismatching download was stored as a trusted object before the import stopped"
    );
}

#[test]
fn a_pack_import_records_files_as_unchanged_and_items_as_used() {
    let (ctx, tmp) = store_ctx();
    let jar = b"pack mod bytes".to_vec();
    let path = write_hashed_mrpack(tmp.path(), &[("mods/a.jar", &jar, both_hashes(&jar))]);
    let root = ctx.paths.instances_root();
    let imported = import_mrpack_with(Some(&ctx), &path, &root, None, false).unwrap();
    let instance_dir = root.join(&imported.instance_id);
    let manifest = read_test_manifest(&root, &imported.instance_id);

    // Hand-edit detection must not flag a file the import just placed.
    let rows = crate::installed_content::list_installed_content(
        &instance_dir,
        &manifest,
        None,
        None,
        Some(&ctx.paths.content_objects_dir()),
    );
    let row = rows
        .iter()
        .find(|row| row.filename == "a.jar")
        .expect("the imported mod is listed");
    assert_eq!(
        row.file_state,
        crate::installed_content::FileState::Ok,
        "a file the import just placed is unchanged"
    );

    // The item the import created belongs to this instance, so it is not unused. The store holds
    // it and the instance scanned cleanly, so an empty listing means "in use", not "unreadable".
    assert_eq!(crate::content_bridge::store_item_count(&ctx), 1);
    let listing = crate::content_bridge::list_unused(&ctx).unwrap();
    assert!(listing.warnings.is_empty(), "{:?}", listing.warnings);
    assert!(
        listing.items.is_empty(),
        "a pack's own item is not unused: {listing:?}"
    );
}

/// A pack whose one listed file carries a Modrinth download URL while its bytes are embedded, so
/// the import identifies the project without any request.
fn write_downloaded_mrpack(dir: &Path, rel: &str, bytes: &[u8], url: &str) -> PathBuf {
    let path = dir.join("downloaded.mrpack");
    let hashes = both_hashes(bytes);
    let index = format!(
        r#"{{"formatVersion":1,"game":"minecraft","versionId":"1.0.0","name":"Downloaded Pack","summary":"","files":[{{"path":"{rel}","hashes":{hashes},"env":{{"client":"required","server":"required"}},"downloads":["{url}"],"fileSize":{}}}],
            "dependencies":{{"minecraft":"1.20.4","fabric-loader":"0.15.0"}}}}"#,
        bytes.len()
    );
    let mut writer = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    writer.start_file("modrinth.index.json", opts).unwrap();
    writer.write_all(index.as_bytes()).unwrap();
    writer.start_file(rel, opts).unwrap();
    writer.write_all(bytes).unwrap();
    writer.finish().unwrap();
    path
}

struct SilentMetadataSource {
    calls: std::sync::atomic::AtomicUsize,
}

impl MetadataSource for SilentMetadataSource {
    fn available(&self) -> Result<(), LauncherError> {
        Err(LauncherError::Generic {
            code: "ERR_TEST_NETWORK_OFF".into(),
            message: "network off".into(),
        })
    }

    fn projects(&self, _project_ids: &[String]) -> Result<Vec<ProjectDetails>, LauncherError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("a metadata request was made while the network was off");
    }

    fn versions_from_hashes(
        &self,
        _hashes: &[String],
    ) -> Result<Vec<ProjectVersion>, LauncherError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("a metadata request was made while the network was off");
    }

    fn icon_bytes(&self, _url: &str) -> Result<Option<Vec<u8>>, LauncherError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("a metadata request was made while the network was off");
    }
}

struct AnsweringMetadataSource {
    fail_projects: bool,
    project_calls: std::sync::atomic::AtomicUsize,
}

impl MetadataSource for AnsweringMetadataSource {
    fn projects(&self, project_ids: &[String]) -> Result<Vec<ProjectDetails>, LauncherError> {
        self.project_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_projects {
            return Err(LauncherError::Generic {
                code: "ERR_TEST_BATCH".into(),
                message: "the batch could not be fetched".into(),
            });
        }
        Ok(project_ids
            .iter()
            .map(|id| ProjectDetails {
                project_id: id.clone(),
                title: Some("Sodium".into()),
                description: Some("A modern rendering engine for Minecraft.".into()),
                page_url: Some("https://modrinth.com/mod/sodium".into()),
                icon_url: Some("https://cdn.modrinth.com/data/PROJ/icon.png".into()),
            })
            .collect())
    }

    fn versions_from_hashes(
        &self,
        hashes: &[String],
    ) -> Result<Vec<ProjectVersion>, LauncherError> {
        Ok(hashes
            .iter()
            .map(|hash| ProjectVersion {
                hash: hash.clone(),
                project_id: "PROJ".into(),
                version_number: Some("1.3.9".into()),
            })
            .collect())
    }

    fn icon_bytes(&self, _url: &str) -> Result<Option<Vec<u8>>, LauncherError> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[9u8; 16]);
        Ok(Some(bytes))
    }
}

fn imported_item(ctx: &Ctx, root: &Path, instance_id: &str) -> crate::content_bridge::ContentItem {
    let manifest = read_test_manifest(root, instance_id);
    let entry = manifest.mods.first().expect("the mod entry");
    crate::content_bridge::load_item(
        ctx,
        entry.content_item_id.as_deref().expect("the store item id"),
    )
    .unwrap()
}

#[test]
fn an_offline_pack_import_records_project_ids_and_page_urls() {
    let (ctx, tmp) = store_ctx();
    let bytes = b"offline mod bytes".to_vec();
    let path = write_downloaded_mrpack(
        tmp.path(),
        "mods/a.jar",
        &bytes,
        "https://cdn.modrinth.com/data/PROJ/versions/VER/a.jar",
    );
    let root = ctx.paths.instances_root();
    let source = SilentMetadataSource {
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let imported = import_mrpack_inner(
        &path,
        &root,
        false,
        None,
        None,
        None,
        OverridePolicy::Standard,
        None,
        false,
        false,
        Some(&ctx),
        Some(&source),
    )
    .unwrap();

    assert!(imported.warnings.is_empty());
    let item = imported_item(&ctx, &root, &imported.instance_id);
    assert_eq!(item.provider.as_deref(), Some("modrinth"));
    assert_eq!(item.provider_id.as_deref(), Some("PROJ"));
    assert!(
        item.version.is_none(),
        "an offline import leaves the human version absent; the CDN id is provenance, not a label"
    );
    assert!(
        item.provider_origin
            .as_deref()
            .is_some_and(|origin| origin.contains("VER")),
        "the version id stays in the provider fields for the lookup"
    );
    assert_eq!(
        item.page_url.as_deref(),
        Some("https://modrinth.com/mod/PROJ")
    );
    assert!(
        item.description.is_none(),
        "an offline import records no description and invents none"
    );
    assert!(item.icon_sha256.is_none());
    assert_eq!(
        source.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an offline import makes no metadata request"
    );
}

#[test]
fn a_pack_import_saves_descriptions_titles_and_icons_when_modrinth_answers() {
    let (ctx, tmp) = store_ctx();
    let bytes = b"online mod bytes".to_vec();
    let path = write_downloaded_mrpack(
        tmp.path(),
        "mods/bbe-fabric-1.3.9+mc26.2-mod.jar",
        &bytes,
        "https://cdn.modrinth.com/data/PROJ/versions/VER/bbe.jar",
    );
    let root = ctx.paths.instances_root();
    let source = AnsweringMetadataSource {
        fail_projects: false,
        project_calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let imported = import_mrpack_inner(
        &path,
        &root,
        false,
        None,
        None,
        None,
        OverridePolicy::Standard,
        None,
        false,
        false,
        Some(&ctx),
        Some(&source),
    )
    .unwrap();

    assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
    assert_eq!(
        source
            .project_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one batched request answers the whole pack"
    );
    let item = imported_item(&ctx, &root, &imported.instance_id);
    assert_eq!(
        item.description.as_deref(),
        Some("A modern rendering engine for Minecraft.")
    );
    assert_eq!(
        item.name, "Sodium",
        "the project title replaces the file name"
    );
    assert_eq!(
        item.version.as_deref(),
        Some("1.3.9"),
        "the hash lookup supplies the human version label"
    );
    assert_eq!(
        item.page_url.as_deref(),
        Some("https://modrinth.com/mod/sodium")
    );
    assert!(item.icon_sha256.is_some(), "the icon bytes were saved");
}

#[test]
fn a_failed_metadata_batch_warns_and_the_import_still_succeeds() {
    let (ctx, tmp) = store_ctx();
    let bytes = b"mod bytes with a failing batch".to_vec();
    let path = write_downloaded_mrpack(
        tmp.path(),
        "mods/failing.jar",
        &bytes,
        "https://cdn.modrinth.com/data/PROJ/versions/VER/failing.jar",
    );
    let root = ctx.paths.instances_root();
    let source = AnsweringMetadataSource {
        fail_projects: true,
        project_calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let imported = import_mrpack_inner(
        &path,
        &root,
        false,
        None,
        None,
        None,
        OverridePolicy::Standard,
        None,
        false,
        false,
        Some(&ctx),
        Some(&source),
    )
    .unwrap();

    assert_eq!(imported.imported_mods, 1, "the import itself succeeded");
    assert_eq!(imported.warnings.len(), 1, "{:?}", imported.warnings);
    assert!(
        imported.warnings[0].contains("the batch could not be fetched"),
        "{:?}",
        imported.warnings
    );
    let item = imported_item(&ctx, &root, &imported.instance_id);
    assert!(item.description.is_none());
    assert_eq!(item.provider_id.as_deref(), Some("PROJ"));
}
