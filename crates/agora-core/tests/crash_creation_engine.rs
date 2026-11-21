//! The Creation Engine crash pack's parsers, run through the real QuickJS bridge
//! and the pack file the plugin ships. Every log here is invented for these
//! tests; nothing is downloaded and no real log text is used.

use agora_core::crash_packs::{
    artifact_handle, load_plugin_packs, parse_artifact, parser_request, ArtifactParse,
    MemoryArtifact, ParserDispatch,
};
use agora_game_api::crash::{
    AttrValue, CrashPack, Fact, GameHandle, ParserLimits, ParserOutput, Scalar,
};
use agora_plugin_api::host::{ActivationRequest, HostBridge, ScriptHost};
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const PLUGIN: &str = "agora.creation-engine";
const FIXTURES: &str = "tests/crash_fixtures/creation_engine";

struct NoopBridge;

impl HostBridge for NoopBridge {
    fn call(&self, _plugin_id: &PluginId, request: HostRequest) -> HostResponse {
        HostResponse::Ok {
            request_id: request.request_id,
            value: serde_json::Value::Null,
        }
    }

    fn log(&self, _plugin_id: &PluginId, _level: LogLevel, _message: &str) {}
}

fn official_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/official/creation-engine")
}

fn read_manifest(root: &Path) -> PluginManifest {
    let text = std::fs::read_to_string(root.join("agora-plugin.json")).unwrap();
    PluginManifest::parse(&text).unwrap()
}

fn crash_pack(root: &Path) -> CrashPack {
    let manifest = read_manifest(root);
    let report = load_plugin_packs(PLUGIN, root, &manifest);
    assert!(
        report.is_clean(),
        "the official creation-engine pack must be clean: {:#?}",
        report.problems
    );
    assert_eq!(report.packs.len(), 1, "{report:#?}");
    report.packs[0].pack.clone()
}

struct Running {
    _data: TempDir,
    host: QuickJsHost,
    pack: CrashPack,
}

fn start() -> Running {
    let root = official_root();
    let pack = crash_pack(&root);
    let data = tempfile::tempdir().unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: PluginId::parse(PLUGIN).unwrap(),
            package_root: root,
            data_root: data.path().to_path_buf(),
            entrypoint: "main.js".into(),
            memory_limit_bytes: ActivationRequest::DEFAULT_MEMORY_LIMIT,
            stack_limit_bytes: ActivationRequest::DEFAULT_STACK_LIMIT,
            api_version: agora_plugin_api::host_api_version_string(),
            settings: serde_json::json!({}),
        },
        Arc::new(NoopBridge),
    )
    .unwrap();
    Running {
        _data: data,
        host,
        pack,
    }
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path:?}: {error}"))
}

fn parse_with_format(running: &Running, name: &str, bytes: &[u8], format: &str) -> ArtifactParse {
    let limits = ParserLimits::DEFAULT;
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("log", bytes.to_vec());
    let request = parser_request(
        format,
        artifact_handle("log", name, bytes.len() as u64, Vec::new()),
        GameHandle {
            id: "skyrim-se".into(),
            runtime_version: None,
        },
        limits,
    );
    parse_artifact(
        &running.host,
        PLUGIN,
        &running.pack,
        &request,
        Arc::new(artifact),
        Some(format),
        bytes,
    )
}

fn parse_by_detection(running: &Running, name: &str, bytes: &[u8]) -> ArtifactParse {
    let limits = ParserLimits::DEFAULT;
    let mut artifact = MemoryArtifact::new(limits);
    artifact.insert("log", bytes.to_vec());
    let request = parser_request(
        "detect",
        artifact_handle("log", name, bytes.len() as u64, Vec::new()),
        GameHandle {
            id: "skyrim-se".into(),
            runtime_version: None,
        },
        limits,
    );
    parse_artifact(
        &running.host,
        PLUGIN,
        &running.pack,
        &request,
        Arc::new(artifact),
        None,
        bytes,
    )
}

fn parsed(parsed: &ArtifactParse) -> &ParserOutput {
    parsed.parsed().unwrap_or_else(|| {
        panic!(
            "expected a parse, got attempts {:#?}",
            parsed
                .attempts
                .iter()
                .map(|attempt| format!("{:?}", attempt.outcome))
                .collect::<Vec<_>>()
        )
    })
}

