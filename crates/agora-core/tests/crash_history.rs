//! Hostile tests for the cross-crash summary and its crash-place contract. Each is named for the
//! mistake it catches.

use agora_core::crash_engine::history::{
    crash_place, summarize, CrashPlace, CrashRecord, HistorySuspect,
};
use agora_game_api::crash::{AttrValue, Fact, Owner, Ref, Role, Scalar, Strength};
use std::collections::BTreeMap;

fn item(key: &str) -> Owner {
    Owner::Item {
        key: key.to_string(),
        name: format!("{key} mod"),
    }
}

fn suspect(key: &str, role: Option<Role>) -> HistorySuspect {
    HistorySuspect {
        owner: item(key),
        role,
    }
}

fn place(module: &str, offset: &str) -> CrashPlace {
    CrashPlace {
        kind: "module".to_string(),
        value: module.to_string(),
        location: Some(offset.to_string()),
    }
}

fn record(candidates: Vec<HistorySuspect>, place: Option<CrashPlace>) -> CrashRecord {
    CrashRecord {
        artifact: "crash.log".to_string(),
        readable: true,
        candidates,
        place,
    }
}

#[test]
fn an_empty_list_says_nothing_and_counts_nothing() {
    let summary = summarize(&[]);
    assert_eq!(summary.crashes, 0);
    assert_eq!(summary.unreadable, 0);
    assert_eq!(summary.named_nobody, 0);
    assert!(summary.suspects.is_empty());
    assert!(summary.always_running.is_empty());
}

#[test]
fn an_unreadable_crash_is_counted_and_never_treated_as_a_clean_one() {
    let summary = summarize(&[CrashRecord {
        artifact: "unreadable.log".to_string(),
        readable: false,
        // Even if a caller handed suspects for an unreadable file, they are not counted.
        candidates: vec![suspect("ghost", None)],
        place: Some(place("Ghost.dll", "0x1")),
    }]);
    assert_eq!(summary.crashes, 1);
    assert_eq!(summary.unreadable, 1);
    assert_eq!(summary.named_nobody, 0);
    assert!(summary.suspects.is_empty());
}

#[test]
fn a_crash_that_named_nobody_is_counted_separately() {
    let summary = summarize(&[record(Vec::new(), None)]);
    assert_eq!(summary.crashes, 1);
    assert_eq!(summary.unreadable, 0);
    assert_eq!(summary.named_nobody, 1);
    assert!(summary.suspects.is_empty());
}

#[test]
fn the_same_place_repeated_is_one_bug() {
    let summary = summarize(&[
        record(
            vec![suspect("alpha", None)],
            Some(place("Game.exe", "0x1000")),
        ),
        record(
            vec![suspect("alpha", None)],
            Some(place("Game.exe", "0x1000")),
        ),
        record(
            vec![suspect("alpha", None)],
            Some(place("Game.exe", "0x1000")),
        ),
    ]);
    let alpha = &summary.suspects[0];
    assert_eq!(alpha.crashes_named, 3);
    assert_eq!(alpha.distinct_places, 1);
}

#[test]
fn one_owner_at_many_places_counts_each_place() {
    let summary = summarize(&[
        record(vec![suspect("alpha", None)], Some(place("One.dll", "0x10"))),
        record(vec![suspect("alpha", None)], Some(place("Two.dll", "0x20"))),
        record(
            vec![suspect("alpha", None)],
            Some(place("Three.dll", "0x30")),
        ),
        record(
            vec![suspect("alpha", None)],
            Some(place("Three.dll", "0x30")),
        ),
    ]);
    let alpha = &summary.suspects[0];
    assert_eq!(alpha.crashes_named, 4);
    assert_eq!(alpha.distinct_places, 3);
}

#[test]
fn a_crash_with_no_place_still_counts_as_named() {
    let summary = summarize(&[record(vec![suspect("alpha", None)], None)]);
    let alpha = &summary.suspects[0];
    assert_eq!(alpha.crashes_named, 1);
    assert_eq!(alpha.distinct_places, 0);
}

#[test]
fn the_top_suspect_is_counted_as_well_as_named() {
    let summary = summarize(&[
        record(
            vec![suspect("top", None), suspect("second", None)],
            Some(place("Game.exe", "0x1")),
        ),
        record(
            vec![suspect("second", None), suspect("top", None)],
            Some(place("Game.exe", "0x2")),
        ),
        record(
            vec![suspect("second", None)],
            Some(place("Game.exe", "0x3")),
        ),
    ]);
    let top = summary
        .suspects
        .iter()
        .find(|owner| owner.owner == item("top"))
        .unwrap();
    let second = summary
        .suspects
        .iter()
        .find(|owner| owner.owner == item("second"))
        .unwrap();
    assert_eq!((top.crashes_named, top.crashes_top), (2, 1));
    assert_eq!((second.crashes_named, second.crashes_top), (3, 2));
}

#[test]
fn framework_and_hook_framework_owners_are_listed_apart() {
    let summary = summarize(&[
        record(
            vec![
                suspect("alpha", None),
                suspect("engine-core", Some(Role::Framework)),
                suspect("hook-host", Some(Role::HookFramework)),
            ],
            Some(place("Game.exe", "0x1")),
        ),
        record(
            vec![
                suspect("engine-core", Some(Role::Framework)),
                suspect("hook-host", Some(Role::HookFramework)),
            ],
            Some(place("Game.exe", "0x2")),
        ),
    ]);
    assert_eq!(summary.suspects.len(), 1);
    assert_eq!(summary.suspects[0].owner, item("alpha"));
    let always: Vec<&str> = summary
        .always_running
        .iter()
        .map(|owner| match &owner.owner {
            Owner::Item { key, .. } => key.as_str(),
            _ => "?",
        })
        .collect();
    assert_eq!(always, ["engine-core", "hook-host"]);
    assert!(summary
        .always_running
        .iter()
        .all(|owner| matches!(owner.role, Some(Role::Framework | Role::HookFramework))));
}

