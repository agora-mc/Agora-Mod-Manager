//! Hostile tests for the save-risk classifier and its instance file lookup. Each is named for the
//! mistake it catches: guessing "safe" when a file is unknown, or claiming anything without an
//! instance.

use agora_core::crash_engine::attribution::{InstanceView, LayerOwner, LayerView};
use agora_core::crash_engine::save_risk::{apply, classify, merge};
use agora_core::crash_engine::{RefResolver, Resolved};
use agora_game_api::crash::{
    lint_pack_json, Candidate, Cause, Family, Level, LintCode, Owner, Ref, SaveRisk, SaveRiskLabel,
};
use std::collections::BTreeMap;

fn rules(risky: &[&str], safe: &[&str]) -> SaveRisk {
    SaveRisk {
        risky: risky.iter().map(|pattern| pattern.to_string()).collect(),
        safe: safe.iter().map(|pattern| pattern.to_string()).collect(),
    }
}

fn files(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

fn skyrim_rules() -> SaveRisk {
    rules(
        &[
            "Data/**/*.esp",
            "Data/**/*.esm",
            "Data/**/*.esl",
            "Data/**/*.pex",
            "Data/**/*.dll",
        ],
        &["Data/textures/**", "Data/meshes/**", "Data/sound/**"],
    )
}

#[test]
fn an_empty_file_list_is_unknown_never_safe() {
    assert_eq!(classify(None, &skyrim_rules()), SaveRiskLabel::Unknown);
    assert_eq!(classify(Some(&[]), &skyrim_rules()), SaveRiskLabel::Unknown);
}

#[test]
fn all_assets_are_safe() {
    let files = files(&[
        "Data/textures/actors/example.dds",
        "Data/meshes/example.nif",
        "Data/sound/fx/example.wav",
    ]);
    assert_eq!(classify(Some(&files), &skyrim_rules()), SaveRiskLabel::Safe);
}

#[test]
fn one_plugin_makes_the_whole_item_risky() {
    let files = files(&[
        "Data/textures/example.dds",
        "Data/Example.esp",
        "Data/SKSE/Plugins/Example.dll",
    ]);
    assert_eq!(
        classify(Some(&files), &skyrim_rules()),
        SaveRiskLabel::Risky
    );
}

#[test]
fn an_unmatched_file_makes_the_item_unknown() {
    let files = files(&["Data/textures/example.dds", "Data/readme.txt"]);
    assert_eq!(
        classify(Some(&files), &skyrim_rules()),
        SaveRiskLabel::Unknown
    );
}

#[test]
fn a_risky_match_wins_even_when_a_safe_match_also_fits() {
    let rules = rules(&["Data/**/*.dll"], &["Data/**"]);
    let files = files(&["Data/SKSE/Plugins/Example.dll"]);
    assert_eq!(classify(Some(&files), &rules), SaveRiskLabel::Risky);
}

#[test]
fn paths_are_compared_case_insensitively_and_accept_windows_separators() {
    let files = files(&[r"data\TEXTURES\Example.DDS"]);
    assert_eq!(classify(Some(&files), &skyrim_rules()), SaveRiskLabel::Safe);
}

#[test]
fn a_pack_that_says_nothing_leaves_every_suspect_alone() {
    assert_eq!(merge(std::iter::empty()), None);
    assert_eq!(merge([&SaveRisk::default()]), None);
}

#[test]
fn merging_deduplicates_and_keeps_both_lists_deterministic() {
    let first = rules(&["Data/**/*.dll"], &["Data/textures/**"]);
    let second = rules(&["Data/**/*.dll", "Data/**/*.esp"], &["Data/textures/**"]);
    let merged = merge([&first, &second]).expect("the packs said something");
    assert_eq!(merged.risky, ["Data/**/*.dll", "Data/**/*.esp"]);
    assert_eq!(merged.safe, ["Data/textures/**"]);
}

#[test]
fn only_installed_item_suspects_are_labelled() {
    let mut candidates = vec![
        candidate(Owner::Item {
            key: "asset-mod".to_string(),
            name: "Asset mod".to_string(),
        }),
        candidate(Owner::Base),
        candidate(Owner::Framework {
            name: "Engine".to_string(),
        }),
    ];
    let resolver = FakeResolver::new(BTreeMap::from([(
        "asset-mod".to_string(),
        Some(files(&["Data/textures/example.dds"])),
    )]));
    apply(&mut candidates, &skyrim_rules(), &resolver);

    assert_eq!(candidates[0].save_risk, Some(SaveRiskLabel::Safe));
    assert_eq!(candidates[1].save_risk, None);
    assert_eq!(candidates[2].save_risk, None);
}

#[test]
fn a_resolver_that_cannot_list_files_says_unknown_not_safe() {
    let mut candidates = vec![candidate(Owner::Item {
        key: "unlisted".to_string(),
        name: "Unlisted".to_string(),
    })];
    apply(&mut candidates, &skyrim_rules(), &FakeResolver::default());
    assert_eq!(candidates[0].save_risk, Some(SaveRiskLabel::Unknown));
}

#[test]
fn the_instance_view_lists_only_enabled_layers_for_the_item() {
    let view = InstanceView::new(vec![
        LayerView::new(
            "one",
            true,
            LayerOwner::Item {
                key: "mod-a".to_string(),
                name: "Mod A".to_string(),
            },
            files(&["Data/A.esp", "Data/shared.txt"]),
        ),
        LayerView::new(
            "two",
            false,
            LayerOwner::Item {
                key: "mod-a".to_string(),
                name: "Mod A".to_string(),
            },
            files(&["Data/Hidden.esp"]),
        ),
        LayerView::new(
            "three",
            true,
            LayerOwner::Item {
                key: "mod-b".to_string(),
                name: "Mod B".to_string(),
            },
            files(&["Data/B.esp"]),
        ),
    ]);
    assert_eq!(
        view.item_files("mod-a"),
        Some(files(&["Data/A.esp", "Data/shared.txt"]))
    );
    assert_eq!(view.item_files("mod-b"), Some(files(&["Data/B.esp"])));
    assert_eq!(view.item_files("missing"), Some(Vec::new()));
    assert_eq!(
        InstanceView::unavailable("cannot list").item_files("mod-a"),
        None
    );
}

#[test]
fn a_good_save_risk_lints_clean() {
    let pack = r#"{
        "contract": 1,
        "save_risk": {
            "risky": ["Data/**/*.esp", "Data/**/*.dll"],
            "safe": ["Data/textures/**"]
        }
    }"#;
    lint_pack_json(pack).expect("a valid save_risk lints");
}

