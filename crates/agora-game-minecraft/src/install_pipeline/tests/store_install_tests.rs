//! Installs that go through the shared content store: linking, copying, updating, removing,
//! and the offline add.
//!
//! These live under `tests` so they reuse its instance and plan helpers, and so every test runs
//! the whole execute path (snapshot, apply, rollback, health) rather than `apply_transaction`
//! alone.

use super::*;

/// A context whose data root owns the instance, so the store and the instance sit under the
/// same test root.
fn ctx_instance(tmp: &tempfile::TempDir, id: &str) -> (Ctx, std::path::PathBuf) {
    let ctx = crate::context::testing_context(tmp.path().join("data"));
    ctx.paths.create_required_dirs().unwrap();
    crate::content_bridge::init_local_state(&ctx);
    let dir = ctx.paths.instance_dir(id).unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    let manifest = InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: id.into(),
        name: "Test".into(),
        created_from_pack: None,
        minecraft_version: TEST_INSTANCE_MC_VERSION.into(),
        loader: "fabric".into(),
        loader_version: "0.16.0".into(),
        is_locked: false,
        mods: vec![],
        resourcepacks: vec![],
        shaders: vec![],
        datapacks: vec![],
        worlds: vec![],
        user_preferences: serde_json::json!({}),
    };
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    (ctx, dir)
}

fn read_instance_manifest(dir: &Path) -> InstanceManifest {
    // allow-raw-instance-manifest
    serde_json::from_slice(&std::fs::read(dir.join("instance_manifest.json")).unwrap()).unwrap()
}

fn run_store_plan(plan: &ResolvedInstallPlan, dir: &Path, ctx: &Ctx) -> InstallOutcome {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(InstallPipeline.execute_plan_with_scheduler(
            plan,
            dir,
            "registry-rev",
            &NoopReporter,
            &CancellationToken::new(),
            InstallExecutionResources {
                content_store: Some(ctx),
                ..Default::default()
            },
        ))
}

fn write_mod_jar(dir: &Path, id: &str, version: &str) -> std::path::PathBuf {
    let path = dir.join(format!("{id}-{version}.jar"));
    write_fabric_test_jar(
        &path,
        serde_json::json!({
            "schemaVersion": 1,
            "id": id,
            "version": version,
            "name": id,
        }),
    );
    path
}

fn store_objects(ctx: &Ctx) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&ctx.paths.content_objects_dir(), &mut out);
    out
}

fn assert_success(outcome: InstallOutcome) {
    assert!(
        matches!(
            outcome,
            InstallOutcome::Success { .. } | InstallOutcome::HealthRollback { .. }
        ),
        "install did not complete: {outcome:?}"
    );
}

fn success_warnings(outcome: &InstallOutcome) -> Vec<PlanWarning> {
    match outcome {
        InstallOutcome::Success { warnings, .. }
        | InstallOutcome::HealthRollback { warnings, .. } => warnings.clone(),
        _ => Vec::new(),
    }
}

#[test]
fn a_browse_install_links_the_jar_from_the_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);

    assert_success(run_store_plan(&plan, &dir, &ctx));

    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        std::fs::read(&source).unwrap()
    );
    let manifest = read_instance_manifest(&dir);
    let entry = manifest
        .mods
        .iter()
        .find(|m| m.filename == "sodium.jar")
        .unwrap();
    assert!(entry.content_item_id.is_some(), "the item id is recorded");
    let object = entry.object_sha256.clone().unwrap();
    assert!(ctx.paths.content_object_path(&object).is_file());
    assert_eq!(object, entry.sha256);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let links = std::fs::metadata(dir.join("mods/sodium.jar"))
            .unwrap()
            .nlink();
        assert!(
            links >= 2,
            "the instance file is a link to the stored object"
        );
    }
}

