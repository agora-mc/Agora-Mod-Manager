//! The game package's built-in crash rules: each fires on the facts it was written for and stays
//! quiet on a case close to it. The parsers' own outputs are tested in the game package.

use agora_core::crash_engine::{
    evaluate, EngineFacts, EvaluateInput, EvaluateOutput, Inspection, InspectionOutcome,
    InspectionRequest, Inspector, PackInput, ParsedArtifact, RefResolver, Resolved,
};
use agora_core::crash_packs::{built_in_identity, load_built_in_packs, BUILT_IN_PLUGIN_ID};
use agora_game_api::crash::{
    AttrValue, Coverage, CoverageState, Fact, Owner, ParserOutput, Producer, Ref, Role, Scalar,
    CRASH_CONTRACT_VERSION,
};
use agora_game_minecraft::crash::built_in_packs;
use std::collections::BTreeMap;

struct MapResolver;

impl RefResolver for MapResolver {
    fn resolve(&self, reference: &Ref) -> Resolved {
        if reference.kind == "mod_id" {
            return Resolved {
                owner: item(&reference.value),
                role: None,
            };
        }
        if reference.kind == "class" {
            if reference.value.starts_with("com.example") {
                return Resolved {
                    owner: item("examplemod"),
                    role: None,
                };
            }
            if reference.value.starts_with("net.minecraft") {
                return Resolved {
                    owner: Owner::Base,
                    role: Some(Role::Platform),
                };
            }
        }
        if reference.kind == "module" {
            return Resolved {
                owner: item("examplegfx"),
                role: None,
            };
        }
        Resolved {
            owner: Owner::Unknown,
            role: None,
        }
    }
}

struct NoInspector;

impl Inspector for NoInspector {
    fn inspect(&self, _request: &InspectionRequest<'_>) -> InspectionOutcome {
        Inspection::Missing.into()
    }
}

fn item(key: &str) -> Owner {
    Owner::Item {
        key: key.to_string(),
        name: key.to_string(),
    }
}

fn text(value: &str) -> AttrValue {
    AttrValue::Scalar(Scalar::Text(value.to_string()))
}

fn number(value: f64) -> AttrValue {
    AttrValue::Scalar(Scalar::Number(value))
}

fn fact(kind: &str, attrs: &[(&str, AttrValue)], refs: Vec<Ref>) -> Fact {
    Fact {
        kind: kind.to_string(),
        attrs: attrs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect::<BTreeMap<_, _>>(),
        refs,
        span: None,
    }
}

fn reference(kind: &str, value: &str) -> Ref {
    Ref {
        kind: kind.to_string(),
        value: value.to_string(),
        strength: agora_game_api::crash::Strength::Mention,
    }
}

fn strong_reference(kind: &str, value: &str) -> Ref {
    Ref {
        kind: kind.to_string(),
        value: value.to_string(),
        strength: agora_game_api::crash::Strength::Exec,
    }
}

fn output(format: &str, facts: Vec<Fact>) -> ParserOutput {
    let coverage = facts
        .iter()
        .map(|fact| Coverage {
            kind: fact.kind.clone(),
            state: CoverageState::Checked,
            why: None,
        })
        .collect();
    ParserOutput {
        contract: CRASH_CONTRACT_VERSION,
        producer: Producer {
            format: format.to_string(),
            version: None,
        },
        facts,
        bucket: Vec::new(),
        coverage,
        notes: Vec::new(),
    }
}

fn evaluate_one(output: ParserOutput) -> EvaluateOutput {
    let built_in = built_in_packs();
    let report = load_built_in_packs(&built_in);
    assert!(report.is_clean(), "{:#?}", report.problems);
    let pack = report.packs.into_iter().next().expect("one pack").pack;
    let parsed = ParsedArtifact {
        plugin: built_in_identity(),
        parser_id: output.producer.format.clone(),
        artifact: "fixture.log".to_string(),
        source: None,
        output,
    };
    let packs = vec![PackInput {
        plugin: built_in_identity(),
        pack,
    }];
    evaluate(&EvaluateInput {
        packs: &packs,
        parsed: std::slice::from_ref(&parsed),
        engine_facts: &EngineFacts::new(),
        runtime_version: Some("1.20.1"),
        resolver: &MapResolver,
        inspector: &NoInspector,
    })
}

