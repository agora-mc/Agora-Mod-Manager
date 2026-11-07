//! The contract proved on four games that have nothing in common, and the linter's refusals. All
//! example data is invented for these tests; the names and numbers mean nothing.

use agora_game_api::crash::*;
use std::collections::BTreeSet;

const GAMES: &[(&str, &str, &str)] = &[
    (
        "minecraft",
        include_str!("crash_examples/minecraft.pack.json"),
        include_str!("crash_examples/minecraft.output.json"),
    ),
    (
        "skyrim",
        include_str!("crash_examples/skyrim.pack.json"),
        include_str!("crash_examples/skyrim.output.json"),
    ),
    (
        "cyberpunk",
        include_str!("crash_examples/cyberpunk.pack.json"),
        include_str!("crash_examples/cyberpunk.output.json"),
    ),
    (
        "ironfen",
        include_str!("crash_examples/ironfen.pack.json"),
        include_str!("crash_examples/ironfen.output.json"),
    ),
];

fn pack(name: &str) -> CrashPack {
    let (_, json, _) = GAMES.iter().find(|(n, _, _)| *n == name).unwrap();
    lint_pack_json(json).unwrap()
}

#[test]
fn every_example_pack_lints_clean() {
    for (name, json, _) in GAMES {
        if let Err(problems) = lint_pack_json(json) {
            panic!("{name}: {problems:#?}");
        }
    }
}

#[test]
fn every_example_output_is_valid() {
    for (name, _, json) in GAMES {
        let output: ParserOutput =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));
        let problems = validate_parser_output(&output, &ParserLimits::DEFAULT);
        assert!(problems.is_empty(), "{name}: {problems:#?}");
    }
}

/// Stops the examples being fiction: each game's rules ask only for what its parser can say. Every
/// rule queries a kind and attribute some fact has, or the parser said it can't hold, and every
/// ref kind a rule wants has a way to be traced to its owner.
#[test]
fn every_example_rule_asks_for_what_its_parser_says() {
    for (name, pack_json, output_json) in GAMES {
        let pack = lint_pack_json(pack_json).unwrap();
        let output: ParserOutput = serde_json::from_str(output_json).unwrap();
        assert!(!pack.rules.is_empty(), "{name} has no rule");
        let mapped: BTreeSet<&str> = pack.ref_maps.iter().map(|m| m.kind.as_str()).collect();

        for rule in &pack.rules {
            for query in fact_queries(&rule.when) {
                let of_kind: Vec<&Fact> = output
                    .facts
                    .iter()
                    .filter(|f| f.kind == query.kind)
                    .collect();
                assert!(
                    !of_kind.is_empty(),
                    "{name}/{}: no fact of `{}`",
                    rule.id,
                    query.kind
                );
                for attr in query.filter.keys().filter(|a| *a != "actionable_index") {
                    assert!(
                        of_kind.iter().any(|f| f.attrs.contains_key(attr)),
                        "{name}/{}: no `{}` has `{attr}`",
                        rule.id,
                        query.kind
                    );
                }
                if let Some(reference) = &query.reference {
                    assert!(
                        of_kind
                            .iter()
                            .any(|f| f.refs.iter().any(|r| r.kind == reference.kind)),
                        "{name}/{}: no `{}` has a `{}` ref",
                        rule.id,
                        query.kind,
                        reference.kind
                    );
                    assert!(
                        mapped.contains(reference.kind.as_str()),
                        "{name}/{}: nothing traces `{}` refs to an owner",
                        rule.id,
                        reference.kind
                    );
                }
            }
        }
    }
}

fn fact_queries(condition: &Condition) -> Vec<&FactQuery> {
    match condition {
        Condition::All(list) | Condition::Any(list) | Condition::AtLeast { of: list, .. } => {
            list.iter().flat_map(fact_queries).collect()
        }
        Condition::Not(inner) => fact_queries(inner),
        Condition::Fact(query) => vec![query],
        Condition::Count(_) | Condition::Text(_) => vec![],
    }
}

/// The two shapes that once looked universal and aren't: a Java frame names a class where a native
/// one names a module, and a folder of files with no text log is still an artifact.
#[test]
fn nothing_assumes_a_native_stack_or_a_text_log() {
    let minecraft = pack("minecraft");
    assert!(minecraft.ref_maps.iter().any(|m| m.kind == "class"));
    assert!(minecraft.roles.iter().all(|r| r.kind != "module"));

    let cyberpunk = pack("cyberpunk");
    let report = &cyberpunk.sources[0];
    assert_eq!(report.unit, ArtifactUnit::Folder);
    assert!(report.children.iter().any(|c| c.copy == CopyPolicy::Never));
    assert!(matches!(
        cyberpunk.parsers[0].entry,
        CodeEntry::Generic { .. }
    ));
    assert!(matches!(cyberpunk.parsers[1].entry, CodeEntry::Wasm { .. }));

    let ironfen = pack("ironfen");
    assert_eq!(
        ironfen.parsers[0].detect.magic_hex.as_deref(),
        Some("49464644")
    );
}

