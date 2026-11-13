//! Content rules place a whole item before the layout, and the odd names an archive can produce
//! are sent to the player rather than guessed (MASTER_SPEC §30.12).

use agora_game_api::{
    Condition, ConditionFacts, ContentLayout, ContentMatch, ContentRule, FrameworkId, RelPath,
    Suggestion,
};
use std::collections::BTreeSet;

struct Facts {
    present: BTreeSet<String>,
    framework: Option<Option<String>>,
    version: Option<String>,
}

impl Facts {
    fn none() -> Self {
        Self {
            present: BTreeSet::new(),
            framework: Some(None),
            version: None,
        }
    }
}

impl ConditionFacts for Facts {
    fn present(&self, path: &RelPath) -> Option<bool> {
        Some(self.present.contains(path.as_str()))
    }

    fn framework(&self, _framework: &FrameworkId) -> Option<Option<String>> {
        self.framework.clone()
    }

    fn game_version(&self) -> Option<String> {
        self.version.clone()
    }
}

fn paths(files: &[&str]) -> Vec<RelPath> {
    files.iter().map(|f| RelPath::new(*f).unwrap()).collect()
}

fn layout() -> ContentLayout {
    ContentLayout {
        data_path: RelPath::new("Data").unwrap(),
        data_markers: vec!["*.esp".into()],
        root_markers: vec!["*.exe".into()],
        thunderstore_bepinex: false,
    }
}

fn marker_rule(kind: &str, marker: &str, mount: &str) -> ContentRule {
    ContentRule {
        kind: kind.into(),
        matching: ContentMatch {
            marker: Some(marker.into()),
            all_files: None,
        },
        mount: RelPath::new(mount).unwrap(),
        when: None,
    }
}

fn all_files_rule(kind: &str, pattern: &str, mount: &str) -> ContentRule {
    ContentRule {
        kind: kind.into(),
        matching: ContentMatch {
            marker: None,
            all_files: Some(pattern.into()),
        },
        mount: RelPath::new(mount).unwrap(),
        when: None,
    }
}

fn place(files: &[&str], rules: &[ContentRule], item: &str) -> Suggestion {
    agora_game_api::suggest_placement_with_rules(
        &paths(files),
        &layout(),
        rules,
        Some(&Facts::none()),
        item,
        None,
    )
}

#[test]
fn a_marker_rule_mounts_its_folder() {
    let rules = [marker_rule("REDmod mod", "info.json", "mods/{folder}")];
    match place(
        &["Wrapper/MyMod/info.json", "Wrapper/MyMod/data/things.bin"],
        &rules,
        "Whatever",
    ) {
        Suggestion::Place {
            source_path,
            mount_path,
            reason,
        } => {
            assert_eq!(source_path.as_str(), "Wrapper/MyMod");
            assert_eq!(mount_path.as_str(), "mods/MyMod");
            assert!(reason.contains("REDmod mod"), "{reason}");
        }
        other => panic!("expected a place, got {other:?}"),
    }
}

#[test]
fn an_all_files_rule_needs_every_file() {
    let rules = [all_files_rule("Archive mod", "*.archive", "archive/pc/mod")];
    assert!(matches!(
        place(&["a.archive", "b.archive"], &rules, "Pack"),
        Suggestion::Place { .. }
    ));
    match place(&["a.archive", "readme.txt"], &rules, "Pack") {
        Suggestion::Unknown { .. } => {}
        other => panic!("a stray file must stop the rule: {other:?}"),
    }
}

#[test]
fn first_matching_rule_wins() {
    let rules = [
        marker_rule("First", "info.json", "first/{folder}"),
        marker_rule("Second", "info.json", "second/{folder}"),
    ];
    match place(&["Mod/info.json"], &rules, "Mod") {
        Suggestion::Place {
            mount_path, reason, ..
        } => {
            assert_eq!(mount_path.as_str(), "first/Mod");
            assert!(reason.contains("First"), "{reason}");
        }
        other => panic!("expected the first rule, got {other:?}"),
    }
}

#[test]
fn no_rule_falls_through_to_the_layout_then_classify_then_unknown() {
    // The layout knows `*.esp` is Data content.
    match place(&["Thing.esp"], &[], "Thing") {
        Suggestion::Place { mount_path, .. } => assert_eq!(mount_path.as_str(), "Data"),
        other => panic!("expected the layout, got {other:?}"),
    }
    // No layout match: classify gets the item.
    let files = paths(&["notes.txt"]);
    let suggestion = agora_game_api::suggest_placement_with_rules(
        &files,
        &layout(),
        &[],
        Some(&Facts::none()),
        "Notes",
        Some(&|_name, _files| {
            Some(Suggestion::Place {
                source_path: RelPath::default(),
                mount_path: RelPath::new("classified").unwrap(),
                reason: "the package classified it".into(),
            })
        }),
    );
    assert_eq!(suggestion_mount(&suggestion), Some("classified"));
    // Nothing matched and nothing classified.
    assert!(matches!(
        place(&["notes.txt"], &[], "Notes"),
        Suggestion::Unknown { .. }
    ));
}

