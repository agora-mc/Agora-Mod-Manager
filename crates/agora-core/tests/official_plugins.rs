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
use agora_core::game_registry::{GameRegistry, PackageSource};
use agora_core::plugins::official::{tree_digest, zip_tree_digest};
use agora_core::plugins::{
    Consent, NoopUiSink, OfficialCatalog, OfficialInstallState, OfficialStatus, PackageFetcher,
    PluginService, PluginStatus, RegistryTrust, UpdateOutcome, PLUGINS_EMERGENCY_STOP_SETTING,
};
use agora_game_api::{
    DeploymentStrategy, GameDefinition, GameId, PackageDefinition, StoreId, StoreIdentifier,
};
use agora_plugin_api::distribution::{PublicKey, UPDATE_SOURCE_FILENAME};
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
        game_templates: std::collections::BTreeMap::new(),
        behaviours: None,
        games: vec![GameDefinition {
            mo2_game_name: None,
            session: Default::default(),
            id: GameId::new(plugin_game).unwrap(),
            extends: None,
            name: plugin_game.to_string(),
            stores: vec![],
            version_sources: vec![],
            deployment: DeploymentStrategy::Redirect,
            content_rules: vec![],
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
            load_orders: Vec::new(),
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
    package_zip_with(id, version, game_package(game), capability, extra)
}

/// A package whose game is claimed in the Steam store under `product`.
fn game_package_in_steam(game: &str, product: &str) -> PackageDefinition {
    let mut package = game_package(game);
    package.games[0].stores = vec![StoreIdentifier {
        store: StoreId::steam(),
        product: product.to_string(),
    }];
    package
}

fn package_zip_with(
    id: &str,
    version: &str,
    package: PackageDefinition,
    capability: &str,
    extra: &str,
) -> Vec<u8> {
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
        zip.write_all(serde_json::to_string(&package).unwrap().as_bytes())
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

/// A package with its own dependencies, an optional game package, and optionally
/// a script. Used for the official dependency tests.
fn dependency_zip(
    id: &str,
    version: &str,
    dependencies: &[(&str, &str)],
    capability: &str,
    game: Option<&str>,
    runs_code: bool,
) -> Vec<u8> {
    use std::io::Write;
    let mut manifest = serde_json::json!({
        "manifest": 1,
        "id": id,
        "name": format!("Fixture {id}"),
        "version": version,
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
    });
    let mut required = Vec::new();
    if !capability.is_empty() {
        required.push(capability.to_string());
    }
    if game.is_some() && capability != "game:define" {
        required.push("game:define".to_string());
    }
    if !required.is_empty() {
        manifest["capabilities"] = serde_json::json!({ "required": required });
    }
    if !dependencies.is_empty() {
        let deps: serde_json::Map<String, serde_json::Value> = dependencies
            .iter()
            .map(|(dep, requirement)| {
                (
                    dep.to_string(),
                    serde_json::Value::String(requirement.to_string()),
                )
            })
            .collect();
        manifest["dependencies"] = serde_json::Value::Object(deps);
    }
    if game.is_some() {
        manifest["contributions"] =
            serde_json::json!({ "gamePackages": [ { "path": "games/package.json" } ] });
    }
    if runs_code {
        manifest["entrypoint"] = serde_json::json!("main.js");
    }
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();
        if let Some(game) = game {
            zip.start_file("games/package.json", options).unwrap();
            zip.write_all(
                serde_json::to_string(&game_package(game))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        }
        if runs_code {
            zip.start_file("main.js", options).unwrap();
            zip.write_all(b"export function noop() { return 1; }\n")
                .unwrap();
        }
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

struct Release {
    plugin_id: &'static str,
    version: &'static str,
    channel: &'static str,
    url: String,
    sha256: String,
    tree_sha256: String,
    size: i64,
    min_agora: &'static str,
    revoked: bool,
}

impl Release {
    fn of(version: &'static str, channel: &'static str, zip: &[u8]) -> Self {
        Self::of_plugin(PLUGIN, version, channel, zip)
    }

    fn of_plugin(
        plugin_id: &'static str,
        version: &'static str,
        channel: &'static str,
        zip: &[u8],
    ) -> Self {
        Self {
            plugin_id,
            version,
            channel,
            url: format!("https://plugins.example.org/{plugin_id}-{version}.zip"),
            sha256: sha(zip),
            tree_sha256: zip_tree_digest(zip).unwrap(),
            size: zip.len() as i64,
            min_agora: "0.1.0",
            revoked: false,
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
            package_tree_sha256 TEXT NOT NULL,
            package_size INTEGER NOT NULL,
            min_agora_version TEXT NOT NULL,
            revoked INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (plugin_id, version)
         );",
    )
    .unwrap();
    for release in releases {
        conn.execute(
            "INSERT INTO official_plugins VALUES (?1, ?2, ?3, 'Fixture', 'A test plugin', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                release.plugin_id,
                release.version,
                release.channel,
                format!(r#"["{GAME}"]"#),
                release.url,
                release.sha256,
                release.tree_sha256,
                release.size,
                release.min_agora,
                release.revoked as i64,
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

    /// Engage or release the emergency stop through the setting, as `settings set` would.
    fn set_emergency_stop(&self, engaged: bool) {
        let conn = agora_core::db::local_state_connection(&self.paths().local_state_db()).unwrap();
        agora_core::db::set_setting(
            &conn,
            PLUGINS_EMERGENCY_STOP_SETTING,
            &serde_json::json!(engaged),
        )
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

    fn packages_root(&self) -> PathBuf {
        self.paths().plugin_packages_root()
    }

    /// The installed version of any plugin id, if it is installed.
    fn version_of(&self, plugin_id: &str) -> Option<String> {
        self.service
            .list()
            .into_iter()
            .find(|plugin| plugin.id == plugin_id)
            .map(|plugin| plugin.version)
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
async fn a_zip_with_the_right_id_but_not_the_signed_hash_is_blocked_not_official() {
    let world = World::new();
    let signed = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &signed), signed)]);

    // The same id and version, different bytes, through the ordinary installer.
    let lookalike = package_zip(PLUGIN, "1.0.0", GAME, "game:define", "not what was signed");
    let archive = world.root().join("lookalike.zip");
    std::fs::write(&archive, &lookalike).unwrap();
    let plugin = world.service.install_package(&archive, true).unwrap();

    // Installed, but it is not the official plugin and it borrows its id: blocked.
    assert!(matches!(plugin.official, OfficialStatus::Unverified { .. }));
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
    assert!(
        matches!(plugin.status, PluginStatus::Blocked { .. }),
        "{:?}",
        plugin.status
    );
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::NotOfficial);

    // Not loaded, whatever the emergency stop says.
    for engaged in [true, false] {
        world.set_emergency_stop(engaged);
        let (ctx, warnings) = world.start();
        assert!(
            ctx.games.game(&GameId::new(GAME).unwrap()).is_none(),
            "emergency stop engaged = {engaged}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(PLUGIN) && w.contains("blocked")),
            "warnings: {warnings:?}"
        );
    }
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

    // No longer the official install. Its id is still an official plugin's, so it is
    // blocked rather than quietly becoming a community plugin.
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn data_only_plugins_load_whatever_the_emergency_stop_says() {
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
    // A community plugin that defines a different game. Its install consent was the opt-in.
    let community = package_zip(
        "acme.community",
        "1.0.0",
        "community-game",
        "game:define",
        "",
    );
    world
        .service
        .install_package_bytes(&community, true)
        .unwrap();

    for engaged in [true, false] {
        world.set_emergency_stop(engaged);
        let (ctx, warnings) = world.start();
        assert!(
            ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
            "official data-only, stop engaged = {engaged}; warnings: {warnings:?}"
        );
        assert!(
            ctx.games
                .game(&GameId::new("community-game").unwrap())
                .is_some(),
            "community data-only, stop engaged = {engaged}"
        );
    }
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
    world.service.reload().unwrap();
    let summary = world.service.list().remove(0);
    assert!(
        matches!(summary.official, OfficialStatus::Unverified { .. }),
        "the manager must show why, not silently hide the badge"
    );
    assert!(
        matches!(summary.status, PluginStatus::Blocked { .. }),
        "{:?}",
        summary.status
    );
    let list = world.service.official_plugins().unwrap();
    assert_eq!(list.plugins[0].state, OfficialInstallState::NotOfficial);

    // With the community switch off the edited plugin does not load, and the user is told.
    world.set_emergency_stop(true);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains(PLUGIN) && w.contains("blocked") && w.contains("changed")),
        "warnings: {warnings:?}"
    );

    // With the switch on it is still blocked: an edited official plugin is not a
    // community plugin, and must not load for whoever turned the switch on.
    world.set_emergency_stop(false);
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(warnings.iter().any(|w| w.contains("blocked")));

    // Putting the file back restores it: the check is about the files, not a one-way flag.
    std::fs::write(&target, &original).unwrap();
    assert!(world.status().is_official());

    // An added file counts as an edit too.
    std::fs::write(world.installed_dir().join("extra.js"), b"1").unwrap();
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
}

#[tokio::test]
async fn an_entry_the_registry_no_longer_lists_keeps_loading_with_a_warning() {
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

    let OfficialStatus::NotListed { reason } = world.status() else {
        panic!("a removed entry must not stay official");
    };
    assert!(reason.contains("no longer listed"), "reason: {reason}");
    world.set_emergency_stop(true);
    let (ctx, warnings) = world.start();
    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
        "an old version that is no longer listed still loads its game"
    );
    assert!(warnings.iter().any(|w| w.contains("no longer listed")));
    assert!(
        !world
            .service
            .list()
            .iter()
            .any(|plugin| matches!(plugin.status, PluginStatus::Blocked { .. })),
        "it is not blocked"
    );
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
async fn an_installed_official_plugin_keeps_loading_when_the_registry_stops_verifying() {
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

    let OfficialStatus::CatalogUnavailable { reason } = world.status() else {
        panic!("without a verifiable registry it is not official");
    };
    assert!(reason.contains("signature"), "reason: {reason}");
    world.set_emergency_stop(true);
    let (ctx, warnings) = world.start();
    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
        "the game stays while the catalog cannot be checked"
    );
    assert!(warnings.iter().any(|w| w.contains("could not check")));
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

// ---------------------------------------------------------------------------
// Official dependencies
// ---------------------------------------------------------------------------

const BASE: &str = "acme.base";
const GAME_PLUGIN: &str = "acme.gameplugin";

/// The target as the card offers it, and the shared base it names.
fn dependency_releases(
    game_version: &'static str,
    game_requirement: &str,
    base_version: &'static str,
) -> (Vec<u8>, Vec<u8>) {
    let base = dependency_zip(BASE, base_version, &[], "", None, false);
    let game = dependency_zip(
        GAME_PLUGIN,
        game_version,
        &[(BASE, game_requirement)],
        "game:define",
        Some("dep-game"),
        false,
    );
    (base, game)
}

#[tokio::test]
async fn installing_a_game_plugin_installs_its_missing_official_dependency_first() {
    let world = World::new();
    let (base, game) = dependency_releases("1.0.0", ">=1.0, <2", "1.0.0");
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), base),
    ]);

    let plugin = installed(
        world
            .service
            .install_official(GAME_PLUGIN, Consent::without_code(), &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(plugin.id, GAME_PLUGIN);
    assert_eq!(world.version_of(BASE).as_deref(), Some("1.0.0"));
    assert_eq!(world.version_of(GAME_PLUGIN).as_deref(), Some("1.0.0"));
    let target = world
        .service
        .list()
        .into_iter()
        .find(|installed| installed.id == GAME_PLUGIN)
        .expect("the game plugin is installed");
    assert_eq!(
        target.status,
        PluginStatus::Ready,
        "the dependency was installed before the game plugin needed it"
    );
}

#[tokio::test]
async fn consent_is_asked_once_and_lists_the_dependency() {
    let world = World::new();
    let base = dependency_zip(BASE, "1.0.0", &[], "", None, false);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <2")],
        "game:define",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), base),
    ]);

    let outcome = world
        .service
        .install_official(GAME_PLUGIN, false, &world.fetcher)
        .await
        .unwrap();
    let UpdateOutcome::NeedsConsent { preview } = outcome else {
        panic!("expected a consent request");
    };
    assert_eq!(preview.manifest.id.as_str(), GAME_PLUGIN);
    assert_eq!(preview.also_installs.len(), 1);
    assert_eq!(preview.also_installs[0].manifest.id.as_str(), BASE);
    assert!(
        files_under(&world.packages_root()).is_empty(),
        "nothing is installed before consent covers the whole set"
    );

    installed(
        world
            .service
            .install_official(GAME_PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(world.version_of(BASE).as_deref(), Some("1.0.0"));
    assert_eq!(world.version_of(GAME_PLUGIN).as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn a_dependency_not_in_the_signed_catalog_refuses_before_writing_anything() {
    let world = World::new();
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[("acme.missing", ">=4.0, <5")],
        "",
        Some("dep-game"),
        false,
    );
    world.publish(vec![(
        Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
        game,
    )]);

    let error = world
        .service
        .install_official(GAME_PLUGIN, true, &world.fetcher)
        .await
        .expect_err("a missing dependency refuses the install");
    assert_eq!(error.code(), "ERR_OFFICIAL_PLUGIN_DEPENDENCY");
    let message = error.to_string();
    assert!(message.contains("acme.missing"), "{message}");
    assert!(message.contains(">=4.0, <5"), "{message}");
    assert!(
        files_under(&world.packages_root()).is_empty(),
        "nothing was written"
    );
    assert_eq!(world.version_of(GAME_PLUGIN), None);
}

#[tokio::test]
async fn an_installed_dependency_that_satisfies_is_left_alone() {
    let world = World::new();
    let base_v1 = dependency_zip(BASE, "1.0.0", &[], "", None, false);
    world.publish(vec![(
        Release::of_plugin(BASE, "1.0.0", "current", &base_v1),
        base_v1,
    )]);
    installed(
        world
            .service
            .install_official(BASE, true, &world.fetcher)
            .await
            .unwrap(),
    );
    let calls_after_base = world.fetcher.calls();

    // The catalog now offers a newer base, but the installed one already satisfies.
    let base_v2 = dependency_zip(BASE, "2.0.0", &[], "", None, false);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <3")],
        "",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(BASE, "2.0.0", "current", &base_v2),
            base_v2,
        ),
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
    ]);

    installed(
        world
            .service
            .install_official(GAME_PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(
        world.version_of(BASE).as_deref(),
        Some("1.0.0"),
        "the satisfying install is not replaced"
    );
    assert_eq!(
        world.fetcher.calls() - calls_after_base,
        1,
        "only the target package was downloaded"
    );
}

#[tokio::test]
async fn an_outdated_dependency_is_updated_when_the_new_version_needs_it() {
    let world = World::new();
    let base_v1 = dependency_zip(BASE, "1.0.0", &[], "", None, false);
    world.publish(vec![(
        Release::of_plugin(BASE, "1.0.0", "current", &base_v1),
        base_v1,
    )]);
    installed(
        world
            .service
            .install_official(BASE, true, &world.fetcher)
            .await
            .unwrap(),
    );

    let base_v2 = dependency_zip(BASE, "2.0.0", &[], "", None, false);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=2.0, <3")],
        "",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(BASE, "2.0.0", "current", &base_v2),
            base_v2,
        ),
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
    ]);

    installed(
        world
            .service
            .install_official(GAME_PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert_eq!(world.version_of(BASE).as_deref(), Some("2.0.0"));
    assert_eq!(world.version_of(GAME_PLUGIN).as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn a_dependency_hash_mismatch_installs_nothing() {
    let world = World::new();
    let base = dependency_zip(BASE, "1.0.0", &[], "", None, false);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <2")],
        "",
        Some("dep-game"),
        false,
    );
    // Same length as the real package, so only the hash can tell them apart.
    let mut forged = base.clone();
    let last = forged.len() - 1;
    forged[last] ^= 0xFF;
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), forged),
    ]);

    assert_eq!(
        error_code(
            world
                .service
                .install_official(GAME_PLUGIN, true, &world.fetcher)
                .await
        ),
        "ERR_OFFICIAL_PLUGIN_HASH"
    );
    assert!(
        files_under(&world.packages_root()).is_empty(),
        "a dependency that fails its hash installs neither package"
    );
}