/// The victim-frame case: a top frame in the game's own code must not accuse the game or the hook
/// framework beneath it, and only a failed inspection can name a root cause.
#[test]
fn a_victim_frame_accuses_nobody_until_an_inspector_finds_something() {
    let skyrim = pack("skyrim");
    let rule = &skyrim.rules[0];
    for effect in &rule.effects {
        if let Effect::Finding { scope, .. } = effect {
            assert_eq!(*scope, Scope::Mechanism);
        }
    }
    let verify = rule.verify.as_ref().unwrap();
    assert!(matches!(
        verify.on_fail[0],
        Effect::Finding {
            scope: Scope::RootCause,
            ..
        }
    ));
    assert!(verify.on_pass.is_empty());
}

// ---------------------------------------------------------------------------
// The linter
// ---------------------------------------------------------------------------

/// A rule that lints clean, as the base for each refusal below.
const GOOD: &str = r#"{
    "id": "good", "version": 1,
    "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
    "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 4 } }],
    "text": { "headline": { "en": "{owner:m} was running." } }
}"#;

fn with(path: &[&str], value: serde_json::Value) -> String {
    let mut rule: serde_json::Value = serde_json::from_str(GOOD).unwrap();
    let mut at = &mut rule;
    for key in &path[..path.len() - 1] {
        at = at.get_mut(key).unwrap();
    }
    at[path[path.len() - 1]] = value;
    rule.to_string()
}

fn codes(json: &str) -> Vec<LintCode> {
    lint_rule_json(json)
        .err()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.code)
        .collect()
}

fn refused_with(json: &str, code: LintCode) -> String {
    let problems = lint_rule_json(json).expect_err("the linter let this through");
    let found = problems.iter().find(|p| p.code == code);
    found
        .unwrap_or_else(|| panic!("no {code:?} in {problems:#?}"))
        .to_string()
}

#[test]
fn the_base_rule_is_clean() {
    assert_eq!(codes(GOOD), vec![]);
}

#[test]
fn unknown_fields_are_refused_by_name_everywhere() {
    use serde_json::json;
    let cases = [
        with(&["surprise"], json!(1)),
        with(&["text", "surprise"], json!(1)),
        with(
            &["when"],
            json!({ "fact": { "kind": "core.frame", "wher": {} } }),
        ),
        with(
            &["when"],
            json!({ "fact": { "kind": "core.frame", "where": { "index": { "lessthan": 3 } } } }),
        ),
        with(&["when"], json!({ "all_of": [] })),
        with(
            &["effects"],
            json!([{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 4, "cap": "high" } }]),
        ),
    ];
    for json in &cases {
        let message = refused_with(json, LintCode::Parse);
        assert!(message.contains("unknown"), "{message}");
    }
    assert!(refused_with(&cases[0], LintCode::Parse).contains("surprise"));
    assert!(refused_with(&cases[3], LintCode::Parse).contains("lessthan"));
}

/// Source attribution can't be claimed from inside a pack, and there's no way to say "reviewed".
#[test]
fn a_rule_cannot_name_its_own_source_or_claim_review() {
    use serde_json::json;
    for field in ["source", "plugin_id", "reviewed_on", "confidence"] {
        refused_with(&with(&[field], json!("official")), LintCode::Parse);
    }
}

#[test]
fn a_certain_level_does_not_exist() {
    use serde_json::json;
    let json = with(
        &["effects"],
        json!([{ "finding": { "scope": "mechanism", "level": "certain" } }]),
    );
    let message = refused_with(&json, LintCode::Parse);
    assert!(message.contains("very_likely"), "{message}");
}

#[test]
fn certainty_wording_is_refused_in_every_text_and_locale() {
    use serde_json::json;
    for text in [
        "{owner:m} is definitely the cause.",
        "DEFINITELY {owner:m}",
        "This is certainly {owner:m}.",
        "100% {owner:m}",
        "It's {owner:m} for sure.",
    ] {
        let json = with(&["text", "headline"], json!({ "en": text }));
        assert!(refused_with(&json, LintCode::Wording).contains("very likely"));
    }
    let german = with(
        &["text", "headline"],
        json!({ "en": "Possibly.", "de": "definitely" }),
    );
    refused_with(&german, LintCode::Wording);
    let in_context = with(
        &["effects"],
        json!([{ "context": { "text": { "en": "Without a doubt, this." } } }]),
    );
    refused_with(&in_context, LintCode::Wording);
    let in_why = with(
        &["text", "why_not_sure"],
        json!({ "en": "It's guaranteed." }),
    );
    refused_with(&in_why, LintCode::Wording);
}

