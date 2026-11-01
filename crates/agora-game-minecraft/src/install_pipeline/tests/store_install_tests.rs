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
    serde_json::from_slice(&std::fs::read(dir.join("instance_manifest.json")).unwrap()).unwrap()
}

fn run_store_plan(plan: &ResolvedInstallPlan, dir: &Path, ctx: &Ctx) -> InstallOutcome {
    tokio::runtime::Runtime::new().unwrap().block_on(
        InstallPipeline.execute_plan_with_scheduler(
            plan,
            dir,
            "registry-rev",
            &NoopReporter,
            &CancellationToken::new(),
            InstallExecutionResources {
                content_store: Some(ctx),
                ..Default::default()
            },
        ),
    )
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
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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

    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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
    let older = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));
    let before = read_instance_manifest(&dir).mods[0].clone();
    let old_bytes = std::fs::read(dir.join("mods/sodium.jar")).unwrap();

    let newer = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "2.0.0");
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
    assert_eq!(updated.mods[0].version.as_deref(), Some("2.0.0"));
}

#[test]
fn removing_a_linked_mod_leaves_the_store_object() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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
    let older = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
    let first = resolve_local_plan(&dir, &older, "sodium.jar", None);
    assert_success(run_store_plan(&first, &dir, &ctx));

    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();
    std::fs::write(dir.join("mods/sodium.jar"), b"the user's new bytes").unwrap();
    let newer = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "2.0.0");
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
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
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
    let source = write_mod_jar(&tmp.path().to_path_buf(), "manual-item", "1.0.0");
    let plan = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&plan, &dir, &ctx));
    std::fs::remove_file(dir.join("mods/sodium.jar")).unwrap();

    let again = resolve_local_plan(&dir, &source, "sodium.jar", None);
    assert_success(run_store_plan(&again, &dir, &ctx));
    assert!(dir.join("mods/sodium.jar").is_file());
    assert_eq!(read_instance_manifest(&dir).mods.len(), 1);
}

#[test]
fn adding_a_stored_item_offline_needs_no_network() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (mut ctx, dir) = ctx_instance(&tmp, "test");
    let source = write_mod_jar(&tmp.path().to_path_buf(), "offline", "1.0.0");
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