#[test]
fn a_browse_install_copies_when_sharing_is_off() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    crate::content_bridge::set_sharing(&ctx, false).unwrap();
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);

    assert_success(run_store_plan(&plan, &dir, &ctx));

    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        std::fs::read(&source).unwrap()
    );
    let manifest = read_instance_manifest(&dir);
    let entry = manifest
        .mods
        .iter()
        .find(|m| m.filename == "sodium.jar")
        .unwrap();
    assert!(entry.content_item_id.is_some());
    let object = entry.object_sha256.clone().unwrap();
    assert!(ctx.paths.content_object_path(&object).is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(dir.join("mods/sodium.jar"))
                .unwrap()
                .nlink(),
            1,
            "sharing off places an ordinary copy"
        );
    }
}

#[test]
fn two_instances_installing_the_same_mod_share_one_object() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, first) = ctx_instance(&tmp, "first");
    let second = ctx.paths.instance_dir("second").unwrap();
    std::fs::create_dir_all(second.join("mods")).unwrap();
    let mut manifest = read_instance_manifest(&first);
    manifest.instance_id = "second".into();
    std::fs::write(
        second.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    for dir in [&first, &second] {
        let plan = resolve_local_plan(dir, &source, "shared.jar", None);
        assert_success(run_store_plan(&plan, dir, &ctx));
    }

    let hashes: Vec<String> = [&first, &second]
        .iter()
        .map(|dir| {
            read_instance_manifest(dir).mods[0]
                .object_sha256
                .clone()
                .unwrap()
        })
        .collect();
    assert_eq!(hashes[0], hashes[1], "both placements use one object");
    assert_eq!(store_objects(&ctx).len(), 1, "the bytes are stored once");
}

#[test]
fn updating_a_linked_mod_replaces_it_and_rollback_restores_it() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let older = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));
    let before = read_instance_manifest(&dir).mods[0].clone();
    let old_bytes = std::fs::read(dir.join("mods/sodium.jar")).unwrap();

    let newer = write_mod_jar(tmp.path(), "manual-item", "2.0.0");
    let update = resolve_local_plan(&dir, &newer, "sodium.jar", None);
    assert_eq!(update.files_to_remove.len(), 1, "the old file is replaced");
    set_install_failpoint(Some("manifest-rename"));
    let outcome = run_store_plan(&update, &dir, &ctx);
    set_install_failpoint(None);
    assert!(
        matches!(
            outcome,
            InstallOutcome::Failed {
                rollback_performed: true,
                ..
            }
        ),
        "the injected failure must roll back: {outcome:?}"
    );
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        old_bytes,
        "rollback puts the old bytes back"
    );
    let rolled_back = read_instance_manifest(&dir).mods[0].clone();
    assert_eq!(rolled_back.sha256, before.sha256);
    assert_eq!(rolled_back.content_item_id, before.content_item_id);

    assert_success(run_store_plan(&update, &dir, &ctx));
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        std::fs::read(&newer).unwrap()
    );
    let updated = read_instance_manifest(&dir);
    assert_eq!(updated.mods.len(), 1, "the update leaves one entry");
    assert_ne!(updated.mods[0].sha256, before.sha256);
}

#[test]
fn a_failed_apply_rolls_a_linked_update_back_without_the_snapshot() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let older = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));
    let old_bytes = std::fs::read(dir.join("mods/sodium.jar")).unwrap();

    let newer = write_mod_jar(tmp.path(), "manual-item", "2.0.0");
    let update = resolve_local_plan(&dir, &newer, "sodium.jar", None);
    let staging = dir.join(".agora").join("staging").join("direct-test");
    std::fs::create_dir_all(staging.join("artifacts")).unwrap();
    for add in &update.files_to_add {
        std::fs::copy(
            &newer,
            staging.join("artifacts").join(&add.staging_filename),
        )
        .unwrap();
    }
    prepare_manifest(&update, &dir, &staging, Some(&ctx)).unwrap();
    set_install_failpoint(Some("manifest-rename"));
    let result = apply_transaction(&update, &dir, &staging, Some(&ctx));
    set_install_failpoint(None);

    assert!(result.is_err(), "the injected failure must surface");
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        old_bytes,
        "the fast rollback puts the linked file back"
    );
}

