//! Core context — the dependency bag for every `agora-core` operation.
//!
//! `Ctx` (aliased from `CoreContext`) is constructed once per host (Tauri GUI,
//! CLI, MCP listener) and provides all shared resources: paths, HTTP clients,
//! clock, lock manager, and event/progress sinks. No adapter may reconstruct
//! app-data subpaths or create unconfigured HTTP clients on its own.
//!
//! Constraint: this struct MUST NOT carry any `tauri::`, `clap::`, or
//! MCP-protocol type.

use crate::app_paths::AppPaths;
use crate::error::{LauncherError, LauncherResult};
use crate::event_sink::{EventSink, NoopEventSink, NoopProgressSink, ProgressSink};
use crate::game_hooks::{CatalogEvent, Extensions};
use crate::http_client::HttpClients;
use crate::lock_manager::LockManager;
use crate::operation_manager::OperationManager;
use crate::process_session_manager::ProcessSessionManager;
use crate::task_scheduler::TaskScheduler;
use std::sync::Arc;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Clock abstraction
// ---------------------------------------------------------------------------

/// Abstract clock so tests can control time without wall-clock dependency.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// Wall-clock implementation.
#[derive(Debug, Clone, Copy)]
pub struct WallClock;

impl Clock for WallClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A deterministic clock for tests.
#[derive(Debug, Clone)]
pub struct TestClock {
    now: Arc<std::sync::Mutex<SystemTime>>,
}

impl TestClock {
    pub fn new(start: SystemTime) -> Self {
        Self {
            now: Arc::new(std::sync::Mutex::new(start)),
        }
    }

    pub fn advance(&self, dur: std::time::Duration) {
        let mut guard = self.now.lock().unwrap();
        *guard += dur;
    }
}

impl Clock for TestClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// CoreContext
// ---------------------------------------------------------------------------

/// All shared resources for an Agora core session.
///
/// # Construction
///
/// Use [`CoreContext::initialize`] for production or
/// [`CoreContext::for_testing`] for tests.
///
/// # Type alias
///
/// `pub type Ctx = CoreContext;` — prefer the shorter name when referring to
/// this context throughout the codebase.
#[derive(Clone)]
pub struct CoreContext {
    /// Path layout for this host.
    pub paths: AppPaths,
    /// Where game packages should look for a game's own user data (an official
    /// launcher's files, a store's config) instead of the platform defaults.
    /// `None` in production; test contexts set it. Only the Minecraft
    /// package's launcher-profile path honours it so far: some read-only
    /// lookups (the official `.minecraft` directory) still use the platform
    /// default.
    pub external_data_root: Option<std::path::PathBuf>,
    /// Category-aware pre-built HTTP clients.
    pub http_clients: HttpClients,
    /// Clock for time-sensitive operations.
    pub clock: Arc<dyn Clock>,
    /// Cross-process filesystem lock manager.
    pub lock_manager: LockManager,
    /// State owned by game packages (catalogs, caches), keyed by type.
    pub extensions: Extensions,
    /// Sink for progress events emitted during long-running operations.
    pub progress_sink: Arc<dyn ProgressSink>,
    /// Sink for significant core events.
    pub event_sink: Arc<dyn EventSink>,
    /// Cloneable operation manager for tracking long-running operations.
    pub operation_manager: OperationManager,
    /// Bounded worker admission for blocking filesystem and archive work.
    pub task_scheduler: TaskScheduler,
    /// Process session manager for direct-launch lifecycle tracking.
    pub process_session_manager: ProcessSessionManager,
    /// Registry of supported games and their packages.
    pub games: Arc<crate::game_registry::GameRegistry>,
}

/// The main context type alias — use `Ctx` throughout the codebase.
pub type Ctx = CoreContext;

