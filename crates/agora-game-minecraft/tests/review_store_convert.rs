//! Review tests for the store-convert slice. Run the `#[ignore]`d ones with
//! `cargo test -p agora-game-minecraft --test review_store_convert -- --ignored`.

#[cfg(windows)]
use agora_core::models::{InstalledMod, InstanceManifest, CURRENT_MANIFEST_VERSION};

#[cfg(windows)]
#[test]
#[ignore = "proves bug: a present file that cannot be read (held open by the game or a scanner) is reported as Missing, not as unreadable (installed_content.rs file_state `Err(_) => FileState::Missing`)"]
fn a_present_file_that_is_locked_is_not_reported_missing() {
    use agora_game_minecraft::installed_content::{list_installed_content, FileState};
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("mods")).unwrap();
    let jar = dir.path().join("mods/held.jar");
    std::fs::write(&jar, b"bytes the manifest recorded").unwrap();
    let hash = agora_core::helpers::hash_file_sha256(&jar).unwrap();

    let entry = InstalledMod {
        provider: None,
        update_pinned: false,
        pack_managed: false,
        installed_as_dependency: false,
        filename: "held.jar".into(),
        registry_id: None,
        modrinth_id: None,
        source: "manual_drag_drop".into(),
        source_url: None,
        version: None,
        sha256: hash,
        hash_verified: true,
        installed_at: "2026-01-01T00:00:00Z".into(),
        java_packages: Vec::new(),
        mod_jar_id: None,
        provided_mod_ids: Vec::new(),
        enabled: true,
        content_type: "mod".into(),
        depends_on: Vec::new(),
        optional_deps: Vec::new(),
        incompatible_deps: Vec::new(),
        content_item_id: None,
        object_sha256: None,
    };
    let manifest = InstanceManifest {
        manifest_version: CURRENT_MANIFEST_VERSION,
        game_data: Default::default(),
        pack_origin: None,
        instance_id: "t".into(),
        name: "T".into(),
        created_from_pack: None,
        minecraft_version: "1.21".into(),
        loader: "fabric".into(),
        loader_version: "1".into(),
        is_locked: false,
        mods: vec![entry],
        resourcepacks: Vec::new(),
        shaders: Vec::new(),
        datapacks: Vec::new(),
        worlds: Vec::new(),
        user_preferences: serde_json::Value::Object(Default::default()),
    };

    // Held open with no sharing, as a game or virus scanner might.
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&jar)
        .unwrap();
    let rows = list_installed_content(dir.path(), &manifest, None, None);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].file_present);
    assert_ne!(
        rows[0].file_state,
        FileState::Missing,
        "the file is on disk (file_present is true) yet it is shown as 'Missing file'"
    );
}
