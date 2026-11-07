//! `ScriptedPackage`: a plugin's `GamePackage`, implemented by calling its
//! script entrypoints with JSON (MASTER_SPEC §30.12).
//!
//! Nothing here knows about QuickJS. The engine arrives through
//! [`BehaviourInvoker`], which `agora-core`'s plugin service implements. That
//! keeps the engine out of core, as it is everywhere else, and lets the tests
//! use a fake invoker to exercise the reply validation without a runtime.
//!
//! Two error rules matter and are deliberately different:
//!
//! - a **malformed reply** is an error, never `None` — `None` means "use the
//!   declaration", and a broken hook must not silently pretend to have nothing
//!   to say;
//! - a hook that **throws, times out or runs out of memory** is reported to the
//!   plugin log and core carries on, exactly as one broken plugin must not stop
//!   a launch.

use agora_game_api::{
    Behaviours, ContentClassification, Diagnostic, DownloadRecognition, GameContext, GameError,
    GameFuture, GameHost, GameInstall, GamePackage, LoadOrder, LoadOrderDefinition, LoadOrderRule,
    PackageDefinition as PackageDefinitionType, RuntimeIdentity,
};
use agora_plugin_api::error::{PluginError, PluginErrorCode};
use agora_plugin_api::host::GameReads;
use agora_plugin_api::manifest::PluginId;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How long one game hook may run. Long enough for a parser over a large
/// folder, short enough that a stuck hook cannot hold a launch.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// `recognize_download` runs on the download watcher's thread, so it gets a
/// shorter budget and never waits for a script that is still running.
pub const RECOGNIZE_TIMEOUT: Duration = Duration::from_secs(2);

/// What core supplies so a scripted package can call its plugin's script.
pub trait BehaviourInvoker: Send + Sync {
    /// Call one export. The engine enforces `timeout`; this layer does not
    /// trust the script to stop itself.
    fn invoke(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, PluginError>;

    /// The same, with one instance's merged view in scope for the call.
    fn invoke_with_game_reads(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
        reads: Arc<dyn GameReads>,
    ) -> Result<serde_json::Value, PluginError>;

    /// Call a game hook. The implementation may put an instance's merged view
    /// in scope, derived from the instance the hook's arguments name. The
    /// default has no reads, which is correct for a hook that needs none.
    fn invoke_hook(
        &self,
        plugin_id: &PluginId,
        export: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, PluginError> {
        self.invoke(plugin_id, export, args, timeout)
    }

    /// Record a hook failure in the plugin's own log. Never fails a launch.
    fn report(&self, plugin_id: &PluginId, message: &str);
}

/// The invoker for the process's plugin service, installed after the context
/// exists.
///
/// The game registry is built before the plugin service, so the package holds
/// this handle and the service fills it in. A hook called before that returns
/// a clear error rather than quietly doing nothing.
#[derive(Clone, Default)]
pub struct BehaviourRuntime {
    inner: Arc<RwLock<Option<Arc<dyn BehaviourInvoker>>>>,
}

impl BehaviourRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, invoker: Arc<dyn BehaviourInvoker>) {
        if let Ok(mut slot) = self.inner.write() {
            *slot = Some(invoker);
        }
    }

    fn invoker(&self) -> Option<Arc<dyn BehaviourInvoker>> {
        self.inner.read().ok().and_then(|slot| slot.clone())
    }

    pub fn is_installed(&self) -> bool {
        self.invoker().is_some()
    }
}

/// A game package backed by a plugin's script.
pub struct ScriptedPackage {
    definition: PackageDefinitionType,
    plugin_id: PluginId,
    behaviours: Behaviours,
    runtime: BehaviourRuntime,
}

impl ScriptedPackage {
    pub fn new(
        definition: PackageDefinitionType,
        plugin_id: PluginId,
        runtime: BehaviourRuntime,
    ) -> Self {
        let behaviours = definition.behaviours.clone().unwrap_or_default();
        Self {
            definition,
            plugin_id,
            behaviours,
            runtime,
        }
    }

    fn invoker(&self) -> Result<Arc<dyn BehaviourInvoker>, GameError> {
        self.runtime.invoker().ok_or_else(|| GameError {
            code: "ERR_GAME_SCRIPT_HOST".into(),
            message: "the plugin runtime is not available yet; try again after Agora starts".into(),
        })
    }

