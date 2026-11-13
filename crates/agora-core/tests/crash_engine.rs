//! Hostile tests for the crash doctor's evaluator, consensus and answer. Each is named for the
//! failure it catches, so a broken scorer fails a test rather than quietly naming the wrong mod.

use agora_core::crash_engine::{
    answer, consensus, evaluate, AnswerInput, ConsensusInput, ConsensusOutput, ConsensusProfile,
    EngineFacts, EvaluateInput, EvaluateOutput, Inspection, InspectionOutcome, InspectionRequest,
    Inspector, PackInput, ParsedArtifact, PluginIdentity,
};
use agora_game_api::crash::{
    certainty_claim, AttrValue, Cause, Coverage, CoverageState, CrashPack, Fact, Level, MatchedBy,
    NextStepKind, Owner, ParserOutput, Producer, Ref, Role, Rule, Scalar, Scope, SessionVerdict,
    Strength, CRASH_CONTRACT_VERSION,
};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct FakeResolver {
    entries: Vec<(String, String, Owner, Option<Role>)>,
    prefixes: Vec<(String, Owner, Option<Role>)>,
}

impl FakeResolver {
    fn new() -> Self {
        Self::default()
    }

    fn with(mut self, kind: &str, value: &str, owner: Owner, role: Option<Role>) -> Self {
        self.entries
            .push((kind.to_string(), value.to_string(), owner, role));
        self
    }

    fn with_prefix(mut self, kind: &str, prefix: &str, owner: Owner, role: Option<Role>) -> Self {
        self.prefixes
            .push((format!("{kind}\0{prefix}"), owner, role));
        self
    }
}

impl agora_core::crash_engine::RefResolver for FakeResolver {
    fn resolve(&self, reference: &Ref) -> agora_core::crash_engine::Resolved {
        for (kind, value, owner, role) in &self.entries {
            if kind == &reference.kind && value == &reference.value {
                return agora_core::crash_engine::Resolved {
                    owner: owner.clone(),
                    role: *role,
                };
            }
        }
        for (key, owner, role) in &self.prefixes {
            let (kind, prefix) = key.split_once('\0').expect("a prefix key");
            if kind == reference.kind && reference.value.starts_with(prefix) {
                return agora_core::crash_engine::Resolved {
                    owner: owner.clone(),
                    role: *role,
                };
            }
        }
        agora_core::crash_engine::Resolved {
            owner: Owner::Unknown,
            role: None,
        }
    }
}

struct FixedInspector(Inspection);

impl Inspector for FixedInspector {
    fn inspect(&self, _request: &InspectionRequest<'_>) -> InspectionOutcome {
        self.0.clone().into()
    }
}

fn plugin(id: &str) -> PluginIdentity {
    PluginIdentity {
        id: id.to_string(),
        name: format!("{id} plugin"),
        version: "1.0".to_string(),
    }
}

fn pack(plugin_id: &str, rules: Vec<Value>) -> PackInput {
    let rules = rules
        .into_iter()
        .map(|value| serde_json::from_value::<Rule>(value).expect("test rule"))
        .collect();
    PackInput {
        plugin: plugin(plugin_id),
        pack: CrashPack {
            contract: CRASH_CONTRACT_VERSION,
            games: Vec::new(),
            sources: Vec::new(),
            parsers: Vec::new(),
            inspectors: Vec::new(),
            ref_maps: Vec::new(),
            roles: Vec::new(),
            save_risk: None,
            rules,
        },
    }
}

fn parsed_with(
    plugin_id: &str,
    parser_id: &str,
    format: &str,
    facts: Vec<Fact>,
    coverage: Vec<Coverage>,
) -> ParsedArtifact {
    ParsedArtifact {
        plugin: plugin(plugin_id),
        parser_id: parser_id.to_string(),
        artifact: "crash.log".to_string(),
        source: None,
        output: ParserOutput {
            contract: CRASH_CONTRACT_VERSION,
            producer: Producer {
                format: format.to_string(),
                version: None,
            },
            facts,
            bucket: Vec::new(),
            coverage,
            notes: Vec::new(),
        },
    }
}

fn item(key: &str, name: &str) -> Owner {
    Owner::Item {
        key: key.to_string(),
        name: name.to_string(),
    }
}

fn number_attr(value: f64) -> AttrValue {
    AttrValue::Scalar(Scalar::Number(value))
}

fn text_attr(value: &str) -> AttrValue {
    AttrValue::Scalar(Scalar::Text(value.to_string()))
}

fn frame(index: u32, module: &str) -> Fact {
    Fact {
        kind: "core.frame".to_string(),
        attrs: [("index".to_string(), number_attr(index.into()))]
            .into_iter()
            .collect(),
        refs: vec![Ref {
            kind: "module".to_string(),
            value: module.to_string(),
            strength: Strength::Exec,
        }],
        span: None,
    }
}

fn file_object(path: &str) -> Fact {
    Fact {
        kind: "creation.object".to_string(),
        attrs: [("type".to_string(), text_attr("ExampleTriShape*"))]
            .into_iter()
            .collect(),
        refs: vec![Ref {
            kind: "file".to_string(),
            value: path.to_string(),
            strength: Strength::Object,
        }],
        span: None,
    }
}

fn signal_rule(id: &str, family: &str, weight: u32) -> Value {
    json!({
        "id": id,
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": family, "candidate": { "owner_of": "m" }, "weight": weight } }],
        "text": { "headline": { "en": "A clue." } }
    })
}

fn signal_rule_with_cause(id: &str, family: &str, weight: u32, module: &str, cause: &str) -> Value {
    json!({
        "id": id,
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": module, "bind": "m" } } },
        "effects": [{ "signal": { "family": family, "candidate": { "owner_of": "m" }, "weight": weight, "cause": cause } }],
        "text": { "headline": { "en": "A clue." } }
    })
}

fn root_cause_rule(id: &str, family: &str, weight: u32, level: &str) -> Value {
    json!({
        "id": id,
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{
            "finding": {
                "scope": "root_cause",
                "level": level,
                "candidate": { "owner_of": "m" },
                "family": family,
                "weight": weight
            }
        }],
        "text": { "headline": { "en": "{owner:m} is named by this rule." } }
    })
}

