//! Installing a curated catalog entry into an instance (MASTER_SPEC §26.8). The network is a fake
//! transport: no test reaches GitHub or any other host.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agora_core::artifact_hash::HashOrigin;
use agora_core::catalog_install::{
    install, is_framework_refusal, requirement_findings, CatalogEntryRef, CatalogPlan,
    CatalogTransport, FrameworkProblem, InstallReport, InstallRequest, InstallState,
    RequirementFinding,
};
use agora_core::content_store::{self, ContentSource};
use agora_core::ctx::Ctx;
use agora_core::download::sha256_hex;
use agora_core::error::{LauncherError, LauncherResult};
use agora_core::game_base::BaseMode;
use agora_core::game_deploy::{self, PlacementDecision};
use agora_core::game_discovery::{DiscoveredInstall, InstallCapabilities};
use agora_core::game_instance::{self, get_manifest, GameInstanceRecord};
use agora_core::game_registry::{
    GameRegistry, IdentifiedInstall, PackageSource, RuntimeResolution,
};
use agora_core::github_release::{GitHubRelease, GitHubReleaseAsset};
use agora_core::registry::{
    DownloadSource, FrameworkRequirement, GameCatalogItem, GameCompatibility, SourcePin,
};
use agora_game_api::{
    ContentLayout, DeploymentStrategy, FrameworkDefinition, FrameworkDetection, FrameworkId,
    FrameworkVersionSource, GameDefinition, GameId, GamePackage, GamePath, InstallId, InstallKind,
    LaunchRecipe, LaunchValue, LayerSource, PackageDefinition, RelPath, RuntimeIdentity, StoreId,
    StoreIdentifier,
};
use async_trait::async_trait;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fake network
// ---------------------------------------------------------------------------

/// Releases by page (page 1 first), and the bytes behind each download URL.
struct FakeTransport {
    pages: Vec<Vec<GitHubRelease>>,
    files: HashMap<String, Vec<u8>>,
    fetches: Mutex<Vec<String>>,
}

impl FakeTransport {
    fn new(pages: Vec<Vec<GitHubRelease>>, files: Vec<(String, Vec<u8>)>) -> Self {
        Self {
            pages,
            files: files.into_iter().collect(),
            fetches: Mutex::new(Vec::new()),
        }
    }

    /// A transport that serves no releases and no files: for direct downloads that must not run.
    fn silent() -> Self {
        Self::new(Vec::new(), Vec::new())
    }

    fn fetch_count(&self) -> usize {
        self.fetches.lock().unwrap().len()
    }
}

#[async_trait]
impl CatalogTransport for FakeTransport {
    async fn releases_page(
        &self,
        _repo: &str,
        page: u32,
    ) -> LauncherResult<(Vec<GitHubRelease>, u32)> {
        let total = self.pages.len().max(1) as u32;
        let releases = self
            .pages
            .get(page as usize - 1)
            .cloned()
            .unwrap_or_default();
        Ok((releases, total))
    }

    async fn fetch(&self, url: &str, _pinned_host: Option<&str>) -> LauncherResult<Vec<u8>> {
        self.fetches.lock().unwrap().push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| LauncherError::Generic {
                code: "TEST_NO_FILE".into(),
                message: format!("no fake file for {url}"),
            })
    }
}

const REPO: &str = "example-author/crash-logger";

fn asset_url(tag: &str, name: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/{tag}/{name}")
}

/// How a fake asset's digest is published.
#[derive(Clone, Copy)]
enum Digest {
    /// GitHub's digest for the bytes.
    Matching,
    /// No digest at all.
    Absent,
    /// A digest that does not describe these bytes.
    Other,
}

struct Asset<'a> {
    name: &'a str,
    bytes: &'a [u8],
    digest: Digest,
}

fn release(
    tag: &str,
    published_at: &str,
    draft: bool,
    prerelease: bool,
    assets: &[Asset],
) -> GitHubRelease {
    GitHubRelease {
        tag_name: tag.to_string(),
        published_at: Some(published_at.to_string()),
        draft,
        prerelease,
        assets: assets
            .iter()
            .map(|asset| GitHubReleaseAsset {
                name: asset.name.to_string(),
                browser_download_url: asset_url(tag, asset.name),
                size: Some(asset.bytes.len() as u64),
                digest: match asset.digest {
                    Digest::Matching => Some(format!("sha256:{}", sha256_hex(asset.bytes))),
                    Digest::Absent => None,
                    Digest::Other => Some(format!("sha256:{}", "e".repeat(64))),
                },
            })
            .collect(),
    }
}

