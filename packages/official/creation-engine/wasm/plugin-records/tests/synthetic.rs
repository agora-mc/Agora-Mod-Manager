//! Record parsing and overlap detection over plugins this test writes itself.
//!
//! The bytes follow UESP's "Skyrim Mod:Mod File Format" (a 24-byte record
//! header, a 24-byte group header whose size includes the header, and zlib
//! compressed fields prefixed by their decompressed size). Every name here is
//! invented.

use plugin_records::{build_overlaps, parse_plugin, run_request, Index};

const COMPRESSED: u32 = 0x0004_0000;
const LIGHT: u32 = 0x0000_0200;

fn subrecord(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(kind);
    out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn zstring(text: &str) -> Vec<u8> {
    let mut out = text.as_bytes().to_vec();
    out.push(0);
    out
}

fn editor_id(text: &str) -> Vec<u8> {
    subrecord(b"EDID", &zstring(text))
}

fn record(kind: &[u8; 4], flags: u32, form_id: u32, fields: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(kind);
    out.extend_from_slice(&u32::try_from(fields.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&form_id.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(fields);
    out
}

fn group(label: &[u8; 4], group_type: i32, children: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"GRUP");
    out.extend_from_slice(&u32::try_from(24 + children.len()).unwrap().to_le_bytes());
    out.extend_from_slice(label);
    out.extend_from_slice(&group_type.to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
    out.extend_from_slice(children);
    out
}

/// A TES4 header record, then the given body.
fn plugin(masters: &[&str], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut fields = subrecord(b"HEDR", &[0u8; 12]);
    for master in masters {
        fields.extend(subrecord(b"MAST", &zstring(master)));
        fields.extend(subrecord(b"DATA", &[0u8; 8]));
    }
    let mut out = record(b"TES4", flags, 0, &fields);
    out.extend_from_slice(body);
    out
}

/// One GMST whose value is `value`, with an invented editor id.
fn gmst(form_id: u32, value: i32, edid: &str) -> Vec<u8> {
    let mut fields = editor_id(edid);
    fields.extend(subrecord(b"DATA", &value.to_le_bytes()));
    record(b"GMST", 0, form_id, &fields)
}

fn index(name: &str, masters: &[&str], flags: u32, records: &[Vec<u8>]) -> Index {
    let mut body = group(b"GMST", 0, &records.concat());
    if body.is_empty() {
        body = Vec::new();
    }
    let bytes = plugin(masters, flags, &body);
    parse_plugin(name, &bytes).unwrap_or_else(|error| panic!("{name}: {error}"))
}

type FlatKey = (String, String, Vec<(String, u64)>);

fn overlaps(indexes: &[Index]) -> Vec<FlatKey> {
    let (keys, _) = build_overlaps(indexes);
    keys.into_iter()
        .map(|key| {
            (
                key.key,
                key.label,
                key.items
                    .into_iter()
                    .map(|item| (item.name, item.digest))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn an_empty_request_is_an_error() {
    assert!(run_request("").is_err());
    assert!(run_request("{}").is_err());
    assert!(run_request("{\"plugins\":\"Skyrim.esm\"}").is_err());
}

#[test]
fn a_request_with_no_plugins_has_nothing_to_report() {
    let reply = run_request("{\"plugins\":[]}").unwrap();
    assert!(reply.contains("\"keys\":[]"), "{reply}");
    assert!(reply.contains("\"notes\":[]"), "{reply}");
    assert!(reply.contains("\"indexes\":[]"), "{reply}");
}

#[test]
fn an_empty_plugin_file_is_an_error_not_an_empty_index() {
    let error = parse_plugin("Empty.esp", &[]).unwrap_err();
    assert!(error.contains("shorter"), "{error}");
}

#[test]
fn a_file_without_a_tes4_header_is_an_error() {
    let bytes = plugin(&[], 0, &[]);
    let mut broken = bytes.clone();
    broken[0..4].copy_from_slice(b"NOPE");
    let error = parse_plugin("Broken.esp", &broken).unwrap_err();
    assert!(error.contains("TES4"), "{error}");
}

#[test]
fn a_truncated_tes4_record_is_an_error() {
    let bytes = plugin(&[], 0, &[]);
    let error = parse_plugin("Cut.esp", &bytes[..20]).unwrap_err();
    assert!(
        error.contains("TES4") || error.contains("truncated") || error.contains("shorter"),
        "{error}"
    );
}

#[test]
fn a_record_that_overruns_its_group_is_an_error() {
    let mut one = gmst(0x000abc, 1, "Value");
    one[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    let body = group(b"GMST", 0, &one);
    let bytes = plugin(&[], 0, &body);
    let error = parse_plugin("Overrun.esp", &bytes).unwrap_err();
    assert!(error.contains("overruns"), "{error}");
}

#[test]
fn a_group_that_overruns_its_parent_is_an_error() {
    let inner = group(b"GMST", 0, &gmst(0x000abc, 1, "Value"));
    let mut outer = group(b"CELL", 0, &inner);
    let size = u32::from_le_bytes(outer[4..8].try_into().unwrap());
    outer[4..8].copy_from_slice(&(size + 4096).to_le_bytes());
    let bytes = plugin(&[], 0, &outer);
    let error = parse_plugin("Nested.esp", &bytes).unwrap_err();
    assert!(error.contains("overruns"), "{error}");
}

#[test]
fn two_plugins_that_change_one_record_differently_report_that_key() {
    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let first = index(
        "First.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 2, "JumpSetting")],
    );
    let second = index(
        "Second.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 3, "JumpSetting")],
    );

    let keys = overlaps(&[master, first, second]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    let (key, label, items) = &keys[0];
    assert_eq!(key, "master.esm:000abc");
    assert_eq!(label, "GMST JumpSetting");
    assert_eq!(
        items
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Master.esm", "First.esp", "Second.esp"]
    );
    assert_ne!(items[0].1, items[1].1);
    assert_ne!(items[1].1, items[2].1);
}

#[test]
fn the_same_change_has_the_same_digest() {
    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let first = index(
        "First.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 2, "JumpSetting")],
    );
    let second = index(
        "Second.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 2, "JumpSetting")],
    );
    let keys = overlaps(&[master, first, second]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].2[1].1, keys[0].2[2].1);
}

#[test]
fn a_record_identical_to_its_master_is_still_reported_with_the_same_digest() {
    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let child = index(
        "Child.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 1, "JumpSetting")],
    );
    let keys = overlaps(&[master, child]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].2.len(), 2);
    assert_eq!(keys[0].2[0].1, keys[0].2[1].1);
}

#[test]
fn a_light_plugin_resolves_its_own_records_and_its_master_references() {
    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let light = index(
        "Light.esl",
        &["Master.esm"],
        LIGHT,
        &[
            gmst(0x0100_0abc, 5, "LightSetting"),
            gmst(0x000abc, 1, "JumpSetting"),
        ],
    );
    let keys = overlaps(&[master, light]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].0, "master.esm:000abc");
    assert_eq!(keys[0].1, "GMST JumpSetting");
    assert_eq!(
        keys[0]
            .2
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Master.esm", "Light.esl"]
    );
}

#[test]
fn a_light_plugins_own_record_key_uses_its_name_and_low_24_bits() {
    let light = index(
        "Light.esl",
        &["Master.esm"],
        LIGHT,
        &[gmst(0x0100_0def, 5, "LightSetting")],
    );
    let other = index(
        "Other.esp",
        &["Light.esl"],
        0,
        &[gmst(0x000def, 5, "LightSetting")],
    );
    let keys = overlaps(&[light, other]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].0, "light.esl:000def");
    assert_eq!(
        keys[0]
            .2
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["Light.esl", "Other.esp"]
    );

    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let light = index(
        "Light.esl",
        &["Master.esm"],
        LIGHT,
        &[gmst(0x000abc, 7, "JumpSetting")],
    );
    let keys = overlaps(&[master, light]);
    assert_eq!(keys[0].0, "master.esm:000abc");
}

#[test]
fn a_compressed_record_has_the_same_digest_as_its_plain_form() {
    let mut fields = editor_id("JumpSetting");
    fields.extend(subrecord(b"DATA", &2i32.to_le_bytes()));
    let mut compressed_fields = u32::try_from(fields.len()).unwrap().to_le_bytes().to_vec();
    compressed_fields.extend(miniz_oxide::deflate::compress_to_vec_zlib(&fields, 6));
    let compressed = record(b"GMST", COMPRESSED, 0x000abc, &compressed_fields);

    let plain = index(
        "Plain.esp",
        &["Master.esm"],
        0,
        &[gmst(0x000abc, 2, "JumpSetting")],
    );
    let packed = index("Packed.esp", &["Master.esm"], 0, &[compressed]);
    let master = index("Master.esm", &[], 0, &[gmst(0x000abc, 1, "JumpSetting")]);
    let keys = overlaps(&[master, plain, packed]);
    assert_eq!(keys.len(), 1, "{keys:?}");
    let plain_digest = keys[0]
        .2
        .iter()
        .find(|(name, _)| name == "Plain.esp")
        .unwrap()
        .1;
    let packed_digest = keys[0]
        .2
        .iter()
        .find(|(name, _)| name == "Packed.esp")
        .unwrap()
        .1;
    assert_eq!(plain_digest, packed_digest);
}

#[test]
fn a_nested_cell_reference_is_indexed() {
    let reference = record(b"REFR", 0, 0x0100_0def, &subrecord(b"NAME", &[1, 2, 3, 4]));
    let inner = group(b"REFR", 0, &reference);
    let cell_children = group(&[0, 0, 0, 0], 8, &inner);
    let cells = group(b"CELL", 0, &cell_children);
    let bytes = plugin(&["Master.esm"], 0, &cells);
    let parsed = parse_plugin("Nested.esp", &bytes).unwrap();
    assert_eq!(parsed.records.len(), 1);
    assert_eq!(&parsed.records[0].kind, b"REFR");
    assert_eq!(parsed.records[0].file, 1);
    assert_eq!(parsed.records[0].low, 0x000def);
}

#[test]
fn an_index_round_trips_through_its_stored_form() {
    let original = index(
        "Round.esp",
        &["Master.esm", "Other.esm"],
        LIGHT,
        &[gmst(0x0200_0123, 9, "RoundTrip")],
    );
    let bytes = original.serialize();
    let restored = Index::deserialize(&bytes).unwrap();
    assert_eq!(restored.name, original.name);
    assert_eq!(restored.size, original.size);
    assert_eq!(restored.modified, original.modified);
    assert_eq!(restored.files, original.files);
    assert_eq!(restored.strings, original.strings);
    assert_eq!(restored.records.len(), original.records.len());
    for (left, right) in restored.records.iter().zip(&original.records) {
        assert_eq!(left.file, right.file);
        assert_eq!(left.low, right.low);
        assert_eq!(left.digest, right.digest);
        assert_eq!(left.kind, right.kind);
        assert_eq!(left.edid, right.edid);
    }
}

#[test]
fn a_changed_file_that_cannot_be_read_is_a_note_naming_it() {
    let request = "{\"plugins\":[\"Missing.esp\"]}";
    let reply = run_request(request).unwrap();
    assert!(reply.contains("\"keys\":[]"), "{reply}");
    assert!(reply.contains("\"item\":\"Missing.esp\""), "{reply}");
    assert!(reply.contains("could not be read"), "{reply}");
}

#[test]
fn a_name_that_is_not_a_plain_file_is_refused_with_a_note() {
    let reply = run_request("{\"plugins\":[\"../Escape.esp\"]}").unwrap();
    assert!(reply.contains("not a plain file name"), "{reply}");
}

#[test]
fn a_round_tripped_index_still_finds_an_overlap() {
    let master = parse_plugin(
        "Master.esm",
        &plugin(
            &[],
            0,
            &group(b"GMST", 0, &gmst(0x000abc, 1, "JumpSetting")),
        ),
    )
    .unwrap();
    let child = parse_plugin(
        "Child.esp",
        &plugin(
            &["Master.esm"],
            0,
            &group(b"GMST", 0, &gmst(0x000abc, 2, "JumpSetting")),
        ),
    )
    .unwrap();
    let restored: Vec<Index> = [master, child]
        .iter()
        .map(|index| Index::deserialize(&index.serialize()).unwrap())
        .collect();
    let keys = overlaps(&restored);
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert_eq!(keys[0].0, "master.esm:000abc");
}

#[test]
fn a_restricted_parse_keeps_only_the_named_keys() {
    let bytes = plugin(
        &[],
        0,
        &group(
            b"GMST",
            0,
            &[
                gmst(0x000abc, 1, "JumpSetting"),
                gmst(0x000def, 2, "OtherSetting"),
            ]
            .concat(),
        ),
    );
    let full = parse_plugin("Master.esm", &bytes).unwrap();
    assert_eq!(full.records.len(), 2);
    assert_eq!(full.only_hash, 0);

    let restricted = plugin_records::parse_plugin_restricted(
        "Master.esm",
        &bytes,
        &["master.esm:000abc".into()],
    )
    .unwrap();
    assert_eq!(restricted.records.len(), 1);
    assert_eq!(restricted.records[0].low, 0x000abc);
    assert_ne!(restricted.only_hash, 0);
}
