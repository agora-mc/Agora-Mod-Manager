//! Minecraft's built-in crash pack, compiled into Agora rather than installed as a plugin.
//!
//! The pack is the same JSON a plugin writes, and is linted and loaded exactly like a plugin's
//! (MASTER_SPEC §30.11). Only its parsers differ: they are Rust code in this module, behind the
//! same request and output boundary a script uses. A plugin's crash pack for Minecraft can read
//! the facts these parsers produce and add rules of its own.

mod captured;
mod crash_report;
mod hs_err;
mod latest_log;
mod shared;

use agora_core::crash_engine::gate;
use agora_core::crash_live::{
    diagnose_session, load_installed_packs, record_could_not_analyze, LivePacks, SessionAnswer,
};
use agora_game_api::crash::BuiltInPack;

pub use captured::CapturedOutputParser;
pub use crash_report::CrashReportParser;
pub use hs_err::HsErrParser;
pub use latest_log::LatestLogParser;

/// Diagnose a finished Minecraft session with the same packs and engine the rest of Agora uses,
/// through the shared instance router's Minecraft branch. The caller supplies the host; every
/// failure becomes a stored "could not analyze", never a missing answer.
pub fn diagnose_minecraft_session(
    ctx: &agora_core::ctx::Ctx,
    session_id: &str,
    host: std::sync::Arc<dyn agora_plugin_api::host::ScriptHost>,
) -> Option<SessionAnswer> {
    match load_installed_packs(ctx, host.clone()) {
        Ok(installed) => {
            let built_in = built_in_packs();
            let instances = crate::installed_content::MinecraftInstanceProvider;
            diagnose_session(
                ctx,
                session_id,
                LivePacks {
                    packs: &installed.packs,
                    built_in: &built_in,
                    host: host.as_ref(),
                    instances: Some(&instances),
                },
            )
        }
        Err(error) => record_could_not_analyze(
            ctx,
            session_id,
            format!("the crash packs could not be loaded: {error}"),
        ),
    }
}

/// Whether Agora's own captured launch output names a known crash, through the same engine and
/// built-in pack the rest of the crash doctor uses. No installed plugin code runs here.
pub fn captured_output_names_a_crash(text: &str) -> bool {
    gate::names_a_crash(
        text,
        "agora-launch-output.log",
        &[],
        &built_in_packs(),
        None,
    )
}

/// Whether a tail of the game's own log names a known crash. Used by the delegated launch monitor,
/// where the log is the only signal there is.
pub fn log_text_names_a_crash(text: &str) -> bool {
    gate::names_a_crash(text, "latest.log", &[], &built_in_packs(), None)
}

/// Every crash pack this game package ships, with the parsers its JSON names.
///
/// The caller lints the JSON with `agora_core::crash_packs::load_built_in_packs`, which stamps the
/// findings' source as Agora. Nothing here names a core type, so the game package keeps its
/// dependence on `agora-core` shrinking (MASTER_SPEC §26.12).
pub fn built_in_packs() -> Vec<BuiltInPack> {
    vec![BuiltInPack {
        file: "crash/pack.json".to_string(),
        json: include_str!("../../crash/pack.json").to_string(),
        parsers: vec![
            (
                "crash_report".to_string(),
                Box::new(crash_report::CrashReportParser),
            ),
            (
                "latest_log".to_string(),
                Box::new(latest_log::LatestLogParser),
            ),
            (
                "captured_launch".to_string(),
                Box::new(captured::CapturedOutputParser),
            ),
            ("hs_err".to_string(), Box::new(hs_err::HsErrParser)),
        ],
    }]
}