#[tokio::test]
async fn a_failure_after_the_dependency_reports_what_was_installed() {
    let world = World::new();
    let base = dependency_zip(BASE, "1.0.0", &[], "", None, false);
    // The target asks for a capability this build cannot provide, so its own
    // install fails after the dependency has already been written.
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <2")],
        "not:a-real:capability",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), base),
    ]);

    let error = world
        .service
        .install_official(GAME_PLUGIN, true, &world.fetcher)
        .await
        .expect_err("the target fails after its dependency");
    assert_eq!(error.code(), "ERR_OFFICIAL_PLUGIN_PARTIAL");
    let message = error.to_string();
    assert!(message.contains(BASE), "{message}");
    assert!(message.contains(GAME_PLUGIN), "{message}");
    assert_eq!(
        world.version_of(BASE).as_deref(),
        Some("1.0.0"),
        "the dependency is kept"
    );
    assert_eq!(
        world.version_of(GAME_PLUGIN),
        None,
        "the target is not installed"
    );
}

#[tokio::test]
async fn a_dependency_that_runs_code_installs_with_its_code_off_when_not_allowed() {
    let world = World::new();
    let base = dependency_zip(BASE, "1.0.0", &[], "", None, true);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <2")],
        "",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), base),
    ]);

    let plugin = installed(
        world
            .service
            .install_official(
                GAME_PLUGIN,
                Consent {
                    capabilities: true,
                    scripts: false,
                },
                &world.fetcher,
            )
            .await
            .unwrap(),
    );
    assert_eq!(plugin.id, GAME_PLUGIN);
    assert_eq!(world.version_of(BASE).as_deref(), Some("1.0.0"));

    let base_plugin = world
        .service
        .list()
        .into_iter()
        .find(|plugin| plugin.id == BASE)
        .expect("the dependency is installed");
    assert!(base_plugin.runs_code, "the dependency does have code");
    assert!(
        !base_plugin.scripts_allowed,
        "the dependency's code stays off until it is allowed"
    );
}

