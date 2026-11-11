//! Loose crash files, end to end: detect, parse, evaluate, consensus, answer.
//!
//! Every log and every name here is invented for these tests. Nothing is copied from a real crash
//! file.

use agora_core::crash_engine::{analyze, AnalyzeInput, CrashFile, RefResolver};
use agora_core::crash_packs::{load_plugin_packs, LoadedCrashPack, ParserFailureKind};
use agora_game_api::crash::{CrashAnswer, Level, MatchedBy, Owner, ParserLimits, SessionVerdict};
use agora_plugin_api::host::{ActivationRequest, HostBridge, ScriptHost};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

const HEADER: &str = "^== FAULTS ==$";

fn generic_pack(roles: &str, rules: &str) -> String {
    format!(
        r#"{{
  "contract": 1,
  "parsers": [
    {{
      "id": "loose_log",
      "entry": {{ "generic": {{ "name": "text_sections", "config": {{
        "sections": [
          {{
            "id": "faults",
            "header": "{HEADER}",
            "lines": [
              {{
                "pattern": "^module (?P<module>[A-Za-z0-9_.-]+) failed$",
                "kind": "acme.fault",
                "refs": [{{ "kind": "module", "value": "module" }}]
              }}
            ]
          }}
        ]
      }} }} }},
      "detect": {{ "all": ["(?m){HEADER}"] }}
    }}
  ],
  "roles": [{roles}],
  "rules": [{rules}]
}}"#
    )
}

fn fault_rule(id: &str) -> String {
    format!(
        r#"{{
  "id": "{id}",
  "version": 1,
  "when": {{
    "fact": {{ "kind": "acme.fault", "ref": {{ "kind": "module", "bind": "m" }} }}
  }},
  "effects": [
    {{ "signal": {{ "family": "execution", "candidate": {{ "owner_of": "m" }}, "weight": 16 }} }}
  ],
  "text": {{ "headline": {{ "en": "{{owner:m}} failed while the game was running." }} }}
}}"#
    )
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative.replace('\\', "/"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn manifest(id: &str, name: &str, entrypoint: Option<&str>) -> String {
    let entry = entrypoint
        .map(|entry| format!(r#", "entrypoint": "{entry}""#))
        .unwrap_or_default();
    format!(
        r#"{{
  "manifest": 1,
  "id": "{id}",
  "name": "{name}",
  "version": "1.0.0",
  "license": "MIT",
  "apiRange": ">=0.1, <0.2",
  "capabilities": {{ "required": ["crash:contribute"] }}{entry},
  "contributions": {{ "crashPacks": [{{ "path": "crash/pack.json" }}] }}
}}"#
    )
}

/// A development folder holding one pack, loaded exactly as `agora plugin validate` would load it.
fn plugin_folder(
    id: &str,
    name: &str,
    pack: &str,
    script: Option<&str>,
) -> (TempDir, LoadedCrashPack) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "crash/pack.json", pack);
    write(
        dir.path(),
        "agora-plugin.json",
        &manifest(id, name, script.map(|_| "index.js")),
    );
    if let Some(script) = script {
        write(dir.path(), "index.js", script);
    }
    let manifest = agora_core::plugins::install::read_folder_manifest(dir.path()).unwrap();
    let report = load_plugin_packs(id, dir.path(), &manifest);
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.packs.len(), 1);
    (dir, report.packs[0].clone())
}

struct NoopBridge;