#[test]
fn an_empty_save_risk_is_refused() {
    let problems = lint_pack_json(r#"{ "contract": 1, "save_risk": {} }"#).unwrap_err();
    assert!(
        problems
            .iter()
            .any(|problem| problem.code == LintCode::BadSaveRisk),
        "{problems:#?}"
    );
}

#[test]
fn absolute_climbing_and_backslash_patterns_are_refused() {
    for pattern in [
        "/Data/*.esp",
        "C:/Data/*.esp",
        "../outside.esp",
        r"Data\*.esp",
    ] {
        let json = format!(
            r#"{{ "contract": 1, "save_risk": {{ "risky": [{:?}] }} }}"#,
            pattern
        );
        let problems = lint_pack_json(&json).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|problem| problem.code == LintCode::BadSaveRisk),
            "{pattern}: {problems:#?}"
        );
    }
}

#[test]
fn an_unknown_field_in_save_risk_is_refused() {
    let problems = lint_pack_json(
        r#"{ "contract": 1, "save_risk": { "risky": ["Data/*.esp"], "surprise": [] } }"#,
    )
    .unwrap_err();
    assert!(
        problems
            .iter()
            .any(|problem| problem.code == LintCode::Parse),
        "{problems:#?}"
    );
}

fn candidate(owner: Owner) -> Candidate {
    Candidate {
        owner,
        level: Level::Possible,
        score: 1,
        cause: Cause::Mod,
        families: vec![Family::Execution],
        sources: Vec::new(),
        also_provided_by: Vec::new(),
        save_risk: None,
    }
}

#[derive(Default)]
struct FakeResolver {
    items: BTreeMap<String, Option<Vec<String>>>,
}

impl FakeResolver {
    fn new(items: BTreeMap<String, Option<Vec<String>>>) -> Self {
        Self { items }
    }
}

impl RefResolver for FakeResolver {
    fn resolve(&self, _reference: &Ref) -> Resolved {
        Resolved {
            owner: Owner::Unknown,
            role: None,
        }
    }

    fn item_files(&self, content_key: &str) -> Option<Vec<String>> {
        self.items.get(content_key).cloned().flatten()
    }
}
