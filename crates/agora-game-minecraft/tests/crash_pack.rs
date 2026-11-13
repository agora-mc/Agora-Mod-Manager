//! The built-in Minecraft crash pack and its parsers, over fixtures written for these tests.

use agora_game_api::crash::{
    lint_pack_json, validate_parser_output, ArtifactHandle, AttrValue, BuiltInParser, CodeEntry,
    CoverageState, GameHandle, LinesReply, ParseNoteKind, ParserLimits, ParserOutput,
    ParserReadError, ParserReads, ParserRequest, Scalar, Strength, CRASH_CONTRACT_VERSION,
};
use agora_game_minecraft::crash::built_in_packs;
use std::collections::BTreeSet;

struct Bytes(Vec<u8>);

impl ParserReads for Bytes {
    fn read_bytes(
        &self,
        _artifact: &str,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, ParserReadError> {
        let start = (offset as usize).min(self.0.len());
        let end = start.saturating_add(len as usize).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }

    fn read_lines(
        &self,
        _artifact: &str,
        from_line: u64,
        max_lines: u32,
    ) -> Result<LinesReply, ParserReadError> {
        let text = String::from_utf8_lossy(&self.0);
        let mut lines: Vec<String> = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                .collect()
        };
        if lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        let start = (from_line as usize).min(lines.len());
        let end = start.saturating_add(max_lines as usize).min(lines.len());
        let slice = lines[start..end].to_vec();
        Ok(LinesReply {
            lines: slice,
            next_line: (end < lines.len()).then_some(end as u64),
            cut: false,
            gap: None,
        })
    }
}

fn request() -> ParserRequest {
    ParserRequest {
        contract: CRASH_CONTRACT_VERSION,
        parser: "probe".to_string(),
        artifact: ArtifactHandle {
            id: "artifact".to_string(),
            name: "artifact.log".to_string(),
            size: 0,
            modified_at: None,
            source: None,
            children: Vec::new(),
        },
        game: GameHandle {
            id: "minecraft".to_string(),
            runtime_version: None,
        },
        limits: ParserLimits::DEFAULT,
    }
}

fn parser(name: &str) -> Box<dyn BuiltInParser> {
    let mut packs = built_in_packs();
    let pack = packs.remove(0);
    let (_, parser) = pack
        .parsers
        .into_iter()
        .find(|(supplied, _)| supplied == name)
        .unwrap_or_else(|| panic!("no parser named {name}"));
    parser
}

fn run_ok(name: &str, bytes: &[u8]) -> ParserOutput {
    let request = request();
    let output = parser(name)
        .parse(&request, &Bytes(bytes.to_vec()))
        .unwrap_or_else(|error| panic!("{name}: {}", error.message));
    let problems = validate_parser_output(&output, &request.limits);
    assert!(problems.is_empty(), "{name}: {problems:#?}");
    output
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("crash_fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

fn text_of(fact: &agora_game_api::crash::Fact, name: &str) -> String {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Text(text))) => text.clone(),
        other => panic!("`{name}` is not text: {other:?}"),
    }
}

fn number_of(fact: &agora_game_api::crash::Fact, name: &str) -> f64 {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Number(number))) => *number,
        other => panic!("`{name}` is not a number: {other:?}"),
    }
}

fn find<'a>(
    output: &'a ParserOutput,
    kind: &str,
    where_attr: &str,
    value: &str,
) -> Option<&'a agora_game_api::crash::Fact> {
    output.facts.iter().find(|fact| {
        fact.kind == kind
            && fact.attrs.get(where_attr).and_then(|value| match value {
                AttrValue::Scalar(Scalar::Text(text)) => Some(text.as_str()),
                _ => None,
            }) == Some(value)
    })
}

fn coverage_state(output: &ParserOutput, kind: &str) -> CoverageState {
    output
        .coverage
        .iter()
        .find(|coverage| coverage.kind == kind)
        .unwrap_or_else(|| panic!("no coverage for `{kind}`: {:?}", output.coverage))
        .state
}

mod crash {
    use super::*;

    #[test]
    fn the_built_in_pack_passes_the_plugin_linter() {
        let packs = built_in_packs();
        assert_eq!(packs.len(), 1);
        let pack = lint_pack_json(&packs[0].json).expect("the built-in pack lints");
        assert_eq!(pack.games, vec!["minecraft"]);
        assert!(!pack.rules.is_empty());
    }

