//! Which game a library item belongs to: recorded, catalog, the item it was made from, and the
//! instances that use it, with nothing guessed. Also the per-item details the Library shows for a
//! game with a plugin list (plugin files, an installer).

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use agora_core::content_store::{
    self, add_archive, add_file, derive_item, get_item, record_game_if_missing, ContentSource,
};
use agora_core::ctx::Ctx;
use agora_core::game_instance::{GameInstanceManifest, SavesChoice};
use agora_core::game_registry::{DeclarativePackage, GameRegistry, PackageSource};
use agora_core::library;
use agora_game_api::{
    BaseReference, GameId, InstallId, Layer, LayerId, LayerSource, LayerStack, PackageDefinition,
    PluginListRule, RelPath,
};
use tempfile::TempDir;

fn plugin_rule() -> PluginListRule {
    PluginListRule {
        user_file: RelPath::new("user/Plugins.txt").unwrap(),
        plugin_folder: RelPath::new("Data").unwrap(),
        patterns: vec!["*.esm".into(), "*.esl".into(), "*.esp".into()],
        active_prefix: "*".into(),
        header: Vec::new(),
        semantics: None,
        implicit: Vec::new(),
        implicit_list_file: None,
        loot: None,
    }
}

/// A context whose registry knows Skyrim SE with a plugin list.
fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let mut game = common::skyrim_definition();
    game.plugin_list = Some(plugin_rule());
    let package = PackageDefinition {
        id: "test.skyrim".into(),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: Vec::new(),
        games: vec![game],
        frameworks: Vec::new(),
        tools: Vec::new(),
    };
    let mut builder = GameRegistry::builder();
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test-skyrim".into(),
            },
            Arc::new(DeclarativePackage(package)),
        )
        .expect("the test Skyrim package is valid");
    let ctx = Ctx::for_testing(tmp.path().to_path_buf()).with_games(Arc::new(builder.build()));
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

fn add_zip(ctx: &Ctx, work: &TempDir, name: &str, entries: &[(&str, &[u8])]) -> String {
    let path = work.path().join(format!("{name}.zip"));
    create_zip(&path, entries);
    add_archive(ctx, &path, Some(name))
        .unwrap()
        .item()
        .item_id
        .clone()
}

fn write_generic_instance(ctx: &Ctx, id: &str, game: &str, item_ids: &[String]) {
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
        name: format!("{id} instance"),
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

fn listed(ctx: &Ctx, item_id: &str) -> library::LibraryItem {
    library::list(ctx)
        .unwrap()
        .items
        .into_iter()
        .find(|item| item.item_id == item_id)
        .expect("the item is listed")
}

#[test]
fn an_item_nothing_ties_to_a_game_has_no_game() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item_id = add_zip(&ctx, &work, "Loose", &[("readme.txt", b"hello")]);

    let item = listed(&ctx, &item_id);
    assert!(item.game_ids.is_empty(), "nothing says which game");
    assert_eq!(item.game_id, None);

    let unused = library::list_unused(&ctx).unwrap();
    let row = unused
        .items
        .iter()
        .find(|row| row.item_id == item_id)
        .unwrap();
    assert!(row.game_ids.is_empty());
}

#[test]
fn the_recorded_game_and_every_using_instance_game_are_listed_once_each() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let shared = add_zip(&ctx, &work, "Shared", &[("Shared.esp", b"esp")]);
    record_game_if_missing(&ctx, &shared, "skyrim-se").unwrap();
    write_generic_instance(&ctx, "sse", "skyrim-se", std::slice::from_ref(&shared));
    write_generic_instance(&ctx, "fo4", "fallout-4", std::slice::from_ref(&shared));

    let item = listed(&ctx, &shared);
    assert_eq!(item.game_ids, vec!["skyrim-se", "fallout-4"]);
    assert_eq!(
        item.game_id.as_deref(),
        Some("skyrim-se"),
        "the recorded game is still the one answer"
    );
}

#[test]
fn an_item_used_by_instances_of_two_games_belongs_to_both_and_to_neither_alone() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let both = add_zip(&ctx, &work, "Both", &[("x.txt", b"x")]);
    write_generic_instance(&ctx, "sse", "skyrim-se", std::slice::from_ref(&both));
    write_generic_instance(&ctx, "fo4", "fallout-4", std::slice::from_ref(&both));

    let item = listed(&ctx, &both);
    // In the order of the instances that use it (by name).
    assert_eq!(item.game_ids, vec!["fallout-4", "skyrim-se"]);
    assert_eq!(item.game_id, None, "two games, so no single one");
}

