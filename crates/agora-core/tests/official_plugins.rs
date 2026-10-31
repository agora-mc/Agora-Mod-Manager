//! Official plugins: packages the signed registry vouches for (id, version, package SHA-256).
//!
//! Every registry here is signed with a key made for the test, and handed to the code under
//! test through `RegistryTrust::with_key`; the packages come from an injected fetcher, so nothing
//! touches a network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agora_core::app_paths::AppPaths;
use agora_core::ctx::CoreContext;
use agora_core::error::LauncherResult;
use agora_core::game_registry::GameRegistry;
use agora_core::plugins::{
    NoopUiSink, OfficialCatalog, OfficialInstallState, OfficialStatus, PackageFetcher,
    PluginService, RegistryTrust, UpdateOutcome, PLUGINS_ENABLED_SETTING,
};
use agora_game_api::{DeploymentStrategy, GameDefinition, GameId, PackageDefinition};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_host::QuickJsHost;
use ed25519_dalek::{Signer, SigningKey};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};

const PLUGIN: &str = "acme.fixture";
const GAME: &str = "fixture-game";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn trust() -> RegistryTrust {
    RegistryTrust::with_key(hex::encode(signing_key().verifying_key().to_bytes()))
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn game_package(plugin_game: &str) -> PackageDefinition {
    PackageDefinition {
        id: format!("test.pkg.{plugin_game}"),
        version: Version::new(0, 1, 0),
        api_range: VersionReq::parse(">=0.1, <0.2").unwrap(),
        parents: vec![],
        games: vec![GameDefinition {
            mo2_game_name: None,
            id: GameId::new(plugin_game).unwrap(),
            name: plugin_game.to_string(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::Redirect,
            content_rules: vec![],
            native_code_patterns: vec![],
            framework_ids: vec![],
            tool_ids: vec![],
            launch: None,
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
            launch_alternatives: Vec::new(),
            content_layout: None,
            copy_patterns: Vec::new(),
        }],
        frameworks: vec![],
        tools: vec![],
    }
}

/// A plugin package: the manifest plus one game package. `extra` lets a test make two zips that
/// claim the same id and version but differ.
fn package_zip(id: &str, version: &str, game: &str, capability: &str, extra: &str) -> Vec<u8> {
    use std::io::Write;
    let manifest = format!(
        r#"{{
        "manifest": 1,
        "id": "{id}",
        "name": "Fixture",
        "version": "{version}",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "capabilities": {{ "required": ["{capability}"] }},
        "contributions": {{ "gamePackages": [ {{ "path": "games/package.json" }} ] }}
    }}"#
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("games/package.json", options).unwrap();
        zip.write_all(
            serde_json::to_string(&game_package(game))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        if !extra.is_empty() {
            zip.start_file("extra.txt", options).unwrap();
            zip.write_all(extra.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

fn fixture_zip(version: &str) -> Vec<u8> {
    package_zip(PLUGIN, version, GAME, "game:define", "")
}

struct Release {
    version: &'static str,
    channel: &'static str,
    url: String,
    sha256: String,
    size: i64,
    min_agora: &'static str,
}

impl Release {
    fn of(version: &'static str, channel: &'static str, zip: &[u8]) -> Self {
        Self {
            version,
            channel,
            url: format!("https://plugins.example.org/{PLUGIN}-{version}.zip"),
            sha256: sha(zip),
            size: zip.len() as i64,
            min_agora: "0.1.0",
        }
    }
}

/// Write `registry.db` with an `official_plugins` table and sign it.
fn write_registry(root: &Path, releases: &[Release], key: &SigningKey, sign: bool) {
    let db = root.join("registry.db");
    let _ = std::fs::remove_file(&db);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
         INSERT INTO schema_version (version) VALUES (9);
         CREATE TABLE official_plugins (
            plugin_id TEXT NOT NULL,
            version TEXT NOT NULL,
            channel TEXT NOT NULL CHECK (channel IN ('current', 'previous')),
            name TEXT NOT NULL,
            description TEXT,
            game_ids_json TEXT NOT NULL DEFAULT '[]',
            package_url TEXT NOT NULL,
            package_sha256 TEXT NOT NULL,
            package_size INTEGER NOT NULL,
            min_agora_version TEXT NOT NULL,
            PRIMARY KEY (plugin_id, version)
         );",
    )
    .unwrap();
    for release in releases {
        conn.execute(
            "INSERT INTO official_plugins VALUES (?1, ?2, ?3, 'Fixture', 'A test plugin', ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                PLUGIN,
                release.version,
                release.channel,
                format!(r#"["{GAME}"]"#),
                release.url,
                release.sha256,
                release.size,
                release.min_agora,
            ],
        )
        .unwrap();
    }
    drop(conn);
    sign_registry(root, key, sign);
}

fn sign_registry(root: &Path, key: &SigningKey, sign: bool) {
    let sig = root.join("registry.db.sig");
    let _ = std::fs::remove_file(&sig);
    if sign {
        let bytes = std::fs::read(root.join("registry.db")).unwrap();
        std::fs::write(&sig, key.sign(&bytes).to_bytes()).unwrap();
    }
}

#[derive(Default)]
struct FakeFetcher {
    packages: Mutex<HashMap<String, Vec<u8>>>,
    calls: AtomicUsize,
}

impl FakeFetcher {
    fn serve(&self, url: &str, bytes: Vec<u8>) {
        self.packages.lock().unwrap().insert(url.into(), bytes);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PackageFetcher for FakeFetcher {
    async fn fetch(&self, url: &str, pinned_host: &str) -> LauncherResult<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(pinned_host, "plugins.example.org");
        self.packages
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .ok_or_else(|| agora_core::error::LauncherError::Generic {
                code: "ERR_TEST_NOT_SERVED".into(),
                message: format!("nothing is served at {url}"),
            })
    }
}

struct World {
    dir: tempfile::TempDir,
    service: PluginService,
    fetcher: FakeFetcher,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = CoreContext::for_testing(dir.path().to_path_buf());
        ctx.paths.create_required_dirs().unwrap();
        agora_core::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
        let service = PluginService::new_with_registry_trust(
            ctx,
            Arc::new(QuickJsHost::new()),
            Arc::new(NoopUiSink),
            trust(),
        );
        Self {
            dir,
            service,
            fetcher: FakeFetcher::default(),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn paths(&self) -> AppPaths {
        AppPaths::from_root(self.root().to_path_buf())
    }

    /// Sign a registry listing `releases`, and serve each one's real zip.
    fn publish(&self, releases: Vec<(Release, Vec<u8>)>) {
        for (release, zip) in &releases {
            self.fetcher.serve(&release.url, zip.clone());
        }
        let list: Vec<Release> = releases.into_iter().map(|(release, _)| release).collect();
        write_registry(self.root(), &list, &signing_key(), true);
    }

    fn set_plugins_enabled(&self, enabled: bool) {
        let conn = agora_core::db::local_state_connection(&self.paths().local_state_db()).unwrap();
        agora_core::db::set_setting(&conn, PLUGINS_ENABLED_SETTING, &serde_json::json!(enabled))
            .unwrap();
    }

    fn id() -> PluginId {
        PluginId::parse(PLUGIN).unwrap()
    }

    fn status(&self) -> OfficialStatus {
        self.service.official_status(&Self::id()).unwrap()
    }

    fn installed_dir(&self) -> PathBuf {
        self.paths().plugin_packages_root().join(PLUGIN)
    }

    /// Start the app against this data folder, trusting the test key.
    ///
    /// Startup also checks the cached registry against the *production* key, which a test-signed
    /// registry fails and gets quarantined for. The files are put back afterwards so a test can
    /// keep using them; the official check at startup has already run against the test key.
    fn start(&self) -> (CoreContext, Vec<String>) {
        let db = self.paths().registry_db();
        let sig = self.paths().registry_signature();
        let saved = (std::fs::read(&db).ok(), std::fs::read(&sig).ok());
        let started = CoreContext::initialize_with_registry_trust(
            self.paths(),
            GameRegistry::builder(),
            trust(),
        )
        .unwrap();
        for (path, bytes) in [(db, saved.0), (sig, saved.1)] {
            let _ = std::fs::remove_file(&path);
            if let Some(bytes) = bytes {
                std::fs::write(&path, bytes).unwrap();
            }
        }
        started
    }
}

/// Every file under the plugins folder, so "nothing was written" can be asserted.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

fn installed(outcome: UpdateOutcome) -> agora_core::plugins::PluginSummary {
    match outcome {
        UpdateOutcome::Installed { plugin } => *plugin,
        UpdateOutcome::NeedsConsent { .. } => panic!("expected an install, got a consent request"),
    }
}

fn error_code<T: std::fmt::Debug>(result: LauncherResult<T>) -> String {
    result.expect_err("expected a refusal").code()
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

#[tokio::test]
async fn install_verifies_the_hash_before_extracting_anything() {
    let world = World::new();
    let good = fixture_zip("1.0.0");
    // Same length as the real package, so only the hash can tell them apart.
    let mut forged = good.clone();
    let last = forged.len() - 1;
    forged[last] ^= 0xFF;
    world.publish(vec![(Release::of("1.0.0", "current", &good), forged)]);

    let code = error_code(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await,
    );

    assert_eq!(code, "ERR_OFFICIAL_PLUGIN_HASH");
    assert_eq!(world.fetcher.calls(), 1, "it did download");
    assert!(
        files_under(&world.paths().plugins_root()).is_empty(),
        "a refused package must leave nothing under the plugins folder: {:?}",
        files_under(&world.paths().plugins_root())
    );
    assert!(!world.installed_dir().exists());
    assert!(world.service.list().is_empty());
}

#[tokio::test]
async fn a_size_mismatch_is_refused_before_anything_is_written() {
    let world = World::new();
    let good = fixture_zip("1.0.0");
    let mut longer = good.clone();
    longer.extend_from_slice(b"trailing bytes");
    // The hash in the registry is of what is served, so only the size can be wrong.
    let mut release = Release::of("1.0.0", "current", &longer);
    release.size = good.len() as i64;
    world.publish(vec![(release, longer)]);

    let code = error_code(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await,
    );

    assert_eq!(code, "ERR_OFFICIAL_PLUGIN_SIZE");
    assert!(files_under(&world.paths().plugins_root()).is_empty());
    assert!(world.service.list().is_empty());
}

#[tokio::test]
async fn a_good_install_is_official_and_records_the_signed_entry() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip.clone())]);

    let plugin = installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    assert_eq!(plugin.version, "1.0.0");
    assert_eq!(
        plugin.official,
        OfficialStatus::Official {
            version: "1.0.0".into()
        }
    );
    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();
    let provenance = agora_core::plugins::store::list_official(&conn).unwrap();
    let row = &provenance[PLUGIN];
    assert_eq!(row.entry_id, PLUGIN);
    assert_eq!(row.entry_version, "1.0.0");
    assert_eq!(row.entry_sha256, sha(&zip));
    // The staged download is not kept.
    assert!(files_under(&world.paths().plugin_rollback_root()).is_empty());

    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.unavailable_reason, None);
    assert_eq!(list.plugins[0].state, OfficialInstallState::UpToDate);
    assert_eq!(list.plugins[0].game_ids, vec![GAME.to_string()]);
}

#[tokio::test]
async fn the_normal_capability_consent_still_applies() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);

    let outcome = world
        .service
        .install_official(PLUGIN, false, &world.fetcher)
        .await
        .unwrap();
    match outcome {
        UpdateOutcome::NeedsConsent { preview } => {
            assert_eq!(preview.added_capabilities, vec!["game:define".to_string()]);
        }
        UpdateOutcome::Installed { .. } => panic!("official must not skip capability consent"),
    }
    assert!(world.service.list().is_empty(), "nothing installed yet");
    assert!(!world.installed_dir().exists());

    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert!(world.status().is_official());
}

