//! The archive reader's own tests (MASTER_SPEC §32.3).
//!
//! Every archive here is built in the test, in the format the test names, so the reader meets the
//! real decoders rather than an invented shape.

use super::*;
use std::io::Write;
use std::path::PathBuf;

const MIB: u64 = 1024 * 1024;

/// A zip with `entries`, stored (not deflated), so a test can patch its own headers.
fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// A zip whose entries are deflated, for the ratio tests: repeated bytes compress well.
fn deflated_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// A zip with one entry whose unix mode says symbolic link.
fn zip_with_symlink_entry(name: &str) -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o120777);
        writer.add_symlink(name, "..", options).unwrap();
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

/// A tar written by hand, so a test can name an entry the `tar` crate's own writer refuses: `..`,
/// an absolute path. This is the shape a malicious archive has, and the decoder must meet it.
fn raw_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, bytes) in entries {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let take = name_bytes.len().min(100);
        header[..take].copy_from_slice(&name_bytes[..take]);
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", bytes.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(bytes);
        let padding = (512 - bytes.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, padding));
    }
    // Two zero blocks end a tar.
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn tar_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, name, *bytes).unwrap();
    }
    builder.into_inner().unwrap()
}

fn tar_with_symlink(name: &str, target: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_cksum();
    builder.append_link(&mut header, name, target).unwrap();
    builder.into_inner().unwrap()
}

fn tar_gz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let tar = tar_bytes(entries);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

/// A **solid** 7z: every entry shares one compressed block, which is the shape a reader that walks
/// past the entries in front of the one it wants without draining them decodes wrongly. The library
/// says as much itself: "when decoding a solid archive, the data to be decompressed depends on the
/// data in front of it, you cannot simply skip the previous data and only decompress the data in the
/// back".
fn solid_sevenz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("solid.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&path).unwrap();
    let listed: Vec<sevenz_rust2::ArchiveEntry> = entries
        .iter()
        .map(|(name, _)| sevenz_rust2::ArchiveEntry::new_file(name))
        .collect();
    let readers: Vec<sevenz_rust2::SourceReader<std::io::Cursor<&[u8]>>> = entries
        .iter()
        .map(|(_, bytes)| sevenz_rust2::SourceReader::new(std::io::Cursor::new(*bytes)))
        .collect();
    writer.push_archive_entries(listed, readers).unwrap();
    writer.finish().unwrap();
    std::fs::read(&path).unwrap()
}

fn sevenz_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&path).unwrap();
    for (name, bytes) in entries {
        let entry = sevenz_rust2::ArchiveEntry::new_file(name);
        writer
            .push_archive_entry(entry, Some(std::io::Cursor::new(*bytes)))
            .unwrap();
    }
    writer.finish().unwrap();
    std::fs::read(&path).unwrap()
}

/// An archive whose declared size is larger than the bytes it holds, which is what a truncated or
/// lying entry looks like: the central-directory and local-header sizes are patched by name. The
/// entry is deflated, so the stream ends where the real data ends and the lie is visible without a
/// second copy of the file to truncate.
fn zip_that_overdeclares(name: &str, bytes: &[u8], declared: u32) -> Vec<u8> {
    let mut raw = deflated_zip_bytes(&[(name, bytes)]);
    // The local file header: "PK\x03\x04", then the uncompressed size at offset 22.
    let mut patched = 0;
    for start in [b"PK\x03\x04".as_slice(), b"PK\x01\x02".as_slice()] {
        let offset = if start[2] == 0x04 { 22 } else { 24 };
        let mut cursor = 0;
        while cursor + 30 < raw.len() {
            if &raw[cursor..cursor + 4] == start {
                let at = cursor + offset;
                raw[at..at + 4].copy_from_slice(&declared.to_le_bytes());
                patched += 1;
                break;
            }
            cursor += 1;
        }
    }
    assert_eq!(patched, 2, "both headers were patched");
    raw
}

fn small_limits() -> ArchiveLimits {
    ArchiveLimits {
        preset: "test",
        max_entries: 4,
        max_entry_bytes: 32,
        max_total_bytes: 48,
        max_ratio: 0,
        bounds_the_listing: true,
        counts_folders: true,
    }
}

fn extract_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out");
    (dir, dest)
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

#[test]
fn the_presets_carry_the_limits_the_call_sites_used() {
    assert_eq!(ArchiveLimits::PACK.max_entries, 5_000);
    assert_eq!(ArchiveLimits::PACK.max_entry_bytes, 500 * MIB);
    assert_eq!(ArchiveLimits::PACK.max_total_bytes, 2 * 1024 * MIB);
    assert_eq!(ArchiveLimits::PACK.max_ratio, 200);

    assert_eq!(ArchiveLimits::TECHNIC_PACK.max_entries, 5_000);
    assert_eq!(ArchiveLimits::TECHNIC_PACK.max_entry_bytes, 500 * MIB);
    assert_eq!(ArchiveLimits::TECHNIC_PACK.max_total_bytes, 2 * 1024 * MIB);

    // A content-store mod: the numbers `content_store` enforced before this reader existed, which
    // were none. A Skyrim texture or BSA mod is past Technic's numbers, and refusing it would
    // refuse the mod the player downloaded.
    assert_eq!(ArchiveLimits::GAME_MOD.max_entries, 0, "no entry cap");
    assert_eq!(ArchiveLimits::GAME_MOD.max_entry_bytes, u64::MAX);
    assert_eq!(ArchiveLimits::GAME_MOD.max_total_bytes, 0, "no total cap");
    assert_eq!(ArchiveLimits::GAME_MOD.max_ratio, 0, "no ratio check");
    let bounds_the_listing = ArchiveLimits::GAME_MOD.bounds_the_listing;
    assert!(!bounds_the_listing, "it reads the entries it is asked for");

    assert_eq!(ArchiveLimits::BACKUP.max_entries, 10_000);
    assert_eq!(ArchiveLimits::BACKUP.max_entry_bytes, 1024 * MIB);
    assert_eq!(ArchiveLimits::BACKUP.max_total_bytes, 64 * 1024 * MIB);
    assert_eq!(
        ArchiveLimits::BACKUP.max_ratio,
        0,
        "a backup of compressed data would trip a ratio check Agora did not ask for"
    );

    assert_eq!(ArchiveLimits::PLUGIN_PACKAGE.max_entries, 2_000);
    assert_eq!(ArchiveLimits::PLUGIN_PACKAGE.max_entry_bytes, 16 * MIB);
    assert_eq!(ArchiveLimits::PLUGIN_PACKAGE.max_total_bytes, 64 * MIB);
    assert_eq!(ArchiveLimits::PLUGIN_PACKAGE.max_ratio, 200);

    assert_eq!(ArchiveLimits::JAR_METADATA.max_entry_bytes, 32 * MIB);
    assert_eq!(
        ArchiveLimits::JAR_METADATA.max_total_bytes,
        0,
        "the 128 MiB was a budget for the nested bytes a read may use, never a cap on the jar"
    );

    assert_eq!(ArchiveLimits::RUNTIME.max_entries, 10_000);
    assert_eq!(ArchiveLimits::RUNTIME.max_entry_bytes, 500 * MIB);
    assert_eq!(ArchiveLimits::RUNTIME.max_total_bytes, 2 * 1024 * MIB);

    assert_eq!(ArchiveLimits::WORLD_ARCHIVE.max_entries, 200_000);
    assert_eq!(
        ArchiveLimits::WORLD_ARCHIVE.max_total_bytes,
        16 * 1024 * MIB
    );
    assert_eq!(
        ArchiveLimits::WORLD_ARCHIVE.max_entry_bytes,
        2 * 1024 * MIB,
        "the old code had no per-entry limit; 2 GiB is the largest a region file plausibly gets"
    );
    assert_eq!(
        ArchiveLimits::WORLD_ARCHIVE.max_total_bytes,
        16 * 1024 * MIB
    );

    let listing_checks: Vec<bool> = [
        ArchiveLimits::PACK,
        ArchiveLimits::TECHNIC_PACK,
        ArchiveLimits::BACKUP,
        ArchiveLimits::PLUGIN_PACKAGE,
        ArchiveLimits::RUNTIME,
        ArchiveLimits::WORLD_ARCHIVE,
        ArchiveLimits::JAR_METADATA,
        ArchiveLimits::GAME_MOD,
    ]
    .into_iter()
    .map(|limits| limits.bounds_the_listing)
    .collect();
    assert_eq!(
        listing_checks,
        vec![true, true, true, true, true, true, false, false],
        "the two jobs that read what they need (a jar's metadata, a store's mod) do not bound the \
         listing"
    );
}

