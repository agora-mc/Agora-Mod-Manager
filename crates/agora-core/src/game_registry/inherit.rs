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
use std::collections::{BTreeMap, HashMap, HashSet};

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

/// Which plugin declared each script entrypoint, recorded while packages are merged
/// (MASTER_SPEC §30.12). An inherited behaviour runs in the plugin that wrote it, with that
/// plugin's grants, not in the child that inherited it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BehaviourOwners {
    /// A `behaviours` field name (`classify`, `load_order_rules`, ...) to its plugin id.
    pub behaviours: BTreeMap<String, String>,
    /// A load order's script entrypoint to its plugin id. Keys are built by [`script_key`].
    pub scripts: BTreeMap<String, String>,
}

/// The key that records one load order script slot: `parse`, `render` or `rules`.
pub fn script_key(game: &str, list: &str, slot: &str) -> String {
    format!("{game}/{list}/{slot}")
}

impl BehaviourOwners {
    /// The plugin that owns a `behaviours` field, if it is not this package's own.
    pub fn behaviour(&self, field: &str) -> Option<&str> {
        self.behaviours.get(field).map(String::as_str)
    }

    /// The plugin that owns one load order script slot.
    pub fn script(&self, game: &str, list: &str, slot: &str) -> Option<&str> {
        self.scripts
            .get(&script_key(game, list, slot))
            .map(String::as_str)
    }
}

/// A package after inheritance, ready for the registry.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub source: PackageSource,
    pub blocked: bool,
    /// How a warning names the package, e.g. `Plugin agora.creation-engine, games/package.json`.
    pub label: String,
    pub definition: PackageDefinition,
    pub owners: BehaviourOwners,
}

/// Resolve every package that can be resolved. Returns the survivors and one warning per package
/// that could not be, each naming its chain. Resolution never stops the others.
pub fn resolve(inputs: Vec<PackageInput>) -> (Vec<ResolvedPackage>, Vec<String>) {
    let (resolved, warnings, _) = resolve_inner(inputs, false);
    (resolved, warnings)
}

/// Resolve what can be resolved when some parents may live in another plugin (the `agora plugin
/// validate` check). A parent that is not among `inputs` is a note saying it cannot be checked
/// here, not an error; a cycle, a missing template or an unparsable package is still an error.
///
/// Returns the packages that resolved, the problems, and the notes.
pub fn resolve_partial(
    inputs: Vec<PackageInput>,
) -> (Vec<ResolvedPackage>, Vec<String>, Vec<String>) {
    resolve_inner(inputs, true)
}

