//! Conditional launch arguments (MASTER_SPEC §30.12): a true `when` adds the argument in its
//! declared place, and an unknown one leaves it out and says so.

use agora_core::game_launch::{resolve_recipe_with_facts, LaunchRoots};
use agora_game_api::{
    Condition, ConditionFacts, FrameworkId, GamePath, LaunchRecipe, LaunchValue, RelPath,
    VersionGate,
};
use std::collections::BTreeMap;
use tempfile::TempDir;

struct Facts {
    present: Option<bool>,
}

impl ConditionFacts for Facts {
    fn present(&self, _path: &RelPath) -> Option<bool> {
        self.present
    }

    fn framework(&self, _framework: &FrameworkId) -> Option<Option<String>> {
        None
    }

    fn game_version(&self) -> Option<String> {
        None
    }
}

fn recipe(arguments: Vec<LaunchValue>) -> LaunchRecipe {
    LaunchRecipe {
        executable: GamePath::Runtime {
            path: RelPath::new("Game.exe").unwrap(),
        },
        arguments,
        environment: BTreeMap::new(),
        working_directory: GamePath::Runtime {
            path: RelPath::default(),
        },
    }
}

fn literal(value: &str) -> LaunchValue {
    LaunchValue::Literal {
        value: value.into(),
    }
}

fn conditional(value: &str, present: Option<bool>) -> LaunchValue {
    let when = Condition::Present(RelPath::new("marker.txt").unwrap());
    let _ = present;
    LaunchValue::Conditional {
        value: Box::new(literal(value)),
        when,
    }
}

fn roots(dir: &TempDir) -> LaunchRoots {
    LaunchRoots {
        runtime: dir.path().to_path_buf(),
        install: None,
        base: None,
    }
}

fn run(arguments: Vec<LaunchValue>, facts: &dyn ConditionFacts) -> (Vec<String>, Vec<String>) {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("Game.exe"), b"fake").unwrap();
    let (resolved, notes) =
        resolve_recipe_with_facts(&recipe(arguments), &roots(&dir), facts).unwrap();
    let args = resolved
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    (args, notes)
}

#[test]
fn an_unknown_when_leaves_the_launch_arg_off() {
    let (args, notes) = run(
        vec![literal("-a"), conditional("-b", None), literal("-c")],
        &Facts { present: None },
    );
    assert_eq!(args, ["-a", "-c"]);
    assert!(
        notes.iter().any(|note| note.contains("Could not tell")),
        "{notes:?}"
    );
}

#[test]
fn a_when_with_an_unreadable_view_is_unknown_and_logged() {
    let (args, notes) = run(vec![conditional("-only", None)], &Facts { present: None });
    assert!(args.is_empty(), "{args:?}");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("marker.txt"), "{notes:?}");
    assert!(notes[0].contains("left out"), "{notes:?}");
}

#[test]
fn a_true_when_adds_the_arg_in_declared_order() {
    let (args, notes) = run(
        vec![literal("-a"), conditional("-b", Some(true)), literal("-c")],
        &Facts {
            present: Some(true),
        },
    );
    assert_eq!(args, ["-a", "-b", "-c"]);
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_false_when_leaves_the_arg_off_without_a_note() {
    let (args, notes) = run(
        vec![literal("-a"), conditional("-b", Some(false))],
        &Facts {
            present: Some(false),
        },
    );
    assert_eq!(args, ["-a"]);
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_version_gate_on_an_unknown_game_version_is_unknown() {
    let when = Condition::Version(VersionGate {
        min: Some("2.0".into()),
        ..Default::default()
    });
    let arguments = vec![LaunchValue::Conditional {
        value: Box::new(literal("-new")),
        when,
    }];
    // `Facts::game_version` is None, so the gate is unknown and the argument stays off.
    let (args, notes) = run(
        arguments,
        &Facts {
            present: Some(true),
        },
    );
    assert!(args.is_empty(), "{args:?}");
    assert!(
        notes.iter().any(|n| n.contains("game version")),
        "{notes:?}"
    );
}