fn run(
    packs: &[PackInput],
    parsed: &[ParsedArtifact],
    engine_facts: &EngineFacts,
    resolver: &FakeResolver,
    inspector: &dyn Inspector,
    runtime: Option<&str>,
) -> (EvaluateOutput, ConsensusOutput) {
    let evaluated = evaluate(&EvaluateInput {
        packs,
        parsed,
        engine_facts,
        runtime_version: runtime,
        resolver,
        inspector,
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
    (evaluated, consensus)
}

fn answer_for(
    verdict: SessionVerdict,
    evaluated: &EvaluateOutput,
    consensus: &ConsensusOutput,
) -> agora_game_api::crash::CrashAnswer {
    answer(&AnswerInput {
        verdict,
        evaluated,
        consensus,
    })
}

fn top_key(candidates: &[agora_game_api::crash::Candidate]) -> Option<String> {
    candidates
        .first()
        .and_then(|candidate| match &candidate.owner {
            Owner::Item { key, .. } => Some(key.clone()),
            _ => None,
        })
}

fn english(value: &agora_game_api::crash::LocalText) -> String {
    value.get("en").cloned().unwrap_or_default()
}

#[test]
fn empty_parser_output_abstains_and_names_nobody() {
    let packs = vec![pack("p", vec![signal_rule("r", "execution", 10)])];
    let parsed: Vec<ParsedArtifact> = Vec::new();
    let engine_facts: EngineFacts = Vec::new();
    let resolver = FakeResolver::new();
    let inspector = FixedInspector(Inspection::Missing);
    let (evaluated, consensus) = run(
        &packs,
        &parsed,
        &engine_facts,
        &resolver,
        &inspector,
        Some("1.0"),
    );
    assert!(evaluated.signals.is_empty());
    assert!(consensus.candidates.is_empty());
    assert!(consensus.abstained);
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &evaluated,
        &consensus,
    );
    assert!(answer.candidates.is_empty());
    assert_eq!(answer.level, Level::NotSure);
    let headline = english(&answer.headline).to_lowercase();
    assert!(!headline.contains("healthy"));
    assert!(!headline.contains("fine"));
    assert_eq!(certainty_claim(&headline), None);
    assert_ne!(answer.next.kind, NextStepKind::NothingNeeded);
}

fn example(name: &str) -> String {
    let path = match name {
        "minecraft.pack" => {
            include_str!("../../agora-game-api/tests/crash_examples/minecraft.pack.json")
        }
        "minecraft.output" => {
            include_str!("../../agora-game-api/tests/crash_examples/minecraft.output.json")
        }
        "skyrim.pack" => include_str!("../../agora-game-api/tests/crash_examples/skyrim.pack.json"),
        "skyrim.output" => {
            include_str!("../../agora-game-api/tests/crash_examples/skyrim.output.json")
        }
        _ => panic!("unknown example {name}"),
    };
    path.to_string()
}

#[test]
fn positive_control_minecraft_names_the_mod_the_loader_named() {
    let pack = agora_game_api::crash::lint_pack_json(&example("minecraft.pack"))
        .expect("the Minecraft example lints");
    let output: ParserOutput =
        serde_json::from_str(&example("minecraft.output")).expect("the Minecraft output parses");
    let packs = vec![PackInput {
        plugin: plugin("minecraft"),
        pack,
    }];
    let parsed = vec![ParsedArtifact {
        plugin: plugin("minecraft"),
        parser_id: "jvm_log".to_string(),
        artifact: "crash-report.txt".to_string(),
        source: None,
        output,
    }];
    let resolver = FakeResolver::new()
        .with(
            "mod_id",
            "examplemod",
            item("examplemod", "Example Mod"),
            None,
        )
        .with_prefix("class", "net.minecraft", Owner::Base, Some(Role::Platform))
        .with_prefix(
            "class",
            "org.spongepowered",
            Owner::Framework {
                name: "Mixin".to_string(),
            },
            Some(Role::Framework),
        )
        .with_prefix(
            "class",
            "com.example",
            item("example-widgets", "Example Widgets"),
            None,
        );
    let inspector = FixedInspector(Inspection::Pass);
    let (evaluated, consensus) = run(
        &packs,
        &parsed,
        &EngineFacts::new(),
        &resolver,
        &inspector,
        Some("1.21"),
    );
    assert_eq!(
        top_key(&consensus.candidates).as_deref(),
        Some("examplemod")
    );
    assert_eq!(consensus.level, Level::VeryLikely);
    assert!(!consensus.abstained);
    assert!(evaluated
        .findings
        .iter()
        .any(|finding| finding.source.plugin_id == "minecraft"));
}

#[test]
fn positive_control_skyrim_names_the_mesh_the_inspector_failed() {
    let pack = agora_game_api::crash::lint_pack_json(&example("skyrim.pack"))
        .expect("the Skyrim example lints");
    let output: ParserOutput =
        serde_json::from_str(&example("skyrim.output")).expect("the Skyrim output parses");
    let packs = vec![PackInput {
        plugin: plugin("skyrim-se"),
        pack,
    }];
    let parsed = vec![ParsedArtifact {
        plugin: plugin("skyrim-se"),
        parser_id: "crash_logger".to_string(),
        artifact: "crash-2026-09-18.log".to_string(),
        source: None,
        output,
    }];
    let resolver = FakeResolver::new()
        .with("module", "SkyrimSE.exe", Owner::Base, Some(Role::Platform))
        .with(
            "module",
            "ExamplePhysics.dll",
            item("example-physics", "Example Physics"),
            Some(Role::HookFramework),
        )
        .with(
            "file",
            "meshes/example/hair.nif",
            item("example-hair", "Example Hair"),
            None,
        );
    let inspector = FixedInspector(Inspection::Fail);
    let (evaluated, consensus) = run(
        &packs,
        &parsed,
        &EngineFacts::new(),
        &resolver,
        &inspector,
        Some("1.6.9999"),
    );
    assert_eq!(
        top_key(&consensus.candidates).as_deref(),
        Some("example-hair")
    );
    assert!(!consensus.abstained);
    assert!(evaluated
        .findings
        .iter()
        .any(|finding| finding.level >= Level::Likely));
}

#[test]
fn same_fact_twice_or_two_rules_in_one_family_does_not_raise_the_score() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let inspector = FixedInspector(Inspection::Missing);
    let packs = vec![pack("p", vec![signal_rule("once", "execution", 10)])];
    let single = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, one) = run(
        &packs,
        std::slice::from_ref(&single),
        &EngineFacts::new(),
        &resolver,
        &inspector,
        Some("1.0"),
    );
    let twice = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, duplicate) = run(
        &packs,
        std::slice::from_ref(&twice),
        &EngineFacts::new(),
        &resolver,
        &inspector,
        Some("1.0"),
    );
    assert_eq!(one.candidates[0].score, duplicate.candidates[0].score);
    assert_eq!(one.candidates[0].score, 10);

    let packs = vec![pack(
        "p",
        vec![
            signal_rule("strong", "execution", 10),
            signal_rule("weak", "execution", 6),
        ],
    )];
    let (evaluated, both) = run(
        &packs,
        std::slice::from_ref(&single),
        &EngineFacts::new(),
        &resolver,
        &inspector,
        Some("1.0"),
    );
    assert_eq!(evaluated.signals.len(), 2);
    assert_eq!(both.candidates[0].score, 10);
    assert_eq!(both.candidates[0].families.len(), 1);
}

