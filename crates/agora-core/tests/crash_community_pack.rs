//! Community crash signatures: the files contributors write, and the rules the app makes from
//! them. Every log line here is invented for these tests.

use agora_core::crash_engine::{
    evaluate, ArtifactText, EngineFacts, EvaluateInput, EvaluateOutput, Inspection,
    InspectionOutcome, InspectionRequest, Inspector, PackInput, ParsedArtifact, PluginIdentity,
    RefResolver, Resolved,
};
use agora_core::crash_packs::{
    parse_community_signature_json, COMMUNITY_PACK_PLUGIN_ID, COMMUNITY_PACK_PLUGIN_NAME,
};
use agora_game_api::crash::{
    lint_rule, AttrValue, CouldNotCheckReason, Coverage, CoverageState, CrashPack, Fact,
    ParserOutput, Producer, Rule, Scalar, CRASH_CONTRACT_VERSION,
};
use std::collections::BTreeMap;
use std::path::Path;

struct NoResolver;

impl RefResolver for NoResolver {
    fn resolve(&self, _reference: &agora_game_api::crash::Ref) -> Resolved {
        Resolved {
            owner: agora_game_api::crash::Owner::Unknown,
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

fn rule_from_json(json: &str) -> Rule {
    let signature = parse_community_signature_json(json).expect("the signature parses");
    let rule = signature.to_rule().expect("the signature converts");
    let problems = lint_rule(&rule);
    assert!(problems.is_empty(), "{problems:#?}");
    rule
}

fn pack(rule: Rule) -> CrashPack {
    CrashPack {
        contract: CRASH_CONTRACT_VERSION,
        games: Vec::new(),
        sources: Vec::new(),
        parsers: Vec::new(),
        inspectors: Vec::new(),
        ref_maps: Vec::new(),
        roles: Vec::new(),
        save_risk: None,
        rules: vec![rule],
    }
}

fn parsed_log_line() -> ParsedArtifact {
    let fact = Fact {
        kind: "core.log_line".to_string(),
        attrs: BTreeMap::from([
            (
                "level".to_string(),
                AttrValue::Scalar(Scalar::Text("error".to_string())),
            ),
            (
                "text".to_string(),
                AttrValue::Scalar(Scalar::Text(
                    "Unhandled exception while loading the example thing".to_string(),
                )),
            ),
        ]),
        refs: Vec::new(),
        span: None,
    };
    ParsedArtifact {
        plugin: PluginIdentity {
            id: "test.reader".to_string(),
            name: "Test reader".to_string(),
            version: "1.0".to_string(),
        },
        parser_id: "test_reader".to_string(),
        artifact: "invented-crash.log".to_string(),
        source: None,
        output: ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: "test_log".to_string(),
                version: None,
            },
            facts: vec![fact],
            bucket: Vec::new(),
            coverage: vec![Coverage {
                kind: "core.log_line".to_string(),
                state: CoverageState::Checked,
                why: None,
            }],
            notes: Vec::new(),
        },
    }
}

fn instance_facts(game: &str) -> EngineFacts {
    vec![Fact {
        kind: "core.instance".to_string(),
        attrs: BTreeMap::from([(
            "game".to_string(),
            AttrValue::Scalar(Scalar::Text(game.to_string())),
        )]),
        refs: Vec::new(),
        span: None,
    }]
}

fn evaluate_for_game(rule: Rule, game: &str) -> EvaluateOutput {
    let parsed = parsed_log_line();
    let texts = [ArtifactText {
        artifact: "invented-crash.log".to_string(),
        source: None,
        text: "Unhandled exception while loading the example thing\n".to_string(),
    }];
    let facts = instance_facts(game);
    let packs = [PackInput {
        plugin: PluginIdentity {
            id: COMMUNITY_PACK_PLUGIN_ID.to_string(),
            name: COMMUNITY_PACK_PLUGIN_NAME.to_string(),
            version: String::new(),
        },
        pack: pack(rule),
    }];
    evaluate(&EvaluateInput {
        packs: &packs,
        parsed: std::slice::from_ref(&parsed),
        texts: &texts,
        engine_facts: &facts,
        runtime_version: None,
        resolver: &NoResolver,
        inspector: &NoInspector,
    })
}

const SKYRIM_SIGNATURE: &str = r#"{
  "id": "skyrim_example",
  "name": "Skyrim Example",
  "games": ["skyrim-se"],
  "regex_pattern": "Unhandled exception",
  "level": "possible",
  "cause": "other",
  "text": "The game reported an unhandled exception."
}"#;

