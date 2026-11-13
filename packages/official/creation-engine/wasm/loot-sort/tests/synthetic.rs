//! Synthetic, self-contained fixtures for the module's contract.
//!
//! They prove the ordering, the message wording and the request's active set
//! without needing a real game install or the large spike fixture.

use std::path::Path;

use loot_sort::json;
use loot_sort::run_request;

fn subrecord(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(kind);
    out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    out.extend_from_slice(data);
}

/// A TES4 record with an HEDR subrecord and a MAST/DATA pair per master,
/// laid out the way esplugin reads a Skyrim SE header.
fn plugin_bytes(masters: &[&str], flags: u32) -> Vec<u8> {
    let mut body = Vec::new();
    subrecord(&mut body, b"HEDR", &[0u8; 12]);
    for master in masters {
        let mut name = master.as_bytes().to_vec();
        name.push(0);
        subrecord(&mut body, b"MAST", &name);
        subrecord(&mut body, b"DATA", &[0u8; 8]);
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"TES4");
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&body);
    out
}

fn write_plugin(data: &Path, name: &str, masters: &[&str]) {
    std::fs::write(data.join(name), plugin_bytes(masters, 0)).unwrap();
}

fn request(
    data: &Path,
    order: &[&str],
    active: &[&str],
    masterlist: &Path,
    prelude: Option<&Path>,
) -> String {
    let order: Vec<String> = order.iter().map(|name| (*name).to_string()).collect();
    let active: Vec<String> = active.iter().map(|name| (*name).to_string()).collect();
    json_object(&[
        ("game_type", quote("SkyrimSE")),
        ("data", quote(&data.to_string_lossy())),
        ("load_order", format_array(&order)),
        ("active", format_array(&active)),
        ("masterlist", quote(&masterlist.to_string_lossy())),
        (
            "prelude",
            match prelude {
                Some(path) => quote(&path.to_string_lossy()),
                None => "null".to_string(),
            },
        ),
    ])
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn format_array(values: &[String]) -> String {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&value.replace('\\', "\\\\").replace('"', "\\\""));
        out.push('"');
    }
    out.push(']');
    out
}

fn json_object(entries: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(key);
        out.push_str("\":");
        out.push_str(value);
    }
    out.push('}');
    out
}

fn order_of(reply: &str) -> Vec<String> {
    let value = json::parse(reply).unwrap();
    value
        .get("order")
        .and_then(json::Value::as_array)
        .unwrap()
        .iter()
        .map(|entry| entry.as_str().unwrap().to_string())
        .collect()
}

fn position(order: &[String], name: &str) -> usize {
    order.iter().position(|entry| entry == name).unwrap()
}

#[test]
fn orders_plugins_by_a_masterlist_after_rule() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    for name in ["A.esp", "B.esp", "C.esp"] {
        write_plugin(&data, name, &[]);
    }
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(
        &masterlist,
        "plugins:\n  - name: 'B.esp'\n    after:\n      - 'A.esp'\n",
    )
    .unwrap();

    let reply = run_request(&request(
        &data,
        &["B.esp", "A.esp", "C.esp"],
        &["A.esp", "B.esp", "C.esp"],
        &masterlist,
        None,
    ))
    .unwrap();
    let order = order_of(&reply);
    assert_eq!(order.len(), 3);
    assert!(position(&order, "A.esp") < position(&order, "B.esp"));
}

#[test]
fn a_missing_master_does_not_stop_the_sort() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    write_plugin(&data, "Child.esp", &["Missing.esm"]);
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(&masterlist, "plugins: []\n").unwrap();

    let reply = run_request(&request(
        &data,
        &["Child.esp", "A.esp"],
        &["Child.esp", "A.esp"],
        &masterlist,
        None,
    ))
    .unwrap();
    assert_eq!(order_of(&reply), ["Child.esp", "A.esp"]);
}