/// The downloadable bytes for every asset of `releases`, keyed by URL.
fn files_of(releases: &[(&str, &[Asset])]) -> Vec<(String, Vec<u8>)> {
    releases
        .iter()
        .flat_map(|(tag, assets)| {
            assets
                .iter()
                .map(|asset| (asset_url(tag, asset.name), asset.bytes.to_vec()))
                .collect::<Vec<_>>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Archives and PE files
// ---------------------------------------------------------------------------

fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A minimal PE32 file whose version resource says `components`, for the framework version reader.
fn pe_with_version(components: [u16; 4]) -> Vec<u8> {
    let mut file = vec![0u8; 0x400];
    let put_u16 =
        |file: &mut Vec<u8>, at: usize, v: u16| file[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put_u32 =
        |file: &mut Vec<u8>, at: usize, v: u32| file[at..at + 4].copy_from_slice(&v.to_le_bytes());
    file[0..2].copy_from_slice(b"MZ");
    put_u32(&mut file, 0x3C, 0x40);
    file[0x40..0x44].copy_from_slice(b"PE\0\0");
    put_u16(&mut file, 0x46, 1);
    put_u16(&mut file, 0x44 + 16, 224);
    let optional = 0x58;
    put_u16(&mut file, optional, 0x010B);
    put_u32(&mut file, optional + 96 + 16, 0x1000);
    put_u32(&mut file, optional + 96 + 20, 0x200);
    let section = optional + 224;
    file[section..section + 6].copy_from_slice(b".rsrc\0");
    put_u32(&mut file, section + 8, 0x200);
    put_u32(&mut file, section + 12, 0x1000);
    put_u32(&mut file, section + 16, 0x200);
    put_u32(&mut file, section + 20, 0x200);
    let rsrc = 0x200;
    put_u16(&mut file, rsrc + 14, 1);
    put_u32(&mut file, rsrc + 16, 16);
    put_u32(&mut file, rsrc + 20, 0x8000_0000 | 0x18);
    put_u16(&mut file, rsrc + 0x18 + 14, 1);
    put_u32(&mut file, rsrc + 0x18 + 16, 1);
    put_u32(&mut file, rsrc + 0x18 + 20, 0x8000_0000 | 0x28);
    put_u16(&mut file, rsrc + 0x28 + 14, 1);
    put_u32(&mut file, rsrc + 0x28 + 16, 0x409);
    put_u32(&mut file, rsrc + 0x28 + 20, 0x48);
    put_u32(&mut file, rsrc + 0x48, 0x1060);
    put_u32(&mut file, rsrc + 0x48 + 4, 188);
    let fixed = rsrc + 0x60 + 136;
    put_u32(&mut file, fixed, 0xFEEF_04BD);
    put_u32(&mut file, fixed + 4, 0x0001_0000);
    put_u32(
        &mut file,
        fixed + 8,
        ((components[0] as u32) << 16) | components[1] as u32,
    );
    put_u32(
        &mut file,
        fixed + 12,
        ((components[2] as u32) << 16) | components[3] as u32,
    );
    file
}

// ---------------------------------------------------------------------------
// Game, package and instance
// ---------------------------------------------------------------------------

struct TestPackage(PackageDefinition);

impl GamePackage for TestPackage {
    fn definition(&self) -> &PackageDefinition {
        &self.0
    }
}

fn test_definition() -> GameDefinition {
    GameDefinition {
        id: GameId::new("test-game").unwrap(),
        name: "Test Game".into(),
        stores: vec![StoreIdentifier {
            store: StoreId::new("steam").unwrap(),
            product: "12345".into(),
        }],
        version_sources: vec![],
        deployment: DeploymentStrategy::VirtualFileSystem,
        content_rules: vec![],
        native_code_patterns: vec![],
        framework_ids: vec![FrameworkId::new("skse").unwrap()],
        tool_ids: vec![],
        launch: Some(LaunchRecipe {
            executable: GamePath::Runtime {
                path: RelPath::new("Game.exe").unwrap(),
            },
            arguments: vec![LaunchValue::Literal {
                value: "-test".into(),
            }],
            environment: Default::default(),
            working_directory: GamePath::Runtime {
                path: RelPath::default(),
            },
        }),
        log_paths: vec![],
        crash_paths: vec![],
        user_files: vec![],
        save_paths: vec![],
        linked_archive_patterns: vec![],
        declared_writes: vec![],
        excluded_paths: vec![],
        plugin_list: None,
        runtime_files: Vec::new(),
        save_location: Vec::new(),
        mo2_game_name: None,
        launch_alternatives: Vec::new(),
        content_layout: Some(ContentLayout {
            data_path: RelPath::new("Data").unwrap(),
            data_markers: vec!["*.esp".into(), "*.bsa".into()],
            root_markers: vec![],
            thunderstore_bepinex: false,
        }),
        copy_patterns: Vec::new(),
    }
}

fn skse(game: &GameId) -> FrameworkDefinition {
    FrameworkDefinition {
        id: FrameworkId::new("skse").unwrap(),
        game: game.clone(),
        name: "Script Extender".into(),
        version: "2.2.6".into(),
        supported_runtimes: vec![],
        required_frameworks: vec![],
        content: vec![],
        launch: None,
        detection: Some(FrameworkDetection {
            marker: RelPath::new("skse64_loader.exe").unwrap(),
            version: Some(FrameworkVersionSource::PeFileVersion),
        }),
    }
}

fn register(def: GameDefinition, frameworks: Vec<FrameworkDefinition>) -> Arc<GameRegistry> {
    let mut builder = GameRegistry::builder();
    let package = PackageDefinition {
        id: "test.catalog".into(),
        version: semver::Version::new(0, 1, 0),
        api_range: semver::VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        games: vec![def],
        frameworks,
        tools: vec![],
    };
    builder
        .add(
            PackageSource::Compiled {
                crate_name: "test".into(),
            },
            Arc::new(TestPackage(package)),
        )
        .expect("register the test package");
    Arc::new(builder.build())
}

/// A game install on disk, with a base file set and optionally a framework loader.
struct Fixture {
    _tmp: TempDir,
    ctx: Ctx,
    install_dir: PathBuf,
    definition: GameDefinition,
}

/// `loader` is the bytes of `skse64_loader.exe` in the install, or `None` for no framework.
fn fixture(loader: Option<Vec<u8>>, frameworks: bool) -> Fixture {
    let declared = if frameworks {
        vec![skse(&test_definition().id)]
    } else {
        vec![]
    };
    fixture_declaring(loader, declared)
}

/// [`fixture`], with the game declaring exactly `declared`.
fn fixture_declaring(loader: Option<Vec<u8>>, declared: Vec<FrameworkDefinition>) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let mut definition = test_definition();
    definition.framework_ids = declared.iter().map(|f| f.id.clone()).collect();
    let registry = register(definition.clone(), declared);
    let ctx = Ctx::for_testing(tmp.path().join("app_data")).with_games(registry);
    agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(install_dir.join("Data")).unwrap();
    std::fs::write(install_dir.join("Game.exe"), b"fake game binary").unwrap();
    std::fs::write(install_dir.join("Data").join("Skyrim.bsa"), b"BSA DATA").unwrap();
    if let Some(bytes) = loader {
        std::fs::write(install_dir.join("skse64_loader.exe"), bytes).unwrap();
    }
    Fixture {
        _tmp: tmp,
        ctx,
        install_dir,
        definition,
    }
}

impl Fixture {
    fn identified_install(&self) -> IdentifiedInstall {
        let store = StoreId::new("steam").unwrap();
        let runtime = RuntimeIdentity {
            game: GameId::new("test-game").unwrap(),
            store: store.clone(),
            version: "1.0.0".into(),
            build: None,
        };
        let detector = agora_core::game_discovery::volume::VolumeDetector::new();
        let discovered = DiscoveredInstall {
            store,
            product: "12345".into(),
            name: "Test Game".into(),
            kind: InstallKind::BaseGame,
            parent_product: None,
            location: self.install_dir.clone(),
            store_version: Some(runtime.version.clone()),
            store_build: None,
            executables: vec!["Game.exe".into()],
            capabilities: InstallCapabilities {
                executables_readable: true,
                accepts_new_files: true,
                relocatable: true,
            },
            volume: detector.get_volume_info(&self.install_dir),
        };
        IdentifiedInstall {
            game: runtime.game.clone(),
            install_id: InstallId::new("steam:12345").unwrap(),
            discovered,
            add_ons: vec![],
            runtime: RuntimeResolution::Identified {
                runtime,
                source: "executable".into(),
            },
        }
    }

    /// A new pinned instance of the test game, named `name`.
    fn instance(&self, name: &str) -> GameInstanceRecord {
        game_instance::create(
            &self.ctx,
            &self.identified_install(),
            &self.definition,
            name,
            None,
            BaseMode::Linked,
            &|_| {},
        )
        .expect("create instance")
    }

    fn content_items(&self) -> Vec<agora_core::content_store::ContentItem> {
        content_store::list_items(&self.ctx)
            .unwrap()
            .into_iter()
            .map(|entry| entry.into_item().expect("every stored item reads"))
            .collect()
    }

    fn layer_count(&self, instance_id: &str) -> usize {
        get_manifest(&self.ctx, instance_id)
            .unwrap()
            .layers
            .layers()
            .iter()
            .filter(|layer| matches!(layer.source, LayerSource::Content { .. }))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Catalog entries
// ---------------------------------------------------------------------------

fn github_source(pins: Vec<SourcePin>) -> DownloadSource {
    DownloadSource {
        strategy: "github_release".into(),
        identifier: REPO.into(),
        pins,
    }
}

fn compat(
    stores: &[&str],
    versions: &[&str],
    asset: Option<&str>,
    requires: Vec<FrameworkRequirement>,
) -> GameCompatibility {
    GameCompatibility {
        stores: stores.iter().map(|s| s.to_string()).collect(),
        game_versions: versions.iter().map(|v| v.to_string()).collect(),
        requires,
        asset: asset.map(str::to_string),
    }
}

fn skse_requirement(min: Option<&str>) -> Vec<FrameworkRequirement> {
    vec![FrameworkRequirement {
        framework: "skse".into(),
        min_version: min.map(str::to_string),
    }]
}

fn entry(
    id: &str,
    game: &str,
    sources: Vec<DownloadSource>,
    compats: Vec<GameCompatibility>,
) -> GameCatalogItem {
    GameCatalogItem {
        id: id.into(),
        game: game.into(),
        name: "CrashLogger".into(),
        author: Some("example-author".into()),
        content_type: "mod".into(),
        download_strategy: sources
            .first()
            .map(|s| s.strategy.clone())
            .unwrap_or_default(),
        source_identifier: REPO.into(),
        sha256: None,
        download_sources: sources,
        game_compatibility: compats,
        description: None,
        license_id: Some("MIT".into()),
        page_url: None,
        icon_url: None,
        status: "active".into(),
        date_added: None,
    }
}

/// The standard entry: GitHub, `steam`, `1.0.*`, the asset `CrashLogger-*.zip`.
fn standard_entry(pins: Vec<SourcePin>) -> GameCatalogItem {
    entry(
        "crash-logger",
        "test-game",
        vec![github_source(pins)],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    )
}

async fn run(
    fixture: &Fixture,
    transport: &dyn CatalogTransport,
    item: &GameCatalogItem,
    instance_id: &str,
    install_anyway: bool,
    dry_run: bool,
) -> LauncherResult<InstallReport> {
    install(
        &fixture.ctx,
        transport,
        InstallRequest {
            instance_id,
            item,
            install_anyway,
            allow_missing_frameworks: false,
            dry_run,
        },
        &mut |_: &CatalogPlan| {},
    )
    .await
}

/// [`run`] with `allow_missing_frameworks` set.
async fn run_past_frameworks(
    fixture: &Fixture,
    transport: &dyn CatalogTransport,
    item: &GameCatalogItem,
    instance_id: &str,
    install_anyway: bool,
    dry_run: bool,
) -> LauncherResult<InstallReport> {
    install(
        &fixture.ctx,
        transport,
        InstallRequest {
            instance_id,
            item,
            install_anyway,
            allow_missing_frameworks: true,
            dry_run,
        },
        &mut |_: &CatalogPlan| {},
    )
    .await
}

fn plain_archive() -> Vec<u8> {
    zip_bytes(&[
        ("CrashLogger.esp", b"plugin"),
        ("CrashLogger.bsa", b"archive"),
    ])
}

// ---------------------------------------------------------------------------
// Compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_store_mismatch_refuses_and_says_what_the_entry_supports() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("store");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["gog"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    );
    let transport = FakeTransport::silent();
    let error = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_NO_COMPATIBLE_ENTRY");
    let message = error.to_string();
    assert!(
        message.contains("1.0.*") && message.contains("gog"),
        "{message}"
    );
    assert!(message.contains("steam"), "{message}");
}

#[tokio::test]
async fn a_version_mismatch_refuses_and_says_what_the_entry_supports() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("version");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["2.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    );
    let transport = FakeTransport::silent();
    let message = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("2.*") && message.contains("1.0.0"),
        "{message}"
    );
}

#[tokio::test]
async fn a_glob_version_matches_and_the_first_of_two_matching_entries_is_chosen() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("first");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![
            compat(&["steam"], &["1.0.*"], Some("CrashLogger-A-*.zip"), vec![]),
            compat(&["steam"], &["1.0.0"], Some("CrashLogger-B-*.zip"), vec![]),
        ],
    );
    let a = b"a".as_slice();
    let releases = vec![release(
        "v1",
        "2026-01-01T00:00:00Z",
        false,
        false,
        &[
            Asset {
                name: "CrashLogger-A-1.zip",
                bytes: a,
                digest: Digest::Matching,
            },
            Asset {
                name: "CrashLogger-B-1.zip",
                bytes: b"b",
                digest: Digest::Matching,
            },
        ],
    )];
    let transport = FakeTransport::new(
        vec![releases],
        files_of(&[(
            "v1",
            &[
                Asset {
                    name: "CrashLogger-A-1.zip",
                    bytes: a,
                    digest: Digest::Matching,
                },
                Asset {
                    name: "CrashLogger-B-1.zip",
                    bytes: b"b",
                    digest: Digest::Matching,
                },
            ],
        )]),
    );
    let report = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap();
    assert_eq!(report.plan.compatibility_index, 0);
    assert_eq!(report.plan.source.file_name(), "CrashLogger-A-1.zip");
}

#[tokio::test]
async fn the_asset_pattern_is_case_insensitive() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("case");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("crashlogger-*.ZIP"),
            vec![],
        )],
    );
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap();
    assert_eq!(report.plan.source.file_name(), "CrashLogger-1.zip");
}