#[test]
fn honest_doubt_is_not_mistaken_for_certainty() {
    for text in [
        "We can't be certain.",
        "Uncertain, and not sure.",
        "Very likely.",
        "An indefinite wait.",
    ] {
        assert_eq!(certainty_claim(text), None, "{text}");
    }
}

#[test]
fn an_unbounded_text_match_alone_is_refused() {
    use serde_json::json;
    let alone = with(&["when"], json!({ "text": { "regex": "something" } }));
    refused_with(&alone, LintCode::Unanchored);

    // Being one branch of `any` is still alone: that branch can hold by itself.
    let either = with(
        &["when"],
        json!({ "any": [{ "text": { "regex": "something" } }, { "fact": { "kind": "core.frame" } }] }),
    );
    refused_with(&either, LintCode::Unanchored);

    // The same query spelled as a fact is the same thing.
    let as_fact = with(
        &["when"],
        json!({ "fact": { "kind": "core.log_line", "where": { "text": { "regex": "something" } } } }),
    );
    refused_with(&as_fact, LintCode::Unanchored);

    // The weakest two could be the ones holding.
    let two_of_three = with(
        &["when"],
        json!({ "at_least": { "n": 2, "of": [
            { "text": { "regex": "a" } }, { "text": { "regex": "b" } }, { "fact": { "kind": "core.frame" } }
        ] } }),
    );
    refused_with(&two_of_three, LintCode::Unanchored);
}

fn signal_free(json: String) -> String {
    let mut rule: serde_json::Value = serde_json::from_str(&json).unwrap();
    rule["effects"] =
        serde_json::json!([{ "finding": { "scope": "mechanism", "level": "possible" } }]);
    rule["text"] = serde_json::json!({ "headline": { "en": "A thing happened." } });
    rule.to_string()
}

#[test]
fn a_bounded_or_accompanied_text_match_is_allowed() {
    use serde_json::json;
    for when in [
        json!({ "text": { "regex": "something", "section": "exception" } }),
        json!({ "text": { "regex": "something", "source": "game_log" } }),
        json!({ "all": [{ "text": { "regex": "something" } }, { "fact": { "kind": "core.frame" } }] }),
        json!({ "fact": { "kind": "core.log_line", "where": { "section": "x", "text": { "regex": "something" } } } }),
    ] {
        assert_eq!(codes(&signal_free(with(&["when"], when))), vec![]);
    }
}

#[test]
fn conditions_that_hold_on_an_empty_log_are_refused() {
    use serde_json::json;
    for when in [
        json!({ "not": { "fact": { "kind": "core.frame" } } }),
        json!({ "count": { "kind": "core.frame", "at_most": 3 } }),
        json!({ "all": [{ "not": { "fact": { "kind": "core.frame" } } }, { "not": { "fact": { "kind": "core.module" } } }] }),
    ] {
        refused_with(&signal_free(with(&["when"], when)), LintCode::Unanchored);
    }
    for when in [
        json!({ "all": [] }),
        json!({ "any": [] }),
        json!({ "at_least": { "n": 0, "of": [{ "fact": { "kind": "core.frame" } }] } }),
        json!({ "at_least": { "n": 2, "of": [{ "fact": { "kind": "core.frame" } }] } }),
        json!({ "count": { "kind": "core.frame" } }),
        json!({ "count": { "kind": "core.frame", "at_least": 0 } }),
        json!({ "fact": { "kind": "core.frame", "where": { "index": {} } } }),
        json!({ "fact": { "kind": "core.frame", "where": { "index": { "in": [] } } } }),
    ] {
        refused_with(
            &signal_free(with(&["when"], when)),
            LintCode::EmptyCondition,
        );
    }
}

