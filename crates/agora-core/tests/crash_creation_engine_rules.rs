//! The official Creation Engine pack's rules, end to end: an invented Crash Logger log goes through
//! the real script parser and the real pack, and each rule fires where it should and stays quiet
//! where it should not. Every log and mod name here is invented; only the game's own module and
//! plugin names are real, because the roles under test name them.

use agora_core::crash_engine::{analyze, AnalyzeInput, AnalyzeOutput, CrashFile};
use agora_core::crash_packs::{load_plugin_packs, LoadedCrashPack};
use agora_game_api::crash::{Level, Owner};
use agora_plugin_api::host::{ActivationRequest, HostBridge, ScriptHost};
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

const PLUGIN: &str = "agora.creation-engine";

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

struct Running {
    _data: TempDir,
    host: QuickJsHost,
    pack: LoadedCrashPack,
}

fn start() -> Running {
    let root = official_root();
    let text = std::fs::read_to_string(root.join("agora-plugin.json")).unwrap();
    let manifest = PluginManifest::parse(&text).unwrap();
    let report = load_plugin_packs(PLUGIN, &root, &manifest);
    assert!(report.is_clean(), "{:#?}", report.problems);
    let pack = report.packs[0].clone();
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

/// A Crash Logger 1.16 log with the given exception code, call stack lines and register lines.
fn log(exception: &str, stack: &[&str], registers: &[&str], stack_slots: &[&str]) -> String {
    let mut text = format!(
        "Skyrim SSE v1.6.1170\nCrashLoggerSSE v1-16-0-0 Jan  1 2026 00:00:00\n\n\
Unhandled exception \"{exception}\" at 0x7FF700001000 SkyrimSE.exe+001000\tmov rax, [rcx]\n\n\
SYSTEM SPECS:\n\tOS: Microsoft Windows 11 Pro v10.0.26200\n\tCPU: InventedVendor Invented CPU 9200\n\
\tGPU #1: Invented Graphics 9200\n\tPHYSICAL MEMORY: 15.50 GB/16.00 GB\n\nPROBABLE CALL STACK:\n"
    );
    for (index, module) in stack.iter().enumerate() {
        text.push_str(&format!(
            "\t[{index:2}] 0x7FF70000{index}000 {module}+00{index}000\n"
        ));
    }
    text.push_str("\nREGISTERS:\n");
    for line in registers {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\nSTACK:\n");
    for line in stack_slots {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(
        "\nMODULES:\n\tSkyrimSE.exe                 0x7FF700000000\n\nSKSE PLUGINS:\n\tInventedMod.dll v1.0.0\n\n\
PLUGINS:\n\tLight: 0\tRegular: 2\tTotal: 2\n\t[ 0]     Skyrim.esm\n\t[ 1]     InventedPatch.esp\n",
    );
    text
}

fn object_in(place: &str, file: &str) -> [String; 4] {
    [
        format!("\t{place} 0x2F000000000      (TESObjectREFR*)"),
        format!("\t\tFile: \"{file}\""),
        "\t\tFormID: 0x00012345".to_string(),
        "\t\tFormType: Reference (61)".to_string(),
    ]
}

fn run(running: &Running, text: &str) -> AnalyzeOutput {
    let output = analyze(&AnalyzeInput {
        packs: std::slice::from_ref(&running.pack),
        files: &[CrashFile {
            name: "crash-2026-01-01-12-00-00.log".into(),
            bytes: text.as_bytes().to_vec(),
        }],
        runtime_version: None,
        host: &running.host,
        built_in: &[],
        instance: None,
    })
    .unwrap();
    assert!(output.read_any(), "{:#?}", output.files);
    output
}

fn names(output: &AnalyzeOutput) -> Vec<String> {
    output
        .answer
        .candidates
        .iter()
        .map(|candidate| match &candidate.owner {
            Owner::Item { name, .. } => name.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn rule_fired(output: &AnalyzeOutput, rule: &str) -> bool {
    output
        .answer
        .findings
        .iter()
        .any(|finding| finding.source.rule_id.as_deref() == Some(rule))
}

fn lines(groups: &[&[String]]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| group.iter().cloned())
        .collect()
}

#[test]
fn a_mod_dll_near_the_top_of_the_stack_is_possible_and_the_game_is_never_named() {
    let running = start();
    let text = log(
        "EXCEPTION_ACCESS_VIOLATION",
        &[
            "SkyrimSE.exe",
            "KERNELBASE.dll",
            "InventedMod.dll",
            "SkyrimSE.exe",
        ],
        &[],
        &[],
    );
    let output = run(&running, &text);
    assert_eq!(names(&output), vec!["InventedMod.dll"]);
    // One family alone never reads stronger than possible.
    assert_eq!(output.answer.candidates[0].level, Level::Possible);
}

#[test]
fn a_hook_framework_on_top_gives_way_to_a_mod_further_down() {
    let running = start();
    let text = log(
        "EXCEPTION_ACCESS_VIOLATION",
        &["hdtSMP64.dll", "vcruntime140.dll", "InventedMod.dll"],
        &[],
        &[],
    );
    let output = run(&running, &text);
    assert_eq!(
        names(&output).first().map(String::as_str),
        Some("InventedMod.dll")
    );
    assert!(!names(&output).iter().any(|name| name == "vcruntime140.dll"));
}

#[test]
fn a_frame_deep_in_the_stack_alone_names_nobody() {
    let running = start();
    let mut stack = vec!["SkyrimSE.exe"; 6];
    stack.push("InventedMod.dll");
    let output = run(
        &running,
        &log("EXCEPTION_ACCESS_VIOLATION", &stack, &[], &[]),
    );
    assert!(output.answer.candidates.is_empty(), "{:?}", names(&output));
}

#[test]
fn a_plugin_record_in_a_register_is_named_but_the_base_game_is_not() {
    let running = start();
    let mod_record = object_in("RDX", "InventedPatch.esp");
    let base_record = object_in("RCX", "Skyrim.esm");
    let text = log(
        "EXCEPTION_ACCESS_VIOLATION",
        &["SkyrimSE.exe", "SkyrimSE.exe"],
        &lines(&[&base_record, &mod_record])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        &[],
    );
    let output = run(&running, &text);
    assert_eq!(names(&output), vec!["InventedPatch.esp"]);
}

#[test]
fn the_unofficial_patch_gives_way_to_another_plugin() {
    let running = start();
    let ussep = object_in("RCX", "Unofficial Skyrim Special Edition Patch.esp");
    let other = object_in("RDX", "InventedPatch.esp");
    let both = lines(&[&ussep, &other]);
    let output = run(
        &running,
        &log(
            "EXCEPTION_ACCESS_VIOLATION",
            &["SkyrimSE.exe"],
            &both.iter().map(String::as_str).collect::<Vec<_>>(),
            &[],
        ),
    );
    assert_eq!(names(&output), vec!["InventedPatch.esp"]);
}

#[test]
fn a_record_only_on_the_stack_names_nobody() {
    let running = start();
    let slot = object_in("[RSP+48  ]", "InventedPatch.esp");
    let output = run(
        &running,
        &log(
            "EXCEPTION_ACCESS_VIOLATION",
            &["SkyrimSE.exe"],
            &[],
            &slot.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
    );
    assert!(output.answer.candidates.is_empty(), "{:?}", names(&output));
    // Not vacuous: the slot was read, with its plugin, and still named nobody.
    let read_slot = output.parsed.iter().any(|parsed| {
        parsed.output.facts.iter().any(|fact| {
            fact.kind == "creation.object"
                && fact
                    .refs
                    .iter()
                    .any(|reference| reference.value == "InventedPatch.esp")
        })
    });
    assert!(read_slot);
}

#[test]
fn out_of_memory_is_a_finding_and_caps_any_mod_at_possible() {
    let running = start();
    let record = object_in("RDX", "InventedPatch.esp");
    let output = run(
        &running,
        &log(
            "EXCEPTION_OUT_OF_MEMORY",
            &["InventedMod.dll"],
            &record.iter().map(String::as_str).collect::<Vec<_>>(),
            &[],
        ),
    );
    assert!(rule_fired(&output, "out_of_memory"));
    // The mod is still listed, never hidden; the cap says how far to trust it. (A loose file's one
    // family already tops out at possible, so the cap itself only bites in a live session.)
    assert!(names(&output).contains(&"InventedMod.dll".to_string()));
    assert!(output.answer.level <= Level::Possible);
    let notes = serde_json::to_string(&output.answer.trust_notes).unwrap();
    assert!(notes.contains("ran out of memory"), "{notes}");
}

#[test]
fn an_access_violation_with_memory_nearly_full_is_not_called_out_of_memory() {
    // The template's RAM is 97% in use: normal in real logs, and not evidence on its own.
    let running = start();
    let output = run(
        &running,
        &log("EXCEPTION_ACCESS_VIOLATION", &["InventedMod.dll"], &[], &[]),
    );
    assert!(!rule_fired(&output, "out_of_memory"));
}

#[test]
fn a_failed_delay_load_is_a_finding_that_names_nobody() {
    let running = start();
    let output = run(&running, &log("C06D007E", &["SkyrimSE.exe"], &[], &[]));
    assert!(rule_fired(&output, "dll_dependency_failed_to_load"));
    assert!(output.answer.candidates.is_empty());
}

/// The template on another game build, with one more SKSE plugin listed.
fn on_build(text: &str, build: &str, extra_skse: Option<&str>) -> String {
    let mut text = text.replace("Skyrim SSE v1.6.1170", &format!("Skyrim SSE v{build}"));
    if let Some(line) = extra_skse {
        text = text.replace("SKSE PLUGINS:\n", &format!("SKSE PLUGINS:\n\t{line}\n"));
    }
    text
}

fn signalled(output: &AnalyzeOutput, rule: &str) -> bool {
    output.answer.candidates.iter().any(|candidate| {
        candidate
            .sources
            .iter()
            .any(|source| source.rule_id.as_deref() == Some(rule))
    })
}

#[test]
fn an_old_skyclimb_on_a_new_game_build_is_named_and_a_current_one_is_not() {
    let running = start();
    let base = log("EXCEPTION_ACCESS_VIOLATION", &["SkyrimSE.exe"], &[], &[]);
    let old = run(
        &running,
        &on_build(&base, "1.6.1170", Some("SkyClimb.dll v1.1.0")),
    );
    assert!(signalled(&old, "skyclimb_outdated"), "{:?}", names(&old));
    assert_eq!(
        names(&old).first().map(String::as_str),
        Some("SkyClimb.dll")
    );

    let current = run(
        &running,
        &on_build(&base, "1.6.1170", Some("SkyClimb.dll v1.2.0.1")),
    );
    assert!(!signalled(&current, "skyclimb_outdated"));

    // Before 1.6.1130 the old build was the right one.
    let older_game = run(
        &running,
        &on_build(&base, "1.6.640", Some("SkyClimb.dll v1.1.0")),
    );
    assert!(!signalled(&older_game, "skyclimb_outdated"));
}

#[test]
fn the_missing_master_site_is_recognised_only_on_its_own_game_build() {
    let running = start();
    let at_site = log("EXCEPTION_ACCESS_VIOLATION", &["SkyrimSE.exe"], &[], &[])
        .replace("SkyrimSE.exe+001000", "SkyrimSE.exe+0198090");
    let on_640 = run(&running, &on_build(&at_site, "1.6.640", None));
    assert!(rule_fired(&on_640, "missing_masters_known_site"));
    assert!(on_640.answer.candidates.is_empty(), "it accuses nobody");
    // The same raw offset on another build is a different place in the code.
    let on_1170 = run(&running, &on_build(&at_site, "1.6.1170", None));
    assert!(!rule_fired(&on_1170, "missing_masters_known_site"));
}

fn mesh_in(place: &str, name: &str) -> [String; 2] {
    [
        format!("\t{place} 0x2F000000000      (BSDynamicTriShape*)"),
        format!("\t\tName: \"{name}\""),
    ]
}

fn registers(groups: &[&[String]]) -> String {
    log(
        "EXCEPTION_ACCESS_VIOLATION",
        &["SkyrimSE.exe"],
        &lines(groups).iter().map(String::as_str).collect::<Vec<_>>(),
        &[],
    )
}

#[test]
fn a_hair_mesh_in_use_is_a_finding_and_with_smp_it_says_so() {
    let running = start();
    let hair = mesh_in("R13", "HairFemaleNord01");
    let plain = run(&running, &registers(&[&hair]));
    assert!(rule_fired(&plain, "hair_mesh_in_use"));
    assert!(!rule_fired(&plain, "hair_mesh_with_smp"));
    assert!(plain.answer.candidates.is_empty(), "it accuses nobody");

    let with_smp = run(
        &running,
        &on_build(&registers(&[&hair]), "1.6.1170", Some("hdtSMP64.dll")),
    );
    assert!(rule_fired(&with_smp, "hair_mesh_with_smp"));
    assert!(
        !rule_fired(&with_smp, "hair_mesh_in_use"),
        "the SMP wording replaces the plain one"
    );
}

#[test]
fn a_mesh_that_is_not_hair_is_not_a_hair_finding() {
    let running = start();
    let other = mesh_in("R13", "CrosshairMarker");
    let output = run(&running, &registers(&[&other]));
    assert!(!rule_fired(&output, "hair_mesh_in_use"));
    assert!(!rule_fired(&output, "hair_mesh_with_smp"));
}

#[test]
fn an_old_engine_fixes_is_noted_only_on_the_builds_it_applies_to() {
    let running = start();
    let base = log("EXCEPTION_ACCESS_VIOLATION", &["SkyrimSE.exe"], &[], &[]);
    let old = run(
        &running,
        &on_build(&base, "1.6.1170", Some("EngineFixes.dll v7.0.19")),
    );
    assert!(rule_fired(&old, "engine_fixes_outdated"));
    let current = run(
        &running,
        &on_build(&base, "1.6.1170", Some("EngineFixes.dll v7.0.20")),
    );
    assert!(!rule_fired(&current, "engine_fixes_outdated"));
    let other_build = run(
        &running,
        &on_build(&base, "1.6.640", Some("EngineFixes.dll v7.0.19")),
    );
    assert!(!rule_fired(&other_build, "engine_fixes_outdated"));
}

#[test]
fn a_shadow_crash_names_the_engine_fixes_update_only_when_it_is_too_old() {
    let running = start();
    let shadow = [
        "\tR14 0x2F000000000      (ShadowSceneNode*)".to_string(),
        "\t\tRTTIName: \"ShadowSceneNode\"".to_string(),
    ];
    let text = registers(&[&shadow]);
    let old = run(
        &running,
        &on_build(&text, "1.6.1170", Some("EngineFixes.dll v7.0.10")),
    );
    let fixed = run(
        &running,
        &on_build(&text, "1.6.1170", Some("EngineFixes.dll v7.0.19")),
    );
    let none = run(&running, &on_build(&text, "1.6.1170", None));
    assert!(
        rule_fired(&old, "shadow_scene_crash_engine_fixes_can_prevent"),
        "{:#?}",
        old.parsed
            .iter()
            .flat_map(|parsed| parsed.output.facts.iter())
            .filter(|fact| fact.kind == "creation.object")
            .collect::<Vec<_>>()
    );
    assert!(!rule_fired(
        &fixed,
        "shadow_scene_crash_engine_fixes_can_prevent"
    ));
    assert!(!rule_fired(
        &none,
        "shadow_scene_crash_engine_fixes_can_prevent"
    ));
}

#[test]
fn an_older_crash_logger_is_noted_and_a_current_one_is_not() {
    let running = start();
    let base = log("EXCEPTION_ACCESS_VIOLATION", &["SkyrimSE.exe"], &[], &[]);
    let old = run(&running, &base);
    assert!(rule_fired(&old, "crash_logger_outdated"));
    let current = run(
        &running,
        &base.replace("CrashLoggerSSE v1-16-0-0", "CrashLoggerSSE v1-20-1-0"),
    );
    assert!(!rule_fired(&current, "crash_logger_outdated"));
}

#[test]
fn a_cpp_bad_alloc_is_out_of_memory_and_another_cpp_exception_is_not() {
    let running = start();
    let with_cpp = |kind: &str| {
        log(
            "C++ Exception",
            &["KERNELBASE.dll", "SkyrimSE.exe"],
            &[],
            &[],
        )
        .replace(
            "\nSYSTEM SPECS:",
            &format!("\nC++ EXCEPTION:\n\tType: (std::{kind})\n\tInfo: invented\n\nSYSTEM SPECS:"),
        )
    };
    let oom = run(&running, &with_cpp("bad_alloc"));
    assert!(rule_fired(&oom, "out_of_memory"));
    let other = run(&running, &with_cpp("runtime_error"));
    assert!(!rule_fired(&other, "out_of_memory"));
}
