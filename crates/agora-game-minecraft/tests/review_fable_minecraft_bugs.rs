//! Review findings (review/fable-minecraft). Each test states the behaviour a
//! player should get and currently FAILS; none of them is a fix.

use std::io::Write;

use agora_core::ctx::Ctx;

fn zip_with(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, bytes) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
}

/// The direct content paths (`remove_artifact`, `add_manual_artifact`,
/// `install_artifact*`) read-modify-write the instance manifest without taking
/// the instance lock or asking whether a game is using the instance. Worlds,
/// servers, loadouts and data packs all refuse here (`check_idle`); these do not.
#[test]
#[ignore = "review finding H1: fixed by slice W1-mcguard"]
fn removing_a_mod_is_refused_while_a_launch_is_in_progress() {
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_path_buf());
    let dir = ctx.paths.instance_dir("inst").unwrap();
    std::fs::create_dir_all(dir.join("mods")).unwrap();
    let jar = dir.join("mods/sodium.jar");
    std::fs::write(&jar, b"jar bytes").unwrap();
    let sha256 = agora_core::helpers::hash_file_sha256(&jar).unwrap();
    let manifest = serde_json::json!({
        "instance_id": "inst", "name": "Inst", "minecraft_version": "1.21.1",
        "loader": "fabric", "loader_version": "0.16.0", "is_locked": false,
        "mods": [{
            "filename": "sodium.jar", "registry_id": null, "modrinth_id": null,
            "source": "manual", "version": null, "sha256": sha256, "installed_at": "now"
        }],
        "resourcepacks": [], "shaders": [], "datapacks": [], "worlds": [],
        "user_preferences": {},
        "manifest_version": agora_core::models::CURRENT_MANIFEST_VERSION
    });
    std::fs::write(
        ctx.paths.instance_manifest("inst").unwrap(),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // What LaunchService publishes immediately before it spawns Java.
    agora_core::instance_runtime::publish_starting(&ctx.paths, "inst").unwrap();
    // Sanity: the shared gate does consider this instance busy.
    assert!(agora_core::instance_runtime::check_idle(&ctx.paths, "inst").is_err());

    let result = agora_game_minecraft::install_service::InstallService::new(ctx.clone())
        .remove_artifact("inst", "sodium.jar");

    assert!(
        result.is_err(),
        "remove_artifact changed an instance a game is starting in: {result:?}"
    );
    assert!(jar.is_file(), "the jar was deleted under a starting game");
}

/// `[[dependencies.x]] side = "CLIENT"` says on which side a *dependency* is
/// needed. Server export reads any `side = "CLIENT"` line in mods.toml as "this
/// mod is client-only" and leaves the mod out of the server.
#[test]
fn server_export_keeps_a_forge_mod_whose_dependency_is_client_side() {
    let tmp = tempfile::tempdir().unwrap();
    let instance = tmp.path().join("instance");
    std::fs::create_dir_all(instance.join("mods")).unwrap();
    let mods_toml = br#"
modLoader = "javafml"
loaderVersion = "[47,)"
[[mods]]
modId = "worldgen"
version = "1.0.0"
[[dependencies.worldgen]]
modId = "minecraft"
mandatory = true
versionRange = "[1.20.1]"
side = "BOTH"
[[dependencies.worldgen]]
modId = "jei"
mandatory = false
versionRange = "[15,)"
side = "CLIENT"
"#;
    zip_with(
        &instance.join("mods/worldgen.jar"),
        &[("META-INF/mods.toml", mods_toml)],
    );
    let dest = tmp.path().join("server");

    let result = agora_game_minecraft::server_export::export_server_environment(
        &instance, &dest, "forge", "1.20.1",
    )
    .unwrap();

    assert!(
        result.removed_client_only.is_empty(),
        "a both-sides mod was dropped as client-only: {:?}",
        result.removed_client_only
    );
    assert!(dest.join("mods/worldgen.jar").is_file());
}

/// The override policy refuses `overrides/config/run.bat`. The same file listed
/// in the index's `files` (bytes carried inside the pack) is only checked for
/// path traversal, so it lands in the instance; so would `.agora/...`.
#[test]
fn an_mrpack_cannot_place_a_banned_file_through_its_files_list() {
    use sha1::Digest as _;
    let payload: &[u8] = b"@echo off\r\necho hello\r\n";
    let sha1 = hex::encode(sha1::Sha1::digest(payload));
    let sha512 = hex::encode(sha2::Sha512::digest(payload));
    let index = serde_json::json!({
        "formatVersion": 1, "game": "minecraft", "name": "listed-bat", "versionId": "1",
        "dependencies": { "minecraft": "1.21.1" },
        "files": [{
            "path": "config/run.bat",
            "hashes": { "sha1": sha1, "sha512": sha512 },
            "downloads": [], "fileSize": payload.len()
        }]
    });
    let tmp = tempfile::tempdir().unwrap();
    let pack = tmp.path().join("listed-bat.mrpack");
    zip_with(
        &pack,
        &[
            (
                "modrinth.index.json",
                serde_json::to_vec(&index).unwrap().as_slice(),
            ),
            ("config/run.bat", payload),
        ],
    );
    let instances_root = tmp.path().join("instances");

    let result = agora_game_minecraft::import::import_mrpack(&pack, &instances_root, false);

    assert!(
        !instances_root.join("listed-bat/config/run.bat").exists(),
        "a .bat the override policy bans was imported through `files` (result: {:?})",
        result.as_ref().map(|r| &r.instance_id)
    );
}

/// Launch commands (wrapper, pre-launch, post-exit, custom arguments) are split
/// by `parse_argument_string`, which treats `\\` as an escaped backslash
/// everywhere, so a Windows network path loses its leading backslash.
#[test]
#[ignore = "review finding L1: fixed by slice W1-import-small"]
fn a_unc_path_survives_launch_argument_parsing() {
    let parsed =
        agora_game_minecraft::launch_planner::parse_argument_string(r"\\nas\tools\wrap.exe --fast")
            .unwrap();
    assert_eq!(parsed, vec![r"\\nas\tools\wrap.exe", "--fast"]);
}
