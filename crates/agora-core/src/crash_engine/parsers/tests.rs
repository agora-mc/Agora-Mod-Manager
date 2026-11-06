//! Hostile tests for the generic parsers. Every dump is written from the documented structures in
//! this file; no real crash dump is committed.

use crate::crash_packs::{
    artifact_handle, load_plugin_packs, parse_artifact, parser_request, ArtifactParse,
    CrashPackReport, MemoryArtifact, ParserDispatch,
};
use agora_game_api::crash::{
    AttrValue, CoverageState, CrashPack, Fact, GameHandle, ParseNoteKind, ParserLimits,
    ParserOutput, Scalar, Strength, CRASH_CONTRACT_VERSION,
};
use agora_plugin_api::manifest::PluginManifest;
use agora_plugin_host::QuickJsHost;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn run_generic(name: &str, config: Value, bytes: &[u8], limits: ParserLimits) -> ArtifactParse {
    let host = QuickJsHost::new();
    let pack: CrashPack = serde_json::from_value(json!({
        "contract": CRASH_CONTRACT_VERSION,
        "parsers": [{
            "id": "probe",
            "entry": { "generic": { "name": name, "config": config } },
        }],
    }))
    .unwrap();
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("artifact", bytes.to_vec());
    let request = parser_request(
        "probe",
        artifact_handle("artifact", "artifact.bin", bytes.len() as u64, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        limits,
    );
    parse_artifact(
        &host,
        "acme.test",
        &pack,
        &request,
        Arc::new(artifact),
        Some("probe"),
        b"",
    )
}

fn output(name: &str, config: Value, bytes: &[u8], limits: ParserLimits) -> ParserOutput {
    let attempt = run_generic(name, config, bytes, limits);
    attempt
        .parsed()
        .cloned()
        .unwrap_or_else(|| panic!("expected a parsed output, got {attempt:?}"))
}

fn only_outcome(attempt: &ArtifactParse) -> &ParserDispatch {
    assert_eq!(attempt.attempts.len(), 1);
    &attempt.attempts[0].outcome
}

fn report_for(pack: &Value) -> CrashPackReport {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash/pack.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(pack).unwrap()).unwrap();
    let manifest = PluginManifest::parse(
        &json!({
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
    load_plugin_packs("acme.test", dir.path(), &manifest)
}

fn text_config() -> Value {
    json!({
        "producer": { "format": "ironfen_log" },
        "sections": [{
            "id": "errors",
            "header": "^== ERRORS ==$",
            "kind": "ironfen.script_error",
            "absent_why": "This log has no errors section.",
            "lines": [{
                "pattern": "^(?P<category>\\w+): (?P<module>\\S+)$",
                "kind": "core.exception",
                "attrs": { "category": "category" },
                "refs": [{ "kind": "module", "value": "module", "strength": "exec" }]
            }]
        }]
    })
}

fn text_of(fact: &Fact, name: &str) -> String {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Text(text))) => text.clone(),
        other => panic!("`{name}` is not text: {other:?}"),
    }
}

fn number_of(fact: &Fact, name: &str) -> f64 {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Number(number))) => *number,
        other => panic!("`{name}` is not a number: {other:?}"),
    }
}

fn bool_of(fact: &Fact, name: &str) -> bool {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Bool(value))) => *value,
        other => panic!("`{name}` is not a bool: {other:?}"),
    }
}

fn kind_state(output: &ParserOutput, kind: &str) -> CoverageState {
    output
        .coverage
        .iter()
        .find(|coverage| coverage.kind == kind)
        .unwrap_or_else(|| panic!("no coverage for `{kind}`: {:?}", output.coverage))
        .state
}

// ---------------------------------------------------------------------------
// text_sections
// ---------------------------------------------------------------------------

#[test]
fn text_sections_empty_file_has_absent_coverage() {
    let out = output("text_sections", text_config(), b"", ParserLimits::DEFAULT);
    assert!(out.facts.is_empty());
    assert_eq!(kind_state(&out, "core.exception"), CoverageState::Absent);
    assert_eq!(
        kind_state(&out, "ironfen.script_error"),
        CoverageState::Absent
    );
}