    #[test]
    fn the_pack_declares_every_built_in_parser_it_ships() {
        let packs = built_in_packs();
        let pack = lint_pack_json(&packs[0].json).expect("the built-in pack lints");
        let declared: BTreeSet<String> = pack
            .parsers
            .iter()
            .filter_map(|parser| match &parser.entry {
                CodeEntry::BuiltIn { name } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let supplied: BTreeSet<String> = packs[0]
            .parsers
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(declared, supplied);
    }

    #[test]
    fn crash_report_fixtures_parse_for_every_loader() {
        for (file, loader) in [
            ("crash_fabric.txt", "fabric"),
            ("crash_quilt.txt", "quilt"),
            ("crash_forge.txt", "forge"),
            ("crash_neoforge.txt", "neoforge"),
        ] {
            let output = run_ok("crash_report", &fixture(file));
            assert!(
                output
                    .facts
                    .iter()
                    .any(|fact| fact.kind == "core.exception"),
                "{file}: no exception"
            );
            assert!(
                output.facts.iter().any(|fact| fact.kind == "core.frame"),
                "{file}: no frame"
            );
            assert!(
                output
                    .facts
                    .iter()
                    .any(|fact| fact.kind == "core.process"
                        && fact.attrs.contains_key("image_version")),
                "{file}: no game version"
            );
            let named = output
                .facts
                .iter()
                .find(|fact| fact.kind == "minecraft.loader")
                .unwrap_or_else(|| panic!("{file}: no loader"));
            assert_eq!(text_of(named, "id"), loader, "{file}");
            let mods: Vec<String> = output
                .facts
                .iter()
                .filter(|fact| fact.kind == "minecraft.mod")
                .map(|fact| text_of(fact, "id"))
                .collect();
            assert!(mods.iter().any(|id| id == "examplemod"), "{file}: {mods:?}");
            assert!(mods.iter().any(|id| id == "examplelib"), "{file}: {mods:?}");
            assert_eq!(
                coverage_state(&output, "minecraft.mod"),
                CoverageState::Checked,
                "{file}"
            );
        }
    }

    #[test]
    fn a_forge_frame_names_the_jar_it_came_from() {
        let output = run_ok("crash_report", &fixture("crash_forge.txt"));
        let frame = output
            .facts
            .iter()
            .find(|fact| fact.kind == "core.frame")
            .expect("a frame");
        assert!(
            frame.refs.iter().any(|reference| {
                reference.kind == "file" && reference.value == "examplemod-1.2.3.jar"
            }),
            "{:?}",
            frame.refs
        );
    }

    #[test]
    fn the_deepest_cause_frame_is_marked_as_the_top() {
        let output = run_ok("crash_report", &fixture("crash_fabric.txt"));
        let frame = output
            .facts
            .iter()
            .find(|fact| {
                fact.kind == "core.frame"
                    && fact.refs.iter().any(|reference| {
                        reference.kind == "class"
                            && reference.value == "com.example.mod.ExampleWidget"
                    })
            })
            .expect("the deepest frame");
        assert!(frame
            .refs
            .iter()
            .any(|reference| reference.kind == "class" && reference.strength == Strength::ExecTop));
    }

    #[test]
    fn a_frame_strips_module_loader_and_lambda_decoration() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
java.lang.IllegalStateException: decorated frames\n\
\tat java.base/java.lang.Thread.run(Thread.java:1)\n\
\tat app//net.example.Loader.bar(Loader.java:2)\n\
\tat knot//net.example.Knot.qux(Knot.java:3)\n\
\tat a.b.C$$Lambda$12/0x000001a2b3c4.run(Unknown Source)\n\
\tat a.b.C$$Lambda$12/991528828.apply(Unknown Source)\n\
\tat a.b.C$$Lambda/0x0000aaaa.run(Unknown Source)\n\
\tat a.b.C$Inner.run(Inner.java:4)\n\
\tat 991528828.apply(Unknown Source)\n\
";
        let output = run_ok("crash_report", report.as_bytes());
        let classes: Vec<String> = output
            .facts
            .iter()
            .filter(|fact| fact.kind == "core.frame")
            .flat_map(|fact| fact.refs.iter())
            .filter(|reference| reference.kind == "class")
            .map(|reference| reference.value.clone())
            .collect();
        for expected in [
            "java.lang.Thread",
            "net.example.Loader",
            "net.example.Knot",
            "a.b.C",
            "a.b.C$Inner",
        ] {
            assert!(classes.iter().any(|class| class == expected), "{expected}");
        }
        assert!(
            classes
                .iter()
                .all(|class| !class.chars().all(char::is_numeric)),
            "{classes:?}"
        );
        let numeric = output
            .facts
            .iter()
            .find(|fact| {
                fact.kind == "core.frame"
                    && fact.attrs.get("symbol").and_then(|value| match value {
                        AttrValue::Scalar(Scalar::Text(symbol)) => {
                            Some(symbol == "991528828.apply")
                        }
                        _ => None,
                    }) == Some(true)
            })
            .expect("the frame with no sensible class");
        assert!(
            numeric
                .refs
                .iter()
                .all(|reference| reference.kind != "class"),
            "{:?}",
            numeric.refs
        );
    }

    #[test]
    fn a_latest_log_lambda_frame_has_no_numeric_class() {
        let log = "[01:02:03] [main/INFO]: Loading Minecraft 1.20.1 with Fabric Loader 0.16.0\n\
[01:02:04] [main/ERROR]: java.lang.IllegalStateException: decorated frames\n\
\tat java.base/java.lang.Thread.run(Thread.java:1)\n\
\tat a.b.C$$Lambda$12/991528828.apply(Unknown Source)\n";
        let output = run_ok("latest_log", log.as_bytes());
        let classes: Vec<&str> = output
            .facts
            .iter()
            .filter(|fact| fact.kind == "core.frame")
            .flat_map(|fact| fact.refs.iter())
            .filter(|reference| reference.kind == "class")
            .map(|reference| reference.value.as_str())
            .collect();
        assert!(classes.contains(&"java.lang.Thread"), "{classes:?}");
        assert!(classes.contains(&"a.b.C"), "{classes:?}");
        assert!(
            classes
                .iter()
                .all(|class| !class.chars().all(char::is_numeric)),
            "{classes:?}"
        );
    }

    #[test]
    fn windows_and_unix_line_endings_parse_the_same() {
        let lf = fixture("crash_fabric.txt");
        let crlf = String::from_utf8(lf.clone())
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes();
        let unix = run_ok("crash_report", &lf);
        let windows = run_ok("crash_report", &crlf);
        assert_eq!(unix.facts, windows.facts);
    }

    #[test]
    fn empty_input_is_an_error_and_not_a_clean_parse() {
        for name in ["crash_report", "latest_log", "hs_err"] {
            let request = request();
            let error = parser(name)
                .parse(&request, &Bytes(Vec::new()))
                .expect_err(name);
            assert!(error.message.contains("empty"), "{name}: {}", error.message);
        }
    }

    #[test]
    fn a_lookalike_is_wrong_format_and_not_a_crash() {
        let output = run_ok("crash_report", &fixture("lookalike.txt"));
        assert!(output.facts.is_empty());
        assert!(output
            .notes
            .iter()
            .any(|note| note.kind == ParseNoteKind::WrongFormat));
    }

    #[test]
    fn a_truncated_report_says_the_mod_list_is_absent() {
        let output = run_ok("crash_report", &fixture("truncated.txt"));
        assert!(output
            .facts
            .iter()
            .any(|fact| fact.kind == "core.exception"));
        assert_eq!(
            coverage_state(&output, "minecraft.mod"),
            CoverageState::Absent
        );
        let coverage = output
            .coverage
            .iter()
            .find(|coverage| coverage.kind == "minecraft.mod")
            .unwrap();
        assert!(coverage.why.as_deref().is_some_and(|why| !why.is_empty()));
    }

    #[test]
    fn a_missing_dependency_names_the_missing_mod() {
        let output = run_ok("latest_log", &fixture("latest_fabric_missing.log"));
        let problem =
            find(&output, "core.dependency_problem", "state", "absent").expect("a problem");
        assert_eq!(text_of(problem, "needed_by"), "examplemod");
        assert_eq!(problem.refs[0].kind, "mod_id");
        assert_eq!(problem.refs[0].value, "examplelib");
    }

    #[test]
    fn a_recommendation_is_not_a_missing_dependency() {
        let output = run_ok("latest_log", &fixture("latest_fabric_recommends.log"));
        assert!(!output
            .facts
            .iter()
            .any(|fact| fact.kind == "core.dependency_problem"));
    }

    #[test]
    fn a_wrong_dependency_version_is_reported_as_such() {
        let output = run_ok("latest_log", &fixture("latest_fabric_wrong_version.log"));
        let problem =
            find(&output, "core.dependency_problem", "state", "wrong_version").expect("a problem");
        assert_eq!(problem.refs[0].value, "examplelib");
        assert!(text_of(problem, "requirement").contains("2.0.0"));
    }

    #[test]
    fn an_incompatible_dependency_is_a_loader_problem() {
        let output = run_ok("latest_log", &fixture("latest_fabric_incompatible.log"));
        let problem =
            find(&output, "minecraft.loader_problem", "kind", "incompatible").expect("a problem");
        assert_eq!(problem.refs[0].value, "examplelib");
        assert!(!output
            .facts
            .iter()
            .any(|fact| fact.kind == "core.dependency_problem"));
    }

    #[test]
    fn a_forge_missing_dependency_names_both_mods() {
        let output = run_ok("latest_log", &fixture("latest_forge_missing.log"));
        let problem =
            find(&output, "core.dependency_problem", "state", "absent").expect("a problem");
        assert_eq!(text_of(problem, "needed_by"), "examplemod");
        assert_eq!(problem.refs[0].value, "examplelib");
        let loader = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.loader")
            .expect("a loader");
        assert_eq!(text_of(loader, "id"), "neoforge");
    }

    #[test]
    fn a_crash_report_reads_the_fabric_unmet_dependency_listing() {
        let output = run_ok(
            "crash_report",
            &fixture("crash_fabric_missing_dependency.txt"),
        );
        let first = find(
            &output,
            "core.dependency_problem",
            "needed_by",
            "examplemod",
        )
        .expect("a problem");
        assert_eq!(text_of(first, "state"), "absent");
        assert_eq!(text_of(first, "requirement"), "any version");
        assert!(first
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplelib"));
        let second = find(
            &output,
            "core.dependency_problem",
            "needed_by",
            "exampletools",
        )
        .expect("a problem");
        assert_eq!(text_of(second, "requirement"), "version 2.0.0 or later");
        assert!(second
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplelib"));
        assert_eq!(
            output
                .facts
                .iter()
                .filter(|fact| fact.kind == "core.dependency_problem")
                .count(),
            2,
            "the install suggestion is not a dependency of its own"
        );
        assert!(find(&output, "minecraft.loader_problem", "kind", "resolution").is_some());
        assert_eq!(
            coverage_state(&output, "minecraft.loader_problem"),
            CoverageState::Checked
        );
    }

    #[test]
    fn a_crash_report_reads_a_loader_incompatible_failure() {
        let output = run_ok("crash_report", &fixture("crash_fabric_incompatible.txt"));
        assert!(find(&output, "minecraft.loader_problem", "kind", "incompatible").is_some());
    }

    #[test]
    fn a_mixin_failure_names_the_config_target_and_mod() {
        let output = run_ok("latest_log", &fixture("latest_mixin.log"));
        let failure = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.mixin_failure")
            .expect("a mixin failure");
        assert_eq!(text_of(failure, "config"), "examplemod.mixins.json");
        assert_eq!(text_of(failure, "target"), "net.minecraft.class_123");
        assert_eq!(failure.refs[0].kind, "mod_id");
        assert_eq!(failure.refs[0].value, "examplemod");
    }

    #[test]
    fn a_duplicate_mod_is_reported_with_its_id() {
        let output = run_ok("latest_log", &fixture("latest_duplicate.log"));
        let duplicate = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.duplicate_mod")
            .expect("a duplicate");
        assert_eq!(duplicate.refs[0].value, "examplemod");
    }

    #[test]
    fn out_of_memory_is_its_own_exception_fact() {
        let output = run_ok("latest_log", &fixture("latest_oom.log"));
        let oom = find(&output, "core.exception", "category", "oom").expect("an oom fact");
        assert_eq!(text_of(oom, "class"), "java.lang.OutOfMemoryError");
        assert!(output.facts.iter().any(|fact| fact.kind == "core.frame"));
    }

    #[test]
    fn a_mod_error_config_error_and_world_error_are_reported() {
        let mod_error = run_ok("latest_log", &fixture("latest_mod_error.log"));
        assert!(mod_error
            .facts
            .iter()
            .any(|fact| fact.kind == "minecraft.mod_error"));

        let config = run_ok("latest_log", &fixture("latest_config.log"));
        assert!(config
            .facts
            .iter()
            .any(|fact| fact.kind == "minecraft.config_error"));

        let world = run_ok("latest_log", &fixture("latest_world.log"));
        assert!(world
            .facts
            .iter()
            .any(|fact| fact.kind == "minecraft.world_error"));
    }

    #[test]
    fn a_forge_failure_message_names_the_mod_that_failed() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Time: 2026-01-02 03:04:05\n\
Description: Mod loading error has occurred\n\
\n\
java.lang.Exception: Mod loading failed\n\
\n\
\tMod File: /mods/examplemod.jar\n\
\tFailure message: Example Mod (examplemod) has failed to load correctly\n\
\tMod Version: 1.2.3\n\
\tException message: java.lang.IllegalStateException: nope\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let crash = find(&output, "minecraft.mod_crash", "id", "examplemod").expect("a mod crash");
        assert_eq!(text_of(crash, "phase"), "load");
        assert_eq!(number_of(crash, "order"), 0.0);
        assert_eq!(text_of(crash, "name"), "Example Mod");
        assert!(crash
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplemod"));
    }

    #[test]
    fn a_fabric_entrypoint_failure_names_the_mod() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: Initializing game\n\
\n\
java.lang.RuntimeException: Could not execute entrypoint stage 'main' due to errors, provided by 'examplemod'!\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let crash = find(&output, "minecraft.mod_crash", "id", "examplemod").expect("a mod crash");
        assert_eq!(text_of(crash, "phase"), "entrypoint");
    }

    #[test]
    fn a_crash_report_mixin_failure_names_its_owner() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: Initializing game\n\
\n\
org.spongepowered.asm.mixin.transformer.throwables.MixinTransformerError: An unexpected critical error was encountered\n\
Caused by: org.spongepowered.asm.mixin.throwables.MixinApplyError: Mixin [examplemod.mixins.json:ExampleMixin from mod examplemod] from phase [DEFAULT] in config [examplemod.mixins.json] FAILED during APPLY\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let failure = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.mixin_failure")
            .expect("a mixin failure");
        assert_eq!(text_of(failure, "config"), "examplemod.mixins.json");
        assert_eq!(failure.refs[0].kind, "mod_id");
        assert_eq!(failure.refs[0].value, "examplemod");
    }

    #[test]
    fn a_mixin_that_lost_its_target_to_another_transformer_is_not_blamed() {
        let report = "---- Minecraft Crash Report ----\n\
Description: Initializing game\n\
\n\
org.spongepowered.asm.mixin.transformer.throwables.MixinTransformerError: An unexpected critical error was encountered\n\
Caused by: org.spongepowered.asm.mixin.throwables.MixinApplyError: Mixin [examplemod.mixins.json:ExampleMixin from mod examplemod] from phase [DEFAULT] in config [examplemod.mixins.json] FAILED during APPLY\n\
Caused by: org.spongepowered.asm.mixin.injection.throwables.InvalidInjectionException: target was removed by another injector [INJECT Applicator Phase]\n";
        let output = run_ok("crash_report", report.as_bytes());
        assert!(!output
            .facts
            .iter()
            .any(|fact| fact.kind == "minecraft.mixin_failure"));
    }

    #[test]
    fn a_mixin_failure_carries_the_config_and_the_target_as_refs() {
        let log = "[01:02:03] [main/ERROR]: Mixin apply for mod examplemod failed \
examplemod.mixins.json:ExampleMixin -> com.other.mod.TargetScreen: boom\n";
        let output = run_ok("latest_log", log.as_bytes());
        let failure = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.mixin_failure")
            .expect("a mixin failure");
        assert_eq!(text_of(failure, "config"), "examplemod.mixins.json");
        assert_eq!(text_of(failure, "target"), "com.other.mod.TargetScreen");
        assert!(failure
            .refs
            .iter()
            .any(|reference| reference.kind == "mixin_config"
                && reference.value == "examplemod.mixins.json"));
        assert!(failure
            .refs
            .iter()
            .any(|reference| reference.kind == "class"
                && reference.value == "com.other.mod.TargetScreen"));
    }

    #[test]
    fn a_target_already_loaded_names_two_mods_but_a_platform_target_names_one() {
        let two_mods =
            "[01:02:03] [main/FATAL]: Mixin prepare for mod examplemod failed preparing \
ExampleMixin in examplemod.mixins.json: Critical problem: examplemod.mixins.json:ExampleMixin \
from mod examplemod target com.other.mod.TargetScreen was loaded too early.\n";
        let output = run_ok("latest_log", two_mods.as_bytes());
        let failure = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.mixin_failure")
            .expect("a mixin failure");
        assert_eq!(text_of(failure, "target"), "com.other.mod.TargetScreen");
        assert!(failure
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplemod"));

        let platform =
            "[01:02:03] [main/FATAL]: Mixin prepare for mod examplemod failed preparing \
ExampleMixin in examplemod.mixins.json: Critical problem: examplemod.mixins.json:ExampleMixin \
from mod examplemod target net.minecraft.world.level.LevelReader was loaded too early.\n";
        let output = run_ok("latest_log", platform.as_bytes());
        assert!(!output
            .facts
            .iter()
            .any(|fact| fact.kind == "minecraft.mixin_failure"));
    }

    #[test]
    fn a_loader_exception_names_the_missing_mod_and_the_mod_that_needs_it() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: There was a severe problem during mod loading\n\
\n\
cpw.mods.fml.common.LoaderException: java.lang.NoClassDefFoundError: Lexampletardis/common/items/WidgetItem;\n\
\tat cpw.mods.fml.common.LoadController.transition(LoadController.java:1)\n\
Caused by: java.lang.NoClassDefFoundError: Lexampletardis/common/items/WidgetItem;\n\
Caused by: java.lang.ClassNotFoundException: exampletardis.common.items.WidgetItem\n\
Caused by: java.lang.NoClassDefFoundError: examplecraft/api/tools/ITool\n\
Caused by: java.lang.ClassNotFoundException: examplecraft.api.tools.ITool\n\
\n\
A detailed walkthrough of the error:\n\
-- System Details --\n\
\tMinecraft Version: 1.7.10\n";
        let output = run_ok("crash_report", report.as_bytes());
        let problem = find(&output, "core.dependency_problem", "state", "absent")
            .expect("a dependency problem");
        assert_eq!(text_of(problem, "needed_by"), "exampletardis");
        assert!(problem
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplecraft"));
        assert!(problem
            .refs
            .iter()
            .any(|reference| reference.kind == "needed_by" && reference.value == "exampletardis"));
    }

    #[test]
    fn a_mixin_error_names_the_class_in_its_message() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: Initializing game\n\
\n\
org.spongepowered.asm.mixin.transformer.throwables.MixinTransformerError: broken\n\
Caused by: org.spongepowered.asm.mixin.throwables.ClassMetadataNotFoundException: com.examplehidden.config.ThingConfig\n\
\n\
A detailed walkthrough of the error:\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let exception = find(
            &output,
            "core.exception",
            "class",
            "org.spongepowered.asm.mixin.throwables.ClassMetadataNotFoundException",
        )
        .expect("the mixin exception");
        assert!(exception.refs.iter().any(|reference| {
            reference.kind == "named_class"
                && reference.value == "com.examplehidden.config.ThingConfig"
        }));
    }

    #[test]
    fn a_missing_member_names_the_provider_class() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: Rendering entity in world\n\
\n\
java.lang.AbstractMethodError: Receiver class com.example.app.Model does not define or inherit \
an implementation of the resolved method 'abstract com.example.api.ProvidedThing root()' of \
abstract class com.example.api.AbstractThing.\n\
\n\
A detailed walkthrough of the error:\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let exception = find(
            &output,
            "core.exception",
            "class",
            "java.lang.AbstractMethodError",
        )
        .expect("the abstract method error");
        assert!(exception
            .refs
            .iter()
            .any(|reference| reference.kind == "named_class"
                && reference.value == "com.example.api.AbstractThing"));
    }