#[test]
fn removing_a_linked_mod_leaves_the_store_object() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));
    let object = read_instance_manifest(&dir).mods[0]
        .object_sha256
        .clone()
        .unwrap();

    let service = crate::install_service::InstallService::new(ctx.clone());
    assert!(service.remove_artifact("test", "sodium.jar").unwrap());

    assert!(!dir.join("mods/sodium.jar").exists());
    assert!(read_instance_manifest(&dir).mods.is_empty());
    assert!(
        ctx.paths.content_object_path(&object).is_file(),
        "removing the instance file does not remove the stored object"
    );
}

#[test]
fn a_user_replaced_file_is_never_deleted() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));

    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();
    std::fs::write(dir.join("mods/sodium.jar"), b"the user's new bytes").unwrap();
    let service = crate::install_service::InstallService::new(ctx.clone());
    let error = service.remove_artifact("test", "sodium.jar").unwrap_err();
    assert_eq!(error.code(), "ERR_FILE_CHANGED");
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        b"the user's new bytes"
    );
}

#[test]
fn a_user_replaced_file_is_moved_aside_by_an_update() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let older = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));

    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();
    std::fs::write(dir.join("mods/sodium.jar"), b"the user's new bytes").unwrap();
    let newer = write_mod_jar(tmp.path(), "manual-item", "2.0.0");
    let update = resolve_local_plan(&dir, &newer, "sodium.jar", None);
    let outcome = run_store_plan(&update, &dir, &ctx);
    assert_success(outcome.clone());
    let moved = success_warnings(&outcome)
        .into_iter()
        .filter(|w| w.code == "WARN_USER_FILE_MOVED_ASIDE")
        .count();
    assert_eq!(moved, 1, "the user is told where the file went");
    let aside = dir
        .join(".agora")
        .join("moved-aside")
        .join("mods")
        .join("sodium.jar");
    assert_eq!(std::fs::read(&aside).unwrap(), b"the user's new bytes");
    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        std::fs::read(&newer).unwrap()
    );
}

#[test]
fn disabling_and_enabling_a_linked_mod_works() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));

    crate::content_bridge::set_artifact_enabled(&ctx, "test", "sodium.jar", false).unwrap();
    assert!(dir.join("mods/sodium.jar.disabled").is_file());
    assert!(!dir.join("mods/sodium.jar").exists());
    assert!(!read_instance_manifest(&dir).mods[0].enabled);

    crate::content_bridge::set_artifact_enabled(&ctx, "test", "sodium.jar", true).unwrap();
    assert!(dir.join("mods/sodium.jar").is_file());
    assert!(!dir.join("mods/sodium.jar.disabled").exists());
    assert!(read_instance_manifest(&dir).mods[0].enabled);
}

#[test]
fn a_mod_deleted_by_hand_can_be_installed_again() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));
    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();

    let again = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&again, &dir, &ctx));
    assert!(dir.join("mods/sodium.jar").is_file());
    assert_eq!(read_instance_manifest(&dir).mods.len(), 1);
}

#[test]
fn a_direct_artifact_install_links_from_the_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let bytes = std::fs::read(&source).unwrap();
    let sha = super::sha256_hex(&bytes);
    let service = crate::install_service::InstallService::new(ctx.clone());

    let installed = service
        .commit_verified_artifact(
            "test",
            "manual.jar",
            "mod",
            "https://example.invalid/manual.jar",
            None,
            None,
            "registry",
            Some("1.0.0"),
            None,
            Some(&sha),
            None,
            None,
            None,
            false,
            bytes,
        )
        .unwrap();

    assert!(installed.content_item_id.is_some());
    let object = installed.object_sha256.clone().unwrap();
    assert!(ctx.paths.content_object_path(&object).is_file());
    assert_eq!(
        std::fs::read(dir.join("mods/manual.jar")).unwrap(),
        std::fs::read(&source).unwrap()
    );
    assert_eq!(
        read_instance_manifest(&dir).mods[0]
            .content_item_id
            .as_deref(),
        installed.content_item_id.as_deref()
    );
}

#[test]
fn a_manual_drop_links_from_the_store() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-drop", "1.0.0");

    let service = crate::install_service::InstallService::new(ctx.clone());
    let installed = service
        .add_manual_artifact("test", source.to_str().unwrap())
        .unwrap();

    assert_eq!(installed.source, "manual_drag_drop");
    assert!(installed.content_item_id.is_some());
    let object = installed.object_sha256.clone().unwrap();
    assert!(ctx.paths.content_object_path(&object).is_file());
    assert_eq!(
        std::fs::read(dir.join("mods").join(&installed.filename)).unwrap(),
        std::fs::read(&source).unwrap()
    );
}