// ---------------------------------------------------------------------------
// Frameworks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_framework_is_refused_and_names_what_to_install() {
    let f = fixture(None, true);
    let instance = f.instance("missing");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            skse_requirement(Some("2.2.6")),
        )],
    );
    let transport = FakeTransport::silent();
    let message = run(&f, &transport, &item, &instance.instance_id, true, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("Script Extender"), "{message}");
    assert!(message.contains("2.2.6"), "{message}");
    assert_eq!(transport.fetch_count(), 0);
}

#[tokio::test]
async fn a_framework_below_the_minimum_is_refused_naming_both_versions_even_with_install_anyway() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 5])), true);
    let instance = f.instance("old");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            skse_requirement(Some("2.2.6")),
        )],
    );
    let transport = FakeTransport::silent();
    for anyway in [false, true] {
        let error = run(&f, &transport, &item, &instance.instance_id, anyway, false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), "ERR_CATALOG_FRAMEWORK_TOO_OLD");
        let message = error.to_string();
        assert!(
            message.contains("2.2.6") && message.contains("2.2.5"),
            "{message}"
        );
    }
    assert_eq!(transport.fetch_count(), 0);
}

#[tokio::test]
async fn a_framework_equal_or_newer_than_the_minimum_passes_and_reports_its_version() {
    for (components, expected) in [([0, 2, 2, 6], "2.2.6"), ([0, 2, 3, 0], "2.3.0")] {
        let f = fixture(Some(pe_with_version(components)), true);
        let instance = f.instance(expected);
        let item = entry(
            "crash-logger",
            "test-game",
            vec![github_source(vec![])],
            vec![compat(
                &["steam"],
                &["1.0.*"],
                Some("CrashLogger-*.zip"),
                skse_requirement(Some("2.2.6")),
            )],
        );
        let bytes = plain_archive();
        let assets = [Asset {
            name: "CrashLogger-1.zip",
            bytes: &bytes,
            digest: Digest::Matching,
        }];
        let transport = FakeTransport::new(
            vec![vec![release(
                "v1",
                "2026-01-01T00:00:00Z",
                false,
                false,
                &assets,
            )]],
            files_of(&[("v1", &assets)]),
        );
        let report = run(&f, &transport, &item, &instance.instance_id, false, true)
            .await
            .unwrap();
        assert_eq!(
            report.plan.frameworks[0].found_version.as_deref(),
            Some(expected)
        );
        assert!(report.plan.warnings.is_empty());
    }
}

#[tokio::test]
async fn an_unreadable_framework_version_warns_and_proceeds() {
    let f = fixture(Some(b"not a pe file".to_vec()), true);
    let instance = f.instance("unreadable");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            skse_requirement(Some("2.2.6")),
        )],
    );
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap();
    assert_eq!(report.plan.frameworks[0].found_version, None);
    assert_eq!(report.plan.warnings.len(), 1, "{:?}", report.plan.warnings);
    assert!(report.plan.warnings[0].contains("Script Extender"));
}

// ---------------------------------------------------------------------------
// Installing past an unmet framework
// ---------------------------------------------------------------------------

/// A declared framework with no way to detect it.
fn undetectable_skse() -> FrameworkDefinition {
    FrameworkDefinition {
        detection: None,
        ..skse(&test_definition().id)
    }
}

/// A second framework the game declares, found by its own marker.
fn second_framework() -> FrameworkDefinition {
    FrameworkDefinition {
        id: FrameworkId::new("address-library").unwrap(),
        name: "Address Library".into(),
        detection: Some(FrameworkDetection {
            marker: RelPath::new("Data/versionlib.bin").unwrap(),
            version: None,
        }),
        ..skse(&test_definition().id)
    }
}

/// Each way a requirement goes unmet: the fixture, the core's refusal code, and what the plan says.
fn unmet_cases() -> Vec<(&'static str, Fixture, &'static str, FrameworkProblem)> {
    vec![
        (
            "absent",
            fixture(None, true),
            "ERR_CATALOG_FRAMEWORK_MISSING",
            FrameworkProblem::Absent,
        ),
        (
            "too-old",
            fixture(Some(pe_with_version([0, 2, 2, 5])), true),
            "ERR_CATALOG_FRAMEWORK_TOO_OLD",
            FrameworkProblem::TooOld {
                found: "2.2.5".into(),
            },
        ),
        (
            "undetectable",
            fixture_declaring(None, vec![undetectable_skse()]),
            "ERR_CATALOG_FRAMEWORK_UNDETECTABLE",
            FrameworkProblem::Undetectable,
        ),
        (
            "undeclared",
            fixture(None, false),
            "ERR_CATALOG_UNKNOWN_FRAMEWORK",
            FrameworkProblem::Undeclared { malformed: None },
        ),
    ]
}

fn item_requiring(requires: Vec<FrameworkRequirement>) -> GameCatalogItem {
    entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            requires,
        )],
    )
}

/// One release of the plain archive, with a digest as given.
fn one_release(bytes: &[u8], digest: Digest) -> FakeTransport {
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes,
        digest,
    }];
    FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    )
}

#[tokio::test]
async fn every_kind_of_unmet_framework_refuses_without_the_override() {
    for (kind, f, code, _) in unmet_cases() {
        let instance = f.instance(kind);
        let item = item_requiring(skse_requirement(Some("2.2.6")));
        // A file is on offer, so nothing but the framework can be what refused it.
        let bytes = plain_archive();
        let transport = one_release(&bytes, Digest::Matching);
        // The hash override is not a framework override, on or off.
        for anyway in [false, true] {
            let error = run(&f, &transport, &item, &instance.instance_id, anyway, false)
                .await
                .expect_err(kind);
            assert_eq!(error.code(), code, "{kind}");
            assert!(is_framework_refusal(&error.code()), "{kind}");
        }
        assert_eq!(
            transport.fetch_count(),
            0,
            "{kind}: refused before any fetch"
        );
        assert!(f.content_items().is_empty(), "{kind}: nothing stored");
        assert_eq!(f.layer_count(&instance.instance_id), 0, "{kind}");
    }
}