#[tokio::test]
async fn a_package_that_names_itself_something_else_is_refused_despite_the_right_hash() {
    let world = World::new();
    let other = package_zip("acme.other", "1.0.0", GAME, "game:define", "");
    world.publish(vec![(Release::of("1.0.0", "current", &other), other)]);

    let code = error_code(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await,
    );

    assert_eq!(code, "ERR_OFFICIAL_PLUGIN_MISMATCH");
    assert!(world.service.list().is_empty());
    assert!(!world.installed_dir().exists());
    assert!(files_under(&world.paths().plugin_rollback_root()).is_empty());
}

#[tokio::test]
async fn a_zip_with_the_right_id_but_not_the_signed_hash_is_only_a_community_plugin() {
    let world = World::new();
    let signed = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &signed), signed)]);

    // The same id and version, different bytes, through the ordinary installer.
    let lookalike = package_zip(PLUGIN, "1.0.0", GAME, "game:define", "not what was signed");
    let archive = world.root().join("lookalike.zip");
    std::fs::write(&archive, &lookalike).unwrap();
    let plugin = world.service.install_package(&archive, true).unwrap();

    assert_eq!(plugin.official, OfficialStatus::Community);
    assert_eq!(world.status(), OfficialStatus::Community);
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::NotOfficial);

    // It does not get the exemption: with the community switch off it is not loaded.
    world.set_plugins_enabled(false);
    let (ctx, _) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
}