#[test]
fn a_hard_link_clone_of_a_linked_mod_stays_removable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));

    let prefs = crate::clone::ClonePrefs {
        copy_saves: false,
        copy_mods: true,
        copy_resource_packs: false,
        copy_shader_packs: false,
        copy_screenshots: false,
        copy_config: false,
        copy_servers: false,
        copy_options: false,
        use_hard_links: true,
        use_sym_links: false,
    };
    let clone_dir = tmp.path().join("clone");
    crate::clone::clone_instance(&dir, &clone_dir, &prefs).unwrap();

    let cloned = clone_dir.join("mods").join("sodium.jar");
    assert_eq!(
        std::fs::read(&cloned).unwrap(),
        std::fs::read(&source).unwrap()
    );
    std::fs::remove_file(&cloned).expect("a cloned link can be deleted");
}

#[test]
fn catalog_and_modrinth_provenance_survive_an_offline_add() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, first) = ctx_instance(&tmp, "first");
    let second = ctx.paths.instance_dir("second").unwrap();
    std::fs::create_dir_all(second.join("mods")).unwrap();
    let mut manifest = read_instance_manifest(&first);
    manifest.instance_id = "second".into();
    std::fs::write(
        second.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let service = crate::install_service::InstallService::new(ctx.clone());

    let catalog_jar = write_mod_jar(tmp.path(), "catalog-mod", "1.0.0");
    let catalog_bytes = std::fs::read(&catalog_jar).unwrap();
    let catalog_sha = super::sha256_hex(&catalog_bytes);
    let catalog = service
        .commit_verified_artifact(
            "first",
            "catalog.jar",
            "mod",
            "https://example.invalid/catalog.jar",
            Some("catalog-mod"),
            None,
            "registry",
            Some("1.0.0"),
            None,
            Some(&catalog_sha),
            None,
            None,
            None,
            false,
            catalog_bytes,
        )
        .unwrap();

    let raw_jar = write_mod_jar(tmp.path(), "modrinth-raw", "1.0.0");
    let raw_bytes = std::fs::read(&raw_jar).unwrap();
    let raw_sha = super::sha256_hex(&raw_bytes);
    let raw = service
        .commit_verified_artifact(
            "first",
            "raw.jar",
            "mod",
            "https://cdn.modrinth.com/data/modrinth-raw/versions/1/raw.jar",
            None,
            Some("modrinth-raw"),
            "modrinth_raw",
            Some("1.0.0"),
            None,
            Some(&raw_sha),
            None,
            None,
            None,
            false,
            raw_bytes,
        )
        .unwrap();

    let catalog_again = service
        .install_stored_item("second", catalog.content_item_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(catalog_again.registry_id.as_deref(), Some("catalog-mod"));
    assert!(catalog_again.modrinth_id.is_none());
    assert_eq!(catalog_again.source, "registry");

    let raw_again = service
        .install_stored_item("second", raw.content_item_id.as_deref().unwrap())
        .unwrap();
    assert!(raw_again.registry_id.is_none());
    assert_eq!(raw_again.modrinth_id.as_deref(), Some("modrinth-raw"));
    assert_eq!(raw_again.source, "modrinth_raw");
}

#[cfg(windows)]
fn restrict_folder_to_modify(dir: &Path) {
    // `icacls /inheritance:r /grant:r <user>:(OI)(CI)M` keeps the default drive ACL (Modify),
    // which does not include FILE_DELETE_CHILD. See the Windows icacls reference:
    // https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/icacls
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

#[test]
#[cfg(windows)]
fn an_update_can_clean_the_trash_without_delete_child() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("restricted-data");
    std::fs::create_dir_all(&root).unwrap();
    restrict_folder_to_modify(&root);
    let ctx = crate::context::testing_context(root.clone());
    crate::content_bridge::init_local_state(&ctx);
    ctx.paths.create_required_dirs().unwrap();
    let dir = ctx.paths.instance_dir("test").unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    let manifest = InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: "test".into(),
        name: "Test".into(),
        created_from_pack: None,
        minecraft_version: TEST_INSTANCE_MC_VERSION.into(),
        loader: "fabric".into(),
        loader_version: "0.16.0".into(),
        is_locked: false,
        mods: vec![],
        resourcepacks: vec![],
        shaders: vec![],
        datapacks: vec![],
        worlds: vec![],
        user_preferences: serde_json::json!({}),
    };
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let older = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));

    let newer = write_mod_jar(tmp.path(), "manual-item", "2.0.0");
    let update = resolve_local_plan(&dir, &newer, "sodium.jar", None);
    assert_success(run_store_plan(&update, &dir, &ctx));

    assert_eq!(
        std::fs::read(dir.join("mods/sodium.jar")).unwrap(),
        std::fs::read(&newer).unwrap()
    );
    let staging_left: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join(".agora/staging"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert!(
        staging_left.is_empty(),
        "the trash holding the old link was cleaned; left: {staging_left:?}"
    );
    restore_inherited_acl(&root);
}