#[tokio::test]
async fn every_kind_of_unmet_framework_installs_with_the_override_and_is_named() {
    for (kind, f, _, problem) in unmet_cases() {
        let instance = f.instance(kind);
        let item = item_requiring(skse_requirement(Some("2.2.6")));
        let bytes = plain_archive();
        let transport = one_release(&bytes, Digest::Matching);

        let report =
            run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, false)
                .await
                .expect(kind);

        assert!(
            matches!(report.state, InstallState::Placed { .. }),
            "{kind}: {:?}",
            report.state
        );
        let unmet = &report.plan.unmet_frameworks;
        assert_eq!(unmet.len(), 1, "{kind}");
        assert_eq!(unmet[0].problem, problem, "{kind}");
        assert_eq!(unmet[0].framework, "skse", "{kind}");
        assert_eq!(unmet[0].min_version.as_deref(), Some("2.2.6"), "{kind}");
        assert!(
            report.plan.frameworks.is_empty(),
            "{kind}: not listed as met"
        );
        assert_eq!(f.layer_count(&instance.instance_id), 1, "{kind}");
    }
}

#[tokio::test]
async fn the_warning_says_what_is_needed_and_why_for_each_kind() {
    let say = |f: &Fixture| f.instance("wording");
    let expected = [
        "This mod needs Script Extender 2.2.6 or newer, and this instance doesn't have it. It probably won't work until you add it.",
        "This mod needs Script Extender 2.2.6 or newer, and this instance has 2.2.5. It probably won't work until you update it.",
        "This mod needs Script Extender 2.2.6 or newer, and Agora can't tell whether this instance has it. It probably won't work unless you have it.",
        "This mod needs skse 2.2.6 or newer, which this game doesn't list, so Agora can't check for it. It probably won't work until you add it.",
    ];
    for ((kind, f, _, _), expected) in unmet_cases().into_iter().zip(expected) {
        let instance = say(&f);
        let item = item_requiring(skse_requirement(Some("2.2.6")));
        let bytes = plain_archive();
        let transport = one_release(&bytes, Digest::Matching);
        let report = run(&f, &transport, &item, &instance.instance_id, false, true)
            .await
            .expect(kind);
        assert_eq!(report.plan.unmet_frameworks[0].message, expected, "{kind}");
    }
}

#[tokio::test]
async fn a_malformed_framework_id_refuses_and_the_override_passes_it_as_undeclared() {
    let f = fixture(None, true);
    let instance = f.instance("malformed");
    let item = item_requiring(vec![FrameworkRequirement {
        framework: "Not A Valid Id!".into(),
        min_version: None,
    }]);
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    let error = run(&f, &transport, &item, &instance.instance_id, true, false)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_BAD_REQUIREMENT");
    assert!(is_framework_refusal(&error.code()));

    let report = run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap();
    assert!(matches!(
        report.plan.unmet_frameworks[0].problem,
        FrameworkProblem::Undeclared { malformed: Some(_) }
    ));
}

#[tokio::test]
async fn two_unmet_frameworks_are_both_named_refused_or_planned() {
    let f = fixture_declaring(
        Some(pe_with_version([0, 2, 2, 5])),
        vec![skse(&test_definition().id), second_framework()],
    );
    let instance = f.instance("two");
    let item = item_requiring(vec![
        FrameworkRequirement {
            framework: "skse".into(),
            min_version: Some("2.2.6".into()),
        },
        FrameworkRequirement {
            framework: "address-library".into(),
            min_version: None,
        },
    ]);
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);

    let refusal = run(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("Script Extender"), "{refusal}");
    assert!(refusal.contains("2.2.5"), "{refusal}");
    assert!(refusal.contains("Address Library"), "{refusal}");

    for dry_run in [true, false] {
        let report =
            run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, dry_run)
                .await
                .unwrap();
        let names: Vec<&str> = report
            .plan
            .unmet_frameworks
            .iter()
            .map(|unmet| unmet.name.as_str())
            .collect();
        assert_eq!(names, ["Script Extender", "Address Library"]);
    }
}

#[tokio::test]
async fn a_met_requirement_beside_an_unmet_one_stays_listed_as_met() {
    let f = fixture_declaring(
        Some(pe_with_version([0, 2, 2, 6])),
        vec![skse(&test_definition().id), second_framework()],
    );
    let instance = f.instance("mixed");
    let item = item_requiring(vec![
        FrameworkRequirement {
            framework: "skse".into(),
            min_version: Some("2.2.6".into()),
        },
        FrameworkRequirement {
            framework: "address-library".into(),
            min_version: None,
        },
    ]);
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    let report = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap();
    assert_eq!(report.plan.frameworks.len(), 1);
    assert_eq!(report.plan.frameworks[0].name, "Script Extender");
    assert_eq!(report.plan.unmet_frameworks.len(), 1);
    assert_eq!(report.plan.unmet_frameworks[0].name, "Address Library");
}

#[tokio::test]
async fn the_framework_override_does_not_get_past_a_differing_published_hash() {
    let f = fixture(None, true);
    let instance = f.instance("published");
    let item = item_requiring(skse_requirement(None));
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Other);
    // Not with the hash override either: a published hash that differs has no override at all.
    for anyway in [false, true] {
        let error =
            run_past_frameworks(&f, &transport, &item, &instance.instance_id, anyway, false)
                .await
                .expect_err("a differing published hash always stops");
        assert!(
            !is_framework_refusal(&error.code()),
            "the stop is the hash's, not the framework's: {}",
            error.code()
        );
        assert_ne!(error.code(), "ERR_HASH_CONFIRMATION_REQUIRED");
    }
    assert!(f.content_items().is_empty());
    assert_eq!(f.layer_count(&instance.instance_id), 0);
}

#[tokio::test]
async fn the_framework_override_does_not_get_past_a_differing_curator_pin() {
    let f = fixture(None, true);
    let instance = f.instance("pin");
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![SourcePin {
            tag: "v1".into(),
            asset: "CrashLogger-1.zip".into(),
            sha256: "f".repeat(64),
        }])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            skse_requirement(None),
        )],
    );
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Absent);
    let error = run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_HASH_CONFIRMATION_REQUIRED");
    assert_eq!(f.layer_count(&instance.instance_id), 0);

    // Both overrides, each answering its own stop.
    let report = run_past_frameworks(&f, &transport, &item, &instance.instance_id, true, false)
        .await
        .unwrap();
    assert!(matches!(report.state, InstallState::Placed { .. }));
    assert_eq!(report.plan.unmet_frameworks.len(), 1);
}

#[tokio::test]
async fn a_dry_run_names_what_is_unmet_and_writes_nothing() {
    for (kind, f, _, problem) in unmet_cases() {
        let instance = f.instance(kind);
        let item = item_requiring(skse_requirement(Some("2.2.6")));
        let bytes = plain_archive();
        let transport = one_release(&bytes, Digest::Matching);
        let plain = run(&f, &transport, &item, &instance.instance_id, false, true)
            .await
            .expect(kind);
        let past = run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, true)
            .await
            .expect(kind);
        for report in [plain, past] {
            assert_eq!(report.state, InstallState::DryRun, "{kind}");
            assert_eq!(report.plan.unmet_frameworks[0].problem, problem, "{kind}");
        }
        assert_eq!(transport.fetch_count(), 0, "{kind}");
        assert!(f.content_items().is_empty(), "{kind}");
        assert_eq!(f.layer_count(&instance.instance_id), 0, "{kind}");
        assert!(
            !f.ctx
                .paths
                .content_root()
                .join("catalog-downloads")
                .exists(),
            "{kind}: not even a scratch folder"
        );
    }
}

#[tokio::test]
async fn a_dry_run_with_another_obstacle_reports_that_one_so_no_override_is_offered() {
    let f = fixture(None, true);
    let instance = f.instance("no-asset");
    let item = item_requiring(skse_requirement(None));
    // No release has a file: installing anyway could not succeed either.
    let transport = FakeTransport::silent();
    let error = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_NO_ASSET");
}

