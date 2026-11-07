//! One real plugin, every platform hook (MASTER_SPEC §30.12).
//!
//! The package's script supplies `detect_runtime`, load-order rules, a load-order format,
//! `classify` and `diagnostics`. Each answer carries a marker only the script can produce, so a
//! hook that never reached the real QuickJS host cannot pass. `detect_runtime` reads a text file
//! through the install's game-path view.

use std::sync::Arc;

use agora_core::app_paths::AppPaths;
use agora_core::ctx::CoreContext;
use agora_core::game_registry::GameRegistry;
use agora_core::plugins::service::PluginService;
use agora_core::plugins::store::{self, PluginSource};
use agora_core::plugins::PLUGINS_EMERGENCY_STOP_SETTING;
use agora_game_api::{
    BaseReference, GameContext, GameDefinition, GameId, GameInstall, InstallId, LayerStack,
    LoadOrder, LoadOrderRule, RuntimeIdentity,
};
use agora_plugin_api::capability::{Capability, CapabilitySet};
use agora_plugin_api::manifest::PluginManifest;
use agora_plugin_host::QuickJsHost;

const PLUGIN: &str = "acme.platform";
const GAME: &str = "scripted-game";

fn package_json() -> serde_json::Value {
    serde_json::json!({
        "id": "acme.platform.package",
        "version": "0.1.0",
        "api_range": ">=0.1, <0.2",
        "parents": [],
        "behaviours": {
            "detect_runtime": "detectRuntime",
            "classify": "classifyItem",
            "load_order_rules": "rulesFor",
            "diagnostics": "diagnose"
        },
        "games": [{
            "id": GAME,
            "name": "Scripted Game",
            "stores": [],
            "version_sources": [],
            "deployment": "redirect",
            "content_rules": [{
                "kind": "never",
                "match": { "marker": "never.marker" },
                "mount": "never"
            }],
            "framework_ids": [],
            "tool_ids": [],
            "log_paths": [],
            "crash_paths": [],
            "user_files": [],
            "save_paths": [],
            "linked_archive_patterns": [],
            "load_orders": [{
                "id": "plugins",
                "name": "Plugins",
                "file": "user/Plugins.txt",
                "items": { "folder": "Data", "patterns": ["*.esp"] },
                "format": { "type": "script", "parse": "parseList", "render": "renderList" },
                "rules": { "kind": "script", "entry": "rulesFor" }
            }]
        }],
        "frameworks": [],
        "tools": []
    })
}

fn script() -> &'static str {
    r#"
import { game } from "agora";

export function detectRuntime(_args) {
  const lines = game.readLines("VERSION.txt").lines;
  return { game: "scripted-game", store: "steam", version: lines[0].trim() + "-script-marker" };
}

export function rulesFor(_args) {
  return [{ kind: "before", first: "A.esp", second: "B.esp" }];
}

export function parseList(args) {
  const text = args.bytes.map((byte) => String.fromCharCode(byte)).join("");
  return text
    .split("\n")
    .filter((line) => line.length > 0)
    .map((line) => {
      const [name, state] = line.split("|");
      return { name, active: state === "on" };
    });
}

export function renderList(args) {
  const body = args.lines
    .map((line) => line.name + "|" + (line.active ? "on" : "off"))
    .join("\n");
  return "script-rendered\n" + body;
}

export function classifyItem(_args) {
  return {
    type: "place",
    source_path: "",
    mount_path: "assets/script-classified",
    reason: "classify-ran-marker"
  };
}

export function diagnose(_args) {
  return [{
    code: "SCRIPT_DIAG",
    message: "diagnostics-ran-marker",
    severity: "warning",
    repairs: []
  }];
}
"#
}

fn manifest() -> PluginManifest {
    serde_json::from_value(serde_json::json!({
        "manifest": 1,
        "id": PLUGIN,
        "name": "Platform tracer",
        "version": "0.1.0",
        "license": "GPL-3.0-only",
        "apiRange": ">=0.1, <0.2",
        "entrypoint": "main.js",
        "capabilities": { "required": ["game:define", "game:read"] },
        "contributions": {
            "gamePackages": [{ "path": "games/package.json" }]
        }
    }))
    .unwrap()
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the test")
        .block_on(future)
}

fn game_context(runtime: RuntimeIdentity) -> GameContext {
    GameContext {
        instance: "e2e-1".into(),
        runtime,
        base: Some(BaseReference::Unpinned {
            install: InstallId::new("steam:test").unwrap(),
            reason: "test".into(),
        }),
        frameworks: Vec::new(),
        layers: LayerStack::default(),
        load_order: LoadOrder::default(),
        settings: Default::default(),
    }
}