#[test]
fn boost_signals_alone_give_no_candidate() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![signal_rule("memory", "local_confirmation", 20)],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(evaluated.signals.len(), 1);
    assert!(consensus.candidates.is_empty());
    assert!(consensus.abstained);
}

#[test]
fn a_candidate_cause_comes_from_its_strongest_signal_that_states_one() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    // The heavier clue states no cause; the lighter one says why. The lighter one still decides
    // the candidate's cause, because only it has an opinion.
    let packs = vec![pack(
        "p",
        vec![
            signal_rule_for_sort("heavy", "execution", 12, "modA.dll", None),
            signal_rule_for_sort(
                "why",
                "known_signature",
                4,
                "modA.dll",
                Some("version_mismatch"),
            ),
            signal_rule_for_sort("silent", "execution", 12, "modB.dll", None),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.candidates[0].cause, Cause::VersionMismatch);
    assert_eq!(consensus.candidates[1].cause, Cause::Mod);
}

#[test]
fn a_cause_tie_prefers_a_specific_kind_over_mod() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![
            signal_rule_for_sort("says_mod", "execution", 8, "modA.dll", Some("mod")),
            signal_rule_for_sort("says_file", "object_reference", 8, "modA.dll", Some("file")),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.candidates[0].cause, Cause::File);
}

#[test]
fn a_root_cause_finding_shares_its_cause_with_the_clue_it_feeds() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "built_for_another_version",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{
            "finding": {
                "scope": "root_cause",
                "level": "likely",
                "cause": "version_mismatch",
                "candidate": { "owner_of": "m" },
                "family": "verified_asset",
                "weight": 16
            }
        }],
        "text": { "headline": { "en": "{owner:m} was built for another version." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(
        evaluated.signals[0].cause,
        Some(Cause::VersionMismatch),
        "a finding's clue keeps its cause"
    );
    assert_eq!(consensus.candidates[0].cause, Cause::VersionMismatch);
}

#[test]
fn the_answer_cause_follows_the_top_candidate() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let packs = vec![pack(
        "p",
        vec![
            signal_rule_for_sort(
                "top",
                "known_signature",
                14,
                "modA.dll",
                Some("version_mismatch"),
            ),
            signal_rule_for_sort("second", "execution", 6, "modB.dll", Some("memory")),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Pid,
        },
        &evaluated,
        &consensus,
    );
    assert_eq!(top_key(&consensus.candidates).as_deref(), Some("mod-a"));
    assert_eq!(answer.cause, Some(Cause::VersionMismatch));
}

fn signal_rule_for_sort(
    id: &str,
    family: &str,
    weight: u32,
    module: &str,
    cause: Option<&str>,
) -> Value {
    match cause {
        Some(cause) => signal_rule_with_cause(id, family, weight, module, cause),
        None => signal_rule_for(id, family, weight, module),
    }
}

#[test]
fn not_over_an_absent_section_stays_unknown_and_does_not_fire() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "needs_system",
        "version": 1,
        "when": {
            "all": [
                { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
                { "not": { "fact": { "kind": "core.system" } } }
            ]
        },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let coverage = vec![Coverage {
        kind: "core.system".to_string(),
        state: CoverageState::Absent,
        why: Some("this format has no system section".to_string()),
    }];
    let parsed = parsed_with("p", "parser", "test", vec![frame(0, "modA.dll")], coverage);
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.signals.is_empty());
    assert!(consensus.abstained);
    assert!(evaluated.could_not_check.iter().any(|note| {
        note.subject == "core.system"
            && note.reason == agora_game_api::crash::CouldNotCheckReason::AbsentFromArtifact
    }));
}

#[test]
fn only_platform_and_system_on_the_stack_name_nobody() {
    let resolver = FakeResolver::new()
        .with("module", "game.exe", Owner::Base, Some(Role::Platform))
        .with("module", "ntdll.dll", Owner::System, Some(Role::System));
    let packs = vec![pack(
        "p",
        vec![
            signal_rule("game", "execution", 16),
            signal_rule("system", "execution", 16),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "game.exe"), frame(1, "ntdll.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let _ = evaluated;
    assert!(consensus.candidates.is_empty());
    assert!(consensus.abstained);
}

#[test]
fn hook_framework_alone_names_it_low_and_loses_to_another_evidenced_candidate() {
    let resolver = FakeResolver::new()
        .with(
            "module",
            "hook.dll",
            item("hook", "Hook Framework"),
            Some(Role::HookFramework),
        )
        .with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let hook_only = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "hook.dll")],
        Vec::new(),
    );
    let hook_pack = vec![pack("p", vec![signal_rule("hook", "execution", 16)])];
    let (_, alone) = run(
        &hook_pack,
        std::slice::from_ref(&hook_only),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(top_key(&alone.candidates).as_deref(), Some("hook"));
    assert_eq!(alone.level, Level::Possible);

    let both = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "hook.dll"), frame(1, "modA.dll")],
        Vec::new(),
    );
    let both_pack = vec![pack(
        "p",
        vec![
            signal_rule_for("hook", "execution", 16, "hook.dll"),
            decisive_rule_for("real", "modA.dll", 16),
        ],
    )];
    let (_, with_real) = run(
        &both_pack,
        std::slice::from_ref(&both),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(top_key(&with_real.candidates).as_deref(), Some("mod-a"));
    assert!(!with_real
        .candidates
        .iter()
        .any(|candidate| matches!(&candidate.owner, Owner::Item { key, .. } if key == "hook")));
}

#[test]
fn tie_for_first_abstains_and_shows_both_with_their_sources() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let packs = vec![
        pack(
            "p1",
            vec![signal_rule_for("a", "execution", 10, "modA.dll")],
        ),
        pack(
            "p2",
            vec![signal_rule_for("b", "execution", 10, "modB.dll")],
        ),
    ];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.candidates.len(), 2);
    assert_eq!(consensus.level, Level::NotSure);
    assert!(consensus.abstained);
    for candidate in &consensus.candidates {
        assert!(!candidate.sources.is_empty());
    }
}