#[tokio::test]
async fn a_code_dependency_still_asks_when_capabilities_are_unanswered() {
    let world = World::new();
    let base = dependency_zip(BASE, "1.0.0", &[], "", None, true);
    let game = dependency_zip(
        GAME_PLUGIN,
        "1.0.0",
        &[(BASE, ">=1.0, <2")],
        "",
        Some("dep-game"),
        false,
    );
    world.publish(vec![
        (
            Release::of_plugin(GAME_PLUGIN, "1.0.0", "current", &game),
            game,
        ),
        (Release::of_plugin(BASE, "1.0.0", "current", &base), base),
    ]);

    let outcome = world
        .service
        .install_official(GAME_PLUGIN, false, &world.fetcher)
        .await
        .unwrap();
    let UpdateOutcome::NeedsConsent { preview } = outcome else {
        panic!("the capability question is still unanswered");
    };
    assert_eq!(preview.also_installs.len(), 1);
    assert!(preview.also_installs[0].runs_code);
    assert!(
        files_under(&world.packages_root()).is_empty(),
        "nothing is installed before the capability answer"
    );
}

// ---------------------------------------------------------------------------
// Red-team fixes
// ---------------------------------------------------------------------------

/// The same two files as `OfficialPluginDigestTests.GOLDEN` in compiler/test_compile.py, whose
/// Python side is `scripts/official_plugin_digest.py`.
const GOLDEN_TREE_DIGEST: &str = "edf96c2334a03f422f09963524f136c27b969682ebead88f3d40865741d830a8";

#[test]
fn the_tree_digest_matches_the_python_script_and_the_unpacked_folder() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("games")).unwrap();
    std::fs::write(dir.path().join("agora-plugin.json"), b"{}").unwrap();
    std::fs::write(dir.path().join("games/package.json"), b"[]").unwrap();
    assert_eq!(tree_digest(dir.path()).unwrap(), GOLDEN_TREE_DIGEST);

    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(b"{}").unwrap();
        zip.start_file("games/package.json", options).unwrap();
        zip.write_all(b"[]").unwrap();
        zip.finish().unwrap();
    }
    assert_eq!(
        zip_tree_digest(&buffer.into_inner()).unwrap(),
        GOLDEN_TREE_DIGEST
    );
}

#[tokio::test]
async fn a_package_unpacks_to_exactly_the_tree_its_digest_promised() {
    let world = World::new();
    let zip = package_zip(PLUGIN, "1.0.0", GAME, "game:define", "extra file");
    world
        .service
        .install_package_bytes(&zip, true)
        .expect("install from memory");
    assert_eq!(
        tree_digest(&world.installed_dir()).unwrap(),
        zip_tree_digest(&zip).unwrap()
    );
}

#[tokio::test]
async fn an_entry_that_pins_the_wrong_tree_digest_is_refused() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    let mut release = Release::of("1.0.0", "current", &zip);
    release.tree_sha256 = "0".repeat(64);
    world.fetcher.serve(&release.url, zip);
    write_registry(world.root(), &[release], &signing_key(), true);

    let code = error_code(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await,
    );

    assert_eq!(code, "ERR_OFFICIAL_PLUGIN_TREE");
    assert!(files_under(&world.paths().plugins_root()).is_empty());
    assert!(world.service.list().is_empty());
}