#[tokio::test]
async fn a_community_install_over_an_official_one_ends_its_official_standing() {
    let world = World::new();
    let signed = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &signed), signed)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert!(world.status().is_official());

    let lookalike = package_zip(PLUGIN, "1.0.0", GAME, "game:define", "different bytes");
    let archive = world.root().join("lookalike.zip");
    std::fs::write(&archive, &lookalike).unwrap();
    world.service.install_package(&archive, true).unwrap();

    assert_eq!(world.status(), OfficialStatus::Community);
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_official_plugin_loads_with_plugins_disabled_and_a_community_plugin_does_not() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    // A community plugin that defines a different game.
    let community = package_zip(
        "acme.community",
        "1.0.0",
        "community-game",
        "game:define",
        "",
    );
    let archive = world.root().join("community.zip");
    std::fs::write(&archive, &community).unwrap();
    world.service.install_package(&archive, true).unwrap();
    world.set_plugins_enabled(false);

    let (ctx, warnings) = world.start();

    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
        "the official plugin's game must load with the community switch off; warnings: {warnings:?}"
    );
    assert!(
        ctx.games
            .game(&GameId::new("community-game").unwrap())
            .is_none(),
        "a community plugin keeps needing the switch"
    );

    // Switch on: both load.
    world.set_plugins_enabled(true);
    let (ctx, _) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());
    assert!(ctx
        .games
        .game(&GameId::new("community-game").unwrap())
        .is_some());
}

