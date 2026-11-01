//! Imports into a fresh instance through the shared content store (brief slice
//! `store-packs`). Kept apart from `import`'s own tests so pack-store behaviour reads on its
//! own.

use super::*;
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

fn assert_mod_is_linked(ctx: &Ctx, instance_dir: &Path, filename: &str) -> InstalledMod {
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(
            fs::metadata(instance_dir.join("mods").join(filename))
                .unwrap()
                .nlink()
                >= 2,
            "the instance file is a link to the stored object"
        );
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
    assert_mod_is_linked(&ctx, &instance_dir, "a.jar");
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
    assert_mod_is_linked(&ctx, &instance_dir, "a.jar");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(instance_dir.join("mods/a.jar"))
                .unwrap()
                .nlink(),
            1,
            "sharing off places an ordinary copy"
        );
    }
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
    let first_entry = assert_mod_is_linked(&ctx, &root.join(&first.instance_id), "a.jar");
    let second_entry = assert_mod_is_linked(&ctx, &root.join(&second.instance_id), "a.jar");
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
    assert_mod_is_linked(&ctx, &instance_dir, "mod.jar");
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