#[test]
fn adding_a_stored_item_offline_needs_no_network() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (mut ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "offline", "1.0.0");
    let stored = crate::content_bridge::store_file(
        &ctx,
        &source,
        &crate::content_bridge::StoreDetails {
            content_type: "mod",
            filename: "offline.jar",
            version: Some("1.0.0"),
            page_url: None,
            description: None,
            catalog_id: None,
            provider: None,
            provider_id: None,
            provider_origin: None,
        },
    )
    .unwrap();
    crate::content_bridge::forbid_network(&mut ctx);

    let service = crate::install_service::InstallService::new(ctx.clone());
    let installed = service
        .install_stored_item("test", &stored.item_id)
        .unwrap();

    assert_eq!(installed.filename, "offline.jar");
    assert_eq!(
        installed.content_item_id.as_deref(),
        Some(stored.item_id.as_str())
    );
    assert_eq!(installed.source, "content_store");
    assert!(dir.join("mods/offline.jar").is_file());
    // Adding it again is a no-op that still needs no network.
    let again = service
        .install_stored_item("test", &stored.item_id)
        .unwrap();
    assert_eq!(again.filename, "offline.jar");
    assert_eq!(read_instance_manifest(&dir).mods.len(), 1);
}

// ---------------------------------------------------------------------------
// Review tests (branch review/store-b), no longer ignored, plus the two follow-up cases.
// ---------------------------------------------------------------------------

fn review_store_item(ctx: &Ctx, source: &Path, rel: &str, game_id: Option<&str>) -> String {
    crate::content_bridge::store_review_item(ctx, source, rel, game_id)
}

#[cfg(windows)]
fn review_set_readonly(path: &Path, readonly: bool) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(readonly);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn review_offline_add_refuses_an_item_of_another_game() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "other", "1.0.0");
    let item_id = review_store_item(&ctx, &source, "mods/other.jar", Some("skyrim"));

    let service = crate::install_service::InstallService::new(ctx.clone());
    let result = service.install_stored_item("test", &item_id);

    assert!(
        result.is_err(),
        "an item stored for another game must be refused, not added"
    );
    assert!(!dir.join("mods/other.jar").exists());
    assert!(read_instance_manifest(&dir).mods.is_empty());
}

#[test]
fn review_offline_add_restores_a_file_deleted_by_hand() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "offline", "1.0.0");
    let item_id = review_store_item(&ctx, &source, "mods/offline.jar", Some("minecraft"));
    let service = crate::install_service::InstallService::new(ctx.clone());
    service.install_stored_item("test", &item_id).unwrap();
    std::fs::remove_file(dir.join("mods/offline.jar")).unwrap();

    service.install_stored_item("test", &item_id).unwrap();

    assert!(
        dir.join("mods/offline.jar").is_file(),
        "the add reported success but the mod is not in the instance"
    );
}