impl HostBridge for NoopBridge {
    fn call(
        &self,
        _plugin_id: &PluginId,
        request: agora_plugin_api::protocol::HostRequest,
    ) -> HostResponse {
        HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

fn active_host(folder: &Path, id: &str) -> QuickJsHost {
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: PluginId::parse(id).unwrap(),
            package_root: folder.to_path_buf(),
            data_root: folder.to_path_buf(),
            entrypoint: "index.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .unwrap();
    host
}

fn idle_host() -> QuickJsHost {
    QuickJsHost::new()
}

fn file(name: &str, text: &str) -> CrashFile {
    CrashFile {
        name: name.to_string(),
        bytes: text.as_bytes().to_vec(),
    }
}

fn english(answer: &CrashAnswer) -> &str {
    answer.headline.get("en").map(String::as_str).unwrap_or("")
}

#[test]
fn a_supplied_file_names_the_module_it_names_itself_and_says_the_user_supplied_it() {
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let output = analyze(&AnalyzeInput {
        packs: &[pack],
        files: &[file(
            "friend-crash.txt",
            "== FAULTS ==\nmodule ExampleMod.dll failed\n",
        )],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert_eq!(
        output.answer.verdict,
        SessionVerdict::Crashed {
            how: MatchedBy::Supplied
        }
    );
    assert!(matches!(
        output.answer.level,
        Level::Likely | Level::VeryLikely
    ));
    assert_eq!(output.answer.candidates.len(), 1);
    assert_eq!(
        output.answer.candidates[0].owner,
        Owner::Item {
            key: "ExampleMod.dll".into(),
            name: "ExampleMod.dll".into(),
        }
    );
    assert!(english(&output.answer).contains("ExampleMod.dll"));
    let read = output.files[0].read_by.as_ref().unwrap();
    assert_eq!(read.plugin_id, "acme.harbor");
    assert_eq!(read.plugin_name, "Acme Harbor crash rules");
    assert!(output.read_any());
}

#[test]
fn no_files_at_all_is_an_error() {
    let host = idle_host();
    let error = analyze(&AnalyzeInput {
        packs: &[],
        files: &[],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap_err();
    assert_eq!(error.code(), "ERR_CRASH_ANALYZE_NO_FILES");
}

#[test]
fn an_empty_file_and_an_unrecognised_file_name_nobody_and_say_so() {
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let output = analyze(&AnalyzeInput {
        packs: &[pack],
        files: &[
            CrashFile {
                name: "empty.txt".into(),
                bytes: Vec::new(),
            },
            file("wrong.txt", "not a crash log at all\n"),
        ],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert!(output.answer.candidates.is_empty());
    assert!(output.answer.findings.is_empty());
    assert!(!output.read_any());
    assert!(output.files[0].reason.as_deref().unwrap().contains("empty"));
    assert!(output.files[1]
        .reason
        .as_deref()
        .unwrap()
        .contains("recognised"));
}

#[test]
fn a_parser_that_throws_is_could_not_check_naming_the_plugin_and_the_report_says_how() {
    let pack = format!(
        r#"{{
  "contract": 1,
  "parsers": [
    {{
      "id": "broken_log",
      "entry": {{ "script": {{ "export": "parseCrash" }} }},
      "detect": {{ "all": ["(?m){HEADER}"] }}
    }}
  ],
  "rules": []
}}"#
    );
    let (dir, loaded) = plugin_folder(
        "acme.broken",
        "Acme Broken crash rules",
        &pack,
        Some("export async function parseCrash() { throw new Error('the probe broke'); }"),
    );
    let host = active_host(dir.path(), "acme.broken");
    let output = analyze(&AnalyzeInput {
        packs: &[loaded],
        files: &[file("bad.txt", "== FAULTS ==\nmodule Nope.dll failed\n")],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert!(output.answer.candidates.is_empty());
    let note = output
        .answer
        .could_not_check
        .iter()
        .find(|check| check.subject == "broken_log")
        .expect("the failure is shown as could-not-check");
    assert_eq!(
        note.source.as_ref().unwrap().plugin_id,
        "acme.broken",
        "the note names the plugin"
    );
    let failure = output.files[0]
        .failures
        .first()
        .expect("a failure is reported");
    assert_eq!(failure.kind, ParserFailureKind::ScriptThrew);
    assert!(failure.message.contains("the probe broke"), "{failure:?}");
    assert!(!output.read_any());
}

#[test]
fn two_files_where_one_parses_use_it_and_still_list_the_other() {
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let output = analyze(&AnalyzeInput {
        packs: &[pack],
        files: &[
            file(
                "readable.txt",
                "== FAULTS ==\nmodule ExampleMod.dll failed\n",
            ),
            file("junk.txt", "nothing here to read\n"),
        ],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert_eq!(output.answer.candidates.len(), 1);
    assert!(output.files[0].read_by.is_some());
    assert!(output.files[1].read_by.is_none());
    assert!(output.files[1].reason.is_some());
    assert!(output.read_any());
}

#[test]
fn a_system_role_never_outranks_the_module_that_was_also_named() {
    let roles =
        r#"{ "kind": "module", "value": "acmert.dll", "match": "casefold", "role": "system" }"#;
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack(roles, &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let output = analyze(&AnalyzeInput {
        packs: &[pack],
        files: &[file(
            "both.txt",
            "== FAULTS ==\nacmert.dll failed\nmodule ExampleMod.dll failed\n",
        )],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert_eq!(output.answer.candidates.len(), 1);
    assert_eq!(
        output.answer.candidates[0].owner,
        Owner::Item {
            key: "ExampleMod.dll".into(),
            name: "ExampleMod.dll".into(),
        }
    );
    assert!(!output
        .answer
        .candidates
        .iter()
        .any(|candidate| matches!(candidate.owner, Owner::System)));
}

#[test]
fn the_same_input_in_a_different_file_order_gives_the_same_answer() {
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let first = file("a.txt", "== FAULTS ==\nmodule ExampleMod.dll failed\n");
    let second = file("b.txt", "nothing here to read\n");

    let forward = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&pack),
        files: &[first.clone(), second.clone()],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    let backward = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&pack),
        files: &[second, first],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert_eq!(
        serde_json::to_value(&forward.answer).unwrap(),
        serde_json::to_value(&backward.answer).unwrap()
    );
}

#[test]
fn a_file_over_the_read_limits_is_refused_and_said() {
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("module_faulted")),
        None,
    );
    let host = idle_host();
    let too_big = ParserLimits::DEFAULT.max_total_read_bytes as usize + 1;
    let output = analyze(&AnalyzeInput {
        packs: &[pack],
        files: &[CrashFile {
            name: "huge.txt".into(),
            bytes: vec![b'x'; too_big],
        }],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    assert!(!output.read_any());
    assert!(output.answer.candidates.is_empty());
    let reason = output.files[0].reason.as_deref().unwrap();
    assert!(reason.contains("at most"), "{reason}");
}

#[test]
fn a_pack_role_still_applies_without_an_instance() {
    let roles = r#"{ "kind": "module", "value": "acmeengine.dll", "match": "casefold", "role": "framework" }"#;
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack(roles, &fault_rule("module_faulted")),
        None,
    );
    let resolver =
        agora_core::crash_engine::attribution::LooseResolver::from_packs([&pack.pack], &[]);
    let resolved = resolver.resolve(&agora_game_api::crash::Ref {
        kind: "module".into(),
        value: "AcmeEngine.dll".into(),
        strength: Default::default(),
    });
    assert_eq!(
        resolved.owner,
        Owner::Framework {
            name: "AcmeEngine.dll".into()
        }
    );
    assert_eq!(resolved.role, Some(agora_game_api::crash::Role::Framework));
}

fn for_games(pack: String, games: &str) -> String {
    pack.replacen(
        r#""contract": 1,"#,
        &format!(r#""contract": 1, "games": [{games}],"#),
        1,
    )
}

#[test]
fn another_games_rules_stay_out_of_a_loose_file_read_for_one_game() {
    let (_a, harbor) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &for_games(generic_pack("", &fault_rule("harbor_rule")), r#""harbor""#),
        None,
    );
    let (_b, other) = plugin_folder(
        "acme.other",
        "Acme Other crash rules",
        &for_games(
            generic_pack("", &fault_rule("other_game_rule")),
            r#""other""#,
        ),
        None,
    );
    let (_c, every) = plugin_folder(
        "acme.every",
        "Acme rules for every game",
        &generic_pack("", &fault_rule("every_game_rule")),
        None,
    );
    let host = idle_host();
    let output = analyze(&AnalyzeInput {
        packs: &[other, every, harbor],
        files: &[file(
            "pasted.txt",
            "== FAULTS ==\nmodule ExampleMod.dll failed\n",
        )],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();

    let rules: Vec<&str> = output
        .answer
        .candidates
        .iter()
        .flat_map(|candidate| candidate.sources.iter())
        .filter_map(|source| source.rule_id.as_deref())
        .collect();
    // `acme.every` sorts first, so it is the one that read the file; it serves every game, so the
    // file names no game and every pack takes part.
    assert_eq!(
        output.files[0].read_by.as_ref().unwrap().plugin_id,
        "acme.every"
    );
    assert!(rules.contains(&"other_game_rule"), "{rules:?}");

    // Read by the harbor pack alone, the other game's rule must not speak.
    let (_a2, harbor) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &for_games(generic_pack("", &fault_rule("harbor_rule")), r#""harbor""#),
        None,
    );
    let (_b2, other) = plugin_folder(
        "acme.other",
        "Acme Other crash rules",
        &for_games(
            generic_pack("", &fault_rule("other_game_rule")),
            r#""other""#,
        ),
        None,
    );
    let (_c2, universal) = plugin_folder(
        "acme.universal",
        "Acme rules for every game",
        &generic_pack("", &fault_rule("every_game_rule")),
        None,
    );
    let output = analyze(&AnalyzeInput {
        packs: &[universal, other, harbor],
        files: &[file(
            "pasted.txt",
            "== FAULTS ==\nmodule ExampleMod.dll failed\n",
        )],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    assert_eq!(
        output.files[0].read_by.as_ref().unwrap().plugin_id,
        "acme.harbor"
    );
    let rules: Vec<&str> = output
        .answer
        .candidates
        .iter()
        .flat_map(|candidate| candidate.sources.iter())
        .filter_map(|source| source.rule_id.as_deref())
        .collect();
    assert!(rules.contains(&"harbor_rule"), "{rules:?}");
    assert!(rules.contains(&"every_game_rule"), "{rules:?}");
    assert!(!rules.contains(&"other_game_rule"), "{rules:?}");
}

fn many_faults(count: usize) -> String {
    let mut text = String::from("== FAULTS ==\n");
    for index in 0..count {
        text.push_str(&format!("module Invented{index}.dll failed\n"));
    }
    text
}

fn with_max_branches(rule: String, limit: u32) -> String {
    rule.replacen(
        r#""version": 1,"#,
        &format!(r#""version": 1, "max_branches": {limit},"#),
        1,
    )
}

#[test]
fn a_rule_over_a_long_list_fires_only_when_it_asks_for_enough_branches() {
    let host = idle_host();
    let log = many_faults(40);

    let (_a, default_limit) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &fault_rule("every_fault")),
        None,
    );
    let output = analyze(&AnalyzeInput {
        packs: &[default_limit],
        files: &[file("crash.txt", &log)],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    assert!(output.answer.candidates.is_empty());
    assert!(output
        .answer
        .could_not_check
        .iter()
        .any(|note| note.reason == agora_game_api::crash::CouldNotCheckReason::TooManyMatches));

    let (_b, raised) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", &with_max_branches(fault_rule("every_fault"), 64)),
        None,
    );
    let output = analyze(&AnalyzeInput {
        packs: &[raised],
        files: &[file("crash.txt", &log)],
        runtime_version: None,
        host: &host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    assert_eq!(output.answer.candidates.len(), 40);
    assert!(!output
        .answer
        .could_not_check
        .iter()
        .any(|note| note.reason == agora_game_api::crash::CouldNotCheckReason::TooManyMatches));
}

#[test]
fn a_fact_about_something_else_is_not_a_match_even_when_its_attribute_is_unknown() {
    // The rule wants `Wanted.dll` with a size it cannot judge on these facts (no `size` attribute).
    // A fault about another module is simply not a match: no "could not check" for it.
    let rule = r#"{
  "id": "wanted_with_size",
  "version": 1,
  "when": {
    "fact": { "kind": "acme.fault", "where": { "size": { "gte": 1 } },
              "ref": { "kind": "module", "value": "Wanted.dll", "bind": "m" } }
  },
  "effects": [
    { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 16 } }
  ],
  "text": { "headline": { "en": "{owner:m} failed." } }
}"#;
    let (_dir, pack) = plugin_folder(
        "acme.harbor",
        "Acme Harbor crash rules",
        &generic_pack("", rule),
        None,
    );
    let host = idle_host();
    let run = |text: &str| {
        analyze(&AnalyzeInput {
            packs: std::slice::from_ref(&pack),
            files: &[file("crash.txt", text)],
            runtime_version: None,
            host: &host,
            built_in: &[],
            instance: None,
        })
        .unwrap()
    };
    let unknown_notes = |output: &agora_core::crash_engine::AnalyzeOutput| {
        output
            .answer
            .could_not_check
            .iter()
            .filter(|note| {
                note.source
                    .as_ref()
                    .and_then(|source| source.rule_id.as_deref())
                    == Some("wanted_with_size")
            })
            .count()
    };
    let elsewhere = run("== FAULTS ==\nmodule Other.dll failed\n");
    assert_eq!(
        unknown_notes(&elsewhere),
        0,
        "{:#?}",
        elsewhere.answer.could_not_check
    );
    // The same unknown attribute on the module it asks about still says it could not check.
    let wanted = run("== FAULTS ==\nmodule Wanted.dll failed\n");
    assert_eq!(
        unknown_notes(&wanted),
        1,
        "{:#?}",
        wanted.answer.could_not_check
    );
}
