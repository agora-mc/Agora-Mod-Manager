//! INI files that mods ship, kept as the instance's own copy (MASTER_SPEC §26.5).
//!
//! The tests use a hand-written instance manifest with content layers, so no game install or base
//! is needed. Every temporary path is unique to its test through `tempfile`.

use std::path::{Path, PathBuf};

use agora_core::content_store;
use agora_core::ctx::CoreContext;
use agora_core::game_instance::GameInstanceManifest;
use agora_core::game_mod_ini::{self, ModIniError, ModIniSource};
use agora_game_api::{
    BaseReference, GameId, InstallId, Layer, LayerId, LayerSource, LayerStack, RelPath,
};
use tempfile::TempDir;

fn context(tmp: &TempDir) -> CoreContext {
    let ctx = CoreContext::for_testing(tmp.path().join("app_data"));
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    ctx
}

fn layer(id: &str, item_id: &str, enabled: bool) -> Layer {
    Layer {
        id: LayerId::new(id).unwrap(),
        enabled,
        mount_path: RelPath::default(),
        source_path: RelPath::default(),
        source: LayerSource::Content {
            content: item_id.to_string(),
        },
        whiteouts: Vec::new(),
        own_copy: false,
    }
}

fn make_instance(ctx: &CoreContext, instance_id: &str, layers: Vec<Layer>) {
    let dir = ctx.paths.instance_dir(instance_id).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let mut manifest = GameInstanceManifest::new(
        GameId::new("test-game").unwrap(),
        instance_id,
        instance_id,
        None,
        BaseReference::Unpinned {
            install: InstallId::new("steam:1").unwrap(),
            reason: "test".into(),
        },
    );
    manifest.layers = LayerStack::new(layers).unwrap();
    std::fs::write(
        dir.join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn set_layers(ctx: &CoreContext, instance_id: &str, layers: Vec<Layer>) {
    let path = ctx
        .paths
        .instance_dir(instance_id)
        .unwrap()
        .join("instance_manifest.json");
    let mut manifest: GameInstanceManifest =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    manifest.layers = LayerStack::new(layers).unwrap();
    std::fs::write(
        ctx.paths
            .instance_dir(instance_id)
            .unwrap()
            .join("instance_manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn add_item(ctx: &CoreContext, name: &str, files: &[(&str, &[u8])]) -> String {
    let folder = tempfile::tempdir().unwrap();
    for (rel, bytes) in files {
        let path = folder.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    content_store::add_folder(ctx, folder.path(), Some(name))
        .unwrap()
        .item()
        .item_id
        .clone()
}

fn store_item(ctx: &CoreContext, item_id: &str) -> content_store::ContentItem {
    content_store::get_item(ctx, item_id).unwrap()
}

fn override_file(ctx: &CoreContext, instance_id: &str, rel: &str) -> PathBuf {
    ctx.paths
        .instance_dir(instance_id)
        .unwrap()
        .join("overrides")
        .join(rel)
}

fn write_override(ctx: &CoreContext, instance_id: &str, rel: &str, bytes: &[u8]) {
    let path = override_file(ctx, instance_id, rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn files_under(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.is_file() {
                out.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn the_same_ini_from_two_enabled_mods_is_one_row_naming_the_winning_mod() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[
            (
                "SKSE/Plugins/SSEDisplayTweaks.ini",
                b"[Display]\nfGamma=1.0\n",
            ),
            ("Readme.txt", b"not an ini"),
        ],
    );
    let b = add_item(
        &ctx,
        "Mod B",
        &[(
            "SKSE/Plugins/SSEDisplayTweaks.ini",
            b"[Display]\nfGamma=2.0\n",
        )],
    );
    make_instance(
        &ctx,
        "inst-a",
        vec![layer("mod-a", &a, true), layer("mod-b", &b, true)],
    );

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "SKSE/Plugins/SSEDisplayTweaks.ini");
    assert_eq!(rows[0].mod_id, b);
    assert_eq!(rows[0].mod_name, "Mod B");
    assert!(rows[0].mod_enabled);
    assert!(!rows[0].overridden);
    assert!(rows[0].shadowed_by.is_none());
}

#[test]
fn the_extension_is_matched_without_case_but_other_files_are_not_listed() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[("Data/Config.INI", b"k=v\n"), ("Data/readme.txt", b"x")],
    );
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "Data/Config.INI");
}

#[test]
fn writing_makes_only_the_instances_copy_and_reset_goes_back_to_the_mod() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[(
            "SKSE/Plugins/SSEDisplayTweaks.ini",
            b"[Display]\r\n; mine\r\nfGamma=1.0\r\n",
        )],
    );
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let item = store_item(&ctx, &a);
    let sha = &item.files[0].sha256;
    let store_path = ctx.paths.content_object_path(sha);
    let before = (
        std::fs::read(&store_path).unwrap(),
        std::fs::metadata(&store_path).unwrap().modified().unwrap(),
    );

    let changed = game_mod_ini::set_mod_ini_value(
        &ctx,
        "inst-a",
        "SKSE/Plugins/SSEDisplayTweaks.ini",
        "Display",
        "fGamma",
        "2.2",
    )
    .unwrap();
    assert!(changed);

    let copy = override_file(&ctx, "inst-a", "SKSE/Plugins/SSEDisplayTweaks.ini");
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        b"[Display]\r\n; mine\r\nfGamma=2.2\r\n".as_slice(),
        "only that key changed; comments and order are kept"
    );
    assert_eq!(
        (
            std::fs::read(&store_path).unwrap(),
            std::fs::metadata(&store_path).unwrap().modified().unwrap()
        ),
        before,
        "the stored mod's bytes and time are unchanged"
    );
    let read =
        game_mod_ini::read_mod_ini(&ctx, "inst-a", "SKSE/Plugins/SSEDisplayTweaks.ini").unwrap();
    assert_eq!(read.source, ModIniSource::Override);
    assert_eq!(
        read.document.get("Display", "fGamma").as_deref(),
        Some("2.2")
    );

    assert!(game_mod_ini::unset_mod_ini_value(
        &ctx,
        "inst-a",
        "SKSE/Plugins/SSEDisplayTweaks.ini",
        "Display",
        "fGamma",
    )
    .unwrap());
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        b"[Display]\r\n; mine\r\n".as_slice()
    );

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "SKSE/Plugins/SSEDisplayTweaks.ini").unwrap();
    let read =
        game_mod_ini::read_mod_ini(&ctx, "inst-a", "SKSE/Plugins/SSEDisplayTweaks.ini").unwrap();
    assert_eq!(read.source, ModIniSource::Mod);
    assert_eq!(
        read.document.get("Display", "fGamma").as_deref(),
        Some("1.0")
    );
    assert!(
        !override_file(&ctx, "inst-a", "SKSE/Plugins").exists(),
        "emptied folders go"
    );
    assert!(
        override_file(&ctx, "inst-a", "").exists(),
        "the overrides folder itself stays"
    );
    assert_eq!(
        (
            std::fs::read(&store_path).unwrap(),
            std::fs::metadata(&store_path).unwrap().modified().unwrap()
        ),
        before
    );
}

