//! Cross-review of the mod INI slice (`game_mod_ini`): hostile inputs, junctions, hardlinks, temp
//! files and the stored mod file. Each test names the contract it checks in `inis.md`.
//!
//! Tests that FAIL here are proven bugs, not test mistakes; see the commit message and the report.

use std::path::{Path, PathBuf};

use agora_core::content_store;
use agora_core::ctx::CoreContext;
use agora_core::game_instance::GameInstanceManifest;
use agora_core::game_mod_ini;
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

/// Every entry under `root` (folders and files, with a file's bytes), sorted. Nothing a refused
/// call creates or deletes changes it.
fn snapshot(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            out.push(format!("unreadable {}", dir.display()));
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_dir() {
                out.push(format!("dir {rel}"));
                walk(root, &path, out);
            } else if meta.file_type().is_file() {
                out.push(format!("file {rel} {:?}", std::fs::read(&path).unwrap()));
            } else {
                out.push(format!("other {rel}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn temp_leftovers(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".agora-tmp"))
        .collect()
}

#[test]
fn review_hostile_inputs_are_refused_and_change_nothing_on_disk() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[
            (
                "SKSE/Plugins/SSEDisplayTweaks.ini",
                b"[Display]\nfGamma=1\n",
            ),
            ("x.ini", b"a=b\n"),
        ],
    );
    let off = add_item(&ctx, "Mod Off", &[("off.ini", b"a=b\n")]);
    let other = add_item(&ctx, "Mod Other", &[("elsewhere.ini", b"a=b\n")]);
    make_instance(
        &ctx,
        "inst-a",
        vec![layer("mod-a", &a, true), layer("mod-off", &off, false)],
    );
    make_instance(&ctx, "inst-b", vec![layer("mod-other", &other, true)]);

    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("evil.ini"), b"a=evil\n").unwrap();

    let instance = ctx.paths.instance_dir("inst-a").unwrap();
    let before_instance = snapshot(&instance);
    let before_outside = snapshot(&outside);

    let hostile = [
        "",
        " ",
        "\t",
        "/x.ini",
        "\\x.ini",
        "\\\\server\\share\\x.ini",
        "C:/x.ini",
        "C:\\x.ini",
        "C:x.ini",
        "c:x.ini",
        "../evil.ini",
        "..\\evil.ini",
        "..\\..\\evil.ini",
        "SKSE/../../evil.ini",
        "a/..\\..\\x.ini",
        "./x.ini",
        ".",
        "..",
        "NUL.ini",
        "con.ini",
        "COM1.ini",
        "aux.INI",
        "x.ini.",
        "x.ini ",
        "x.ini\0",
        "x.txt",
        "x",
        "off.ini",
        "elsewhere.ini",
        "SKSE//Plugins/SSEDisplayTweaks.ini",
        "SKSE/Plugins/SSEDisplayTweaks.ini/",
        "SKSE/Plugins/SSEDisplayTweaks.ini/..",
        "overrides/x.ini",
    ];
    for path in hostile {
        assert!(
            game_mod_ini::set_mod_ini_value(&ctx, "inst-a", path, "s", "k", "v").is_err(),
            "set must refuse {path:?}"
        );
        assert!(
            game_mod_ini::unset_mod_ini_value(&ctx, "inst-a", path, "s", "k").is_err(),
            "unset must refuse {path:?}"
        );
        assert!(
            game_mod_ini::reset_mod_ini(&ctx, "inst-a", path).is_err(),
            "reset must refuse {path:?}"
        );
    }
    assert_eq!(
        snapshot(&instance),
        before_instance,
        "refused calls touch nothing in the instance"
    );
    assert_eq!(
        snapshot(&outside),
        before_outside,
        "refused calls touch nothing outside it"
    );

    // Control: the same calls with a listed path succeed, so the refusals above are not just
    // everything failing.
    assert!(game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "v").unwrap());
    assert!(override_file(&ctx, "inst-a", "x.ini").is_file());
    #[cfg(windows)]
    {
        // Windows matches the listed path without regard to case or separator, and writes to the
        // listed spelling, never to the spelling typed.
        assert!(game_mod_ini::set_mod_ini_value(
            &ctx,
            "inst-a",
            "skse\\plugins\\ssedisplaytweaks.INI",
            "Display",
            "fGamma",
            "2"
        )
        .unwrap());
        assert!(override_file(&ctx, "inst-a", "SKSE/Plugins/SSEDisplayTweaks.ini").is_file());
    }
    // And the other instance never saw any of it.
    assert!(!ctx
        .paths
        .instance_dir("inst-b")
        .unwrap()
        .join("overrides")
        .exists());
}

