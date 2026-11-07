//! Check a plugin's game packages without installing or running it (`agora plugin validate`).
//!
//! The check reads each contributed package, resolves its inheritance as far as this plugin
//! allows, parses it, and runs the same [`GameRegistryBuilder::add`](crate::game_registry::GameRegistryBuilder::add)
//! validation that startup runs. Nothing is written and no data root is needed.
//!
//! A parent that lives in another plugin cannot be read here, so it is a note, not a problem. A
//! package that needs such a parent and cannot even parse alone is a note too. A cycle, a missing
//! template, a misspelled field or a package the registry would refuse is a problem naming the
//! file and, where the message carries one, the field.

use crate::game_registry::inherit::{resolve_partial, PackageInput};
use crate::game_registry::scripted::needs_entrypoint;
use crate::game_registry::{DeclarativePackage, GameRegistry, PackageSource};
use agora_game_api::{LoadOrderFormat, PackageDefinition, RelPath};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

/// The result of checking every game package a plugin contributes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePackageReport {
    /// Packages that parsed and passed registration validation.
    pub packages: Vec<CheckedGamePackage>,
    /// Author mistakes: each names the file and, when it is known, the field.
    pub problems: Vec<GamePackageProblem>,
    /// Things this folder cannot answer, such as a parent in another plugin.
    pub notes: Vec<String>,
}

impl GamePackageReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// One package that parsed and validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedGamePackage {
    pub file: String,
    pub id: String,
}

