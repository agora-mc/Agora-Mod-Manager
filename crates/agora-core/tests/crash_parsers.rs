//! Hostile parser tests and one runnable example. Every log here is written
//! for these tests; nothing is downloaded.

use agora_core::crash_packs::{
    artifact_handle, parse_artifact, parser_request, MemoryArtifact, ParserFailureKind,
};
use agora_game_api::crash::{GameHandle, ParserLimits, ParserOutput, CRASH_CONTRACT_VERSION};
use agora_plugin_api::host::{ActivationRequest, HostBridge, ScriptHost};
use agora_plugin_api::manifest::PluginId;
use agora_plugin_api::protocol::{HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct NoopBridge;

impl HostBridge for NoopBridge {
    fn call(
        &self,
        _plugin_id: &PluginId,
        request: agora_plugin_api::protocol::HostRequest,
    ) -> HostResponse {
        HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

fn plugin_id() -> PluginId {
    PluginId::parse("acme.crash").unwrap()
}

struct Running {
    _dir: TempDir,
    host: QuickJsHost,
}

fn start(script: &str, memory_limit_bytes: usize) -> Running {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.js"), script).unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: plugin_id(),
            package_root: dir.path().to_path_buf(),
            entrypoint: "index.js".into(),
            memory_limit_bytes,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .unwrap();
    Running { _dir: dir, host }
}

fn limits(max_ms: u64) -> ParserLimits {
    ParserLimits {
        max_ms,
        ..ParserLimits::DEFAULT
    }
}

fn pack_for(export: &str, detect: serde_json::Value) -> agora_game_api::crash::CrashPack {
    serde_json::from_value(serde_json::json!({
        "contract": CRASH_CONTRACT_VERSION,
        "parsers": [{
            "id": "probe",
            "entry": { "script": { "export": export } },
            "detect": detect,
        }],
    }))
    .unwrap()
}

fn run(
    running: &Running,
    script_pack: &agora_game_api::crash::CrashPack,
    log: &[u8],
    limits: ParserLimits,
) -> (agora_core::crash_packs::ArtifactParse, Duration) {
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("log", log.to_vec());
    let request = parser_request(
        "probe",
        artifact_handle("log", "crash.log", log.len() as u64, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        limits,
    );
    let started = Instant::now();
    let parsed = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        script_pack,
        &request,
        Arc::new(artifact),
        Some("probe"),
        b"",
    );
    (parsed, started.elapsed())
}

fn failure(
    parsed: &agora_core::crash_packs::ArtifactParse,
) -> &agora_core::crash_packs::ParserFailure {
    parsed.failures().next().expect("expected a failure")
}

const VALID_OUTPUT: &str = r#"{ contract: 1, producer: { format: "probe" }, facts: [], bucket: [], coverage: [], notes: [] }"#;

#[test]
fn a_catastrophic_regex_is_interrupted_within_the_limit() {
    let script = format!(
        r#"
export async function parseCrash(request) {{
  const bomb = /(a+)+$/;
  bomb.test("a".repeat(40) + "!");
  return {VALID_OUTPUT};
}}
"#
    );
    let running = start(&script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let limits = limits(300);
    let (parsed, elapsed) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits,
    );
    let failure = failure(&parsed);
    assert_eq!(failure.kind, ParserFailureKind::TimedOut, "{failure}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the interrupt took {elapsed:?}"
    );
}

#[test]
fn an_infinite_loop_is_interrupted() {
    let running = start(
        "export async function parseCrash() { while (true) {} }",
        ActivationRequest::DEFAULT_MEMORY_LIMIT,
    );
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(300),
    );
    assert_eq!(failure(&parsed).kind, ParserFailureKind::TimedOut);
}

#[test]
fn a_memory_bomb_stops_the_parser_without_taking_the_host() {
    let running = start(
        r#"
export async function parseCrash() {
  const hoard = [];
  while (true) hoard.push(new Array(100000).fill(0));
}
"#,
        4 * 1024 * 1024,
    );
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(5_000),
    );
    let failure = failure(&parsed);
    assert!(
        matches!(
            failure.kind,
            ParserFailureKind::OutOfMemory | ParserFailureKind::TimedOut
        ),
        "{failure}"
    );
}

#[test]
fn a_huge_output_is_refused_by_the_size_cap() {
    let script = r#"
export async function parseCrash() {
  return {
    contract: 1,
    producer: { format: "probe" },
    facts: [{ kind: "core.log_line", attrs: { text: "x".repeat(9 * 1024 * 1024) } }],
  };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(5_000),
    );
    assert_eq!(failure(&parsed).kind, ParserFailureKind::OutputTooLarge);
}

#[test]
fn a_wrong_contract_version_is_an_error_that_names_the_parser() {
    let script = r#"
export async function parseCrash() {
  return { contract: 99, producer: { format: "probe" }, facts: [] };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(2_000),
    );
    let failure = failure(&parsed);
    assert_eq!(failure.kind, ParserFailureKind::WrongContract);
    assert_eq!(failure.parser_id, "probe");
    assert_eq!(failure.plugin_id, "acme.crash");
}

#[test]
fn output_that_is_not_json_is_an_error_not_an_empty_result() {
    let running = start(
        "export async function parseCrash() { return undefined; }",
        ActivationRequest::DEFAULT_MEMORY_LIMIT,
    );
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(2_000),
    );
    assert_eq!(failure(&parsed).kind, ParserFailureKind::InvalidOutput);
}

