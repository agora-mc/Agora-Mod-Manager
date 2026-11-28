//! A built-in pack reaches the engine through the same door as a plugin's, and a plugin's rule
//! sees the facts its parsers produce. Attribution is the only difference.

use agora_core::crash_engine::{
    answer, consensus, evaluate, AnswerInput, ConsensusInput, ConsensusProfile, EngineFacts,
    EvaluateInput, Inspection, InspectionOutcome, InspectionRequest, Inspector, ParsedArtifact,
    PluginIdentity, RefResolver, Resolved,
};
use agora_core::crash_packs::{
    artifact_handle, built_in_identity, load_built_in_packs, load_plugin_packs,
    parse_artifact_with_builtin, parser_request, MemoryArtifact, ParserFailureKind,
};
use agora_game_api::crash::{
    lint_pack_json, BuiltInPack, BuiltInParser, BuiltInParserError, Coverage, CoverageState,
    CrashPack, Fact, GameHandle, Level, MatchedBy, Owner, ParserOutput, ParserReads, ParserRequest,
    Producer, Ref, SessionVerdict, CRASH_CONTRACT_VERSION,
};
use agora_plugin_api::manifest::PluginManifest;
use agora_plugin_host::QuickJsHost;
use std::sync::Arc;

struct FakeParser;

impl BuiltInParser for FakeParser {
    fn parse(
        &self,
        _request: &ParserRequest,
        _reads: &dyn ParserReads,
    ) -> Result<ParserOutput, BuiltInParserError> {
        Ok(ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: "demo".to_string(),
                version: None,
            },
            facts: vec![Fact {
                kind: "demo.signal".to_string(),
                attrs: std::collections::BTreeMap::new(),
                refs: Vec::new(),
                span: None,
            }],
            bucket: Vec::new(),
            coverage: vec![Coverage {
                kind: "demo.signal".to_string(),
                state: CoverageState::Checked,
                why: None,
            }],
            notes: Vec::new(),
        })
    }
}

struct NoResolver;

impl RefResolver for NoResolver {
    fn resolve(&self, _reference: &Ref) -> Resolved {
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

fn built_in_pack() -> BuiltInPack {
    BuiltInPack {
        file: "demo/pack.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "parsers": [{
                "id": "demo",
                "entry": { "built_in": { "name": "demo" } }
            }],
            "rules": [{
                "id": "builtin_rule",
                "version": 1,
                "scope": { "producer": { "format": "demo" } },
                "when": { "fact": { "kind": "demo.signal" } },
                "effects": [{ "finding": { "scope": "environment", "level": "possible" } }],
                "text": { "headline": { "en": "The built-in parser saw a signal." } }
            }]
        })
        .to_string(),
        parsers: vec![("demo".to_string(), Box::new(FakeParser))],
    }
}

fn plugin_pack() -> CrashPack {
    lint_pack_json(
        &serde_json::json!({
            "contract": 1,
            "rules": [{
                "id": "plugin_rule",
                "version": 1,
                "when": { "fact": { "kind": "demo.signal" } },
                "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
                "text": { "headline": { "en": "A plugin rule saw the same signal." } }
            }]
        })
        .to_string(),
    )
    .expect("the plugin pack lints")
}

fn parse_with(built_in: Option<&BuiltInPack>, pack: &CrashPack) -> ParsedArtifact {
    let limits = agora_game_api::crash::ParserLimits::DEFAULT;
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("artifact", b"demo".to_vec());
    let request = parser_request(
        "demo",
        artifact_handle("artifact", "artifact.log", 4, Vec::new()),
        GameHandle {
            id: "demo-game".to_string(),
            runtime_version: None,
        },
        limits,
    );
    let parsed = parse_artifact_with_builtin(
        &QuickJsHost::new(),
        "agora",
        pack,
        &request,
        Arc::new(artifact),
        Some("demo"),
        b"",
        built_in,
    );
    let output = parsed
        .parsed()
        .cloned()
        .unwrap_or_else(|| panic!("expected the built-in parser to parse: {parsed:?}"));
    ParsedArtifact {
        plugin: built_in_identity(),
        parser_id: "demo".to_string(),
        artifact: "artifact.log".to_string(),
        source: None,
        output,
    }
}

#[test]
fn crash_a_built_in_pack_loads_and_names_agora() {
    let report = load_built_in_packs(&[built_in_pack()]);
    assert!(report.is_clean(), "{:#?}", report.problems);
    assert_eq!(report.packs.len(), 1);
    assert_eq!(report.packs[0].plugin_id, "agora");
    let identity = built_in_identity();
    assert_eq!(identity.id, "agora");
    assert_eq!(identity.name, "Agora");
    assert!(!identity.version.is_empty());
}