#[test]
fn a_raw_address_needs_an_exact_runtime() {
    use serde_json::json;
    let by_rva = json!({ "fact": { "kind": "core.frame", "where": { "rva": "0x00A0B0C0" } } });
    let by_module_offset =
        json!({ "fact": { "kind": "core.frame", "where": { "symbol": "Game.exe+A0B0C0" } } });
    let by_fault = json!({ "fact": { "kind": "core.exception", "where": { "fault_address": { "lte": 4096 } } } });
    let by_custom =
        json!({ "fact": { "kind": "example.thing", "where": { "entry_address": "0x10" } } });
    let by_scheme =
        json!({ "fact": { "kind": "core.frame", "where": { "symbol_scheme": "rva" } } });
    let by_regex = json!({ "text": { "section": "x", "regex": "Game\\.exe\\+0A0B0C0" } });
    let by_hex_regex = json!({ "text": { "section": "x", "regex": "at 0x7ff6a000" } });
    let cases = [
        by_rva,
        by_module_offset,
        by_fault,
        by_custom,
        by_scheme,
        by_regex,
        by_hex_regex,
    ];
    for when in &cases {
        let json = signal_free(with(&["when"], when.clone()));
        let message = refused_with(&json, LintCode::RawAddress);
        assert!(message.contains("scope.runtime.exact"), "{message}");

        // A range isn't enough, because an address moves within one.
        let mut range: serde_json::Value = serde_json::from_str(&json).unwrap();
        range["scope"] = json!({ "runtime": { "min": "1.0", "below": "2.0" } });
        refused_with(&range.to_string(), LintCode::RawAddress);

        let mut exact: serde_json::Value = serde_json::from_str(&json).unwrap();
        exact["scope"] = json!({ "runtime": { "exact": ["1.2.3.4"] } });
        assert_eq!(codes(&exact.to_string()), vec![], "{when}");
    }

    // A stable symbol, whether or not the attribute looks hexy, is not a raw address.
    let stable = json!({ "fact": { "kind": "core.frame", "where": { "symbol_scheme": "addrlib-ae", "symbol": "10001", "symbol_offset": { "lte": 32 } } } });
    assert_eq!(codes(&signal_free(with(&["when"], stable))), vec![]);
    let present =
        json!({ "fact": { "kind": "core.frame", "where": { "rva": { "present": true } } } });
    assert_eq!(codes(&signal_free(with(&["when"], present))), vec![]);
}

#[test]
fn regexes_are_bounded() {
    use serde_json::json;
    let long = "a".repeat(MAX_RULE_REGEX_LEN + 1);
    let cases = [
        long.as_str(),
        "(unclosed",
        "",
        // Backreferences and look-around don't exist in the `regex` crate.
        r"(a)\1",
        r"(?=a)",
        // Compiles to more than the size limit.
        r"(\w{1000}){1000}",
    ];
    for regex in cases {
        let in_text = with(
            &["when"],
            json!({ "text": { "section": "x", "regex": regex } }),
        );
        refused_with(&signal_free(in_text), LintCode::BadRegex);
        let in_where = with(
            &["when"],
            json!({ "fact": { "kind": "example.thing", "where": { "x": { "regex": regex } } } }),
        );
        refused_with(&signal_free(in_where), LintCode::BadRegex);
    }
}

#[test]
fn a_suspect_must_be_a_ref_that_is_always_bound() {
    use serde_json::json;
    let cases = [
        // Never bound.
        GOOD.replace(r#""owner_of": "m""#, r#""owner_of": "nope""#),
        // A plain attribute has no owner to look up.
        with(
            &["when"],
            json!({ "fact": { "kind": "core.frame", "bind": { "m": "symbol" } } }),
        ),
        // Bound in only one branch.
        with(
            &["when"],
            json!({ "any": [
                { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
                { "fact": { "kind": "core.module" } }
            ] }),
        ),
        // Bound under `not`, so it can't get out.
        with(
            &["when"],
            json!({ "all": [
                { "fact": { "kind": "core.frame" } },
                { "not": { "fact": { "kind": "core.module", "ref": { "kind": "module", "bind": "m" } } } }
            ] }),
        ),
    ];
    for json in &cases {
        refused_with(json, LintCode::BadVariable);
    }

    // A capture named in `refs` is a ref; one that isn't, isn't.
    let capture =
        json!({ "text": { "section": "x", "regex": "(?P<m>\\w+)", "refs": { "m": "module" } } });
    assert_eq!(codes(&with(&["when"], capture)), vec![]);
    let plain = json!({ "text": { "section": "x", "regex": "(?P<m>\\w+)" } });
    refused_with(&with(&["when"], plain), LintCode::BadVariable);

    // Text and joins can't name what isn't bound.
    let bad_text = with(&["text", "headline"], json!({ "en": "{other} did it" }));
    refused_with(&bad_text, LintCode::BadVariable);
    let join_too_early = with(
        &["when"],
        json!({ "all": [
            { "fact": { "kind": "core.frame", "where": { "symbol": "$later" }, "ref": { "kind": "module", "bind": "m" } } },
            { "fact": { "kind": "core.frame", "bind": { "later": "symbol" } } }
        ] }),
    );
    refused_with(&join_too_early, LintCode::BadVariable);
}

#[test]
fn effects_must_be_honest_about_what_they_claim() {
    use serde_json::json;
    let cases = [
        json!([]),
        // A mechanism never fills the "who did it" slot.
        json!([{ "finding": { "scope": "mechanism", "level": "likely", "candidate": { "owner_of": "m" }, "family": "execution", "weight": 4 } }]),
        // A root cause has to name someone.
        json!([{ "finding": { "scope": "root_cause", "level": "likely" } }]),
        // Memory alone never accuses a mod.
        json!([{ "finding": { "scope": "root_cause", "level": "likely", "candidate": { "owner_of": "m" }, "family": "local_confirmation", "weight": 4 } }]),
        json!([{ "finding": { "scope": "root_cause", "level": "likely", "candidate": { "owner_of": "m" } } }]),
        json!([{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 0 } }]),
        json!([{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 1000 } }]),
    ];
    for effects in cases {
        refused_with(&with(&["effects"], effects), LintCode::BadEffect);
    }
}