#[test]
fn a_script_that_throws_is_reported() {
    let running = start(
        r#"export async function parseCrash() { throw new Error("boom"); }"#,
        ActivationRequest::DEFAULT_MEMORY_LIMIT,
    );
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(2_000),
    );
    assert_eq!(failure(&parsed).kind, ParserFailureKind::ScriptThrew);
}

#[test]
fn a_missing_export_is_its_own_failure() {
    let running = start(
        "export async function somethingElse() {}",
        ActivationRequest::DEFAULT_MEMORY_LIMIT,
    );
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"x",
        limits(2_000),
    );
    assert_eq!(failure(&parsed).kind, ParserFailureKind::MissingExport);
}

#[test]
fn an_empty_log_is_parsed_not_failed() {
    let script = format!(
        r#"
export async function parseCrash(request) {{
  const reply = crashReadLines(request.artifact.id, 0, 10);
  if (reply.lines.length !== 0) throw new Error("expected no lines");
  return {VALID_OUTPUT};
}}

import {{ crash }} from "agora";
function crashReadLines(id, from, max) {{ return crash.readLines(id, from, max); }}
"#
    );
    let running = start(&script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        b"",
        limits(2_000),
    );
    assert!(parsed.parsed().is_some(), "{parsed:?}");
}

#[test]
fn a_log_with_invalid_utf8_still_reads() {
    let script = r#"
import { crash } from "agora";
export async function parseCrash(request) {
  const reply = crash.readLines(request.artifact.id, 0, 10);
  return { contract: 1, producer: { format: "probe" }, facts: [], notes: [], coverage: [] };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let log = [0xff, 0xfe, b'a', b'\n', b'b'];
    let (parsed, _) = run(
        &running,
        &pack_for("parseCrash", serde_json::json!({})),
        &log,
        limits(2_000),
    );
    assert!(parsed.parsed().is_some(), "{parsed:?}");
}

#[test]
fn detect_matching_nothing_is_distinct_from_a_parser_failure() {
    let running = start(
        r#"export async function parseCrash() { throw new Error("should not run"); }"#,
        ActivationRequest::DEFAULT_MEMORY_LIMIT,
    );
    let pack = pack_for(
        "parseCrash",
        serde_json::json!({ "all": ["(?m)^Unhandled exception "] }),
    );

    let mut artifact = MemoryArtifact::new(ParserLimits::DEFAULT);
    artifact.insert("log", b"an ordinary log line".to_vec());
    let request = parser_request(
        "probe",
        artifact_handle("log", "crash.log", 21, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        ParserLimits::DEFAULT,
    );
    let nothing = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        &pack,
        &request,
        Arc::new(artifact),
        None,
        b"an ordinary log line",
    );
    assert!(nothing.no_parser);
    assert!(nothing.attempts.is_empty());

    let mut artifact = MemoryArtifact::new(ParserLimits::DEFAULT);
    artifact.insert("log", b"Unhandled exception 0xC0000005\n".to_vec());
    let request = parser_request(
        "probe",
        artifact_handle("log", "crash.log", 29, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        ParserLimits::DEFAULT,
    );
    let failed = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        &pack,
        &request,
        Arc::new(artifact),
        None,
        b"Unhandled exception 0xC0000005\n",
    );
    assert!(!failed.no_parser);
    assert_eq!(failure(&failed).kind, ParserFailureKind::ScriptThrew);
}

#[test]
fn a_script_cannot_reach_require_fetch_or_the_file_system() {
    for (name, body) in [
        ("require", "return require('fs');"),
        ("fetch", "return await fetch('https://example.invalid/');"),
        ("file", "return process.mainModule.require('fs');"),
    ] {
        let script = format!("export async function parseCrash() {{ {body} }}");
        let running = start(&script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
        let (parsed, _) = run(
            &running,
            &pack_for("parseCrash", serde_json::json!({})),
            b"x",
            limits(2_000),
        );
        let failure = failure(&parsed);
        assert_eq!(
            failure.kind,
            ParserFailureKind::ScriptThrew,
            "{name}: {failure}"
        );
    }
}

/// The worked Skyrim example, produced by a real script from a log we wrote.
#[test]
fn the_skyrim_example_is_reproduced_by_a_script() {
    let script = include_str!("crash_fixtures/skyrim_parser.js");
    let log = include_str!("crash_fixtures/skyrim_crash.txt").as_bytes();
    let expected: ParserOutput = serde_json::from_str(include_str!(
        "../../agora-game-api/tests/crash_examples/skyrim.output.json"
    ))
    .unwrap();

    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let pack = pack_for(
        "parseCrashLogger",
        serde_json::json!({ "all": ["(?m)^Unhandled exception "] }),
    );
    let mut artifact = MemoryArtifact::new(ParserLimits::DEFAULT);
    artifact.insert("log", log.to_vec());
    let request = parser_request(
        "crash_logger",
        artifact_handle("log", "crash-2024.log", log.len() as u64, Vec::new()),
        GameHandle {
            id: "skyrim-se".into(),
            runtime_version: None,
        },
        ParserLimits::DEFAULT,
    );
    let parsed = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        &pack,
        &request,
        Arc::new(artifact),
        None,
        log,
    );
    let output = parsed.parsed().expect("the script should have parsed it");
    assert_eq!(output, &expected);
}

#[test]
fn binary_reads_cross_as_bytes_through_read_bytes() {
    let script = r#"
import { crash } from "agora";
export async function parseCrash(request) {
  const bytes = new Uint8Array(crash.readBytes(request.artifact.id, 0, 4));
  if (bytes.length !== 4 || bytes[0] !== 0x4e || bytes[1] !== 0x49 || bytes[2] !== 0x46) {
    throw new Error("unexpected header " + Array.from(bytes));
  }
  return {
    contract: 1,
    producer: { format: "probe" },
    facts: [],
    coverage: [{ kind: "core.module", state: "checked" }],
  };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let mut artifact = MemoryArtifact::new(ParserLimits::DEFAULT);
    artifact.insert("mesh", b"NIF\x00\x01\x02".to_vec());
    let request = parser_request(
        "probe",
        artifact_handle("mesh", "mesh.nif", 7, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        ParserLimits::DEFAULT,
    );
    let parsed = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        &pack_for("parseCrash", serde_json::json!({})),
        &request,
        Arc::new(artifact),
        Some("probe"),
        b"",
    );
    assert!(parsed.parsed().is_some(), "{parsed:?}");
}

#[test]
fn a_parser_failure_does_not_stop_another_parser() {
    let script = r#"
import { crash } from "agora";
const exports = {};
export async function broken() { throw new Error("first one is bad"); }
export async function working(request) {
  return { contract: 1, producer: { format: "probe" }, facts: [], coverage: [] };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let pack: agora_game_api::crash::CrashPack = serde_json::from_value(serde_json::json!({
        "contract": 1,
        "parsers": [
            { "id": "broken", "entry": { "script": { "export": "broken" } }, "detect": { "all": ["log"] } },
            { "id": "working", "entry": { "script": { "export": "working" } }, "detect": { "all": ["log"] } }
        ]
    }))
    .unwrap();
    let log = b"log";
    let mut artifact = MemoryArtifact::new(ParserLimits::DEFAULT);
    artifact.insert("log", log.to_vec());
    let request = parser_request(
        "broken",
        artifact_handle("log", "crash.log", 3, Vec::new()),
        GameHandle {
            id: "test-game".into(),
            runtime_version: None,
        },
        ParserLimits::DEFAULT,
    );
    let parsed = parse_artifact(
        &running.host,
        plugin_id().as_str(),
        &pack,
        &request,
        Arc::new(artifact),
        None,
        log,
    );
    assert_eq!(parsed.attempts.len(), 2);
    assert_eq!(parsed.failures().count(), 1);
    assert!(parsed.parsed().is_some());
}

#[test]
fn a_pack_can_ask_for_less_than_the_engine_allows() {
    let script = r#"
import { crash } from "agora";
export async function parseCrash(request) {
  crash.readBytes(request.artifact.id, 0, 16);
  return { contract: 1, producer: { format: "probe" }, facts: [] };
}
"#;
    let running = start(script, ActivationRequest::DEFAULT_MEMORY_LIMIT);
    let mut pack = pack_for("parseCrash", serde_json::json!({}));
    pack.parsers[0].limits = Some(ParserLimits {
        max_read_bytes: 4,
        ..ParserLimits::DEFAULT
    });
    let (parsed, _) = run(&running, &pack, b"0123456789abcdef", limits(2_000));
    let failure = failure(&parsed);
    assert_eq!(failure.kind, ParserFailureKind::ScriptThrew);
    assert!(
        failure.message.contains("asked for 16 bytes"),
        "{}",
        failure.message
    );
}