#[tokio::test]
async fn the_refusal_comes_before_the_network_without_the_override() {
    let f = fixture(None, true);
    let instance = f.instance("order");
    let item = item_requiring(skse_requirement(None));
    // The same silent transport as above: the framework is what a real install reports.
    let transport = FakeTransport::silent();
    let error = run(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_FRAMEWORK_MISSING");
}

#[tokio::test]
async fn a_mod_already_installed_past_a_framework_still_names_it_and_changes_nothing() {
    let f = fixture(None, true);
    let instance = f.instance("again");
    let item = item_requiring(skse_requirement(None));
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap();

    let preview = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap();
    assert!(matches!(
        preview.state,
        InstallState::AlreadyInstalled { .. }
    ));
    assert_eq!(preview.plan.unmet_frameworks.len(), 1);
    assert_eq!(f.layer_count(&instance.instance_id), 1);
}

// ---------------------------------------------------------------------------
// The catalog entry that provides a framework
// ---------------------------------------------------------------------------

/// A registry on disk holding exactly `items`.
fn seed_registry(ctx: &Ctx, items: &[GameCatalogItem]) {
    let conn = rusqlite::Connection::open(ctx.paths.registry_db()).unwrap();
    conn.execute_batch(
        "CREATE TABLE game_catalog_items (
            id TEXT PRIMARY KEY,
            game TEXT NOT NULL,
            name TEXT NOT NULL,
            author TEXT,
            content_type TEXT NOT NULL,
            download_strategy TEXT NOT NULL,
            source_identifier TEXT NOT NULL,
            sha256 TEXT,
            download_sources_json TEXT NOT NULL DEFAULT '[]',
            game_compatibility_json TEXT NOT NULL DEFAULT '[]',
            description TEXT,
            license_id TEXT,
            page_url TEXT,
            icon_url TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            date_added TEXT
        );",
    )
    .unwrap();
    for item in items {
        conn.execute(
            "INSERT INTO game_catalog_items
                (id, game, name, content_type, download_strategy, source_identifier,
                 game_compatibility_json, status)
             VALUES (?1, ?2, ?3, 'mod', 'github_release', 'src', ?4, 'active')",
            rusqlite::params![
                item.id,
                item.game,
                item.name,
                serde_json::to_string(&item.game_compatibility).unwrap()
            ],
        )
        .unwrap();
    }
}

fn named(id: &str, name: &str) -> GameCatalogItem {
    GameCatalogItem {
        name: name.into(),
        ..entry(id, "test-game", vec![github_source(vec![])], vec![])
    }
}

async fn catalog_link_with(items: &[GameCatalogItem]) -> Option<CatalogEntryRef> {
    let f = fixture(None, true);
    seed_registry(&f.ctx, items);
    let instance = f.instance("link");
    let item = item_requiring(skse_requirement(None));
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap()
        .plan
        .unmet_frameworks[0]
        .catalog_entry
        .clone()
}

#[tokio::test]
async fn an_unmet_framework_links_to_the_catalog_entry_that_is_that_framework() {
    for items in [
        vec![named("skse", "SKSE")],
        vec![named("skse64", "SKSE64")],
        vec![named("some-id", "Script Extender")],
    ] {
        let link = catalog_link_with(&items).await.expect("a link");
        assert_eq!(link.id, items[0].id);
        assert_eq!(link.name, items[0].name);
    }
}

#[tokio::test]
async fn no_link_is_guessed_from_a_lookalike_or_a_choice_of_two() {
    assert_eq!(catalog_link_with(&[]).await, None);
    // A mod that is about the framework is not the framework.
    assert_eq!(
        catalog_link_with(&[named("skse-plugin-preloader", "Preloader")]).await,
        None
    );
    // Another game's entry cannot be installed here.
    let other_game = GameCatalogItem {
        game: "other-game".into(),
        ..named("skse", "SKSE")
    };
    assert_eq!(catalog_link_with(&[other_game]).await, None);
    // Two candidates: no guess.
    assert_eq!(
        catalog_link_with(&[named("skse", "SKSE"), named("skse64", "SKSE64")]).await,
        None
    );
    // The mod itself never answers its own requirement.
    assert_eq!(
        catalog_link_with(&[named("crash-logger", "Script Extender")]).await,
        None
    );
}

// ---------------------------------------------------------------------------
// The launch check, afterwards
// ---------------------------------------------------------------------------

/// Install `item` past its frameworks into a new instance, with the catalog listing it.
async fn installed_past_frameworks(f: &Fixture, item: &GameCatalogItem) -> GameInstanceRecord {
    let instance = f.instance("after");
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    let report = run_past_frameworks(f, &transport, item, &instance.instance_id, false, false)
        .await
        .unwrap();
    assert!(matches!(report.state, InstallState::Placed { .. }));
    instance
}

#[tokio::test]
async fn the_launch_check_still_reports_what_was_installed_past() {
    for (kind, f, _, problem) in unmet_cases() {
        let item = item_requiring(skse_requirement(Some("2.2.6")));
        seed_registry(&f.ctx, std::slice::from_ref(&item));
        let instance = installed_past_frameworks(&f, &item).await;

        let checks = game_instance::launch_checks(&f.ctx, &instance, &f.definition).expect(kind);

        assert_eq!(checks.requirements.len(), 1, "{kind}");
        let RequirementFinding::Unmet { needed_by, unmet } = &checks.requirements[0] else {
            panic!("{kind}: {:?}", checks.requirements[0]);
        };
        assert_eq!(unmet.problem, problem, "{kind}");
        assert_eq!(needed_by, &["CrashLogger"], "{kind}");
        let summary = checks.requirements[0].summary();
        assert!(summary.starts_with("CrashLogger needs "), "{summary}");
        assert!(summary.contains("2.2.6"), "{summary}");
    }
}

