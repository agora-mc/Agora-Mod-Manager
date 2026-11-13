//! Pasted fragments: a snippet parser may read text the user pasted when no whole-file parser
//! claimed it, and the answer says so. Every fragment and every name here is invented for these
//! tests; none is copied from a real log.

use agora_core::crash_engine::{
    analyze, AnalyzeInput, AnalyzeOutput, CrashFile, CrashFileOrigin, SNIPPET_TRUST_NOTE,
};
use agora_core::crash_packs::{load_built_in_packs, load_plugin_packs, LoadedCrashPack};
use agora_game_api::crash::{BuiltInPack, Level, MatchedBy, Owner, SessionVerdict};
use agora_plugin_api::host::{ActivationRequest, HostBridge, ScriptHost};
use agora_plugin_api::manifest::{PluginId, PluginManifest};
use agora_plugin_api::protocol::{HostRequest, HostResponse, LogLevel};
use agora_plugin_host::QuickJsHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

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

struct Running {
    packs: Vec<LoadedCrashPack>,
    built_in: Vec<BuiltInPack>,
    host: QuickJsHost,
    _data: Option<TempDir>,
}

fn minecraft() -> Running {
    let built_in = agora_game_minecraft::crash::built_in_packs();
    let report = load_built_in_packs(&built_in);
    assert!(report.is_clean(), "{:#?}", report.problems);
    Running {
        packs: report.packs,
        built_in,
        host: QuickJsHost::new(),
        _data: None,
    }
}

fn official_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/official/creation-engine")
}

fn creation_engine() -> Running {
    let root = official_root();
    let text = std::fs::read_to_string(root.join("agora-plugin.json")).unwrap();
    let manifest = PluginManifest::parse(&text).unwrap();
    let report = load_plugin_packs("agora.creation-engine", &root, &manifest);
    assert!(report.is_clean(), "{:#?}", report.problems);
    let data = tempfile::tempdir().unwrap();
    let host = QuickJsHost::new();
    host.activate(
        ActivationRequest {
            plugin_id: PluginId::parse("agora.creation-engine").unwrap(),
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
        packs: report.packs,
        built_in: Vec::new(),
        host,
        _data: Some(data),
    }
}

fn run(running: &Running, files: &[CrashFile]) -> AnalyzeOutput {
    analyze(&AnalyzeInput {
        packs: &running.packs,
        files,
        runtime_version: None,
        host: &running.host,
        built_in: &running.built_in,
        instance: None,
    })
    .unwrap()
}

fn owner_name(owner: &Owner) -> String {
    match owner {
        Owner::Item { name, .. } => name.clone(),
        other => format!("{other:?}"),
    }
}

fn candidates(output: &AnalyzeOutput) -> Vec<String> {
    output
        .answer
        .candidates
        .iter()
        .map(|candidate| owner_name(&candidate.owner))
        .collect()
}

fn has_snippet_note(output: &AnalyzeOutput) -> bool {
    output
        .answer
        .trust_notes
        .iter()
        .any(|note| note.text.get("en").map(String::as_str) == Some(SNIPPET_TRUST_NOTE))
}

/// A pasted fragment gets a real answer, the snippet note, and never a level above `possible`.
fn assert_snippet_answer(output: &AnalyzeOutput, label: &str) {
    assert!(
        output.read_any(),
        "{label}: nothing was read: {:#?}",
        output.files
    );
    assert!(output.files[0].snippet, "{label}: {:#?}", output.files);
    assert!(
        has_snippet_note(output),
        "{label}: {:#?}",
        output.answer.trust_notes
    );
    assert!(
        output.answer.level <= Level::Possible,
        "{label}: level {:?}",
        output.answer.level
    );
    assert!(
        !output.answer.findings.is_empty() || !output.answer.candidates.is_empty(),
        "{label}: no answer: {:#?}",
        output.answer
    );
}

/// The eight old-tool fixtures the comparison marked "old better" only because they are
/// fragments, rebuilt with invented names. `svc_*` and `ev_*` name the old test that used them.
fn minecraft_fragments() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "diag_duplicate_asm",
            "java.lang.RuntimeException: Found duplicate ASM classes found on classpath: invented-asm-9.3.jar, invented-asm-9.10.1.jar\n",
        ),
        (
            "diag_mixin_conflict",
            "[06:12:33] [Worker-3/FABRIC]: Mixin apply failed examplemix.mixins.json:core.mixins.json:CoreMixin -> net.minecraft.class_123\n\
java.lang.RuntimeException: @Overwrite sentinel mismatch\n\
\tat org.spongepowered.asm.mixin.injection.callback.InventedCallbackInjection.inject(InventedCallbackInjection.java:42)\n",
        ),
        (
            "diag_oom",
            "Caused by: java.lang.OutOfMemoryError: Java heap space\n",
        ),
        (
            "ev_mixin_crash",
            "Mixin apply failed examplegarden.mixins.json:GardenMixin -> net.minecraft.class_2000\n\
java.lang.RuntimeException: sentinel mismatch\n\
\tat org.spongepowered.asm.mixin.injection.callback.InventedInjector.inject(InventedInjector.java:77)\n",
        ),
        (
            "ev_oom_latest_log",
            "[INFO] Loading the game\n[ERROR] java.lang.OutOfMemoryError: Java heap space\n",
        ),
        (
            "svc_empty_message_npe",
            "java.lang.NullPointerException:\n\tat com.example.invented.Main.tick(Main.java:1)\n",
        ),
        (
            "svc_no_such_method",
            "java.lang.NoSuchMethodError: 'net.minecraft.invented.Widget net.minecraft.invented.Widget.inventedMethod(net.minecraft.invented.Holder)'\n\tat com.example.invented.Main.tick(Main.java:1)\n",
        ),
        (
            "svc_sample_crash_log",
            "Java HotSpot(TM) 64-Bit Server VM warning: Sharing is only supported for boot loader classes\n\
Exception in thread \"Render thread\" java.lang.RuntimeException: An invented crash happened!\n\
    at net.minecraft.client.invented.InventedMain.main(InventedMain.java:42)\n\
Caused by: java.lang.IllegalStateException: The first invented cause\n\
    at com.example.invented.Core.init(Core.java:10)\n\
Caused by: java.lang.NullPointerException\n\
    at com.example.invented.mixin.InventedFoo.render(InventedFoo.java:45)\n\
    at com.example.invented.Core.onTick(Core.java:100)\n\
    at net.minecraft.client.invented.InventedMinecraft.tick(InventedMinecraft.java:520)\n",
        ),
    ]
}