/// A zip with one `Stored` entry whose local header and central record both claim `extra` bytes
/// more than the entry holds: the shape a lying size header has.
fn zip_that_over_declares_by(extra: u32) -> Vec<u8> {
    let mut bytes = zip_bytes(&[("mods/mod.jar", b"0123456789")]);
    let data = b"0123456789".len() as u32;
    let declared = data + extra;
    // The local header, then every central-directory record, carry the sizes twice.
    bytes[18..22].copy_from_slice(&declared.to_le_bytes());
    bytes[22..26].copy_from_slice(&declared.to_le_bytes());
    let mut at = 0usize;
    while at + 46 <= bytes.len() {
        if &bytes[at..at + 4] == b"PK\x01\x02" {
            bytes[at + 20..at + 24].copy_from_slice(&declared.to_le_bytes());
            bytes[at + 24..at + 28].copy_from_slice(&declared.to_le_bytes());
            at += 46;
        } else {
            at += 1;
        }
    }
    bytes
}

#[test]
fn two_entries_with_one_name_refuse_the_archive() {
    // The zip crate keeps only the last of several same-named records, so a listing that goes
    // through it cannot see the collision: an archive whose bytes hide one entry behind another is
    // exactly the shape a classpath check refuses. The reader counts the central directory's own
    // records and names them.
    let duplicate = zip_with_one_duplicated_name();
    let error = SafeArchive::open_bytes(duplicate, ArchiveLimits::PACK)
        .err()
        .expect("two records, one name");
    match error {
        ArchiveError::DuplicateNames { names, .. } => {
            assert_eq!(names, vec!["a.txt".to_string()]);
        }
        other => panic!("unexpected error {other:?}"),
    }

    // A tar stores every header, so the listing itself shows both.
    let error = SafeArchive::open_bytes(
        raw_tar(&[("config/a.toml", b"one"), ("config/a.toml", b"two")]),
        ArchiveLimits::PACK,
    )
    .err()
    .expect("two headers, one name");
    assert!(
        matches!(error, ArchiveError::DuplicateNames { .. }),
        "{error:?}"
    );
}

/// A zip holding two records with **one** name: `zip::ZipWriter` refuses to write that, so the
/// second name is renamed in place afterwards. This is the shape a classpath check has to catch,
/// because the zip crate keeps only the last of the two.
fn zip_with_one_duplicated_name() -> Vec<u8> {
    let mut bytes = zip_bytes(&[("a.txt", b"first"), ("b.txt", b"second")]);
    let mut at = 0usize;
    while at + 5 <= bytes.len() {
        if &bytes[at..at + 5] == b"b.txt" {
            bytes[at..at + 5].copy_from_slice(b"a.txt");
            at += 5;
        } else {
            at += 1;
        }
    }
    bytes
}

#[test]
fn a_stored_entry_that_over_declares_its_size_is_refused() {
    // A `Stored` entry has no stream to stop at, so the only thing that can catch a header claiming
    // more bytes than the entry holds is reading to the end and checking the CRC over what arrived.
    // The extra bytes here are the central directory that follows, which a trusting reader would
    // hand over as if they were the entry's own (MASTER_SPEC §32.13).
    let bytes = zip_that_over_declares_by(8);
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .expect("the listing is well formed: only the entry lies");
    let error = archive
        .read("mods/mod.jar")
        .expect_err("the CRC is over the bytes the entry really holds");
    assert!(
        matches!(
            error,
            ArchiveError::Corrupt { .. } | ArchiveError::EntrySizeLies { .. }
        ),
        "{error:?}"
    );
}

#[test]
fn the_per_entry_limit_is_a_read_limit_where_the_preset_says_so() {
    // `JAR_METADATA` reads a handful of entries, so a jar holding one literal that no metadata reader
    // asks for lists fine and is refused only when someone asks for that entry.
    let big = vec![0u8; (32 * 1024 * 1024) + 1024];
    let bytes = zip_bytes(&[("assets/big.bin", &big), ("fabric.mod.json", b"{}")]);
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::JAR_METADATA)
        .expect("an entry nobody reads does not fail the listing");
    assert_eq!(archive.file_entries().count(), 2);
    assert_eq!(archive.read("fabric.mod.json").unwrap(), b"{}");
    let error = archive
        .read("assets/big.bin")
        .expect_err("reading the big entry is what the limit stops");
    assert!(matches!(error, ArchiveError::TooLarge { .. }), "{error:?}");
}

#[test]
fn more_entries_than_the_preset_allows_is_refused() {
    let names: Vec<String> = (0..5).map(|i| format!("file{i}.txt")).collect();
    let entries: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), &b"x"[..]))
        .collect();
    let error = SafeArchive::open_bytes(zip_bytes(&entries), small_limits())
        .err()
        .expect("five files, four allowed");
    assert!(matches!(
        error,
        ArchiveError::TooManyEntries {
            count: 5,
            limit: 4,
            ..
        }
    ));
}

#[test]
fn more_bytes_than_the_preset_allows_is_refused() {
    let entry = [b'a'; 20];
    let error = SafeArchive::open_bytes(
        zip_bytes(&[
            ("a.txt", &entry[..]),
            ("b.txt", &entry[..]),
            ("c.txt", &entry[..]),
        ]),
        small_limits(),
    )
    .err()
    .expect("60 bytes, 48 allowed");
    assert!(matches!(error, ArchiveError::TooLarge { limit: 48, .. }));
}

#[test]
fn one_entry_over_the_per_entry_limit_is_refused() {
    let entry = [b'a'; 40];
    let error = SafeArchive::open_bytes(zip_bytes(&[("a.txt", &entry[..])]), small_limits())
        .err()
        .expect("40 bytes in one file, 32 allowed");
    assert!(matches!(
        error,
        ArchiveError::TooLarge {
            limit: 32,
            ref entry,
            ..
        } if entry == "a.txt"
    ));
}

#[test]
fn a_zip_bomb_is_refused_by_the_ratio() {
    // A megabyte of zeros deflates far better than the 200 times the pack preset allows: this is
    // the shape of a decompression bomb, and its declared size looks perfectly ordinary.
    let zeros = vec![0u8; 1024 * 1024];
    let error = SafeArchive::open_bytes(
        deflated_zip_bytes(&[("bomb.bin", &zeros)]),
        ArchiveLimits::PACK,
    )
    .err()
    .expect("a megabyte of zeros is a ratio bomb");
    assert!(matches!(
        error,
        ArchiveError::RatioExceeded {
            limit: 200,
            ref entry,
            ..
        } if entry == "bomb.bin"
    ));
}