#[tokio::test]
async fn the_launch_check_follows_a_mod_through_its_fomod_installer() {
    let f = fixture(None, true);
    let item = item_requiring(skse_requirement(None));
    seed_registry(&f.ctx, std::slice::from_ref(&item));
    let instance = f.instance("fomod-after");
    let bytes = zip_bytes(&[
        (
            "fomod/ModuleConfig.xml",
            br#"<config><moduleName>Crash Logger</moduleName><requiredInstallFiles><file source="Data/CrashLogger.esp" destination="CrashLogger.esp"/></requiredInstallFiles></config>"#,
        ),
        ("Data/CrashLogger.esp", b"plugin"),
    ]);
    let transport = one_release(&bytes, Digest::Matching);
    let report = run_past_frameworks(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap();
    let InstallState::NeedsInstaller {
        content_item_id, ..
    } = &report.state
    else {
        panic!("expected an installer, got {:?}", report.state);
    };
    assert_eq!(
        report.plan.unmet_frameworks.len(),
        1,
        "the installer outcome names it too"
    );

    let (_, _, outcome) =
        agora_core::content_fomod::install(&f.ctx, content_item_id, &[], None).unwrap();
    game_deploy::add_content(
        &f.ctx,
        &instance.instance_id,
        &outcome.item().item_id,
        Some("Data"),
        None,
    )
    .unwrap();

    let findings = requirement_findings(&f.ctx, &instance.instance_id, &f.definition);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(matches!(&findings[0], RequirementFinding::Unmet { .. }));
}

#[tokio::test]
async fn the_launch_check_is_quiet_when_the_framework_is_there() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let item = item_requiring(skse_requirement(Some("2.2.6")));
    seed_registry(&f.ctx, std::slice::from_ref(&item));
    let instance = f.instance("met");
    let bytes = plain_archive();
    let transport = one_release(&bytes, Digest::Matching);
    run(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap();
    let checks = game_instance::launch_checks(&f.ctx, &instance, &f.definition).unwrap();
    assert_eq!(checks.requirements, vec![]);
}

#[tokio::test]
async fn the_launch_check_does_not_blame_a_mod_that_is_turned_off() {
    let f = fixture(None, true);
    let item = item_requiring(skse_requirement(None));
    seed_registry(&f.ctx, std::slice::from_ref(&item));
    let instance = installed_past_frameworks(&f, &item).await;

    let stored = f.content_items().remove(0).item_id;
    game_deploy::set_content_enabled(&f.ctx, &instance.instance_id, &stored, false).unwrap();

    assert_eq!(
        requirement_findings(&f.ctx, &instance.instance_id, &f.definition),
        vec![]
    );
}

#[tokio::test]
async fn two_mods_needing_one_framework_are_one_finding_naming_both() {
    let f = fixture(None, true);
    let first = item_requiring(skse_requirement(None));
    let second = GameCatalogItem {
        id: "second-mod".into(),
        name: "Second Mod".into(),
        ..first.clone()
    };
    seed_registry(&f.ctx, &[first.clone(), second.clone()]);
    let instance = f.instance("both");
    for item in [&first, &second] {
        let bytes = if item.id == first.id {
            plain_archive()
        } else {
            zip_bytes(&[("Second.esp", b"another plugin")])
        };
        let transport = one_release(&bytes, Digest::Matching);
        run_past_frameworks(&f, &transport, item, &instance.instance_id, false, false)
            .await
            .unwrap();
    }

    let findings = requirement_findings(&f.ctx, &instance.instance_id, &f.definition);
    assert_eq!(findings.len(), 1, "{findings:?}");
    let RequirementFinding::Unmet { needed_by, .. } = &findings[0] else {
        panic!("{findings:?}");
    };
    assert_eq!(needed_by, &["CrashLogger", "Second Mod"]);
    assert!(findings[0]
        .summary()
        .starts_with("CrashLogger, Second Mod need "));
}

#[tokio::test]
async fn an_unreadable_catalog_says_it_could_not_check_rather_than_all_clear() {
    let f = fixture(None, true);
    let item = item_requiring(skse_requirement(None));
    // No registry on disk at all.
    let instance = installed_past_frameworks(&f, &item).await;
    let findings = requirement_findings(&f.ctx, &instance.instance_id, &f.definition);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(
        matches!(&findings[0], RequirementFinding::NotChecked { .. }),
        "{findings:?}"
    );
    assert!(findings[0].summary().contains("could not check"));
}

#[tokio::test]
async fn an_instance_with_no_catalog_mods_has_no_requirements_to_check() {
    let f = fixture(None, true);
    // No registry either: nothing about this instance needs one.
    let instance = f.instance("plain");
    assert_eq!(
        requirement_findings(&f.ctx, &instance.instance_id, &f.definition),
        vec![]
    );
}

// ---------------------------------------------------------------------------
// GitHub releases
// ---------------------------------------------------------------------------

fn github_fixture_item() -> GameCatalogItem {
    standard_entry(vec![])
}

#[tokio::test]
async fn the_newest_matching_asset_wins() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("newest");
    let older = b"older".as_slice();
    let newer = b"newer".as_slice();
    let old_assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: older,
        digest: Digest::Matching,
    }];
    let new_assets = [Asset {
        name: "CrashLogger-2.zip",
        bytes: newer,
        digest: Digest::Matching,
    }];
    // Listed oldest first on purpose: the order of the listing must not decide.
    let transport = FakeTransport::new(
        vec![vec![
            release("v1", "2026-01-01T00:00:00Z", false, false, &old_assets),
            release("v2", "2026-03-01T00:00:00Z", false, false, &new_assets),
        ]],
        files_of(&[("v1", &old_assets), ("v2", &new_assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.plan.source.release(), Some("v2"));
    assert_eq!(report.plan.source.file_name(), "CrashLogger-2.zip");
}

#[tokio::test]
async fn drafts_and_prereleases_are_skipped() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("drafts");
    let stable = b"stable".as_slice();
    let draft = b"draft".as_slice();
    let pre = b"pre".as_slice();
    let stable_assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: stable,
        digest: Digest::Matching,
    }];
    let draft_assets = [Asset {
        name: "CrashLogger-3.zip",
        bytes: draft,
        digest: Digest::Matching,
    }];
    let pre_assets = [Asset {
        name: "CrashLogger-2.zip",
        bytes: pre,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![
            release("v3", "2026-05-01T00:00:00Z", true, false, &draft_assets),
            release("v2", "2026-04-01T00:00:00Z", false, true, &pre_assets),
            release("v1", "2026-01-01T00:00:00Z", false, false, &stable_assets),
        ]],
        files_of(&[
            ("v1", &stable_assets),
            ("v2", &pre_assets),
            ("v3", &draft_assets),
        ]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.plan.source.release(), Some("v1"));
}

#[tokio::test]
async fn no_matching_asset_is_a_clear_error() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("nomatch");
    let other = b"other".as_slice();
    let assets = [Asset {
        name: "Something-Else.zip",
        bytes: other,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        vec![],
    );
    let error = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        true,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_NO_ASSET");
    assert!(error.to_string().contains("CrashLogger-*.zip"), "{error}");
}

#[tokio::test]
async fn a_match_on_a_later_page_is_found() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("pages");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![
            vec![release(
                "v2",
                "2026-02-01T00:00:00Z",
                false,
                false,
                &[Asset {
                    name: "Other.zip",
                    bytes: b"x",
                    digest: Digest::Matching,
                }],
            )],
            vec![release("v1", "2026-01-01T00:00:00Z", false, false, &assets)],
        ],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.plan.source.release(), Some("v1"));
}

// ---------------------------------------------------------------------------
// Hashes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_published_digest_that_matches_verifies_and_is_recorded_as_verified() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("digest-match");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(hash.verified);
    assert!(hash.basis.contains("GitHub published"), "{}", hash.basis);
    let recorded = f.content_items();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0]
        .sources
        .iter()
        .any(|s| matches!(s, ContentSource::Catalog { verified: true, .. })));
}

#[tokio::test]
async fn a_published_digest_that_differs_is_refused_even_with_install_anyway() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("digest-mismatch");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Other,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    for anyway in [false, true] {
        let error = run(
            &f,
            &transport,
            &github_fixture_item(),
            &instance.instance_id,
            anyway,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "ERR_HASH_MISMATCH", "install_anyway={anyway}");
    }
    assert_eq!(f.layer_count(&instance.instance_id), 0);
    assert!(
        f.content_items().is_empty(),
        "a refused download is not stored"
    );
}

#[tokio::test]
async fn no_published_digest_installs_as_unverified_records_the_hash_and_says_so() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("no-digest");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Absent,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(!hash.verified);
    assert!(
        hash.basis
            .contains("GitHub published no checksum for CrashLogger-1.zip"),
        "{}",
        hash.basis
    );
    assert_eq!(hash.sha256, sha256_hex(&bytes));
    let recorded = f.content_items();
    assert!(recorded[0].sources.iter().any(|s| matches!(
        s,
        ContentSource::Catalog { verified: false, sha256, release: Some(r), .. }
            if sha256 == &sha256_hex(&bytes) && r == "v1"
    )));
}

#[tokio::test]
async fn a_second_install_whose_bytes_differ_from_the_remembered_hash_stops_until_confirmed() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("first");
    let second = f.instance("second");
    let bytes_one = plain_archive();
    let bytes_two = zip_bytes(&[("CrashLogger.esp", b"a changed plugin")]);
    let assets_one = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes_one,
        digest: Digest::Absent,
    }];
    let assets_two = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes_two,
        digest: Digest::Absent,
    }];
    let transport_one = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets_one,
        )]],
        files_of(&[("v1", &assets_one)]),
    );
    run(
        &f,
        &transport_one,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();

    let transport_two = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets_two,
        )]],
        files_of(&[("v1", &assets_two)]),
    );
    let error = run(
        &f,
        &transport_two,
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "ERR_HASH_CONFIRMATION_REQUIRED");
    assert_eq!(f.layer_count(&second.instance_id), 0);

    let report = run(
        &f,
        &transport_two,
        &github_fixture_item(),
        &second.instance_id,
        true,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert_eq!(hash.confirmed_past, Some(HashOrigin::PreviousInstall));
    assert_eq!(f.layer_count(&second.instance_id), 1);
}

#[tokio::test]
async fn a_curator_pin_that_matches_passes_as_unverified() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("pin-match");
    let bytes = plain_archive();
    let pin = SourcePin {
        tag: "v1".into(),
        asset: "CrashLogger-1.zip".into(),
        sha256: sha256_hex(&bytes),
    };
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![pin])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    );
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Absent,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(!hash.verified);
    assert!(hash.basis.contains("curator's pin"), "{}", hash.basis);
}