/// `local_state.db` is not signed. Whatever it says, the folder is checked against the digest the
/// signed entry pins.
#[tokio::test]
async fn local_state_cannot_vouch_for_a_tampered_folder_or_a_rewritten_record() {
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
    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();

    // 1. Edit a file and rewrite the remembered digest to match it.
    let target = world.installed_dir().join("games/package.json");
    let original = std::fs::read(&target).unwrap();
    let original_digest: String = conn
        .query_row(
            "SELECT tree_sha256 FROM plugin_official WHERE plugin_id = ?1",
            [PLUGIN],
            |row| row.get(0),
        )
        .unwrap();
    std::fs::write(&target, b"{\"hijacked\": true}").unwrap();
    let forged = tree_digest(&world.installed_dir()).unwrap();
    conn.execute(
        "UPDATE plugin_official SET tree_sha256 = ?1 WHERE plugin_id = ?2",
        rusqlite::params![forged, PLUGIN],
    )
    .unwrap();
    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("a digest remembered in local state must not vouch for edited files");
    };
    assert!(reason.contains("different files"), "{reason}");
    std::fs::write(&target, &original).unwrap();
    conn.execute(
        "UPDATE plugin_official SET tree_sha256 = ?1 WHERE plugin_id = ?2",
        rusqlite::params![original_digest, PLUGIN],
    )
    .unwrap();
    assert!(world.status().is_official(), "restored files verify again");

    // 2. Rewrite the stored manifest (contributions are read from it) without touching a file.
    let manifest: String = conn
        .query_row(
            "SELECT manifest_json FROM plugin_installs WHERE plugin_id = ?1",
            [PLUGIN],
            |row| row.get(0),
        )
        .unwrap();
    let mut doctored: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    doctored["name"] = serde_json::json!("Something Else");
    conn.execute(
        "UPDATE plugin_installs SET manifest_json = ?1 WHERE plugin_id = ?2",
        rusqlite::params![doctored.to_string(), PLUGIN],
    )
    .unwrap();
    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("a rewritten manifest record must not stay official");
    };
    assert!(reason.contains("manifest"), "{reason}");
    conn.execute(
        "UPDATE plugin_installs SET manifest_json = ?1 WHERE plugin_id = ?2",
        rusqlite::params![manifest, PLUGIN],
    )
    .unwrap();
    assert!(world.status().is_official());

    // 3. Widen the grant beyond what the manifest asks for.
    let granted: String = conn
        .query_row(
            "SELECT granted_capabilities FROM plugin_installs WHERE plugin_id = ?1",
            [PLUGIN],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE plugin_installs SET granted_capabilities = ?1 WHERE plugin_id = ?2",
        rusqlite::params![r#"["game:define","content:write"]"#, PLUGIN],
    )
    .unwrap();
    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("an extra grant must not stay official");
    };
    assert!(reason.contains("content:write"), "{reason}");
    conn.execute(
        "UPDATE plugin_installs SET granted_capabilities = ?1 WHERE plugin_id = ?2",
        rusqlite::params![granted, PLUGIN],
    )
    .unwrap();
    assert!(world.status().is_official());

    // 4. Point the record at an identical copy somewhere else.
    let copy = world.root().join("elsewhere");
    std::fs::create_dir_all(copy.join("games")).unwrap();
    for file in ["agora-plugin.json", "games/package.json"] {
        std::fs::copy(world.installed_dir().join(file), copy.join(file)).unwrap();
    }
    assert_eq!(
        tree_digest(&copy).unwrap(),
        tree_digest(&world.installed_dir()).unwrap()
    );
    let moved = copy.to_string_lossy().to_string();
    let home: String = conn
        .query_row(
            "SELECT install_dir FROM plugin_installs WHERE plugin_id = ?1",
            [PLUGIN],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE plugin_installs SET install_dir = ?1 WHERE plugin_id = ?2",
        rusqlite::params![moved, PLUGIN],
    )
    .unwrap();
    let OfficialStatus::Unverified { reason } = world.status() else {
        panic!("a record pointing outside the installer's folder must not stay official");
    };
    assert!(reason.contains("folder"), "{reason}");
    conn.execute(
        "UPDATE plugin_installs SET install_dir = ?1 WHERE plugin_id = ?2",
        rusqlite::params![home, PLUGIN],
    )
    .unwrap();
    assert!(world.status().is_official());
}

/// The package is inspected and unpacked from the verified bytes in memory. If a staged copy on
/// disk were still involved, an unwritable staging folder would stop the install.
#[tokio::test]
async fn an_official_install_never_stages_the_package_on_disk() {
    let world = World::new();
    let zip = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    let downloads = world.paths().plugin_rollback_root().join("downloads");
    std::fs::create_dir_all(downloads.parent().unwrap()).unwrap();
    std::fs::write(&downloads, b"in the way").unwrap();

    let plugin = installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .expect("an obstructed staging folder must not matter"),
    );

    assert!(plugin.official.is_official());
    assert_eq!(std::fs::read(&downloads).unwrap(), b"in the way");
}

#[tokio::test]
async fn official_plugins_register_before_community_ones() {
    let world = World::new();
    // The official plugin is Skyrim as the real one declares it: `skyrim-se`, Steam 489830.
    let official = package_zip_with(
        PLUGIN,
        "1.0.0",
        game_package_in_steam("skyrim-se", "489830"),
        "game:define",
        "",
    );
    world.publish(vec![(Release::of("1.0.0", "current", &official), official)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    // Two community plugins whose ids sort before the official one: one claims the game id,
    // the other the same Steam product under a different game id.
    for (id, package) in [
        ("aaaa.hijack", game_package_in_steam("skyrim-se", "489830")),
        ("aaab.thief", game_package_in_steam("not-skyrim", "489830")),
    ] {
        world
            .service
            .install_package_bytes(
                &package_zip_with(id, "1.0.0", package, "game:define", ""),
                true,
            )
            .unwrap();
    }
    world.set_emergency_stop(false);

    let (ctx, warnings) = world.start();

    let skyrim = GameId::new("skyrim-se").unwrap();
    assert_eq!(
        ctx.games.source_for(&skyrim),
        Some(&agora_core::game_registry::PackageSource::Plugin {
            plugin_id: PLUGIN.into()
        }),
        "the official plugin must own skyrim-se; warnings: {warnings:?}"
    );
    assert!(ctx
        .games
        .game(&GameId::new("not-skyrim").unwrap())
        .is_none());
    for community in ["aaaa.hijack", "aaab.thief"] {
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(community) && w.contains("rejected")),
            "the user must be told {community} lost: {warnings:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Two script switches
// ---------------------------------------------------------------------------

/// A plugin with a script that starts at launch, so whether its runtime is running shows which
/// switch let it.
fn script_zip(id: &str, version: &str) -> Vec<u8> {
    use std::io::Write;
    let manifest = format!(
        r#"{{
        "manifest": 1,
        "id": "{id}",
        "name": "Scripted",
        "version": "{version}",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js",
        "activation": ["onStartup"],
        "capabilities": {{ "required": ["instance:read"] }}
    }}"#
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("main.js", options).unwrap();
        zip.write_all(b"export function noop() { return 1; }\n")
            .unwrap();
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

/// The script package above with a chosen capability, so an update can add
/// code without adding a permission.
fn script_zip_requiring(id: &str, version: &str, capability: &str) -> Vec<u8> {
    use std::io::Write;
    let manifest = format!(
        r#"{{
        "manifest": 1,
        "id": "{id}",
        "name": "Scripted",
        "version": "{version}",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js",
        "activation": ["onStartup"],
        "capabilities": {{ "required": ["{capability}"] }}
    }}"#
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("main.js", options).unwrap();
        zip.write_all(b"export function noop() { return 1; }\n")
            .unwrap();
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

/// A publisher key made from raw bytes, for update-source fixtures.
fn publisher_key(seed: u8) -> PublicKey {
    use base64::Engine as _;
    PublicKey {
        id: format!("key-{seed}"),
        algorithm: "ed25519".into(),
        public_key: base64::engine::general_purpose::STANDARD.encode([seed; 32]),
    }
}

/// The script package above, plus an update source signed by `key`.
fn script_zip_with_source(id: &str, version: &str, key: &PublicKey) -> Vec<u8> {
    use std::io::Write;
    let manifest = format!(
        r#"{{
        "manifest": 1,
        "id": "{id}",
        "name": "Scripted",
        "version": "{version}",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js",
        "activation": ["onStartup"],
        "capabilities": {{ "required": ["instance:read"] }}
    }}"#
    );
    let source = serde_json::json!({
        "schema": 1,
        "url": "https://plugins.example.org/updates.json",
        "keys": [key],
    });
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("main.js", options).unwrap();
        zip.write_all(b"export function noop() { return 1; }\n")
            .unwrap();
        zip.start_file(UPDATE_SOURCE_FILENAME, options).unwrap();
        zip.write_all(source.to_string().as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

/// A script package with extra files beside `main.js`, e.g. an imported
/// module and a data file.
fn script_zip_with_files(id: &str, version: &str, files: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let manifest = format!(
        r#"{{
        "manifest": 1,
        "id": "{id}",
        "name": "Scripted",
        "version": "{version}",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js",
        "activation": ["onStartup"],
        "capabilities": {{ "required": ["instance:read"] }}
    }}"#
    );
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("agora-plugin.json", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file("main.js", options).unwrap();
        zip.write_all(
            b"export function noop() { return 1; }
",
        )
        .unwrap();
        for (name, body) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

const COMMUNITY_SCRIPT: &str = "acme.community";

impl World {
    /// Whether this plugin's script runtime is up.
    fn running(&self, id: &str) -> bool {
        self.service
            .list()
            .into_iter()
            .find(|plugin| plugin.id == id)
            .unwrap_or_else(|| panic!("{id} is not installed"))
            .running
    }

    fn stop_everything(&self) {
        for plugin in self.service.list() {
            self.service
                .deactivate(&PluginId::parse(&plugin.id).unwrap())
                .unwrap();
        }
    }
}

const SCRIPTED_B: &str = "acme.second";

/// An official plugin whose fixture ships no code, and two community script
/// plugins, all installed with their code allowed.
async fn world_with_script_plugins() -> World {
    let world = World::new();
    let official = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &official), official)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    for id in [COMMUNITY_SCRIPT, SCRIPTED_B] {
        world
            .service
            .install_package_bytes(&script_zip(id, "1.0.0"), true)
            .unwrap();
    }
    assert!(world.status().is_official());
    world
}

fn plugin_id(id: &str) -> PluginId {
    PluginId::parse(id).unwrap()
}

#[tokio::test]
async fn the_emergency_stop_is_not_engaged_by_default_and_the_old_key_means_nothing() {
    let world = World::new();
    assert!(!world.service.emergency_stop_engaged());
    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();
    assert!(
        agora_core::db::get_setting(&conn, PLUGINS_EMERGENCY_STOP_SETTING)
            .unwrap()
            .is_none(),
        "not seeded"
    );
    // The old key meant the opposite and was seeded `false` everywhere. It is ignored: it
    // neither engages nor releases the stop, whichever way it is set.
    for old in [false, true] {
        agora_core::db::set_setting(&conn, "plugins_enabled", &serde_json::json!(old)).unwrap();
        assert!(
            !world.service.emergency_stop_engaged(),
            "plugins_enabled = {old}"
        );
    }
    // A stored value that is not a boolean cannot be told from "engage it": fail closed.
    agora_core::db::set_setting(
        &conn,
        PLUGINS_EMERGENCY_STOP_SETTING,
        &serde_json::json!("yes"),
    )
    .unwrap();
    assert!(world.service.emergency_stop_engaged());
}

#[tokio::test]
async fn a_scripted_plugin_installed_without_allowing_code_runs_no_script() {
    let world = World::new();
    let zip = script_zip(COMMUNITY_SCRIPT, "1.0.0");

    let preview = world.service.preview_package_bytes(&zip).unwrap();
    assert!(preview.runs_code && preview.adds_code);

    let plugin = world
        .service
        .install_package_bytes(&zip, Consent::without_code())
        .unwrap();
    assert!(plugin.runs_code);
    assert!(!plugin.scripts_allowed);

    let failures = world.service.activate_all().unwrap();
    assert!(
        failures.is_empty(),
        "a denied plugin is skipped, not failed"
    );
    assert!(!world.running(COMMUNITY_SCRIPT));
    assert!(world
        .service
        .activate(&plugin_id(COMMUNITY_SCRIPT))
        .is_err());
    assert!(!world
        .service
        .scripts_permitted(&plugin_id(COMMUNITY_SCRIPT)));

    // The choice is recorded in local state, not the manifest, and can change later.
    let allowed = world
        .service
        .set_scripts_allowed(&plugin_id(COMMUNITY_SCRIPT), true)
        .unwrap();
    assert!(allowed.scripts_allowed);
    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));
    world.stop_everything();
}