    fn report(&self, message: &str) {
        if let Some(invoker) = self.runtime.invoker() {
            invoker.report(&self.plugin_id, message);
        }
    }

    /// Call one hook and return `None` when it said nothing.
    ///
    /// A script error, timeout or exhausted resource is reported and becomes
    /// `None` (core carries on). A successful call whose reply is not the shape
    /// the hook promises is an error, because core would otherwise act on a
    /// guess.
    fn call_optional(
        &self,
        export: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>, GameError> {
        let invoker = self.invoker()?;
        match invoker.invoke_hook(&self.plugin_id, export, args, HOOK_TIMEOUT) {
            Ok(value) if value.is_null() => Ok(None),
            Ok(value) => Ok(Some(value)),
            Err(error) if carries_on(&error) => {
                self.report(&format!("`{export}` did not complete: {}", error.message));
                Ok(None)
            }
            Err(error) => Err(GameError {
                code: "ERR_GAME_HOOK_FAILED".into(),
                message: error.message,
            }),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(
        &self,
        export: &str,
        value: serde_json::Value,
    ) -> Result<T, GameError> {
        serde_json::from_value(value).map_err(|error| GameError {
            code: "ERR_GAME_HOOK_REPLY".into(),
            message: format!("`{export}` returned a value Agora could not read: {error}"),
        })
    }

    fn game_script_args<T: serde::Serialize>(&self, name: &str, value: T) -> serde_json::Value {
        serde_json::json!({ name: value })
    }
}

/// Errors that mean "the hook did not run", not "the hook gave a bad answer".
fn carries_on(error: &PluginError) -> bool {
    matches!(
        error.code,
        PluginErrorCode::ScriptError
            | PluginErrorCode::Timeout
            | PluginErrorCode::NotActivated
            | PluginErrorCode::Cancelled
            | PluginErrorCode::ResourceExhausted
    )
}

impl GamePackage for ScriptedPackage {
    fn definition(&self) -> &PackageDefinitionType {
        &self.definition
    }

    fn detect_runtime<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        install: GameInstall,
    ) -> GameFuture<'a, Option<RuntimeIdentity>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.detect_runtime.as_deref() else {
                return Ok(None);
            };
            let args = self.game_script_args("install", install);
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn classify<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        context: GameContext,
        archive: agora_game_api::ExtractedArchive,
    ) -> GameFuture<'a, Option<Vec<ContentClassification>>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.classify.as_deref() else {
                return Ok(None);
            };
            let args = serde_json::json!({ "context": context, "archive": archive });
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn recognize_download(
        &self,
        _game: &agora_game_api::GameId,
        file_name: &str,
        entries: &[String],
    ) -> Option<DownloadRecognition> {
        let export = self.behaviours.recognize_download.as_deref()?.to_string();
        let invoker = self.runtime.invoker()?;
        let args = serde_json::json!({ "fileName": file_name, "entries": entries });
        let plugin_id = self.plugin_id.clone();

        // This runs on the download watcher's thread. A slow script must not
        // hold that thread, so the call is made on a helper thread and the
        // watcher waits only a short, fixed time. If the script has not
        // answered by then the answer is "not recognised this time"; the
        // script keeps running and is stopped by its own timeout.
        let report_export = export.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("agora-recognize".into())
            .spawn(move || {
                let value = invoker.invoke_hook(&plugin_id, &export, args, RECOGNIZE_TIMEOUT);
                let _ = tx.send(value);
            });
        if spawned.is_err() {
            return None;
        }
        match rx.recv_timeout(RECOGNIZE_TIMEOUT) {
            Ok(Ok(value)) if value.is_null() => None,
            Ok(Ok(value)) => match serde_json::from_value::<DownloadRecognition>(value) {
                Ok(recognition) => Some(recognition),
                Err(error) => {
                    // The trait gives no way to return an error here, so the
                    // malformed reply is reported rather than silently treated
                    // as "not this game's content".
                    self.report(&format!(
                        "`{report_export}` returned a download recognition Agora could not read: {error}"
                    ));
                    None
                }
            },
            Ok(Err(error)) => {
                self.report(&format!(
                    "`{report_export}` did not complete: {}",
                    error.message
                ));
                None
            }
            Err(_) => {
                self.report(&format!(
                    "`{report_export}` did not answer within {}ms; recognition was skipped this time",
                    RECOGNIZE_TIMEOUT.as_millis()
                ));
                None
            }
        }
    }

    fn load_order_rules<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        context: GameContext,
    ) -> GameFuture<'a, Option<Vec<LoadOrderRule>>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.load_order_rules.as_deref() else {
                return Ok(None);
            };
            let args = self.game_script_args("context", context);
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn diagnostics<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        context: GameContext,
    ) -> GameFuture<'a, Vec<Diagnostic>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.diagnostics.as_deref() else {
                return Ok(Vec::new());
            };
            let args = self.game_script_args("context", context);
            match self.call_optional(export, args)? {
                None => Ok(Vec::new()),
                Some(value) => self.parse(export, value),
            }
        })
    }

    fn prepare_launch<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        context: GameContext,
    ) -> GameFuture<'a, Option<agora_game_api::LaunchRecipe>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.prepare_launch.as_deref() else {
                return Ok(None);
            };
            let args = self.game_script_args("context", context);
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn parse_load_order<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        _definition: &'a LoadOrderDefinition,
        _contents: Vec<u8>,
    ) -> GameFuture<'a, Option<LoadOrder>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.parse_load_order.as_deref() else {
                return Ok(None);
            };
            let args = serde_json::json!({
                "definition": _definition,
                "contents": _contents,
            });
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn render_load_order<'a>(
        &'a self,
        _host: &'a dyn GameHost,
        _definition: &'a LoadOrderDefinition,
        order: &'a LoadOrder,
    ) -> GameFuture<'a, Option<String>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.render_load_order.as_deref() else {
                return Ok(None);
            };
            let args = serde_json::json!({ "definition": _definition, "order": order });
            let Some(value) = self.call_optional(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }
}