#[cfg(windows)]
#[test]
fn review_offline_add_failing_manifest_write_leaves_no_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "offline", "1.0.0");
    let item_id = review_store_item(&ctx, &source, "mods/offline.jar", Some("minecraft"));
    let manifest_path = dir.join("instance_manifest.json");
    review_set_readonly(&manifest_path, true);

    let service = crate::install_service::InstallService::new(ctx.clone());
    let result = service.install_stored_item("test", &item_id);
    review_set_readonly(&manifest_path, false);

    assert!(result.is_err(), "the manifest write was made to fail");
    assert!(
        !dir.join("mods/offline.jar").exists(),
        "a failed add left a placed file with no manifest entry"
    );
}

#[cfg(windows)]
#[test]
fn review_failed_manual_reinstall_keeps_the_old_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let v1 = tmp.path().join("v1");
    let v2 = tmp.path().join("v2");
    std::fs::create_dir_all(&v1).unwrap();
    std::fs::create_dir_all(&v2).unwrap();
    std::fs::copy(
        write_mod_jar(tmp.path(), "same", "1.0.0"),
        v1.join("same.jar"),
    )
    .unwrap();
    std::fs::copy(
        write_mod_jar(tmp.path(), "same", "2.0.0"),
        v2.join("same.jar"),
    )
    .unwrap();
    let service = crate::install_service::InstallService::new(ctx.clone());
    service
        .add_manual_artifact("test", v1.join("same.jar").to_str().unwrap())
        .unwrap();
    let old_bytes = std::fs::read(dir.join("mods/same.jar")).unwrap();
    let manifest_path = dir.join("instance_manifest.json");
    let manifest_before = std::fs::read(&manifest_path).unwrap();
    review_set_readonly(&manifest_path, true);

    let result = service.add_manual_artifact("test", v2.join("same.jar").to_str().unwrap());
    review_set_readonly(&manifest_path, false);

    assert!(result.is_err(), "the manifest write was made to fail");
    assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest_before);
    assert_eq!(
        std::fs::read(dir.join("mods/same.jar")).unwrap(),
        old_bytes,
        "the instance file no longer matches its manifest entry after a failed install"
    );
}

#[test]
fn review_manual_reinstall_leaves_one_manifest_entry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "same", "1.0.0");
    let service = crate::install_service::InstallService::new(ctx.clone());
    service
        .add_manual_artifact("test", source.to_str().unwrap())
        .unwrap();
    service
        .add_manual_artifact("test", source.to_str().unwrap())
        .unwrap();

    assert_eq!(
        read_instance_manifest(&dir).mods.len(),
        1,
        "one file, one manifest entry"
    );
}

#[test]
fn review_remove_never_deletes_a_same_named_file_in_another_folder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "pack", "1.0.0");
    let bytes = std::fs::read(&source).unwrap();
    let sha = super::sha256_hex(&bytes);
    let service = crate::install_service::InstallService::new(ctx.clone());
    service
        .commit_verified_artifact(
            "test",
            "Pack.zip",
            "shader",
            "https://example.invalid/Pack.zip",
            None,
            None,
            "registry",
            Some("1.0.0"),
            None,
            Some(&sha),
            None,
            None,
            None,
            false,
            bytes,
        )
        .unwrap();
    // The user deletes the shader by hand and keeps their own resource pack of the same name.
    std::fs::remove_file(dir.join("shaderpacks/Pack.zip")).unwrap();
    std::fs::create_dir_all(dir.join("resourcepacks")).unwrap();
    std::fs::write(dir.join("resourcepacks/Pack.zip"), b"the users own pack").unwrap();

    let _ = service.remove_artifact("test", "Pack.zip");

    assert_eq!(
        std::fs::read(dir.join("resourcepacks/Pack.zip"))
            .ok()
            .as_deref(),
        Some(&b"the users own pack"[..]),
        "a file Agora did not place was deleted"
    );
}

#[test]
fn review_remove_never_deletes_an_untracked_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    std::fs::write(dir.join("mods/mine.jar"), b"the users own mod").unwrap();

    let service = crate::install_service::InstallService::new(ctx.clone());
    let _ = service.remove_artifact("test", "mine.jar");

    assert_eq!(
        std::fs::read(dir.join("mods/mine.jar")).ok().as_deref(),
        Some(&b"the users own mod"[..]),
        "a file Agora did not place was deleted"
    );
}