    #[test]
    fn a_fatal_json_error_names_the_mods_data_namespace() {
        let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\
\n\
Description: Initializing game\n\
\n\
java.lang.RuntimeException: could not reload\n\
Caused by: com.google.gson.JsonSyntaxException: Failed to read data/examplemod/recipes/thing.json\n\
\n\
A detailed walkthrough of the error:\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let problem =
            find(&output, "minecraft.data_error", "namespace", "examplemod").expect("a data error");
        assert!(problem
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplemod"));
    }

    #[test]
    fn unsupported_class_version_records_the_required_java() {
        let report = "---- Minecraft Crash Report ----\n\
Description: Initializing game\n\
\n\
java.lang.UnsupportedClassVersionError: com/example/Thing has been compiled by a more recent version of the Java Runtime (class file version 61.0), this version of the Java Runtime only recognizes class file versions up to 52.0\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let exception = find(&output, "core.exception", "category", "load_failure")
            .expect("a load failure exception");
        assert_eq!(
            text_of(exception, "class"),
            "java.lang.UnsupportedClassVersionError"
        );
        let problem = find(&output, "minecraft.jvm_problem", "kind", "class_version")
            .expect("a class-version problem");
        assert_eq!(number_of(problem, "required_java"), 17.0);
    }

    #[test]
    fn a_ticking_entity_report_names_the_type_and_location() {
        let report = "---- Minecraft Crash Report ----\n\
Description: Ticking entity\n\
\n\
java.lang.IllegalStateException: the entity failed\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- Entity being ticked --\n\
Details:\n\
\tEntity Type: examplemod:example_entity\n\
\tEntity Name: Example\n\
\tEntity's Exact location: 1.5, 64.0, -2.5\n\
\tLevel dimension: minecraft:overworld\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let object =
            find(&output, "minecraft.ticking_object", "kind", "entity").expect("a ticking entity");
        assert_eq!(text_of(object, "type"), "examplemod:example_entity");
        assert_eq!(text_of(object, "name"), "Example");
        assert_eq!(text_of(object, "dimension"), "minecraft:overworld");
        assert_eq!(number_of(object, "x"), 1.5);
        assert_eq!(number_of(object, "y"), 64.0);
        assert_eq!(number_of(object, "z"), -2.5);
        assert!(object
            .refs
            .iter()
            .any(|reference| reference.kind == "mod_id" && reference.value == "examplemod"));
    }

    #[test]
    fn a_ticking_block_entity_report_names_the_block() {
        let report = "---- Minecraft Crash Report ----\n\
Description: Ticking block entity\n\
\n\
java.lang.IllegalStateException: the block failed\n\
\n\
A detailed walkthrough of the error:\n\
\n\
-- Block entity being ticked --\n\
Details:\n\
\tBlock type: examplemod:example_block\n\
\tBlock location: World: (3, 70, 4), Chunk: (0, 4, 0)\n\
\n\
-- System Details --\n\
\tMinecraft Version: 1.20.1\n";
        let output = run_ok("crash_report", report.as_bytes());
        let object = find(&output, "minecraft.ticking_object", "kind", "block_entity")
            .expect("a ticking block entity");
        assert_eq!(text_of(object, "type"), "examplemod:example_block");
        assert_eq!(number_of(object, "x"), 3.0);
        assert_eq!(number_of(object, "y"), 70.0);
        assert_eq!(number_of(object, "z"), 4.0);
    }

    #[test]
    fn loader_lines_that_name_a_failed_mod_are_their_own_fact() {
        let create = "[01:02:03] [main/ERROR]: Failed to create mod instance. ModID: examplemod, for mod Example Mod\n";
        let output = run_ok("latest_log", create.as_bytes());
        let crash = find(&output, "minecraft.mod_crash", "id", "examplemod").expect("a mod crash");
        assert_eq!(text_of(crash, "phase"), "load");
        assert_eq!(number_of(crash, "order"), 0.0);

        let caught =
            "[01:02:03] [main/ERROR]: LoaderExceptionModCrash: Caught exception from Example Mod (othermod)\n";
        let output = run_ok("latest_log", caught.as_bytes());
        assert!(find(&output, "minecraft.mod_crash", "id", "othermod").is_some());
    }

    #[test]
    fn duplicate_mod_lines_from_the_loader_are_recognised() {
        for line in [
            "Mod ID: 'examplemod' from mod files: one.jar, two.jar\n",
            "java.lang.RuntimeException: Duplicate versions for mod ID 'examplemod': [1.2.3 at /mods/examplemod.jar]\n",
        ] {
            let output = run_ok("latest_log", line.as_bytes());
            let duplicate = find(&output, "minecraft.duplicate_mod", "id", "examplemod")
                .unwrap_or_else(|| panic!("no duplicate for {line}"));
            assert_eq!(duplicate.refs[0].value, "examplemod");
        }
    }

    #[test]
    fn a_named_conflict_carries_both_mods() {
        let output = run_ok(
            "latest_log",
            b"[01:02:03] [main/ERROR]: Found conflicting mods: examplemod conflicts with othermod\n",
        );
        let conflict = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.mod_conflict")
            .expect("a conflict");
        assert_eq!(text_of(conflict, "first"), "examplemod");
        assert_eq!(text_of(conflict, "second"), "othermod");
        assert_eq!(conflict.refs.len(), 2);
    }

    #[test]
    fn a_world_saved_with_a_missing_or_changed_mod_is_reported() {
        let missing = run_ok(
            "latest_log",
            b"[01:02:03] [main/WARN]: This world was saved with mod examplemod which appears to be missing, things may not work well\n",
        );
        let fact = find(&missing, "minecraft.world_mod", "kind", "missing").expect("a missing mod");
        assert_eq!(text_of(fact, "mod"), "examplemod");

        let changed = run_ok(
            "latest_log",
            b"[01:02:03] [main/WARN]: This world was saved with mod examplemod version 1.0 and it is now at version 2.0, things may not work well\n",
        );
        let fact = find(&changed, "minecraft.world_mod", "kind", "version").expect("a version");
        assert_eq!(text_of(fact, "expected"), "1.0");
        assert_eq!(text_of(fact, "current"), "2.0");
    }

    #[test]
    fn a_jvm_problem_is_recorded_without_naming_a_mod() {
        let missing = run_ok(
            "latest_log",
            b"[01:02:03] [main/ERROR]: java.lang.UnsatisfiedLinkError: Failed to locate library: examplegfx\n",
        );
        assert!(find(&missing, "minecraft.jvm_problem", "kind", "native_library").is_some());

        let memory = run_ok(
            "latest_log",
            b"[01:02:03] [main/ERROR]: # There is insufficient memory for the Java Runtime Environment to continue.\n",
        );
        assert!(find(&memory, "minecraft.jvm_problem", "kind", "memory_startup").is_some());

        let ids = run_ok(
            "latest_log",
            b"[01:02:03] [main/ERROR]: maximum id range exceeded\n",
        );
        assert!(find(&ids, "minecraft.loader_problem", "kind", "id_limit").is_some());
    }

    #[test]
    fn an_existing_file_is_reported_so_the_answer_can_name_it() {
        let output = run_ok(
            "latest_log",
            b"[01:02:03] [main/ERROR]: java.nio.file.FileAlreadyExistsException: C:\\\\games\\\\config\\\\examplemod.toml.old\n",
        );
        let problem = find(&output, "minecraft.file_problem", "kind", "already_exists")
            .expect("a file problem");
        assert_eq!(text_of(problem, "path"), "examplemod.toml.old");
        assert_eq!(problem.refs[0].kind, "file");
        assert_eq!(problem.refs[0].value, "examplemod.toml.old");
    }

    #[test]
    fn the_hs_err_file_names_the_signal_frame_and_library() {
        let output = run_ok("hs_err", &fixture("hs_err_sample.log"));
        let exception = output
            .facts
            .iter()
            .find(|fact| fact.kind == "core.exception")
            .expect("an exception");
        assert_eq!(text_of(exception, "class"), "EXCEPTION_ACCESS_VIOLATION");
        assert_eq!(text_of(exception, "category"), "access_violation");
        assert_eq!(text_of(exception, "fault_address"), "0x0000000031e41000");
        let frame = output
            .facts
            .iter()
            .find(|fact| fact.kind == "core.frame")
            .expect("a frame");
        assert!(frame
            .refs
            .iter()
            .any(|reference| reference.kind == "module" && reference.value == "examplegfx.dll"));
        let module = output
            .facts
            .iter()
            .find(|fact| fact.kind == "core.module")
            .expect("a module");
        assert_eq!(module.refs[0].value, "examplegfx.dll");
        let system = output
            .facts
            .iter()
            .find(|fact| fact.kind == "minecraft.system")
            .expect("a system fact");
        assert!(text_of(system, "java_version").contains("Example Runtime"));
        let process = output
            .facts
            .iter()
            .find(|fact| fact.kind == "core.process")
            .expect("a process fact");
        assert_eq!(number_of(process, "pid"), 2216.0);
    }
}

mod captured {
    use super::*;