#[test]
fn core_names_are_checked_and_plugin_names_are_not() {
    use serde_json::json;
    for when in [
        json!({ "fact": { "kind": "core.framez" } }),
        json!({ "fact": { "kind": "core.frame", "where": { "indx": 0 } } }),
        json!({ "fact": { "kind": "core.frame", "bind": { "x": "indx" } } }),
    ] {
        refused_with(
            &signal_free(with(&["when"], when)),
            LintCode::UnknownCoreName,
        );
    }
    refused_with(
        &signal_free(with(
            &["when"],
            json!({ "fact": { "kind": "Nonamespace" } }),
        )),
        LintCode::BadKind,
    );
    let plugin_kind = json!({ "fact": { "kind": "example.thing", "where": { "anything": 1 } } });
    assert_eq!(codes(&signal_free(with(&["when"], plugin_kind))), vec![]);
    // Engine facts and attributes are for rules to read.
    let engine = json!({ "all": [
        { "fact": { "kind": "core.frame", "where": { "actionable_index": 0 } } },
        { "fact": { "kind": "core.history", "where": { "same_bucket_count": { "gte": 2 } } } },
        { "fact": { "kind": "core.change", "where": { "change": "updated" }, "ref": { "kind": "content" } } }
    ] });
    assert_eq!(codes(&signal_free(with(&["when"], engine))), vec![]);
}

#[test]
fn version_gates_are_checked() {
    use serde_json::json;
    for gate in [
        json!({}),
        json!({ "exact": ["1.0"], "min": "1.0" }),
        json!({ "min": "24w14a" }),
        json!({ "min": "2.0", "below": "2.0" }),
        json!({ "min": "3.0", "below": "2.0" }),
    ] {
        let json = with(&["scope"], json!({ "runtime": gate }));
        refused_with(&json, LintCode::BadVersionGate);
    }
}

#[test]
fn a_version_gate_says_when_it_cannot_judge() {
    let range = VersionGate {
        min: Some("1.6".into()),
        below: Some("1.6.1000".into()),
        ..Default::default()
    };
    assert_eq!(range.matches("1.6"), Some(true));
    assert_eq!(range.matches("1.6.0.0"), Some(true));
    assert_eq!(range.matches("1.6.999.9"), Some(true));
    assert_eq!(range.matches("1.6.1000"), Some(false));
    assert_eq!(range.matches("1.5.99"), Some(false));
    // A snapshot name can't be placed in a range: could not check, not "no".
    assert_eq!(range.matches("24w14a"), None);
    assert_eq!(range.matches(""), None);

    let exact = VersionGate {
        exact: vec!["24w14a".into()],
        ..Default::default()
    };
    assert_eq!(exact.matches("24w14a"), Some(true));
    assert_eq!(exact.matches("24w15a"), Some(false));
}

#[test]
fn unknown_stays_unknown_through_logic() {
    use Truth::*;
    assert_eq!(Unknown.and(True), Unknown);
    assert_eq!(Unknown.and(False), False);
    assert_eq!(Unknown.or(True), True);
    assert_eq!(Unknown.or(False), Unknown);
    assert_eq!(Unknown.negate(), Unknown);
    assert_eq!(True.negate(), False);
}

// ---------------------------------------------------------------------------
// Packs
// ---------------------------------------------------------------------------

const EMPTY_PACK: &str = r#"{ "contract": 1 }"#;

