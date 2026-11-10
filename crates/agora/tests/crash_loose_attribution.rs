//! Naming a mod, not a Java class, when a Minecraft crash file is read on its own.
//!
//! Every log here is invented for these tests. No line is copied from a real crash report.

use agora_core::crash_engine::{analyze, AnalyzeInput, AnalyzeOutput, CrashFile};
use agora_core::crash_packs::{load_built_in_packs, LoadedCrashPack};
use agora_game_api::crash::Owner;
use agora_game_minecraft::crash::built_in_packs;
use agora_plugin_host::QuickJsHost;

/// The report as it comes off a real disk: a normal Fabric crash, an invented mod, a mod list the
/// caller decides on. `None` leaves the list out, `Some("")` writes the header with no rows.
fn report(mods: Option<&str>) -> String {
    let mod_list = match mods {
        Some(rows) => format!("Fabric Mods: \n{rows}"),
        None => String::new(),
    };
    format!(
        "---- Minecraft Crash Report ----
// An invented crash for these tests.

Time: 2026-01-02 03:04:05
Description: Rendering a shiny thing

java.lang.IllegalStateException: a shiny thing failed
\tat net.example.shinyshaders.pipeline.ShadowThing.draw(ShadowThing.java:12)
\tat net.minecraft.class_123.method_456(class_123.java:10)

A detailed walkthrough of the error, its code path and all known details is as follows:
---------------------------------------------------------------------------------------

-- System Details --
Details:
\tMinecraft Version: 1.21.1
\tOperating System: Windows 11 (amd64) version 10.0
\tJava Version: 21.0.7, Example JDK Vendor
\tLaunched Version: fabric-loader-0.16.0-1.21.1
{mod_list}"
    )
}

fn built_ins() -> Vec<LoadedCrashPack> {
    let built_in = built_in_packs();
    let report = load_built_in_packs(&built_in);
    assert!(report.is_clean(), "{:#?}", report.problems);
    report.packs
}

fn analyze_text(text: &str) -> AnalyzeOutput {
    let built_in = built_in_packs();
    let packs = built_ins();
    let host = QuickJsHost::new();
    analyze(&AnalyzeInput {
        packs: &packs,
        files: &[CrashFile {
            name: "pasted-crash.txt".to_string(),
            bytes: text.as_bytes().to_vec(),
        }],
        runtime_version: None,
        host: &host,
        built_in: &built_in,
    })
    .expect("the crash report is read")
}

fn first_candidate(output: &AnalyzeOutput) -> Option<&Owner> {
    output
        .answer
        .candidates
        .first()
        .map(|candidate| &candidate.owner)
}

fn item(key: &str, name: &str) -> Owner {
    Owner::Item {
        key: key.to_string(),
        name: name.to_string(),
    }
}

#[test]
fn a_loose_crash_names_the_mod_its_own_mod_list_names() {
    let output = analyze_text(&report(Some("\t\tshinyshaders: Shiny Shaders 1.2.3\n")));
    assert_eq!(
        first_candidate(&output),
        Some(&item("shinyshaders", "Shiny Shaders")),
        "{:?}",
        output.answer.candidates
    );
}

#[test]
fn a_loose_crash_without_a_mod_list_keeps_the_class_name() {
    let output = analyze_text(&report(None));
    assert_eq!(
        first_candidate(&output),
        Some(&item(
            "net.example.shinyshaders.pipeline.ShadowThing",
            "net.example.shinyshaders.pipeline.ShadowThing",
        )),
        "{:?}",
        output.answer.candidates
    );
}

#[test]
fn a_loose_crash_with_an_empty_mod_list_keeps_the_class_name() {
    let output = analyze_text(&report(Some("")));
    assert_eq!(
        first_candidate(&output),
        Some(&item(
            "net.example.shinyshaders.pipeline.ShadowThing",
            "net.example.shinyshaders.pipeline.ShadowThing",
        )),
        "{:?}",
        output.answer.candidates
    );
}

#[test]
fn a_loose_crash_library_frame_is_never_the_first_candidate() {
    let mut text = report(Some("\t\tshinyshaders: Shiny Shaders 1.2.3\n"));
    text = text.replace(
        "java.lang.IllegalStateException: a shiny thing failed\n",
        "java.lang.IllegalStateException: a shiny thing failed\n\tat com.google.gson.internal.Streams.op(Streams.java:9)\n",
    );
    let output = analyze_text(&text);
    assert_eq!(
        first_candidate(&output),
        Some(&item("shinyshaders", "Shiny Shaders")),
        "{:?}",
        output.answer.candidates
    );
}

#[test]
fn a_loose_crash_lambda_frame_never_makes_a_numeric_candidate() {
    let mut text = report(Some("\t\tshinyshaders: Shiny Shaders 1.2.3\n"));
    text = text.replace(
        "\tat net.minecraft.class_123.method_456(class_123.java:10)\n",
        "\tat net.minecraft.class_123.method_456(class_123.java:10)\n\tat net.minecraft.client.Minecraft$$Lambda$3726/991528828.apply(Unknown Source)\n",
    );
    let output = analyze_text(&text);
    for candidate in &output.answer.candidates {
        if let Owner::Item { key, name } = &candidate.owner {
            assert!(
                !key.chars().all(|c| c.is_ascii_digit())
                    && !name.chars().all(|c| c.is_ascii_digit()),
                "a numeric candidate: {candidate:?}"
            );
        }
    }
    assert_eq!(
        first_candidate(&output),
        Some(&item("shinyshaders", "Shiny Shaders")),
        "{:?}",
        output.answer.candidates
    );
}
