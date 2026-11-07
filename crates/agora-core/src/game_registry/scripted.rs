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
    Behaviours, Diagnostic, DownloadRecognition, GameContext, GameError, GameFuture, GameInstall,
    GamePackage, LoadOrderLine, LoadOrderRule, PackageDefinition as PackageDefinitionType, RelPath,
    RuntimeIdentity, Suggestion,
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

    /// Call a hook whose `None` means "not implemented". A failure is an
    /// error here, never `None`: collapsing it would read as a clean answer.
    fn call_required(
        &self,
        export: &str,
        args: serde_json::Value,
    ) -> Result<Option<serde_json::Value>, GameError> {
        let invoker = self.invoker()?;
        match invoker.invoke_hook(&self.plugin_id, export, args, HOOK_TIMEOUT) {
            Ok(value) if value.is_null() => Ok(None),
            Ok(value) => Ok(Some(value)),
            Err(error) => {
                self.report(&format!("`{export}` did not complete: {}", error.message));
                Err(GameError {
                    code: "ERR_GAME_HOOK_FAILED".into(),
                    message: format!("{} `{export}` failed: {}", self.plugin_id, error.message),
                })
            }
        }
    }

    fn warning(&self, export: &str, error: &GameError) -> Diagnostic {
        Diagnostic {
            code: "ERR_GAME_HOOK_FAILED".into(),
            message: format!(
                "{} could not run its `{export}` check: {}. This check was not completed.",
                self.plugin_id, error.message
            ),
            severity: agora_game_api::DiagnosticSeverity::Warning,
            repairs: Vec::new(),
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
        context: GameContext,
        item_name: &'a str,
        files: Vec<RelPath>,
    ) -> GameFuture<'a, Option<Suggestion>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.classify.as_deref() else {
                return Ok(None);
            };
            let args = serde_json::json!({
                "context": context,
                "itemName": item_name,
                "files": files,
            });
            let Some(value) = self.call_required(export, args)? else {
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
        let Some(invoker) = self.runtime.invoker() else {
            // Before the plugin service starts nothing is known about this
            // file. Reporting it as "not recognised" would be a wrong answer;
            // it is skipped and asked again on the next scan.
            self.report(&format!(
                "`{export}` could not run: the plugin runtime is not available yet; recognition                  was skipped this time"
            ));
            return None;
        };
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
        context: GameContext,
        list_id: &'a str,
    ) -> GameFuture<'a, Option<Vec<LoadOrderRule>>> {
        Box::pin(async move {
            let export = self
                .definition
                .games
                .iter()
                .flat_map(|game| game.load_orders.iter())
                .find(|order| order.id == list_id)
                .and_then(|order| order.rules.entry.clone())
                .or_else(|| self.behaviours.load_order_rules.clone());
            let Some(export) = export else {
                return Ok(None);
            };
            let args = serde_json::json!({ "context": context, "listId": list_id });
            let Some(value) = self.call_required(&export, args)? else {
                return Ok(None);
            };
            self.parse(&export, value).map(Some)
        })
    }

    fn diagnostics<'a>(&'a self, context: GameContext) -> GameFuture<'a, Vec<Diagnostic>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.diagnostics.as_deref() else {
                return Ok(Vec::new());
            };
            let args = self.game_script_args("context", context);
            match self.call_required(export, args) {
                Ok(None) => Ok(Vec::new()),
                Ok(Some(value)) => match self.parse(export, value) {
                    Ok(diagnostics) => Ok(diagnostics),
                    Err(error) => Ok(vec![self.warning(export, &error)]),
                },
                Err(error) => Ok(vec![self.warning(export, &error)]),
            }
        })
    }

    fn prepare_launch<'a>(
        &'a self,
        context: GameContext,
    ) -> GameFuture<'a, Option<agora_game_api::LaunchRecipe>> {
        Box::pin(async move {
            let Some(export) = self.behaviours.prepare_launch.as_deref() else {
                return Ok(None);
            };
            let args = self.game_script_args("context", context);
            let Some(value) = self.call_required(export, args)? else {
                return Ok(None);
            };
            self.parse(export, value).map(Some)
        })
    }

    fn parse_load_order<'a>(
        &'a self,
        context: GameContext,
        list_id: &'a str,
        bytes: Vec<u8>,
    ) -> GameFuture<'a, Option<Vec<LoadOrderLine>>> {
        Box::pin(async move {
            let Some(export) = self.script_format_export(list_id, true) else {
                return Ok(None);
            };
            let args = serde_json::json!({
                "context": context,
                "listId": list_id,
                "bytes": bytes,
            });
            let Some(value) = self.call_required(&export, args)? else {
                return Ok(None);
            };
            self.parse(&export, value).map(Some)
        })
    }

    fn render_load_order<'a>(
        &'a self,
        context: GameContext,
        list_id: &'a str,
        lines: Vec<LoadOrderLine>,
    ) -> GameFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let Some(export) = self.script_format_export(list_id, false) else {
                return Ok(None);
            };
            let args = serde_json::json!({
                "context": context,
                "listId": list_id,
                "lines": lines,
            });
            let Some(value) = self.call_required(&export, args)? else {
                return Ok(None);
            };
            // A script may return text or an array of byte values.
            if let Some(text) = value.as_str() {
                return Ok(Some(text.as_bytes().to_vec()));
            }
            let bytes: Vec<u8> = self.parse(&export, value)?;
            Ok(Some(bytes))
        })
    }
}