#[test]
fn a_real_plugin_supplies_every_platform_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_root(tmp.path().to_path_buf());
    paths.create_required_dirs().unwrap();
    agora_core::db::init_local_state_db(&paths.local_state_db()).unwrap();

    // Install the plugin package on disk and in the store, as an install would.
    let plugin_dir = paths.plugin_packages_root().join(PLUGIN);
    std::fs::create_dir_all(plugin_dir.join("games")).unwrap();
    std::fs::write(
        plugin_dir.join("games/package.json"),
        serde_json::to_vec(&package_json()).unwrap(),
    )
    .unwrap();
    std::fs::write(plugin_dir.join("main.js"), script()).unwrap();

    let manifest = manifest();
    let mut granted = CapabilitySet::new();
    granted.insert(Capability::GameDefine);
    granted.insert(Capability::GameRead);
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        agora_core::db::set_setting(
            &conn,
            PLUGINS_EMERGENCY_STOP_SETTING,
            &serde_json::Value::Bool(false),
        )
        .unwrap();
        store::upsert(
            &conn,
            &manifest,
            &granted,
            &PluginSource::Package,
            &plugin_dir,
            true,
            "2026-11-06T00:00:00Z",
        )
        .unwrap();
    }

    let (ctx, warnings) = CoreContext::initialize(paths.clone(), GameRegistry::builder()).unwrap();
    assert!(
        !warnings.iter().any(|warning| warning.contains(PLUGIN)),
        "{warnings:?}"
    );
    let game_id = GameId::new(GAME).unwrap();
    let definition: &GameDefinition = ctx.games.game(&game_id).expect("the game is registered");
    assert_eq!(
        definition.load_orders[0].format,
        agora_game_api::LoadOrderFormat::Script {
            parse: "parseList".into(),
            render: "renderList".into(),
        }
    );

    // The real plugin service installs the bridge the package's hooks call.
    let service = PluginService::headless(ctx.clone(), Arc::new(QuickJsHost::new()));
    service.reload().expect("the plugin records load");
    let plugin_id = agora_plugin_api::manifest::PluginId::parse(PLUGIN).unwrap();
    service
        .set_scripts_allowed(&plugin_id, true)
        .expect("scripts are allowed for the test");

    let package = ctx
        .games
        .package_for(&game_id)
        .expect("the package is registered")
        .clone();

    // detect_runtime reads a text file by game path through the install's view.
    let install_dir = tmp.path().join("install");
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::write(install_dir.join("VERSION.txt"), b"1.2.3\n").unwrap();
    let install = GameInstall {
        id: InstallId::new("steam:test").unwrap(),
        runtime: RuntimeIdentity {
            game: game_id.clone(),
            store: agora_game_api::StoreId::steam(),
            version: "0".into(),
            build: None,
        },
        kind: agora_game_api::InstallKind::BaseGame,
        location: install_dir.to_string_lossy().to_string(),
        volume: None,
        capabilities: agora_game_api::InstallCapabilities {
            executables_readable: true,
            accepts_new_files: true,
            relocatable: true,
        },
    };
    let identity = block_on(package.detect_runtime(install))
        .expect("detect_runtime runs")
        .expect("the script found a version");
    assert_eq!(identity.version, "1.2.3-script-marker");

    let runtime = identity.clone();
    let context = game_context(runtime);

    // Script rules, format and diagnostics each answer with a marker only the script has.
    let rules = block_on(package.load_order_rules(context.clone(), "plugins"))
        .expect("rules run")
        .expect("the script supplies rules");
    assert_eq!(
        rules,
        vec![LoadOrderRule::Before {
            first: "A.esp".into(),
            second: "B.esp".into(),
        }]
    );

    let parsed = block_on(package.parse_load_order(
        context.clone(),
        "plugins",
        b"A.esp|on\nB.esp|off\n".to_vec(),
    ))
    .expect("parse runs")
    .expect("the script supplies a parser");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].name, "A.esp");
    assert!(parsed[0].active);
    assert!(!parsed[1].active);

    let rendered = block_on(package.render_load_order(context.clone(), "plugins", parsed))
        .expect("render runs")
        .expect("the script supplies a renderer");
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.starts_with("script-rendered\n"), "{rendered}");

    let diagnostics = block_on(package.diagnostics(context.clone())).expect("diagnostics run");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "diagnostics-ran-marker");

    // classify is not called directly: `add_to_instance` reaches it after the content rule and
    // layout fail, which is the real caller the platform contract needs.
    let base = BaseReference::Unpinned {
        install: InstallId::new("steam:test").unwrap(),
        reason: "test".into(),
    };
    {
        let conn = agora_core::db::local_state_connection(&paths.local_state_db()).unwrap();
        conn.execute(
            "INSERT INTO game_instances (instance_id, game, name, base_json, created_at)
             VALUES ('e2e-1', ?1, 'Scripted', ?2, '2026-11-06T00:00:00Z')",
            rusqlite::params![GAME, serde_json::to_string(&base).unwrap()],
        )
        .unwrap();
    }
    let instance_dir = ctx.paths.instance_dir("e2e-1").unwrap();
    std::fs::create_dir_all(&instance_dir).unwrap();
    let instance_manifest = agora_core::game_instance::GameInstanceManifest::new(
        game_id.clone(),
        "e2e-1",
        "Scripted",
        Some(identity),
        base,
    );
    std::fs::write(
        instance_dir.join("instance_manifest.json"),
        serde_json::to_vec(&instance_manifest).unwrap(),
    )
    .unwrap();

    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("notes.txt"), b"notes").unwrap();
    let item = agora_core::content_store::add_folder(&ctx, source.path(), Some("Notes"))
        .expect("the item is stored")
        .item()
        .item_id
        .clone();

    match agora_core::game_deploy::add_to_instance(&ctx, "e2e-1", &item) {
        Ok(agora_core::game_deploy::AddToInstance::Added {
            layer,
            placement_reason,
            ..
        }) => {
            assert_eq!(layer.mount_path.as_str(), "assets/script-classified");
            assert_eq!(
                placement_reason.as_deref(),
                Some("classify-ran-marker"),
                "add_to_instance must reach the script's classify hook"
            );
        }
        Ok(other) => panic!("expected the item to be placed, got {other:?}"),
        Err(error) => panic!("add_to_instance failed: {error}"),
    }
}
