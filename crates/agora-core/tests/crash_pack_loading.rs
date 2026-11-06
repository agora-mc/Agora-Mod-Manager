//! Pack loading: every refusal names the file, and one bad pack never hides a
//! good one.

use agora_core::crash_packs::{
    load_for_plugins, load_plugin_packs, validate_plugin_folder, PluginPackSource,
};
use agora_plugin_api::contributions::CrashPackContribution;
use agora_plugin_api::manifest::PluginManifest;
use std::path::Path;

fn manifest(packs: &[&str]) -> PluginManifest {
    PluginManifest::parse(
        &serde_json::json!({
            "manifest": 1,
            "id": "acme.skyrim",
            "name": "Skyrim crash rules",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "capabilities": { "required": ["crash:contribute"] },
            "contributions": {
                "crashPacks": packs.iter().map(|path| serde_json::json!({ "path": path })).collect::<Vec<_>>()
            }
        })
        .to_string(),
    )
    .unwrap()
}

fn manifest_without_packs() -> PluginManifest {
    PluginManifest::parse(
        &serde_json::json!({
            "manifest": 1,
            "id": "acme.skyrim",
            "name": "Skyrim crash rules",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2"
        })
        .to_string(),
    )
    .unwrap()
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative.replace('\\', "/"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

#[test]
fn the_four_worked_example_packs_pass_validation() {
    let examples = [
        (
            "minecraft",
            include_str!("../../agora-game-api/tests/crash_examples/minecraft.pack.json"),
        ),
        (
            "skyrim",
            include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
        ),
        (
            "cyberpunk",
            include_str!("../../agora-game-api/tests/crash_examples/cyberpunk.pack.json"),
        ),
        (
            "ironfen",
            include_str!("../../agora-game-api/tests/crash_examples/ironfen.pack.json"),
        ),
    ];
    for (name, json) in examples {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crash/pack.json", json);
        let manifest = manifest(&["crash/pack.json"]);
        let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
        assert!(report.is_clean(), "{name}: {:#?}", report.problems);
        assert_eq!(report.packs.len(), 1, "{name}");
    }
}

#[test]
fn a_pack_without_the_capability_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "crash/pack.json",
        include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
    );
    let mut manifest = manifest_without_packs();
    manifest
        .contributions
        .crash_packs
        .push(CrashPackContribution {
            path: "crash/pack.json".into(),
        });
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems.len(), 1);
    assert_eq!(report.problems[0].code, "capability");
    assert!(report.problems[0].message.contains("crash:contribute"));
}

#[test]
fn a_pack_path_that_climbs_out_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = manifest(&["crash/pack.json"]);
    manifest.contributions.crash_packs[0].path = "../outside.json".into();
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "path");
}

#[test]
fn an_absolute_pack_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "/outside.json",
        "C:/outside.json",
        r"\\server\share\pack.json",
    ] {
        let mut manifest = manifest(&["crash/pack.json"]);
        manifest.contributions.crash_packs[0].path = bad.into();
        let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
        assert!(report.packs.is_empty(), "{bad}");
        assert_eq!(report.problems[0].code, "path", "{bad}");
    }
}

#[test]
fn a_missing_pack_file_is_a_validation_error() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manifest(&["crash/missing.json"]);
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "missing");
}

#[test]
fn an_unparsable_pack_is_a_validation_error() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "crash/pack.json", "{ this is not json");
    let manifest = manifest(&["crash/pack.json"]);
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "parse");
}

#[test]
fn a_wrong_contract_version_is_a_validation_error() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "crash/pack.json",
        r#"{ "contract": 99, "parsers": [] }"#,
    );
    let manifest = manifest(&["crash/pack.json"]);
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "contractversion");
}

#[test]
fn a_lint_problem_names_the_file_and_the_rule() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "crash/pack.json",
        r#"{
          "contract": 1,
          "rules": [{
            "id": "says_certain",
            "version": 1,
            "when": { "fact": { "kind": "core.exception" } },
            "effects": [{ "finding": { "scope": "root_cause", "level": "possible" } }],
            "text": { "headline": { "en": "This is definitely the cause." } }
          }]
        }"#,
    );
    let manifest = manifest(&["crash/pack.json"]);
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].file, "crash/pack.json");
    assert_eq!(report.problems[0].at, "rule `says_certain`");
    assert!(
        report.problems.iter().any(|problem| {
            problem.code == "wording" && problem.message.to_lowercase().contains("definitely")
        }),
        "{:#?}",
        report.problems
    );
}

#[test]
fn two_packs_with_one_broken_keeps_the_good_one() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "crash/good.json",
        include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
    );
    write(
        dir.path(),
        "crash/bad.json",
        r#"{ "contract": 1, "rules": [}"#,
    );
    let manifest = manifest(&["crash/good.json", "crash/bad.json"]);
    let report = load_plugin_packs("acme.skyrim", dir.path(), &manifest);
    assert_eq!(report.packs.len(), 1);
    assert!(!report.problems.is_empty());
}

#[test]
fn one_broken_plugin_does_not_stop_another() {
    let good = tempfile::tempdir().unwrap();
    write(
        good.path(),
        "crash/pack.json",
        include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
    );
    let bad = tempfile::tempdir().unwrap();
    write(bad.path(), "crash/pack.json", r#"{ "contract": 42 }"#);

    let good_manifest = manifest(&["crash/pack.json"]);
    let bad_manifest = manifest(&["crash/pack.json"]);
    let report = load_for_plugins(&[
        PluginPackSource {
            plugin_id: "acme.skyrim",
            root: good.path(),
            manifest: &good_manifest,
        },
        PluginPackSource {
            plugin_id: "acme.cyberpunk",
            root: bad.path(),
            manifest: &bad_manifest,
        },
    ]);
    assert_eq!(report.packs.len(), 1);
    assert_eq!(report.packs[0].plugin_id, "acme.skyrim");
    assert_eq!(report.problems.len(), 1);
    assert_eq!(report.problems[0].plugin_id, "acme.cyberpunk");
}

#[test]
fn validate_plugin_folder_reads_the_manifest_and_the_packs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agora-plugin.json"),
        serde_json::json!({
            "manifest": 1,
            "id": "acme.skyrim",
            "name": "Skyrim crash rules",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "capabilities": { "required": ["crash:contribute"] },
            "contributions": { "crashPacks": [{ "path": "crash/pack.json" }] }
        })
        .to_string(),
    )
    .unwrap();
    write(
        dir.path(),
        "crash/pack.json",
        include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
    );
    let report = validate_plugin_folder(dir.path()).unwrap();
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.packs.len(), 1);
}