#[test]
fn review_a_failed_write_leaves_no_temp_file_behind() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"[s]\nk=1\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    // A folder where the override file should be: the final rename onto it must fail.
    let blocker = override_file(&ctx, "inst-a", "x.ini");
    std::fs::create_dir_all(blocker.join("keep")).unwrap();

    let result = game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "2");
    assert!(
        result.is_err(),
        "the write must fail when its target is a folder"
    );
    assert_eq!(
        temp_leftovers(blocker.parent().unwrap()),
        Vec::<String>::new(),
        "a failed write must not leave its temp file in overrides/"
    );
}

#[test]
fn review_a_hardlinked_override_is_refused_for_write_and_reset() {
    // inis.md: "the override FILE itself is a junction/hardlink to a file outside. Writes and
    // resets must refuse."
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"[s]\nk=1\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let outside = tmp.path().join("outside.ini");
    std::fs::write(&outside, b"a=evil\n").unwrap();
    let dest = override_file(&ctx, "inst-a", "x.ini");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::hard_link(&outside, &dest).unwrap();

    assert!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "v").is_err(),
        "a write to a hardlinked override must be refused"
    );
    assert!(
        game_mod_ini::reset_mod_ini(&ctx, "inst-a", "x.ini").is_err(),
        "a reset of a hardlinked override must be refused"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"a=evil\n");
}

#[test]
fn review_reset_removes_only_the_folders_it_empties_inside_overrides() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[("a/b/c/x.ini", b"k=1\n"), ("a/keep.ini", b"k=2\n")],
    );
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    write_override(&ctx, "inst-a", "a/b/c/x.ini", b"k=3\n");
    write_override(&ctx, "inst-a", "a/keep.ini", b"k=4\n");
    let overrides = ctx.paths.instance_dir("inst-a").unwrap().join("overrides");

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "a/b/c/x.ini").unwrap();
    assert!(!overrides.join("a/b").exists(), "emptied folders go");
    assert!(
        overrides.join("a/keep.ini").is_file(),
        "a folder with a file left in it stays"
    );

    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "a/keep.ini").unwrap();
    assert!(!overrides.join("a").exists());
    assert!(overrides.is_dir(), "overrides/ itself is never removed");

    // An orphan override (no mod ships it) resets the same way.
    write_override(&ctx, "inst-a", "q/r/orphan.ini", b"k=5\n");
    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "q/r/orphan.ini").unwrap();
    assert!(!overrides.join("q").exists());
    assert!(overrides.is_dir());
}

#[cfg(windows)]
#[test]
fn review_a_junction_on_the_way_to_a_listed_file_is_refused_and_writes_nothing_outside() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(
        &ctx,
        "Mod A",
        &[("linked/x.ini", b"a=b\n"), ("plain.ini", b"a=b\n")],
    );
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.ini"), b"a=evil\n").unwrap();
    let overrides = ctx.paths.instance_dir("inst-a").unwrap().join("overrides");
    std::fs::create_dir_all(&overrides).unwrap();
    junction::create(&outside, overrides.join("linked")).unwrap();
    let before = snapshot(&outside);

    assert!(
        game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "linked/x.ini", "a", "b", "c").is_err()
    );
    assert!(game_mod_ini::reset_mod_ini(&ctx, "inst-a", "linked/x.ini").is_err());
    assert_eq!(
        snapshot(&outside),
        before,
        "nothing outside the instance changed"
    );
}