/// Three more shapes the old loose reader accepted, kept as their own tests. They are not part of
/// the eight: the comparison marked these usable for a whole file too.
fn dependency_fragments() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "fabric_missing_api",
            "net.fabricmc.loader.impl.discovery.ModResolutionException: Mod resolution encountered an incompatible mod set!\n\
A potential solution has been determined, this may resolve your problem:\n\
\t - Install fabric-api, any version.\n\
Unmet dependency listing:\n\
\t - Mod 'Example Enchanting' (exampleench) 1.2.3 requires any version of fabric-api, which is missing!\n\
\t - Mod 'Example Map' (examplemap) 4.5.6 requires version 0.9.0 or later of fabric-api, which is missing!\n",
        ),
        (
            "mod_resolution",
            "net.fabricmc.loader.impl.discovery.ModResolutionException: Some of your mods are incompatible with the game or each other!\n",
        ),
        (
            "named_targets",
            "<log4j:Message><![CDATA[Mod resolution encountered an incompatible mod set!\n\
\t - Mod 'Example Menu' (examplemenu) 18.0.2 requires version 0.130.0 or later of Fabric API (fabric-api), which is missing!\n\
\t - Mod 'Invented Iris' (inventediris) 1.11.4 requires version 0.9.2 or later of 'Sodium' (sodium), which is missing!\n\
]]></log4j:Message>\n",
        ),
    ]
}

#[test]
fn each_old_fragment_shape_answers_with_the_snippet_note() {
    let running = minecraft();
    for (label, text) in minecraft_fragments() {
        let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
        assert_snippet_answer(&output, label);
        // The snippet parser, not a whole-file parser, read it.
        let reader = output.files[0].read_by.as_ref().expect("a reader");
        assert_eq!(reader.parser_id, "snippet", "{label}");
        assert!(
            output
                .answer
                .trust_notes
                .iter()
                .any(|note| note.source.parser_id.as_deref() == Some("snippet")),
            "{label}: the note must name its source"
        );
    }
}

#[test]
fn the_same_fragment_as_a_picked_file_is_nothing_to_read() {
    let running = minecraft();
    for (label, text) in minecraft_fragments() {
        let output = run(&running, &[CrashFile::new("picked.txt", text)]);
        assert!(
            !output.read_any(),
            "{label}: a picked file used the fallback"
        );
        assert!(!output.files[0].snippet, "{label}");
        assert!(output.answer.trust_notes.is_empty(), "{label}");
        let reason = output.files[0].reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains("no installed crash pack recognised"),
            "{label}: {reason}"
        );
    }
}

