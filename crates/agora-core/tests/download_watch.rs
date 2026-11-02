//! Download watching (MASTER_SPEC §26.7): recognition by contents, partial files, the baseline,
//! ignoring, and adding without touching the player's file.

mod common;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use agora_core::content_store::{self, AddOutcome};
use agora_core::ctx::Ctx;
use agora_core::download_watch::{
    self, AddDownloadOutcome, WatchState, DOWNLOADS_FOLDER_SETTING, IGNORED_DOWNLOADS_SETTING,
    NOTICE_DOWNLOADS_SETTING,
};
use agora_core::game_registry::{DeclarativePackage, GameRegistry, PackageSource};
use agora_core::settings::SettingsService;
use agora_game_api::{ContentLayout, PackageDefinition, RelPath};
use tempfile::TempDir;

fn skyrim_layout() -> ContentLayout {
    ContentLayout {
        data_path: RelPath::new("Data").unwrap(),
        data_markers: vec![
            "*.esp".into(),
            "*.esm".into(),
            "*.esl".into(),
            "*.bsa".into(),
            "meshes".into(),
            "textures".into(),
            "scripts".into(),
            "skse".into(),
        ],
        root_markers: vec!["*.exe".into(), "*.dll".into()],
        thunderstore_bepinex: false,
    }
}

fn test_ctx() -> (TempDir, Ctx) {
    let tmp = tempfile::tempdir().unwrap();
    let mut game = common::skyrim_definition();
    game.content_layout = Some(skyrim_layout());
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
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    (tmp, ctx)
}

fn set(ctx: &Ctx, key: &str, value: serde_json::Value) {
    SettingsService::new(ctx.clone()).set(key, &value).unwrap();
}

fn watch_folder(ctx: &Ctx, folder: &Path) {
    set(
        ctx,
        DOWNLOADS_FOLDER_SETTING,
        serde_json::json!(folder.to_string_lossy().to_string()),
    );
}

fn turn_on(ctx: &Ctx) {
    set(ctx, NOTICE_DOWNLOADS_SETTING, serde_json::json!(true));
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

fn create_7z(path: &Path, entries: &[(&str, &[u8])]) {
    let mut sz = sevenz_rust2::ArchiveWriter::create(path).unwrap();
    for (name, content) in entries {
        let entry = sevenz_rust2::ArchiveEntry::new_file(name);
        sz.push_archive_entry(entry, Some(std::io::Cursor::new(*content)))
            .unwrap();
    }
    sz.finish().unwrap();
}

/// A scan has to see the same size twice before it offers a file.
fn scan_until_stable(state: &mut WatchState, ctx: &Ctx) -> Vec<download_watch::StoredDownload> {
    let first = state.scan(ctx).unwrap();
    assert!(first.is_empty(), "the first look only records sizes");
    state.scan(ctx).unwrap()
}

#[test]
fn recognises_skyrim_archives_by_their_contents() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();

    // A FOMOD 7z.
    create_7z(
        &downloads.path().join("Cool Mod-42-1.0-1700000000.7z"),
        &[
            (
                "fomod/ModuleConfig.xml",
                b"<config><moduleName>Cool Mod</moduleName></config>",
            ),
            ("meshes/armor.nif", b"mesh"),
        ],
    );
    // A plain data-folder zip.
    let plain_path = downloads.path().join("Plain Mod.zip");
    create_zip(
        &plain_path,
        &[("Cool.esp", b"esp"), ("meshes/armor.nif", b"mesh")],
    );

    let added = scan_until_stable(&mut state, &ctx);
    assert_eq!(added.len(), 2);
    let fomod = added
        .iter()
        .find(|download| download.file_name.ends_with(".7z"))
        .unwrap();
    assert_eq!(fomod.game_id, "skyrim-se");
    assert_eq!(fomod.kind, "FOMOD installer");
    assert_eq!(fomod.reason, "has fomod/ModuleConfig.xml");
    assert_eq!(fomod.nexus_mod_id.as_deref(), Some("42"));

    let plain = added
        .iter()
        .find(|download| download.file_name == "Plain Mod.zip")
        .unwrap();
    assert_eq!(plain.game_id, "skyrim-se");
    assert_eq!(plain.kind, "game content");
    assert!(plain.reason.contains("Cool.esp"));
    assert_eq!(
        plain.nexus_mod_id, None,
        "a name outside the pattern has no label"
    );
}

#[test]
fn an_unrelated_zip_and_a_disguised_text_file_are_not_offered() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();

    create_zip(
        &downloads.path().join("holiday.zip"),
        &[("beach.jpg", b"photo"), ("menu.txt", b"menu")],
    );
    // A zip's bytes under a name the watcher never looks at.
    create_zip(&downloads.path().join("notes.txt"), &[("Cool.esp", b"esp")]);

    assert!(scan_until_stable(&mut state, &ctx).is_empty());
    assert!(download_watch::list(&ctx).unwrap().is_empty());
}

#[test]
fn partial_downloads_are_not_offered_until_complete() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();

    let partial = downloads.path().join("Cool.zip.crdownload");
    std::fs::write(&partial, b"PK").unwrap();
    state.scan(&ctx).unwrap();
    state.scan(&ctx).unwrap();
    state.scan(&ctx).unwrap();
    assert!(
        download_watch::list(&ctx).unwrap().is_empty(),
        "a browser partial is never offered"
    );

    // A .zip that is still being written is offered only once two polls see the same size.
    let growing = downloads.path().join("Growing.zip");
    create_zip(&growing, &[("Cool.esp", b"esp")]);
    state.scan(&ctx).unwrap(); // record the first size
    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .open(&growing)
        .unwrap();
    append.write_all(b"more").unwrap();
    drop(append);
    assert!(
        state.scan(&ctx).unwrap().is_empty(),
        "a size that changed is still being written"
    );
    let offered = state.scan(&ctx).unwrap();
    assert_eq!(offered.len(), 1, "the stable file is offered");
}