#[tokio::test]
async fn an_allowed_scripted_plugin_runs_and_turning_it_off_stops_it() {
    let world = World::new();
    world
        .service
        .install_package_bytes(&script_zip(COMMUNITY_SCRIPT, "1.0.0"), true)
        .unwrap();
    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));

    let off = world
        .service
        .set_scripts_allowed(&plugin_id(COMMUNITY_SCRIPT), false)
        .unwrap();
    assert!(!off.scripts_allowed);
    assert!(
        !world.running(COMMUNITY_SCRIPT),
        "stopped now, not at the next start"
    );
    world.service.activate_all().unwrap();
    assert!(!world.running(COMMUNITY_SCRIPT));
}

#[tokio::test]
async fn the_emergency_stop_stops_every_script_and_data_only_plugins_still_load() {
    let world = world_with_script_plugins().await;
    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));
    assert!(world.running(SCRIPTED_B));

    world.service.set_emergency_stop(true).unwrap();

    assert!(world.service.emergency_stop_engaged());
    assert!(!world.running(COMMUNITY_SCRIPT));
    assert!(!world.running(SCRIPTED_B));
    world.service.activate_all().unwrap();
    assert!(!world.running(COMMUNITY_SCRIPT));
    assert!(!world.running(SCRIPTED_B));
    assert!(world
        .service
        .activate(&plugin_id(COMMUNITY_SCRIPT))
        .is_err());
    assert!(world.service.contributions().is_empty());

    // Data-only plugins run no code, so the stop does not take their games away.
    let (ctx, _) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());

    // Releasing it lets what each plugin was allowed to do run again.
    world.service.set_emergency_stop(false).unwrap();
    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));
    assert!(world.running(SCRIPTED_B));
    world.stop_everything();
}

#[tokio::test]
async fn one_plugins_permission_does_not_affect_another() {
    const SCRIPTED_C: &str = "acme.third";
    let world = world_with_script_plugins().await;
    world
        .service
        .install_package_bytes(&script_zip(SCRIPTED_C, "1.0.0"), true)
        .unwrap();

    world
        .service
        .set_scripts_allowed(&plugin_id(COMMUNITY_SCRIPT), false)
        .unwrap();
    world.service.activate_all().unwrap();
    assert!(!world.running(COMMUNITY_SCRIPT));
    assert!(
        world.running(SCRIPTED_B),
        "another plugin is its own decision"
    );
    assert!(world.running(SCRIPTED_C));

    // And the other way: stopping one leaves the others alone.
    world.stop_everything();
    world
        .service
        .set_scripts_allowed(&plugin_id(COMMUNITY_SCRIPT), true)
        .unwrap();
    world
        .service
        .set_scripts_allowed(&plugin_id(SCRIPTED_B), false)
        .unwrap();
    world.service.activate_all().unwrap();
    assert!(!world.running(SCRIPTED_B));
    assert!(world.running(COMMUNITY_SCRIPT));
    assert!(world.running(SCRIPTED_C));
    world.stop_everything();
}