#[test]
fn two_plugins_disagree_both_findings_are_shown_each_naming_its_plugin() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let rule = |id: &str, module: &str| {
        json!({
            "id": id,
            "version": 1,
            "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": module, "bind": "m" } } },
            "effects": [{
                "finding": {
                    "scope": "root_cause",
                    "level": "likely",
                    "candidate": { "owner_of": "m" },
                    "family": "known_signature",
                    "weight": 10
                }
            }],
            "text": { "headline": { "en": "This plugin names {owner:m}." } }
        })
    };
    let rule_a = rule("a", "modA.dll");
    let rule_b = rule("b", "modB.dll");
    let packs = vec![pack("p1", vec![rule_a]), pack("p2", vec![rule_b])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(evaluated.findings.len(), 2);
    let pairs: Vec<(String, String)> = evaluated
        .findings
        .iter()
        .map(|finding| {
            (
                finding.source.plugin_id.clone(),
                finding
                    .candidate
                    .as_ref()
                    .map(|owner| match owner {
                        Owner::Item { key, .. } => key.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default(),
            )
        })
        .collect();
    assert!(pairs.contains(&("p1".to_string(), "mod-a".to_string())));
    assert!(pairs.contains(&("p2".to_string(), "mod-b".to_string())));
    assert!(consensus.abstained);
}

#[test]
fn a_trust_note_lowers_a_top_level_answer() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "decisive_but_doubtful",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [
            {
                "finding": {
                    "scope": "root_cause",
                    "level": "very_likely",
                    "candidate": { "owner_of": "m" },
                    "family": "decisive_verdict",
                    "weight": 16
                }
            },
            { "trust": { "cap": "possible", "text": { "en": "A crash suppressor was loaded." } } }
        ],
        "text": { "headline": { "en": "The loader named this mod." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(evaluated.level_cap, Some(Level::Possible));
    assert_eq!(evaluated.trust_notes.len(), 1);
    assert_eq!(consensus.level, Level::Possible);
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &evaluated,
        &consensus,
    );
    assert_eq!(answer.level, Level::Possible);
    assert!(!consensus.abstained);
}

#[test]
fn inspector_failure_takes_on_unknown_and_never_the_passing_path() {
    let resolver =
        FakeResolver::new().with("file", "meshes/bad.nif", item("bad-mesh", "Bad Mesh"), None);
    let rule = json!({
        "id": "inspect_it",
        "version": 1,
        "when": { "fact": { "kind": "creation.object", "ref": { "kind": "file", "strength": "object", "bind": "mesh" } } },
        "verify": {
            "inspector": "mesh_header",
            "target": "mesh",
            "on_pass": [{ "signal": { "family": "decisive_verdict", "candidate": { "owner_of": "mesh" }, "weight": 16 } }],
            "on_fail": [{ "signal": { "family": "verified_asset", "candidate": { "owner_of": "mesh" }, "weight": 10 } }],
            "on_unknown": [{ "context": { "text": { "en": "Agora could not open the file." } } }]
        },
        "effects": [],
        "text": { "headline": { "en": "A mesh was in flight." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![file_object("meshes/bad.nif")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Failed("the inspector crashed".to_string())),
        Some("1.0"),
    );
    assert!(evaluated.signals.is_empty());
    assert!(consensus.abstained);
    assert!(evaluated.could_not_check.iter().any(|note| {
        note.subject == "mesh_header"
            && note.reason == agora_game_api::crash::CouldNotCheckReason::CodeFailed
    }));
    assert!(evaluated
        .findings
        .iter()
        .any(|finding| finding.scope == Scope::Environment));
}

#[test]
fn stopped_and_normal_exits_name_nobody() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![root_cause_rule(
            "blame",
            "decisive_verdict",
            16,
            "very_likely",
        )],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(!consensus.candidates.is_empty());

    for verdict in [SessionVerdict::Stopped, SessionVerdict::Normal] {
        let answer = answer_for(verdict, &evaluated, &consensus);
        assert!(answer.candidates.is_empty());
        assert_eq!(answer.level, Level::NotSure);
        assert_eq!(answer.next.kind, NextStepKind::NothingNeeded);
        let headline = english(&answer.headline).to_lowercase();
        assert!(!headline.contains("mod a"));
        assert_eq!(certainty_claim(&headline), None);
    }
}

#[test]
fn a_rule_whose_scope_excludes_this_runtime_or_producer_does_not_fire() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let scoped = |id: &str, scope: Value| {
        let mut rule = signal_rule(id, "execution", 10);
        rule["scope"] = scope;
        rule
    };
    let packs = vec![pack(
        "p",
        vec![
            scoped("runtime", json!({ "runtime": { "exact": ["1.0"] } })),
            scoped(
                "producer",
                json!({ "producer": { "format": "another_format" } }),
            ),
            scoped(
                "producer_version",
                json!({ "producer": { "format": "test", "version": { "exact": ["9"] } } }),
            ),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("2.0"),
    );
    assert!(evaluated.signals.is_empty());
}

fn shuffled_scenario() -> (Vec<PackInput>, Vec<ParsedArtifact>) {
    let rule_a = json!({
        "id": "a",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [
            { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 12 } },
            { "signal": { "family": "object_reference", "candidate": { "owner_of": "m" }, "weight": 8 } }
        ],
        "text": { "headline": { "en": "Two independent clues." } }
    });
    let rule_b = json!({
        "id": "b",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "where": { "index": 1 }, "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "known_signature", "candidate": { "owner_of": "m" }, "weight": 6 } }],
        "text": { "headline": { "en": "A weaker clue." } }
    });
    let rule_c = json!({
        "id": "c",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "where": { "actionable_index": 0 }, "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 5 } }],
        "text": { "headline": { "en": "The top actionable frame." } }
    });
    let packs = vec![pack("p1", vec![rule_a]), pack("p2", vec![rule_b, rule_c])];
    let coverage = vec![Coverage {
        kind: "core.system".to_string(),
        state: CoverageState::Absent,
        why: Some("not in this format".to_string()),
    }];
    let mut parsed = vec![
        parsed_with(
            "p1",
            "parser",
            "test",
            vec![frame(0, "modA.dll"), frame(2, "modB.dll")],
            coverage.clone(),
        ),
        parsed_with(
            "p2",
            "parser2",
            "test2",
            vec![frame(1, "modA.dll")],
            coverage.clone(),
        ),
        parsed_with(
            "p2",
            "parser3",
            "test3",
            vec![frame(0, "modB.dll"), frame(0, "modA.dll")],
            coverage,
        ),
    ];
    parsed[0].artifact = "a.log".to_string();
    parsed[1].artifact = "b.log".to_string();
    parsed[2].artifact = "c.log".to_string();
    (packs, parsed)
}

#[test]
fn shuffled_input_gives_byte_identical_serialized_output() {
    let (mut packs, mut parsed) = shuffled_scenario();
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let verdict = SessionVerdict::Crashed {
        how: MatchedBy::Time,
    };
    let first = {
        let (evaluated, consensus) = run(
            &packs,
            &parsed,
            &EngineFacts::new(),
            &resolver,
            &FixedInspector(Inspection::Missing),
            Some("1.0"),
        );
        answer_for(verdict, &evaluated, &consensus)
    };

    packs.reverse();
    parsed.reverse();
    for artifact in &mut parsed {
        artifact.output.facts.reverse();
        artifact.output.coverage.reverse();
    }
    let second = {
        let (evaluated, consensus) = run(
            &packs,
            &parsed,
            &EngineFacts::new(),
            &resolver,
            &FixedInspector(Inspection::Missing),
            Some("1.0"),
        );
        answer_for(verdict, &evaluated, &consensus)
    };
    let first = serde_json::to_string(&first).expect("serialize");
    let second = serde_json::to_string(&second).expect("serialize");
    assert_eq!(first, second);
}

#[test]
fn justified_abstention_names_nobody_and_starts_a_bisect() {
    let resolver = FakeResolver::new().with(
        "module",
        "hook.dll",
        item("hook", "Hook Framework"),
        Some(Role::HookFramework),
    );
    let rule = json!({
        "id": "read_bad_pointer",
        "version": 1,
        "when": {
            "all": [
                { "fact": { "kind": "core.exception", "where": { "category": "access_violation" } } },
                { "fact": { "kind": "core.frame", "ref": { "kind": "module", "role": "hook_framework", "bind": "hook" } } }
            ]
        },
        "effects": [
            { "finding": { "scope": "mechanism", "level": "likely" } },
            { "signal": { "family": "execution", "candidate": { "owner_of": "hook" }, "weight": 2 } }
        ],
        "text": {
            "headline": { "en": "The game read a bad pointer in its own code." },
            "why_not_sure": { "en": "The top frame is usually the victim, not the cause." }
        },
        "next_step": "bisect"
    });
    let exception = Fact {
        kind: "core.exception".to_string(),
        attrs: [("category".to_string(), text_attr("access_violation"))]
            .into_iter()
            .collect(),
        refs: Vec::new(),
        span: None,
    };
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![exception, frame(0, "hook.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(consensus.candidates.is_empty());
    assert!(consensus.abstained);
    assert!(evaluated
        .findings
        .iter()
        .any(|finding| finding.scope == Scope::Mechanism));
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &evaluated,
        &consensus,
    );
    assert!(answer.candidates.is_empty());
    assert_eq!(answer.next.kind, NextStepKind::Bisect);
    assert_ne!(english(&answer.headline), "");
}

#[test]
fn one_evidence_group_is_not_enough_for_the_top_level() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![root_cause_rule(
            "only_one_group",
            "verified_asset",
            16,
            "likely",
        )],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(top_key(&consensus.candidates).as_deref(), Some("mod-a"));
    assert_eq!(consensus.level, Level::Likely);
}

#[test]
fn same_event_corroboration_raises_the_level() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    // Two facts of the same event, in different families and from two rules, corroborate.
    let execution = json!({
        "id": "execution_clue",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "Its code was running." } }
    });
    let object = json!({
        "id": "object_clue",
        "version": 1,
        "when": { "fact": { "kind": "core.exception", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "object_reference", "candidate": { "owner_of": "m" }, "weight": 8 } }],
        "text": { "headline": { "en": "Its object was in flight." } }
    });
    let exception = Fact {
        kind: "core.exception".to_string(),
        attrs: [("category".to_string(), text_attr("access_violation"))]
            .into_iter()
            .collect(),
        refs: vec![Ref {
            kind: "module".to_string(),
            value: "modA.dll".to_string(),
            strength: Strength::Object,
        }],
        span: None,
    };
    let packs = vec![pack("p", vec![execution, object])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), exception],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::VeryLikely);
    assert_eq!(top_key(&consensus.candidates).as_deref(), Some("mod-a"));
    assert_eq!(consensus.candidates[0].score, 18);

    // One fact seen through two rules in one family raises nothing.
    let one = json!({
        "id": "one",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let same = json!({
        "id": "same",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "The same clue again." } }
    });
    let packs = vec![pack("p", vec![one, same])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.candidates[0].score, 10);
    assert_eq!(consensus.candidates[0].families.len(), 1);
}