#[test]
fn an_entry_that_declares_more_than_it_holds_is_refused() {
    let raw = zip_that_overdeclares("a.txt", b"short", 4096);
    // `BACKUP` is the preset with no ratio rule, so the declared size is the only thing this test is
    // holding the reader to.
    let mut archive = SafeArchive::open_bytes(raw, ArchiveLimits::BACKUP).expect("lists");
    let mut sink = Vec::new();
    let error = match archive.read_into("a.txt", &mut sink) {
        Ok(_) => panic!("the entry declared 4096 bytes and holds 5"),
        Err(error) => error,
    };
    // Which layer notices is the decoder's business: the zip crate reports the deflate stream ending
    // early, and the reader's own check would catch a decoder that returned quietly. Either way the
    // answer is a refusal that names the entry, never a short file that looks whole.
    let message = error.to_string();
    assert!(message.contains("a.txt"), "{message}");
    assert!(
        matches!(
            error,
            ArchiveError::EntrySizeLies { .. } | ArchiveError::Corrupt { .. }
        ),
        "{error:?}"
    );
    assert!(
        sink.len() < 4096,
        "nothing larger than the entry was handed over"
    );
}

// ---------------------------------------------------------------------------
// Names that escape, and names a disk cannot hold
// ---------------------------------------------------------------------------

fn assert_escape_refused(bytes: Vec<u8>, what: &str) {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .unwrap_or_else(|error| panic!("{what}: should list: {error}"));
    let error = archive
        .extract(&dest, NamePolicy::WithoutThem)
        .expect_err("an escaping name is refused even when the caller would skip entries");
    assert!(
        matches!(
            error,
            ArchiveError::Escapes { .. } | ArchiveError::LinkEntry { .. }
        ),
        "{what}: {error:?}"
    );
    assert!(!dest.join("evil.txt").exists(), "{what}: nothing written");
    drop(dir);
}

#[test]
fn zip_slip_forms_are_refused() {
    for name in [
        "../evil.txt",
        "config/../../evil.txt",
        "/etc/evil.txt",
        "C:/Windows/evil.txt",
        "C:\\Windows\\evil.txt",
        "\\\\server\\share\\evil.txt",
    ] {
        assert_escape_refused(zip_bytes(&[(name, b"evil")]), name);
    }
}

#[test]
fn tar_slip_forms_are_refused() {
    for name in [
        "../evil.txt",
        "/etc/evil.txt",
        "config/../../evil.txt",
        "C:/evil.txt",
    ] {
        assert_escape_refused(raw_tar(&[(name, b"evil")]), name);
    }
}

#[test]
fn a_tar_symlink_entry_is_refused() {
    assert_escape_refused(tar_with_symlink("link.txt", "/etc/passwd"), "tar symlink");
}

#[test]
fn a_zip_symlink_entry_is_refused() {
    assert_escape_refused(zip_with_symlink_entry("link.txt"), "zip symlink");
}

#[test]
fn a_reserved_name_is_refused_and_named() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[("textures/aux.dds", b"data")]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let error = archive
        .extract(&dest, NamePolicy::Refuse)
        .expect_err("aux is a name Windows reserves");
    match error {
        ArchiveError::UnsafeNames { entries, .. } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "textures/aux.dds");
            assert!(
                entries[0].reason.contains("device name"),
                "the reason names the rule: {}",
                entries[0].reason
            );
        }
        other => panic!("unexpected error {other:?}"),
    }
    assert!(!dest.join("textures").exists(), "nothing was written");
    drop(dir);
}

#[test]
fn a_dot_component_is_normalized_not_refused() {
    // Zip tools write `./main.js`. A `.` component names the folder it is in, so dropping it keeps
    // the file inside the root; refusing it would refuse a package that unpacks perfectly well.
    let (dir, dest) = extract_dir();
    let mut archive =
        SafeArchive::open_bytes(zip_bytes(&[("./main.js", b"x")]), ArchiveLimits::PACK).unwrap();
    let report = archive
        .extract(&dest, NamePolicy::Refuse)
        .expect("a `./` prefix is not a name a disk cannot hold");
    assert_eq!(report.written, vec!["main.js".to_string()]);
    assert!(dest.join("main.js").is_file());
    assert!(report.skipped.is_empty());
    drop(dir);
}

#[test]
fn a_preview_and_the_extraction_answer_the_same_way_about_names() {
    // `left_out_names` is the answer a preview needs: it decides without creating anything, and it is
    // the same decision the extraction makes, so an install cannot fail after a preview passed.
    let bytes = zip_bytes(&[
        ("plugin.json", b"{}"),
        ("textures/aux.dds", b"data"),
        ("assets/Readme.txt", b"x"),
        ("assets/README.TXT", b"y"),
    ]);
    let archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PLUGIN_PACKAGE).unwrap();
    let error = archive
        .left_out_names(NamePolicy::Refuse)
        .expect_err("two names a disk cannot hold refuse the package");
    match &error {
        ArchiveError::UnsafeNames { entries, .. } => {
            let named: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
            assert!(named.contains(&"textures/aux.dds"), "{named:?}");
            assert!(named.contains(&"assets/README.TXT"), "{named:?}");
        }
        other => panic!("unexpected error {other:?}"),
    }

    let skipped = archive
        .left_out_names(NamePolicy::WithoutThem)
        .expect("the caller may opt in to doing without them");
    let names: Vec<&str> = skipped.iter().map(|entry| entry.name.as_str()).collect();
    assert!(names.contains(&"textures/aux.dds"), "{names:?}");
    assert!(names.contains(&"assets/README.TXT"), "{names:?}");
    assert!(!names.contains(&"plugin.json"), "{names:?}");

    // Nothing was created: a preview leaves the disk alone.
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_name_that_ends_in_a_dot_is_refused() {
    let (dir, dest) = extract_dir();
    let mut archive =
        SafeArchive::open_bytes(zip_bytes(&[("config/run.bat.", b"x")]), ArchiveLimits::PACK)
            .unwrap();
    let error = archive.extract(&dest, NamePolicy::Refuse).unwrap_err();
    assert!(
        matches!(error, ArchiveError::UnsafeNames { .. }),
        "{error:?}"
    );
    drop(dir);
}

#[test]
fn a_case_collision_is_refused_and_both_names_are_named() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[("config/Mod.toml", b"one"), ("config/mod.toml", b"two")]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let error = archive.extract(&dest, NamePolicy::Refuse).unwrap_err();
    match error {
        ArchiveError::UnsafeNames { entries, .. } => {
            assert_eq!(entries.len(), 1, "the second spelling is the offender");
            assert_eq!(entries[0].name, "config/mod.toml");
            assert!(entries[0].reason.contains("case-insensitively"));
        }
        other => panic!("unexpected error {other:?}"),
    }
    drop(dir);
}

#[test]
fn a_case_collision_is_extracted_without_when_the_caller_opts_in() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[
            ("config/Mod.toml", b"one"),
            ("config/mod.toml", b"two"),
            ("config/other.toml", b"three"),
            ("textures/aux.dds", b"four"),
        ]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let report = archive.extract(&dest, NamePolicy::WithoutThem).unwrap();
    assert_eq!(report.written.len(), 2);
    assert!(report.written.contains(&"config/Mod.toml".to_string()));
    assert!(report.written.contains(&"config/other.toml".to_string()));
    let skipped: Vec<&str> = report
        .skipped
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(skipped.contains(&"config/mod.toml"));
    assert!(skipped.contains(&"textures/aux.dds"));
    // One of the two spellings was written, and it is the first in the archive. Which name the
    // filesystem then reports for it depends on the platform, which is exactly why the collision is
    // refused rather than written twice.
    assert_eq!(std::fs::read(dest.join("config/Mod.toml")).unwrap(), b"one");
    assert_eq!(
        std::fs::read(dest.join("config/other.toml")).unwrap(),
        b"three"
    );
    assert!(!dest.join("textures").exists());
    drop(dir);
}