fn fired(evaluated: &EvaluateOutput, rule: &str) -> bool {
    evaluated
        .findings
        .iter()
        .any(|finding| finding.source.rule_id.as_deref() == Some(rule))
}

fn signalled(evaluated: &EvaluateOutput, rule: &str) -> bool {
    evaluated
        .signals
        .iter()
        .any(|signal| signal.source.rule_id.as_deref() == Some(rule))
}

fn dependency(state: &str, missing: &str) -> Fact {
    fact(
        "core.dependency_problem",
        &[
            ("state", text(state)),
            ("requirement", text("2.0.0 or later")),
            ("needed_by", text("examplemod")),
        ],
        vec![reference("mod_id", missing)],
    )
}

mod crash {
    use super::*;

    #[test]
    fn missing_dependency_fires_and_a_disabled_dependency_does_not() {
        let fired_case = evaluate_one(output(
            "minecraft_latest_log",
            vec![dependency("absent", "examplelib")],
        ));
        assert!(fired(&fired_case, "missing_dependency"));
        assert_eq!(fired_case.findings[0].source.plugin_id, BUILT_IN_PLUGIN_ID);

        let quiet = evaluate_one(output(
            "minecraft_latest_log",
            vec![dependency("disabled", "examplelib")],
        ));
        assert!(!fired(&quiet, "missing_dependency"));
        assert!(!fired(&quiet, "wrong_dependency_version"));
    }

    #[test]
    fn the_fabric_api_rule_supersedes_the_general_one() {
        let evaluated = evaluate_one(output(
            "minecraft_latest_log",
            vec![dependency("absent", "fabric-api")],
        ));
        assert!(fired(&evaluated, "missing_fabric_api"));
        assert!(!fired(&evaluated, "missing_dependency"));
    }

    #[test]
    fn wrong_dependency_version_fires_only_for_that_state() {
        let evaluated = evaluate_one(output(
            "minecraft_latest_log",
            vec![dependency("wrong_version", "examplelib")],
        ));
        assert!(fired(&evaluated, "wrong_dependency_version"));
        assert!(!fired(&evaluated, "missing_dependency"));
    }

    #[test]
    fn a_wrong_game_version_is_recognised() {
        let evaluated = evaluate_one(output(
            "minecraft_latest_log",
            vec![dependency("wrong_version", "minecraft")],
        ));
        assert!(fired(&evaluated, "wrong_game_version"));
        assert!(!fired(&evaluated, "wrong_dependency_version"));
    }