// --- review fixes -----------------------------------------------------------

fn signal_rule_for(id: &str, family: &str, weight: u32, module: &str) -> Value {
    json!({
        "id": id,
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": module, "bind": "m" } } },
        "effects": [{ "signal": { "family": family, "candidate": { "owner_of": "m" }, "weight": weight } }],
        "text": { "headline": { "en": "A clue." } }
    })
}

#[test]
fn any_true_collects_the_bindings_of_the_child_that_held() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "any_true",
        "version": 1,
        "when": { "any": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "modA.dll", "bind": "m" } } },
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "nope.dll", "bind": "n" } } }
        ] },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(evaluated.signals.len(), 1);
    assert_eq!(top_key(&consensus.candidates).as_deref(), Some("mod-a"));
}

#[test]
fn any_of_nothing_that_fits_is_false() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "any_false",
        "version": 1,
        "when": { "any": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "nope.dll", "bind": "m" } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
}

#[test]
fn any_with_one_unknown_and_no_true_is_unknown() {
    let resolver = FakeResolver::new();
    let rule = json!({
        "id": "any_unknown",
        "version": 1,
        "when": { "any": [
            { "fact": { "kind": "core.system" } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let coverage = vec![Coverage {
        kind: "core.system".to_string(),
        state: CoverageState::Absent,
        why: Some("not in this format".to_string()),
    }];
    let parsed = parsed_with("p", "parser", "test", vec![frame(0, "modA.dll")], coverage);
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
    assert!(evaluated
        .could_not_check
        .iter()
        .any(|note| note.subject == "core.system"));
}

#[test]
fn any_inside_all_and_not_behaves_as_a_real_disjunction() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "nested_any",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "modA.dll", "bind": "m" } } },
            { "not": { "any": [
                { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "nope.dll" } } }
            ] } }
        ] },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(evaluated.signals.len(), 1);
}

fn superseding(id: &str, module: &str, gone: &str) -> Value {
    json!({
        "id": id,
        "version": 1,
        "supersedes": [gone],
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": module, "bind": "m" } } },
        "effects": [{ "signal": { "family": "known_signature", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "The newer rule." } }
    })
}

fn genuine_rule() -> Value {
    signal_rule_for("genuine", "execution", 10, "modA.dll")
}

fn signals_from(evaluated: &EvaluateOutput) -> Vec<String> {
    evaluated
        .signals
        .iter()
        .filter_map(|signal| signal.source.rule_id.clone())
        .collect()
}

#[test]
fn a_superseding_rule_that_is_false_does_not_remove_the_genuine_rule() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![
            genuine_rule(),
            superseding("superseder", "nope.dll", "genuine"),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(signals_from(&evaluated).contains(&"genuine".to_string()));
}

#[test]
fn a_superseding_rule_that_is_true_removes_the_rule_it_replaces() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![
            genuine_rule(),
            superseding("superseder", "modA.dll", "genuine"),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let rules = signals_from(&evaluated);
    assert!(rules.contains(&"superseder".to_string()));
    assert!(!rules.contains(&"genuine".to_string()));
}

#[test]
fn a_superseding_rule_that_is_unknown_does_not_remove_the_genuine_rule() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let uncertain = json!({
        "id": "superseder",
        "version": 1,
        "supersedes": ["genuine"],
        "when": { "fact": { "kind": "core.system" } },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "The newer rule." } }
    });
    let packs = vec![pack("p", vec![genuine_rule(), uncertain])];
    let coverage = vec![Coverage {
        kind: "core.system".to_string(),
        state: CoverageState::Absent,
        why: Some("not in this format".to_string()),
    }];
    let parsed = parsed_with("p", "parser", "test", vec![frame(0, "modA.dll")], coverage);
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(signals_from(&evaluated).contains(&"genuine".to_string()));
}