#[test]
fn text_sections_invalid_utf8_does_not_panic() {
    let bytes = [0xff, 0xfe, b'a', b'\n', b'b'];
    let out = output(
        "text_sections",
        text_config(),
        &bytes,
        ParserLimits::DEFAULT,
    );
    assert!(out.facts.is_empty());
}

#[test]
fn text_sections_crlf_and_lf_agree() {
    let lf = b"== ERRORS ==\naccess_violation: mymod.dll\n";
    let crlf = b"== ERRORS ==\r\naccess_violation: mymod.dll\r\n";
    let a = output("text_sections", text_config(), lf, ParserLimits::DEFAULT);
    let b = output("text_sections", text_config(), crlf, ParserLimits::DEFAULT);
    assert_eq!(a.facts, b.facts);
    assert_eq!(a.facts.len(), 1);
}

#[test]
fn text_sections_ten_megabyte_line_is_cut_not_allocated() {
    let bytes = vec![b'a'; 10 * 1024 * 1024];
    let limits = ParserLimits {
        max_line_len: 256,
        ..ParserLimits::DEFAULT
    };
    let started = Instant::now();
    let out = output("text_sections", text_config(), &bytes, limits);
    let elapsed = started.elapsed();
    assert!(out.facts.is_empty());
    assert!(
        elapsed < Duration::from_secs(5),
        "the 10 MB line took {elapsed:?}"
    );
}

#[test]
fn text_sections_catastrophic_regex_is_linear() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"== ERRORS ==\n");
    bytes.extend(std::iter::repeat_n(b'a', 200_000));
    bytes.extend_from_slice(b"!\n");
    let config = json!({
        "sections": [{
            "id": "errors",
            "header": "^== ERRORS ==$",
            "lines": [{ "pattern": "^(a+)+$", "kind": "core.log_line" }]
        }]
    });
    // Keep the `!` inside the line: a cut line would end in `a`s and match after all.
    let limits = ParserLimits {
        max_line_len: 300_000,
        ..ParserLimits::DEFAULT
    };
    let started = Instant::now();
    let out = output("text_sections", config, &bytes, limits);
    let elapsed = started.elapsed();
    assert!(out.facts.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "the regex took {elapsed:?}"
    );
}

#[test]
fn text_sections_invalid_regex_is_a_pack_error() {
    let config = json!({
        "sections": [{ "id": "errors", "header": "(unclosed" }]
    });
    assert!(super::validate_config("text_sections", &config).is_err());
    let report = report_for(&json!({
        "contract": CRASH_CONTRACT_VERSION,
        "parsers": [{
            "id": "probe",
            "entry": { "generic": { "name": "text_sections", "config": config } }
        }]
    }));
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "parser_config");
    assert!(report.problems[0].at.contains("probe"), "{report:?}");
}

#[test]
fn text_sections_capture_group_must_exist() {
    let config = json!({
        "sections": [{
            "id": "errors",
            "header": "^== ERRORS ==$",
            "lines": [{
                "pattern": "^(?P<category>\\w+)$",
                "kind": "core.exception",
                "attrs": { "message": "missing_group" }
            }]
        }]
    });
    let error = super::validate_config("text_sections", &config).unwrap_err();
    assert!(error.contains("missing_group"), "{error}");
}

#[test]
fn text_sections_header_twice_reads_both_bodies() {
    let log = b"== ERRORS ==\naccess_violation: first.dll\n== ERRORS ==\noom: second.dll\n";
    let out = output("text_sections", text_config(), log, ParserLimits::DEFAULT);
    assert_eq!(out.facts.len(), 2);
    let lines: Vec<u64> = out
        .facts
        .iter()
        .filter_map(|fact| fact.span.as_ref().and_then(|span| span.line_start))
        .collect();
    assert_eq!(lines, vec![2, 4]);
}

#[test]
fn text_sections_header_with_no_body_is_checked() {
    let out = output(
        "text_sections",
        text_config(),
        b"== ERRORS ==\n",
        ParserLimits::DEFAULT,
    );
    assert!(out.facts.is_empty());
    assert_eq!(kind_state(&out, "core.exception"), CoverageState::Checked);
}

