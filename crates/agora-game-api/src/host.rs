use crate::{GamePath, InstallId, LayerId, LoadOrder, ToolId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, future::Future, pin::Pin};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct GameError {
    pub code: String,
    pub message: String,
}

pub type GameResult<T> = Result<T, GameError>;
/// Object-safe async operations without tying packages to Tokio or async-trait.
pub type GameFuture<'a, T> = Pin<Box<dyn Future<Output = GameResult<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    pub sha256: Option<String>,
    /// Host interprets the purpose against policy, never as a grant itself.
    pub purpose: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedArtifact {
    pub id: String,
    pub sha256: String,
    pub source_hash_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReadRequest {
    pub install: InstallId,
    pub path: crate::RelPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReadRequest {
    pub artifact: String,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRequest {
    pub artifact: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedArchive {
    pub content: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FomodRequest {
    pub content: String,
    /// Saved choices for deterministic replay. None asks the host's own UI.
    pub choices: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FomodResult {
    pub content: String,
    pub choices: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortLoadOrderRequest {
    pub engine: String,
    pub order: LoadOrder,
    pub roots: Vec<GamePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunRequest {
    pub instance: String,
    /// Host resolves a registered definition; no arbitrary executable here.
    pub tool: ToolId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRunResult {
    pub run: String,
    pub generated_layer: LayerId,
    pub input_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameEvent {
    pub name: String,
    pub fields: BTreeMap<String, String>,
}