#[test]
fn a_rule_with_too_many_branches_does_not_fire_and_says_so() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack("p", vec![signal_rule("many", "execution", 10)])];
    let mut facts: Vec<Fact> = (0..40).map(|index| frame(index, "modA.dll")).collect();
    facts.push(frame(0, "modA.dll"));
    let parsed = parsed_with("p", "parser", "test", facts, Vec::new());
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.signals.is_empty());
    assert!(consensus.candidates.is_empty());
    assert!(evaluated.could_not_check.iter().any(|note| {
        note.reason == agora_game_api::crash::CouldNotCheckReason::TooManyMatches
            && note
                .source
                .as_ref()
                .and_then(|source| source.rule_id.as_deref())
                == Some("many")
    }));
}

#[test]
fn local_confirmation_is_boost_only_and_never_counts_as_a_group() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "execution_and_memory",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [
            { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 14 } },
            { "signal": { "family": "local_confirmation", "candidate": { "owner_of": "m" }, "weight": 1 } }
        ],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::Likely);
}

fn decisive_rule_for(id: &str, module: &str, weight: u32) -> Value {
    json!({
        "id": id,
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": module, "bind": "m" } } },
        "effects": [{
            "finding": {
                "scope": "root_cause",
                "level": "very_likely",
                "candidate": { "owner_of": "m" },
                "family": "decisive_verdict",
                "weight": weight
            }
        }],
        "text": { "headline": { "en": "{owner:m} was named." } }
    })
}

#[test]
fn a_decisive_family_still_needs_the_top_core_minimum() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![decisive_rule_for("weak_decisive", "modA.dll", 10)],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::Likely);
    assert!(!consensus.abstained);
}

#[test]
fn a_demoted_hook_framework_is_not_decisive() {
    let resolver = FakeResolver::new().with(
        "module",
        "hook.dll",
        item("hook", "Hook Framework"),
        Some(Role::HookFramework),
    );
    let packs = vec![pack("p", vec![decisive_rule_for("hook", "hook.dll", 16)])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "hook.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::Possible);
}

#[test]
fn two_decisive_candidates_tie_and_abstain_showing_both() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let packs = vec![
        pack("p1", vec![decisive_rule_for("a", "modA.dll", 16)]),
        pack("p2", vec![decisive_rule_for("b", "modB.dll", 16)]),
    ];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(consensus.abstained);
    assert_eq!(consensus.level, Level::NotSure);
    assert_eq!(consensus.candidates.len(), 2);
}

#[test]
fn core_score_ranks_first_and_a_boost_only_breaks_a_core_tie() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let rule_a = signal_rule_for("a", "execution", 12, "modA.dll");
    let rule_b = json!({
        "id": "b",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "value": "modB.dll", "bind": "m" } } },
        "effects": [
            { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 8 } },
            { "signal": { "family": "local_confirmation", "candidate": { "owner_of": "m" }, "weight": 10 } }
        ],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule_a, rule_b])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(top_key(&consensus.candidates).as_deref(), Some("mod-a"));
    assert_eq!(consensus.candidates[0].score, 12);
    assert_eq!(consensus.candidates[1].score, 18);
    assert!(consensus.abstained);
    assert_eq!(consensus.candidates.len(), 2);
}

#[test]
fn a_lower_level_uses_core_plus_boost() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "small_core_with_boost",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [
            { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 4 } },
            { "signal": { "family": "local_confirmation", "candidate": { "owner_of": "m" }, "weight": 5 } }
        ],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::Likely);
}

fn history_fact(predates: bool) -> Fact {
    Fact {
        kind: "core.history".to_string(),
        attrs: [(
            "predates_last_change".to_string(),
            AttrValue::Scalar(Scalar::Bool(predates)),
        )]
        .into_iter()
        .collect(),
        refs: Vec::new(),
        span: None,
    }
}

#[test]
fn change_correlation_does_not_fire_when_the_bucket_predates_the_change() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "changed",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
        "effects": [
            { "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } },
            { "signal": { "family": "change_correlation", "candidate": { "owner_of": "m" }, "weight": 8 } }
        ],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let facts = vec![history_fact(true)];
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &facts,
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(!evaluated
        .signals
        .iter()
        .any(|signal| signal.family == agora_game_api::crash::Family::ChangeCorrelation));

    let facts = vec![history_fact(false)];
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &facts,
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated
        .signals
        .iter()
        .any(|signal| signal.family == agora_game_api::crash::Family::ChangeCorrelation));
}

fn frame_with_symbol(index: u32, module: &str, symbol: &str) -> Fact {
    let mut value = frame(index, module);
    value.attrs.insert("symbol".to_string(), text_attr(symbol));
    value
}

