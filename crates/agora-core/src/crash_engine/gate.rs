//! Ask the crash engine a small question: does this captured text name a known crash?
//!
//! A launch gate only needs a yes or no, so it runs the same parser and rule pipeline over the
//! text a launch captured, with no instance. The answer's findings decide it; a text nothing could
//! read is not a crash, and neither is an empty one.

use std::sync::Arc;
use std::time::Duration;

use agora_game_api::crash::BuiltInPack;
use agora_plugin_api::error::PluginResult;
use agora_plugin_api::host::{ActivationRequest, GameReads, HostBridge, ScriptHost, ScriptReads};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::PluginEventEnvelope;
use agora_plugin_api::{PluginError, PluginErrorCode};

use super::{analyze, AnalyzeInput, CrashFile};
use crate::crash_packs::{load_built_in_packs, LoadedCrashPack};

/// Whether `text`, named as `file_name`, makes the engine name at least one finding. The packs
/// supplied alongside the built-in ones take part too, so a launch gate can use an installed game
/// pack. Nothing is logged and nothing is written.
pub fn names_a_crash(
    text: &str,
    file_name: &str,
    packs: &[LoadedCrashPack],
    built_in: &[BuiltInPack],
    runtime_version: Option<&str>,
) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let mut all: Vec<LoadedCrashPack> = packs.to_vec();
    all.extend(load_built_in_packs(built_in).packs);
    if all.is_empty() {
        return false;
    }
    let files = [CrashFile {
        name: file_name.to_string(),
        bytes: text.as_bytes().to_vec(),
    }];
    let host = NoHost;
    match analyze(&AnalyzeInput {
        packs: &all,
        files: &files,
        runtime_version,
        host: &host,
        built_in,
        instance: None,
    }) {
        Ok(output) => {
            // A finding means a rule recognised something in the text, even when the answer
            // deliberately caps it at "not sure" because nobody was named.
            output.read_any()
                && (!output.answer.candidates.is_empty() || !output.answer.findings.is_empty())
        }
        Err(_) => false,
    }
}

/// A gate runs only parsers compiled into the binary; a script parser has no host here.
pub struct NoHost;

impl ScriptHost for NoHost {
    fn describe(&self) -> String {
        "no host".to_string()
    }

    fn activate(
        &self,
        _request: ActivationRequest,
        _bridge: Arc<dyn HostBridge>,
    ) -> PluginResult<()> {
        Err(PluginError::new(
            PluginErrorCode::NotActivated,
            "the launch gate runs no plugin code",
        ))
    }

    fn deactivate(&self, _plugin_id: &PluginId) -> PluginResult<()> {
        Ok(())
    }

    fn invoke(
        &self,
        _plugin_id: &PluginId,
        _export: &str,
        _args: serde_json::Value,
        _timeout: Duration,
    ) -> PluginResult<serde_json::Value> {
        Err(PluginError::new(
            PluginErrorCode::NotActivated,
            "the launch gate runs no plugin code",
        ))
    }

    fn invoke_with_reads(
        &self,
        _plugin_id: &PluginId,
        _export: &str,
        _args: serde_json::Value,
        _timeout: Duration,
        _reads: Arc<dyn ScriptReads>,
    ) -> PluginResult<serde_json::Value> {
        Err(PluginError::new(
            PluginErrorCode::NotActivated,
            "the launch gate runs no plugin code",
        ))
    }

    fn invoke_with_game_reads(
        &self,
        _plugin_id: &PluginId,
        _export: &str,
        _args: serde_json::Value,
        _timeout: Duration,
        _reads: Arc<dyn GameReads>,
    ) -> PluginResult<serde_json::Value> {
        Err(PluginError::new(
            PluginErrorCode::NotActivated,
            "the launch gate runs no plugin code",
        ))
    }

    fn deliver_event(
        &self,
        _plugin_id: &PluginId,
        _envelope: &PluginEventEnvelope,
    ) -> PluginResult<()> {
        Ok(())
    }

    fn cancel(&self, _plugin_id: &PluginId) {}

    fn is_active(&self, _plugin_id: &PluginId) -> bool {
        false
    }

    fn active_plugins(&self) -> Vec<PluginId> {
        Vec::new()
    }
}