#[test]
fn an_empty_pack_is_legal_and_another_version_is_not() {
    assert!(lint_pack_json(EMPTY_PACK).is_ok());
    let problems = lint_pack_json(r#"{ "contract": 2 }"#).unwrap_err();
    assert_eq!(problems[0].code, LintCode::ContractVersion);
    assert_eq!(
        lint_pack_json(r#"{ }"#).unwrap_err()[0].code,
        LintCode::Parse
    );
    assert_eq!(lint_pack_json("").unwrap_err()[0].code, LintCode::Parse);
}

#[test]
fn pack_level_mistakes_are_refused() {
    let primary_unbound = r#"{ "contract": 1, "sources": [{ "id": "a",
        "where": { "root": "instance", "path": "logs" }, "match": { "glob": "*.log" },
        "role": "primary" }] }"#;
    assert_eq!(
        lint_pack_json(primary_unbound).unwrap_err()[0].code,
        LintCode::BadSource
    );

    let no_capture = r#"{ "contract": 1, "sources": [{ "id": "a",
        "where": { "root": "instance", "path": "logs" }, "match": { "glob": "*.log" },
        "bind": [{ "pid_in_name": { "regex": "\\d+" } }] }] }"#;
    assert_eq!(
        lint_pack_json(no_capture).unwrap_err()[0].code,
        LintCode::BadSource
    );

    let bad_detect = r#"{ "contract": 1, "parsers": [{ "id": "a",
        "entry": { "script": { "export": "f" } }, "detect": { "any": ["(unclosed"] } }] }"#;
    assert_eq!(
        lint_pack_json(bad_detect).unwrap_err()[0].code,
        LintCode::BadRegex
    );

    let bad_magic = r#"{ "contract": 1, "parsers": [{ "id": "a",
        "entry": { "script": { "export": "f" } }, "detect": { "magic_hex": "xyz" } }] }"#;
    assert_eq!(
        lint_pack_json(bad_magic).unwrap_err()[0].code,
        LintCode::BadParser
    );

    let duplicate = format!(r#"{{ "contract": 1, "rules": [{GOOD}, {GOOD}] }}"#);
    assert_eq!(
        lint_pack_json(&duplicate).unwrap_err()[0].code,
        LintCode::DuplicateId
    );

    let unknown_parser_entry =
        r#"{ "contract": 1, "parsers": [{ "id": "a", "entry": { "dll": { "path": "x.dll" } } }] }"#;
    let problems = lint_pack_json(unknown_parser_entry).unwrap_err();
    assert_eq!(problems[0].code, LintCode::Parse);
    assert!(problems[0].message.contains("dll"), "{}", problems[0]);
}

#[test]
fn a_ref_map_or_role_cannot_match_everything() {
    for pack in [
        r#"{ "contract": 1, "ref_maps": [{ "kind": "file", "lookup": { "path": { "patterns": ["Data/x"] } }, "match": "exact" }] }"#,
        r#"{ "contract": 1, "ref_maps": [{ "kind": "file", "lookup": { "path": { "patterns": [] } }, "match": "exact" }] }"#,
        r#"{ "contract": 1, "ref_maps": [{ "kind": "file", "lookup": { "index": "" }, "match": "exact" }] }"#,
        r#"{ "contract": 1, "roles": [{ "kind": "class", "value": "", "match": "prefix_dot", "role": "system" }] }"#,
    ] {
        assert_eq!(
            lint_pack_json(pack).unwrap_err()[0].code,
            LintCode::BadRefMap,
            "{pack}"
        );
    }
}

#[test]
fn a_path_pattern_wildcard_is_one_whole_segment() {
    let map = |pattern: &str| {
        format!(
            r#"{{ "contract": 1, "ref_maps": [{{ "kind": "module", "lookup": {{ "path": {{ "patterns": ["{pattern}"] }} }}, "match": "casefold" }}] }}"#
        )
    };
    assert!(
        lint_pack_json(&map("red4ext/plugins/*/{value}")).is_ok(),
        "one whole segment is supported"
    );
    for pattern in [
        "red4ext/plugins/foo*/{value}",
        "red4ext/plugins/**/{value}",
        "red4ext/plugins/{value}/*",
    ] {
        let problems = lint_pack_json(&map(pattern)).expect_err(pattern);
        assert_eq!(problems[0].code, LintCode::BadRefMap, "{pattern}");
        assert!(
            problems[0].message.contains("`*`"),
            "{}: {}",
            pattern,
            problems[0]
        );
    }
}