#[tokio::test]
async fn a_curator_pin_that_differs_stops_until_confirmed() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("pin-differs");
    let bytes = plain_archive();
    let pin = SourcePin {
        tag: "v1".into(),
        asset: "CrashLogger-1.zip".into(),
        sha256: "f".repeat(64),
    };
    let item = entry(
        "crash-logger",
        "test-game",
        vec![github_source(vec![pin])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    );
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Absent,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let error = run(&f, &transport, &item, &instance.instance_id, false, false)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_HASH_CONFIRMATION_REQUIRED");
    let report = run(&f, &transport, &item, &instance.instance_id, true, false)
        .await
        .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert_eq!(hash.confirmed_past, Some(HashOrigin::CuratorPin));
}

#[tokio::test]
async fn a_direct_hash_that_matches_verifies_and_one_that_differs_is_refused_even_with_install_anyway(
) {
    let bytes = plain_archive();
    let url = "https://files.example-author.example/crash-logger/CrashLogger-1.zip".to_string();
    let direct = |sha: String| {
        let mut item = entry(
            "crash-logger",
            "test-game",
            vec![DownloadSource {
                strategy: "direct_hash".into(),
                identifier: url.clone(),
                pins: vec![],
            }],
            vec![compat(&["steam"], &["1.0.*"], None, vec![])],
        );
        item.sha256 = Some(sha);
        item
    };

    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("direct-ok");
    let transport = FakeTransport::new(vec![], vec![(url.clone(), bytes.clone())]);
    let report = run(
        &f,
        &transport,
        &direct(sha256_hex(&bytes)),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(hash.verified);
    assert!(hash.basis.contains("curator's manifest"), "{}", hash.basis);

    let other = f.instance("direct-bad");
    let error = run(
        &f,
        &transport,
        &direct("d".repeat(64)),
        &other.instance_id,
        true,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "ERR_HASH_MISMATCH");
}

// ---------------------------------------------------------------------------
// The content store's own copy
// ---------------------------------------------------------------------------

/// Serves releases like the fake, and fails the test if any file is downloaded.
struct NoDownloads(FakeTransport);

#[async_trait]
impl CatalogTransport for NoDownloads {
    async fn releases_page(
        &self,
        repo: &str,
        page: u32,
    ) -> LauncherResult<(Vec<GitHubRelease>, u32)> {
        self.0.releases_page(repo, page).await
    }

    async fn fetch(&self, url: &str, _pinned_host: Option<&str>) -> LauncherResult<Vec<u8>> {
        panic!("{url} was downloaded, but the content store already holds this release file");
    }
}

fn direct_item(url: &str, sha256: &str) -> GameCatalogItem {
    let mut item = entry(
        "crash-logger",
        "test-game",
        vec![DownloadSource {
            strategy: "direct_hash".into(),
            identifier: url.to_string(),
            pins: vec![],
        }],
        vec![compat(&["steam"], &["1.0.*"], None, vec![])],
    );
    item.sha256 = Some(sha256.to_string());
    item
}

#[tokio::test]
async fn a_stored_copy_under_a_published_digest_is_used_without_a_download() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("stored-first");
    let second = f.instance("stored-second");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let releases = vec![vec![release(
        "v1",
        "2026-01-01T00:00:00Z",
        false,
        false,
        &assets,
    )]];
    let downloads = FakeTransport::new(releases.clone(), files_of(&[("v1", &assets)]));
    run(
        &f,
        &downloads,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(downloads.fetch_count(), 1);
    let stored_id = f.content_items()[0].item_id.clone();

    let report = run(
        &f,
        &NoDownloads(FakeTransport::new(releases, vec![])),
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed {
        content_item_id,
        hash,
        ..
    } = &report.state
    else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert_eq!(content_item_id, &stored_id);
    assert!(hash.verified);
    assert!(hash.basis.contains("reused the copy"), "{}", hash.basis);
    assert_eq!(f.content_items().len(), 1, "no second copy is stored");
    assert_eq!(f.layer_count(&second.instance_id), 1);
}

#[tokio::test]
async fn a_stored_copy_whose_bytes_are_corrupt_is_downloaded_again() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("corrupt-first");
    let second = f.instance("corrupt-second");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let releases = vec![vec![release(
        "v1",
        "2026-01-01T00:00:00Z",
        false,
        false,
        &assets,
    )]];
    let downloads = FakeTransport::new(releases.clone(), files_of(&[("v1", &assets)]));
    run(
        &f,
        &downloads,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();

    // Overwrite one stored file with bytes of the same size, as a tampered object would be.
    let stored = f.content_items().remove(0);
    let object = f.ctx.paths.content_object_path(&stored.files[0].sha256);
    content_store::protect::unprotect(&object).unwrap();
    std::fs::write(&object, vec![b'X'; stored.files[0].size as usize]).unwrap();

    let again = FakeTransport::new(releases, files_of(&[("v1", &assets)]));
    let report = run(
        &f,
        &again,
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(again.fetch_count(), 1, "the corrupt copy is not used");
    assert!(
        matches!(report.state, InstallState::Placed { .. }),
        "{:?}",
        report.state
    );
    assert!(
        report
            .plan
            .warnings
            .iter()
            .any(|w| w.contains("does not match its recorded hashes")),
        "{:?}",
        report.plan.warnings
    );
    let check =
        content_store::verify_item(&f.ctx, &stored.item_id, content_store::VerifyDepth::Full)
            .unwrap();
    assert!(check.problems.is_empty(), "{:?}", check.problems);
}

#[tokio::test]
async fn a_damaged_protected_object_of_the_right_size_is_replaced_by_the_next_install() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("protected-first");
    let second = f.instance("protected-second");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let releases = vec![vec![release(
        "v1",
        "2026-01-01T00:00:00Z",
        false,
        false,
        &assets,
    )]];
    let downloads = FakeTransport::new(releases.clone(), files_of(&[("v1", &assets)]));
    run(
        &f,
        &downloads,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();

    // Damage one stored object without changing its size, and protect it again: the quick check
    // trusts a protected object of the right size, so only the full check can find this.
    let stored = f.content_items().remove(0);
    let object = f.ctx.paths.content_object_path(&stored.files[0].sha256);
    let original = std::fs::read(&object).unwrap();
    content_store::protect::unprotect(&object).unwrap();
    std::fs::write(&object, vec![b'X'; original.len()]).unwrap();
    content_store::protect::protect(&object).unwrap();
    assert_eq!(
        content_store::protect::protection(&object).unwrap(),
        content_store::Protection::Protected
    );
    let quick =
        content_store::verify_item(&f.ctx, &stored.item_id, content_store::VerifyDepth::Quick)
            .unwrap();
    assert!(quick.problems.is_empty(), "{:?}", quick.problems);

    let again = FakeTransport::new(releases, files_of(&[("v1", &assets)]));
    let report = run(
        &f,
        &again,
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(again.fetch_count(), 1, "the damaged copy is not used");
    assert!(
        matches!(report.state, InstallState::Placed { .. }),
        "{:?}",
        report.state
    );

    // The next install's import replaced the object with the archive's own bytes.
    assert_eq!(std::fs::read(&object).unwrap(), original);
    let full =
        content_store::verify_item(&f.ctx, &stored.item_id, content_store::VerifyDepth::Full)
            .unwrap();
    assert!(full.problems.is_empty(), "{:?}", full.problems);
}

#[tokio::test]
async fn a_stored_copy_is_not_used_when_the_source_publishes_no_hash() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("unpublished-first");
    let second = f.instance("unpublished-second");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Absent,
    }];
    let releases = vec![vec![release(
        "v1",
        "2026-01-01T00:00:00Z",
        false,
        false,
        &assets,
    )]];
    let downloads = FakeTransport::new(releases.clone(), files_of(&[("v1", &assets)]));
    run(
        &f,
        &downloads,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();

    let again = FakeTransport::new(releases, files_of(&[("v1", &assets)]));
    let report = run(
        &f,
        &again,
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(again.fetch_count(), 1, "nothing published, so it downloads");
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(!hash.verified);
    assert!(!hash.basis.contains("reused"), "{}", hash.basis);
}

#[tokio::test]
async fn an_empty_published_digest_is_no_hash_and_never_matches_a_stored_copy() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("empty-digest-first");
    let second = f.instance("empty-digest-second");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Absent,
    }];
    let downloads = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    run(
        &f,
        &downloads,
        &github_fixture_item(),
        &first.instance_id,
        false,
        false,
    )
    .await
    .unwrap();

    let mut listed = release("v1", "2026-01-01T00:00:00Z", false, false, &assets);
    listed.assets[0].digest = Some(String::new());
    let again = FakeTransport::new(vec![vec![listed]], files_of(&[("v1", &assets)]));
    run(
        &f,
        &again,
        &github_fixture_item(),
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(again.fetch_count(), 1, "an empty digest is not a hash");
}

#[tokio::test]
async fn a_direct_download_is_reused_from_the_store_under_its_manifest_hash() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let first = f.instance("direct-stored-first");
    let second = f.instance("direct-stored-second");
    let bytes = plain_archive();
    let url = "https://files.example-author.example/crash-logger/CrashLogger-1.zip";
    let item = direct_item(url, &sha256_hex(&bytes));
    let downloads = FakeTransport::new(vec![], vec![(url.to_string(), bytes.clone())]);
    run(&f, &downloads, &item, &first.instance_id, false, false)
        .await
        .unwrap();
    assert_eq!(downloads.fetch_count(), 1);

    let report = run(
        &f,
        &NoDownloads(FakeTransport::new(vec![], vec![])),
        &item,
        &second.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed { hash, .. } = &report.state else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    assert!(hash.verified);
    assert!(hash.basis.contains("reused the copy"), "{}", hash.basis);
    assert_eq!(f.layer_count(&second.instance_id), 1);
}

#[tokio::test]
async fn an_empty_direct_download_hash_is_refused_and_nothing_is_fetched() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("direct-empty");
    let bytes = plain_archive();
    let url = "https://files.example-author.example/crash-logger/CrashLogger-1.zip";
    let transport = FakeTransport::new(vec![], vec![(url.to_string(), bytes)]);
    for hash in ["", "   "] {
        let error = run(
            &f,
            &transport,
            &direct_item(url, hash),
            &instance.instance_id,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "ERR_CATALOG_NO_HASH");
    }
    assert_eq!(transport.fetch_count(), 0);
}

