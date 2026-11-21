//! One place that turns an instance id into the view the crash engine reads.
//!
//! A generic game instance is read by core itself. A game with its own instance system (for
//! example the one whose files Agora manages directly) is supplied by that game package through
//! [`ExtraInstanceProvider`], so core never names a game. The CLI, the live session path and the
//! desktop all call [`resolve_crash_instance`], so they cannot disagree about which instance a
//! crash came from.

use agora_game_api::crash::CouldNotCheck;

use crate::crash_engine::attribution::{from_instance, InstanceView};
use crate::ctx::Ctx;
use crate::error::{LauncherError, LauncherResult};

/// An instance as the crash engine reads it: the game, its version, who owns each file, and the
/// gaps the instance itself admits to.
pub struct ResolvedCrashInstance {
    pub game_id: String,
    pub runtime_version: Option<String>,
    pub view: InstanceView,
    pub could_not_check: Vec<CouldNotCheck>,
}

/// A game package's own instance system, consulted when the id is not a generic game instance.
pub trait ExtraInstanceProvider {
    fn resolve(
        &self,
        ctx: &Ctx,
        instance_id: &str,
    ) -> LauncherResult<Option<ResolvedCrashInstance>>;
}

/// Resolve `instance_id` once, for every caller. A game package that owns an instance system is
/// asked first, because its manifest may also parse as a generic one; a generic game instance is
/// read by core when that provider does not know the id. An id neither knows is an error, never an
/// empty view that would make every ref look foreign.
pub fn resolve_crash_instance(
    ctx: &Ctx,
    instance_id: &str,
    extra: Option<&dyn ExtraInstanceProvider>,
) -> LauncherResult<ResolvedCrashInstance> {
    if let Some(extra) = extra {
        if let Some(found) = extra.resolve(ctx, instance_id)? {
            return Ok(found);
        }
    }
    match crate::game_instance::get_manifest(ctx, instance_id) {
        Ok(manifest) => {
            let view = from_instance(ctx, instance_id).map_err(|error| LauncherError::Generic {
                code: "ERR_CRASH_INSTANCE_VIEW".to_string(),
                message: format!("Could not read instance '{instance_id}': {error}"),
            })?;
            Ok(ResolvedCrashInstance {
                game_id: manifest.game.as_str().to_string(),
                runtime_version: runtime_version(&manifest),
                view,
                could_not_check: Vec::new(),
            })
        }
        Err(generic_error) => Err(LauncherError::Generic {
            code: "ERR_CRASH_INSTANCE_UNKNOWN".to_string(),
            message: format!("Unknown instance '{instance_id}': {generic_error}"),
        }),
    }
}

fn runtime_version(manifest: &crate::game_instance::GameInstanceManifest) -> Option<String> {
    manifest
        .runtime_identity
        .as_ref()
        .map(|runtime| runtime.version.clone())
        .or_else(|| match &manifest.base {
            agora_game_api::BaseReference::Pinned { runtime, .. } => Some(runtime.version.clone()),
            agora_game_api::BaseReference::Unpinned { .. } => None,
        })
}
