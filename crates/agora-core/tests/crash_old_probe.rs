//! Manual harness for the pre-engine Minecraft crash diagnoser (`crash_service` and
//! `crash_diagnostics`). It is not part of the product and runs only when asked for:
//!
//! ```text
//! AGORA_OLD_PROBE_IN=<folder of .log/.txt files> \
//! AGORA_OLD_PROBE_OUT=<json file> \
//! cargo test -p agora-core --test crash_old_probe -- --ignored --nocapture
//! ```
//!
//! Every file runs through the same three hand-callable steps the old tool offers without an
//! instance: the curated signature triage, the crash fingerprint, and the missing-dependency
//! reader. A file that cannot be read is reported as an error, never skipped.
//!
//! This exists so the old and new Minecraft diagnosers can be compared over the same files. It
//! changes no behaviour: an ignored integration test is built only for tests.

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::Path;

#[test]
#[ignore = "manual old-vs-new comparison harness; sets AGORA_OLD_PROBE_IN/OUT"]
fn probe_the_old_crash_diagnoser() {
    let input = env::var("AGORA_OLD_PROBE_IN").expect("AGORA_OLD_PROBE_IN is required");
    let output = env::var("AGORA_OLD_PROBE_OUT").expect("AGORA_OLD_PROBE_OUT is required");
    let input = Path::new(&input);

    let mut entries: Vec<_> = fs::read_dir(input)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", input.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.is_file())
        .collect();
    entries.sort();

    let mut rows: Vec<Value> = Vec::with_capacity(entries.len());
    for path in entries {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let row = match fs::read_to_string(&path) {
            Ok(text) => probe_one(&name, &text),
            Err(error) => json!({
                "name": name,
                "error": format!("could not read the file: {error}"),
            }),
        };
        rows.push(row);
    }

    let serialized = serde_json::to_string_pretty(&rows).expect("the probe rows serialize");
    fs::write(&output, serialized)
        .unwrap_or_else(|error| panic!("could not write {output}: {error}"));
}

fn probe_one(name: &str, text: &str) -> Value {
    let triage = agora_core::crash_diagnostics::triage(text);
    let fingerprint = agora_core::crash_service::parse_crash_log(text);
    let missing = agora_core::crash_diagnostics::missing_dependencies(text);

    json!({
        "name": name,
        "empty": text.is_empty(),
        "triage": {
            "matched": triage.matched,
            "signature_name": triage.signature_name,
            "solution_markdown": triage.solution_markdown,
            "action_button_json": triage.action_button_json,
        },
        "fingerprint": fingerprint.map(|fingerprint| json!({
            "exception_class": fingerprint.exception_class,
            "top_frames": fingerprint.top_frames,
        })),
        "missing_dependencies": missing.iter().map(|item| json!({
            "missing_id": item.missing_id,
            "missing_name": item.missing_name,
            "requirement": item.requirement,
            "required_by_name": item.required_by_name,
            "required_by_id": item.required_by_id,
        })).collect::<Vec<_>>(),
    })
}