#[test]
fn an_item_an_installer_made_belongs_to_the_game_of_the_item_it_came_from() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let parent = add_zip(
        &ctx,
        &work,
        "Installer",
        &[
            ("fomod/ModuleConfig.xml", b"<config/>"),
            ("Option A/A.esp", b"a"),
        ],
    );
    record_game_if_missing(&ctx, &parent, "skyrim-se").unwrap();
    let parent_item = get_item(&ctx, &parent).unwrap();
    let option = parent_item
        .files
        .iter()
        .find(|file| file.path.as_str() == "Option A/A.esp")
        .unwrap();
    let derived = derive_item(
        &ctx,
        &parent,
        vec![(RelPath::new("A.esp").unwrap(), option.sha256.clone())],
        "Installer (A)",
        ContentSource::FomodInstall {
            from_item: parent.clone(),
            choices: Vec::new(),
            added_at_unix_ms: 1,
        },
    )
    .unwrap()
    .item()
    .item_id
    .clone();

    let item = listed(&ctx, &derived);
    assert_eq!(item.game_ids, vec!["skyrim-se"]);
    assert_eq!(item.game_id.as_deref(), Some("skyrim-se"));
    assert_eq!(item.plugin_files, vec!["A.esp"]);
    assert!(!item.has_installer);

    let unused = library::list_unused(&ctx).unwrap();
    let row = unused
        .items
        .iter()
        .find(|row| row.item_id == derived)
        .unwrap();
    assert_eq!(row.game_ids, vec!["skyrim-se"]);
}

#[test]
fn plugin_files_and_an_installer_are_listed_for_a_game_with_a_plugin_list() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item_id = add_zip(
        &ctx,
        &work,
        "Lighting",
        &[
            ("Lighting.esp", b"esp"),
            ("Data/Lighting - Patch.ESL", b"esl"),
            ("fomod/ModuleConfig.xml", b"<config/>"),
            ("Options/Deep/Ignored.esp", b"deep"),
            ("textures/sky.dds", b"dds"),
        ],
    );
    record_game_if_missing(&ctx, &item_id, "skyrim-se").unwrap();

    let item = listed(&ctx, &item_id);
    assert_eq!(
        item.plugin_files,
        vec!["Lighting - Patch.ESL", "Lighting.esp"]
    );
    assert!(item.has_installer);
    assert_eq!(item.file_count, 5);
    assert!(item.total_size > 0);
}

#[test]
fn a_game_without_a_plugin_list_lists_no_plugin_files() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("Thing.esp");
    std::fs::write(&file, b"esp").unwrap();
    let item_id = add_file(&ctx, &file, None, None)
        .unwrap()
        .item()
        .item_id
        .clone();
    record_game_if_missing(&ctx, &item_id, "minecraft").unwrap();

    assert!(listed(&ctx, &item_id).plugin_files.is_empty());
}

#[test]
fn recording_a_game_never_replaces_one_already_recorded() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item_id = add_zip(&ctx, &work, "Mine", &[("a.txt", b"a")]);

    let first = record_game_if_missing(&ctx, &item_id, "skyrim-se").unwrap();
    assert_eq!(first.game_id.as_deref(), Some("skyrim-se"));
    let second = record_game_if_missing(&ctx, &item_id, "fallout-4").unwrap();
    assert_eq!(second.game_id.as_deref(), Some("skyrim-se"));
    assert_eq!(
        content_store::get_item(&ctx, &item_id)
            .unwrap()
            .game_id
            .as_deref(),
        Some("skyrim-se")
    );
    // A blank game is not a game.
    let item = add_zip(&ctx, &work, "Blank", &[("b.txt", b"b")]);
    assert_eq!(
        record_game_if_missing(&ctx, &item, "  ").unwrap().game_id,
        None
    );
}

// A restorable deleted instance still needs its items, so "Unused items" must not offer them.
#[test]
fn an_item_a_restorable_deleted_instance_needs_is_not_unused() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item = add_zip(&ctx, &work, "Needed", &[("Needed.esp", b"esp")]);
    write_generic_instance(&ctx, "sse", "skyrim-se", std::slice::from_ref(&item));
    let live = ctx.paths.instance_dir("sse").unwrap();
    let entry = ctx.paths.recently_deleted_games_root().join("entry1");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::rename(&live, entry.join("instance")).unwrap();

    let unused = library::list_unused(&ctx).unwrap();
    assert!(
        !unused.items.iter().any(|row| row.item_id == item),
        "the deleted-but-restorable instance still needs the item"
    );

    // A purged entry's folder is gone, so it no longer uses anything.
    std::fs::remove_dir_all(&entry).unwrap();
    let unused = library::list_unused(&ctx).unwrap();
    assert!(
        unused.items.iter().any(|row| row.item_id == item),
        "a purged entry stops counting"
    );
}

// Minecraft's own Recently deleted area is scanned the same way.
#[test]
fn an_item_a_restorable_minecraft_deleted_instance_needs_is_not_unused() {
    let (_tmp, ctx) = test_ctx();
    let work = tempfile::tempdir().unwrap();
    let item = add_zip(&ctx, &work, "NeededMc", &[("Needed.esp", b"esp")]);
    write_generic_instance(&ctx, "mc", "minecraft", std::slice::from_ref(&item));
    let live = ctx.paths.instance_dir("mc").unwrap();
    let entry = ctx.paths.recently_deleted_root().join("entry1");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::rename(&live, entry.join("instance")).unwrap();

    let unused = library::list_unused(&ctx).unwrap();
    assert!(
        !unused.items.iter().any(|row| row.item_id == item),
        "Minecraft's deleted-but-restorable instance still needs the item"
    );
}