fn facts<'a>(output: &'a ParserOutput, kind: &str) -> Vec<&'a Fact> {
    output
        .facts
        .iter()
        .filter(|fact| fact.kind == kind)
        .collect()
}

fn text_attr(fact: &Fact, name: &str) -> Option<String> {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Text(text))) => Some(text.clone()),
        _ => None,
    }
}

fn number_attr(fact: &Fact, name: &str) -> Option<f64> {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Number(number))) => Some(*number),
        _ => None,
    }
}

fn bool_attr(fact: &Fact, name: &str) -> Option<bool> {
    match fact.attrs.get(name) {
        Some(AttrValue::Scalar(Scalar::Bool(value))) => Some(*value),
        _ => None,
    }
}

fn ref_values(fact: &Fact, kind: &str) -> Vec<String> {
    fact.refs
        .iter()
        .filter(|reference| reference.kind == kind)
        .map(|reference| reference.value.clone())
        .collect()
}

fn coverage(output: &ParserOutput, kind: &str) -> String {
    output
        .coverage
        .iter()
        .find(|entry| entry.kind == kind)
        .map(|entry| format!("{:?}", entry.state))
        .unwrap_or_else(|| "missing".to_string())
}

#[test]
fn the_official_pack_loads_and_both_parsers_detect_their_logs() {
    let running = start();
    let parsed = parse_by_detection(&running, "crash-1.log", &fixture("crash_logger_1_16.txt"));
    assert_eq!(parsed.attempts.len(), 1);
    assert_eq!(parsed.attempts[0].parser_id, "crash_logger");
    let parsed = parse_by_detection(&running, "crash_1.log", &fixture("trainwreck_1_4.txt"));
    assert_eq!(parsed.attempts.len(), 1);
    assert_eq!(parsed.attempts[0].parser_id, "trainwreck");
    let parsed = parse_by_detection(&running, "skse64.log", &fixture("skse_log.txt"));
    assert_eq!(parsed.attempts.len(), 1);
    assert_eq!(parsed.attempts[0].parser_id, "skse_log");
}

#[test]
fn the_skse_log_keeps_one_verdict_per_dll_and_the_last_line_wins() {
    let running = start();
    let output = parse_by_detection(&running, "skse64.log", &fixture("skse_log.txt"));
    let output = parsed(&output);

    assert_eq!(output.producer.format, "skse_log");
    assert_eq!(output.producer.version.as_deref(), Some("2.2.6"));
    assert_eq!(coverage(output, "core.load_verdict"), "Checked");
    assert_eq!(coverage(output, "creation.skse_load"), "Checked");

    let verdicts = facts(output, "core.load_verdict");
    assert_eq!(verdicts.len(), 5, "one verdict per DLL, not per line");
    let verdict_for = |dll: &str| {
        verdicts
            .iter()
            .find(|fact| ref_values(fact, "module") == vec![dll.to_string()])
            .unwrap_or_else(|| panic!("no verdict for {dll}"))
    };

    let loaded = verdict_for("ExampleLoaded.dll");
    assert_eq!(text_attr(loaded, "outcome").as_deref(), Some("loaded"));
    let old = verdict_for("ExampleOld.dll");
    assert_eq!(text_attr(old, "outcome").as_deref(), Some("refused"));
    assert_eq!(
        text_attr(old, "reason").as_deref(),
        Some("different_game_version")
    );
    let missing = verdict_for("ExampleMissing.dll");
    assert_eq!(text_attr(missing, "outcome").as_deref(), Some("failed"));
    assert_eq!(
        text_attr(missing, "reason").as_deref(),
        Some("dependent_dll_missing")
    );
    let post = verdict_for("ExamplePostLoad.dll");
    assert_eq!(
        text_attr(post, "outcome").as_deref(),
        Some("failed"),
        "the post-load crash is the last line, so it wins over loaded correctly"
    );
    assert_eq!(
        text_attr(post, "reason").as_deref(),
        Some("post_load_crash")
    );
    let spaced = verdict_for("Example Spaced.dll");
    assert_eq!(text_attr(spaced, "outcome").as_deref(), Some("loaded"));

    let detail = facts(output, "creation.skse_load")
        .into_iter()
        .find(|fact| ref_values(fact, "module") == vec!["ExampleOld.dll".to_string()])
        .expect("the display detail must be kept");
    assert_eq!(
        text_attr(detail, "status").as_deref(),
        Some("disabled, incompatible with current version of the game")
    );
    assert_eq!(
        text_attr(detail, "plugin_name").as_deref(),
        Some("ExampleOld")
    );
}

