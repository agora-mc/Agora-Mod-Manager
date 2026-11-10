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
