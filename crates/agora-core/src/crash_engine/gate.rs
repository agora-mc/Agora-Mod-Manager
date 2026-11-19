//! Ask the crash engine a small question: does this captured text name a known crash?
//!
//! A launch gate only needs a yes or no, so it runs the same parser and rule pipeline over the
//! text a launch captured, with no instance. The answer's findings decide it; a text nothing could
//! read is not a crash, and neither is an empty one.

use std::sync::Arc;
use std::time::Duration;

use agora_game_api::crash::{BuiltInPack, CodeEntry, CrashPack, ParserDecl, Reads};
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
    // Captured launch output is collected by Agora, never a paste: no snippet fallback.
    let files = [CrashFile {
        name: file_name.to_string(),
        bytes: text.as_bytes().to_vec(),
        origin: super::CrashFileOrigin::Collected,
        source: None,
    }];
    let host = NoHost;
    if let Ok(output) = analyze(&AnalyzeInput {
        packs: &all,
        files: &files,
        runtime_version,
        host: &host,
        built_in,
        instance: None,
    }) {
        // A finding means a rule recognised something in the text, even when the answer
        // deliberately caps it at "not sure" because nobody was named.
        if output.read_any()
            && (!output.answer.candidates.is_empty() || !output.answer.findings.is_empty())
        {
            return true;
        }
    }

    all.iter()
        .any(|pack| hosted_primary_names_a_crash(pack, file_name, text))
}

fn hosted_primary_names_a_crash(pack: &LoadedCrashPack, file_name: &str, text: &str) -> bool {
    let parsers: Vec<ParserDecl> = pack
        .pack
        .parsers
        .iter()
        .filter(|parser| {
            parser.reads == Reads::Primary
                && matches!(
                    parser.entry,
                    CodeEntry::Script { .. } | CodeEntry::Wasm { .. }
                )
        })
        .cloned()
        .collect();
    if parsers.is_empty() {
        return false;
    }
    let hosted = CrashPack {
        contract: pack.pack.contract,
        games: Vec::new(),
        sources: Vec::new(),
        parsers,
        inspectors: Vec::new(),
        ref_maps: Vec::new(),
        roles: Vec::new(),
        save_risk: None,
        rules: Vec::new(),
    };
    crate::crash_packs::detect_parsers(&hosted, file_name, text.as_bytes())
        .is_ok_and(|matched| !matched.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use agora_game_api::crash::{
        CodeEntry, CrashPack, Detect, ParserDecl, ParserLimits, Reads, CRASH_CONTRACT_VERSION,
    };

    fn pack_with_parser(id: &str, entry: CodeEntry, detect: Detect) -> LoadedCrashPack {
        LoadedCrashPack {
            plugin_id: "test.plugin".to_string(),
            plugin_name: "Test Plugin".to_string(),
            plugin_version: "1.0".to_string(),
            dependencies: Vec::new(),
            official: true,
            file: "crash/pack.json".to_string(),
            pack: CrashPack {
                contract: CRASH_CONTRACT_VERSION,
                games: vec!["test-game".to_string()],
                sources: Vec::new(),
                parsers: vec![ParserDecl {
                    id: id.to_string(),
                    reads: Reads::Primary,
                    detect,
                    entry,
                    limits: Some(ParserLimits::DEFAULT),
                }],
                inspectors: Vec::new(),
                ref_maps: Vec::new(),
                roles: Vec::new(),
                save_risk: None,
                rules: Vec::new(),
            },
        }
    }

    #[test]
    fn test_hosted_primary_detection_recognises_a_crash_log_without_a_script_host() {
        let crash_logger = pack_with_parser(
            "crash_logger",
            CodeEntry::Script {
                export: "parseCrashLogger".to_string(),
            },
            Detect {
                name_glob: None,
                all: vec!["(?m)^CrashLoggerSSE v".to_string()],
                any: vec!["(?m)^Unhandled exception ".to_string()],
                magic_hex: None,
            },
        );

        let crash_text = "CRASH TIME: 2026-03-01 12:00:00\nSkyrim SSE v1.6.1170\nCrashLoggerSSE v1-20-1-0 Mar  1 2026 00:00:00\n\nUnhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF700001000 ExampleGame.exe+001000\n";
        assert!(
            names_a_crash(
                crash_text,
                "crash-2026-03-01.log",
                std::slice::from_ref(&crash_logger),
                &[],
                None
            ),
            "a hosted primary parser's declared detect must let the gate name a crash"
        );

        let normal_text = "SKSE64 runtime: initialize (version = 2.2.6)\nimagebase = 00007FF712340000\nloading plugin \"ExampleLoaded\"\n";
        assert!(
            !names_a_crash(normal_text, "skse64.log", &[crash_logger], &[], None),
            "normal log must not be recognised as a crash"
        );

        let built_in_like = pack_with_parser(
            "captured_launch",
            CodeEntry::BuiltIn {
                name: "captured_launch".to_string(),
            },
            Detect {
                name_glob: None,
                all: Vec::new(),
                any: vec!["(?m)^\\[stdout\\] ".to_string()],
                magic_hex: None,
            },
        );
        assert!(
            !names_a_crash(
                "[stdout] [01:02:03] [main/INFO]: Done.\n",
                "agora-launch-output.log",
                &[built_in_like],
                &[],
                None
            ),
            "a built-in parser's broad format detect must not set the launch flag"
        );
    }
}