#[test]
fn a_jar_entry_named_after_a_windows_device_is_still_readable() {
    // An entry name is not a file name until something writes it: reading `aux.class` out of a jar
    // is what a mod loader does, and the reader must not refuse it.
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[("aux.class", b"\xca\xfe\xba\xbe")]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let bytes = archive.read("aux.class").unwrap();
    assert_eq!(bytes, b"\xca\xfe\xba\xbe");
}

// ---------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------

#[test]
fn a_tar_gz_is_listed_and_extracted() {
    let (dir, dest) = extract_dir();
    let bytes = tar_gz_bytes(&[("bin/java", b"binary"), ("lib/modules", b"module")]);
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::RUNTIME).unwrap();
    assert_eq!(archive.format(), ArchiveFormat::TarGz);
    assert_eq!(archive.total_declared_bytes(), 12);
    let report = archive.extract(&dest, NamePolicy::Refuse).unwrap();
    assert_eq!(report.written.len(), 2);
    assert_eq!(std::fs::read(dest.join("bin/java")).unwrap(), b"binary");
    drop(dir);
}

#[test]
fn a_sevenz_is_listed_read_and_extracted() {
    let (dir, dest) = extract_dir();
    let bytes = sevenz_bytes(&[
        ("mods/info.json", b"{\"id\":\"x\"}"),
        ("mods/a.class", b"code"),
    ]);
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::TECHNIC_PACK).unwrap();
    assert_eq!(archive.format(), ArchiveFormat::SevenZ);
    assert_eq!(archive.file_entries().count(), 2);
    assert_eq!(archive.read("mods/info.json").unwrap(), b"{\"id\":\"x\"}");
    let report = archive.extract(&dest, NamePolicy::Refuse).unwrap();
    assert_eq!(report.written.len(), 2);
    assert_eq!(std::fs::read(dest.join("mods/a.class")).unwrap(), b"code");
    drop(dir);
}

#[test]
fn a_solid_sevenz_gives_every_member_its_own_bytes() {
    // Two members of one block, of the same length so a mix-up cannot show up as a length: the
    // second member's bytes are only right if the first member's were decoded on the way past.
    let entries: [(&str, &[u8]); 2] = [("a.txt", b"AAAA"), ("b.txt", b"BBBB")];
    let mut archive = SafeArchive::open_bytes(solid_sevenz_bytes(&entries), ArchiveLimits::PACK)
        .expect("a solid 7z lists like any other");
    assert_eq!(archive.read("a.txt").unwrap(), b"AAAA");
    assert_eq!(archive.read("b.txt").unwrap(), b"BBBB");

    let (dir, dest) = extract_dir();
    let mut archive =
        SafeArchive::open_bytes(solid_sevenz_bytes(&entries), ArchiveLimits::PACK).unwrap();
    archive.extract(&dest, NamePolicy::Refuse).unwrap();
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"AAAA");
    assert_eq!(std::fs::read(dest.join("b.txt")).unwrap(), b"BBBB");
    drop(dir);
}

#[test]
fn a_plain_tar_is_listed_by_its_magic_not_its_extension() {
    let bytes = tar_bytes(&[("a.txt", b"hello")]);
    let archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    assert_eq!(archive.format(), ArchiveFormat::Tar);
}

#[test]
fn a_file_that_is_not_an_archive_is_refused() {
    let error = SafeArchive::open_bytes(b"not an archive at all".to_vec(), ArchiveLimits::PACK)
        .err()
        .expect("refused");
    assert!(
        matches!(error, ArchiveError::UnknownFormat { .. }),
        "{error:?}"
    );
}

#[test]
fn a_missing_file_is_refused_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone.zip");
    let error = match SafeArchive::open(&missing, ArchiveLimits::PACK) {
        Ok(_) => panic!("a missing file is never an empty archive"),
        Err(error) => error,
    };
    match error {
        ArchiveError::Unreadable { error, .. } => assert!(error.contains("missing")),
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn a_truncated_zip_is_refused() {
    let mut raw = zip_bytes(&[("a.txt", b"hello")]);
    raw.truncate(raw.len() / 2);
    let error = SafeArchive::open_bytes(raw, ArchiveLimits::PACK)
        .err()
        .expect("a truncated zip has no central directory");
    assert!(matches!(error, ArchiveError::Corrupt { .. }), "{error:?}");
}

#[test]
fn a_corrupt_archive_of_each_format_is_refused_with_a_reason() {
    let mut zip = b"PK\x03\x04".to_vec();
    zip.extend_from_slice(&[0xEE; 200]);
    let mut seven = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
    seven.extend_from_slice(&[0xEE; 200]);
    let mut gz = vec![0x1f, 0x8b, 0x08, 0x00];
    gz.extend_from_slice(&[0xEE; 200]);
    let mut tar = vec![0u8; 257];
    tar.extend_from_slice(b"ustar  \0");
    tar.extend_from_slice(&[0xEE; 200]);

    for (name, bytes) in [("zip", zip), ("7z", seven), ("tar.gz", gz), ("tar", tar)] {
        let error = match SafeArchive::open_bytes(bytes, ArchiveLimits::PACK) {
            Ok(_) => panic!("{name}: a damaged archive is never read as empty"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                ArchiveError::Corrupt { .. } | ArchiveError::Unreadable { .. }
            ),
            "{name}: {error:?}"
        );
        assert!(
            !error.to_string().is_empty(),
            "{name}: the reason is stated"
        );
    }
}

// ---------------------------------------------------------------------------
// RAR, behind the same interface
// ---------------------------------------------------------------------------

fn rar_magic_bytes() -> Vec<u8> {
    let mut bytes = b"Rar!\x1a\x07\x00".to_vec();
    bytes.extend_from_slice(&[0u8; 64]);
    bytes
}

#[test]
fn a_rar_archive_without_a_reader_is_refused() {
    let error = SafeArchive::open_bytes(rar_magic_bytes(), ArchiveLimits::TECHNIC_PACK)
        .err()
        .expect("no reader was given");
    assert!(
        matches!(error, ArchiveError::NoRarReader { .. }),
        "{error:?}"
    );
}

struct FakeRar;

impl RarReader for FakeRar {
    fn list(&self, _archive: &Path) -> Result<Vec<RarEntry>, String> {
        Ok(vec![
            RarEntry {
                name: "mods/a.esp".to_string(),
                size: 4,
                is_directory: false,
            },
            RarEntry {
                name: "textures/aux.dds".to_string(),
                size: 2,
                is_directory: false,
            },
        ])
    }

    fn read_entry(&self, _archive: &Path, name: &str, out: &mut dyn Write) -> Result<u64, String> {
        let bytes: &[u8] = if name == "mods/a.esp" { b"data" } else { b"xx" };
        out.write_all(bytes).map_err(|error| error.to_string())?;
        Ok(bytes.len() as u64)
    }
}

#[test]
fn a_rar_archive_reads_through_the_seam() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.rar");
    std::fs::write(&path, rar_magic_bytes()).unwrap();
    let mut archive =
        SafeArchive::open_with_rar(&path, ArchiveLimits::TECHNIC_PACK, Box::new(FakeRar)).unwrap();
    assert_eq!(archive.format(), ArchiveFormat::Rar);
    assert_eq!(archive.file_entries().count(), 2);
    assert_eq!(archive.read("mods/a.esp").unwrap(), b"data");
    assert_eq!(archive.total_declared_bytes(), 6);

    // The same reader behind the same interface extracts, and the same naming rules apply: the
    // RAR's `textures/aux.dds` is refused by default like an entry of any other format.
    let out = dir.path().join("out");
    let mut archive =
        SafeArchive::open_with_rar(&path, ArchiveLimits::TECHNIC_PACK, Box::new(FakeRar)).unwrap();
    let error = archive.extract(&out, NamePolicy::Refuse).unwrap_err();
    assert!(
        matches!(error, ArchiveError::UnsafeNames { .. }),
        "{error:?}"
    );
    let report = archive.extract(&out, NamePolicy::WithoutThem).unwrap();
    assert_eq!(report.written, vec!["mods/a.esp".to_string()]);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(std::fs::read(out.join("mods/a.esp")).unwrap(), b"data");
}