#[test]
fn a_missing_attribute_is_unknown_not_false_under_not() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "missing_attr",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
            { "not": { "fact": { "kind": "core.frame", "where": { "symbol": "anything" } } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
    assert!(evaluated
        .could_not_check
        .iter()
        .any(|note| note.subject == "core.frame"));
}

#[test]
fn an_unparsable_version_is_unknown_not_false_under_not() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "pre_release_version",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
            { "not": { "fact": { "kind": "core.frame", "where": { "symbol": { "version": { "min": "1.20" } } } } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame_with_symbol(0, "modA.dll", "1.20-pre")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
    assert!(!evaluated.could_not_check.is_empty());
}

#[test]
fn an_unbound_variable_is_unknown_not_a_literal_compare() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "unbound_variable",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
            { "not": { "fact": { "kind": "core.frame", "where": { "symbol": "$nope" } } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame_with_symbol(0, "modA.dll", "anything")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
    assert!(!evaluated.could_not_check.is_empty());
}

#[test]
fn an_invalid_regex_inside_where_is_unknown_and_names_the_rule() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let rule = json!({
        "id": "bad_regex_rule",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
            { "not": { "fact": { "kind": "core.frame", "where": { "symbol": { "regex": "(" } } } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame_with_symbol(0, "modA.dll", "anything")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.findings.is_empty());
    assert!(evaluated.could_not_check.iter().any(|note| {
        note.reason == agora_game_api::crash::CouldNotCheckReason::CodeFailed
            && note
                .source
                .as_ref()
                .and_then(|source| source.rule_id.as_deref())
                == Some("bad_regex_rule")
    }));
}

#[test]
fn the_linter_refuses_an_unbound_variable() {
    let json = r#"{
        "id": "unbound",
        "version": 1,
        "when": { "all": [
            { "fact": { "kind": "core.frame", "ref": { "kind": "module", "bind": "m" } } },
            { "not": { "fact": { "kind": "core.frame", "where": { "symbol": "$nope" } } } }
        ] },
        "effects": [{ "finding": { "scope": "mechanism", "level": "possible" } }],
        "text": { "headline": { "en": "A clue." } }
    }"#;
    let problems = agora_game_api::crash::lint_rule_json(json).expect_err("the linter refuses it");
    assert!(problems
        .iter()
        .any(|problem| problem.code == agora_game_api::crash::LintCode::BadVariable));
}

#[test]
fn an_unknown_owner_is_never_named_and_gets_a_could_not_check_note() {
    let resolver = FakeResolver::new();
    let packs = vec![pack("p", vec![signal_rule("ghost", "execution", 10)])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "ghost.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated.signals.is_empty());
    assert!(consensus.candidates.is_empty());
    assert!(evaluated.could_not_check.iter().any(|note| {
        note.subject == "ghost.dll"
            && note
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("could not tell who owns"))
    }));
}

#[test]
fn system_and_not_in_instance_owners_are_never_candidates() {
    let resolver = FakeResolver::new()
        .with("module", "ntdll.dll", Owner::System, Some(Role::System))
        .with("module", "missing.esp", Owner::NotInInstance, None);
    let packs = vec![pack(
        "p",
        vec![
            signal_rule_for("system", "execution", 10, "ntdll.dll"),
            signal_rule_for("missing", "execution", 10, "missing.esp"),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "ntdll.dll"), frame(1, "missing.esp")],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(consensus.candidates.is_empty());
}

#[test]
fn a_root_cause_finding_with_an_unknown_owner_becomes_environment() {
    let resolver = FakeResolver::new();
    let packs = vec![pack(
        "p",
        vec![root_cause_rule(
            "ghost",
            "decisive_verdict",
            16,
            "very_likely",
        )],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "ghost.dll")],
        Vec::new(),
    );
    let (evaluated, _) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(evaluated
        .findings
        .iter()
        .all(|finding| finding.scope != Scope::RootCause && finding.candidate.is_none()));
    assert!(!evaluated.could_not_check.is_empty());
}

#[test]
fn base_generated_and_writable_owners_can_be_candidates() {
    let resolver = FakeResolver::new()
        .with("module", "game.exe", Owner::Base, None)
        .with(
            "module",
            "tool.dll",
            Owner::Generated {
                tool: "A Tool".to_string(),
            },
            None,
        )
        .with("module", "loose.dll", Owner::Writable, None);
    let packs = vec![pack(
        "p",
        vec![
            signal_rule_for("game", "execution", 10, "game.exe"),
            signal_rule_for("tool", "execution", 10, "tool.dll"),
            signal_rule_for("loose", "execution", 10, "loose.dll"),
        ],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![
            frame(0, "game.exe"),
            frame(1, "tool.dll"),
            frame(2, "loose.dll"),
        ],
        Vec::new(),
    );
    let (_, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(consensus
        .candidates
        .iter()
        .any(|candidate| candidate.owner == Owner::Base));
    assert!(consensus.candidates.iter().any(|candidate| matches!(
        &candidate.owner,
        Owner::Generated { tool } if tool == "A Tool"
    )));
    assert!(consensus
        .candidates
        .iter()
        .any(|candidate| candidate.owner == Owner::Writable));
}

#[test]
fn a_finding_level_never_exceeds_its_candidate_level_or_the_answer() {
    let resolver = FakeResolver::new().with(
        "module",
        "hook.dll",
        item("hook", "Hook Framework"),
        Some(Role::HookFramework),
    );
    let packs = vec![pack("p", vec![decisive_rule_for("hook", "hook.dll", 16)])];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "hook.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &evaluated,
        &consensus,
    );
    assert_eq!(answer.level, Level::Possible);
    for finding in &answer.findings {
        assert!(finding.level <= Level::Possible);
    }
    for candidate in &answer.candidates {
        assert!(candidate.level <= answer.level);
    }
}

#[test]
fn a_root_cause_finding_with_no_named_candidate_is_at_most_possible() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let packs = vec![
        pack("p1", vec![decisive_rule_for("a", "modA.dll", 16)]),
        pack("p2", vec![decisive_rule_for("b", "modB.dll", 16)]),
    ];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll"), frame(1, "modB.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert!(consensus.abstained);
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &evaluated,
        &consensus,
    );
    assert!(answer
        .findings
        .iter()
        .filter(|finding| finding.scope == Scope::RootCause)
        .all(|finding| finding.level <= Level::Possible));
}

#[test]
fn a_closed_unknown_session_caps_candidates_and_says_it_cannot_tell() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![decisive_rule_for("decisive", "modA.dll", 16)],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let answer = answer_for(SessionVerdict::ClosedUnknown, &evaluated, &consensus);
    assert_eq!(answer.level, Level::Possible);
    assert!(answer
        .candidates
        .iter()
        .all(|candidate| candidate.level <= Level::Possible));
    assert!(answer
        .findings
        .iter()
        .all(|finding| finding.level <= Level::Possible));
    let headline = english(&answer.headline).to_lowercase();
    assert!(headline.contains("cannot tell whether") || headline.contains("can't tell whether"));
}

#[test]
fn answer_applies_a_callers_trust_cap_to_every_candidate() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![decisive_rule_for("decisive", "modA.dll", 16)],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    assert_eq!(consensus.level, Level::VeryLikely);
    let mut capped = evaluated.clone();
    capped.level_cap = Some(Level::Possible);
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Time,
        },
        &capped,
        &consensus,
    );
    assert_eq!(answer.level, Level::Possible);
    assert!(answer
        .candidates
        .iter()
        .all(|candidate| candidate.level <= Level::Possible));
    assert!(answer
        .findings
        .iter()
        .all(|finding| finding.level <= Level::Possible));
}