#[test]
fn review_update_keeps_a_user_replaced_file_of_an_entry_without_object_hash() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let older = write_mod_jar(tmp.path(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));
    // An entry written by an install path that does not record the store object (pack import).
    let mut manifest = read_instance_manifest(&dir);
    manifest.mods[0].content_item_id = None;
    manifest.mods[0].object_sha256 = None;
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();
    std::fs::write(dir.join("mods/sodium.jar"), b"the users new bytes").unwrap();

    let newer = write_mod_jar(tmp.path(), "manual-item", "2.0.0");
    let update = resolve_local_plan(&dir, &newer, "sodium.jar", None);
    assert_success(run_store_plan(&update, &dir, &ctx));

    let aside = dir
        .join(".agora")
        .join("moved-aside")
        .join("mods")
        .join("sodium.jar");
    assert_eq!(
        std::fs::read(&aside).ok().as_deref(),
        Some(&b"the users new bytes"[..]),
        "the user's file was deleted instead of kept"
    );
}

#[test]
fn review_offline_add_refuses_a_world_item() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "world", "1.0.0");
    let item_id = review_store_item(&ctx, &source, "saves/World", Some("minecraft"));

    let service = crate::install_service::InstallService::new(ctx.clone());
    let result = service.install_stored_item("test", &item_id);

    assert!(
        result.is_err(),
        "a world folder is not a single content file"
    );
    assert!(!dir.join("saves/World").exists());
}

#[test]
fn review_offline_add_without_game_id_is_a_manual_mod() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(tmp.path(), "untagged", "1.0.0");
    let item_id = review_store_item(&ctx, &source, "mods/untagged.jar", None);

    let service = crate::install_service::InstallService::new(ctx.clone());
    let installed = service.install_stored_item("test", &item_id).unwrap();

    assert_eq!(installed.source, "manual");
    assert!(installed.registry_id.is_none());
    assert!(installed.modrinth_id.is_none());
    assert!(dir.join("mods/untagged.jar").is_file());
}

#[test]
fn review_provider_origin_round_trips_offline() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, first) = ctx_instance(&tmp, "first");
    let second = ctx.paths.instance_dir("second").unwrap();
    std::fs::create_dir_all(second.join("mods")).unwrap();
    let mut manifest = read_instance_manifest(&first);
    manifest.instance_id = "second".into();
    std::fs::write(
        second.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let source = write_mod_jar(tmp.path(), "provider-mod", "1.0.0");
    let mut plan = resolve_local_plan(&first, &source, "provider.jar", None);
    if let ResolvedArtifact::Download(local) = &mut plan.files_to_add[0].artifact {
        local.metadata.source_type = SourceType::Provider;
        local.metadata.provider = Some(ProviderArtifact {
            provider_id: "plugin-x".into(),
            project_id: "proj".into(),
            version_id: "v1".into(),
            download_hosts: Vec::new(),
            low_security: false,
            security_notes: Vec::new(),
        });
    } else {
        panic!("the local plan is a download artifact");
    }
    let staging = first.join(".agora").join("staging").join("provider-test");
    std::fs::create_dir_all(staging.join("artifacts")).unwrap();
    std::fs::copy(
        &source,
        staging
            .join("artifacts")
            .join(&plan.files_to_add[0].staging_filename),
    )
    .unwrap();
    prepare_manifest(&plan, &first, &staging, Some(&ctx)).unwrap();
    // allow-raw-instance-manifest
    let prepared: InstanceManifest = serde_json::from_slice(
        &std::fs::read(staging.join("instance_manifest.next.json")).unwrap(),
    )
    .unwrap();
    let item_id = prepared.mods[0].content_item_id.clone().unwrap();

    let service = crate::install_service::InstallService::new(ctx.clone());
    let installed = service.install_stored_item("second", &item_id).unwrap();
    let origin = installed.provider.expect("the provider origin is rebuilt");
    assert_eq!(origin.provider_id, "plugin-x");
    assert_eq!(origin.project_id, "proj");
    assert_eq!(origin.version_id, "v1");
    assert_eq!(installed.source, "provider");
}