#[test]
fn text_sections_too_many_facts_is_an_error_naming_the_parser() {
    let mut log = Vec::new();
    log.extend_from_slice(b"== ERRORS ==\n");
    for index in 0..10 {
        log.extend_from_slice(format!("A: mod{index}.dll\n").as_bytes());
    }
    let limits = ParserLimits {
        max_facts: 2,
        ..ParserLimits::DEFAULT
    };
    let attempt = run_generic("text_sections", text_config(), &log, limits);
    match only_outcome(&attempt) {
        ParserDispatch::Failed(failure) => {
            assert_eq!(
                failure.kind,
                crate::crash_packs::ParserFailureKind::InvalidOutput
            );
            assert_eq!(failure.parser_id, "probe");
            assert!(failure.message.contains("text_sections"), "{failure}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn text_sections_absent_section_is_absent_coverage() {
    let out = output(
        "text_sections",
        text_config(),
        b"an ordinary log line\n",
        ParserLimits::DEFAULT,
    );
    let coverage = out
        .coverage
        .iter()
        .find(|coverage| coverage.kind == "core.exception")
        .unwrap();
    assert_eq!(coverage.state, CoverageState::Absent);
    assert_eq!(
        coverage.why.as_deref(),
        Some("This log has no errors section.")
    );
}

#[test]
fn text_sections_maps_attrs_and_refs_with_line_span() {
    let out = output(
        "text_sections",
        text_config(),
        b"== ERRORS ==\naccess_violation: mymod.dll\n",
        ParserLimits::DEFAULT,
    );
    assert_eq!(out.facts.len(), 1);
    let fact = &out.facts[0];
    assert_eq!(fact.kind, "core.exception");
    assert_eq!(text_of(fact, "category"), "access_violation");
    assert_eq!(fact.refs.len(), 1);
    assert_eq!(fact.refs[0].kind, "module");
    assert_eq!(fact.refs[0].value, "mymod.dll");
    assert_eq!(fact.refs[0].strength, Strength::Exec);
    let span = fact.span.as_ref().unwrap();
    assert_eq!(span.section.as_deref(), Some("errors"));
    assert_eq!(span.line_start, Some(2));
    assert_eq!(span.line_end, Some(2));
}

// ---------------------------------------------------------------------------
// key_value
// ---------------------------------------------------------------------------

fn kv_pattern_config() -> Value {
    json!({
        "producer": { "format": "ini" },
        "keys": [{
            "key": "Exception",
            "kind": "core.exception",
            "value_pattern": "^(?P<category>\\w+) (?P<address>0x[0-9a-fA-F]+)$",
            "attrs": { "category": "category", "fault_address": "address" }
        }]
    })
}

fn kv_value_config() -> Value {
    json!({
        "keys": [{
            "key": "Exception",
            "kind": "core.log_line",
            "attrs": { "text": "value" }
        }]
    })
}

#[test]
fn key_value_duplicate_keys_last_wins() {
    let log = b"Exception = access_violation 0x1\nException = oom 0x2\n";
    let out = output("key_value", kv_pattern_config(), log, ParserLimits::DEFAULT);
    assert_eq!(out.facts.len(), 1);
    assert_eq!(text_of(&out.facts[0], "category"), "oom");
    assert_eq!(text_of(&out.facts[0], "fault_address"), "0x2");
}

#[test]
fn key_value_line_without_separator_is_ignored() {
    let log = b"just some words\nException = boom\n";
    let out = output("key_value", kv_value_config(), log, ParserLimits::DEFAULT);
    assert_eq!(out.facts.len(), 1);
    assert_eq!(text_of(&out.facts[0], "text"), "boom");
}

#[test]
fn key_value_utf8_bom_is_stripped() {
    let mut log = vec![0xEF, 0xBB, 0xBF];
    log.extend_from_slice(b"Exception = boom\n");
    let out = output("key_value", kv_value_config(), &log, ParserLimits::DEFAULT);
    assert_eq!(out.facts.len(), 1);
    assert_eq!(text_of(&out.facts[0], "text"), "boom");
}

#[test]
fn key_value_colon_and_ini_sections() {
    let log = b"[General]\nException: general boom\n[Other]\nException: other boom\n";
    let config = json!({
        "keys": [
            {
                "key": "Exception",
                "section": "General",
                "kind": "core.log_line",
                "attrs": { "text": "value" }
            },
            {
                "key": "Exception",
                "section": "Other",
                "kind": "core.log_line",
                "attrs": { "text": "value" }
            }
        ]
    });
    let out = output("key_value", config, log, ParserLimits::DEFAULT);
    assert_eq!(out.facts.len(), 2);
    assert_eq!(text_of(&out.facts[0], "text"), "general boom");
    assert_eq!(text_of(&out.facts[1], "text"), "other boom");
    assert_eq!(
        out.facts[0].span.as_ref().unwrap().section.as_deref(),
        Some("General")
    );
    assert_eq!(
        out.facts[1].span.as_ref().unwrap().section.as_deref(),
        Some("Other")
    );
}

#[test]
fn key_value_invalid_regex_is_a_pack_error() {
    let config = json!({
        "keys": [{
            "key": "Exception",
            "kind": "core.exception",
            "value_pattern": "(unclosed"
        }]
    });
    assert!(super::validate_config("key_value", &config).is_err());
    let report = report_for(&json!({
        "contract": CRASH_CONTRACT_VERSION,
        "parsers": [{
            "id": "probe",
            "entry": { "generic": { "name": "key_value", "config": config } }
        }]
    }));
    assert!(report.packs.is_empty());
    assert_eq!(report.problems[0].code, "parser_config");
}

// ---------------------------------------------------------------------------
// minidump
// ---------------------------------------------------------------------------

struct Fixture {
    bytes: Vec<u8>,
    modules_at: usize,
    module_name_at: usize,
    threads_at: usize,
}

fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn set_directory(bytes: &mut [u8], index: usize, stream_type: u32, size: u32, rva: u32) {
    let at = 32 + index * 12;
    write_u32(bytes, at, stream_type);
    write_u32(bytes, at + 4, size);
    write_u32(bytes, at + 8, rva);
}

/// A small valid dump: one module `game.exe` (0x140000000 + 0x1000, version 1.2.3.4), one
/// ACCESS_VIOLATION, and one thread whose stack holds a code address.
fn fixture() -> Fixture {
    let mut bytes = vec![0u8; 32];
    let directory_at = bytes.len();
    bytes.extend(std::iter::repeat_n(0u8, 12 * 3));
    let exception_at = bytes.len();
    bytes.extend(std::iter::repeat_n(0u8, 168));
    let modules_at = bytes.len();
    bytes.extend(std::iter::repeat_n(0u8, 4 + 108));
    let module_name_at = bytes.len();
    let name: Vec<u16> = "game.exe".encode_utf16().collect();
    bytes.extend((name.len() as u32 * 2).to_le_bytes());
    for unit in &name {
        bytes.extend(unit.to_le_bytes());
    }
    let threads_at = bytes.len();
    bytes.extend(std::iter::repeat_n(0u8, 4 + 48));
    let stack_at = bytes.len();
    bytes.extend(0x1_4000_0200u64.to_le_bytes());
    bytes.extend(std::iter::repeat_n(0u8, 56));

    write_u32(&mut bytes, 0, 0x504D_444D);
    write_u32(&mut bytes, 4, 0xA793);
    write_u32(&mut bytes, 8, 3);
    write_u32(&mut bytes, 12, directory_at as u32);
    set_directory(&mut bytes, 0, 6, 168, exception_at as u32);
    set_directory(&mut bytes, 1, 4, 4 + 108, modules_at as u32);
    set_directory(&mut bytes, 2, 3, 4 + 48, threads_at as u32);

    write_u32(&mut bytes, exception_at, 42);
    write_u32(&mut bytes, exception_at + 8, 0xC000_0005);
    write_u64(&mut bytes, exception_at + 24, 0x1_4000_0123);
    write_u32(&mut bytes, exception_at + 32, 2);
    write_u64(&mut bytes, exception_at + 40, 0);
    write_u64(&mut bytes, exception_at + 48, 0x10);

    let module = modules_at;
    write_u32(&mut bytes, module, 1);
    let entry = module + 4;
    write_u64(&mut bytes, entry, 0x1_4000_0000);
    write_u32(&mut bytes, entry + 8, 0x1000);
    write_u32(&mut bytes, entry + 16, 0x1111_1111);
    write_u32(&mut bytes, entry + 20, module_name_at as u32);
    write_u32(&mut bytes, entry + 24, 0xFEEF_04BD);
    write_u32(&mut bytes, entry + 32, 0x0001_0002);
    write_u32(&mut bytes, entry + 36, 0x0003_0004);

    write_u32(&mut bytes, threads_at, 1);
    let thread = threads_at + 4;
    write_u32(&mut bytes, thread, 7);
    write_u64(&mut bytes, thread + 24, 0x1_4000_0200);
    write_u32(&mut bytes, thread + 32, 64);
    write_u32(&mut bytes, thread + 36, stack_at as u32);

    Fixture {
        bytes,
        modules_at,
        module_name_at,
        threads_at,
    }
}

fn minidump(bytes: &[u8]) -> ArtifactParse {
    run_generic("minidump", Value::Null, bytes, ParserLimits::DEFAULT)
}

#[test]
fn minidump_wrong_signature_is_wrong_format() {
    let mut fixture = fixture();
    write_u32(&mut fixture.bytes, 0, 0);
    let attempt = minidump(&fixture.bytes);
    match only_outcome(&attempt) {
        ParserDispatch::WrongFormat(output) => {
            assert!(output
                .notes
                .iter()
                .any(|note| note.kind == ParseNoteKind::WrongFormat));
        }
        other => panic!("expected wrong format, got {other:?}"),
    }
}

#[test]
fn minidump_truncated_at_every_header_length() {
    let original = fixture();
    for length in 0..=32 {
        let bytes = &original.bytes[..length];
        let attempt = minidump(bytes);
        assert!(
            !matches!(only_outcome(&attempt), ParserDispatch::Failed(_)),
            "length {length}: {attempt:?}"
        );
    }
}

#[test]
fn minidump_stream_count_u32_max_does_not_allocate() {
    let mut fixture = fixture();
    write_u32(&mut fixture.bytes, 8, u32::MAX);
    let started = Instant::now();
    let attempt = minidump(&fixture.bytes);
    assert!(!matches!(only_outcome(&attempt), ParserDispatch::Failed(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn minidump_stream_directory_past_end() {
    let mut fixture = fixture();
    let past_end = fixture.bytes.len() as u32 + 100;
    write_u32(&mut fixture.bytes, 12, past_end);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    assert!(out.facts.is_empty());
    for coverage in &out.coverage {
        assert_eq!(coverage.state, CoverageState::Absent, "{coverage:?}");
    }
}

#[test]
fn minidump_module_count_larger_than_file() {
    let mut fixture = fixture();
    write_u32(&mut fixture.bytes, fixture.modules_at, u32::MAX);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    let modules = out
        .facts
        .iter()
        .filter(|fact| fact.kind == "core.module")
        .count();
    assert_eq!(modules, 1);
}

#[test]
fn minidump_module_name_rva_past_end() {
    let mut fixture = fixture();
    let entry = fixture.modules_at + 4;
    write_u32(&mut fixture.bytes, entry + 20, u32::MAX);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    let module = out
        .facts
        .iter()
        .find(|fact| fact.kind == "core.module")
        .unwrap();
    assert!(module.refs.is_empty());
    assert_eq!(text_of(module, "base_address"), "0x140000000");
    assert!(!out.notes.is_empty());
}

#[test]
fn minidump_name_without_terminator_and_odd_length() {
    let mut fixture = fixture();
    write_u32(&mut fixture.bytes, fixture.module_name_at, 15);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    let module = out
        .facts
        .iter()
        .find(|fact| fact.kind == "core.module")
        .unwrap();
    assert_eq!(module.refs.len(), 1);
    assert!(
        module.refs[0].value.starts_with("game."),
        "{:?}",
        module.refs
    );
}

#[test]
fn minidump_overlapping_streams_do_not_loop() {
    let mut fixture = fixture();
    let exception_at = {
        let at = 32 + 8;
        u32::from_le_bytes(fixture.bytes[at..at + 4].try_into().unwrap()) as usize
    };
    set_directory(&mut fixture.bytes, 0, 3, 4 + 48, fixture.threads_at as u32);
    set_directory(&mut fixture.bytes, 1, 3, 4 + 48, fixture.threads_at as u32);
    set_directory(&mut fixture.bytes, 2, 6, 168, exception_at as u32);
    let started = Instant::now();
    let attempt = minidump(&fixture.bytes);
    assert!(!matches!(only_outcome(&attempt), ParserDispatch::Failed(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn minidump_thread_list_past_end_is_absent_not_checked() {
    let mut fixture = fixture();
    set_directory(&mut fixture.bytes, 2, 3, 4 + 48, u32::MAX);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    assert_eq!(kind_state(&out, "core.frame"), CoverageState::Absent);
}

#[test]
fn minidump_zero_size_stack() {
    let mut fixture = fixture();
    write_u32(&mut fixture.bytes, fixture.threads_at + 4 + 32, 0);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    assert_eq!(kind_state(&out, "core.frame"), CoverageState::Checked);
    assert!(!out.facts.iter().any(|fact| fact.kind == "core.frame"));
}

#[test]
fn minidump_stack_over_budget_is_cut_and_says_so() {
    let fixture = fixture();
    // The header, directory and streams use about 420 bytes; the 64-byte stack gets only the
    // remainder, so the scan is cut after finding the address at its start.
    let limits = ParserLimits {
        max_total_read_bytes: 448,
        ..ParserLimits::DEFAULT
    };
    let out = output("minidump", Value::Null, &fixture.bytes, limits);
    assert!(out.facts.iter().any(|fact| fact.kind == "core.frame"));
    let cut = out.notes.iter().any(|note| {
        note.kind == ParseNoteKind::Truncated
            && note
                .message
                .as_deref()
                .is_some_and(|message| message.contains("cut"))
    });
    assert!(cut, "{:?}", out.notes);
}

#[test]
fn minidump_without_exception_stream_is_absent_coverage() {
    let mut fixture = fixture();
    set_directory(&mut fixture.bytes, 0, 0, 0, 0);
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );
    assert_eq!(kind_state(&out, "core.exception"), CoverageState::Absent);
    let exception = out
        .coverage
        .iter()
        .find(|coverage| coverage.kind == "core.exception")
        .unwrap();
    assert!(exception.why.is_some());
}

#[test]
fn minidump_reads_exception_modules_and_scan_frames() {
    let fixture = fixture();
    let out = output(
        "minidump",
        Value::Null,
        &fixture.bytes,
        ParserLimits::DEFAULT,
    );

    assert_eq!(out.producer.format, "windows_minidump");

    let exception = out
        .facts
        .iter()
        .find(|fact| fact.kind == "core.exception")
        .unwrap();
    assert_eq!(text_of(exception, "category"), "access_violation");
    assert_eq!(text_of(exception, "class"), "0xC0000005");
    assert_eq!(text_of(exception, "fault_address"), "0x10");
    assert_eq!(text_of(exception, "access"), "read");
    assert!(bool_of(exception, "near_null"));

    let module = out
        .facts
        .iter()
        .find(|fact| fact.kind == "core.module")
        .unwrap();
    assert_eq!(text_of(module, "version"), "1.2.3.4");
    assert_eq!(text_of(module, "base_address"), "0x140000000");
    assert_eq!(number_of(module, "size"), 4096.0);
    assert_eq!(module.refs.len(), 1);
    assert_eq!(module.refs[0].kind, "module");
    assert_eq!(module.refs[0].value, "game.exe");
    assert_eq!(module.refs[0].strength, Strength::Mention);

    let frame = out
        .facts
        .iter()
        .find(|fact| fact.kind == "core.frame")
        .unwrap();
    assert_eq!(text_of(frame, "kind"), "scan");
    assert_eq!(number_of(frame, "index"), 0.0);
    assert_eq!(text_of(frame, "address"), "0x140000200");
    assert_eq!(text_of(frame, "rva"), "0x200");
    assert_eq!(frame.refs.len(), 1);
    assert_eq!(frame.refs[0].value, "game.exe");

    assert!(out
        .bucket
        .iter()
        .any(|part| part.scheme == "minidump.exception" && part.value == "0xC0000005"));

    for kind in ["core.exception", "core.module", "core.frame"] {
        assert_eq!(kind_state(&out, kind), CoverageState::Checked, "{kind}");
    }
}