#[test]
fn a_crash_log_is_not_read_as_skse_own_log_and_the_reverse() {
    let running = start();
    let crash = parse_by_detection(&running, "crash-1.log", &fixture("crash_logger_1_16.txt"));
    assert!(
        crash
            .attempts
            .iter()
            .all(|attempt| attempt.parser_id != "skse_log"),
        "the crash-log parser claims it; SKSE's parser must not"
    );
    let skse = parse_with_format(
        &running,
        "skse64.log",
        &fixture("skse_log.txt"),
        "crash_logger",
    );
    assert!(skse.parsed().is_none(), "{skse:#?}");
}

#[test]
fn an_empty_file_is_not_a_clean_parse() {
    let running = start();
    let empty = fixture("empty.txt");
    assert!(empty.is_empty());

    let direct = parse_with_format(&running, "crash-empty.log", &empty, "crash_logger");
    assert!(direct.parsed().is_none());
    assert!(direct.failures().next().is_none());
    match &direct.attempts[0].outcome {
        ParserDispatch::WrongFormat(output) => {
            assert!(output.facts.is_empty());
            assert!(output
                .notes
                .iter()
                .any(|note| note.kind == agora_game_api::crash::ParseNoteKind::WrongFormat));
        }
        other => panic!("an empty file must be wrong-format, got {other:?}"),
    }

    let detected = parse_by_detection(&running, "crash-empty.log", &empty);
    assert!(detected.no_parser);
    assert!(detected.parsed().is_none());
}

#[test]
fn a_file_that_only_looks_similar_is_not_claimed() {
    let running = start();
    let similar = fixture("similar.txt");
    let detected = parse_by_detection(&running, "crash-notes.log", &similar);
    assert!(detected.no_parser, "{detected:#?}");

    let direct = parse_with_format(&running, "crash-notes.log", &similar, "crash_logger");
    assert!(direct.parsed().is_none());
    assert!(direct.failures().next().is_none());
}

#[test]
fn the_oldest_crash_logger_band_keeps_its_sections() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-old.log",
        &fixture("crash_logger_1_0.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    assert_eq!(output.producer.format, "crash_logger");
    assert_eq!(output.producer.version.as_deref(), Some("1.0.0"));

    let game = &facts(output, "creation.game")[0];
    assert_eq!(text_attr(game, "edition").as_deref(), Some("SSE"));
    assert_eq!(text_attr(game, "version").as_deref(), Some("1.6.353"));
    assert_eq!(bool_attr(game, "vr"), Some(false));

    let process = &facts(output, "core.process")[0];
    assert_eq!(text_attr(process, "image").as_deref(), Some("SkyrimSE.exe"));
    assert_eq!(
        text_attr(process, "image_version").as_deref(),
        Some("1.6.353")
    );
    assert!(
        number_attr(process, "pid").is_none(),
        "this band has no process block"
    );

    let exception = &facts(output, "core.exception")[0];
    assert_eq!(
        text_attr(exception, "class").as_deref(),
        Some("EXCEPTION_ACCESS_VIOLATION")
    );
    assert_eq!(
        text_attr(exception, "category").as_deref(),
        Some("access_violation")
    );
    assert_eq!(ref_values(exception, "module"), vec!["ExampleGame.dll"]);

    let frames = facts(output, "core.frame");
    assert_eq!(frames.len(), 3);
    assert_eq!(number_attr(frames[0], "index"), Some(0.0));
    assert_eq!(text_attr(frames[0], "symbol").as_deref(), Some("11111"));
    assert_eq!(
        text_attr(frames[0], "symbol_scheme").as_deref(),
        Some("addrlib-ae")
    );
    assert_eq!(
        frames[0].refs[0].strength,
        agora_game_api::crash::Strength::ExecTop
    );
    assert_eq!(
        frames[1].refs[0].strength,
        agora_game_api::crash::Strength::Exec
    );

    assert_eq!(facts(output, "core.module").len(), 2);
    assert_eq!(facts(output, "creation.script_extender_plugin").len(), 2);

    let plugins = facts(output, "creation.plugin");
    assert_eq!(plugins.len(), 3);
    let indexes: Vec<String> = plugins
        .iter()
        .map(|fact| text_attr(fact, "index").unwrap())
        .collect();
    assert_eq!(indexes, vec!["00", "01", "FE:000"]);
    assert_eq!(ref_values(plugins[2], "plugin"), vec!["ExampleLight.esl"]);
    assert_eq!(bool_attr(plugins[2], "light"), Some(true));

    let objects = facts(output, "creation.object");
    let form = objects
        .iter()
        .find(|fact| text_attr(fact, "form_id").as_deref() == Some("0x00012345"))
        .expect("the decoded form must be kept");
    assert_eq!(
        text_attr(form, "editor_id").as_deref(),
        Some("ExampleEditorId")
    );
    assert_eq!(ref_values(form, "plugin"), vec!["ExampleBase.esm"]);
    let chain = facts(output, "creation.plugin_chain");
    assert!(!chain.is_empty());
    let mut chain_refs: Vec<String> = chain
        .iter()
        .flat_map(|fact| ref_values(fact, "plugin"))
        .collect();
    chain_refs.sort();
    assert_eq!(chain_refs, vec!["ExampleBase.esm", "ExamplePatch.esp"]);

    let mesh = objects
        .iter()
        .find(|fact| text_attr(fact, "rtti").as_deref() == Some("ExampleTriShape"))
        .expect("the decoded mesh must be kept");
    assert_eq!(
        ref_values(mesh, "file"),
        vec!["meshes\\example\\invented.nif".to_string()]
    );

    assert_eq!(coverage(output, "creation.access_violation"), "Absent");
    assert_eq!(coverage(output, "creation.plugin"), "Checked");
}