#[test]
fn files_present_when_watching_starts_are_never_offered() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    create_zip(
        &downloads.path().join("Already Here.zip"),
        &[("Cool.esp", b"esp")],
    );

    let mut state = WatchState::start(&ctx).unwrap();
    assert!(scan_until_stable(&mut state, &ctx).is_empty());
    assert!(download_watch::list(&ctx).unwrap().is_empty());
}

#[test]
fn nothing_is_offered_while_the_setting_is_off() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    assert!(!download_watch::enabled(&ctx).unwrap(), "off by default");
    let mut state = WatchState::start(&ctx).unwrap();
    create_zip(&downloads.path().join("Cool.zip"), &[("Cool.esp", b"esp")]);
    state.scan(&ctx).unwrap();
    assert!(state.scan(&ctx).unwrap().is_empty());
    assert!(download_watch::list(&ctx).unwrap().is_empty());
}

#[test]
fn ignoring_a_download_keeps_it_out_of_later_scans() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();
    create_zip(&downloads.path().join("Cool.zip"), &[("Cool.esp", b"esp")]);

    let added = scan_until_stable(&mut state, &ctx);
    assert_eq!(added.len(), 1);
    download_watch::ignore(&ctx, &added[0].sha256).unwrap();

    assert!(download_watch::list(&ctx).unwrap().is_empty());
    assert!(download_watch::scan_folder_now(&ctx, downloads.path())
        .unwrap()
        .is_empty());
    let stored: serde_json::Value = SettingsService::new(ctx.clone())
        .get(IGNORED_DOWNLOADS_SETTING)
        .unwrap()
        .unwrap();
    assert_eq!(stored, serde_json::json!([added[0].sha256.clone()]));
}

#[test]
fn a_file_already_in_the_store_is_marked_and_not_offered_again() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();
    let path = downloads.path().join("Cool.zip");
    create_zip(&path, &[("Cool.esp", b"esp")]);
    let imported = content_store::add_archive(&ctx, &path, Some("Cool")).unwrap();
    assert!(matches!(imported, AddOutcome::Added { .. }));

    let added = scan_until_stable(&mut state, &ctx);
    assert_eq!(added.len(), 1);
    let rows = download_watch::list(&ctx).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].already_in_library,
        "the same bytes are already in the library"
    );
}

#[test]
fn adding_to_the_library_leaves_the_download_untouched() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();
    let path = downloads.path().join("Cool.zip");
    create_zip(
        &path,
        &[("Cool.esp", b"esp"), ("meshes/armor.nif", b"mesh")],
    );
    let before = agora_core::helpers::hash_file_sha256(&path).unwrap();

    let added = scan_until_stable(&mut state, &ctx);
    let outcome = download_watch::add(&ctx, &added[0].sha256).unwrap();
    match outcome {
        AddDownloadOutcome::Added { item_id } => {
            let item = content_store::get_item(&ctx, &item_id).unwrap();
            assert_eq!(item.game_id.as_deref(), Some("skyrim-se"));
            assert_eq!(item.total_size, 7);
        }
        AddDownloadOutcome::NeedsInstaller { .. } => panic!("this archive has no installer"),
    }

    let after = agora_core::helpers::hash_file_sha256(&path).unwrap();
    assert_eq!(before, after, "the downloaded file is unchanged");
    assert!(path.is_file(), "the downloaded file is still there");
    assert!(download_watch::list(&ctx).unwrap().is_empty());
}

#[test]
fn a_fomod_download_asks_the_existing_wizard_to_finish_it() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();
    create_7z(
        &downloads.path().join("Cool.7z"),
        &[
            (
                "fomod/ModuleConfig.xml",
                b"<config><moduleName>Cool Mod</moduleName></config>",
            ),
            ("meshes/armor.nif", b"mesh"),
        ],
    );

    let added = scan_until_stable(&mut state, &ctx);
    let outcome = download_watch::add(&ctx, &added[0].sha256).unwrap();
    match outcome {
        AddDownloadOutcome::NeedsInstaller { title, .. } => {
            assert_eq!(title, "Cool Mod");
        }
        AddDownloadOutcome::Added { .. } => panic!("a FOMOD archive must ask its questions"),
    }
}

#[test]
fn the_nexus_label_is_only_a_label() {
    let (_tmp, ctx) = test_ctx();
    let downloads = tempfile::tempdir().unwrap();
    watch_folder(&ctx, downloads.path());
    turn_on(&ctx);
    let mut state = WatchState::start(&ctx).unwrap();

    create_zip(
        &downloads.path().join("Cool-42-1.0.0-1700000000.zip"),
        &[("Cool.esp", b"esp")],
    );
    create_zip(&downloads.path().join("Cool.zip"), &[("Cool.esp", b"esp")]);

    let added = scan_until_stable(&mut state, &ctx);
    assert_eq!(added.len(), 2);
    for download in &added {
        assert_eq!(
            download.game_id, "skyrim-se",
            "the label changes no recognition"
        );
        assert_eq!(download.kind, "game content");
    }
    let labelled = added
        .iter()
        .find(|download| download.file_name.contains("-42-"))
        .unwrap();
    assert_eq!(labelled.nexus_mod_id.as_deref(), Some("42"));
    let plain = added
        .iter()
        .find(|download| download.file_name == "Cool.zip")
        .unwrap();
    assert_eq!(plain.nexus_mod_id, None);
}