#[test]
fn a_rar_reader_that_cannot_list_is_fail_closed() {
    struct Failing;
    impl RarReader for Failing {
        fn list(&self, _archive: &Path) -> Result<Vec<RarEntry>, String> {
            Err("tar.exe: Unrecognized archive format".to_string())
        }
        fn read_entry(
            &self,
            _archive: &Path,
            _name: &str,
            _out: &mut dyn Write,
        ) -> Result<u64, String> {
            Err("unreachable".to_string())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.rar");
    std::fs::write(&path, rar_magic_bytes()).unwrap();
    let error = SafeArchive::open_with_rar(&path, ArchiveLimits::TECHNIC_PACK, Box::new(Failing))
        .err()
        .expect("a reader that cannot list is an error, never an empty archive");
    match error {
        ArchiveError::Rar { reason, .. } => assert!(reason.contains("Unrecognized")),
        other => panic!("unexpected error {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// A caller's own rule
// ---------------------------------------------------------------------------

#[test]
fn a_callers_rule_can_leave_entries_out_and_the_reason_comes_back() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[("config/a.toml", b"a"), ("mods/b.jar", b"b")]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let report = archive
        .extract_where(&dest, NamePolicy::Refuse, &mut |name| {
            if name.starts_with("config/") {
                Select::Write
            } else {
                Select::LeaveOut("not in a folder this importer accepts".to_string())
            }
        })
        .unwrap();
    assert_eq!(report.written, vec!["config/a.toml".to_string()]);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].name, "mods/b.jar");
    assert!(report.skipped[0].reason.contains("not in a folder"));
    drop(dir);
}

#[test]
fn a_callers_rule_can_refuse_the_whole_extraction() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[("config/a.toml", b"a"), ("config/setup.exe", b"b")]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let error = archive
        .extract_where(&dest, NamePolicy::Refuse, &mut |name| {
            if name.ends_with(".exe") {
                Select::Refuse("an executable has no place in a config folder".to_string())
            } else {
                Select::Write
            }
        })
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Refused { .. }), "{error:?}");
    assert!(error.to_string().contains("config/setup.exe"));
    drop(dir);
}

// ---------------------------------------------------------------------------
// A runtime's executable bit, and an operation a person can stop
// ---------------------------------------------------------------------------

#[test]
fn a_tar_entry_that_recorded_an_executable_mode_keeps_it() {
    let (dir, dest) = extract_dir();
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(6);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "bin/java", &b"binary"[..])
        .unwrap();
    let bytes = builder.into_inner().unwrap();

    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::RUNTIME).unwrap();
    archive.extract(&dest, NamePolicy::Refuse).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let written = std::fs::metadata(dest.join("bin/java")).unwrap();
        assert_eq!(written.permissions().mode() & 0o111, 0o111);
    }
    drop(dir);
}

#[test]
fn a_cancelled_extraction_stops_between_entries() {
    let (dir, dest) = extract_dir();
    let mut archive = SafeArchive::open_bytes(
        zip_bytes(&[
            ("a/first.txt", b"one"),
            ("a/second.txt", b"two"),
            ("a/third.txt", b"three"),
        ]),
        ArchiveLimits::PACK,
    )
    .unwrap();
    let asked = std::cell::Cell::new(0u32);
    let error = archive
        .extract_checked(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| {
            asked.set(asked.get() + 1);
            // Asked once before each entry and twice inside each copy — a three-byte entry is one
            // read of its bytes and one of the end — so the seventh ask is the start of the third
            // entry, and the answer arrives after two complete files and no partial third.
            asked.get() > 6
        })
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Cancelled { .. }), "{error:?}");
    assert_eq!(
        archive.written_so_far().len(),
        2,
        "the two entries before the stop are written and reported, so the caller can throw its \
         staging directory away"
    );
    drop(dir);
}

#[test]
fn a_cancelled_extraction_stops_inside_one_large_entry() {
    // One 4 MiB entry is sixty-four buffers long, so asking between entries is not enough: the
    // answer only arrives once the whole entry has been written.
    let (dir, dest) = extract_dir();
    let big = vec![7u8; 4 * 1024 * 1024];
    let mut archive =
        SafeArchive::open_bytes(tar_bytes(&[("big.bin", &big)]), ArchiveLimits::PACK).unwrap();
    let asked = std::cell::Cell::new(0u32);
    let error = archive
        .extract_checked(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| {
            asked.set(asked.get() + 1);
            asked.get() > 2
        })
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Cancelled { .. }), "{error:?}");
    let written = std::fs::metadata(dest.join("big.bin")).unwrap().len();
    assert!(
        written < big.len() as u64,
        "the copy stopped partway through, at {written} bytes"
    );
    drop(dir);
}

#[test]
fn an_extraction_reads_the_archive_once() {
    // A world of 50 000 entries took a fresh parse of the whole archive per file. The count here is
    // the archive's own parses, not the clock: opening lists once, and the extraction is one more.
    let names: Vec<String> = (0..200).map(|index| format!("data/{index}.txt")).collect();
    let empty: &[u8] = b"x";
    let entries: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), empty)).collect();
    let mut archive = SafeArchive::open_bytes(zip_bytes(&entries), ArchiveLimits::PACK).unwrap();
    let (dir, dest) = extract_dir();
    let report = archive.extract(&dest, NamePolicy::Refuse).unwrap();
    assert_eq!(report.written.len(), 200);
    assert_eq!(
        archive.parses(),
        2,
        "one parse to list, one to extract the 200 entries"
    );
    drop(dir);
}