#[test]
fn crash_logger_1_15_reads_the_access_violation_and_memory() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-115.log",
        &fixture("crash_logger_1_15.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let exception = &facts(output, "core.exception")[0];
    assert_eq!(text_attr(exception, "access").as_deref(), Some("write"));
    assert_eq!(
        text_attr(exception, "fault_address").as_deref(),
        Some("0x000000000000")
    );
    assert_eq!(
        text_attr(exception, "pointer_class").as_deref(),
        Some("null")
    );
    assert_eq!(bool_attr(exception, "near_null"), Some(true));

    let system = &facts(output, "core.system")[0];
    assert_eq!(number_attr(system, "vram_pct"), Some(25.0));
    let memory = &facts(output, "creation.system_memory")[0];
    assert_eq!(number_attr(memory, "physical_total_mb"), Some(16384.0));

    assert_eq!(coverage(output, "creation.access_violation"), "Absent");
    assert_eq!(coverage(output, "creation.plugin"), "Checked");
}

#[test]
fn crash_logger_1_16_keeps_process_nesting_and_versions() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-116.log",
        &fixture("crash_logger_1_16.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let process = &facts(output, "core.process")[0];
    assert_eq!(number_attr(process, "pid"), Some(9001.0));
    assert_eq!(
        text_attr(process, "cwd").as_deref(),
        Some("C:\\Games\\Invented Game")
    );
    assert_eq!(
        text_attr(process, "image_hash").as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_eq!(number_attr(process, "uptime_s"), Some(110.886));

    let objects = facts(output, "creation.object");
    let parent = objects
        .iter()
        .find(|fact| text_attr(fact, "field").as_deref() == Some("ParentCell"))
        .expect("the nested ParentCell object must be kept");
    assert_eq!(number_attr(parent, "depth"), Some(1.0));
    assert_eq!(text_attr(parent, "form_id").as_deref(), Some("0x0000003C"));
    assert_eq!(
        text_attr(parent, "place").as_deref(),
        Some("RSP+0"),
        "a nested object keeps the register or stack place it came from"
    );

    assert_eq!(ref_values(parent, "plugin"), vec!["ExampleBase.esm"]);

    let top = objects
        .iter()
        .find(|fact| text_attr(fact, "form_id").as_deref() == Some("0x00054321"))
        .expect("the reference object must be kept");
    assert_eq!(ref_values(top, "plugin"), vec!["ExamplePatch.esp"]);
    assert_eq!(text_attr(top, "name").as_deref(), Some("Example Object"));
}

#[test]
fn crash_logger_1_17_reads_compact_objects_and_frame_names() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-117.log",
        &fixture("crash_logger_1_17.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let objects = facts(output, "creation.object");
    let player = objects
        .iter()
        .find(|fact| text_attr(fact, "place").as_deref() == Some("R12"))
        .expect("the compact object must be kept");
    assert_eq!(
        text_attr(player, "name").as_deref(),
        Some("Invented Player")
    );
    assert_eq!(text_attr(player, "form_id").as_deref(), Some("0x00000014"));
    assert_eq!(ref_values(player, "plugin"), vec!["ExampleBase.esm"]);
    assert_eq!(ref_values(player, "form"), vec!["0x00000014"]);

    let names: Vec<String> = facts(output, "creation.frame_name")
        .iter()
        .filter_map(|fact| text_attr(fact, "name"))
        .collect();
    assert!(names.contains(&"ExampleNamespace::ExampleUpdate".to_string()));
    let logged = facts(output, "creation.frame_name")
        .into_iter()
        .find(|fact| text_attr(fact, "name").as_deref() == Some("ExampleNamespace::ExampleUpdate"))
        .expect("the log's own name must be kept");
    assert_eq!(text_attr(logged, "source").as_deref(), Some("log"));
}

fn named_frames(output: &ParserOutput) -> Vec<(f64, String, String)> {
    facts(output, "creation.frame_name")
        .into_iter()
        .map(|fact| {
            (
                number_attr(fact, "index").unwrap_or(-1.0),
                text_attr(fact, "name").unwrap_or_default(),
                text_attr(fact, "source").unwrap_or_default(),
            )
        })
        .collect()
}

#[test]
fn an_se_log_uses_the_se_address_library_names() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-se.log",
        &fixture("addrlib_se.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let frames = facts(output, "core.frame");
    assert_eq!(
        text_attr(frames[0], "symbol_scheme").as_deref(),
        Some("addrlib-se")
    );
    assert_eq!(text_attr(frames[0], "symbol").as_deref(), Some("19142"));

    let names = named_frames(output);
    assert_eq!(
        names,
        vec![
            (
                0.0,
                "TESObjectREFR::CreateReference".to_string(),
                "address_library_names".to_string()
            ),
            (
                2.0,
                "ExampleHelper::InventedHook".to_string(),
                "log".to_string()
            ),
        ],
        "the unnamed id must stay unnamed and the log's own name must be kept"
    );

    let site = &facts(output, "creation.exception_site")[0];
    assert_eq!(
        text_attr(site, "name").as_deref(),
        Some("TESObjectREFR::CreateReference"),
        "the exception site points at the named top frame"
    );
}

#[test]
fn an_ae_log_uses_the_ae_address_library_names() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-ae.log",
        &fixture("addrlib_ae.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let frames = facts(output, "core.frame");
    assert_eq!(
        text_attr(frames[0], "symbol_scheme").as_deref(),
        Some("addrlib-ae")
    );
    assert_eq!(text_attr(frames[0], "symbol").as_deref(), Some("19544"));

    let names = named_frames(output);
    assert_eq!(
        names,
        vec![(
            0.0,
            "TESObjectREFR::CreateReference".to_string(),
            "address_library_names".to_string()
        )],
        "an SE id on an AE log must not borrow the SE name"
    );
    assert_eq!(
        text_attr(facts(output, "creation.exception_site")[0], "name").as_deref(),
        Some("TESObjectREFR::CreateReference")
    );
}

#[test]
fn crash_logger_1_20_reads_the_access_violation_analysis() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-120.log",
        &fixture("crash_logger_1_20.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    let frames = facts(output, "core.frame");
    assert_eq!(frames.len(), 3, "the 1.20 call-stack section must be read");
    assert_eq!(text_attr(frames[0], "kind").as_deref(), Some("probable"));
    assert_eq!(text_attr(frames[2], "kind").as_deref(), Some("scan"));
    assert_eq!(
        frames[2].refs[0].strength,
        agora_game_api::crash::Strength::Mention,
        "a scanned frame is weak evidence"
    );

    let analysis = &facts(output, "creation.access_violation")[0];
    assert_eq!(text_attr(analysis, "base").as_deref(), Some("rdi"));
    assert_eq!(
        text_attr(analysis, "computed_address").as_deref(),
        Some("0x0000027EDCEFFF90")
    );
    assert_eq!(
        text_attr(analysis, "fault_address").as_deref(),
        Some("0x000000000010")
    );
    assert_eq!(bool_attr(analysis, "mismatch"), Some(true));
    assert_eq!(coverage(output, "creation.access_violation"), "Checked");
}

#[test]
fn trainwreck_1_4_keeps_its_own_sections() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash_tw14.log",
        &fixture("trainwreck_1_4.txt"),
        "trainwreck",
    );
    let output = parsed(&output);

    assert_eq!(output.producer.format, "trainwreck");
    assert_eq!(output.producer.version.as_deref(), Some("1.4.0"));
    let game = &facts(output, "creation.game")[0];
    assert_eq!(text_attr(game, "edition").as_deref(), Some("VR"));
    assert_eq!(bool_attr(game, "vr"), Some(true));

    let exception = &facts(output, "core.exception")[0];
    assert_eq!(text_attr(exception, "class").as_deref(), Some("C06D007E"));
    assert_eq!(text_attr(exception, "category").as_deref(), Some("unknown"));

    let frames = facts(output, "core.frame");
    assert_eq!(frames.len(), 2);
    assert_eq!(
        text_attr(frames[0], "symbol").as_deref(),
        Some("RaiseExample")
    );
    assert_eq!(
        text_attr(frames[0], "symbol_scheme").as_deref(),
        Some("pdb")
    );
    assert!(text_attr(frames[0], "source_file")
        .unwrap()
        .ends_with("bridge.cpp"));
    assert_eq!(number_attr(frames[0], "line"), Some(301.0));

    let modules = facts(output, "core.module");
    assert_eq!(modules.len(), 2);
    let versions: Vec<String> = modules
        .iter()
        .filter_map(|fact| text_attr(fact, "version"))
        .collect();
    assert!(versions.contains(&"1.0.0".to_string()));
    assert!(versions.contains(&"1.4.15".to_string()));
    assert_eq!(facts(output, "creation.module_hash").len(), 2);

    let skse = facts(output, "creation.script_extender_plugin");
    assert_eq!(skse.len(), 2);
    assert!(skse
        .iter()
        .any(|fact| text_attr(fact, "version").as_deref() == Some("1.0.0")));

    assert_eq!(coverage(output, "creation.plugin"), "NotApplicable");
    assert_eq!(coverage(output, "creation.plugin_chain"), "Checked");
}

