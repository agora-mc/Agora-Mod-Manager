//! `agora plugin validate`: everything a development folder contributes, without installing it.
//!
//! The manifest and crash packs were the first check. A plugin may also contribute game packages,
//! so those are checked here too, through the same registration validation startup uses (see
//! [`crate::game_registry::validate`]). Nothing is written and no data root is needed.

use crate::crash_packs::{load_plugin_packs, CrashPackReport};
use crate::error::LauncherResult;
use crate::game_registry::validate::{validate_plugin, GamePackageReport};
use crate::plugins::install::read_folder_manifest;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Everything checking a plugin folder found.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginValidationReport {
    /// Crash pack loading and linting, at the top level as it always was (`packs`, `problems`).
    #[serde(flatten)]
    pub crash: CrashPackReport,
    pub game_packages: GamePackageReport,
}

impl PluginValidationReport {
    pub fn is_clean(&self) -> bool {
        self.crash.is_clean() && self.game_packages.is_clean()
    }

    /// Every problem, crash packs first.
    pub fn problem_count(&self) -> usize {
        self.crash.problems.len() + self.game_packages.problems.len()
    }
}

/// Check a development folder's manifest, crash packs and game packages.
pub fn validate_folder(folder: &Path) -> LauncherResult<PluginValidationReport> {
    let manifest = read_folder_manifest(folder)?;
    let crash = load_plugin_packs(manifest.id.as_str(), folder, &manifest);
    let paths: Vec<String> = manifest
        .contributions
        .game_packages
        .iter()
        .map(|contribution| contribution.path.clone())
        .collect();
    let game_packages = validate_plugin(
        manifest.id.as_str(),
        folder,
        &paths,
        manifest.entrypoint.as_deref(),
    );
    Ok(PluginValidationReport {
        crash,
        game_packages,
    })
}