impl ScriptedPackage {
    /// The parse or render export a list's script format names.
    fn script_format_export(&self, list_id: &str, parse: bool) -> Option<String> {
        self.definition
            .games
            .iter()
            .flat_map(|game| game.load_orders.iter())
            .find(|order| order.id == list_id)
            .and_then(|order| match &order.format {
                agora_game_api::LoadOrderFormat::Script { parse: p, render } => {
                    Some(if parse { p.clone() } else { render.clone() })
                }
                agora_game_api::LoadOrderFormat::Lines { .. } => None,
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
        package_with(invoker, serde_json::json!({ "detect_runtime": "detect" }))
    }

    fn definition_with(behaviours: serde_json::Value) -> PackageDefinitionType {
        serde_json::from_value(serde_json::json!({
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
            "behaviours": behaviours
        }))
        .unwrap()
    }

    fn package_with(
        invoker: Arc<dyn BehaviourInvoker>,
        behaviours: serde_json::Value,
    ) -> ScriptedPackage {
        let definition = definition_with(behaviours);
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
        let error = block_on(package.detect_runtime(install())).unwrap_err();
        assert_eq!(error.code, "ERR_GAME_HOOK_REPLY");
    }

    #[test]
    fn a_hook_that_never_returns_is_stopped() {
        let invoker = FakeInvoker::fail(PluginError::new(
            PluginErrorCode::Timeout,
            "`detect` did not finish within 5000ms",
        ));
        let package = package(invoker);
        let result = block_on(package.detect_runtime(install())).unwrap();
        assert!(result.is_none(), "a timeout carries on");
    }

    #[test]
    fn a_hook_that_throws_is_reported_and_core_carries_on() {
        let fake = FakeInvoker::fail(PluginError::new(
            PluginErrorCode::ScriptError,
            "TypeError: boom",
        ));
        let package = package(fake.clone());
        let result = block_on(package.detect_runtime(install())).unwrap();
        assert!(result.is_none());
        let reports = fake.reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].contains("TypeError: boom"));
    }

    #[test]
    fn a_hook_is_given_a_time_limit() {
        let fake = FakeInvoker::answer(serde_json::Value::Null);
        let package = package(fake.clone());
        block_on(package.detect_runtime(install())).unwrap();
        let timeouts = fake.timeouts.lock().unwrap();
        assert_eq!(timeouts.as_slice(), &[HOOK_TIMEOUT]);
    }

    fn game_context() -> GameContext {
        serde_json::from_value(serde_json::json!({
            "instance": "steam:test",
            "runtime": { "game": "test-game", "store": "steam", "version": "1.0" },
            "base": null,
            "frameworks": [],
            "layers": [],
            "load_order": { "entries": [], "rules": [] },
            "settings": {}
        }))
        .unwrap()
    }

    #[test]
    fn a_diagnostics_hook_that_fails_is_not_a_clean_launch() {
        // A diagnostics hook that throws or runs out of time must not come back
        // as an empty list, which launch checks show as "nothing wrong".
        for error in [
            PluginError::new(PluginErrorCode::ScriptError, "TypeError: boom"),
            PluginError::new(
                PluginErrorCode::Timeout,
                "`diagnose` did not finish within 5000ms",
            ),
        ] {
            let package = package_with(
                FakeInvoker::fail(error),
                serde_json::json!({ "diagnostics": "diagnose" }),
            );
            match block_on(package.diagnostics(game_context())) {
                Err(_) => {}
                Ok(found) => assert!(
                    !found.is_empty(),
                    "a failed diagnostics hook reads as no findings"
                ),
            }
        }
    }
    #[test]
    fn an_early_detect_call_cannot_stick() {
        let runtime = BehaviourRuntime::new();
        let package = ScriptedPackage::new(
            definition_with(serde_json::json!({ "detect_runtime": "detect" })),
            PluginId::parse("acme.test").unwrap(),
            runtime.clone(),
        );
        // Before the plugin service exists the answer is unknown, not "no
        // version", and nothing caches it.
        let early = block_on(package.detect_runtime(install())).unwrap_err();
        assert_eq!(early.code, "ERR_GAME_SCRIPT_HOST");

        // Once the service starts, the same package asks again and gets a
        // version. An early call did not stick.
        runtime.install(FakeInvoker::answer(serde_json::json!({
            "game": "test-game",
            "store": "steam",
            "version": "2.0"
        })));
        let later = block_on(package.detect_runtime(install()))
            .unwrap()
            .expect("a version once the runtime is available");
        assert_eq!(later.version, "2.0");
    }
}