#[tokio::test]
async fn an_official_plugin_with_a_script_installs_when_its_hash_matches() {
    let world = World::new();
    let zip = script_zip(PLUGIN, "1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);

    // Official plugins may carry an entrypoint. The signed entry pins the whole
    // package tree, so the script is covered by the same hash as the data.
    // Installing it is the first decision; running its code is the second.
    let plugin = installed(
        world
            .service
            .install_official(PLUGIN, Consent::without_code(), &world.fetcher)
            .await
            .expect("an official package with a matching hash installs"),
    );
    assert!(plugin.runs_code);
    assert!(!plugin.scripts_allowed, "the script was not allowed yet");
    assert!(matches!(world.status(), OfficialStatus::Official { .. }));
}

#[tokio::test]
async fn official_plugins_get_no_extra_capability() {
    let zip = script_zip(PLUGIN, "1.0.0");

    let official_world = World::new();
    official_world.publish(vec![(Release::of("1.0.0", "current", &zip), zip.clone())]);
    let official = installed(
        official_world
            .service
            .install_official(PLUGIN, true, &official_world.fetcher)
            .await
            .unwrap(),
    );

    let community_world = World::new();
    let community = community_world
        .service
        .install_package_bytes(&zip, true)
        .unwrap();

    // The same bytes, so the same request. Official status changes where the
    // package came from, not what it may ask for.
    assert_eq!(official.capabilities, community.capabilities);
    assert_eq!(official.capabilities, vec!["instance:read".to_string()]);
}

#[tokio::test]
async fn an_official_script_changed_after_pinning_is_refused() {
    let world = World::new();
    let zip = script_zip(PLUGIN, "1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &zip), zip)]);
    installed(
        world
            .service
            .install_official(PLUGIN, Consent::without_code(), &world.fetcher)
            .await
            .unwrap(),
    );

    // The pinned tree covers `main.js`, so editing it loses official standing
    // and its game packages stop loading.
    std::fs::write(
        world.installed_dir().join("main.js"),
        b"export function noop() { return 2; }
",
    )
    .unwrap();
    world.service.reload().unwrap();
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
}

#[tokio::test]
async fn a_data_only_plugin_needs_no_script_consent() {
    let world = World::new();
    let plugin = world
        .service
        .install_package_bytes(&fixture_zip("1.0.0"), Consent::without_code())
        .unwrap();
    assert!(!plugin.runs_code);
    assert!(!plugin.scripts_allowed, "there is nothing to allow");
    assert!(world.service.scripts_permitted(&World::id()));
    assert!(world
        .service
        .set_scripts_allowed(&World::id(), true)
        .is_err());
    world.set_emergency_stop(true);
    let (ctx, _) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());
}

#[tokio::test]
async fn an_update_keeps_the_answer_and_a_new_install_over_an_uninstall_asks_again() {
    let world = World::new();
    world
        .service
        .install_package_bytes(
            &script_zip(COMMUNITY_SCRIPT, "1.0.0"),
            Consent::without_code(),
        )
        .unwrap();
    // An update does not inherit permission just because its install said yes.
    let updated = world
        .service
        .install_package_bytes(&script_zip(COMMUNITY_SCRIPT, "1.1.0"), true)
        .unwrap();
    assert!(!updated.scripts_allowed, "the earlier answer (no) stands");

    // Uninstalling ends the answer with the files; a reinstall is a new decision.
    world
        .service
        .set_scripts_allowed(&plugin_id(COMMUNITY_SCRIPT), true)
        .unwrap();
    world
        .service
        .uninstall(&plugin_id(COMMUNITY_SCRIPT), false)
        .unwrap();
    let again = world
        .service
        .install_package_bytes(
            &script_zip(COMMUNITY_SCRIPT, "1.1.0"),
            Consent::without_code(),
        )
        .unwrap();
    assert!(!again.scripts_allowed);
}

/// A blocked (edited) official plugin's game does not load, while community scripts are
/// unaffected.
#[tokio::test]
async fn a_blocked_official_plugin_does_not_load_its_game() {
    let world = world_with_script_plugins().await;
    std::fs::write(world.installed_dir().join("extra.txt"), b"edited").unwrap();
    world.service.reload().unwrap();
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));

    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));

    let (ctx, warnings) = world.start();
    assert!(warnings.iter().any(|w| w.contains("blocked")));
    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_none(),
        "a blocked official plugin's game is not loaded"
    );
    world.stop_everything();
}

/// A community plugin that borrows an official plugin's id is blocked by the signed catalog,
/// and what is running of it stops at the next re-verification, not the next launch.
#[tokio::test]
async fn a_community_plugin_squatting_an_official_id_stops_at_the_next_reload() {
    let world = World::new();
    // No official catalog yet: this is an ordinary community plugin.
    world
        .service
        .install_package_bytes(&script_zip(PLUGIN, "1.0.0"), true)
        .unwrap();
    world
        .service
        .install_package_bytes(&script_zip(COMMUNITY_SCRIPT, "1.0.0"), true)
        .unwrap();
    world.service.activate_all().unwrap();
    assert!(world.running(PLUGIN));
    assert!(world.running(COMMUNITY_SCRIPT));

    // The signed catalog now gives that id to an official package the installed
    // files do not match, so the install loses its standing.
    let signed = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &signed), signed)]);
    world.service.reload().unwrap();

    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
    assert!(!world.running(PLUGIN), "the squatter stops now");
    assert!(
        world.running(COMMUNITY_SCRIPT),
        "only the blocked one stops"
    );
    world.service.activate_all().unwrap();
    assert!(!world.running(PLUGIN));
    world.stop_everything();
}

// ---------------------------------------------------------------------------
// Registration order
// ---------------------------------------------------------------------------

/// Official plugins register their games before community ones, even when the community
/// plugin's id sorts first. Otherwise a package named `aaa.community` could claim a game id
/// the signed registry gives to the official plugin.
#[tokio::test]
async fn an_installed_official_plugin_wins_a_game_id_over_a_community_plugin() {
    let world = World::new();
    let official = fixture_zip("1.0.0");
    world.publish(vec![(Release::of("1.0.0", "current", &official), official)]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // A different package id (so the packages themselves do not collide) that claims the
    // same game, under an id that sorts before `acme.fixture`.
    let mut package = game_package(GAME);
    package.id = "test.pkg.community".into();
    let community = package_zip_with("aaa.community", "1.0.0", package, "game:define", "");
    world
        .service
        .install_package_bytes(&community, true)
        .unwrap();

    let (ctx, warnings) = world.start();
    let game = GameId::new(GAME).unwrap();
    assert!(ctx.games.game(&game).is_some(), "{warnings:?}");
    assert_eq!(
        ctx.games.source_for(&game),
        Some(&PackageSource::Plugin {
            plugin_id: PLUGIN.into()
        }),
        "the official package must have registered first; warnings: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("rejected game package")),
        "the community claim must be named, not silently dropped: {warnings:?}"
    );
}

/// The documented development path: install `packages/official/skyrim-se` as a folder, with no
/// signed entry and no plugin switch, and the game still appears.
#[tokio::test]
async fn the_documented_development_install_of_skyrim_still_loads_the_game() {
    let world = World::new();
    let official = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/official");
    // Skyrim inherits the Creation Engine base, which is its own plugin.
    world
        .service
        .add_development_folder(&official.join("creation-engine"), true)
        .unwrap();
    world
        .service
        .add_development_folder(&official.join("skyrim-se"), true)
        .unwrap();

    let (ctx, warnings) = world.start();
    let game = GameId::new("skyrim-se").unwrap();
    assert!(ctx.games.game(&game).is_some(), "{warnings:?}");
    assert_eq!(
        ctx.games.source_for(&game),
        Some(&PackageSource::Plugin {
            plugin_id: "agora.skyrim-se".into()
        })
    );
}

// ---------------------------------------------------------------------------
// Final review (plugins): proven bugs, ignored so the suite stays green
// ---------------------------------------------------------------------------

/// The doc comment on `install_package_bytes` says "a version that newly has code ... is denied
/// until the user allows it". The answer is only cleared by an uninstall, so a version with no
/// code in between does not end it: v1 (code, allowed) -> v2 (no code) -> v3 (new code) runs v3
/// with the permission given for v1.
#[tokio::test]
async fn script_consent_does_not_survive_a_version_with_no_code() {
    let world = World::new();
    let v1 = world
        .service
        .install_package_bytes(&script_zip(COMMUNITY_SCRIPT, "1.0.0"), true)
        .unwrap();
    assert!(v1.scripts_allowed, "the user allowed v1's code");

    // v2 ships no code at all (a data-only game package under the same id).
    let v2 = world
        .service
        .install_package_bytes(
            &package_zip(COMMUNITY_SCRIPT, "2.0.0", "some-game", "game:define", ""),
            Consent::without_code(),
        )
        .unwrap();
    assert!(!v2.runs_code);

    // v3 brings code back. The user was never asked about this code.
    let v3 = world
        .service
        .install_package_bytes(
            &script_zip(COMMUNITY_SCRIPT, "3.0.0"),
            Consent::without_code(),
        )
        .unwrap();
    assert!(v3.runs_code);
    assert!(
        !v3.scripts_allowed,
        "v3's code is new: it must not run on the answer given for v1"
    );
    world.stop_everything();
    world.service.activate_all().unwrap();
    assert!(
        !world.running(COMMUNITY_SCRIPT),
        "v3 ran without being allowed"
    );
}

/// A tampered or revoked official plugin is Blocked and its games are not loaded; the game id it
/// held is then free, and any community plugin that declares the same game id takes it over.
#[tokio::test]
async fn a_blocked_official_plugin_does_not_hand_its_game_id_to_a_community_plugin() {
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
    // Someone edits the installed official files: it is demoted and Blocked.
    std::fs::write(world.installed_dir().join("extra.txt"), b"edited").unwrap();
    world.service.reload().unwrap();
    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));

    // A community plugin declares the official plugin's game.
    let mut package = game_package(GAME);
    package.id = "test.pkg.community".into();
    world
        .service
        .install_package_bytes(
            &package_zip_with("aaa.community", "1.0.0", package, "game:define", ""),
            true,
        )
        .unwrap();

    let (ctx, warnings) = world.start();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains(PLUGIN) && w.contains("blocked")),
        "the official plugin is reported blocked: {warnings:?}"
    );
    let source = ctx.games.source_for(&GameId::new(GAME).unwrap());
    assert_ne!(
        source,
        Some(&PackageSource::Plugin {
            plugin_id: "aaa.community".into()
        }),
        "the community plugin must not take over the game of a blocked official plugin"
    );
}