/// One problem in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePackageProblem {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl GamePackageProblem {
    fn new(file: impl Into<String>, field: Option<String>, message: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            field,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GamePackageProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.file)?;
        if let Some(field) = &self.field {
            write!(f, " ({field})")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Check the game packages a plugin folder contributes.
///
/// `package_paths` are the manifest's `contributions.gamePackages` paths. `entrypoint` is the
/// manifest's script entrypoint, if it has one.
pub fn validate_plugin(
    plugin_id: &str,
    folder: &Path,
    package_paths: &[String],
    entrypoint: Option<&str>,
) -> GamePackageReport {
    let mut report = GamePackageReport::default();
    let mut inputs = Vec::new();

    for path in package_paths {
        let Ok(rel) = RelPath::new(path) else {
            report.problems.push(GamePackageProblem::new(
                path.clone(),
                None,
                "this path leaves the plugin folder",
            ));
            continue;
        };
        let file = rel.as_str().to_string();
        let full = folder.join(rel.as_str());
        let content = match std::fs::read_to_string(&full) {
            Ok(content) => content,
            Err(error) => {
                report.problems.push(GamePackageProblem::new(
                    file.clone(),
                    None,
                    format!("cannot read this file: {error}"),
                ));
                continue;
            }
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(json) => json,
            Err(error) => {
                report.problems.push(GamePackageProblem::new(
                    file.clone(),
                    None,
                    format!("cannot parse this file: {error}"),
                ));
                continue;
            }
        };
        inputs.push(PackageInput {
            source: PackageSource::Plugin {
                plugin_id: plugin_id.to_string(),
            },
            blocked: false,
            label: file,
            json,
        });
    }

    let (resolved, resolve_problems, notes) = resolve_partial(inputs);
    report.notes.extend(notes);
    for problem in resolve_problems {
        let (file, message) = split_label(&problem);
        report.problems.push(GamePackageProblem::new(
            file,
            field_from_message(&message),
            message,
        ));
    }

    let mut builder = GameRegistry::builder();
    let mut checked: Vec<(String, PackageDefinition)> = Vec::new();
    for package in resolved {
        let file = package.label.clone();
        if let Some(reason) = needs_entrypoint(&package.definition) {
            if entrypoint.is_none() {
                report.problems.push(GamePackageProblem::new(
                    file.clone(),
                    None,
                    format!("{reason} but the plugin declares no `entrypoint` to run"),
                ));
            }
        }
        let definition = package.definition.clone();
        let package_for_registry = Arc::new(DeclarativePackage(definition.clone()))
            as Arc<dyn agora_game_api::GamePackage>;
        match builder.add(package.source.clone(), package_for_registry) {
            Ok(()) => {
                report.packages.push(CheckedGamePackage {
                    file: file.clone(),
                    id: definition.id.clone(),
                });
                checked.push((file, definition));
            }
            Err(error) => {
                report
                    .problems
                    .push(GamePackageProblem::new(file, None, error.to_string()))
            }
        }
    }

    if let Some(entrypoint) = entrypoint {
        check_script_exports(folder, entrypoint, &checked, &mut report);
    }

    report
}

/// Verify the script names a package declares are exported by the entrypoint, when its exports can
/// be read without running it. Re-exports and CommonJS are not followed: they are reported as not
/// checked rather than guessed.
fn check_script_exports(
    folder: &Path,
    entrypoint: &str,
    packages: &[(String, PackageDefinition)],
    report: &mut GamePackageReport,
) {
    let Ok(rel) = RelPath::new(entrypoint) else {
        report.notes.push(format!(
            "The script functions were not checked: `{entrypoint}` is not a path inside the plugin."
        ));
        return;
    };
    let source = match std::fs::read_to_string(folder.join(rel.as_str())) {
        Ok(source) => source,
        Err(error) => {
            report.notes.push(format!(
                "The script functions were not checked: `{entrypoint}` could not be read: {error}."
            ));
            return;
        }
    };
    let Some(exported) = exported_names(&source) else {
        report.notes.push(format!(
            "The script functions named in the game package were not checked: `{entrypoint}` uses \
             a re-export, a wildcard export or CommonJS, which cannot be read without running it."
        ));
        return;
    };

    for (file, definition) in packages {
        for (field, name) in required_exports(definition) {
            if !exported.contains(&name) {
                report.problems.push(GamePackageProblem::new(
                    file.clone(),
                    Some(field),
                    format!("the entrypoint exports no function named `{name}`"),
                ));
            }
        }
    }
}

/// Every script function a package names, with the field that names it.
fn required_exports(definition: &PackageDefinition) -> Vec<(String, String)> {
    let mut required = Vec::new();
    if let Some(behaviours) = &definition.behaviours {
        for (field, name) in [
            ("behaviours.detect_runtime", &behaviours.detect_runtime),
            ("behaviours.classify", &behaviours.classify),
            (
                "behaviours.recognize_download",
                &behaviours.recognize_download,
            ),
            ("behaviours.load_order_rules", &behaviours.load_order_rules),
            ("behaviours.diagnostics", &behaviours.diagnostics),
            ("behaviours.prepare_launch", &behaviours.prepare_launch),
        ] {
            if let Some(name) = name {
                required.push((field.to_string(), name.clone()));
            }
        }
    }
    for game in &definition.games {
        for order in &game.load_orders {
            if let LoadOrderFormat::Script { parse, render } = &order.format {
                required.push((
                    format!("games.{}.load_orders.{}.format.parse", game.id, order.id),
                    parse.clone(),
                ));
                required.push((
                    format!("games.{}.load_orders.{}.format.render", game.id, order.id),
                    render.clone(),
                ));
            }
            if order.rules.kind == "script" {
                if let Some(entry) = &order.rules.entry {
                    required.push((
                        format!("games.{}.load_orders.{}.rules.entry", game.id, order.id),
                        entry.clone(),
                    ));
                }
            }
        }
    }
    required
}

/// The named exports of an ES module, or `None` when reading them would require running it or
/// following another module.
fn exported_names(source: &str) -> Option<BTreeSet<String>> {
    let reexport = Regex::new(r"(?m)^\s*export\s*\*").expect("valid regex");
    let reexport_from = Regex::new(r"(?m)^\s*export\s*\{[^}]*\}\s*from\b").expect("valid regex");
    let common_js =
        Regex::new(r"(?m)^\s*(module\.exports|exports\.[A-Za-z_$])").expect("valid regex");
    if reexport.is_match(source) || reexport_from.is_match(source) || common_js.is_match(source) {
        return None;
    }

    let declarations = [
        Regex::new(r"(?m)^\s*export\s+(?:async\s+)?function\s+([A-Za-z_$][\w$]*)")
            .expect("valid regex"),
        Regex::new(r"(?m)^\s*export\s+(?:const|let|var)\s+([A-Za-z_$][\w$]*)")
            .expect("valid regex"),
        Regex::new(r"(?m)^\s*export\s+class\s+([A-Za-z_$][\w$]*)").expect("valid regex"),
    ];
    let mut names = BTreeSet::new();
    for declaration in &declarations {
        for capture in declaration.captures_iter(source) {
            names.insert(capture[1].to_string());
        }
    }

    let list = Regex::new(r"(?m)^\s*export\s*\{([^}]*)\}").expect("valid regex");
    for capture in list.captures_iter(source) {
        for item in capture[1].split(',') {
            let item = item.trim();
            if item.is_empty() || item == "default" {
                continue;
            }
            // `a as b` exports `b`.
            let name = item
                .split_once(" as ")
                .map(|(_, alias)| alias.trim())
                .unwrap_or(item);
            if !name.is_empty() {
                names.insert(name.to_string());
            }
        }
    }
    Some(names)
}

/// A resolved package's label is `"{file}: {message}"`; keep the two apart for reporting.
fn split_label(problem: &str) -> (String, String) {
    match problem.split_once(": ") {
        Some((file, message)) => (file.to_string(), message.to_string()),
        None => (String::new(), problem.to_string()),
    }
}

/// The field named by a serde message, when it names one.
fn field_from_message(message: &str) -> Option<String> {
    let field = Regex::new(r"(?:unknown|missing) field `([^`]+)`").expect("valid regex");
    field
        .captures(message)
        .map(|capture| capture[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_declarations_are_read_without_running_the_script() {
        let source = r#"
            export function classifyItem(args) { return null; }
            export async function diagnose(args) { return []; }
            export const redmodRules = (args) => [];
            export { parseModlist, renderModlist as render };
        "#;
        let names = exported_names(source).expect("plain exports are readable");
        for expected in [
            "classifyItem",
            "diagnose",
            "redmodRules",
            "parseModlist",
            "render",
        ] {
            assert!(names.contains(expected), "missing {expected}: {names:?}");
        }
    }

    #[test]
    fn a_reexport_is_reported_as_unchecked_rather_than_guessed() {
        assert!(exported_names("export * from './impl.js';").is_none());
        assert!(exported_names("export { classify } from './impl.js';").is_none());
        assert!(exported_names("module.exports = { classify };").is_none());
    }
}