#[tokio::test]
async fn editing_an_installed_official_plugins_files_removes_official_status_with_a_reason() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert!(world.status().is_official());

    let target = world.installed_dir().join("games/package.json");
    let original = std::fs::read(&target).unwrap();
    let mut edited = original.clone();
    edited.extend_from_slice(b"\n");
    std::fs::write(&target, &edited).unwrap();

    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("an edited official plugin must lose its standing");
    };
    assert!(reason.contains("changed"), "reason: {reason}");
    let summary = world.service.list().remove(0);
    assert!(
        matches!(summary.official, OfficialStatus::Unverified { .. }),
        "the manager must show why, not silently hide the badge"
    );
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::NotOfficial);

    // With the community switch off the edited plugin does not load, and the user is told.
    world.set_plugins_enabled(false);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(
        warnings.iter().any(|w| w.contains(PLUGIN)
            && w.contains("no longer verifies")
            && w.contains("changed")),
        "warnings: {warnings:?}"
    );

    // With the switch on it loads as a community plugin, still with the warning.
    world.set_plugins_enabled(true);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());
    assert!(warnings.iter().any(|w| w.contains("no longer verifies")));

    // Putting the file back restores it: the check is about the files, not a one-way flag.
    std::fs::write(&target, &original).unwrap();
    assert!(world.status().is_official());

    // An added file counts as an edit too.
    std::fs::write(world.installed_dir().join("extra.js"), b"1").unwrap();
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
}