#[test]
fn reset_of_a_listed_path_with_no_override_is_a_no_op_success() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"a=b\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "x.ini").unwrap();
    assert!(!override_file(&ctx, "inst-a", "x.ini").exists());
    assert!(!ctx
        .paths
        .instance_dir("inst-a")
        .unwrap()
        .join("overrides")
        .exists());
}

#[test]
fn a_mod_turned_off_keeps_its_override_listed_but_cannot_edit_it() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[("SKSE/Plugins/tweak.ini", b"[General]\na=b\n")],
    );
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    game_mod_ini::set_mod_ini_value(
        &ctx,
        "inst-a",
        "SKSE/Plugins/tweak.ini",
        "General",
        "a",
        "mine",
    )
    .unwrap();

    set_layers(&ctx, "inst-a", vec![layer("mod-a", &a, false)]);

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert_eq!(rows.len(), 1, "the user's override is still visible");
    assert_eq!(rows[0].path, "SKSE/Plugins/tweak.ini");
    assert_eq!(rows[0].mod_id, a);
    assert!(!rows[0].mod_enabled);
    assert!(rows[0].overridden);

    let error = game_mod_ini::set_mod_ini_value(
        &ctx,
        "inst-a",
        "SKSE/Plugins/tweak.ini",
        "General",
        "a",
        "again",
    )
    .unwrap_err();
    assert!(matches!(error, ModIniError::NotEditable { .. }));
    assert_eq!(
        std::fs::read(override_file(&ctx, "inst-a", "SKSE/Plugins/tweak.ini")).unwrap(),
        b"[General]\na=mine\n"
    );

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "SKSE/Plugins/tweak.ini").unwrap();
    assert!(game_mod_ini::list_mod_inis(&ctx, "inst-a")
        .unwrap()
        .is_empty());
}

