//! The instance facts a package's conditions ask about (MASTER_SPEC §30.12).
//!
//! `present` is judged against the merged view the game will see, a framework's version against
//! the files the game will see, and the game version against the instance's base (or the detected
//! install for an unpinned instance). Anything that cannot be read answers unknown, never false.

use std::cell::RefCell;
use std::collections::BTreeMap;

use agora_game_api::{fold_path_case, ConditionFacts, FrameworkId, GameDefinition, RelPath};

use crate::ctx::Ctx;

/// The facts of one instance. Build it with [`InstanceFacts::for_instance`]; the fields are
/// computed once so a set of conditions sees one reading of the instance.
pub struct InstanceFacts<'a> {
    ctx: &'a Ctx,
    instance_id: &'a str,
    definition: &'a GameDefinition,
    /// The merged view the game will see, folded for comparison. `None` when it could not be read.
    visible: Option<Vec<String>>,
    game_version: Option<String>,
    frameworks: RefCell<BTreeMap<String, Option<Option<String>>>>,
}

impl<'a> InstanceFacts<'a> {
    /// Read the instance once: its merged view, its game version, and (lazily) its frameworks.
    pub fn for_instance(
        ctx: &'a Ctx,
        instance_id: &'a str,
        definition: &'a GameDefinition,
    ) -> Self {
        let visible = crate::game_instance::visible_mode(ctx, instance_id, definition)
            .and_then(|mode| {
                crate::game_deploy::visible_paths(ctx, instance_id, definition, mode)
                    .map_err(|e| crate::game_instance::InstanceError::Other(e.to_string()))
            })
            .ok()
            .map(|paths| paths.into_iter().map(|p| fold_path_case(&p)).collect());
        let game_version = detected_game_version(ctx, instance_id);
        Self {
            ctx,
            instance_id,
            definition,
            visible,
            game_version,
            frameworks: RefCell::new(BTreeMap::new()),
        }
    }

    /// Facts with a view and version chosen by the caller, for tests and callers that already read
    /// the view. A `None` view means it was unreadable.
    pub fn with_view(
        ctx: &'a Ctx,
        instance_id: &'a str,
        definition: &'a GameDefinition,
        visible: Option<Vec<String>>,
        game_version: Option<String>,
    ) -> Self {
        Self {
            ctx,
            instance_id,
            definition,
            visible: visible.map(|paths| paths.iter().map(|p| fold_path_case(p)).collect()),
            game_version,
            frameworks: RefCell::new(BTreeMap::new()),
        }
    }
}

impl ConditionFacts for InstanceFacts<'_> {
    fn present(&self, path: &RelPath) -> Option<bool> {
        let visible = self.visible.as_ref()?;
        let wanted = fold_path_case(path.as_str());
        Some(visible.iter().any(|known| known == &wanted))
    }

    fn framework(&self, framework: &FrameworkId) -> Option<Option<String>> {
        if let Some(known) = self.frameworks.borrow().get(framework.as_str()) {
            return known.clone();
        }
        let known = match self.ctx.games.framework(&self.definition.id, framework) {
            None => None,
            Some(declared) => match crate::game_frameworks::framework_status(
                self.ctx,
                self.instance_id,
                self.definition,
                declared,
            ) {
                Err(_) => None,
                Ok(crate::game_frameworks::FrameworkStatus::NotDetectable) => None,
                Ok(crate::game_frameworks::FrameworkStatus::Absent) => Some(None),
                Ok(crate::game_frameworks::FrameworkStatus::Present { version, .. }) => {
                    version.map(Some)
                }
            },
        };
        self.frameworks
            .borrow_mut()
            .insert(framework.to_string(), known.clone());
        known
    }

    fn game_version(&self) -> Option<String> {
        self.game_version.clone()
    }
}

/// The game version an instance records: its base's runtime version when pinned, else the version
/// discovery found for its install. `None` when it cannot be told.
pub fn detected_game_version(ctx: &Ctx, instance_id: &str) -> Option<String> {
    let record = crate::game_instance::get(ctx, instance_id).ok()??;
    match record.base {
        agora_game_api::BaseReference::Pinned { id, .. } => {
            let path = ctx.paths.base_manifest_path(&id);
            let text = std::fs::read_to_string(path).ok()?;
            let manifest: crate::game_base::BaseManifest = serde_json::from_str(&text).ok()?;
            Some(manifest.runtime.version)
        }
        agora_game_api::BaseReference::Unpinned { install, .. } => {
            let report = crate::game_discovery::discover_with_folders(&ctx.paths);
            report
                .installs
                .into_iter()
                .find(|found| {
                    crate::game_registry::make_install_id(&found.store, &found.product) == install
                })
                .and_then(|found| found.store_version)
        }
    }
}