#[tokio::test]
async fn an_entry_the_registry_no_longer_lists_stops_being_official() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // A later registry lists some other version only.
    let newer = fixture_zip("2.0.0");
    world.publish(vec![(Release::of("2.0.0", "current", &newer), newer)]);

    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("a removed entry must not stay official");
    };
    assert!(reason.contains("no longer lists"), "reason: {reason}");
    world.set_plugins_enabled(false);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(warnings.iter().any(|w| w.contains("no longer lists")));
}

#[tokio::test]
async fn an_entry_that_pins_a_different_hash_stops_being_official() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip.clone())]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // Same id and version, but the signed registry now pins other bytes.
    let other = package_zip(PLUGIN, "1.0.0", GAME, "game:define", "re-released");
    world.publish(vec![(Release::of("1.0.0", "current", &other), other)]);

    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("a re-pinned hash must not keep the old install official");
    };
    assert!(reason.contains("different package"), "reason: {reason}");
}

// ---------------------------------------------------------------------------
// The registry as the trust root
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unsigned_or_tampered_registry_offers_no_official_plugins() {
    let zip = fixture_zip("1.0.0");
    let release = || vec![Release::of("1.0.0", "current", &zip)];

    // Unsigned: no signature sidecar.
    let world = World::new();
    world.fetcher.serve(&release()[0].url, zip.clone());
    write_registry(world.root(), &release(), &signing_key(), false);
    let list = world.service.official_plugins().unwrap();
    assert!(list.plugins.is_empty());
    assert!(list.unavailable_reason.is_some());
    assert_eq!(
        error_code(
            world
                .service
                .install_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_CATALOG"
    );
    assert_eq!(
        world.fetcher.calls(),
        0,
        "nothing is fetched on an unverified catalog"
    );

    // Signed by somebody else.
    let world = World::new();
    world.fetcher.serve(&release()[0].url, zip.clone());
    write_registry(
        world.root(),
        &release(),
        &SigningKey::from_bytes(&[9u8; 32]),
        true,
    );
    assert!(world.service.official_plugins().unwrap().plugins.is_empty());
    assert!(world
        .service
        .official_plugins()
        .unwrap()
        .unavailable_reason
        .is_some());

    // Tampered after signing: the package URL is swapped for another.
    let world = World::new();
    world.fetcher.serve(&release()[0].url, zip.clone());
    write_registry(world.root(), &release(), &signing_key(), true);
    let conn = rusqlite::Connection::open(world.root().join("registry.db")).unwrap();
    conn.execute(
        "UPDATE official_plugins SET package_url = 'https://evil.example/x.zip'",
        [],
    )
    .unwrap();
    drop(conn);
    let list = world.service.official_plugins().unwrap();
    assert!(list.plugins.is_empty());
    assert!(list.unavailable_reason.is_some());
    assert_eq!(
        error_code(
            world
                .service
                .install_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_CATALOG"
    );
    assert_eq!(world.fetcher.calls(), 0);

    // No registry at all.
    let world = World::new();
    let list = world.service.official_plugins().unwrap();
    assert!(list.plugins.is_empty());
    assert!(list.unavailable_reason.is_some());
}

#[tokio::test]
async fn an_installed_official_plugin_loses_standing_when_the_registry_stops_verifying() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // Corrupt the signature.
    std::fs::write(world.root().join("registry.db.sig"), [0u8; 64]).unwrap();

    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("without a verifiable registry nothing is official");
    };
    assert!(reason.contains("signature"), "reason: {reason}");
    world.set_plugins_enabled(false);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(warnings.iter().any(|w| w.contains("no longer verifies")));
}

