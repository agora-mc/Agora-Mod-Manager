use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDefinition {
    pub id: String,
    pub version: semver::Version,
    pub api_range: semver::VersionReq,
    /// Optional toolchain parents; games can stand alone.
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub games: Vec<GameDefinition>,
    #[serde(default)]
    pub frameworks: Vec<FrameworkDefinition>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    /// Partial games a game may start from with `extends` (MASTER_SPEC §30.12). They are merged on
    /// raw JSON before a game is parsed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub game_templates: BTreeMap<String, serde_json::Value>,
    /// Plugin entrypoints a scripted package runs (MASTER_SPEC §30.12). Unused by data-only
    /// packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behaviours: Option<Behaviours>,
}

/// The plugin entrypoints a scripted package exposes (MASTER_SPEC §30.12). Each names a function
/// in the plugin's script.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Behaviours {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect_runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classify: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recognize_download: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_order_rules: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare_launch: Option<String>,
}

impl Behaviours {
    /// Whether this names any script hook at all.
    pub fn names_any(&self) -> bool {
        self.detect_runtime.is_some()
            || self.classify.is_some()
            || self.recognize_download.is_some()
            || self.load_order_rules.is_some()
            || self.diagnostics.is_some()
            || self.prepare_launch.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameContext {
    pub instance: String,
    pub runtime: RuntimeIdentity,
    pub base: Option<BaseReference>,
    pub frameworks: Vec<InstalledFramework>,
    pub layers: LayerStack,
    pub load_order: LoadOrder,
    pub settings: BTreeMap<String, String>,
}

/// How a recognised download enters Agora's content system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DownloadAdd {
    /// Unpack the archive into the store. A FOMOD installer in it is answered afterwards.
    Archive,
    /// Keep the file whole as one library item under `folder` (a game's content folder).
    File { folder: String },
}

/// What a game package recognises a downloaded file as, by its archive entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRecognition {
    /// The content kind in the game's own words, e.g. `mod`, `resource pack`, `FOMOD installer`.
    pub kind: String,
    /// Why it was recognised, in words a player can read, e.g. `has fabric.mod.json`.
    pub reason: String,
    /// How adding it to the library should store it.
    pub add: DownloadAdd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub severity: DiagnosticSeverity,
    pub repairs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// One registry entry point for compiled and script-backed packages. Returning
/// None asks core to use declarative data; optional behaviour needs no private
/// access to services. Core, not the package, spawns a prepared launch recipe.
pub trait GamePackage: Send + Sync {
    fn definition(&self) -> &PackageDefinition;

    fn detect_runtime<'a>(
        &'a self,
        _install: GameInstall,
    ) -> GameFuture<'a, Option<RuntimeIdentity>> {
        Box::pin(async { Ok(None) })
    }

    /// Place an item the game's data rules did not. `item_name` is a hint from Agora (the
    /// archive's or folder's name) and must not be trusted: a `{folder}` in the returned mount is
    /// filled from it and passes the same checks a content rule's `{folder}` does.
    fn classify<'a>(
        &'a self,
        _context: GameContext,
        _item_name: &'a str,
        _files: Vec<RelPath>,
    ) -> GameFuture<'a, Option<Suggestion>> {
        Box::pin(async { Ok(None) })
    }

    /// Recognise a downloaded archive as content for one of this package's games, by its entry
    /// names alone. `file_name` is the download's own name (it may not be trusted); `entries` are
    /// the archive's file paths with forward slashes, listed without extracting. Returning `None`
    /// means "not this game's content", and Agora stays quiet about the file.
    fn recognize_download(
        &self,
        _game: &GameId,
        _file_name: &str,
        _entries: &[String],
    ) -> Option<DownloadRecognition> {
        None
    }

    /// The rules a `script` load order follows (MASTER_SPEC §30.12). `list_id` names the list the
    /// rules are for; a game with several lists may judge each differently. Returning `None` or an
    /// error is a failure to report, never "no rules, so any order is fine".
    fn load_order_rules<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
    ) -> GameFuture<'a, Option<LoadOrderRulesReply>> {
        Box::pin(async { Ok(None) })
    }

    /// Read a `script` load order file (MASTER_SPEC §30.12). `None` means the package does not
    /// implement the format; core reports that instead of treating the file as empty.
    fn parse_load_order<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
        _bytes: Vec<u8>,
    ) -> GameFuture<'a, Option<Vec<LoadOrderLine>>> {
        Box::pin(async { Ok(None) })
    }

    /// Render a `script` load order file (MASTER_SPEC §30.12). Core writes the bytes it returns to
    /// the list's declared file; the hook cannot choose a path. `None` is a failure to report.
    fn render_load_order<'a>(
        &'a self,
        _context: GameContext,
        _list_id: &'a str,
        _lines: Vec<LoadOrderLine>,
    ) -> GameFuture<'a, Option<Vec<u8>>> {
        Box::pin(async { Ok(None) })
    }

    fn diagnostics<'a>(&'a self, _context: GameContext) -> GameFuture<'a, Vec<Diagnostic>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn prepare_launch<'a>(&'a self, _context: GameContext) -> GameFuture<'a, Option<LaunchRecipe>> {
        Box::pin(async { Ok(None) })
    }
}

// This signature fails to compile if the interface stops being object-safe.
const _: Option<&dyn GamePackage> = None;