const MINECRAFT_SIGNATURE: &str = r#"{
  "id": "minecraft_example",
  "name": "Minecraft Example",
  "games": ["minecraft"],
  "regex_pattern": "Unhandled exception",
  "level": "possible",
  "cause": "other",
  "text": "The game reported an unhandled exception."
}"#;

#[test]
fn a_skyrim_signature_fires_on_skyrim_and_not_on_minecraft() {
    let skyrim = evaluate_for_game(rule_from_json(SKYRIM_SIGNATURE), "skyrim-se");
    assert_eq!(skyrim.findings.len(), 1);
    assert_eq!(
        skyrim.findings[0].source.rule_id.as_deref(),
        Some("skyrim_example")
    );
    assert!(skyrim.findings[0].candidate.is_none());
    assert!(skyrim.signals.is_empty());

    let minecraft = evaluate_for_game(rule_from_json(SKYRIM_SIGNATURE), "minecraft");
    assert!(minecraft.findings.is_empty(), "{:?}", minecraft.findings);
}

#[test]
fn a_minecraft_signature_fires_on_minecraft_and_not_on_skyrim() {
    let minecraft = evaluate_for_game(rule_from_json(MINECRAFT_SIGNATURE), "minecraft");
    assert_eq!(minecraft.findings.len(), 1);
    assert_eq!(
        minecraft.findings[0].source.rule_id.as_deref(),
        Some("minecraft_example")
    );

    let skyrim = evaluate_for_game(rule_from_json(MINECRAFT_SIGNATURE), "skyrim-se");
    assert!(skyrim.findings.is_empty(), "{:?}", skyrim.findings);
}

#[test]
fn a_whole_file_rule_with_no_artifacts_is_could_not_check() {
    let rule = rule_from_json(SKYRIM_SIGNATURE);
    let facts = instance_facts("skyrim-se");
    let packs = [PackInput {
        plugin: PluginIdentity {
            id: COMMUNITY_PACK_PLUGIN_ID.to_string(),
            name: COMMUNITY_PACK_PLUGIN_NAME.to_string(),
            version: String::new(),
        },
        pack: pack(rule),
    }];
    let output = evaluate(&EvaluateInput {
        packs: &packs,
        parsed: &[],
        texts: &[],
        engine_facts: &facts,
        runtime_version: None,
        resolver: &NoResolver,
        inspector: &NoInspector,
    });
    assert!(output.findings.is_empty());
    assert_eq!(output.could_not_check.len(), 1);
    assert_eq!(output.could_not_check[0].subject, "artifact text");
    assert_eq!(
        output.could_not_check[0].reason,
        CouldNotCheckReason::NotCovered
    );
}

#[test]
fn every_signature_file_in_the_repository_converts_and_lints() {
    let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crash-signatures");
    let entries = std::fs::read_dir(&folder)
        .unwrap_or_else(|error| panic!("reading {}: {error}", folder.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.display().to_string();
        let signature =
            parse_community_signature_json(&text).unwrap_or_else(|error| panic!("{name}: {error}"));
        let rule = signature
            .to_rule()
            .unwrap_or_else(|errors| panic!("{name}: {errors:?}"));
        let problems = lint_rule(&rule);
        assert!(problems.is_empty(), "{name}: {problems:#?}");
    }
}