#[test]
fn equal_frame_indexes_across_artifacts_are_order_independent() {
    let resolver = FakeResolver::new()
        .with("module", "modA.dll", item("mod-a", "Mod A"), None)
        .with("module", "modB.dll", item("mod-b", "Mod B"), None);
    let rule = json!({
        "id": "top_actionable",
        "version": 1,
        "when": { "fact": { "kind": "core.frame", "where": { "actionable_index": 0 }, "ref": { "kind": "module", "bind": "m" } } },
        "effects": [{ "signal": { "family": "execution", "candidate": { "owner_of": "m" }, "weight": 10 } }],
        "text": { "headline": { "en": "A clue." } }
    });
    let packs = vec![pack("p", vec![rule])];
    let mut parsed = vec![
        parsed_with(
            "p",
            "parser",
            "test",
            vec![frame(0, "modA.dll"), frame(0, "modB.dll")],
            Vec::new(),
        ),
        parsed_with(
            "p",
            "parser2",
            "test2",
            vec![frame(0, "modB.dll"), frame(0, "modA.dll")],
            Vec::new(),
        ),
    ];
    for artifact in &mut parsed {
        artifact.artifact = format!("{}.log", artifact.parser_id);
    }
    let verdict = SessionVerdict::Crashed {
        how: MatchedBy::Time,
    };
    let first = {
        let (evaluated, consensus) = run(
            &packs,
            &parsed,
            &EngineFacts::new(),
            &resolver,
            &FixedInspector(Inspection::Missing),
            Some("1.0"),
        );
        answer_for(verdict, &evaluated, &consensus)
    };
    parsed.reverse();
    for artifact in &mut parsed {
        artifact.output.facts.reverse();
    }
    let second = {
        let (evaluated, consensus) = run(
            &packs,
            &parsed,
            &EngineFacts::new(),
            &resolver,
            &FixedInspector(Inspection::Missing),
            Some("1.0"),
        );
        answer_for(verdict, &evaluated, &consensus)
    };
    assert_eq!(
        serde_json::to_string(&first).expect("serialize"),
        serde_json::to_string(&second).expect("serialize")
    );
}

#[test]
fn a_root_cause_finding_about_a_mod_defaults_to_the_mod_cause() {
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let packs = vec![pack(
        "p",
        vec![root_cause_rule(
            "names_the_mod",
            "known_signature",
            10,
            "likely",
        )],
    )];
    let parsed = parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    );
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );

    let finding = evaluated
        .findings
        .iter()
        .find(|finding| finding.candidate.is_some())
        .expect("a root-cause finding");
    assert_eq!(finding.cause, Cause::Mod);

    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Supplied,
        },
        &evaluated,
        &consensus,
    );
    assert_eq!(answer.cause, Some(Cause::Mod));
}

#[test]
fn a_non_mod_cause_reaches_the_answer_when_nobody_is_named() {
    let rule = json!({
        "id": "out_of_memory",
        "version": 1,
        "when": { "fact": { "kind": "core.exception", "where": { "category": "oom" } } },
        "effects": [{
            "finding": {
                "scope": "environment",
                "level": "likely",
                "cause": "memory"
            }
        }],
        "text": {
            "headline": { "en": "The game ran out of memory." },
            "why_not_sure": { "en": "A mod can cause the memory use, but the log does not say which." }
        }
    });
    let exception = Fact {
        kind: "core.exception".to_string(),
        attrs: [("category".to_string(), text_attr("oom"))]
            .into_iter()
            .collect(),
        refs: Vec::new(),
        span: None,
    };
    let packs = vec![pack("p", vec![rule])];
    let parsed = parsed_with("p", "parser", "test", vec![exception], Vec::new());
    let (evaluated, consensus) = run(
        &packs,
        std::slice::from_ref(&parsed),
        &EngineFacts::new(),
        &FakeResolver::new(),
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let answer = answer_for(
        SessionVerdict::Crashed {
            how: MatchedBy::Supplied,
        },
        &evaluated,
        &consensus,
    );

    assert!(answer.candidates.is_empty());
    assert_eq!(answer.cause, Some(Cause::Memory));
    assert_eq!(answer.findings[0].cause, Cause::Memory);
}

#[test]
fn a_change_since_the_last_good_launch_only_boosts_and_never_accuses_alone() {
    use agora_core::session_ledger::{ChangeKind, ManifestChange};

    let changes = vec![ManifestChange {
        kind: ChangeKind::Updated,
        key: Some("mod-a".to_string()),
        from_version: Some("1.0".to_string()),
        to_version: Some("2.0".to_string()),
    }];

    // A change by itself is boost-only: consensus names nobody.
    let resolver = FakeResolver::new().with("module", "modA.dll", item("mod-a", "Mod A"), None);
    let none = evaluate(&EvaluateInput {
        packs: &[],
        parsed: &[],
        engine_facts: &EngineFacts::new(),
        runtime_version: None,
        resolver: &resolver,
        inspector: &FixedInspector(Inspection::Missing),
    });
    let mut signals = none.signals.clone();
    signals.extend(agora_core::crash_engine::change::change_signals(&changes));
    let alone = consensus(
        &ConsensusInput {
            signals: &signals,
            roles: &none.roles,
            level_caps: &[],
        },
        &ConsensusProfile::default(),
    );
    assert!(alone.candidates.is_empty(), "{alone:?}");

    // The same change raises a suspect another family already named.
    let packs = vec![pack("p", vec![signal_rule("r", "execution", 10)])];
    let parsed = vec![parsed_with(
        "p",
        "parser",
        "test",
        vec![frame(0, "modA.dll")],
        Vec::new(),
    )];
    let (evaluated, base) = run(
        &packs,
        &parsed,
        &EngineFacts::new(),
        &resolver,
        &FixedInspector(Inspection::Missing),
        Some("1.0"),
    );
    let mut boosted_signals = evaluated.signals.clone();
    boosted_signals.extend(agora_core::crash_engine::change::change_signals(&changes));
    let boosted = consensus(
        &ConsensusInput {
            signals: &boosted_signals,
            roles: &evaluated.roles,
            level_caps: &[],
        },
        &ConsensusProfile::default(),
    );
    assert_eq!(
        boosted.candidates[0].owner,
        item("mod-a", "Mod A"),
        "the change kept the named suspect"
    );
    assert!(boosted.candidates[0].score > base.candidates[0].score);
    assert!(boosted.candidates[0]
        .families
        .contains(&agora_game_api::crash::Family::ChangeCorrelation));
}

#[test]
fn a_change_finding_names_what_changed_and_nothing_invents_a_change_without_one() {
    use agora_core::session_ledger::{ChangeKind, ManifestChange};

    assert!(agora_core::crash_engine::change::change_finding(&[]).is_none());
    let finding = agora_core::crash_engine::change::change_finding(&[
        ManifestChange {
            kind: ChangeKind::Added,
            key: Some("mod-b".to_string()),
            from_version: None,
            to_version: Some("1.0".to_string()),
        },
        ManifestChange {
            kind: ChangeKind::Settings,
            key: None,
            from_version: None,
            to_version: None,
        },
    ])
    .expect("a change is described");
    let headline = finding.headline.get("en").cloned().unwrap_or_default();
    assert!(
        headline.contains("This started after you changed"),
        "{headline}"
    );
    assert!(headline.contains("added mod-b"), "{headline}");
    assert!(headline.contains("settings"), "{headline}");
    assert_eq!(finding.candidate, None, "a change never accuses a mod");
}
