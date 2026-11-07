//! Package inheritance (MASTER_SPEC §30.12).
//!
//! All packages are read as raw JSON first, then resolved in dependency order, and only then parsed
//! into [`PackageDefinition`]. A parent is merged on raw JSON, so a game template may be partial: it
//! is the merge, not the template alone, that has to parse.
//!
//! Rules: objects merge deeply; scalars take the child's; arrays of objects with an `id` merge by
//! `id`, parent first, keeping a replaced item's position; other arrays concatenate parent first;
//! `null` removes a field. A missing parent, a missing template or a cycle is an error naming the
//! chain, and that package does not register. Other packages carry on.

use crate::game_registry::PackageSource;
use agora_game_api::PackageDefinition;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// One package read from a plugin, before inheritance is resolved.
#[derive(Debug, Clone)]
pub struct PackageInput {
    pub source: PackageSource,
    /// An official plugin whose files did not verify. It reserves its games but does not register.
    pub blocked: bool,
    /// How a warning names the package, e.g. `Plugin agora.creation-engine, games/package.json`.
    pub label: String,
    pub json: Value,
}

/// A package after inheritance, ready for the registry.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub source: PackageSource,
    pub blocked: bool,
    /// How a warning names the package, e.g. `Plugin agora.creation-engine, games/package.json`.
    pub label: String,
    pub definition: PackageDefinition,
}

/// Resolve every package that can be resolved. Returns the survivors and one warning per package
/// that could not be, each naming its chain. Resolution never stops the others.
pub fn resolve(inputs: Vec<PackageInput>) -> (Vec<ResolvedPackage>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut resolvable = vec![false; inputs.len()];

    for (i, input) in inputs.iter().enumerate() {
        let Some(id) = input.json.get("id").and_then(Value::as_str) else {
            warnings.push(format!(
                "{}: the package has no string id; skipped",
                input.label
            ));
            continue;
        };
        if !index.contains_key(id) {
            index.insert(id.to_string(), i);
        }
        // A later package with the same id still resolves and registers; the registry refuses it
        // if it claims a game or store product already taken. Parents resolve to the first.
        resolvable[i] = true;
    }

    let mut lineage_memo: HashMap<usize, Result<Vec<usize>, String>> = HashMap::new();
    let mut resolved = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        if !resolvable[i] {
            continue;
        }
        let mut visiting = Vec::new();
        let result: Result<PackageDefinition, String> = (|| {
            let order = lineage_of(i, &inputs, &index, &mut lineage_memo, &mut visiting)?;
            let mut merged = Value::Object(Map::new());
            for ancestor in order {
                merged = merge_json(&merged, &inputs[ancestor].json);
            }
            let mut own = input.json.clone();
            // The child keeps its own identity and its own parent list.
            own["id"] = Value::String(package_id(input));
            own["parents"] = Value::Array(
                parent_ids(&input.json)?
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            );
            merged = merge_json(&merged, &own);
            resolve_templates(&mut merged, &inputs[i])?;
            let definition: PackageDefinition = serde_json::from_value::<PackageDefinition>(merged)
                .map_err(|error| format!("package cannot be parsed: {error}"))?;
            Ok(definition)
        })();

        match result {
            Ok(definition) => resolved.push(ResolvedPackage {
                source: input.source.clone(),
                blocked: input.blocked,
                label: input.label.clone(),
                definition,
            }),
            Err(reason) => warnings.push(format!("{}: {reason}; skipped", input.label)),
        }
    }

    (resolved, warnings)
}

fn package_id(input: &PackageInput) -> String {
    input
        .json
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<unnamed>")
        .to_string()
}

/// The package ids a package names as parents.
fn parent_ids(json: &Value) -> Result<Vec<String>, String> {
    match json.get("parents") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "parents must be package ids".to_string())
            })
            .collect(),
        Some(_) => Err("parents must be an array of package ids".to_string()),
    }
}

/// The ancestors of a package in merge order: parents before children, each once. A diamond keeps
/// the shared ancestor once, which is what makes a diamond merge once.
fn lineage_of(
    i: usize,
    inputs: &[PackageInput],
    index: &HashMap<String, usize>,
    memo: &mut HashMap<usize, Result<Vec<usize>, String>>,
    visiting: &mut Vec<usize>,
) -> Result<Vec<usize>, String> {
    if let Some(cached) = memo.get(&i) {
        return cached.clone();
    }
    if let Some(start) = visiting.iter().position(|&seen| seen == i) {
        let chain: Vec<String> = visiting[start..]
            .iter()
            .map(|&seen| package_id(&inputs[seen]))
            .chain(std::iter::once(package_id(&inputs[i])))
            .collect();
        return Err(format!("package inheritance cycle: {}", chain.join(" -> ")));
    }
    visiting.push(i);
    let result = (|| {
        let own_id = package_id(&inputs[i]);
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        for parent in parent_ids(&inputs[i].json)? {
            if parent == own_id {
                return Err(format!("package '{own_id}' names itself as a parent"));
            }
            let Some(&parent_index) = index.get(&parent) else {
                return Err(format!(
                    "parent '{parent}' is not installed (chain: {own_id} -> {parent})"
                ));
            };
            for ancestor in lineage_of(parent_index, inputs, index, memo, visiting)? {
                if seen.insert(ancestor) {
                    order.push(ancestor);
                }
            }
            if seen.insert(parent_index) {
                order.push(parent_index);
            }
        }
        Ok(order)
    })();
    visiting.pop();
    memo.insert(i, result.clone());
    result
}