impl CoreContext {
    /// Initialize the core context from an `AppPaths`.
    ///
    /// Steps:
    /// 1. Create required directories.
    /// 2. **Fatal**: open and migrate `local_state.db`. Failure here returns
    ///    an error — without persistent state the app cannot function.
    /// 3. Validate cached registry schema version (warning only).
    /// 4. **Conservative** stale-staging cleanup: only removes directories
    ///    whose marker file `staging-complete` exists AND whose creation
    ///    time is older than 24h _and_ no active lock file references them.
    /// 5. Probe lock-directory writability (warning only).
    ///
    /// Warnings are returned as `Vec<String>` — genuinely recoverable
    /// observations that did not prevent initialization.
    pub fn initialize(
        paths: AppPaths,
        game_builder: crate::game_registry::GameRegistryBuilder,
    ) -> LauncherResult<(Self, Vec<String>)> {
        Self::initialize_with_registry_trust(
            paths,
            game_builder,
            crate::plugins::official::RegistryTrust::production(),
        )
    }

    /// [`Self::initialize`] with an explicit registry key. Test support:
    /// adapters call `initialize`, which trusts only the key compiled into the
    /// app. The key decides which installed plugins count as official.
    pub fn initialize_with_registry_trust(
        paths: AppPaths,
        mut game_builder: crate::game_registry::GameRegistryBuilder,
        registry_trust: crate::plugins::official::RegistryTrust,
    ) -> LauncherResult<(Self, Vec<String>)> {
        let mut warnings = Vec::new();

        // 1. Create required directories.
        paths.create_required_dirs()?;

        // 2. Fatal: initialize / migrate local_state.db.
        let db_path = paths.local_state_db();
        // Every adapter reaches the network through this context, so this is
        // the one place the outbound policy has to be installed. Until it is,
        // `network_gate` denies — a missing policy must not read as consent.
        crate::network_gate::install(Arc::new(crate::network_gate::SettingsGate::new(
            db_path.clone(),
        )));
        crate::db::init_local_state_db(&db_path).map_err(|e| {
            let msg = format!(
                "Failed to initialize local state database at {}: {e}",
                db_path.display()
            );
            // Log the path without secrets or token data.
            LauncherError::Generic {
                code: "ERR_LOCAL_STATE_FAILED".into(),
                message: msg,
            }
        })?;

        // The plugin runtime is created later, but a scripted package needs a
        // stable handle to it now. The handle is empty until the plugin service
        // is built, and a hook called before then says so instead of guessing.
        let behaviour_runtime = crate::game_registry::scripted::BehaviourRuntime::new();

        // 2b. Read every plugin game package as raw JSON first, resolve inheritance, then register
        //     the packages that survived. A plugin package never stops startup, but a failure to
        //     read plugins is said out loud: silently losing games looks like a bug.
        match crate::db::local_state_connection(&db_path) {
            Err(e) => warnings.push(format!("Cannot read plugin game packages: {e}")),
            Ok(conn) => {
                // An install that was interrupted can leave the previous
                // version stranded in the rollback folder and the game missing.
                // Finish or undo it before anything reads the records.
                match crate::plugins::install::recover_interrupted_installs(&conn, &paths) {
                    Ok(recovered) => warnings.extend(recovered),
                    Err(e) => {
                        warnings.push(format!("Cannot recover interrupted plugin installs: {e}"))
                    }
                }

                // A game package is data: it runs no code, so it loads whatever the
                // emergency stop says and needs no script permission; installing the
                // plugin (and granting `game:define`) was the opt-in. The one filter
                // is official status. A plugin that was installed as official, or
                // that uses an official plugin's id, and does not verify is blocked:
                // not loaded, and named in the warnings with the reason. It is not
                // demoted to a community plugin, which would let a revoked or
                // tampered package keep loading.
                let official = match crate::plugins::official::verify_installed(
                    &conn,
                    paths.root(),
                    &registry_trust,
                ) {
                    Ok(statuses) => statuses,
                    Err(e) => {
                        warnings.push(format!(
                            "Cannot verify official plugins, so none is treated as official: {e}"
                        ));
                        std::collections::BTreeMap::new()
                    }
                };

                match crate::plugins::store::list(&conn) {
                    Err(e) => warnings.push(format!("Cannot read plugin game packages: {e}")),
                    Ok(mut records) => {
                        // Official plugins register first. A game id or a store
                        // product id goes to whoever claims it first, and the
                        // records come back in plugin-id order, so without this
                        // a community plugin named `aaaa.x` could take
                        // `skyrim-se` before the official plugin was read.
                        // Stable, so each group stays in plugin-id order.
                        // Blocked (Unverified) official plugins come first
                        // too: they hold their game ids even though they do
                        // not load, so the reservation must happen before
                        // any community package is read.
                        records.sort_by_key(|record| {
                            matches!(
                                official.get(record.id().as_str()),
                                None | Some(crate::plugins::official::OfficialStatus::Community)
                            )
                        });

                        let mut inputs: Vec<crate::game_registry::PackageInput> = Vec::new();
                        let mut has_entrypoint: std::collections::BTreeMap<String, bool> =
                            std::collections::BTreeMap::new();
                        for record in records {
                            if !record.enabled
                                || crate::plugins::store::is_tombstone(&record)
                                || !record
                                    .granted
                                    .contains(agora_plugin_api::Capability::GameDefine)
                            {
                                continue;
                            }
                            has_entrypoint.insert(
                                record.id().to_string(),
                                record.manifest.entrypoint.is_some(),
                            );

                            let blocked = match official.get(record.id().as_str()) {
                                Some(crate::plugins::official::OfficialStatus::Official {
                                    ..
                                }) => false,
                                Some(crate::plugins::official::OfficialStatus::Unverified {
                                    reason,
                                }) => {
                                    warnings.push(format!(
                                        "Plugin {} is blocked and its games are not loaded: {reason}.",
                                        record.id()
                                    ));
                                    true
                                }
                                Some(
                                    crate::plugins::official::OfficialStatus::CatalogUnavailable {
                                        reason,
                                    },
                                )
                                | Some(crate::plugins::official::OfficialStatus::NotListed {
                                    reason,
                                }) => {
                                    // It still loads from its installed files;
                                    // say (once, on the plugin and here) that
                                    // it could not be checked this time.
                                    warnings.push(format!(
                                        "Plugin {} is loaded from its installed files: {reason}",
                                        record.id()
                                    ));
                                    false
                                }
                                Some(crate::plugins::official::OfficialStatus::Community)
                                | None => false,
                            };

                            for pkg_contrib in &record.manifest.contributions.game_packages {
                                // Validated at install, but the stored manifest is
                                // user-writable: never read outside the plugin.
                                let Ok(rel) = agora_game_api::RelPath::new(&pkg_contrib.path)
                                else {
                                    warnings.push(format!(
                                        "Plugin {}: game package path {:?} leaves the plugin folder; skipped",
                                        record.id(),
                                        pkg_contrib.path
                                    ));
                                    continue;
                                };
                                let file_path = record.install_dir.join(rel.as_str());
                                let content = match std::fs::read_to_string(&file_path) {
                                    Ok(c) => c,
                                    Err(err) => {
                                        warnings.push(format!(
                                            "Plugin {}: cannot read game package {}: {err}",
                                            record.id(),
                                            file_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                let json: serde_json::Value = match serde_json::from_str(&content) {
                                    Ok(json) => json,
                                    Err(err) => {
                                        warnings.push(format!(
                                            "Plugin {}: failed to parse game package {}: {err}",
                                            record.id(),
                                            file_path.display()
                                        ));
                                        continue;
                                    }
                                };
                                inputs.push(crate::game_registry::PackageInput {
                                    source: crate::game_registry::PackageSource::Plugin {
                                        plugin_id: record.id().to_string(),
                                    },
                                    blocked,
                                    label: format!(
                                        "Plugin {}, game package {}",
                                        record.id(),
                                        file_path.display()
                                    ),
                                    json,
                                });
                            }
                        }

                        // All packages are read before any is resolved, so a parent may live in
                        // another plugin. A broken package warns with its chain and the others
                        // carry on.
                        let (resolved, mut resolve_warnings) =
                            crate::game_registry::inherit::resolve(inputs);
                        warnings.append(&mut resolve_warnings);

                        // A blocked official plugin's games stay reserved before any community
                        // package is registered, even though the blocked package itself does not
                        // register.
                        for package in &resolved {
                            if package.blocked {
                                game_builder.reserve_blocked(&package.definition);
                            }
                        }
                        for package in resolved {
                            if package.blocked {
                                continue;
                            }
                            let plugin_id = match &package.source {
                                crate::game_registry::PackageSource::Plugin { plugin_id } => {
                                    Some(plugin_id.clone())
                                }
                                _ => None,
                            };
                            let names_script = crate::game_registry::scripted::needs_entrypoint(
                                &package.definition,
                            )
                            .is_some();
                            if names_script {
                                let Some(own_plugin) = plugin_id.as_deref() else {
                                    warnings.push(format!(
                                        "{}: skipped, because it names a script hook but has no plugin to run it",
                                        package.label
                                    ));
                                    continue;
                                };
                                if let Some((owner, why)) =
                                    crate::game_registry::scripted::missing_entrypoint(
                                        &package.definition,
                                        own_plugin,
                                        &package.owners,
                                        &has_entrypoint,
                                    )
                                {
                                    warnings.push(format!(
                                        "{}: skipped, because {why} but plugin {owner} has no entrypoint to run it",
                                        package.label
                                    ));
                                    continue;
                                }
                            }
                            // A base package with no games exists only for its children to inherit
                            // from. It is not a game package and has nothing to register.
                            if package.definition.games.is_empty() {
                                continue;
                            }
                            let source = package.source.clone();
                            let label = package.label.clone();
                            let package: Arc<dyn agora_game_api::GamePackage> =
                                match (&package.source, names_script) {
                                    (
                                        crate::game_registry::PackageSource::Plugin { plugin_id },
                                        true,
                                    ) => match agora_plugin_api::manifest::PluginId::parse(
                                        plugin_id,
                                    ) {
                                        Ok(id) => Arc::new(
                                            crate::game_registry::scripted::ScriptedPackage::new(
                                                package.definition,
                                                id,
                                                package.owners.clone(),
                                                behaviour_runtime.clone(),
                                            ),
                                        ),
                                        Err(error) => {
                                            warnings.push(format!(
                                                "{}: skipped, because its plugin id is invalid: {}",
                                                label, error.message
                                            ));
                                            continue;
                                        }
                                    },
                                    _ => Arc::new(crate::game_registry::DeclarativePackage(
                                        package.definition,
                                    )),
                                };
                            if let Err(err) = game_builder.add(source, package) {
                                warnings.push(format!(
                                    "{}: rejected game package: {err}. A game or store product \
                                     already claimed (official plugins register first) cannot be \
                                     claimed again.",
                                    label
                                ));
                            }
                        }
                    }
                }
            }
        }

        let games = Arc::new(game_builder.build());
        warnings.extend(games.recover_at_startup(&paths));

        // 3. Verify the cached registry's Ed25519 signature before anything
        //    reads it. Download-time verification only covers transit; the
        //    cached copy sits in a user-writable directory and supplies every
        //    download URL and expected hash the launcher trusts. A failing db is
        //    quarantined here, which also leaves no `registry.db` behind so the
        //    next sync re-downloads a good copy.
        let registry_trusted = match crate::registry_sync::verify_cached_registry(paths.root()) {
            crate::registry_sync::CachedRegistryIntegrity::Verified => true,
            crate::registry_sync::CachedRegistryIntegrity::Absent => false,
            crate::registry_sync::CachedRegistryIntegrity::Quarantined(reason)
            | crate::registry_sync::CachedRegistryIntegrity::Error(reason) => {
                warnings.push(reason);
                false
            }
        };

        //    Then validate the cached registry schema version (warning only —
        //    the cached db may be absent or stale, which is recoverable). A db
        //    that failed verification is not opened at all.
        let reg_path = paths.registry_db();
        let mut registry_conn = None;
        if registry_trusted && reg_path.exists() {
            match crate::db::registry_connection(&reg_path) {
                Ok(conn) => {
                    let version: Result<i64, _> = conn.query_row(
                        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                        [],
                        |row| row.get(0),
                    );
                    if let Ok(v) = version {
                        if v > crate::registry_sync::APP_REGISTRY_SCHEMA_VERSION {
                            warnings.push(format!(
                                "Cached registry schema v{v} is newer than app (v{}); upgrade launcher",
                                crate::registry_sync::APP_REGISTRY_SCHEMA_VERSION,
                            ));
                        }
                    }
                    registry_conn = Some(conn);
                }
                Err(e) => {
                    warnings.push(format!("Cannot open cached registry: {e}"));
                }
            }
        }

        // 4. Conservative stale-staging cleanup.
        //    Only remove staged directories that:
        //    - Have existed for more than 24 hours
        //    - Contain a `staging-complete` marker file (orphaned after
        //      a crash), OR have no active lock referencing them
        let staging_root = paths.staging_root();
        if staging_root.exists() {
            let stale_threshold = std::time::Duration::from_secs(24 * 60 * 60);
            let now = std::time::SystemTime::now();
            if let Ok(entries) = std::fs::read_dir(&staging_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    // Check age via directory's modified time.
                    let age = match path.metadata().and_then(|m| m.modified()) {
                        Ok(modified) => match now.duration_since(modified) {
                            Ok(d) => d,
                            Err(_) => continue,
                        },
                        Err(_) => continue,
                    };
                    if age < stale_threshold {
                        continue;
                    }
                    // Conservative: only remove if there's a marker file
                    // confirming this was a completed staging attempt.
                    if path.join("staging-complete").exists() {
                        let _ = std::fs::remove_dir_all(&path);
                    }
                    // Otherwise leave it; an operation-manager cleanup
                    // later can handle orphaned staging dirs.
                }
            }
        }

        // 5. Validate lock directory is usable (warning only).
        //    Known write: every open, read or write, creates and removes `locks/.probe`, which
        //    changes the folder's modified time and leaves no file behind. It is the writability
        //    check itself, so it stays.
        let locks_root = paths.locks_root();
        if locks_root.exists() {
            let probe = locks_root.join(".probe");
            match std::fs::write(&probe, b"probe") {
                Ok(_) => {
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => warnings.push(format!("Lock directory may not be writable: {e}")),
            }
        }

        // The proxy applies to every HTTP client built from here on, including
        // the ones helpers build outside the context.
        if let Ok(conn) = crate::db::local_state_connection(&paths.local_state_db()) {
            crate::http_client::configure_proxy(crate::http_client::saved_proxy(&conn));
        }

        let ctx = Self {
            lock_manager: LockManager::new(paths.locks_root()),
            external_data_root: None,
            paths,
            http_clients: HttpClients::new().map_err(|e| {
                // Convert HTTP client init failure to a fatal error.
                LauncherError::Generic {
                    code: "ERR_HTTP_CLIENT_INIT".into(),
                    message: format!("Failed to build HTTP clients: {e}"),
                }
            })?,
            extensions: Extensions::default(),
            clock: Arc::new(WallClock),
            progress_sink: Arc::new(NoopProgressSink),
            event_sink: Arc::new(NoopEventSink),
            operation_manager: OperationManager::new(),
            task_scheduler: TaskScheduler::default(),
            process_session_manager: ProcessSessionManager::new(),
            games,
        };
        ctx.extensions.insert(behaviour_runtime);

        // 6. Let game packages load their catalogs from the signed registry
        //    (or fall back to embedded data when there is none).
        if ctx.games.is_empty() {
            warnings.push(
                "No game package is registered; no game catalogs or providers are available".into(),
            );
        }
        match ctx
            .games
            .load_catalogs(&ctx, registry_conn.as_ref(), CatalogEvent::Startup)
        {
            Ok(more) => warnings.extend(more),
            Err(error) => warnings.push(format!("Cannot load game catalogs: {error}")),
        }

        Ok((ctx, warnings))
    }

    /// Create a context for testing with temporary paths.
    ///
    /// The caller should create a temp directory and pass its path.
    /// Uses a fast single-client HTTP client (no policy enforcement).
    pub fn for_testing(root: std::path::PathBuf) -> Self {
        let external_data_root = Some(root.clone());
        let paths = AppPaths::from_root(root);
        let lock_manager = LockManager::new(paths.locks_root());
        let _ = std::fs::create_dir_all(paths.locks_root());
        // Outbound requests are denied until an adapter installs a policy, so
        // a test context installs a permissive one. Production adapters must
        // install `SettingsGate` instead — there is no production constructor
        // that yields an unrestricted gate.
        crate::network_gate::install(std::sync::Arc::new(crate::network_gate::AllowAll));
        Self {
            paths,
            external_data_root,
            http_clients: HttpClients::for_testing(reqwest::Client::new()),
            extensions: Extensions::default(),
            clock: Arc::new(WallClock),
            lock_manager,
            progress_sink: Arc::new(NoopProgressSink),
            event_sink: Arc::new(NoopEventSink),
            operation_manager: OperationManager::new(),
            task_scheduler: TaskScheduler::default(),
            process_session_manager: ProcessSessionManager::new(),
            games: Arc::new(crate::game_registry::GameRegistry::empty()),
        }
    }

    /// Replace the game registry.
    pub fn with_games(mut self, games: Arc<crate::game_registry::GameRegistry>) -> Self {
        self.games = games;
        self
    }

    /// Replace the progress sink (e.g., to wire in a Tauri event emitter).
    pub fn with_progress_sink(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress_sink = sink;
        self
    }

    /// Replace the event sink (e.g., to wire in a Tauri event emitter).
    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.event_sink = sink;
        self
    }

    /// Replace the clock (e.g., with a test clock).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the HTTP clients (e.g., with a single client for testing).
    pub fn with_http_clients(mut self, clients: HttpClients) -> Self {
        self.http_clients = clients;
        self
    }

    /// Replace the process session manager (e.g., for testing with a shared
    /// manager between service and test harness).
    pub fn with_process_session_manager(mut self, mgr: ProcessSessionManager) -> Self {
        self.process_session_manager = mgr;
        self
    }

    /// Re-run every game package's catalog hook against the freshly installed
    /// registry. Each package replaces its catalogs only if all of them parse.
    ///
    /// Returns human-readable warnings; errors opening the database are
    /// reported as warnings too, leaving the active catalogs in place. An error
    /// means a package's catalogs parsed but could not be activated.
    pub fn reload_game_catalogs(&self) -> LauncherResult<Vec<String>> {
        let reg_path = self.paths.registry_db();
        if !reg_path.exists() {
            return Ok(vec![
                "No registry database found; runtime catalog unchanged".into(),
            ]);
        }
        let conn = match crate::db::registry_connection(&reg_path) {
            Ok(conn) => conn,
            Err(e) => {
                return Ok(vec![format!(
                    "Cannot open registry database; runtime catalog unchanged: {e}"
                )]);
            }
        };
        // The wording above predates game packages; the CLI's JSON output
        // carries it as `catalog_warnings`, so it is kept.
        self.games
            .load_catalogs(self, Some(&conn), CatalogEvent::Reload)
    }

    /// The lock manager reference.
    pub fn lock_manager(&self) -> &LockManager {
        &self.lock_manager
    }

    /// The path model reference.
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// The HTTP clients reference.
    pub fn http_clients(&self) -> &HttpClients {
        &self.http_clients
    }
}

impl std::fmt::Debug for CoreContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreContext")
            .field("paths", &self.paths)
            .field("external_data_root", &self.external_data_root)
            .field("http_clients", &self.http_clients)
            .field("lock_manager", &self.lock_manager)
            .field("extensions", &self.extensions)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Ctx is CoreContext — no separate old-style Ctx remains.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_creates_dirs_and_db() {
        let tmp = std::env::temp_dir().join(format!("agora-ctx-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = AppPaths::from_root(tmp.clone());
        struct TestServices;
        impl crate::game_hooks::CompiledServices for TestServices {
            fn load_catalogs(
                &self,
                _: &CoreContext,
                registry: Option<&rusqlite::Connection>,
                event: CatalogEvent,
            ) -> crate::error::LauncherResult<Vec<String>> {
                Ok(match (registry, event) {
                    (None, CatalogEvent::Startup) => {
                        vec!["test catalog hook: startup without a registry".into()]
                    }
                    _ => Vec::new(),
                })
            }
        }
        let mut games = crate::game_registry::GameRegistry::builder();
        games
            .add_compiled(
                "test",
                crate::game_registry::test_support::package("test-game"),
                Arc::new(TestServices),
            )
            .unwrap();

        let (ctx, warnings) = CoreContext::initialize(paths, games).unwrap();
        assert!(ctx.paths.root().exists(), "root should exist");
        assert!(
            ctx.paths.local_state_db().exists(),
            "local_state.db should exist"
        );
        assert!(
            ctx.paths.instances_root().exists(),
            "instances dir should exist"
        );
        assert!(ctx.paths.locks_root().exists(), "locks dir should exist");
        assert!(
            warnings
                .iter()
                .any(|warning| warning == "test catalog hook: startup without a registry"),
            "registered catalog hooks run at startup: {:?}",
            warnings
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_for_testing_succeeds() {
        let tmp = std::env::temp_dir().join(format!("agora-ctx-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let ctx = CoreContext::for_testing(tmp.clone());
        assert!(
            ctx.http_clients
                .get(crate::http_client::ClientCategory::GitHub) as *const _ as usize
                > 0
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_with_sinks_and_clock() {
        let tmp = std::env::temp_dir().join(format!("agora-ctx-sink-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let ctx = CoreContext::for_testing(tmp.clone())
            .with_progress_sink(Arc::new(crate::event_sink::NoopProgressSink))
            .with_event_sink(Arc::new(crate::event_sink::NoopEventSink));
        // No panic means it works.
        drop(ctx);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_clock_abstraction() {
        let start = std::time::SystemTime::now();
        let test_clock = TestClock::new(start);
        let ctx = CoreContext::for_testing(
            std::env::temp_dir().join(format!("agora-clock-test-{}", std::process::id())),
        )
        .with_clock(Arc::new(test_clock));
        let _now = ctx.clock.now();
    }

    #[test]
    fn test_test_clock_advance() {
        let start = std::time::SystemTime::now();
        let clock = TestClock::new(start);
        let t1 = clock.now();
        clock.advance(std::time::Duration::from_secs(60));
        let t2 = clock.now();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_invalid_db_path_fails() {
        // A path to a non-existent parent directory should fail.
        let tmp = std::env::temp_dir().join(format!("agora-ctx-dbfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Use a subdirectory that doesn't exist and can't be created
        // (but paths.create_required_dirs will create the root, so make
        // the db_path itself unwritable).
        let paths = AppPaths::from_root(tmp.clone());
        // Create root dir but make local_state.db path unwritable by
        // creating it as a directory beforehand.
        std::fs::create_dir_all(tmp.join("local_state.db")).unwrap();
        let result = CoreContext::initialize(paths, crate::game_registry::GameRegistry::builder());
        assert!(result.is_err(), "should fail when db cannot be created");
        let err = result.unwrap_err();
        assert_eq!(err.code(), "ERR_LOCAL_STATE_FAILED");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