#[tokio::test]
async fn engaging_the_stop_stops_scripts_even_when_saving_the_setting_fails() {
    let world = world_with_script_plugins().await;
    world.service.activate_all().unwrap();
    assert!(world.running(COMMUNITY_SCRIPT));

    // Make the setting write fail, as a full or read-only database would.
    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_stop_write BEFORE INSERT ON user_settings
         BEGIN SELECT RAISE(ABORT, 'refused for the test'); END;",
    )
    .unwrap();

    let error = world
        .service
        .set_emergency_stop(true)
        .expect_err("the save failure is reported");
    assert_eq!(error.code(), "ERR_LOCAL_STATE_FAILED");
    assert!(
        !world.running(COMMUNITY_SCRIPT),
        "the script stopped even though the setting was not saved"
    );
    assert!(
        !world.service.emergency_stop_engaged(),
        "the setting did not persist, which the caller was told"
    );
}

#[tokio::test]
async fn a_differently_signed_update_is_flagged_and_asks_about_its_code_again() {
    let world = World::new();
    let first = publisher_key(7);
    let second = publisher_key(8);

    let installed = world
        .service
        .install_package_bytes(
            &script_zip_with_source(COMMUNITY_SCRIPT, "1.0.0", &first),
            true,
        )
        .unwrap();
    assert!(installed.scripts_allowed);

    let preview = world
        .service
        .preview_package_bytes(&script_zip_with_source(COMMUNITY_SCRIPT, "2.0.0", &second))
        .unwrap();
    assert!(
        preview.publisher_changed,
        "the preview says this is a different publisher"
    );
    assert!(
        preview.requires_capability_consent(),
        "so the user is asked, even though nothing else widened"
    );

    // Without an explicit yes to that preview it is refused.
    let refused = world
        .service
        .install_package_bytes(
            &script_zip_with_source(COMMUNITY_SCRIPT, "2.0.0", &second),
            Consent {
                capabilities: false,
                scripts: false,
            },
        )
        .expect_err("a different publisher needs an explicit yes");
    assert_eq!(refused.code(), "ERR_PLUGIN_PUBLISHER_CHANGED");

    // With the yes, the code question is asked again: v1's answer does not
    // carry over to the new publisher's code.
    let updated = world
        .service
        .install_package_bytes(
            &script_zip_with_source(COMMUNITY_SCRIPT, "2.0.0", &second),
            Consent {
                capabilities: true,
                scripts: false,
            },
        )
        .unwrap();
    assert!(
        !updated.scripts_allowed,
        "the old permission belonged to the old publisher"
    );
    assert_eq!(
        updated
            .update_source
            .as_ref()
            .unwrap()
            .keys
            .iter()
            .map(|key| key.fingerprint.clone())
            .collect::<Vec<_>>(),
        vec![second.fingerprint()],
        "the new key is what is pinned now"
    );
}

#[tokio::test]
async fn a_crash_mid_install_restores_the_previous_version_at_startup() {
    let world = World::new();
    world
        .service
        .install_package_bytes(&fixture_zip("1.0.0"), true)
        .unwrap();

    // The half-done state: the old files are in the rollback folder, the new
    // ones are unpacked, but the record still names v1.
    let installed = world.installed_dir();
    let rollback = world.paths().plugin_rollback_root().join(PLUGIN);
    std::fs::create_dir_all(rollback.parent().unwrap()).unwrap();
    std::fs::rename(&installed, &rollback).unwrap();
    agora_core::plugins::install::extract_package(&fixture_zip("2.0.0"), &installed).unwrap();

    let (ctx, warnings) = world.start();
    assert!(
        warnings.iter().any(|w| w.contains("Recovered")),
        "the recovery is reported: {warnings:?}"
    );
    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
        "the game is back"
    );
    assert_eq!(
        agora_core::plugins::install::read_folder_manifest(&installed)
            .unwrap()
            .version
            .to_string(),
        "1.0.0",
        "the previous version is the one on disk"
    );
    assert!(!rollback.exists(), "the rollback folder is cleaned up");

    // Running it again does nothing.
    let (_ctx, warnings) = world.start();
    assert!(!warnings.iter().any(|w| w.contains("Recovered")));
}

#[tokio::test]
async fn a_finished_install_left_in_the_rollback_folder_is_just_cleaned_up() {
    let world = World::new();
    world
        .service
        .install_package_bytes(&fixture_zip("1.0.0"), true)
        .unwrap();
    world
        .service
        .install_package_bytes(&fixture_zip("2.0.0"), true)
        .unwrap();

    // The record says v2 and v2 is on disk; only the stashed v1 is left.
    let rollback = world.paths().plugin_rollback_root().join(PLUGIN);
    std::fs::create_dir_all(rollback.parent().unwrap()).unwrap();
    agora_core::plugins::install::extract_package(&fixture_zip("1.0.0"), &rollback).unwrap();

    let (ctx, _) = world.start();
    assert!(!rollback.exists());
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());
    assert_eq!(
        agora_core::plugins::install::read_folder_manifest(&world.installed_dir())
            .unwrap()
            .version
            .to_string(),
        "2.0.0",
        "the finished install is kept"
    );
}

// ---------------------------------------------------------------------------
// Catalog standing: load what was verified, warn when it cannot be checked
// ---------------------------------------------------------------------------

/// A missing or corrupt catalog must not make an installed game vanish.
#[tokio::test]
async fn an_unverifiable_catalog_keeps_a_data_only_official_plugin_loading() {
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

    std::fs::write(world.root().join("registry.db.sig"), [0u8; 64]).unwrap();
    assert!(matches!(
        world.status(),
        OfficialStatus::CatalogUnavailable { .. }
    ));

    let (ctx, warnings) = world.start();
    assert!(
        ctx.games.game(&GameId::new(GAME).unwrap()).is_some(),
        "the game still loads from the installed files"
    );
    assert!(
        warnings.iter().any(|w| w.contains("could not check")),
        "and the warning says why: {warnings:?}"
    );
}

/// Being two releases behind hides the game today; it must keep loading with a warning instead.
#[tokio::test]
async fn a_version_no_longer_listed_keeps_a_data_only_official_plugin_loading() {
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

    let newer = fixture_zip("2.0.0");
    world.publish(vec![(Release::of("2.0.0", "current", &newer), newer)]);
    assert!(matches!(world.status(), OfficialStatus::NotListed { .. }));

    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_some());
    assert!(
        warnings.iter().any(|w| w.contains("no longer listed")),
        "the warning says why: {warnings:?}"
    );
}