#[test]
fn a_checksum_condition_uses_the_plugin_crc() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    write_plugin(&data, "B.esp", &[]);
    let bytes = std::fs::read(data.join("A.esp")).unwrap();
    let crc = crc32fast::hash(&bytes);
    let wrong = crc.wrapping_add(1);

    let good = dir.path().join("good.yaml");
    std::fs::write(
        &good,
        format!(
            "plugins:\n  - name: 'B.esp'\n    after:\n      - name: 'A.esp'\n        condition: 'checksum(\"A.esp\", {crc:08X})'\n"
        ),
    )
    .unwrap();
    let bad = dir.path().join("bad.yaml");
    std::fs::write(
        &bad,
        format!(
            "plugins:\n  - name: 'B.esp'\n    after:\n      - name: 'A.esp'\n        condition: 'checksum(\"A.esp\", {wrong:08X})'\n"
        ),
    )
    .unwrap();

    let matching = run_request(&request(
        &data,
        &["B.esp", "A.esp"],
        &["B.esp", "A.esp"],
        &good,
        None,
    ))
    .unwrap();
    assert_eq!(order_of(&matching), ["A.esp", "B.esp"]);

    let not_matching = run_request(&request(
        &data,
        &["B.esp", "A.esp"],
        &["B.esp", "A.esp"],
        &bad,
        None,
    ))
    .unwrap();
    assert_eq!(order_of(&not_matching), ["B.esp", "A.esp"]);
}

#[test]
fn the_active_set_from_the_request_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    write_plugin(&data, "B.esp", &[]);
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(
        &masterlist,
        "plugins:\n  - name: 'B.esp'\n    after:\n      - name: 'A.esp'\n        condition: 'active(\"A.esp\")'\n",
    )
    .unwrap();

    let active = run_request(&request(
        &data,
        &["B.esp", "A.esp"],
        &["A.esp", "B.esp"],
        &masterlist,
        None,
    ))
    .unwrap();
    assert_eq!(order_of(&active), ["A.esp", "B.esp"]);

    let inactive = run_request(&request(
        &data,
        &["B.esp", "A.esp"],
        &["B.esp"],
        &masterlist,
        None,
    ))
    .unwrap();
    assert_eq!(order_of(&inactive), ["B.esp", "A.esp"]);
}

#[test]
fn a_masterlist_message_keeps_its_wording_and_level() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(
        &masterlist,
        "plugins:\n  - name: 'A.esp'\n    msg:\n      - type: warn\n        content: 'A plain message.'\n",
    )
    .unwrap();

    let reply = run_request(&request(&data, &["A.esp"], &["A.esp"], &masterlist, None)).unwrap();
    let value = json::parse(&reply).unwrap();
    let plugins = value
        .get("plugins")
        .and_then(json::Value::as_array)
        .unwrap();
    assert_eq!(plugins.len(), 1);
    let messages = plugins[0]
        .get("messages")
        .and_then(json::Value::as_array)
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].get("level").and_then(json::Value::as_str),
        Some("warn")
    );
    assert_eq!(
        messages[0].get("text").and_then(json::Value::as_str),
        Some("A plain message.")
    );
}

#[test]
fn a_plugin_missing_from_the_data_folder_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(&masterlist, "plugins: []\n").unwrap();

    let error = run_request(&request(
        &data,
        &["A.esp", "Missing.esp"],
        &["A.esp"],
        &masterlist,
        None,
    ))
    .unwrap_err();
    assert!(error.contains("Missing.esp"), "{error}");
}

#[test]
fn a_masterlist_that_does_not_parse_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);
    let masterlist = dir.path().join("masterlist.yaml");
    std::fs::write(&masterlist, "plugins: [unclosed\n").unwrap();

    let error =
        run_request(&request(&data, &["A.esp"], &["A.esp"], &masterlist, None)).unwrap_err();
    assert!(error.contains("masterlist cannot be read"), "{error}");
}

#[test]
fn inline_yaml_removes_the_need_for_yaml_paths() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write_plugin(&data, "A.esp", &[]);

    let request = json_object(&[
        ("game_type", "\"SkyrimSE\"".to_string()),
        (
            "data",
            format!("\"{}\"", data.to_string_lossy().replace('\\', "\\\\")),
        ),
        ("load_order", "[\"A.esp\"]".to_string()),
        ("active", "[\"A.esp\"]".to_string()),
        ("masterlist", "\"not-read.yaml\"".to_string()),
        ("prelude", "null".to_string()),
        ("masterlist_yaml", "\"plugins: []\\n\"".to_string()),
    ]);
    let reply = run_request(&request).unwrap();
    assert_eq!(order_of(&reply), ["A.esp"]);
}