    #[test]
    fn captured_output_is_stripped_then_read_as_a_log() {
        let output = run_ok("captured_launch", &fixture("captured_oom.txt"));
        assert_eq!(output.producer.format, "minecraft_latest_log");
        assert!(
            find(&output, "core.exception", "category", "oom").is_some(),
            "{:?}",
            output.facts
        );
    }

    #[test]
    fn captured_crash_report_is_read_as_a_crash_report() {
        let output = run_ok("captured_launch", &fixture("captured_crash.txt"));
        assert_eq!(output.producer.format, "minecraft_crash_report");
        assert!(output
            .facts
            .iter()
            .any(|fact| fact.kind == "core.exception"));
    }

    #[test]
    fn captured_output_with_no_lines_is_not_a_clean_bill() {
        let request = request();
        let error = parser("captured_launch")
            .parse(
                &request,
                &Bytes(b"# Agora captured Java output\n# exit_code=1\n".to_vec()),
            )
            .expect_err("nothing to read");
        assert!(error.message.contains("no lines"), "{error:?}");
    }
}

mod gate {
    use super::*;

    fn text(name: &str) -> String {
        String::from_utf8(fixture(name)).expect("fixture is text")
    }

    #[test]
    fn a_known_crash_in_captured_output_sets_the_launch_flag() {
        assert!(agora_game_minecraft::crash::captured_output_names_a_crash(
            &text("captured_oom.txt")
        ));
    }