#[tokio::test]
async fn a_registry_without_the_official_plugins_table_lists_none_and_is_not_an_error() {
    // An older registry: signed, valid, and predating the table.
    let world = World::new();
    let db = world.root().join("registry.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
         INSERT INTO schema_version (version) VALUES (9);",
    )
    .unwrap();
    drop(conn);
    sign_registry(world.root(), &signing_key(), true);

    let list = world.service.official_plugins().unwrap();

    assert!(list.plugins.is_empty());
    assert_eq!(list.unavailable_reason, None);
}

#[tokio::test]
async fn an_entry_whose_url_is_not_https_is_refused() {
    for bad in [
        "http://plugins.example.org/acme.fixture-1.0.0.zip",
        "ftp://plugins.example.org/acme.fixture-1.0.0.zip",
        "https://user:pw@plugins.example.org/acme.fixture-1.0.0.zip",
        "https://plugins.example.org:8443/acme.fixture-1.0.0.zip",
        "plugins.example.org/acme.fixture-1.0.0.zip",
        "",
    ] {
        let world = World::new();
        let zip = fixture_zip("1.0.0");
        let mut release = Release::of("1.0.0", "current", &zip);
        release.url = bad.to_string();
        world.fetcher.serve(bad, zip);
        write_registry(world.root(), &[release], &signing_key(), true);

        let list = world.service.official_plugins().unwrap();
        assert!(
            list.plugins[0].problem.is_some(),
            "{bad:?} must be flagged in the list"
        );
        let code = error_code(
            world
                .service
                .install_official(PLUGIN, true, &world.fetcher)
                .await,
        );
        assert_eq!(code, "ERR_OFFICIAL_PLUGIN_ENTRY", "{bad:?}");
        assert_eq!(world.fetcher.calls(), 0, "{bad:?} must never be fetched");
        assert!(files_under(&world.paths().plugins_root()).is_empty());
    }
}

#[tokio::test]
async fn an_entry_that_needs_a_newer_agora_is_refused() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    let mut release = Release::of("1.0.0", "current", &zip);
    release.min_agora = "99.0.0";
    world.fetcher.serve(&release.url, zip);
    write_registry(world.root(), &[release], &signing_key(), true);

    let list = world.service.official_plugins().unwrap();
    assert!(list.plugins[0]
        .problem
        .as_deref()
        .is_some_and(|p| p.contains("99.0.0")));
    let code = error_code(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await,
    );
    assert_eq!(code, "ERR_OFFICIAL_PLUGIN_INCOMPATIBLE");
    assert_eq!(world.fetcher.calls(), 0);
}

#[tokio::test]
async fn empty_traversal_and_wrong_case_ids_do_not_match_an_entry() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);

    for id in [
        "",
        " ",
        "../x",
        "..",
        "acme.fixture/../x",
        "ACME.FIXTURE",
        "Acme.Fixture",
        " acme.fixture",
        "acme.fixture ",
        "acme.",
        ".fixture",
    ] {
        let code = error_code(
            world
                .service
                .install_official(id, true, &world.fetcher)
                .await,
        );
        assert_eq!(code, "ERR_OFFICIAL_PLUGIN_UNKNOWN", "id {id:?}");
        assert_eq!(
            error_code(
                world
                    .service
                    .update_official(id, true, &world.fetcher)
                    .await
            ),
            "ERR_OFFICIAL_PLUGIN_UNKNOWN",
            "id {id:?}"
        );
        assert_eq!(
            error_code(
                world
                    .service
                    .rollback_official(id, true, &world.fetcher)
                    .await
            ),
            "ERR_OFFICIAL_PLUGIN_UNKNOWN",
            "id {id:?}"
        );
    }
    assert_eq!(world.fetcher.calls(), 0);
    assert!(files_under(&world.paths().plugins_root()).is_empty());

    let catalog =
        agora_core::plugins::official::load_signed_catalog(&trust(), world.root()).unwrap();
    for id in ["", "../x", "ACME.FIXTURE"] {
        assert!(catalog.current(id).is_none(), "{id:?}");
        assert!(catalog.previous(id).is_none(), "{id:?}");
        assert!(catalog.find(id, "1.0.0").is_none(), "{id:?}");
    }
    assert!(catalog.current(PLUGIN).is_some());
    let _: &OfficialCatalog = &catalog;
}