#[test]
fn a_superseded_rule_must_exist() {
    let mut rule: serde_json::Value = serde_json::from_str(GOOD).unwrap();
    rule["supersedes"] = serde_json::json!(["ghost"]);
    let pack = format!(r#"{{ "contract": 1, "rules": [{rule}] }}"#);
    assert_eq!(lint_pack_json(&pack).unwrap_err()[0].code, LintCode::BadId);
}

// ---------------------------------------------------------------------------
// The script boundary
// ---------------------------------------------------------------------------

fn output_with(facts: serde_json::Value) -> ParserOutput {
    serde_json::from_value(serde_json::json!({
        "contract": 1, "producer": { "format": "x" }, "facts": facts
    }))
    .unwrap()
}

fn output_problems(output: &ParserOutput) -> Vec<String> {
    validate_parser_output(output, &ParserLimits::DEFAULT)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn nothing_found_is_a_legal_output() {
    let empty: ParserOutput =
        serde_json::from_str(r#"{ "contract": 1, "producer": { "format": "x" } }"#).unwrap();
    assert_eq!(output_problems(&empty), Vec::<String>::new());
}

#[test]
fn a_parser_cannot_write_the_engines_facts_or_invent_core_ones() {
    use serde_json::json;
    for facts in [
        json!([{ "kind": "core.session", "attrs": { "verdict": "normal" } }]),
        json!([{ "kind": "core.attribution", "attrs": { "owner": "innocent" } }]),
        json!([{ "kind": "core.history" }]),
        json!([{ "kind": "core.made_up" }]),
    ] {
        assert_eq!(output_problems(&output_with(facts)).len(), 1);
    }
    // Nor the attributes the engine adds.
    let actionable =
        json!([{ "kind": "core.frame", "attrs": { "index": 0, "actionable_index": 0 } }]);
    assert!(output_problems(&output_with(actionable))[0].contains("actionable_index"));
}

#[test]
fn shape_rules_for_core_facts_are_enforced() {
    use serde_json::json;
    for facts in [
        json!([{ "kind": "core.frame", "attrs": { "kind": "probable" } }]),
        json!([{ "kind": "core.frame", "attrs": { "index": "0" } }]),
        json!([{ "kind": "core.exception", "attrs": { "category": "mysterious" } }]),
        json!([{ "kind": "core.exception" }]),
        json!([{ "kind": "Bad Kind" }]),
        json!([{ "kind": "x.y", "refs": [{ "kind": "", "value": "v" }] }]),
        json!([{ "kind": "x.y", "refs": [{ "kind": "k", "value": "" }] }]),
    ] {
        assert!(
            !output_problems(&output_with(facts.clone())).is_empty(),
            "{facts}"
        );
    }
}

#[test]
fn limits_are_enforced() {
    let limits = ParserLimits {
        max_facts: 2,
        max_refs_per_fact: 1,
        max_attr_len: 4,
        max_list_len: 2,
        max_bucket_parts: 1,
        max_notes: 1,
        ..ParserLimits::DEFAULT
    };
    let run = |json: serde_json::Value| -> usize {
        let output: ParserOutput = serde_json::from_value(json).unwrap();
        validate_parser_output(&output, &limits).len()
    };
    use serde_json::json;
    let base = |extra: serde_json::Value| {
        let mut output = json!({ "contract": 1, "producer": { "format": "x" } });
        for (k, v) in extra.as_object().unwrap() {
            output[k] = v.clone();
        }
        output
    };
    let fact = json!({ "kind": "x.y" });
    assert_eq!(run(base(json!({ "facts": [fact, fact] }))), 0);
    assert_eq!(run(base(json!({ "facts": [fact, fact, fact] }))), 1);
    assert_eq!(
        run(base(
            json!({ "facts": [{ "kind": "x.y", "attrs": { "a": "12345" } }] })
        )),
        1
    );
    assert_eq!(
        run(base(
            json!({ "facts": [{ "kind": "x.y", "attrs": { "a": ["1", "2", "3"] } }] })
        )),
        1
    );
    assert_eq!(
        run(base(json!({ "facts": [{ "kind": "x.y", "refs": [
            { "kind": "k", "value": "v" }, { "kind": "k", "value": "v" }
        ] }] }))),
        1
    );
    assert_eq!(
        run(base(
            json!({ "bucket": [{ "scheme": "s", "value": "v" }, { "scheme": "s", "value": "v" }] })
        )),
        1
    );
    assert_eq!(
        run(base(
            json!({ "notes": [{ "kind": "truncated" }, { "kind": "other" }] })
        )),
        1
    );
}

#[test]
fn twenty_thousand_bad_facts_are_one_short_report() {
    let facts: Vec<_> = (0..20_000)
        .map(|_| serde_json::json!({ "kind": "core.history" }))
        .collect();
    let output = output_with(serde_json::Value::Array(facts));
    assert!(output_problems(&output).len() <= 10);
}

#[test]
fn coverage_cannot_contradict_the_facts() {
    use serde_json::json;
    let check = |coverage: serde_json::Value, facts: serde_json::Value| -> usize {
        let output: ParserOutput = serde_json::from_value(json!({
            "contract": 1, "producer": { "format": "x" }, "coverage": coverage, "facts": facts
        }))
        .unwrap();
        validate_parser_output(&output, &ParserLimits::DEFAULT).len()
    };
    let fact = json!([{ "kind": "x.y" }]);
    assert_eq!(
        check(json!([{ "kind": "x.y", "state": "checked" }]), fact.clone()),
        0
    );
    assert_eq!(
        check(
            json!([{ "kind": "x.y", "state": "absent", "why": "w" }]),
            fact.clone()
        ),
        1
    );
    assert_eq!(
        check(json!([{ "kind": "x.y", "state": "not_applicable" }]), fact),
        1
    );
    assert_eq!(
        check(json!([{ "kind": "x.y", "state": "absent" }]), json!([])),
        1
    );
    assert_eq!(
        check(
            json!([{ "kind": "x.y", "state": "absent", "why": "" }]),
            json!([])
        ),
        1
    );
}

#[test]
fn a_big_number_as_text_is_how_an_address_travels() {
    let fact: Fact = serde_json::from_value(serde_json::json!({
        "kind": "core.frame",
        "attrs": { "index": 0, "address": "0x7ff6a0001000", "line": 12.0 }
    }))
    .unwrap();
    assert_eq!(
        fact.attrs["address"],
        AttrValue::Scalar(Scalar::Text("0x7ff6a0001000".into()))
    );
    assert_eq!(fact.attrs["index"], AttrValue::Scalar(Scalar::Number(0.0)));
    // Nothing nests, and null isn't a value.
    for attrs in [
        r#"{ "a": { "b": 1 } }"#,
        r#"{ "a": null }"#,
        r#"{ "a": [[1]] }"#,
    ] {
        let json = format!(r#"{{ "kind": "x.y", "attrs": {attrs} }}"#);
        assert!(serde_json::from_str::<Fact>(&json).is_err(), "{json}");
    }
}

/// A QuickJS parser and a WebAssembly parser have to see the same JSON.
#[test]
fn the_request_has_a_stable_hand_readable_shape() {
    let request = ParserRequest {
        contract: CRASH_CONTRACT_VERSION,
        parser: "example".into(),
        artifact: ArtifactHandle {
            id: "a1".into(),
            name: "crash.log".into(),
            size: 10,
            modified_at: None,
            source: Some("logs".into()),
            children: vec![],
        },
        game: GameHandle {
            id: "example".into(),
            runtime_version: None,
        },
        limits: ParserLimits::DEFAULT,
    };
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["artifact"]["id"], "a1");
    assert_eq!(json["limits"]["max_read_bytes"], 4 * 1024 * 1024);
    assert!(json["artifact"].get("path").is_none());
    assert_eq!(
        serde_json::from_value::<ParserRequest>(json).unwrap(),
        request
    );

    let read: HostRead =
        serde_json::from_str(r#"{ "read_bytes": { "artifact": "a1", "offset": 0, "len": 64 } }"#)
            .unwrap();
    assert!(matches!(read, HostRead::ReadBytes { len: 64, .. }));
}

#[test]
fn a_plugin_can_only_ask_for_lower_limits() {
    let asked = ParserLimits {
        max_ms: 60_000,
        max_facts: 5,
        ..ParserLimits::DEFAULT
    };
    let granted = ParserLimits::DEFAULT.lower_of(asked);
    assert_eq!(granted.max_ms, ParserLimits::DEFAULT.max_ms);
    assert_eq!(granted.max_facts, 5);
}

#[test]
fn levels_read_right_and_order_by_strength() {
    assert!(Level::NotSure < Level::Possible);
    assert!(Level::Possible < Level::Likely);
    assert!(Level::Likely < Level::VeryLikely);
    assert_eq!(Level::VeryLikely.label(), "very likely");
    assert_eq!(certainty_claim(Level::VeryLikely.label()), None);
    assert_eq!(certainty_claim(Level::NotSure.label()), None);
}

#[test]
fn verdict_json_is_what_the_ledger_has_stored() {
    let crashed = SessionVerdict::Crashed {
        how: MatchedBy::Pid,
    };
    assert_eq!(
        serde_json::to_string(&crashed).unwrap(),
        r#"{"kind":"crashed","how":"pid"}"#
    );
    assert_eq!(
        serde_json::to_string(&ExitObservation::unavailable("no job")).unwrap(),
        r#"{"kind":"unavailable","reason":"no job"}"#
    );
}