#[test]
fn an_override_no_mod_ships_is_listed_and_can_only_be_reset() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    make_instance(&ctx, "inst-a", Vec::new());
    write_override(
        &ctx,
        "inst-a",
        "SKSE/Plugins/old.ini",
        b"[General]\nold=1\n",
    );

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "SKSE/Plugins/old.ini");
    assert_eq!(rows[0].mod_id, "");
    assert_eq!(rows[0].mod_name, "");
    assert!(!rows[0].mod_enabled);
    assert!(rows[0].overridden);
    assert!(matches!(
        game_mod_ini::set_mod_ini_value(
            &ctx,
            "inst-a",
            "SKSE/Plugins/old.ini",
            "General",
            "old",
            "2"
        ),
        Err(ModIniError::NotEditable { .. })
    ));
    let read = game_mod_ini::read_mod_ini(&ctx, "inst-a", "SKSE/Plugins/old.ini").unwrap();
    assert_eq!(read.source, ModIniSource::Override);
    assert_eq!(read.document.get("General", "old").as_deref(), Some("1"));

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "SKSE/Plugins/old.ini").unwrap();
    assert!(!override_file(&ctx, "inst-a", "SKSE/Plugins/old.ini").exists());
    assert!(game_mod_ini::list_mod_inis(&ctx, "inst-a")
        .unwrap()
        .is_empty());
}