/// Merge every game's `extends` template into the game. Templates come from the merged package, so
/// they may be inherited from a parent.
fn resolve_templates(merged: &mut Value, input: &PackageInput) -> Result<(), String> {
    let templates = merged
        .get("game_templates")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(games) = merged.get_mut("games").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let package = package_id(input);
    for game in games.iter_mut() {
        let Some(template_id) = game
            .get("extends")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        let game_id = game
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>")
            .to_string();
        let Some(template) = templates.get(&template_id) else {
            return Err(format!(
                "game '{game_id}' extends template '{template_id}', which package '{package}' does not define"
            ));
        };
        *game = merge_json(template, game);
    }
    Ok(())
}

/// Merge `child` over `parent` by the §30.12 rules.
pub fn merge_json(parent: &Value, child: &Value) -> Value {
    match (parent, child) {
        (Value::Object(parent), Value::Object(child)) => {
            let mut merged = parent.clone();
            for (key, value) in child {
                if value.is_null() {
                    merged.remove(key);
                } else if let Some(existing) = merged.get(key).cloned() {
                    merged.insert(key.clone(), merge_json(&existing, value));
                } else {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Value::Object(merged)
        }
        (Value::Array(parent), Value::Array(child))
            if is_id_array(parent) && is_id_array(child) =>
        {
            merge_id_arrays(parent, child)
        }
        (Value::Array(parent), Value::Array(child)) => {
            let mut merged = parent.clone();
            merged.extend(child.clone());
            Value::Array(merged)
        }
        (_, child) => child.clone(),
    }
}

fn is_id_array(items: &[Value]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| {
            item.as_object()
                .and_then(|object| object.get("id"))
                .is_some_and(Value::is_string)
        })
}

fn merge_id_arrays(parent: &[Value], child: &[Value]) -> Value {
    let mut merged = parent.to_vec();
    for item in child {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        match merged
            .iter()
            .position(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
        {
            // A replaced item keeps its parent position.
            Some(position) => merged[position] = merge_json(&merged[position], item),
            None => merged.push(item.clone()),
        }
    }
    Value::Array(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(id: &str, games: Value, extra: Value) -> Value {
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

    fn merge_into(base: &mut Value, extra: Value) {
        *base = merge_json(base, &extra);
    }

    fn game(id: &str) -> Value {
        json!({
            "id": id,
            "name": id,
            "stores": [],
            "version_sources": [],
            "deployment": "redirect",
            "framework_ids": [],
            "tool_ids": [],
            "log_paths": [],
            "crash_paths": [],
            "user_files": [],
            "save_paths": [],
            "linked_archive_patterns": [],
        })
    }

    fn input(id: &str, json: Value) -> PackageInput {
        PackageInput {
            source: PackageSource::Plugin {
                plugin_id: id.to_string(),
            },
            blocked: false,
            label: format!("Plugin {id}"),
            json,
        }
    }

    #[test]
    fn a_child_scalar_wins() {
        let merged = merge_json(
            &json!({"name": "parent", "id": "x"}),
            &json!({"name": "child"}),
        );
        assert_eq!(merged["name"], "child");
        assert_eq!(merged["id"], "x");
    }

    #[test]
    fn arrays_with_ids_merge_by_id_keeping_position() {
        let parent = json!([{"id": "a", "value": 1}, {"id": "b", "value": 2}]);
        let child = json!([{"id": "c", "value": 3}, {"id": "a", "value": 9}]);
        let merged = merge_json(&parent, &child);
        assert_eq!(
            merged,
            json!([{"id": "a", "value": 9}, {"id": "b", "value": 2}, {"id": "c", "value": 3}])
        );
    }

    #[test]
    fn plain_arrays_concatenate_parent_first() {
        let merged = merge_json(&json!(["a", "b"]), &json!(["c"]));
        assert_eq!(merged, json!(["a", "b", "c"]));
    }

    #[test]
    fn null_removes_a_parent_field() {
        let merged = merge_json(&json!({"keep": 1, "drop": 2}), &json!({"drop": null}));
        assert_eq!(merged, json!({"keep": 1}));
    }

    #[test]
    fn a_partial_template_parses_only_after_merging() {
        let mut child_game = game("g");
        child_game["extends"] = json!("base");
        child_game.as_object_mut().unwrap().remove("deployment");
        let json = package(
            "child",
            json!([child_game]),
            json!({
                "game_templates": {
                    "base": { "deployment": "virtual_file_system" }
                }
            }),
        );
        let (resolved, warnings) = resolve(vec![input("child", json)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].definition.games[0].deployment,
            agora_game_api::DeploymentStrategy::VirtualFileSystem
        );
    }

    #[test]
    fn a_game_with_a_missing_template_is_refused_by_name() {
        let mut child_game = game("g");
        child_game["extends"] = json!("missing-template");
        let json = package("child", json!([child_game]), json!({}));
        let (resolved, warnings) = resolve(vec![input("child", json)]);
        assert!(resolved.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("missing-template"), "{warnings:?}");
        assert!(warnings[0].contains("child"), "{warnings:?}");
    }

    #[test]
    fn a_missing_parent_names_the_chain() {
        let mut json = package("child", json!([game("g")]), json!({}));
        json["parents"] = json!(["absent-parent"]);
        let (resolved, warnings) = resolve(vec![input("child", json)]);
        assert!(resolved.is_empty());
        assert!(warnings[0].contains("absent-parent"), "{warnings:?}");
        assert!(warnings[0].contains("child"), "{warnings:?}");
    }

    #[test]
    fn a_two_package_cycle_and_a_self_parent_are_refused() {
        let mut a = package("a", json!([game("ga")]), json!({}));
        a["parents"] = json!(["b"]);
        let mut b = package("b", json!([game("gb")]), json!({}));
        b["parents"] = json!(["a"]);
        let (resolved, warnings) = resolve(vec![input("a", a), input("b", b)]);
        assert!(resolved.is_empty(), "{resolved:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("cycle") && w.contains('a') && w.contains('b')),
            "{warnings:?}"
        );

        let mut self_parent = package("self", json!([game("gs")]), json!({}));
        self_parent["parents"] = json!(["self"]);
        let (resolved, warnings) = resolve(vec![input("self", self_parent)]);
        assert!(resolved.is_empty());
        assert!(
            warnings[0].contains("itself"),
            "self parent should be refused: {warnings:?}"
        );
    }

    #[test]
    fn a_broken_package_leaves_others_registered() {
        let good = package("good", json!([game("good-game")]), json!({}));
        let mut broken = package("broken", json!([game("broken-game")]), json!({}));
        broken["parents"] = json!(["missing"]);
        let (resolved, warnings) = resolve(vec![input("good", good), input("broken", broken)]);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].definition.id, "good");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_diamond_of_parents_merges_once() {
        let base = package(
            "base",
            json!([]),
            json!({
                "game_templates": {
                    "base": { "linked_archive_patterns": ["base"] }
                }
            }),
        );
        let mut left = package("left", json!([]), json!({}));
        left["parents"] = json!(["base"]);
        let mut right = package("right", json!([]), json!({}));
        right["parents"] = json!(["base"]);
        let mut top_game = game("top-game");
        top_game["extends"] = json!("base");
        let mut top = package("top", json!([top_game]), json!({}));
        top["parents"] = json!(["left", "right"]);

        // The template's plain array is where a double merge would show.
        let (resolved, warnings) = resolve(vec![
            input("base", base),
            input("left", left),
            input("right", right),
            input("top", top),
        ]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let top = resolved
            .iter()
            .find(|package| package.definition.id == "top")
            .expect("top resolves");
        let patterns = &top.definition.games[0].linked_archive_patterns;
        assert_eq!(patterns, &["base".to_string()]);
    }

    #[test]
    fn a_child_can_extend_a_template_inherited_from_a_parent() {
        let parent = package(
            "parent",
            json!([]),
            json!({ "game_templates": { "base": { "deployment": "virtual_file_system" } } }),
        );
        let mut child_game = game("g");
        child_game["extends"] = json!("base");
        child_game.as_object_mut().unwrap().remove("deployment");
        let mut child = package("child", json!([child_game]), json!({}));
        child["parents"] = json!(["parent"]);
        let (resolved, warnings) = resolve(vec![input("parent", parent), input("child", child)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let child = resolved
            .iter()
            .find(|package| package.definition.id == "child")
            .expect("child resolves");
        assert_eq!(
            child.definition.games[0].deployment,
            agora_game_api::DeploymentStrategy::VirtualFileSystem
        );
    }
}
