//! Minecraft's built-in crash pack, compiled into Agora rather than installed as a plugin.
//!
//! The pack is the same JSON a plugin writes, and is linted and loaded exactly like a plugin's
//! (MASTER_SPEC §30.11). Only its parsers differ: they are Rust code in this module, behind the
//! same request and output boundary a script uses. A plugin's crash pack for Minecraft can read
//! the facts these parsers produce and add rules of its own.

mod crash_report;
mod hs_err;
mod latest_log;
mod shared;
mod snippet;

use agora_game_api::crash::BuiltInPack;

pub use crash_report::CrashReportParser;
pub use hs_err::HsErrParser;
pub use latest_log::LatestLogParser;
pub use snippet::SnippetParser;

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
            ("hs_err".to_string(), Box::new(hs_err::HsErrParser)),
            ("snippet".to_string(), Box::new(snippet::SnippetParser)),
        ],
    }]
}