#[test]
fn classify_gets_the_item_name_and_its_folder_placeholder_is_checked() {
    let files = paths(&["init.reds"]);
    let classified = agora_game_api::suggest_placement_with_rules(
        &files,
        &layout(),
        &[],
        Some(&Facts::none()),
        "CoolRedscript",
        Some(&|name, _files| {
            Some(Suggestion::Place {
                source_path: RelPath::default(),
                mount_path: RelPath::new("r6/scripts/{folder}").unwrap(),
                reason: format!("classified from the hint {name}"),
            })
        }),
    );
    assert_eq!(
        suggestion_mount(&classified),
        Some("r6/scripts/CoolRedscript")
    );

    // A name that cannot be a folder is refused, never used as a path.
    let hostile = agora_game_api::suggest_placement_with_rules(
        &files,
        &layout(),
        &[],
        Some(&Facts::none()),
        "CON",
        Some(&|_name, _files| {
            Some(Suggestion::Place {
                source_path: RelPath::default(),
                mount_path: RelPath::new("r6/scripts/{folder}").unwrap(),
                reason: "classified".into(),
            })
        }),
    );
    match hostile {
        Suggestion::Unknown {
            note: Some(note), ..
        } => assert!(note.contains("folder"), "{note}"),
        other => panic!("a hostile name must be refused, got {other:?}"),
    }
}

fn suggestion_mount(suggestion: &Suggestion) -> Option<&str> {
    match suggestion {
        Suggestion::Place { mount_path, .. } => Some(mount_path.as_str()),
        _ => None,
    }
}

#[test]
fn a_folder_name_of_dotdot_from_an_archive_is_refused() {
    // The item's own name is used when the marker is at its root.
    let rules = [marker_rule("Mod", "info.json", "mods/{folder}")];
    assert!(matches!(
        place(&["info.json"], &rules, ".."),
        Suggestion::Unknown { .. }
    ));
}

#[test]
fn a_device_name_folder_is_refused() {
    let rules = [marker_rule("Mod", "info.json", "mods/{folder}")];
    assert!(matches!(
        place(&["info.json"], &rules, "CON"),
        Suggestion::Unknown { .. }
    ));
    assert!(matches!(
        place(&["CON/info.json"], &rules, "Pack"),
        Suggestion::Unknown { .. }
    ));
}

#[test]
fn a_trailing_dot_folder_is_refused() {
    let rules = [marker_rule("Mod", "info.json", "mods/{folder}")];
    assert!(matches!(
        place(&["info.json"], &rules, "Mod."),
        Suggestion::Unknown { .. }
    ));
}

#[test]
fn marker_depth_is_bounded() {
    let rules = [marker_rule("Mod", "info.json", "mods/{folder}")];
    let deep = format!(
        "{}info.json",
        "a/".repeat(agora_game_api::PLACEMENT_MAX_DEPTH + 1)
    );
    assert!(matches!(
        place(&[deep.as_str()], &rules, "Pack"),
        Suggestion::Unknown { .. }
    ));
    let at_limit = format!(
        "{}info.json",
        "a/".repeat(agora_game_api::PLACEMENT_MAX_DEPTH)
    );
    assert!(matches!(
        place(&[at_limit.as_str()], &rules, "Pack"),
        Suggestion::Place { .. }
    ));
}

#[test]
fn a_marker_in_two_folders_does_not_pick_arbitrarily() {
    let rules = [marker_rule("Mod", "info.json", "mods/{folder}")];
    assert!(matches!(
        place(&["One/info.json", "Two/info.json"], &rules, "Pack"),
        Suggestion::Unknown { .. }
    ));
}

#[test]
fn an_unknown_when_leaves_the_rule_off_and_the_reason_says_so() {
    let mut rule = marker_rule("REDmod mod", "info.json", "mods/{folder}");
    rule.when = Some(Condition::Present(
        RelPath::new("unreadable-marker").unwrap(),
    ));
    let mut facts = Facts::none();
    facts.present.clear();
    // An unreadable fact: the condition cannot be judged.
    struct Unreadable;
    impl ConditionFacts for Unreadable {
        fn present(&self, _path: &RelPath) -> Option<bool> {
            None
        }
        fn framework(&self, _framework: &FrameworkId) -> Option<Option<String>> {
            None
        }
        fn game_version(&self) -> Option<String> {
            None
        }
    }
    let suggestion = agora_game_api::suggest_placement_with_rules(
        &paths(&["info.json", "Thing.esp"]),
        &layout(),
        &[rule],
        Some(&Unreadable),
        "Pack",
        None,
    );
    match suggestion {
        Suggestion::Place { reason, .. } => {
            assert!(reason.contains("REDmod mod"), "{reason}");
            assert!(reason.contains("could not tell"), "{reason}");
        }
        other => panic!("expected the layout to place it, got {other:?}"),
    }
}

#[test]
fn a_false_when_skips_the_rule() {
    let mut rule = marker_rule("REDmod mod", "info.json", "mods/{folder}");
    rule.when = Some(Condition::Present(RelPath::new("never-there").unwrap()));
    match place(&["info.json"], &[rule], "Pack") {
        Suggestion::Unknown { .. } => {}
        other => panic!("a false rule must not place: {other:?}"),
    }
}