#[test]
fn trainwreck_1_2_reads_symbols_and_module_versions() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash_tw12.log",
        &fixture("trainwreck_1_2.txt"),
        "trainwreck",
    );
    let output = parsed(&output);

    let frames = facts(output, "core.frame");
    assert_eq!(frames.len(), 2);
    assert!(text_attr(frames[0], "symbol").is_none());
    assert_eq!(
        text_attr(frames[1], "symbol").as_deref(),
        Some("ExampleHook")
    );
    assert_eq!(
        text_attr(facts(output, "core.module")[1], "version").as_deref(),
        Some("1.2.0")
    );
}

#[test]
fn a_truncated_log_keeps_what_it_has_and_reports_absent_sections() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-truncated.log",
        &fixture("truncated.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    assert!(!facts(output, "core.exception").is_empty());
    assert_eq!(facts(output, "core.frame").len(), 1);
    assert_eq!(coverage(output, "core.module"), "Absent");
    assert_eq!(coverage(output, "creation.plugin"), "Absent");
    assert_eq!(
        coverage(output, "creation.script_extender_plugin"),
        "Absent"
    );
}

#[test]
fn a_missing_section_is_absent_not_a_clean_bill_of_health() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-missing.log",
        &fixture("missing_section.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    assert_eq!(coverage(output, "creation.plugin"), "Absent");
    assert!(output.coverage.iter().any(|entry| {
        entry.kind == "creation.plugin" && entry.why.as_deref().unwrap_or("").contains("PLUGINS")
    }));
    assert_eq!(facts(output, "creation.plugin").len(), 0);
}