#[test]
fn crash_a_plugin_rule_sees_a_built_in_parsers_fact_and_names_the_plugin() {
    let built_in = built_in_pack();
    let report = load_built_in_packs(std::slice::from_ref(&built_in));
    assert!(report.is_clean(), "{:#?}", report.problems);
    let parsed = parse_with(Some(&built_in), &report.packs[0].pack);

    let packs = vec![
        agora_core::crash_engine::PackInput {
            plugin: built_in_identity(),
            pack: report.packs[0].pack.clone(),
        },
        agora_core::crash_engine::PackInput {
            plugin: PluginIdentity {
                id: "acme.extra".to_string(),
                name: "Extra rules".to_string(),
                version: "1.0.0".to_string(),
            },
            pack: plugin_pack(),
        },
    ];
    let evaluated = evaluate(&EvaluateInput {
        packs: &packs,
        parsed: std::slice::from_ref(&parsed),
        texts: &[],
        engine_facts: &EngineFacts::new(),
        runtime_version: Some("1.0"),
        resolver: &NoResolver,
        inspector: &NoInspector,
    });
    let sources: Vec<&str> = evaluated
        .findings
        .iter()
        .map(|finding| finding.source.plugin_id.as_str())
        .collect();
    assert!(sources.contains(&"agora"), "{sources:?}");
    assert!(sources.contains(&"acme.extra"), "{sources:?}");
    assert_eq!(evaluated.findings.len(), 2);
}

#[test]
fn crash_without_its_parser_a_built_in_pack_is_a_could_not_check() {
    let built_in = built_in_pack();
    let report = load_built_in_packs(std::slice::from_ref(&built_in));
    assert!(report.is_clean());

    let limits = agora_game_api::crash::ParserLimits::DEFAULT;
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("artifact", b"demo".to_vec());
    let request = parser_request(
        "demo",
        artifact_handle("artifact", "artifact.log", 4, Vec::new()),
        GameHandle {
            id: "demo-game".to_string(),
            runtime_version: None,
        },
        limits,
    );
    let parsed = parse_artifact_with_builtin(
        &QuickJsHost::new(),
        "agora",
        &report.packs[0].pack,
        &request,
        Arc::new(artifact),
        Some("demo"),
        b"",
        None,
    );
    let failure = parsed.failures().next().expect("a failure");
    assert_eq!(failure.kind, ParserFailureKind::UnsupportedEntry);
    assert_eq!(failure.plugin_id, "agora");
}

#[test]
fn crash_a_plugins_pack_may_not_name_a_built_in_parser() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash/pack.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::json!({
            "contract": 1,
            "parsers": [{
                "id": "sneaky",
                "entry": { "built_in": { "name": "demo" } }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let manifest = PluginManifest::parse(
        &serde_json::json!({
            "manifest": 1,
            "id": "acme.test",
            "name": "Test",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "capabilities": { "required": ["crash:contribute"] },
            "contributions": { "crashPacks": [{ "path": "crash/pack.json" }] }
        })
        .to_string(),
    )
    .unwrap();
    let report = load_plugin_packs("acme.test", dir.path(), &manifest);
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "built_in");
}

#[test]
fn crash_a_built_in_packs_rule_produces_an_answer_with_agora_as_its_source() {
    let built_in = built_in_pack();
    let report = load_built_in_packs(std::slice::from_ref(&built_in));
    let parsed = parse_with(Some(&built_in), &report.packs[0].pack);
    let packs = vec![agora_core::crash_engine::PackInput {
        plugin: built_in_identity(),
        pack: report.packs[0].pack.clone(),
    }];
    let evaluated = evaluate(&EvaluateInput {
        packs: &packs,
        parsed: std::slice::from_ref(&parsed),
        texts: &[],
        engine_facts: &EngineFacts::new(),
        runtime_version: Some("1.0"),
        resolver: &NoResolver,
        inspector: &NoInspector,
    });
    let caps: Vec<Level> = evaluated.level_cap.into_iter().collect();
    let consensus = consensus(
        &ConsensusInput {
            signals: &evaluated.signals,
            roles: &evaluated.roles,
            level_caps: &caps,
        },
        &ConsensusProfile::default(),
    );
    let answer = answer(&AnswerInput {
        verdict: SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        evaluated: &evaluated,
        consensus: &consensus,
    });
    assert!(evaluated
        .findings
        .iter()
        .all(|finding| finding.source.plugin_name == "Agora"));
    assert!(answer
        .findings
        .iter()
        .all(|finding| finding.source.plugin_name == "Agora"));
    assert!(!answer
        .headline
        .get("en")
        .unwrap_or(&String::new())
        .is_empty());
}

#[test]
fn crash_a_built_in_pack_may_ship_only_rules() {
    let pack = BuiltInPack {
        file: "demo/rules.json".to_string(),
        json: serde_json::json!({
            "contract": 1,
            "rules": [{
                "id": "rules_only",
                "version": 1,
                "when": { "fact": { "kind": "demo.signal" } },
                "effects": [{ "context": { "text": { "en": "A rule with no parser." } } }],
                "text": { "headline": { "en": "A rule with no parser." } }
            }]
        })
        .to_string(),
        parsers: Vec::new(),
    };
    let report = load_built_in_packs(&[pack]);
    assert!(report.is_clean(), "{:#?}", report.problems);
}
