//! The supplied-file path the CLI and desktop share: loose text through the real pipeline, and an
//! empty input that says "nothing to read" rather than giving a clean bill of health.
//!
//! Every log and name here is invented for this test.

use agora_core::crash_engine::CrashFile;
use agora_core::crash_loose::{analyze_supplied, SuppliedAnalysis, SuppliedAnswer};
use agora_core::crash_packs::LoadedCrashPack;
use agora_core::ctx::Ctx;
use agora_plugin_host::QuickJsHost;

const PACK: &str = r#"{
  "contract": 1,
  "parsers": [{
    "id": "supplied_log",
    "entry": { "generic": { "name": "text_sections", "config": {
      "sections": [{
        "id": "faults",
        "header": "^== FAULTS ==$",
        "lines": [{
          "pattern": "^module (?P<module>[A-Za-z0-9_.-]+) failed$",
          "kind": "acme.fault",
          "refs": [{ "kind": "module", "value": "module" }]
        }]
      }]
    } } },
    "detect": { "all": ["(?m)^== FAULTS ==$"] }
  }],
  "rules": [{
    "id": "module_faulted",
    "version": 1,
    "when": { "fact": { "kind": "acme.fault", "ref": { "kind": "module", "bind": "m" } } },
    "effects": [{
      "signal": {
        "family": "execution",
        "candidate": { "owner_of": "m" },
        "weight": 16
      }
    }],
    "text": { "headline": { "en": "{owner:m} failed while the game was running." } }
  }]
}"#;

fn pack() -> LoadedCrashPack {
    LoadedCrashPack {
        plugin_id: "acme.harbor".to_string(),
        plugin_name: "Acme Harbor crash rules".to_string(),
        plugin_version: "1.0.0".to_string(),
        dependencies: Vec::new(),
        official: false,
        file: "pack.json".to_string(),
        pack: serde_json::from_str(PACK).expect("the test pack is valid JSON"),
    }
}

fn file(name: &str, text: &str) -> CrashFile {
    CrashFile::new(name, text.as_bytes().to_vec())
}

fn analyze(text: &str) -> SuppliedAnswer {
    let root = tempfile::tempdir().unwrap();
    let ctx = Ctx::for_testing(root.path().to_path_buf());
    let host = QuickJsHost::new();
    let packs = [pack()];
    analyze_supplied(
        &ctx,
        SuppliedAnalysis {
            packs: &packs,
            built_in: &[],
            host: &host,
            files: &[file("friend-crash.txt", text)],
            instance_id: None,
            runtime_version: None,
        },
    )
    .unwrap()
}

#[test]
fn empty_input_is_an_explicit_nothing_to_read() {
    let answer = analyze("   \n\t\n");
    let SuppliedAnswer::NothingToRead { why } = answer else {
        panic!("an empty log must not produce an answer");
    };
    assert!(why.contains("empty"), "{why}");
}

#[test]
fn supplied_text_runs_the_pipeline_and_names_what_it_named_itself() {
    let answer = analyze("== FAULTS ==\nmodule ExampleMod.dll failed\n");
    let SuppliedAnswer::Analyzed { answer, files, .. } = answer else {
        panic!("a readable log should produce an answer");
    };
    assert_eq!(answer.candidates.len(), 1);
    match &answer.candidates[0].owner {
        agora_game_api::crash::Owner::Item { name, .. } => assert_eq!(name, "ExampleMod.dll"),
        other => panic!("expected an item owner, got {other:?}"),
    }
    assert_eq!(files[0].name, "friend-crash.txt");
}
