//! Community signatures on a crash file read on its own, with no instance behind it: the file's
//! game is the game whose parsers read it, so another game's signature stays quiet.
//!
//! Every log line here is invented for these tests.

use agora_core::crash_engine::{analyze, AnalyzeInput, AnalyzeOutput, CrashFile};
use agora_core::crash_packs::{load_built_in_packs, load_community_pack, LoadedCrashPack};
use agora_game_minecraft::crash::built_in_packs;
use agora_plugin_host::QuickJsHost;
use rusqlite::Connection;

const LOG: &str = "[01:02:03] [main/INFO]: Loading Minecraft 1.20.1 with Fabric Loader 0.16.0
[01:02:04] [main/ERROR]: The shiny example widget fell over
";

fn community(rows: &[(&str, &str)]) -> Vec<LoadedCrashPack> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE crash_signatures (
            id TEXT PRIMARY KEY, name TEXT, games_json TEXT NOT NULL, regex_pattern TEXT,
            level TEXT, cause TEXT, text TEXT, action_kind TEXT, action_content_key TEXT,
            action_label TEXT
        );",
    )
    .unwrap();
    for (id, games) in rows {
        conn.execute(
            "INSERT INTO crash_signatures
                (id, name, games_json, regex_pattern, level, cause, text,
                 action_kind, action_content_key, action_label)
             VALUES (?1, ?1, ?2, 'shiny example widget fell over', 'likely', 'other', ?3,
                     NULL, NULL, NULL)",
            rusqlite::params![id, games, format!("{id} matched.")],
        )
        .unwrap();
    }
    let report = load_community_pack(&conn);
    assert!(report.is_clean(), "{:#?}", report.problems);
    report.packs
}

fn analyze_loose(community: Vec<LoadedCrashPack>) -> AnalyzeOutput {
    let built_in = built_in_packs();
    let loaded = load_built_in_packs(&built_in);
    assert!(loaded.is_clean(), "{:#?}", loaded.problems);
    let mut packs = loaded.packs;
    packs.extend(community);
    let host = QuickJsHost::new();
    analyze(&AnalyzeInput {
        packs: &packs,
        files: &[CrashFile::new(
            "latest.log".to_string(),
            LOG.as_bytes().to_vec(),
        )],
        runtime_version: None,
        host: &host,
        built_in: &built_in,
        instance: None,
    })
    .expect("the log is read")
}

fn headlines(output: &AnalyzeOutput) -> Vec<String> {
    output
        .answer
        .findings
        .iter()
        .filter_map(|finding| finding.headline.get("en").cloned())
        .collect()
}

#[test]
fn a_loose_minecraft_log_meets_minecraft_signatures_only() {
    let output = analyze_loose(community(&[
        ("minecraft_widget", r#"["minecraft"]"#),
        ("skyrim_widget", r#"["skyrim-se"]"#),
    ]));
    let headlines = headlines(&output);
    assert!(
        headlines
            .iter()
            .any(|text| text == "minecraft_widget matched."),
        "{headlines:?}"
    );
    assert!(
        !headlines
            .iter()
            .any(|text| text == "skyrim_widget matched."),
        "{headlines:?}"
    );
}

#[test]
fn a_signature_for_several_games_still_fires_once() {
    let output = analyze_loose(community(&[(
        "shared_widget",
        r#"["skyrim-se", "minecraft"]"#,
    )]));
    let matched = headlines(&output)
        .into_iter()
        .filter(|text| text == "shared_widget matched.")
        .count();
    assert_eq!(matched, 1);
}
