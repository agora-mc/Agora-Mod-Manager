//! A snapshot of one instance as attribution reads it.

use std::collections::BTreeMap;

use agora_game_api::crash::Owner;

use super::index::FileIndex;

/// Who a layer belongs to, in the terms [`Owner`] uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerOwner {
    /// The unmodified game.
    Base,
    /// One installed item, by content key and display name.
    Item { key: String, name: String },
    /// A framework, by name.
    Framework { name: String },
    /// One tool's generated output.
    Generated { tool: String },
    /// The instance's writable layer, or its own overrides folder.
    Writable,
}

impl LayerOwner {
    /// The owner this layer gives a ref when it provides the file.
    pub(crate) fn to_owner(&self) -> Owner {
        match self {
            Self::Base => Owner::Base,
            Self::Item { key, name } => Owner::Item {
                key: key.clone(),
                name: name.clone(),
            },
            Self::Framework { name } => Owner::Framework { name: name.clone() },
            Self::Generated { tool } => Owner::Generated { tool: tool.clone() },
            Self::Writable => Owner::Writable,
        }
    }

    /// A stable, short name for `also_provided_by`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Base => "base".to_string(),
            Self::Item { key, .. } => key.clone(),
            Self::Framework { name } => name.clone(),
            Self::Generated { tool } => tool.clone(),
            Self::Writable => "writable".to_string(),
        }
    }
}

/// One layer of the instance, in load order, with the files the game would see from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerView {
    /// The layer's id in the instance stack.
    pub id: String,
    /// A turned-off layer provides nothing, but a view keeps it so attribution can say so.
    pub enabled: bool,
    pub owner: LayerOwner,
    /// Game-relative, `/`-separated paths. Order and duplicates do not matter.
    pub files: Vec<String>,
    /// Named values this layer's item declares, such as mod ids or Java packages. The key is what
    /// a ref map's `index` names; a value may be a package prefix (`prefix_dot`).
    pub indexes: BTreeMap<String, Vec<String>>,
}

impl LayerView {
    pub fn new(
        id: impl Into<String>,
        enabled: bool,
        owner: LayerOwner,
        files: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            enabled,
            owner,
            files,
            indexes: BTreeMap::new(),
        }
    }

    /// Add values to a named index without consuming the layer twice.
    pub fn with_index(mut self, index: impl Into<String>, values: Vec<String>) -> Self {
        self.indexes.entry(index.into()).or_default().extend(values);
        self
    }
}

/// How much of the instance the snapshot could see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// Every layer and its files were listed.
    Complete,
    /// The instance could not be listed. A miss is `Unknown`, never `NotInInstance`.
    Unavailable { reason: String },
    /// No listing was supplied at all.
    NotGiven,
}

impl Listing {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Why the listing is not complete, for an `Unknown` answer.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Complete => None,
            Self::Unavailable { reason } => Some(reason),
            Self::NotGiven => Some("no instance listing was supplied"),
        }
    }
}

/// One instance at crash time: its layers in load order, each with who owns it and the files it
/// provides. Built from plain data; [`from_instance`](super::instance::from_instance) makes one
/// from a real instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceView {
    layers: Vec<LayerView>,
    listing: Listing,
    index: FileIndex,
}

impl InstanceView {
    /// A complete snapshot. `layers` are in load order, lowest first; between layers that provide
    /// the same file, the later one wins.
    pub fn new(layers: Vec<LayerView>) -> Self {
        let index = FileIndex::build(&layers);
        Self {
            layers,
            listing: Listing::Complete,
            index,
        }
    }

    /// A view of an instance that could not be listed. Every miss is `Unknown`.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            layers: Vec::new(),
            listing: Listing::Unavailable {
                reason: reason.into(),
            },
            index: FileIndex::default(),
        }
    }

    /// No listing was given. Every miss is `Unknown`.
    pub fn not_given() -> Self {
        Self {
            layers: Vec::new(),
            listing: Listing::NotGiven,
            index: FileIndex::default(),
        }
    }

    pub fn layers(&self) -> &[LayerView] {
        &self.layers
    }

    pub fn listing(&self) -> &Listing {
        &self.listing
    }

    pub fn is_complete(&self) -> bool {
        self.listing.is_complete()
    }

    /// The game-relative files one installed item provides, or `None` when the listing is not
    /// complete. A turned-off layer provides nothing.
    pub fn item_files(&self, content_key: &str) -> Option<Vec<String>> {
        if !self.is_complete() {
            return None;
        }
        let mut files: Vec<String> = self
            .layers
            .iter()
            .filter(|layer| layer.enabled)
            .filter(|layer| match &layer.owner {
                LayerOwner::Item { key, .. } => key == content_key,
                _ => false,
            })
            .flat_map(|layer| layer.files.iter().cloned())
            .collect();
        files.sort();
        files.dedup();
        Some(files)
    }

    pub(crate) fn index(&self) -> &FileIndex {
        &self.index
    }
}