// ---------------------------------------------------------------------------
// Placement and the instance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_archive_is_placed_the_way_content_add_would_place_it() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("plain");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::Placed {
        mount_path,
        source_path,
        content_item_id,
        ..
    } = &report.state
    else {
        panic!("expected a placed install, got {:?}", report.state);
    };
    // Root-level .esp and .bsa files go under the data folder, as `content add` places them.
    assert_eq!(mount_path, "Data");
    assert_eq!(source_path, "");
    let manifest = get_manifest(&f.ctx, &instance.instance_id).unwrap();
    let layer = manifest
        .layers
        .layers()
        .iter()
        .find(
            |l| matches!(&l.source, LayerSource::Content { content } if content == content_item_id),
        )
        .expect("the content layer is in the instance");
    assert_eq!(layer.mount_path.as_str(), "Data");
    let item = content_store::get_item(&f.ctx, content_item_id).unwrap();
    let layout = f.definition.content_layout.as_ref().unwrap();
    assert!(matches!(
        game_deploy::decide_placement(&item, layout),
        PlacementDecision::Place { ref mount_path, ref source_path, .. }
            if mount_path == "Data" && source_path.is_empty()
    ));
}

#[tokio::test]
async fn a_fomod_archive_is_imported_but_not_added() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("fomod");
    let bytes = zip_bytes(&[
        ("fomod/ModuleConfig.xml", b"<config/>"),
        ("Data/CrashLogger.esp", b"plugin"),
    ]);
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let InstallState::NeedsInstaller {
        content_item_id,
        installer_command,
        ..
    } = &report.state
    else {
        panic!("expected an installer state, got {:?}", report.state);
    };
    assert!(installer_command.contains("agora games content fomod install"));
    assert!(installer_command.contains(content_item_id.as_str()));
    assert_eq!(f.layer_count(&instance.instance_id), 0);
    assert_eq!(f.content_items().len(), 1, "the archive is imported");
}

#[tokio::test]
async fn an_archive_no_rule_places_is_imported_and_refused_with_where_to_put_it() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("unknown");
    let bytes = zip_bytes(&[("readme.txt", b"hello")]);
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let error = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_UNPLACEABLE");
    assert!(error.to_string().contains("--into"), "{error}");
    assert_eq!(f.layer_count(&instance.instance_id), 0);
}

#[tokio::test]
async fn a_reinstall_of_the_same_item_is_a_no_op() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("reinstall");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let fetches_after_first = transport.fetch_count();
    let again = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        again.state,
        InstallState::AlreadyInstalled {
            installed_release: Some("v1".into()),
            same_release: true,
        }
    );
    assert_eq!(
        transport.fetch_count(),
        fetches_after_first,
        "nothing is downloaded again"
    );
    assert_eq!(f.layer_count(&instance.instance_id), 1);
}

#[tokio::test]
async fn an_installed_item_with_a_newer_release_changes_nothing_and_says_so() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("update");
    let old = plain_archive();
    let new = zip_bytes(&[("CrashLogger.esp", b"newer plugin")]);
    let old_assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &old,
        digest: Digest::Matching,
    }];
    let new_assets = [Asset {
        name: "CrashLogger-2.zip",
        bytes: &new,
        digest: Digest::Matching,
    }];
    let first = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &old_assets,
        )]],
        files_of(&[("v1", &old_assets)]),
    );
    run(
        &f,
        &first,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    let second = FakeTransport::new(
        vec![vec![release(
            "v2",
            "2026-03-01T00:00:00Z",
            false,
            false,
            &new_assets,
        )]],
        files_of(&[("v2", &new_assets)]),
    );
    let report = run(
        &f,
        &second,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        report.state,
        InstallState::AlreadyInstalled {
            installed_release: Some("v1".into()),
            same_release: false,
        }
    );
    assert_eq!(second.fetch_count(), 0);
    assert_eq!(f.layer_count(&instance.instance_id), 1);
}

#[tokio::test]
async fn dry_run_downloads_and_stores_nothing() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("dry");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let report = run(
        &f,
        &transport,
        &github_fixture_item(),
        &instance.instance_id,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(report.state, InstallState::DryRun);
    assert_eq!(report.plan.source.file_name(), "CrashLogger-1.zip");
    assert_eq!(transport.fetch_count(), 0);
    assert!(f.content_items().is_empty());
    assert_eq!(f.layer_count(&instance.instance_id), 0);
}

#[tokio::test]
async fn an_item_for_another_game_is_refused() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("wrong-game");
    let item = entry(
        "crash-logger",
        "valheim",
        vec![github_source(vec![])],
        vec![compat(
            &["steam"],
            &["1.0.*"],
            Some("CrashLogger-*.zip"),
            vec![],
        )],
    );
    let transport = FakeTransport::silent();
    let error = run(&f, &transport, &item, &instance.instance_id, false, true)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "ERR_CATALOG_WRONG_GAME");
    assert_eq!(transport.fetch_count(), 0);
}

#[tokio::test]
async fn the_install_names_its_source_before_a_download_starts() {
    let f = fixture(Some(pe_with_version([0, 2, 2, 6])), true);
    let instance = f.instance("announce");
    let bytes = plain_archive();
    let assets = [Asset {
        name: "CrashLogger-1.zip",
        bytes: &bytes,
        digest: Digest::Matching,
    }];
    let transport = FakeTransport::new(
        vec![vec![release(
            "v1",
            "2026-01-01T00:00:00Z",
            false,
            false,
            &assets,
        )]],
        files_of(&[("v1", &assets)]),
    );
    let mut announced: Vec<String> = Vec::new();
    install(
        &f.ctx,
        &transport,
        InstallRequest {
            instance_id: &instance.instance_id,
            item: &github_fixture_item(),
            install_anyway: false,
            allow_missing_frameworks: false,
            dry_run: false,
        },
        &mut |plan: &CatalogPlan| {
            announced.push(format!(
                "{} from {}",
                plan.source.file_name(),
                plan.source.describe()
            ))
        },
    )
    .await
    .unwrap();
    assert_eq!(announced.len(), 1);
    assert!(announced[0].contains("CrashLogger-1.zip"), "{announced:?}");
    assert!(announced[0].contains(REPO), "{announced:?}");
}