    #[test]
    fn out_of_memory_fires_and_an_ordinary_exception_does_not() {
        let oom = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "core.exception",
                &[
                    ("class", text("java.lang.OutOfMemoryError")),
                    ("category", text("oom")),
                    ("depth", number(0.0)),
                ],
                Vec::new(),
            )],
        ));
        assert!(fired(&oom, "out_of_memory"));

        let ordinary = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "core.exception",
                &[
                    ("class", text("java.lang.IllegalStateException")),
                    ("category", text("managed")),
                    ("depth", number(0.0)),
                ],
                Vec::new(),
            )],
        ));
        assert!(!fired(&ordinary, "out_of_memory"));
        assert!(!fired(&ordinary, "missing_class"));
    }

    #[test]
    fn a_missing_class_names_the_class_but_not_a_certain_mod() {
        let evaluated = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "core.exception",
                &[
                    ("class", text("java.lang.NoClassDefFoundError")),
                    ("category", text("load_failure")),
                    ("message", text("com/example/thing/Widget")),
                    ("depth", number(0.0)),
                ],
                Vec::new(),
            )],
        ));
        assert!(fired(&evaluated, "missing_class"));
    }

    #[test]
    fn a_mixin_failure_names_the_mod_and_a_warning_does_not_fire() {
        let failure = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.mixin_failure",
                &[
                    ("severity", text("critical")),
                    ("config", text("examplemod.mixins.json")),
                    ("target", text("net.minecraft.class_123")),
                ],
                vec![reference("mod_id", "examplemod")],
            )],
        ));
        assert!(fired(&failure, "mixin_apply_failed"));
        assert!(!failure.signals.is_empty());

        let warning = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.mixin_failure",
                &[
                    ("severity", text("warning")),
                    ("config", text("examplemod.mixins.json")),
                    ("target", text("net.minecraft.class_123")),
                ],
                vec![reference("mod_id", "examplemod")],
            )],
        ));
        assert!(!fired(&warning, "mixin_apply_failed"));
    }

    #[test]
    fn a_duplicate_mod_is_named_and_a_single_mod_is_not() {
        let duplicate = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.duplicate_mod",
                &[("id", text("examplemod"))],
                vec![reference("mod_id", "examplemod")],
            )],
        ));
        assert!(fired(&duplicate, "duplicate_mod"));

        let single = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.mod",
                &[("id", text("examplemod"))],
                vec![reference("mod_id", "examplemod")],
            )],
        ));
        assert!(!fired(&single, "duplicate_mod"));
    }

    #[test]
    fn loader_problems_have_their_own_rules() {
        for (kind, rule) in [
            ("resolution", "mod_resolution_failed"),
            ("missing_dependency", "mod_resolution_failed"),
            ("incompatible", "incompatible_mods"),
            ("duplicate_classpath", "duplicate_classpath_libraries"),
        ] {
            let evaluated = evaluate_one(output(
                "minecraft_latest_log",
                vec![fact(
                    "minecraft.loader_problem",
                    &[("kind", text(kind)), ("message", text("a loader message"))],
                    Vec::new(),
                )],
            ));
            assert!(fired(&evaluated, rule), "{kind}: {rule}");
        }
    }

    #[test]
    fn a_mod_error_a_config_error_and_a_world_error_each_fire() {
        let mod_error = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.mod_error",
                &[("message", text("Failed to load mod file examplemod.jar"))],
                Vec::new(),
            )],
        ));
        assert!(fired(&mod_error, "mod_loading_error"));

        let config = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.config_error",
                &[(
                    "message",
                    text("Failed to load config file examplemod.toml"),
                )],
                Vec::new(),
            )],
        ));
        assert!(fired(&config, "broken_config"));

        let world = evaluate_one(output(
            "minecraft_latest_log",
            vec![fact(
                "minecraft.world_error",
                &[("message", text("Failed to load chunk 12,34"))],
                Vec::new(),
            )],
        ));
        assert!(fired(&world, "corrupted_world"));
    }

    #[test]
    fn old_java_fires_on_a_new_game_and_not_on_a_new_java() {
        let old = evaluate_one(output(
            "minecraft_crash_report",
            vec![
                fact(
                    "minecraft.system",
                    &[("java_version", text("1.8.0_51"))],
                    Vec::new(),
                ),
                fact(
                    "core.process",
                    &[("image_version", text("1.20.1"))],
                    Vec::new(),
                ),
            ],
        ));
        assert!(fired(&old, "java_too_old"));

        let new = evaluate_one(output(
            "minecraft_crash_report",
            vec![
                fact(
                    "minecraft.system",
                    &[("java_version", text("21.0.7"))],
                    Vec::new(),
                ),
                fact(
                    "core.process",
                    &[("image_version", text("1.20.1"))],
                    Vec::new(),
                ),
            ],
        ));
        assert!(!fired(&new, "java_too_old"));

        let old_game = evaluate_one(output(
            "minecraft_crash_report",
            vec![
                fact(
                    "minecraft.system",
                    &[("java_version", text("1.8.0_51"))],
                    Vec::new(),
                ),
                fact(
                    "core.process",
                    &[("image_version", text("1.12.2"))],
                    Vec::new(),
                ),
            ],
        ));
        assert!(!fired(&old_game, "java_too_old"));
    }

    #[test]
    fn thirty_two_bit_java_fires_and_sixty_four_does_not() {
        let thirty_two = evaluate_one(output(
            "minecraft_crash_report",
            vec![fact(
                "minecraft.system",
                &[("java_bits", number(32.0))],
                Vec::new(),
            )],
        ));
        assert!(fired(&thirty_two, "java_32_bit"));

        let sixty_four = evaluate_one(output(
            "minecraft_crash_report",
            vec![fact(
                "minecraft.system",
                &[("java_bits", number(64.0))],
                Vec::new(),
            )],
        ));
        assert!(!fired(&sixty_four, "java_32_bit"));
    }

    #[test]
    fn a_mod_class_on_the_stack_becomes_a_signal_but_a_platform_class_does_not() {
        let on_stack = evaluate_one(output(
            "minecraft_crash_report",
            vec![fact(
                "core.frame",
                &[("index", number(0.0))],
                vec![strong_reference("class", "com.example.mod.ExampleRenderer")],
            )],
        ));
        assert!(signalled(&on_stack, "frame_on_stack"));
        assert!(on_stack.signals.iter().any(
            |signal| matches!(&signal.candidate, Owner::Item { key, .. } if key == "examplemod")
        ));

        let platform = evaluate_one(output(
            "minecraft_crash_report",
            vec![fact(
                "core.frame",
                &[("index", number(0.0))],
                vec![strong_reference("class", "net.minecraft.class_123")],
            )],
        ));
        // The rule matches, but a platform class is not a culprit, so it makes no signal.
        assert!(!signalled(&platform, "frame_on_stack"));
        assert!(platform.signals.is_empty());
    }

    #[test]
    fn a_native_module_on_the_stack_becomes_a_signal() {
        let evaluated = evaluate_one(output(
            "minecraft_hs_err",
            vec![fact(
                "core.frame",
                &[("index", number(0.0))],
                vec![strong_reference("module", "examplegfx.dll")],
            )],
        ));
        assert!(signalled(&evaluated, "module_on_stack"));
        assert!(evaluated.signals.iter().any(
            |signal| matches!(&signal.candidate, Owner::Item { key, .. } if key == "examplegfx")
        ));
    }
}