#[test]
fn a_plugin_list_shorter_than_its_own_summary_is_not_checked_for_absence() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-short-list.log",
        &fixture("incomplete_plugins.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    assert_eq!(
        coverage(output, "creation.plugin"),
        "Absent",
        "a list that stops before its own Total cannot prove a plugin is missing"
    );
    assert!(
        output
            .coverage
            .iter()
            .any(|entry| entry.kind == "creation.plugin"
                && entry.why.as_deref().unwrap_or("").contains("summary")),
        "{:#?}",
        output.coverage
    );
    assert!(
        facts(output, "creation.plugin").is_empty(),
        "a partial list must not be reported as checked facts"
    );
    assert_eq!(
        coverage(output, "creation.load_order_summary"),
        "Checked",
        "the summary line itself was read"
    );
}

fn one_exception_log(code: &str) -> String {
    format!(
        "Skyrim SSE v1.6.1170\nCrashLoggerSSE v1-16-0-0 Jan  1 2026 00:00:00\n\n\
Unhandled exception \"{code}\" at 0x7FF700001000 ExampleGame.exe+001000\tmov rax, [rcx]\n\n\
PROBABLE CALL STACK:\n\t[ 0] 0x7FF700001000 ExampleGame.exe+001000\n"
    )
}

#[test]
fn the_exception_category_keeps_the_specific_failure_apart() {
    let running = start();
    let category = |code: &str| {
        let output = parse_with_format(
            &running,
            "crash-category.log",
            one_exception_log(code).as_bytes(),
            "crash_logger",
        );
        let output = parsed(&output);
        text_attr(facts(output, "core.exception")[0], "category").unwrap_or_default()
    };

    assert_eq!(category("EXCEPTION_ACCESS_VIOLATION"), "access_violation");
    assert_eq!(category("C0000006"), "in_page_error");
    assert_eq!(category("EXCEPTION_STACK_OVERFLOW"), "stack_overflow");
    assert_eq!(category("C0000374"), "heap_corruption");
    assert_eq!(category("EXCEPTION_BREAKPOINT"), "assert");
}