#[test]
fn a_framework_owner_without_a_role_is_still_separated() {
    let summary = summarize(&[record(
        vec![HistorySuspect {
            owner: Owner::Framework {
                name: "Engine".to_string(),
            },
            role: None,
        }],
        None,
    )]);
    assert!(summary.suspects.is_empty());
    assert_eq!(summary.always_running.len(), 1);
}

#[test]
fn ranking_is_by_crashes_then_places_and_does_not_depend_on_input_order() {
    let records = vec![
        record(vec![suspect("one", None)], Some(place("A.dll", "0x1"))),
        record(vec![suspect("two", None)], Some(place("B.dll", "0x2"))),
        record(vec![suspect("two", None)], Some(place("C.dll", "0x3"))),
        record(vec![suspect("one", None)], Some(place("A.dll", "0x1"))),
        record(vec![suspect("one", None)], Some(place("A.dll", "0x1"))),
        record(vec![suspect("tie", None)], Some(place("D.dll", "0x4"))),
    ];
    let forward = summarize(&records);
    let mut reversed = records.clone();
    reversed.reverse();
    let backward = summarize(&reversed);
    assert_eq!(forward, backward);
    let order: Vec<&str> = forward
        .suspects
        .iter()
        .map(|owner| match &owner.owner {
            Owner::Item { key, .. } => key.as_str(),
            _ => "?",
        })
        .collect();
    assert_eq!(order, ["one", "two", "tie"]);
}

#[test]
fn the_contract_place_is_the_exception_module_and_offset() {
    let facts = vec![
        frame(0.0, "Other.dll", "0x999"),
        exception("Game.exe", "0x1000", 0.0),
    ];
    assert_eq!(
        crash_place(&facts),
        Some(place("Game.exe", "0x1000")),
        "the exception site outranks the top frame"
    );
}

#[test]
fn the_outermost_exception_wins() {
    let facts = vec![
        exception("Inner.dll", "0x2", 1.0),
        exception("Outer.dll", "0x1", 0.0),
    ];
    assert_eq!(crash_place(&facts), Some(place("Outer.dll", "0x1")));
}

#[test]
fn without_an_exception_offset_the_first_frame_is_the_place() {
    let facts = vec![
        frame(2.0, "Low.dll", "0x300"),
        exception_without_offset("Game.exe"),
        frame(0.0, "Top.dll", "0x100"),
        frame(1.0, "Next.dll", "0x200"),
    ];
    assert_eq!(
        crash_place(&facts),
        Some(place("Top.dll", "0x100")),
        "the lowest frame index is the top of the stack"
    );
}

#[test]
fn a_managed_frame_uses_its_class_and_symbol_when_there_is_no_native_module() {
    let facts = vec![managed_frame(
        0.0,
        "com.example.Example",
        "com.example.Example.run",
    )];
    assert_eq!(
        crash_place(&facts),
        Some(CrashPlace {
            kind: "class".to_string(),
            value: "com.example.Example".to_string(),
            location: Some("com.example.Example.run".to_string()),
        })
    );
}

#[test]
fn facts_without_any_place_are_none_not_a_made_up_one() {
    assert_eq!(crash_place(&[]), None);
    let no_place = Fact {
        kind: "core.log_line".to_string(),
        attrs: BTreeMap::new(),
        refs: Vec::new(),
        span: None,
    };
    assert_eq!(crash_place(&[no_place]), None);
}

fn text_attr(value: &str) -> AttrValue {
    AttrValue::Scalar(Scalar::Text(value.to_string()))
}

fn number_attr(value: f64) -> AttrValue {
    AttrValue::Scalar(Scalar::Number(value))
}

fn exception(module: &str, offset: &str, depth: f64) -> Fact {
    let mut attrs = BTreeMap::new();
    attrs.insert("class".to_string(), text_attr("EXCEPTION_ACCESS_VIOLATION"));
    attrs.insert("category".to_string(), text_attr("access_violation"));
    attrs.insert("depth".to_string(), number_attr(depth));
    attrs.insert("offset".to_string(), text_attr(offset));
    Fact {
        kind: "core.exception".to_string(),
        attrs,
        refs: vec![Ref {
            kind: "module".to_string(),
            value: module.to_string(),
            strength: Strength::ExecTop,
        }],
        span: None,
    }
}

fn exception_without_offset(module: &str) -> Fact {
    let mut fact = exception(module, "0x0", 0.0);
    fact.attrs.remove("offset");
    fact
}

fn frame(index: f64, module: &str, rva: &str) -> Fact {
    let mut attrs = BTreeMap::new();
    attrs.insert("index".to_string(), number_attr(index));
    attrs.insert("rva".to_string(), text_attr(rva));
    Fact {
        kind: "core.frame".to_string(),
        attrs,
        refs: vec![Ref {
            kind: "module".to_string(),
            value: module.to_string(),
            strength: Strength::Exec,
        }],
        span: None,
    }
}

fn managed_frame(index: f64, class: &str, symbol: &str) -> Fact {
    let mut attrs = BTreeMap::new();
    attrs.insert("index".to_string(), number_attr(index));
    attrs.insert("symbol".to_string(), text_attr(symbol));
    Fact {
        kind: "core.frame".to_string(),
        attrs,
        refs: vec![Ref {
            kind: "class".to_string(),
            value: class.to_string(),
            strength: Strength::Exec,
        }],
        span: None,
    }
}
