//! An inherited script behaviour runs in the plugin that wrote it (MASTER_SPEC §30.12). Ownership
//! is recorded while packages merge, so a child that inherits a hook does not need an entrypoint
//! of its own and does not run its parent's code with its own grants.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agora_core::game_registry::inherit::{resolve, PackageInput};
use agora_core::game_registry::scripted::{BehaviourInvoker, BehaviourRuntime, ScriptedPackage};
use agora_core::game_registry::PackageSource;
use agora_game_api::{GameInstall, GamePackage};
use agora_plugin_api::error::PluginError;
use agora_plugin_api::host::GameReads;
use agora_plugin_api::manifest::PluginId;

#[derive(Default)]
struct Calls {
    seen: Mutex<Vec<(String, String)>>,
}

struct FakeInvoker {
    calls: Arc<Calls>,
}

impl BehaviourInvoker for FakeInvoker {
    fn invoke(
        &self,
        plugin_id: &PluginId,
        export: &str,
        _args: serde_json::Value,
        _timeout: Duration,
    ) -> Result<serde_json::Value, PluginError> {
        self.calls
            .seen
            .lock()
            .unwrap()
            .push((plugin_id.to_string(), export.to_string()));
        if export.contains("Detect") {
            Ok(serde_json::json!({
                "game": "test-game",
                "store": "steam",
                "version": "1.0"
            }))
        } else if export.contains("Rules") {
            Ok(serde_json::json!({ "rules": [] }))
        } else {
            Ok(serde_json::json!({
                "type": "unknown",
                "top_level": ["data"],
                "reason": "the child's script answered"
            }))
        }
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

    fn report(&self, _plugin_id: &PluginId, _message: &str) {}
}

fn input(plugin: &str, package: serde_json::Value) -> PackageInput {
    PackageInput {
        source: PackageSource::Plugin {
            plugin_id: plugin.to_string(),
        },
        blocked: false,
        label: format!("Plugin {plugin}"),
        json: package,
    }
}

fn game() -> serde_json::Value {
    serde_json::json!({
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
        "load_orders": [{
            "id": "plugins",
            "name": "Plugins",
            "file": "user/Plugins.txt",
            "items": { "folder": "Data", "patterns": ["*.esp"], "folders": false },
            "format": { "type": "lines", "active_prefix": "*" },
            "rules": { "kind": "script" }
        }]
    })
}

fn parent_package(behaviours: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "base",
        "version": "0.1.0",
        "api_range": ">=0.1, <0.2",
        "parents": [],
        "games": [],
        "frameworks": [],
        "tools": [],
        "behaviours": behaviours
    })
}

fn child_package(behaviours: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": "child",
        "version": "0.1.0",
        "api_range": ">=0.1, <0.2",
        "parents": ["base"],
        "games": [game()],
        "frameworks": [],
        "tools": [],
        "behaviours": behaviours
    })
}

fn child_with(
    parent: serde_json::Value,
    child: serde_json::Value,
) -> (ScriptedPackage, Arc<Calls>) {
    let (resolved, warnings) = resolve(vec![
        input("agora.base", parent_package(parent)),
        input("agora.child", child_package(child)),
    ]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let package = resolved
        .into_iter()
        .find(|package| package.definition.id == "child")
        .expect("the child resolves");
    let calls = Arc::new(Calls::default());
    let runtime = BehaviourRuntime::new();
    runtime.install(Arc::new(FakeInvoker {
        calls: calls.clone(),
    }));
    let scripted = ScriptedPackage::new(
        package.definition,
        PluginId::parse("agora.child").unwrap(),
        package.owners,
        runtime,
    );
    // The fake is the only invoker, so anything the package calls is recorded there.
    (scripted, calls)
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
fn an_inherited_behaviour_runs_in_the_parent_plugins_script() {
    let (package, calls) = child_with(
        serde_json::json!({ "detect_runtime": "parentDetect" }),
        serde_json::json!({}),
    );
    block_on(package.detect_runtime(install()))
        .expect("the hook runs")
        .expect("the parent's script answered");
    assert_eq!(
        calls.seen.lock().unwrap().as_slice(),
        [("agora.base".to_string(), "parentDetect".to_string())]
    );
}

#[test]
fn an_overridden_behaviour_runs_in_the_child_plugins_script() {
    let (package, calls) = child_with(
        serde_json::json!({ "classify": "parentClassify", "detect_runtime": "parentDetect" }),
        serde_json::json!({ "classify": "childClassify" }),
    );
    let context = serde_json::from_value(serde_json::json!({
        "instance": "steam:test",
        "runtime": { "game": "test-game", "store": "steam", "version": "1.0" },
        "base": null,
        "frameworks": [],
        "layers": [],
        "load_order": { "entries": [], "rules": [] },
        "settings": {}
    }))
    .unwrap();
    block_on(package.classify(context, "item", Vec::new()))
        .expect("the hook runs")
        .expect("the child's script answered");
    block_on(package.detect_runtime(install()))
        .expect("the hook runs")
        .expect("the parent's script answered");
    assert_eq!(
        calls.seen.lock().unwrap().as_slice(),
        [
            ("agora.child".to_string(), "childClassify".to_string()),
            ("agora.base".to_string(), "parentDetect".to_string()),
        ]
    );
}

#[test]
fn an_inherited_behaviour_uses_the_parent_plugins_grants() {
    // The invoker looks the caller's granted capabilities up by the plugin id it is given, so an
    // inherited hook reaching its parent's id is what lets it read the game with the parent's
    // `game:read` grant, even when the child did not ask for it.
    let (package, calls) = child_with(
        serde_json::json!({ "load_order_rules": "parentRules" }),
        serde_json::json!({}),
    );
    let context = serde_json::from_value(serde_json::json!({
        "instance": "steam:test",
        "runtime": { "game": "test-game", "store": "steam", "version": "1.0" },
        "base": null,
        "frameworks": [],
        "layers": [],
        "load_order": { "entries": [], "rules": [] },
        "settings": {}
    }))
    .unwrap();
    block_on(package.load_order_rules(context, "plugins"))
        .expect("the hook runs")
        .expect("the parent's script answered");
    assert_eq!(
        calls.seen.lock().unwrap().as_slice(),
        [("agora.base".to_string(), "parentRules".to_string())]
    );
}