#[test]
fn an_in_page_error_line_without_a_code_is_still_an_in_page_error() {
    let running = start();
    let text = one_exception_log("EXCEPTION_ACCESS_VIOLATION")
        .replace("\"EXCEPTION_ACCESS_VIOLATION\" ", "")
        .replace(
            "mov rax, [rcx]",
            "mov rax, [rcx]\nIn-Page Error: Tried to read memory at 0x000000000010",
        );
    let output = parse_with_format(&running, "crash-page.log", text.as_bytes(), "crash_logger");
    let output = parsed(&output);
    assert_eq!(
        text_attr(facts(output, "core.exception")[0], "category").as_deref(),
        Some("in_page_error")
    );
}

#[test]
fn a_lost_gpu_device_in_a_cpp_message_is_its_own_category() {
    let running = start();
    let text = one_exception_log("C++ Exception").replace(
        "\nPROBABLE CALL STACK:",
        "\nC++ EXCEPTION:\n\tType: (std::runtime_error)\n\tInfo: DXGI_ERROR_DEVICE_REMOVED\n\nPROBABLE CALL STACK:",
    );
    let output = parse_with_format(&running, "crash-gpu.log", text.as_bytes(), "crash_logger");
    let output = parsed(&output);
    let cpp = facts(output, "core.exception")
        .into_iter()
        .find(|fact| number_attr(fact, "depth") == Some(1.0))
        .expect("the C++ exception is one level in");
    assert_eq!(
        text_attr(cpp, "category").as_deref(),
        Some("gpu_device_lost")
    );
    assert_eq!(
        text_attr(cpp, "message").as_deref(),
        Some("DXGI_ERROR_DEVICE_REMOVED")
    );
}

#[test]
fn a_line_over_the_read_limit_is_cut_and_reported() {
    let running = start();
    let output = parse_with_format(
        &running,
        "crash-long.log",
        &fixture("long_line.txt"),
        "crash_logger",
    );
    let output = parsed(&output);

    assert!(output
        .notes
        .iter()
        .any(|note| note.kind == agora_game_api::crash::ParseNoteKind::Truncated));
    assert!(!facts(output, "creation.plugin_chain").is_empty());
}

#[test]
fn windows_and_unix_line_endings_parse_the_same() {
    let running = start();
    let lf_bytes = fixture("crash_logger_lf.txt");
    let crlf_bytes = String::from_utf8(lf_bytes.clone())
        .unwrap()
        .replace('\n', "\r\n")
        .into_bytes();
    let lf = parse_with_format(&running, "crash-lf.log", &lf_bytes, "crash_logger");
    let crlf = parse_with_format(&running, "crash-crlf.log", &crlf_bytes, "crash_logger");
    let lf = parsed(&lf);
    let crlf = parsed(&crlf);

    assert_eq!(lf.producer, crlf.producer);
    assert_eq!(
        facts(lf, "core.exception").len(),
        facts(crlf, "core.exception").len()
    );
    assert_eq!(
        facts(lf, "core.frame").len(),
        facts(crlf, "core.frame").len()
    );
    assert_eq!(
        facts(lf, "creation.plugin").len(),
        facts(crlf, "creation.plugin").len()
    );
}
