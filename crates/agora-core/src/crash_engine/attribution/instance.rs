//! Building an [`InstanceView`] from a real instance. This is a thin reader over the listings the
//! deployment and instance services already provide; it decides nothing about attribution.

use std::collections::HashMap;
use std::path::PathBuf;

use agora_game_api::{fold_path_case, BaseReference, LayerSource};

use super::view::{InstanceView, LayerOwner, LayerView};
use crate::content_store::{self, ContentItem};
use crate::ctx::Ctx;
use crate::game_deploy::{self, DeployMode, FileSource};
use crate::game_instance;

#[derive(Debug, thiserror::Error)]
pub enum InstanceViewError {
    #[error(transparent)]
    Instance(#[from] crate::game_instance::InstanceError),
    #[error(transparent)]
    Deploy(#[from] crate::game_deploy::DeployError),
    #[error(transparent)]
    Content(#[from] crate::content_store::ContentError),
    #[error("game '{0}' is not registered")]
    GameNotRegistered(String),
    #[error("base manifest could not be read: {0}")]
    BaseManifest(String),
}

/// A snapshot of `instance_id` as a crash would find it: every layer in load order, with each
/// layer's files. It reads the instance manifest, the content store and the deployment the game
/// would see, and never opens a file a ref names.
///
/// A listing that fails is an error, not an empty instance. A caller that wants attribution anyway
/// passes [`InstanceView::unavailable`] with the error's text, so every miss is `Unknown`.
pub fn from_instance(ctx: &Ctx, instance_id: &str) -> Result<InstanceView, InstanceViewError> {
    let manifest = game_instance::get_manifest(ctx, instance_id)?;
    let definition = ctx
        .games
        .game(&manifest.game)
        .ok_or_else(|| InstanceViewError::GameNotRegistered(manifest.game.to_string()))?;
    // Attribution asks what the game can see, not how it is placed. The Links plan lists every
    // layer, including the writable one that the virtual-file-system plan leaves out.
    let plan = game_deploy::plan(ctx, instance_id, definition, DeployMode::Links)?;

    let framework_layers = framework_layers(&manifest);

    let mut layers: Vec<LayerView> = Vec::new();
    layers.push(LayerView::new(
        "base",
        true,
        LayerOwner::Base,
        base_files(ctx, &manifest.base)?,
    ));

    let item_ids = content_store::item_ids(ctx)?;
    let mut by_layer: HashMap<String, Vec<String>> = HashMap::new();
    let mut overrides = Vec::new();
    for file in &plan.files {
        match &file.source {
            FileSource::Generated { .. } | FileSource::Writable { .. } => {
                by_layer
                    .entry(file.layer_id.clone())
                    .or_default()
                    .push(file.path.as_str().to_string());
            }
            FileSource::Override { .. } => overrides.push(file.path.as_str().to_string()),
            FileSource::Base { .. } | FileSource::Content { .. } => {}
        }
    }

    for layer in manifest.layers.layers_to_mount(false) {
        let id = layer.id.as_str().to_string();
        match &layer.source {
            LayerSource::Base { .. } => {}
            LayerSource::Content { content } => {
                let item = content_store::get_item_indexed(ctx, &item_ids, content)?;
                let owner = match framework_layers.get(&id) {
                    Some(name) => LayerOwner::Framework { name: name.clone() },
                    None => LayerOwner::Item {
                        key: item.item_id.clone(),
                        name: item.name.clone(),
                    },
                };
                layers.push(LayerView::new(
                    id,
                    layer.enabled,
                    owner,
                    content_files(layer, &item),
                ));
            }
            LayerSource::Generated { tool, .. } => {
                let files = by_layer.remove(&id).unwrap_or_default();
                layers.push(LayerView::new(
                    id,
                    layer.enabled,
                    LayerOwner::Generated {
                        tool: tool.to_string(),
                    },
                    files,
                ));
            }
            LayerSource::Writable { .. } => {
                if !overrides.is_empty() {
                    layers.push(LayerView::new(
                        "overrides",
                        true,
                        LayerOwner::Writable,
                        std::mem::take(&mut overrides),
                    ));
                }
                let files = by_layer.remove(&id).unwrap_or_default();
                layers.push(LayerView::new(id, true, LayerOwner::Writable, files));
            }
            LayerSource::InstanceContent { .. } | LayerSource::Staging { .. } => {
                // Legacy redirected content and a tool run that is not active are not part of the
                // instance the game launched; the deployment plan does not list them here either.
            }
        }
    }

    if !overrides.is_empty() {
        layers.push(LayerView::new(
            "overrides",
            true,
            LayerOwner::Writable,
            overrides,
        ));
    }

    Ok(InstanceView::new(layers))
}

fn framework_layers(manifest: &game_instance::GameInstanceManifest) -> HashMap<String, String> {
    let mut own = HashMap::new();
    for framework in &manifest.frameworks {
        for layer in &framework.layers {
            own.insert(
                layer.as_str().to_string(),
                framework.id.as_str().to_string(),
            );
        }
    }
    own
}

fn base_files(ctx: &Ctx, base: &BaseReference) -> Result<Vec<String>, InstanceViewError> {
    let BaseReference::Pinned { id, .. } = base else {
        return Ok(Vec::new());
    };
    let path: PathBuf = ctx.paths.base_manifest_path(id);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| InstanceViewError::BaseManifest(error.to_string()))?;
    let manifest: crate::game_base::BaseManifest = serde_json::from_str(&text)
        .map_err(|error| InstanceViewError::BaseManifest(error.to_string()))?;
    Ok(manifest.files.into_iter().map(|file| file.path).collect())
}

/// One content layer's files as the game sees them: the item's paths, with the layer's
/// `source_path` stripped and its `mount_path` added, exactly as the deployment plan mounts them.
fn content_files(layer: &agora_game_api::Layer, item: &ContentItem) -> Vec<String> {
    let source = layer.source_path.as_str().trim_matches('/');
    let source_parts: Vec<&str> = if source.is_empty() {
        Vec::new()
    } else {
        source.split('/').filter(|part| !part.is_empty()).collect()
    };
    let mut files = Vec::with_capacity(item.files.len());
    for file in &item.files {
        let file_parts: Vec<&str> = file
            .path
            .as_str()
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let stripped = if source_parts.is_empty() {
            Some(file.path.as_str().to_string())
        } else if file_parts.len() > source_parts.len()
            && source_parts
                .iter()
                .zip(file_parts.iter())
                .all(|(source, file)| fold_path_case(source) == fold_path_case(file))
        {
            Some(file_parts[source_parts.len()..].join("/"))
        } else {
            None
        };
        if let Some(path) = stripped {
            files.push(mounted(layer, path));
        }
    }
    files
}

fn mounted(layer: &agora_game_api::Layer, relative: String) -> String {
    if layer.mount_path.as_str().is_empty() {
        relative
    } else {
        format!(
            "{}/{}",
            layer.mount_path.as_str().trim_end_matches('/'),
            relative.trim_start_matches('/')
        )
    }
}