#[test]
fn a_whole_crash_report_pasted_still_uses_the_normal_parser() {
    let running = minecraft();
    let report = "---- Minecraft Crash Report ----\n\
// An invented crash for these tests.\n\n\
Time: 2026-01-02 03:04:05\n\
Description: Rendering a shiny thing\n\n\
java.lang.IllegalStateException: a shiny thing failed\n\
\tat net.example.shinyshaders.pipeline.ShadowThing.draw(ShadowThing.java:12)\n\
\tat net.minecraft.class_123.method_456(class_123.java:10)\n";
    let output = run(&running, &[CrashFile::pasted("pasted text", report)]);
    let reader = output.files[0].read_by.as_ref().expect("a reader");
    assert_eq!(reader.parser_id, "crash_report");
    assert!(!output.files[0].snippet);
    assert!(
        !has_snippet_note(&output),
        "{:#?}",
        output.answer.trust_notes
    );
    assert!(output.read_any());
}

#[test]
fn an_empty_paste_is_an_explicit_nothing_to_read() {
    let running = minecraft();
    let output = run(&running, &[CrashFile::pasted("pasted text", Vec::new())]);
    assert!(!output.read_any());
    assert!(!output.files[0].snippet);
    assert!(!has_snippet_note(&output));
    let reason = output.files[0].reason.as_deref().unwrap_or_default();
    assert!(reason.contains("empty"), "{reason}");
}

#[test]
fn text_that_is_not_a_log_names_nobody() {
    let running = minecraft();
    let output = run(
        &running,
        &[CrashFile::pasted(
            "pasted text",
            "I tried to run the game and it did not work.\n",
        )],
    );
    assert!(!output.read_any());
    assert!(output.answer.candidates.is_empty());
    assert!(output.answer.findings.is_empty());
    assert!(!has_snippet_note(&output));
}

#[test]
fn a_snippet_parser_can_never_claim_a_collected_file() {
    let running = minecraft();
    let output = run(
        &running,
        &[CrashFile::collected(
            "collected.log",
            "Caused by: java.lang.OutOfMemoryError: Java heap space\n",
        )],
    );
    assert!(!output.read_any(), "{:#?}", output.files);
    assert!(!output.files[0].snippet);
    assert!(!has_snippet_note(&output));
    let reason = output.files[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("no installed crash pack recognised"),
        "{reason}"
    );
}

#[test]
fn the_origin_defaults_to_a_picked_file() {
    assert_eq!(CrashFile::new("x", "y").origin, CrashFileOrigin::File);
    assert_eq!(CrashFile::pasted("x", "y").origin, CrashFileOrigin::Pasted);
    assert_eq!(
        CrashFile::collected("x", "y").origin,
        CrashFileOrigin::Collected
    );
}

// ---------------------------------------------------------------------------
// Skyrim
// ---------------------------------------------------------------------------

#[test]
fn a_pasted_skyrim_stack_names_the_module_on_it() {
    let running = creation_engine();
    let text = "Unhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF700001000 InventedMod.dll+001000\tmov rax, [rcx]\n\
\t[ 0] 0x7FF700001000 InventedMod.dll+001000\n\
\t[ 1] 0x7FF700002000 SkyrimSE.exe+002000\n";
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert_snippet_answer(&output, "skyrim_stack");
    assert_eq!(
        output.files[0].read_by.as_ref().unwrap().parser_id,
        "fragment"
    );
    assert!(
        candidates(&output).contains(&"InventedMod.dll".to_string()),
        "{:?}",
        candidates(&output)
    );
}

#[test]
fn a_pasted_skyrim_file_line_names_the_plugin() {
    let running = creation_engine();
    let text = "\t\tFile: \"InventedPatch.esp\"\n\
\t\tModified by: \"InventedPatch.esp\" -> \"InventedPatch.esm\"\n";
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert_snippet_answer(&output, "skyrim_file");
    assert!(
        candidates(&output).contains(&"InventedPatch.esp".to_string()),
        "{:?}",
        candidates(&output)
    );
}