/// A tar with one header and no bytes behind it: the shape a stream has when the listing is all
/// that was ever going to arrive.
fn tar_with_one_header(declared: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut header = [0u8; 512];
    header[..9].copy_from_slice(b"big.bin\0\0");
    header[100..108].copy_from_slice(b"0000644\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    header[124..136].copy_from_slice(format!("{declared:011o}\0").as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    out.extend_from_slice(&header);
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn gzipped(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

/// A zip of three empty folders and two files: five records, two of them files.
fn zip_with_folders() -> Vec<u8> {
    use std::io::Write as _;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options: zip::write::SimpleFileOptions = Default::default();
    for folder in ["a/", "b/", "c/"] {
        zip.add_directory(folder, options).unwrap();
    }
    for (name, bytes) in [("a/x.txt", &b"x"[..]), ("b/y.txt", &b"y"[..])] {
        zip.start_file(name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

#[test]
fn a_preset_that_counted_folders_still_counts_them() {
    // The runtime, plugin, world, pack and mod-archive counters were `ZipArchive::len()`, which
    // counts every record: a package of empty folders is not a way around them.
    let bytes = zip_with_folders();
    let mut folding = small_limits();
    folding.counts_folders = true;
    let error = SafeArchive::open_bytes(bytes.clone(), folding)
        .err()
        .expect("five records is one more than four");
    match error {
        ArchiveError::TooManyEntries { count, limit, .. } => {
            assert_eq!(count, 5);
            assert_eq!(limit, 4);
        }
        other => panic!("unexpected error {other:?}"),
    }

    // A job whose counter was its manifest's file list counts files, and the same archive is
    // inside its limit.
    let mut files_only = small_limits();
    files_only.counts_folders = false;
    SafeArchive::open_bytes(bytes, files_only).expect("two files, four allowed");
}

#[test]
fn a_sevenz_with_no_per_entry_sizes_is_held_to_the_whole_archive_ratio() {
    // A 7z records no compressed size per entry, so the entry-by-entry ratio a zip gets cannot be
    // taken: left unchecked, a block of zeros is a bomb nothing counts.
    let zeros = vec![0u8; 20 * 1024 * 1024];
    let bytes = sevenz_bytes(&[("big.bin", &zeros)]);
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("twenty megabytes of zeros out of a few kilobytes is not a pack");
    assert!(
        matches!(error, ArchiveError::RatioExceeded { .. }),
        "{error:?}"
    );
}

#[test]
fn a_tar_gz_is_refused_as_it_streams_not_after_it_has_been_read() {
    // A compressed tar is inflated to be listed at all, so the header that declares more than the
    // preset allows has to be refused there and then. Read to its end first, the stream fails as a
    // truncated archive instead, which is a different thing and a later one.
    let mut limits = small_limits();
    limits.max_entry_bytes = 1024 * 1024;
    limits.max_total_bytes = 4 * 1024 * 1024;
    let bytes = gzipped(&tar_with_one_header(4 * 1024 * 1024));
    let error = SafeArchive::open_bytes(bytes, limits)
        .err()
        .expect("four megabytes is over a one-megabyte entry");
    assert!(
        matches!(error, ArchiveError::TooLarge { .. }),
        "the size is what refuses it: {error:?}"
    );
}

#[test]
fn cp437_names_are_decoded_not_flattened() {
    // A zip written before UTF-8 names means CP437 by those bytes. Decoded lossily, `üü` and `ÇÇ`
    // are both two replacement characters, so two distinct entries looked like one archive hiding
    // an entry behind another.
    let bytes = zip_with_raw_names([[0x81, 0x81], [0x80, 0x80]]);
    let archive =
        SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).expect("two distinct names, not one");
    let names: Vec<&str> = archive.file_entries().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["\u{00FC}\u{00FC}", "\u{00C7}\u{00C7}"]);

    // And two records the decoder reads as one name are still refused.
    let bytes = zip_with_raw_names([[0x81, 0x81], [0x81, 0x81]]);
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("one name declared twice");
    match error {
        ArchiveError::DuplicateNames { names, .. } => {
            assert_eq!(names, vec!["\u{00FC}\u{00FC}".to_string()]);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

/// A zip whose two entries' names are these raw bytes, two bytes each: the writer only takes `str`,
/// so an ASCII name is written and the same number of bytes is patched in its place.
fn zip_with_raw_names(names: [[u8; 2]; 2]) -> Vec<u8> {
    let mut bytes = zip_bytes(&[("aa", b"one"), ("bb", b"two")]);
    for (placeholder, raw) in [b"aa".as_slice(), b"bb".as_slice()].into_iter().zip(names) {
        // Every occurrence: the local header and the central record both carry the name.
        let mut at = 0;
        while at + placeholder.len() <= bytes.len() {
            if &bytes[at..at + placeholder.len()] == placeholder {
                bytes[at..at + raw.len()].copy_from_slice(&raw);
                at += raw.len();
            } else {
                at += 1;
            }
        }
    }
    bytes
}

#[test]
fn a_name_that_is_a_file_once_and_a_folder_once_is_refused() {
    // No disk holds both, and the listing would pick one while the other vanished without a word.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options: zip::write::SimpleFileOptions = Default::default();
    zip.add_directory("a/", options).unwrap();
    zip.start_file("a", options).unwrap();
    zip.write_all(b"here and there").unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("one name, two records");
    match error {
        ArchiveError::DuplicateNames { names, .. } => assert_eq!(names, vec!["a".to_string()]),
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn a_folder_record_that_leaves_the_destination_is_refused() {
    // A folder record writes nothing itself, but it names a place: `../outside/` is a way out of
    // the destination whichever kind of record it is spelled as.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.add_directory("../outside/", zip::write::SimpleFileOptions::default())
        .unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let (dir, dest) = extract_dir();
    let error = archive
        .extract(&dest, NamePolicy::Refuse)
        .expect_err("a folder outside the root");
    assert!(matches!(error, ArchiveError::Escapes { .. }), "{error:?}");
    drop(dir);
}

#[test]
fn a_zip_folder_that_claims_a_symlinks_mode_is_refused() {
    // Read as a folder it writes nothing; read as a link it is a way out. The mode decides.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.add_symlink(
        "textures/",
        "..",
        zip::write::SimpleFileOptions::default().unix_permissions(0o120777),
    )
    .unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let (dir, dest) = extract_dir();
    let error = archive
        .extract(&dest, NamePolicy::Refuse)
        .expect_err("a record that claims a symlink's mode");
    assert!(matches!(error, ArchiveError::LinkEntry { .. }), "{error:?}");
    drop(dir);
}

#[test]
fn a_folder_recorded_twice_is_one_folder() {
    // Archive tools write a parent folder more than once and mean one folder; refusing that would
    // refuse ordinary archives, so only a *file* declared twice is a duplicate.
    let mut builder = tar::Builder::new(Vec::new());
    for _ in 0..2 {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "config/", &[][..])
            .unwrap();
    }
    let bytes = builder.into_inner().unwrap();
    let archive =
        SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).expect("one folder, written twice");
    assert_eq!(archive.file_entries().count(), 0);
}

// ---------------------------------------------------------------------------
// Staged publishing (MASTER_SPEC §32.3, §32.13)
// ---------------------------------------------------------------------------

/// The staging folders left beside `folder`, which is where `SafeArchive::stage` puts them.
fn staging_folders_beside(folder: &Path) -> Vec<String> {
    std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.contains(".agora-extract-"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_publish_that_cannot_finish_leaves_the_destination_alone() {
    // The reviewers' fixture: the pack replaces `config/settings.toml` and then needs
    // `config/blocked` to be a folder, and it is a regular file. Every target is decided before
    // anything moves, so the file the player already had is still theirs and nothing half-written is
    // left behind — not even the staging folder, which is made beside the destination and removed by
    // `Drop` however the publish ends.
    let bytes = zip_bytes(&[
        ("config/settings.toml", b"from the pack"),
        ("config/blocked/extra.toml", b"extra"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("instance");
    std::fs::create_dir_all(dest.join("config")).unwrap();
    std::fs::write(dest.join("config/settings.toml"), b"the player's own").unwrap();
    std::fs::write(dest.join("config/blocked"), b"a regular file").unwrap();

    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let staged = archive
        .stage(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| false)
        .expect("both entries can be written into a staging folder");
    let error = staged
        .publish_into()
        .expect_err("a file where the pack needs a folder");
    let text = error.to_string();

    // The damage first: whatever the message says, the player's file is still theirs.
    assert_eq!(
        std::fs::read(dest.join("config/settings.toml")).unwrap(),
        b"the player's own",
        "the settings file the player had is untouched"
    );
    assert!(text.contains("config/blocked"), "{text}");
    assert_eq!(
        std::fs::read(dest.join("config/blocked")).unwrap(),
        b"a regular file"
    );
    assert!(!dest.join("config/blocked/extra.toml").exists());
    let left = staging_folders_beside(dir.path());
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_publish_into_a_missing_destination_makes_it() {
    // An instance that has never written a `config/` folder is the ordinary case for a first import:
    // the destination is made, the staging folder is made beside it, and the files arrive.
    let bytes = zip_bytes(&[
        ("config/settings.toml", b"from the pack"),
        ("config/deep/note.txt", b"nested"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("instance");
    assert!(!dest.exists());

    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let staged = archive
        .stage(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| false)
        .unwrap();
    let report = staged.publish_into().expect("nothing is in the way");

    assert_eq!(report.written.len(), 2, "{:?}", report.written);
    assert_eq!(
        std::fs::read(dest.join("config/settings.toml")).unwrap(),
        b"from the pack"
    );
    assert_eq!(
        std::fs::read(dest.join("config/deep/note.txt")).unwrap(),
        b"nested"
    );
    let left = staging_folders_beside(dir.path());
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_folder_where_the_file_goes_is_refused_before_anything_moves() {
    // A folder — or a file somebody holds open — is a target the publish cannot land on. It is found
    // in the preflight, so the entry that would have been replaced next to it is still the old one.
    let bytes = zip_bytes(&[
        ("config/settings.toml", b"from the pack"),
        ("config/other.toml", b"from the pack too"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("instance");
    std::fs::create_dir_all(dest.join("config/other.toml")).unwrap();
    std::fs::write(dest.join("config/settings.toml"), b"the player's own").unwrap();

    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let staged = archive
        .stage(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| false)
        .unwrap();
    let error = staged.publish_into().expect_err("a folder is in the way");
    assert!(error.to_string().contains("config/other.toml"), "{error}");
    assert_eq!(
        std::fs::read(dest.join("config/settings.toml")).unwrap(),
        b"the player's own"
    );
    let left = staging_folders_beside(dir.path());
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn an_archive_entry_named_like_the_set_aside_area_does_not_meet_the_players_file() {
    // The files a publish replaces are set aside somewhere the archive's own entries cannot be. An
    // archive that carries `replaced/config/a.toml` next to `config/a.toml` must publish both as the
    // archive has them, and the player's old `config/a.toml` must not turn up in either.
    let bytes = zip_bytes(&[
        ("config/a.toml", b"new a"),
        ("replaced/config/a.toml", b"the archive's own replaced/"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("instance");
    std::fs::create_dir_all(dest.join("config")).unwrap();
    std::fs::write(dest.join("config/a.toml"), b"the player's old a").unwrap();

    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let staged = archive
        .stage(&dest, NamePolicy::Refuse, &mut |_| Select::Write, &|| false)
        .unwrap();
    staged.publish_into().expect("nothing is in the way");

    assert_eq!(std::fs::read(dest.join("config/a.toml")).unwrap(), b"new a");
    assert_eq!(
        std::fs::read(dest.join("replaced/config/a.toml")).unwrap(),
        b"the archive's own replaced/"
    );
    let left = staging_folders_beside(dir.path());
    assert!(left.is_empty(), "{left:?}");
}

/// A backend that records the listing it was staged with, and can refuse staging.
struct StagingRar {
    admitted: std::sync::Arc<std::sync::Mutex<Vec<RarEntry>>>,
    refuse: bool,
}

impl RarReader for StagingRar {
    fn list(&self, _archive: &Path) -> Result<Vec<RarEntry>, String> {
        Ok(vec![
            RarEntry {
                name: "mods/a.esp".to_string(),
                size: 4,
                is_directory: false,
            },
            RarEntry {
                name: "textures/".to_string(),
                size: 0,
                is_directory: true,
            },
        ])
    }

    fn stage(&self, _archive: &Path, admitted: &[RarEntry]) -> Result<(), String> {
        *self.admitted.lock().unwrap() = admitted.to_vec();
        if self.refuse {
            return Err("the archive changed since it was opened".to_string());
        }
        Ok(())
    }

    fn read_entry(&self, _archive: &Path, name: &str, out: &mut dyn Write) -> Result<u64, String> {
        if name != "mods/a.esp" {
            return Err(format!("no entry is named '{name}'"));
        }
        out.write_all(b"data").map_err(|error| error.to_string())?;
        Ok(4)
    }
}

fn staging_rar(refuse: bool) -> (StagingRar, std::sync::Arc<std::sync::Mutex<Vec<RarEntry>>>) {
    let admitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let backend = StagingRar {
        admitted: std::sync::Arc::clone(&admitted),
        refuse,
    };
    (backend, admitted)
}

#[test]
fn a_rar_backend_is_staged_against_the_listing_the_limits_used() {
    // Listing is a second read of the file for a RAR, so the reader says which listing it measured
    // before anything is served: names, sizes and count, as they were when the limits were applied.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.rar");
    std::fs::write(&path, rar_magic_bytes()).unwrap();
    let (backend, admitted) = staging_rar(false);
    let mut archive =
        SafeArchive::open_with_rar(&path, ArchiveLimits::GAME_MOD, Box::new(backend)).unwrap();
    assert_eq!(archive.read("mods/a.esp").unwrap(), b"data");
    assert_eq!(
        *admitted.lock().unwrap(),
        vec![
            RarEntry {
                name: "mods/a.esp".to_string(),
                size: 4,
                is_directory: false
            },
            RarEntry {
                name: "textures/".to_string(),
                size: 0,
                is_directory: true
            },
        ],
        "the whole listing, folders included, with the sizes it was held to"
    );
}

#[test]
fn a_rar_that_changed_since_it_was_opened_is_refused() {
    // A RAR really can be replaced between the listing and the copy. The backend's relisting then
    // disagrees with the listing the limits were applied to, and nothing is served from it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.rar");
    std::fs::write(&path, rar_magic_bytes()).unwrap();
    let (backend, _admitted) = staging_rar(true);
    let mut archive =
        SafeArchive::open_with_rar(&path, ArchiveLimits::GAME_MOD, Box::new(backend)).unwrap();
    let error = archive
        .read("mods/a.esp")
        .expect_err("the archive changed under the reader");
    assert!(matches!(error, ArchiveError::Rar { .. }), "{error:?}");
    assert!(
        error.to_string().contains("changed since it was opened"),
        "{error}"
    );

    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    assert!(archive.extract(&out, NamePolicy::Refuse).is_err());
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "no part of an archive nothing measured reaches a folder"
    );
}

#[test]
fn a_tar_gz_whose_gzip_trailer_is_wrong_is_refused() {
    // The gzip trailer — CRC-32 and the uncompressed length — comes *after* the tar end marker, so a
    // reader that stops at the marker never checks it and calls a corrupt stream a good archive.
    let mut bytes = tar_gz_bytes(&[("mods/a.esp", b"data")]);
    let good = SafeArchive::open_bytes(bytes.clone(), ArchiveLimits::PACK).expect("a good tar.gz");
    assert_eq!(good.file_entries().count(), 1);

    let trailer = bytes.len() - 8;
    bytes[trailer] ^= 0xFF;
    let error = SafeArchive::open_bytes(bytes.clone(), ArchiveLimits::PACK)
        .err()
        .expect("the trailer CRC no longer matches");
    assert!(matches!(error, ArchiveError::Corrupt { .. }), "{error:?}");

    assert!(
        SafeArchive::open_bytes(bytes[..trailer + 2].to_vec(), ArchiveLimits::PACK).is_err(),
        "a trailer that is not all there is the same kind of lie"
    );
}

/// A hand-built zip whose records carry the names, general-purpose flags and Info-ZIP Unicode Path
/// (0x7075) fields a test names, because neither the writer nor the reader's helpers can produce that
/// combination.
///
/// Each record is `(header name bytes, general-purpose flags, Some((the 0x7075 spelling, the bytes
/// its own check value covers)))`.
/// One record for [`zip_with_unicode_paths`]: its header name bytes, its general-purpose flags, and
/// the 0x7075 spelling it carries with the bytes that spelling's check value covers.
type ZipRecord<'a> = (&'a [u8], u16, Option<(&'a str, &'a [u8])>);

fn zip_with_unicode_paths(records: &[ZipRecord<'_>]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        crc.sum()
    }
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for (header, flags, unicode) in records {
        let data = b"x";
        let crc = crc32(data);
        let mut extra = Vec::new();
        if let Some((unicode_name, crc_over)) = unicode {
            let mut body = vec![1u8];
            body.extend_from_slice(&crc32(crc_over).to_le_bytes());
            body.extend_from_slice(unicode_name.as_bytes());
            extra.extend_from_slice(&0x7075u16.to_le_bytes());
            extra.extend_from_slice(&(body.len() as u16).to_le_bytes());
            extra.extend_from_slice(&body);
        }
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(header.len() as u16).to_le_bytes());
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(header);
        out.extend_from_slice(&extra);
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(header.len() as u16).to_le_bytes());
        central.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(header);
        central.extend_from_slice(&extra);
    }
    let central_offset = out.len() as u32;
    let central_len = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(records.len() as u16).to_le_bytes());
    out.extend_from_slice(&(records.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_len.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[test]
fn two_records_whose_unicode_path_names_agree_are_one_name() {
    // `a.txt` and `b.txt` each carry an Info-ZIP Unicode Path field naming `shared.txt`. Whichever
    // spelling a reader trusts, the archive holds one path twice, so it is refused rather than
    // leaving one of them silently unreachable.
    let bytes = zip_with_unicode_paths(&[
        (b"a.txt", 0, Some(("shared.txt", b"a.txt"))),
        (b"b.txt", 0, Some(("shared.txt", b"b.txt"))),
    ]);
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("one path declared twice");
    match error {
        ArchiveError::DuplicateNames { names, .. } => {
            assert_eq!(names, vec!["shared.txt".to_string()]);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn a_unicode_path_on_a_utf8_flagged_record_still_counts() {
    // The UTF-8 flag says how to read the header name; it says nothing about the extra field, which
    // is Info-ZIP's own spelling of the same path. Both records' fields name `shared.txt`, and the
    // check value matches each header name, so both spellings are the archive's claim.
    let bytes = zip_with_unicode_paths(&[
        (b"a.txt", 0x0800, Some(("shared.txt", b"a.txt"))),
        (b"b.txt", 0x0800, Some(("shared.txt", b"b.txt"))),
    ]);
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("one path declared twice, whatever the flag says");
    match error {
        ArchiveError::DuplicateNames { names, .. } => {
            assert_eq!(names, vec!["shared.txt".to_string()]);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[test]
fn a_unicode_path_whose_check_value_is_wrong_names_nothing() {
    // The field's own CRC-32 does not match the header name it sits beside, so — APPNOTE 4.6.9.4 —
    // it says nothing about this record, and two distinct entries stay distinct.
    let bytes = zip_with_unicode_paths(&[
        (b"a.txt", 0, Some(("shared.txt", b"not-this-name"))),
        (b"b.txt", 0, Some(("shared.txt", b"nor-this-one"))),
    ]);
    let archive =
        SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).expect("two distinct names, two files");
    assert_eq!(archive.file_entries().count(), 2);
}

#[test]
fn names_the_zip_crate_reads_as_one_are_caught_even_when_the_bytes_differ() {
    // `[0xC3,0xA9]` is the UTF-8 spelling of `é` and `[0x82]` is the CP437 one. `zip` 9.0.5's own
    // rule (`ZipFileData::name`) is "the bytes as UTF-8 when they are valid UTF-8, CP437 otherwise",
    // so it reads both records as `é`: the archive holds one name twice. A scanner that decoded by
    // the general-purpose flag instead would call them `├⌐` and `é`, and miss it.
    let bytes = zip_with_unicode_paths(&[(&[0xC3u8, 0xA9u8], 0, None), (&[0x82u8], 0, None)]);
    let error = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK)
        .err()
        .expect("one name, two spellings of it");
    match error {
        ArchiveError::DuplicateNames { names, .. } => {
            assert_eq!(names, vec!["\u{00E9}".to_string()]);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

/// A tar.gz of `count` folder records with a block that is not a header after them: an archive whose
/// count is over the preset never reads that far.
fn tar_gz_of_folders_then_garbage(count: usize) -> Vec<u8> {
    let mut tar = Vec::new();
    for index in 0..count {
        let mut header = [0u8; 512];
        let name = format!("d{index}/");
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(b"00000000000\0");
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'5';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        tar.extend_from_slice(&header);
    }
    tar.extend(std::iter::repeat_n(0xEEu8, 512));
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn a_tar_gz_of_too_many_folders_is_refused_as_it_streams() {
    // Folders count where the preset counts folders, and a stream is held to the count while it is
    // being read: the block after the folders is not a header at all, so an archive refused only
    // after its listing is complete would report a damaged archive instead of a count.
    let mut limits = small_limits();
    limits.max_entries = 3;
    let bytes = tar_gz_of_folders_then_garbage(5);
    let error = SafeArchive::open_bytes(bytes, limits)
        .err()
        .expect("five folder records, three allowed");
    assert!(
        matches!(error, ArchiveError::TooManyEntries { count: 4, .. }),
        "the count refuses it while streaming: {error:?}"
    );
}

#[test]
fn stopping_while_an_unwanted_solid_member_is_drained_is_noticed() {
    // A solid 7z decodes every member of a block in order, so a member nobody asked for is drained
    // rather than stepped over. The person who asked to stop must not wait for it: cancellation is
    // asked between buffers, and the extraction ends as cancelled rather than finishing the drain.
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Incompressible, so the archive is not refused by the ratio check before the drain is reached.
    let mut seed = 0x1234_5678u32;
    let big: Vec<u8> = (0..512 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as u8
        })
        .collect();
    let bytes = solid_sevenz_bytes(&[("a.bin", &big)]);
    let mut archive = SafeArchive::open_bytes(bytes, ArchiveLimits::PACK).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out");
    // One member, and nothing is wanted, so the only reads are its drain. The first call is the entry
    // check before it; the second is inside the drain, and says stop — which is the whole point: a
    // stop that is only noticed between entries waits for the member under way.
    let calls = AtomicUsize::new(0);
    let cancelled = || calls.fetch_add(1, Ordering::SeqCst) >= 1;
    let error = archive
        .extract_checked(
            &dest,
            NamePolicy::Refuse,
            &mut |_| Select::LeaveOut("not this one".to_string()),
            &cancelled,
        )
        .expect_err("the drain noticed the stop");
    assert!(matches!(error, ArchiveError::Cancelled { .. }), "{error:?}");
}