// ---------------------------------------------------------------------------
// Update and rollback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_moves_to_the_new_entry_and_rollback_returns_to_the_previous_one() {
    let world = World::new();
    let v1 = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &v1), v1.clone())]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // A newer version is published; 1.0.0 is kept as the previous entry.
    let v2 = fixture_zip("1.1.0");
    world.publish(vec![
        (Release::of("1.1.0", "current", &v2), v2.clone()),
        (Release::of("1.0.0", "previous", &v1), v1.clone()),
    ]);
    assert!(
        world.status().is_official(),
        "the installed 1.0.0 is still listed (as the previous entry), so it stays official"
    );
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::UpdateAvailable);
    assert_eq!(list.plugins[0].version, "1.1.0");
    assert_eq!(list.plugins[0].previous_version.as_deref(), Some("1.0.0"));
    assert!(
        !list.plugins[0].can_roll_back,
        "1.0.0 is the previous entry itself"
    );

    // Install refuses (it is installed); update moves.
    assert_eq!(
        error_code(
            world
                .service
                .install_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_INSTALLED"
    );
    let plugin = installed(
        world
            .service
            .update_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(plugin.version, "1.1.0");
    assert!(plugin.official.is_official());
    assert!(world.service.official_plugins().unwrap().plugins[0].can_roll_back);
    assert_eq!(
        error_code(
            world
                .service
                .update_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_CURRENT"
    );

    // Roll back to the previous entry.
    let plugin = installed(
        world
            .service
            .rollback_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(plugin.version, "1.0.0");
    assert_eq!(
        plugin.official,
        OfficialStatus::Official {
            version: "1.0.0".into()
        }
    );
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::UpdateAvailable);
    assert!(!list.plugins[0].can_roll_back);
    assert_eq!(
        error_code(
            world
                .service
                .rollback_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_CURRENT"
    );

    // And forward again.
    let plugin = installed(
        world
            .service
            .update_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(plugin.version, "1.1.0");
}

#[tokio::test]
async fn rollback_needs_a_previous_entry_and_an_official_install() {
    let world = World::new();
    let v1 = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &v1), v1.clone())]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(
        error_code(
            world
                .service
                .rollback_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_NO_PREVIOUS"
    );

    // With a previous entry, but a community copy installed: nothing official to roll back.
    let v2 = fixture_zip("1.1.0");
    world.publish(vec![
        (Release::of("1.1.0", "current", &v2), v2),
        (Release::of("1.0.0", "previous", &v1), v1),
    ]);
    let lookalike = package_zip(PLUGIN, "1.1.0", GAME, "game:define", "mine");
    let archive = world.root().join("mine.zip");
    std::fs::write(&archive, &lookalike).unwrap();
    world.service.install_package(&archive, true).unwrap();
    assert_eq!(
        error_code(
            world
                .service
                .rollback_official(PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_NOT_OFFICIAL"
    );
    // Update replaces the community copy with the official package.
    let plugin = installed(
        world
            .service
            .update_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert!(plugin.official.is_official());
}

#[tokio::test]
async fn uninstalling_ends_official_standing() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip.clone())]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    world.service.uninstall(&World::id(), false).unwrap();

    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();
    assert!(agora_core::plugins::store::list_official(&conn)
        .unwrap()
        .is_empty());
    assert_eq!(world.status(), OfficialStatus::Community);
}