#[test]
fn a_pasted_module_and_plugin_list_is_read_without_their_headers() {
    let running = creation_engine();
    let text = "\tInventedMod.dll                       0x7FF700000000\n\
\tSkyrimSE.exe                          0x7FF600000000\n\
\tInventedHelper.dll v1.2.3\n\
\t[ 0]     Skyrim.esm\n\
\t[ 1]     InventedPatch.esp\n";
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert!(output.read_any(), "{:#?}", output.files);
    assert!(output.files[0].snippet);
    assert!(has_snippet_note(&output));
    let facts = serde_json::to_string(
        &output
            .parsed
            .iter()
            .flat_map(|parsed| parsed.output.facts.iter())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(facts.contains("InventedMod.dll"), "{facts}");
    assert!(facts.contains("InventedHelper.dll"), "{facts}");
    assert!(facts.contains("InventedPatch.esp"), "{facts}");
}

#[test]
fn the_same_skyrim_fragment_as_a_picked_file_is_nothing_to_read() {
    let running = creation_engine();
    let text = "Unhandled exception \"EXCEPTION_ACCESS_VIOLATION\" at 0x7FF700001000 InventedMod.dll+001000\tmov rax, [rcx]\n\
\t[ 0] 0x7FF700001000 InventedMod.dll+001000\n";
    let output = run(&running, &[CrashFile::new("picked-crash.log", text)]);
    assert!(!output.read_any());
    assert!(!output.files[0].snippet);
    assert!(!has_snippet_note(&output));
}

/// Prints what the snippet fallback says for each of the eight old-tool fragment shapes, for
/// `.spike/out/report.md`. Ignored: it is a measurement, not a gate.
#[test]
#[ignore]
fn measure_fragment_answers() {
    let running = minecraft();
    println!("| fragment | read by | level | candidates | findings (rule ids) |");
    println!("|---|---|---|---|---|");
    for (label, text) in minecraft_fragments() {
        let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
        let reader = output
            .files
            .first()
            .and_then(|file| file.read_by.as_ref())
            .map(|reader| reader.parser_id.clone())
            .unwrap_or_else(|| "nothing".to_string());
        let names = candidates(&output).join(", ");
        let mut rules: Vec<String> = output
            .answer
            .findings
            .iter()
            .filter_map(|finding| finding.source.rule_id.clone())
            .collect();
        rules.sort();
        rules.dedup();
        println!(
            "| {label} | {reader} | {:?} | {} | {} |",
            output.answer.level,
            if names.is_empty() { "none" } else { &names },
            if rules.is_empty() {
                "none".to_string()
            } else {
                rules.join(", ")
            }
        );
    }
}

#[test]
fn a_pasted_dependency_listing_names_the_missing_mod_and_who_needs_it() {
    let running = minecraft();
    let text = dependency_fragments()[0].1;
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert_snippet_answer(&output, "fabric_missing_api");
    let facts = serde_json::to_string(
        &output
            .parsed
            .iter()
            .flat_map(|parsed| parsed.output.facts.iter())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(facts.contains("fabric-api"), "{facts}");
    assert!(facts.contains("exampleench"), "{facts}");
    assert!(
        output
            .answer
            .findings
            .iter()
            .any(|finding| { finding.source.rule_id.as_deref() == Some("missing_fabric_api") }),
        "{:#?}",
        output.answer.findings
    );
}

#[test]
fn a_pasted_resolution_failure_is_recognised() {
    let running = minecraft();
    let text = dependency_fragments()[1].1;
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert_snippet_answer(&output, "mod_resolution");
    assert!(
        output
            .answer
            .findings
            .iter()
            .any(|finding| finding.source.rule_id.as_deref() == Some("mod_resolution_failed")),
        "{:#?}",
        output.answer.findings
    );
}

#[test]
fn a_pasted_named_dependency_target_is_read() {
    let running = minecraft();
    let text = dependency_fragments()[2].1;
    let output = run(&running, &[CrashFile::pasted("pasted text", text)]);
    assert_snippet_answer(&output, "named_targets");
    let facts = serde_json::to_string(
        &output
            .parsed
            .iter()
            .flat_map(|parsed| parsed.output.facts.iter())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(facts.contains("fabric-api"), "{facts}");
    assert!(facts.contains("sodium"), "{facts}");
}

#[test]
fn a_verdict_still_says_the_user_supplied_the_evidence() {
    let running = minecraft();
    let output = run(
        &running,
        &[CrashFile::pasted(
            "pasted text",
            "java.lang.OutOfMemoryError: Java heap space\n",
        )],
    );
    assert_eq!(
        output.answer.verdict,
        SessionVerdict::Crashed {
            how: MatchedBy::Supplied
        }
    );
}