fn resolve_inner(
    inputs: Vec<PackageInput>,
    lenient: bool,
) -> (Vec<ResolvedPackage>, Vec<String>, Vec<String>) {
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut resolvable = vec![false; inputs.len()];

    for (i, input) in inputs.iter().enumerate() {
        let Some(id) = input.json.get("id").and_then(Value::as_str) else {
            problems.push(format!(
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

    let mut lineage_memo: HashMap<usize, Result<(Vec<usize>, bool), String>> = HashMap::new();
    let mut resolved = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        if !resolvable[i] {
            continue;
        }
        let mut visiting = Vec::new();
        let mut missing_parent = false;
        let result: Result<(PackageDefinition, BehaviourOwners), String> = (|| {
            let (order, missing) = lineage_of(
                i,
                &inputs,
                &index,
                &mut lineage_memo,
                &mut visiting,
                lenient,
                &mut notes,
            )?;
            missing_parent = missing;
            let mut merged = Value::Object(Map::new());
            for &ancestor in &order {
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
            let owners = owners_of(&order_with_child(&order, i), &inputs, &merged);
            let definition: PackageDefinition = serde_json::from_value::<PackageDefinition>(merged)
                .map_err(|error| format!("package cannot be parsed: {error}"))?;
            Ok((definition, owners))
        })();

        match result {
            Ok((definition, owners)) => resolved.push(ResolvedPackage {
                source: input.source.clone(),
                blocked: input.blocked,
                label: input.label.clone(),
                definition,
                owners,
            }),
            // A package that needs an absent parent may not even parse alone. That is not the
            // author's mistake to fix in this folder, so it is a note, not a problem.
            Err(reason) if lenient && missing_parent => {
                notes.push(format!("{}: cannot be checked here: {reason}", input.label));
            }
            Err(reason) => problems.push(format!("{}: {reason}; skipped", input.label)),
        }
    }

    (resolved, problems, notes)
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
    memo: &mut HashMap<usize, Result<(Vec<usize>, bool), String>>,
    visiting: &mut Vec<usize>,
    lenient: bool,
    notes: &mut Vec<String>,
) -> Result<(Vec<usize>, bool), String> {
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
        let mut missing = false;
        for parent in parent_ids(&inputs[i].json)? {
            if parent == own_id {
                return Err(format!("package '{own_id}' names itself as a parent"));
            }
            let Some(&parent_index) = index.get(&parent) else {
                if lenient {
                    notes.push(format!(
                        "{}: parent '{parent}' is not in this plugin, so it cannot be checked here",
                        inputs[i].label
                    ));
                    missing = true;
                    continue;
                }
                return Err(format!(
                    "parent '{parent}' is not installed (chain: {own_id} -> {parent})"
                ));
            };
            let (ancestors, ancestor_missing) =
                lineage_of(parent_index, inputs, index, memo, visiting, lenient, notes)?;
            missing |= ancestor_missing;
            for ancestor in ancestors {
                if seen.insert(ancestor) {
                    order.push(ancestor);
                }
            }
            if seen.insert(parent_index) {
                order.push(parent_index);
            }
        }
        Ok((order, missing))
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

fn order_with_child(order: &[usize], child: usize) -> Vec<usize> {
    let mut sequence = order.to_vec();
    sequence.push(child);
    sequence
}

fn plugin_of(input: &PackageInput) -> Option<&String> {
    match &input.source {
        PackageSource::Plugin { plugin_id } => Some(plugin_id),
        PackageSource::Compiled { .. } => None,
    }
}

/// The game object with `id` in one package's JSON, if it defines one.
fn input_game<'a>(json: &'a Value, game_id: &str) -> Option<&'a Value> {
    json.get("games")
        .and_then(Value::as_array)?
        .iter()
        .find(|game| game.get("id").and_then(Value::as_str) == Some(game_id))
}

fn record_script_slots(
    value: &Value,
    game: &str,
    plugin: &str,
    owners: &mut BTreeMap<String, String>,
) {
    let Some(orders) = value.get("load_orders").and_then(Value::as_array) else {
        return;
    };
    for order in orders {
        let Some(list) = order.get("id").and_then(Value::as_str) else {
            continue;
        };
        let format = order.get("format");
        for slot in ["parse", "render"] {
            if format
                .and_then(|format| format.get(slot))
                .and_then(Value::as_str)
                .is_some()
            {
                owners.insert(script_key(game, list, slot), plugin.to_string());
            }
        }
        let rules = order.get("rules");
        if rules
            .and_then(|rules| rules.get("entry"))
            .and_then(Value::as_str)
            .is_some()
        {
            owners.insert(script_key(game, list, "rules"), plugin.to_string());
        }
        if rules
            .and_then(|rules| rules.get("sorter"))
            .and_then(|sorter| sorter.get("hook"))
            .and_then(Value::as_str)
            .is_some()
        {
            owners.insert(script_key(game, list, "sorter"), plugin.to_string());
        }
    }
}

/// Record which plugin wrote each script entrypoint, in merge order (parents first, then the
/// child). A child that overrides a behaviour owns it; an inherited one stays its parent's.
fn owners_of(sequence: &[usize], inputs: &[PackageInput], merged: &Value) -> BehaviourOwners {
    let mut owners = BehaviourOwners::default();
    for &index in sequence {
        let Some(plugin) = plugin_of(&inputs[index]) else {
            continue;
        };
        if let Some(behaviours) = inputs[index]
            .json
            .get("behaviours")
            .and_then(Value::as_object)
        {
            for (field, value) in behaviours {
                if !value.is_null() {
                    owners.behaviours.insert(field.clone(), plugin.clone());
                }
            }
        }
    }

    let Some(games) = merged.get("games").and_then(Value::as_array) else {
        return owners;
    };
    for game in games {
        let game_id = game.get("id").and_then(Value::as_str).unwrap_or_default();
        let extends = game.get("extends").and_then(Value::as_str);
        // The merged game's own fields win over its template, so record them apart and let the
        // game's owners take precedence.
        let mut from_game = BTreeMap::new();
        let mut from_template = BTreeMap::new();
        for &index in sequence {
            let Some(plugin) = plugin_of(&inputs[index]) else {
                continue;
            };
            if let Some(value) = input_game(&inputs[index].json, game_id) {
                record_script_slots(value, game_id, plugin, &mut from_game);
            }
            if let Some(template_id) = extends {
                if let Some(value) = inputs[index]
                    .json
                    .get("game_templates")
                    .and_then(Value::as_object)
                    .and_then(|templates| templates.get(template_id))
                {
                    record_script_slots(value, game_id, plugin, &mut from_template);
                }
            }
        }
        owners.scripts.extend(from_template);
        owners.scripts.extend(from_game);
    }
    owners
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
    fn an_omitted_list_inherits_rather_than_clearing() {
        let mut parent_game = game("g");
        parent_game["linked_archive_patterns"] = json!(["Data/*.bsa"]);
        let parent = package("parent", json!([parent_game]), json!({}));

        let mut child_game = game("g");
        child_game
            .as_object_mut()
            .unwrap()
            .remove("linked_archive_patterns");
        let mut child = package("child", json!([child_game]), json!({}));
        child["parents"] = json!(["parent"]);

        let (resolved, warnings) = resolve(vec![input("parent", parent), input("child", child)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let child = resolved
            .iter()
            .find(|package| package.definition.id == "child")
            .expect("child resolves");
        assert_eq!(
            child.definition.games[0].linked_archive_patterns,
            vec!["Data/*.bsa".to_string()]
        );
    }

    #[test]
    fn a_null_list_in_a_child_still_removes_the_inherited_one() {
        let mut parent_game = game("g");
        parent_game["linked_archive_patterns"] = json!(["Data/*.bsa"]);
        let parent = package("parent", json!([parent_game]), json!({}));

        let mut child_game = game("g");
        child_game["linked_archive_patterns"] = Value::Null;
        let mut child = package("child", json!([child_game]), json!({}));
        child["parents"] = json!(["parent"]);

        let (resolved, warnings) = resolve(vec![input("parent", parent), input("child", child)]);
        assert!(warnings.is_empty(), "{warnings:?}");
        let child = resolved
            .iter()
            .find(|package| package.definition.id == "child")
            .expect("child resolves");
        assert!(child.definition.games[0].linked_archive_patterns.is_empty());
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
