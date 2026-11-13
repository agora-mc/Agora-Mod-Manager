//! The platform contract's registration and inheritance rules (MASTER_SPEC §30.12).

use std::sync::Arc;

use agora_core::game_registry::inherit::{resolve, PackageInput};
use agora_core::game_registry::{
    DeclarativePackage, GameRegistry, GameRegistryError, PackageSource,
};
use agora_game_api::PackageDefinition;
use serde_json::json;

fn package(id: &str, games: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    let mut value = json!({
        "id": id,
        "version": "0.1.0",
        "api_range": ">=0.1, <0.2",
        "parents": [],
        "games": games,
        "frameworks": [],
        "tools": [],
    });
    merge_into(&mut value, extra);
    value
}

fn merge_into(base: &mut serde_json::Value, extra: serde_json::Value) {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
}

fn game(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "name": id,
        "stores": [{"store": "steam", "product": "12345"}],
        "version_sources": [],
        "deployment": "redirect",
        "framework_ids": [],
        "tool_ids": [],
        "log_paths": [],
        "crash_paths": [],
        "user_files": [{
            "source": {"root": "user_data", "location": "local_app_data", "path": "Game/Plugins.txt"},
            "instance_path": "user/Plugins.txt",
            "strategy": "journaled_swap",
            "stores": []
        }],
        "save_paths": [],
        "linked_archive_patterns": []
    })
}

fn plugin_input(id: &str, json: serde_json::Value) -> PackageInput {
    PackageInput {
        source: PackageSource::Plugin {
            plugin_id: id.to_string(),
        },
        blocked: false,
        label: format!("Plugin {id}"),
        json,
    }
}

fn register(definition: PackageDefinition) -> Result<(), GameRegistryError> {
    let mut builder = GameRegistry::builder();
    builder.add(
        PackageSource::Compiled {
            crate_name: "test".into(),
        },
        Arc::new(DeclarativePackage(definition)),
    )
}

#[test]
fn a_mount_with_dotdot_or_an_absolute_path_is_refused() {
    for mount in ["../outside", "C:/outside", "/outside"] {
        let mut mount_game = game("mount-game");
        mount_game["content_rules"] = json!([{
            "kind": "mod",
            "match": {"marker": "info.json"},
            "mount": mount
        }]);
        let json = package("mount-test", json!([mount_game]), json!({}));
        let parsed: Result<PackageDefinition, _> = serde_json::from_value(json);
        assert!(parsed.is_err(), "mount {mount:?} should be refused");
    }
}

#[test]
fn the_guide_minimum_example_parses() {
    // The example in docs/plugins/game-packages.md, "Start with the game". It names only what a
    // game must say; everything else has a sensible default.
    let package: PackageDefinition = serde_json::from_value(json!({
        "id": "example.my-game",
        "version": "0.1.0",
        "api_range": ">=0.1, <0.2",
        "games": [{
            "id": "my-game",
            "name": "My Game",
            "stores": [{ "store": "gog", "product": "1234567890" }],
            "version_sources": [{ "kind": "executable", "path": "bin/game.exe" }],
            "deployment": "virtual_file_system",
            "launch": {
                "executable": { "root": "runtime", "path": "bin/game.exe" },
                "working_directory": { "root": "runtime", "path": "" }
            }
        }]
    }))
    .expect("the guide's minimum example parses");

    assert_eq!(package.games.len(), 1);
    assert!(package.frameworks.is_empty());
    assert!(package.tools.is_empty());
    assert!(package.games[0].log_paths.is_empty());
}

#[test]
fn a_misspelled_game_field_fails_to_parse() {
    let mut misspelled = game("typo-game");
    misspelled["deploymnet"] = json!("redirect");
    let json = package("typo", json!([misspelled]), json!({}));
    let error = serde_json::from_value::<PackageDefinition>(json).unwrap_err();
    assert!(
        error.to_string().contains("deploymnet"),
        "the error must name the misspelled field: {error}"
    );
}

#[test]
fn a_rule_kind_of_anything_else_is_refused() {
    let json = package("rule-test", json!([game("rule-game")]), json!({}));
    let mut package: PackageDefinition = serde_json::from_value(json).expect("the package parses");
    package.games[0]
        .load_orders
        .push(agora_game_api::LoadOrderDefinition {
            id: "plugins".into(),
            name: "Plugins".into(),
            file: agora_game_api::RelPath::new("user/Plugins.txt").unwrap(),
            destination: Default::default(),
            items: agora_game_api::LoadOrderItems {
                folder: agora_game_api::RelPath::new("Data").unwrap(),
                patterns: vec!["*.esp".into()],
                folders: false,
            },
            format: agora_game_api::LoadOrderFormat::Lines {
                active_prefix: "*".into(),
                header: Vec::new(),
            },
            rules: agora_game_api::LoadOrderRules {
                kind: "made_up".into(),
                entry: None,
                implicit: Vec::new(),
                implicit_list_file: None,
                sorter: None,
            },

            intro: None,
            item_heading: None,
        });
    assert!(matches!(
        register(package),
        Err(GameRegistryError::InvalidLoadOrder { .. })
    ));
}

#[test]
fn a_child_cannot_claim_a_game_id_an_official_plugin_owns_through_a_parent() {
    // The official package owns `owned-game`; the community package inherits it through `parents`
    // and is refused, while the official package stays registered.
    let official = package("official", json!([game("owned-game")]), json!({}));
    let mut child = package("child", json!([]), json!({}));
    child["parents"] = json!(["official"]);

    let (resolved, warnings) = resolve(vec![
        plugin_input("official-plugin", official),
        plugin_input("community-plugin", child),
    ]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let official = resolved
        .iter()
        .find(|package| package.definition.id == "official")
        .expect("official resolves");
    let child = resolved
        .iter()
        .find(|package| package.definition.id == "child")
        .expect("child resolves");
    assert_eq!(child.definition.games[0].id.as_str(), "owned-game");

    let mut builder = GameRegistry::builder();
    builder
        .add(
            official.source.clone(),
            Arc::new(DeclarativePackage(official.definition.clone())),
        )
        .expect("official registers first");
    assert!(matches!(
        builder.add(
            child.source.clone(),
            Arc::new(DeclarativePackage(child.definition.clone()))
        ),
        Err(GameRegistryError::GameAlreadyRegistered { .. })
    ));
}