/// An explicit revocation is different from simply not listing an old version.
#[tokio::test]
async fn an_explicitly_revoked_official_version_is_blocked() {
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

    let signed = fixture_zip("1.0.0");
    let mut revoked = Release::of("1.0.0", "current", &signed);
    revoked.revoked = true;
    world.publish(vec![(revoked, signed)]);

    assert!(matches!(world.status(), OfficialStatus::Unverified { .. }));
    let (ctx, warnings) = world.start();
    assert!(ctx.games.game(&GameId::new(GAME).unwrap()).is_none());
    assert!(warnings.iter().any(|w| w.contains("revokes")));
}

/// In the "catalog could not be checked" state a plugin that would otherwise have scripts keeps
/// loading its data but runs no script: consent alone is not a verifiable source.
#[tokio::test]
async fn an_unverified_official_plugin_with_scripts_does_not_run_them() {
    let world = World::new();
    let scripted = script_zip(PLUGIN, "1.0.0");
    world
        .service
        .install_package_bytes(&scripted, true)
        .unwrap();

    // A legacy install from before official plugins could carry scripts, which
    // the catalog vouched for. Record that standing by hand: the test is
    // about what happens when the catalog can no longer be read.
    let conn = agora_core::db::local_state_connection(&world.paths().local_state_db()).unwrap();
    agora_core::plugins::store::set_official(
        &conn,
        &World::id(),
        &agora_core::plugins::store::OfficialProvenance {
            entry_id: PLUGIN.into(),
            entry_version: "1.0.0".into(),
            entry_sha256: "0".repeat(64),
            tree_sha256: tree_digest(&world.installed_dir()).unwrap(),
            recorded_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    drop(conn);

    std::fs::write(world.root().join("registry.db.sig"), [0u8; 64]).unwrap();
    world.service.reload().unwrap();
    assert!(matches!(
        world.status(),
        OfficialStatus::CatalogUnavailable { .. }
    ));
    assert!(
        !world.service.scripts_permitted(&World::id()),
        "no script without a verifiable catalog"
    );

    world.service.activate_all().unwrap();
    assert!(!world.running(PLUGIN));
}

/// Consent is tied to every file the host can load as a module, not just the
/// entrypoint: changing an imported module is changing the code.
#[tokio::test]
async fn an_update_that_changes_only_an_imported_module_asks_again() {
    let world = World::new();
    let first = script_zip_with_files(
        COMMUNITY_SCRIPT,
        "1.0.0",
        &[
            ("util.js", "export const value = 1;\n"),
            ("data.json", "{\"n\":1}\n"),
        ],
    );
    let installed = world.service.install_package_bytes(&first, true).unwrap();
    assert!(installed.scripts_allowed);

    // The same files under a new version are the same code: the answer stands.
    let same_files = script_zip_with_files(
        COMMUNITY_SCRIPT,
        "1.1.0",
        &[
            ("util.js", "export const value = 1;\n"),
            ("data.json", "{\"n\":1}\n"),
        ],
    );
    let kept = world
        .service
        .install_package_bytes(
            &same_files,
            Consent {
                capabilities: true,
                scripts: false,
            },
        )
        .unwrap();
    assert!(
        kept.scripts_allowed,
        "unchanged files keep the earlier answer"
    );

    // main.js and the data file are untouched; only the imported module changed.
    let second = script_zip_with_files(
        COMMUNITY_SCRIPT,
        "2.0.0",
        &[
            ("util.js", "export const value = 2;\n"),
            ("data.json", "{\"n\":1}\n"),
        ],
    );
    let preview = world.service.preview_package_bytes(&second).unwrap();
    assert!(
        preview.code_changed,
        "a changed imported module is new code"
    );
    let updated = world
        .service
        .install_package_bytes(
            &second,
            Consent {
                capabilities: true,
                scripts: false,
            },
        )
        .unwrap();
    assert!(
        !updated.scripts_allowed,
        "the earlier allow was for different code"
    );
}

/// We chose the strict line: the module loader will parse any file it is asked
/// to import, so every file in the package (except the host metadata) is part
/// of the identity. A data file change therefore asks again too.
#[tokio::test]
async fn an_update_that_changes_only_a_data_file_is_new_code_too() {
    let world = World::new();
    let first = script_zip_with_files(
        COMMUNITY_SCRIPT,
        "1.0.0",
        &[
            ("util.js", "export const value = 1;\n"),
            ("data.json", "{\"n\":1}\n"),
        ],
    );
    world.service.install_package_bytes(&first, true).unwrap();

    let second = script_zip_with_files(
        COMMUNITY_SCRIPT,
        "2.0.0",
        &[
            ("util.js", "export const value = 1;\n"),
            ("data.json", "{\"n\":2}\n"),
        ],
    );
    let preview = world.service.preview_package_bytes(&second).unwrap();
    assert!(
        preview.code_changed,
        "every package file is in the code identity, so data changes re-ask"
    );
    let updated = world
        .service
        .install_package_bytes(
            &second,
            Consent {
                capabilities: true,
                scripts: false,
            },
        )
        .unwrap();
    assert!(!updated.scripts_allowed);
}

#[tokio::test]
async fn an_official_update_that_adds_a_script_asks_again_with_no_new_capability() {
    let world = World::new();
    let data_only = fixture_zip("1.0.0");
    world.publish(vec![(
        Release::of("1.0.0", "current", &data_only),
        data_only,
    )]);
    installed(
        world
            .service
            .install_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );

    // v1.1 adds an entrypoint and asks for exactly what v1.0 already had.
    let scripted = script_zip_requiring(PLUGIN, "1.1.0", "game:define");
    world.publish(vec![(Release::of("1.1.0", "current", &scripted), scripted)]);

    let outcome = world
        .service
        .update_official(
            PLUGIN,
            Consent {
                capabilities: true,
                scripts: false,
            },
            &world.fetcher,
        )
        .await
        .unwrap();
    match outcome {
        UpdateOutcome::NeedsConsent { preview } => {
            assert!(preview.adds_code, "the prompt knows it now runs code");
            assert!(preview.added_capabilities.is_empty(), "no new capability");
            assert!(preview.manifest.entrypoint.is_some());
        }
        UpdateOutcome::Installed { .. } => panic!("adding a script must ask again"),
    }

    // Answering yes installs it and the code is allowed.
    let installed = installed(
        world
            .service
            .update_official(PLUGIN, true, &world.fetcher)
            .await
            .unwrap(),
    );
    assert!(installed.runs_code);
    assert!(installed.scripts_allowed);
}

#[test]
fn a_package_with_duplicate_entries_hashes_what_it_extracts() {
    // The `zip` writer refuses duplicate names, so build the bytes directly.
    // A malicious package is not constrained by a writer.
    fn duplicate_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut crc = flate2::Crc::new();
            crc.update(data);
            crc.sum()
        }
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let sum = crc(data);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&sum.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&sum.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_start = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    let first = b"export function noop() { return 1; }\n";
    let last = b"export function noop() { return 2; }\n";
    let bytes = duplicate_zip(&[("main.js", first), ("main.js", last)]);
    let pinned = zip_tree_digest(&bytes).expect("the pinned hash reads both entries");

    let tmp = tempfile::tempdir().unwrap();
    let destination = tmp.path().join("unpacked");
    agora_core::plugins::install::extract_package(&bytes, &destination).unwrap();
    assert_eq!(
        tree_digest(&destination).unwrap(),
        pinned,
        "the pinned hash describes exactly what unpacking wrote"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("main.js")).unwrap(),
        String::from_utf8_lossy(last)
    );
}