#[cfg(windows)]
#[test]
fn review_an_overrides_folder_that_is_a_junction_outside_is_refused() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"a=b\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.ini"), b"a=evil\n").unwrap();
    let overrides = ctx.paths.instance_dir("inst-a").unwrap().join("overrides");
    junction::create(&outside, &overrides).unwrap();
    let before = snapshot(&outside);

    assert!(game_mod_ini::list_mod_inis(&ctx, "inst-a").is_err());
    assert!(game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "a", "b", "c").is_err());
    assert!(game_mod_ini::reset_mod_ini(&ctx, "inst-a", "x.ini").is_err());
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn review_a_nul_byte_in_a_text_looking_mod_file_is_refused_before_any_write() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("notes.ini", b"[s]\nk=v\0junk\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);

    assert!(game_mod_ini::read_mod_ini(&ctx, "inst-a", "notes.ini").is_err());
    assert!(game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "notes.ini", "s", "k", "v").is_err());
    assert!(game_mod_ini::unset_mod_ini_value(&ctx, "inst-a", "notes.ini", "s", "k").is_err());
    assert!(
        !ctx.paths
            .instance_dir("inst-a")
            .unwrap()
            .join("overrides")
            .exists(),
        "nothing was written"
    );
}

#[test]
fn review_the_stored_mod_file_keeps_its_bytes_and_mtime_through_every_edit() {
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let item_id = add_item(&ctx, "Mod A", &[("x.ini", b"[s]\nk=1\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &item_id, true)]);
    let item = content_store::get_item(&ctx, &item_id).unwrap();
    let store = ctx.paths.content_object_path(&item.files[0].sha256);
    let bytes = std::fs::read(&store).unwrap();
    let mtime = std::fs::metadata(&store).unwrap().modified().unwrap();

    game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "2").unwrap();
    assert_eq!(std::fs::read(&store).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&store).unwrap().modified().unwrap(),
        mtime
    );
    game_mod_ini::unset_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k").unwrap();
    assert_eq!(std::fs::read(&store).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&store).unwrap().modified().unwrap(),
        mtime
    );
    game_mod_ini::set_mod_ini_value(&ctx, "inst-a", "x.ini", "s", "k", "3").unwrap();
    game_mod_ini::reset_mod_ini(&ctx, "inst-a", "x.ini").unwrap();
    assert_eq!(std::fs::read(&store).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&store).unwrap().modified().unwrap(),
        mtime
    );
}

#[test]
fn review_a_path_that_is_a_folder_in_overrides_is_not_reset_or_listed_as_a_file() {
    // A folder named like a mod's INI must not be read as an override, or reset as one.
    let tmp = TempDir::new().unwrap();
    let ctx = context(&tmp);
    let a = add_item(&ctx, "Mod A", &[("x.ini", b"a=b\n")]);
    make_instance(&ctx, "inst-a", vec![layer("mod-a", &a, true)]);
    let folder = override_file(&ctx, "inst-a", "x.ini");
    std::fs::create_dir_all(folder.join("inner")).unwrap();
    std::fs::write(folder.join("inner/keep.txt"), b"keep").unwrap();
    let before = snapshot(&folder);

    let rows = game_mod_ini::list_mod_inis(&ctx, "inst-a").unwrap();
    assert!(rows.iter().all(|row| !row.overridden));
    assert!(game_mod_ini::reset_mod_ini(&ctx, "inst-a", "x.ini").is_err());
    assert_eq!(
        snapshot(&folder),
        before,
        "the folder and its contents are untouched"
    );
}