/// Why a package needs its plugin to have an entrypoint, if it does.
///
/// A package that names a behaviour, a script load-order format or a script
/// rule set but whose plugin ships no code would never be able to honour it, so
/// it is warned about and not registered rather than silently half-working.
pub fn needs_entrypoint(definition: &PackageDefinitionType) -> Option<&'static str> {
    if definition
        .behaviours
        .as_ref()
        .is_some_and(Behaviours::names_any)
    {
        return Some("it names script behaviours");
    }
    for game in &definition.games {
        if game
            .load_orders
            .iter()
            .any(|order| order.format.is_script() || order.rules.is_script())
        {
            return Some("it names a script load order");
        }
    }
    None
}

impl ScriptedPackage {
    /// Whether this package names any script hook at all.
    pub fn has_behaviours(&self) -> bool {
        self.definition
            .behaviours
            .as_ref()
            .is_some_and(Behaviours::names_any)
    }

    /// The plugin that provides this package.
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake engine: it answers with whatever the test told it to, and records
    /// what it was asked.
    struct FakeInvoker {
        reply: Result<serde_json::Value, PluginError>,
        reports: Mutex<Vec<String>>,
        timeouts: Mutex<Vec<Duration>>,
    }

    impl FakeInvoker {
        fn answer(value: serde_json::Value) -> Arc<Self> {
            Arc::new(Self {
                reply: Ok(value),
                reports: Mutex::default(),
                timeouts: Mutex::default(),
            })
        }

        fn fail(error: PluginError) -> Arc<Self> {
            Arc::new(Self {
                reply: Err(error),
                reports: Mutex::default(),
                timeouts: Mutex::default(),
            })
        }
    }

    impl BehaviourInvoker for FakeInvoker {
        fn invoke(
            &self,
            _plugin_id: &PluginId,
            _export: &str,
            _args: serde_json::Value,
            timeout: Duration,
        ) -> Result<serde_json::Value, PluginError> {
            self.timeouts.lock().unwrap().push(timeout);
            self.reply.clone()
        }

        fn invoke_with_game_reads(
            &self,
            plugin_id: &PluginId,
            export: &str,
            args: serde_json::Value,
            timeout: Duration,
            _reads: Arc<dyn GameReads>,
        ) -> Result<serde_json::Value, PluginError> {
            self.invoke(plugin_id, export, args, timeout)
        }

        fn report(&self, _plugin_id: &PluginId, message: &str) {
            self.reports.lock().unwrap().push(message.to_string());
        }
    }