#[test]
fn every_path_that_is_not_listed_is_refused_before_anything_is_touched() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("SKSE/Plugins/good.ini", b"a=b\n")]);
    let b = add_item(&ctx, "Mod B", &[("Other/only-in-b.ini", b"c=d\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    make_instance(&ctx, "inst-b", vec![layer("mod-b", &b, true)]);

    let instance_dir = ctx.paths.instance_dir("inst-a").unwrap();
    let before = files_under(&instance_dir);
    let item = store_item(&ctx, &a);
    let store = ctx.paths.content_object_path(&item.files[0].sha256);
    let store_before = (
        std::fs::read(&store).unwrap(),
        std::fs::metadata(&store).unwrap().modified().unwrap(),
    );

    let refused = [
        "",
        "   ",
        "C:/x.ini",
        "/x.ini",
        "\\\\server\\x.ini",
        "../evil.ini",
        "a/../evil.ini",
        "..\\..\\evil.ini",
        "C:x.ini",
        "NUL.ini",
        "con.ini",
        "SKSE/Plugins/good.ini.",
        "SKSE/Plugins/good.ini ",
        "SKSE/Plugins/good.ini/other.ini",
        "not.ini",
        "Other/only-in-b.ini",
    ];
    for path in refused {
        assert!(
            matches!(
                game_mod_ini::set_mod_ini_value(&ctx, "inst-a", path, "s", "k", "v"),
                Err(ModIniError::UnknownPath { .. })
            ),
            "write refused {path:?}"
        );
        assert!(
            matches!(
                game_mod_ini::reset_mod_ini(&ctx, "inst-a", path),
                Err(ModIniError::UnknownPath { .. })
            ),
            "reset refused {path:?}"
        );
        assert!(
            matches!(
                game_mod_ini::read_mod_ini(&ctx, "inst-a", path),
                Err(ModIniError::UnknownPath { .. })
            ),
            "read refused {path:?}"
        );
    }

    assert_eq!(
        files_under(&instance_dir),
        before,
        "nothing was created or deleted"
    );
    assert_eq!(
        (
            std::fs::read(&store).unwrap(),
            std::fs::metadata(&store).unwrap().modified().unwrap()
        ),
        store_before
    );
}

#[test]
fn a_file_where_the_overrides_folder_should_be_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"a=b\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    std::fs::write(
        ctx.paths.instance_dir("inst-a").unwrap().join("overrides"),
        b"junk",
    )
    .unwrap();

    assert!(matches!(
        game_mod_ini::list_mod_inis(&ctx, "inst-a"),
        Err(ModIniError::OverridesNotAFolder { .. })
    ));
    assert!(matches!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "v"),
        Err(ModIniError::OverridesNotAFolder { .. })
    ));
    assert!(!override_file(&ctx, "inst-a", "x.ini").exists());
}

#[cfg(windows)]
#[test]
fn a_junction_inside_overrides_that_points_outside_is_refused() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"a=b\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let outside = TempDir::new().unwrap();
    std::fs::write(outside.path().join("evil.ini"), b"a=evil\n").unwrap();
    let overrides = ctx.paths.instance_dir("inst-a").unwrap().join("overrides");
    std::fs::create_dir_all(&overrides).unwrap();
    junction::create(outside.path(), overrides.join("linked")).unwrap();

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert!(
        rows.iter().all(|row| row.path != "linked/evil.ini"),
        "a path through the junction is not listed"
    );
    assert!(matches!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "linked/evil.ini", "s", "k", "v"),
        Err(ModIniError::UnknownPath { .. })
    ));
    assert!(matches!(
        game_mod_ini::reset_mod_ini(&ctx, "inst-a", "linked/evil.ini"),
        Err(ModIniError::UnknownPath { .. })
    ));
    assert_eq!(
        std::fs::read(outside.path().join("evil.ini")).unwrap(),
        b"a=evil\n",
        "the file outside the instance is untouched"
    );
}

#[test]
fn a_file_that_is_not_text_is_refused_and_never_written() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("binary.ini", &[0xff, 0x00, 0x01])]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);

    assert!(matches!(
        game_mod_ini::read_mod_ini(&ctx, "inst-a", "binary.ini"),
        Err(ModIniError::Unparseable(_))
    ));
    assert!(matches!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "binary.ini", "s", "k", "v"),
        Err(ModIniError::Unparseable(_))
    ));
    assert!(!ctx
        .paths
        .instance_dir("inst-a")
        .unwrap()
        .join("overrides")
        .exists());

    let b = add_item(
        &ctx,
        "Mod B",
        &[(
            "text.ini", b"a=b
",
        )],
    );
    set_layers(&ctx, "inst-a", vec![layer("mod-b", &b, true)]);
    write_override(&ctx, "inst-a", "text.ini", &[0xff, 0xfe]);
    assert!(matches!(
        game_mod_ini::read_mod_ini(&ctx, "inst-a", "text.ini"),
        Err(ModIniError::Unparseable(_))
    ));
    assert!(matches!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "text.ini", "s", "k", "v"),
        Err(ModIniError::Unparseable(_))
    ));
    assert_eq!(
        std::fs::read(override_file(&ctx, "inst-a", "text.ini")).unwrap(),
        vec![0xff, 0xfe]
    );
}