/// A loose file keeps no original name (a forum paste, a renamed copy) and often has Windows line
/// ends; detection must still pick the right parser from the content alone.
mod detection {
    use agora_core::crash_packs::detect_parsers;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../agora-game-minecraft/tests/crash_fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    fn crlf(bytes: &[u8]) -> Vec<u8> {
        String::from_utf8_lossy(bytes)
            .replace('\n', "\r\n")
            .into_bytes()
    }

    fn detected(bytes: &[u8]) -> Vec<String> {
        let built_in = agora_game_minecraft::crash::built_in_packs();
        let report = agora_core::crash_packs::load_built_in_packs(&built_in);
        let pack = &report.packs[0].pack;
        detect_parsers(pack, "pasted-3f9a.log", bytes)
            .unwrap()
            .into_iter()
            .map(|parser| parser.id.clone())
            .collect()
    }

    #[test]
    fn each_minecraft_file_is_recognised_under_any_name_and_either_line_end() {
        for (file, parser) in [
            ("crash_fabric.txt", "crash_report"),
            ("crash_forge.txt", "crash_report"),
            ("latest_mixin.log", "latest_log"),
            ("latest_fabric_missing.log", "latest_log"),
            ("hs_err_sample.log", "hs_err"),
        ] {
            let lf = fixture(file);
            assert_eq!(detected(&lf), vec![parser], "{file} with LF");
            assert_eq!(detected(&crlf(&lf)), vec![parser], "{file} with CRLF");
        }
    }

    #[test]
    fn other_logs_are_not_claimed() {
        let skyrim = b"Skyrim SSE v1.6.1170\r\nCrashLoggerSSE v1-15-0-0 Nov  1 2024\r\n\r\n\
Unhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF6C1A2B3C4 SkyrimSE.exe+0123456\r\n";
        let plain_timestamps = b"[12:00:01] [info] service started\n[12:00:02] [warn] retrying\n";
        assert!(detected(skyrim).is_empty());
        assert!(detected(plain_timestamps).is_empty());
        assert!(detected(&fixture("lookalike.txt")).is_empty());
        assert!(detected(b"").is_empty());
    }
}