    fn package(invoker: Arc<dyn BehaviourInvoker>) -> ScriptedPackage {
        let definition: PackageDefinitionType = serde_json::from_value(serde_json::json!({
            "id": "test.pkg",
            "version": "0.1.0",
            "api_range": ">=0.1, <0.2",
            "parents": [],
            "games": [{
                "id": "test-game",
                "name": "Test Game",
                "stores": [],
                "version_sources": [],
                "deployment": "redirect",
                "framework_ids": [],
                "tool_ids": [],
                "content_rules": [],
                "log_paths": [],
                "crash_paths": [],
                "user_files": [],
                "save_paths": [],
                "linked_archive_patterns": [],
                "declared_writes": [],
                "excluded_paths": [],
                "load_orders": [],
                "runtime_files": [],
                "save_location": [],
                "launch_alternatives": [],
                "copy_patterns": []
            }],
            "frameworks": [],
            "tools": [],
            "behaviours": { "detect_runtime": "detect" }
        }))
        .unwrap();
        let runtime = BehaviourRuntime::new();
        runtime.install(invoker);
        // `definition` is our own type; construct the scripted package without
        // going through the registry helper so the test stays a unit test.
        ScriptedPackage::new(definition, PluginId::parse("acme.test").unwrap(), runtime)
    }

    fn install() -> GameInstall {
        serde_json::from_value(serde_json::json!({
            "id": "steam:test",
            "runtime": { "game": "test-game", "store": "steam", "version": "1.0" },
            "kind": "base_game",
            "location": "C:/games/test",
            "volume": null,
            "capabilities": {
                "executables_readable": true,
                "accepts_new_files": true,
                "relocatable": true
            }
        }))
        .unwrap()
    }

    struct NoHost;
    impl GameHost for NoHost {
        fn download(
            &self,
            _: agora_game_api::DownloadRequest,
        ) -> GameFuture<'_, agora_game_api::DownloadedArtifact> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn read_install_file(
            &self,
            _: agora_game_api::InstallReadRequest,
        ) -> GameFuture<'_, Vec<u8>> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn read_artifact(&self, _: agora_game_api::ArtifactReadRequest) -> GameFuture<'_, Vec<u8>> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn extract_archive(
            &self,
            _: agora_game_api::ArchiveRequest,
        ) -> GameFuture<'_, agora_game_api::ExtractedArchive> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn run_fomod(
            &self,
            _: agora_game_api::FomodRequest,
        ) -> GameFuture<'_, agora_game_api::FomodResult> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn sort_load_order(
            &self,
            _: agora_game_api::SortLoadOrderRequest,
        ) -> GameFuture<'_, LoadOrder> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn run_tool(
            &self,
            _: agora_game_api::ToolRunRequest,
        ) -> GameFuture<'_, agora_game_api::ToolRunResult> {
            Box::pin(async {
                Err(GameError {
                    code: "no".into(),
                    message: "no".into(),
                })
            })
        }
        fn emit_event(&self, _: agora_game_api::GameEvent) -> agora_game_api::GameResult<()> {
            Ok(())
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Wake, Waker};
        struct ThreadWaker(std::thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }

    #[test]
    fn a_malformed_hook_reply_is_an_error_not_none() {
        let invoker = FakeInvoker::answer(serde_json::json!({ "not": "a runtime identity" }));
        let package = package(invoker);
        let error = block_on(package.detect_runtime(&NoHost, install())).unwrap_err();
        assert_eq!(error.code, "ERR_GAME_HOOK_REPLY");
    }

    #[test]
    fn a_hook_that_never_returns_is_stopped() {
        let invoker = FakeInvoker::fail(PluginError::new(
            PluginErrorCode::Timeout,
            "`detect` did not finish within 5000ms",
        ));
        let package = package(invoker);
        let result = block_on(package.detect_runtime(&NoHost, install())).unwrap();
        assert!(result.is_none(), "a timeout carries on");
    }

    #[test]
    fn a_hook_that_throws_is_reported_and_core_carries_on() {
        let fake = FakeInvoker::fail(PluginError::new(
            PluginErrorCode::ScriptError,
            "TypeError: boom",
        ));
        let package = package(fake.clone());
        let result = block_on(package.detect_runtime(&NoHost, install())).unwrap();
        assert!(result.is_none());
        let reports = fake.reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].contains("TypeError: boom"));
    }

    #[test]
    fn a_hook_is_given_a_time_limit() {
        let fake = FakeInvoker::answer(serde_json::Value::Null);
        let package = package(fake.clone());
        block_on(package.detect_runtime(&NoHost, install())).unwrap();
        let timeouts = fake.timeouts.lock().unwrap();
        assert_eq!(timeouts.as_slice(), &[HOOK_TIMEOUT]);
    }
}