    #[test]
    fn a_known_crash_in_a_log_tail_sets_the_launch_flag() {
        let log = "[01:02:03] [main/INFO]: Building unoptimized datafixer\n\
                   [01:02:04] [main/ERROR]: java.lang.OutOfMemoryError: Java heap space";
        assert!(agora_game_minecraft::crash::log_text_names_a_crash(log));
    }

    #[test]
    fn ordinary_launch_output_does_not_set_the_launch_flag() {
        let ordinary =
            "[stdout] [01:02:03] [main/INFO]: Loading Minecraft 1.21 with Fabric Loader 0.16.0\n\
                        [stdout] [01:02:04] [main/INFO]: Done.";
        assert!(!agora_game_minecraft::crash::captured_output_names_a_crash(
            ordinary
        ));
        assert!(!agora_game_minecraft::crash::captured_output_names_a_crash(
            ""
        ));
    }
}

mod actions {
    use super::*;
    use agora_game_api::crash::ActionKind;

    fn rules() -> Vec<agora_game_api::crash::Rule> {
        let packs = built_in_packs();
        lint_pack_json(&packs[0].json)
            .expect("the built-in pack lints")
            .rules
    }

    fn actions(rule_id: &str) -> Vec<agora_game_api::crash::ActionRequest> {
        rules()
            .into_iter()
            .find(|rule| rule.id == rule_id)
            .unwrap_or_else(|| panic!("no rule `{rule_id}`"))
            .actions
    }

    #[test]
    fn the_old_signature_buttons_are_offered_by_the_matching_rules() {
        assert!(actions("missing_fabric_api").iter().any(|action| {
            action.kind == ActionKind::InstallContent
                && action.content_key.as_deref() == Some("fabric-api")
        }));
        for rule in ["missing_dependency", "mod_resolution_failed"] {
            assert!(
                actions(rule)
                    .iter()
                    .any(|action| action.kind == ActionKind::ReviewMods),
                "{rule}"
            );
        }
        assert!(actions("out_of_memory")
            .iter()
            .any(|action| action.kind == ActionKind::OpenJvmSettings));
        for rule in ["mixin_apply_failed", "mixin_failed_crash_report"] {
            assert!(
                actions(rule)
                    .iter()
                    .any(|action| action.kind == ActionKind::OpenCrashLog),
                "{rule}"
            );
        }
        assert!(actions("duplicate_classpath_libraries")
            .iter()
            .any(|action| action.kind == ActionKind::OpenConsole));
    }
}
