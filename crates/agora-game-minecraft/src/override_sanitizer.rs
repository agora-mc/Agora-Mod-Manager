use agora_core::error::{LauncherError, LauncherResult};
use agora_game_api::{ArchiveError, ArchiveLimits, NamePolicy, SafeArchive, Select};
use serde::Serialize;
use std::path::Path;

/// Hard limits for zip extraction (§7.2.1). The sizes an *archive may hold* are the reader's, in
/// [`ArchiveLimits::PACK`]; this is the one a caller can see before anything is opened: the file
/// itself.
pub const MAX_ZIP_SIZE: u64 = 500 * 1024 * 1024; // 500MB compressed

/// What to do about an entry whose name a disk cannot hold.
///
/// `Refuse` is the default and the safe answer: the person who asked for the import is shown every
/// offending name and why. `WithoutThem` is their explicit answer, and the entries they gave up come
/// back in [`ExtractionResult::left_out`] so the caller can list them.
pub type NamesPolicy = NamePolicy;

/// Directory whitelist (§7.2.2). Only files under these prefixes are extracted.
///
/// # `kubejs/` is deliberately allowed, and is NOT inert
///
/// Every other prefix here holds data the game reads. `kubejs/` does not: the
/// KubeJS mod *executes* the `.js` files it finds there, and those scripts can
/// reach Java classes directly — they are not sandboxed the way browser
/// JavaScript is. A pack that ships `kubejs/startup_scripts/x.js` is shipping
/// code that runs on the player's machine with the game's own permissions.
///
/// This is an accepted trade-off, not an oversight: KubeJS scripting is a
/// normal part of how packs are built, and packs are reviewed by hand before
/// they enter the registry. **Curation is the control for this prefix, not
/// [`BANNED_EXTENSIONS`].** Do not reason about `kubejs/` as though the
/// extension ban made overrides safe, and do not widen either list on the
/// assumption that it did.
///
/// `scripts/` is CraftTweaker's equivalent of `kubejs/` and carries the same
/// caveat. `global_packs/`, `openloader/` and `patchouli_books/` are data the
/// game (or a data-loading mod) reads.
pub(crate) const ALLOWED_PREFIXES: &[&str] = &[
    "config/",
    "defaultconfigs/",
    "resourcepacks/",
    "shaderpacks/",
    "datapacks/",
    "kubejs/",
    "scripts/",
    "global_packs/",
    "openloader/",
    "patchouli_books/",
];

/// Banned extensions (§7.2.2). Hard-banned even inside whitelisted directories.
///
/// This list stops an overrides bundle from dropping a binary or a shell script
/// where a config file belongs. It does not, and cannot, make overrides
/// generally inert — see the `kubejs/` note on [`ALLOWED_PREFIXES`].
const BANNED_EXTENSIONS: &[&str] = &[
    ".jar", ".class", ".exe", ".bat", ".cmd", ".sh", ".ps1", ".dll", ".so", ".dylib", ".msi",
    ".dmg",
];

/// Instance files Agora owns. A pack never gets to overwrite these, in any mode.
const AGORA_OWNED_FILES: &[&str] = &["instance_manifest.json"];

/// How much of an overrides archive is accepted.
///
/// `Standard` is the whitelist above. `Permissive` is what the user opts into
/// with **Reduced security mode**: any path inside the instance, `.jar` files
/// included (plenty of packs ship mods in their overrides), because the user
/// chose to trust the pack. Some things stay refused in both modes, since no
/// pack needs them and the downside is large: path traversal, native
/// executables and scripts (Minecraft never runs them, so they are only useful
/// to an attacker), and files Agora itself keeps in the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverridePolicy {
    #[default]
    Standard,
    Permissive,
}

impl OverridePolicy {
    /// The policy the user's settings ask for.
    pub fn from_settings(ctx: &agora_core::ctx::Ctx) -> Self {
        if agora_core::settings::reduced_security_enabled(ctx) {
            Self::Permissive
        } else {
            Self::Standard
        }
    }

    /// Whether a sanitised, instance-relative path is extracted at all.
    /// Paths it declines are skipped, not treated as an attack.
    pub fn admits(self, path: &str) -> bool {
        let path = as_created(path);
        if is_agora_owned(path) {
            return false;
        }
        match self {
            Self::Standard => is_whitelisted(path),
            Self::Permissive => true,
        }
    }

    /// Whether a path that [`admits`](Self::admits) accepted is still refused
    /// outright, failing the extraction.
    pub fn forbids(self, path: &str) -> bool {
        let path = as_created(path);
        match self {
            Self::Standard => has_banned_extension(path),
            Self::Permissive => {
                let lower = path.to_ascii_lowercase();
                agora_plugin_api::provider::BANNED_EXTENSIONS
                    .iter()
                    .any(|ext| lower.ends_with(ext))
            }
        }
    }

    /// Whether a path in a pack's index `files` list is refused outright.
    ///
    /// The `files` list is where a pack's mods live, so a `.jar` there is its
    /// payload and is allowed. What no pack needs is a native executable or
    /// script (Minecraft never runs one) or a file Agora keeps for itself.
    /// Shared by the override extractor and the `files` loop so the two cannot
    /// drift apart, which is how a `.bat` used to get in through `files`.
    pub fn refuses_pack_file(self, path: &str) -> bool {
        let path = as_created(path);
        if is_agora_owned(path) {
            return true;
        }
        let lower = path.to_ascii_lowercase();
        agora_plugin_api::provider::BANNED_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(ext))
    }
}

/// The name a path gets on disk. Windows drops trailing dots and spaces
/// when it creates a file, so `config/run.bat.` is written as
/// `config/run.bat`; every check has to look at that name, not the raw one.
fn as_created(path: &str) -> &str {
    path.trim_end_matches(['.', ' '])
}

fn is_agora_owned(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    AGORA_OWNED_FILES.contains(&lower.as_str())
        || lower
            .split('/')
            .any(|segment| segment.starts_with(".agora"))
}

/// Result of an override extraction.
#[derive(Debug, Clone, Serialize)]
pub struct ExtractionResult {
    /// The paths written, relative to the instance.
    pub extracted: Vec<String>,
    /// The paths not written, by name: outside the policy's folders, or a name a disk cannot hold.
    pub skipped: Vec<String>,
    /// The same entries as `skipped`, each with the reason it was left out. This is what a caller
    /// shows a person who asked for the import, so nothing is dropped without saying why.
    pub left_out: Vec<LeftOutEntry>,
    pub total_bytes_written: u64,
}

/// One entry a pack import did not write, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftOutEntry {
    pub path: String,
    pub reason: String,
}

/// Extract a zip file into an instance directory with full sanitization.
///
/// This is the main entry point for §7.2. The archive is read through
/// [`SafeArchive`](../agora_game_api/archive/struct.SafeArchive.html), which owns the limits
/// (the `PACK` preset), the path rules and the write, so this function owns only what is specific to
/// a pack's overrides:
/// 1. Checks compressed size against MAX_ZIP_SIZE.
/// 2. Leaves out entries outside the whitelist (or the whole archive, under `Permissive`).
/// 3. Refuses banned extensions even within whitelisted directories.
/// 4. Refuses zip slip, links and a damaged archive, through the reader.
/// 5. Refuses by default an entry whose name a disk cannot hold, and names each one; the caller may
///    ask for the rest instead, and gets the list of what was left out.
/// 6. On any refusal, deletes the files it had already written.
pub fn extract_overrides(zip_path: &Path, dest_dir: &Path) -> LauncherResult<ExtractionResult> {
    extract_overrides_with_inputs(
        zip_path,
        dest_dir,
        OverridePolicy::Standard,
        NamesPolicy::Refuse,
    )
}

/// [`extract_overrides`] under an explicit [`OverridePolicy`].
pub fn extract_overrides_with(
    zip_path: &Path,
    dest_dir: &Path,
    policy: OverridePolicy,
) -> LauncherResult<ExtractionResult> {
    extract_overrides_with_inputs(zip_path, dest_dir, policy, NamesPolicy::Refuse)
}

/// [`extract_overrides_with`], and what to do about an entry whose name a disk cannot hold.
///
/// `NamesPolicy::WithoutThem` is the caller's explicit answer to a refusal it was shown: the usable
/// entries are written and the rest come back in [`ExtractionResult::left_out`]. Nothing is ever
/// dropped without a name and a reason.
pub fn extract_overrides_with_inputs(
    zip_path: &Path,
    dest_dir: &Path,
    policy: OverridePolicy,
    names: NamesPolicy,
) -> LauncherResult<ExtractionResult> {
    // Pre-check: compressed file size.
    let zip_size = zip_path
        .metadata()
        .map_err(|_| LauncherError::Generic {
            code: "ERR_OVERRIDE_FAILED".to_string(),
            message: "Could not read zip file metadata.".to_string(),
        })?
        .len();

    if zip_size > MAX_ZIP_SIZE {
        return Err(LauncherError::Generic {
            code: "ERR_ZIP_TOO_LARGE".to_string(),
            message: format!(
                "Zip file is {}MB, exceeds the {}MB limit.",
                zip_size / (1024 * 1024),
                MAX_ZIP_SIZE / (1024 * 1024)
            ),
        });
    }

    let mut archive = SafeArchive::open(zip_path, ArchiveLimits::PACK).map_err(archive_error)?;
    // Staged, never written straight into the instance: a pack that replaces `config/settings.toml`
    // and then fails on a later entry has replaced nothing (MASTER_SPEC §32.3).
    let staged = archive
        .stage(
            dest_dir,
            names,
            &mut |name| {
                if !policy.admits(name) {
                    return Select::LeaveOut(if is_agora_owned(as_created(name)) {
                        "Agora keeps this file in the instance".to_string()
                    } else {
                        "not a folder a pack's overrides may write".to_string()
                    });
                }
                if policy.forbids(name) {
                    return Select::Refuse(format!(
                    "Security Violation: Pack overrides cannot contain executable files or mods. \
                     Banned file type detected: '{name}'. All mods must be routed through the \
                     platform manifest."
                ));
                }
                Select::Write
            },
            &|| false,
        )
        .map_err(archive_error)?;

    let report = staged.publish_into(dest_dir).map_err(archive_error)?;
    let mut extracted = report.written;
    extracted.sort();
    Ok(ExtractionResult {
        extracted,
        skipped: report
            .skipped
            .iter()
            .map(|entry| entry.name.clone())
            .collect(),
        left_out: report
            .skipped
            .into_iter()
            .map(|entry| LeftOutEntry {
                path: entry.name,
                reason: entry.reason,
            })
            .collect(),
        total_bytes_written: report.bytes_written,
    })
}

/// The reader's refusal, as the error codes the CLI and the desktop already report (§32.10).
fn archive_error(error: ArchiveError) -> LauncherError {
    let code = match &error {
        ArchiveError::TooManyEntries { .. } => "ERR_TOO_MANY_FILES",
        ArchiveError::TooLarge { .. } | ArchiveError::RatioExceeded { .. } => "ERR_ZIP_BOMB",
        ArchiveError::Escapes { .. } | ArchiveError::LinkEntry { .. } => "ERR_ZIP_SLIP",
        ArchiveError::UnsafeNames { .. } => "ERR_ZIP_UNSAFE_NAMES",
        ArchiveError::Refused { .. } => "ERR_SECURITY_VIOLATION",
        _ => "ERR_OVERRIDE_FAILED",
    };
    LauncherError::Generic {
        code: code.to_string(),
        message: error.to_string(),
    }
}

/// Check if a path starts with one of the whitelisted directory prefixes.
fn is_whitelisted(path: &str) -> bool {
    ALLOWED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Check if a filename has a banned extension.
fn has_banned_extension(path: &str) -> bool {
    let lower = as_created(path).to_lowercase();
    BANNED_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_rules_are_the_game_contracts() {
        use agora_game_api::{classify_entry_name, EntryName};
        // A pack's entry names are held to the one rule, so this file has no copy of it: what it
        // used to strip silently is now a refusal that names the entry, or a name a disk cannot
        // hold that the caller is told about.
        assert!(matches!(
            classify_entry_name("../../evil.exe"),
            EntryName::Escapes { .. }
        ));
        assert!(matches!(
            classify_entry_name("config/../../evil.exe"),
            EntryName::Escapes { .. }
        ));
        assert!(matches!(
            classify_entry_name("/etc/passwd"),
            EntryName::Escapes { .. }
        ));
        assert!(matches!(
            classify_entry_name("C:/windows/system32/evil.dll"),
            EntryName::Escapes { .. }
        ));
        // Backslashes read as `/`.
        match classify_entry_name("config\\mod\\settings.toml") {
            EntryName::Writable(path) => assert_eq!(path.as_str(), "config/mod/settings.toml"),
            other => panic!("unexpected {other:?}"),
        }
        // A `.` component names the folder it is in, so `./config/./mod.toml` is `config/mod.toml`
        // and not a different file. A trailing dot *is* a name a disk cannot hold, so it is reported
        // rather than quietly rewritten.
        match classify_entry_name("./config/./mod.toml") {
            EntryName::Writable(path) => assert_eq!(path.as_str(), "config/mod.toml"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            classify_entry_name("config/mod.toml."),
            EntryName::Unwritable { .. }
        ));
    }

    #[test]
    fn test_whitelist_allows_config() {
        assert!(is_whitelisted("config/mod.toml"));
        assert!(is_whitelisted("defaultconfigs/server.toml"));
        assert!(is_whitelisted("resourcepacks/mypack.zip"));
        assert!(is_whitelisted("kubejs/server_scripts/script.js"));
    }

    #[test]
    fn test_whitelist_rejects_mods() {
        assert!(!is_whitelisted("mods/evil.jar"));
        assert!(!is_whitelisted("saves/world/level.dat"));
        assert!(!is_whitelisted("README.txt"));
    }

    #[test]
    fn test_banned_extensions() {
        assert!(has_banned_extension("config/setup.exe"));
        assert!(has_banned_extension("config/lib.dll"));
        assert!(has_banned_extension("kubejs/evil.sh"));
        assert!(!has_banned_extension("config/mod.toml"));
        assert!(!has_banned_extension("kubejs/script.js"));
    }

    #[test]
    fn test_whitelist_allows_shaderpacks() {
        assert!(is_whitelisted("shaderpacks/ComplementaryShaders.zip"));
        assert!(is_whitelisted("datapacks/custom_loot.zip"));
    }

    #[test]
    fn permissive_overrides_take_jars_and_any_folder_but_never_executables() {
        let permissive = OverridePolicy::Permissive;
        assert!(permissive.admits("mods/extra.jar"));
        assert!(!permissive.forbids("mods/extra.jar"));
        assert!(permissive.admits("options.txt"));
        assert!(permissive.admits("saves/Tutorial/level.dat"));
        assert!(permissive.forbids("bin/run.sh"));
        assert!(permissive.forbids("natives/lib.dll"));
        assert!(!permissive.admits("instance_manifest.json"));
        assert!(!permissive.admits(".agora/state.json"));
        // Windows would create these without the trailing dot or space.
        assert!(permissive.forbids("config/run.bat."));
        assert!(permissive.forbids("config/run.bat. ."));
        assert!(!permissive.admits("instance_manifest.json."));
        assert!(OverridePolicy::Standard.forbids("config/x.jar."));

        let standard = OverridePolicy::Standard;
        assert!(!standard.admits("mods/extra.jar"));
        assert!(standard.forbids("config/extra.jar"));
        assert!(standard.admits("scripts/recipes.zs"));
        assert!(standard.admits("global_packs/required_data/pack.zip"));
    }

    #[test]
    fn a_bad_entry_leaves_an_existing_file_intact() {
        // A pack replacing `config/settings.toml` and then failing on a later entry may not have
        // replaced anything: the player's file is the one thing here that cannot be rebuilt.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in [
                ("config/settings.toml", &b"from the pack"[..]),
                ("config/other.toml", &b"BROKEN-PAYLOAD"[..]),
            ] {
                zip.start_file(name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        // Corrupt the second entry's bytes, so its CRC fails as the extraction reads it.
        let mut bytes = std::fs::read(&zip_path).unwrap();
        let at = bytes
            .windows(b"BROKEN-PAYLOAD".len())
            .position(|window| window == b"BROKEN-PAYLOAD")
            .expect("the payload is in the archive");
        bytes[at] ^= 0xff;
        std::fs::write(&zip_path, &bytes).unwrap();

        let dest = dir.path().join("instance");
        std::fs::create_dir_all(dest.join("config")).unwrap();
        std::fs::write(dest.join("config/settings.toml"), b"mine").unwrap();

        let error = extract_overrides(&zip_path, &dest).unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("crc")
                || error.to_string().contains("damaged"),
            "the archive is named as damaged: {error}"
        );
        assert_eq!(
            std::fs::read(dest.join("config/settings.toml")).unwrap(),
            b"mine",
            "the player's file is untouched"
        );
        assert!(
            !dest.join("config/other.toml").exists(),
            "nothing from the failed run is in the destination"
        );
        assert_eq!(
            std::fs::read_dir(dest.join("config")).unwrap().count(),
            1,
            "no partial file beside the original"
        );
        assert_eq!(
            std::fs::read_dir(&dest).unwrap().count(),
            1,
            "no staging folder left behind"
        );
    }

    fn write_zip_at(path: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn a_pack_entry_named_after_a_windows_device_is_refused_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        write_zip_at(
            &zip_path,
            &[("config/settings.toml", b"a"), ("config/aux.dds", b"b")],
        );
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();

        let error = extract_overrides(&zip_path, &dest).unwrap_err();
        assert!(
            error.to_string().contains("config/aux.dds"),
            "the refusal names the entry: {error}"
        );
        assert!(
            error.to_string().contains("device name"),
            "the refusal says why: {error}"
        );
        assert!(
            !dest.join("config/settings.toml").exists(),
            "a refusal writes nothing at all"
        );

        // The caller's explicit answer: write what can be written, and say what was left out.
        let result = extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Standard,
            NamesPolicy::WithoutThem,
        )
        .unwrap();
        assert_eq!(result.extracted, vec!["config/settings.toml"]);
        assert_eq!(result.skipped, vec!["config/aux.dds"]);
        assert_eq!(result.left_out.len(), 1);
        assert_eq!(result.left_out[0].path, "config/aux.dds");
        assert!(result.left_out[0].reason.contains("device name"));
    }

    #[test]
    fn a_case_collision_in_a_pack_is_refused_and_then_left_out_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        write_zip_at(
            &zip_path,
            &[
                ("config/Mod.toml", b"one"),
                ("config/mod.toml", b"two"),
                ("config/other.toml", b"three"),
            ],
        );
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();

        let error = extract_overrides(&zip_path, &dest).unwrap_err();
        assert!(error.to_string().contains("mod.toml"), "{error}");

        let result = extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Standard,
            NamesPolicy::WithoutThem,
        )
        .unwrap();
        assert_eq!(result.extracted.len(), 2);
        assert_eq!(result.left_out.len(), 1);
        assert_eq!(result.left_out[0].path, "config/mod.toml");
        assert!(result.left_out[0].reason.contains("case-insensitively"));
        assert_eq!(
            std::fs::read(dest.join("config/Mod.toml")).unwrap(),
            b"one",
            "the first spelling is the one kept"
        );
    }

    #[test]
    fn a_traversal_entry_still_refuses_the_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        {
            use std::io::Write;
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            zip.start_file("../evil.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();

        // An escape is never skippable, even for a caller that would leave entries out.
        let error = extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Standard,
            NamesPolicy::WithoutThem,
        )
        .unwrap_err();
        assert!(
            matches!(&error, LauncherError::Generic { code, .. } if code == "ERR_ZIP_SLIP"),
            "{error:?}"
        );
        assert!(!dir.path().join("evil.txt").exists());
    }

    #[test]
    fn a_damaged_zip_is_an_error_and_not_an_empty_import() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("pack.zip");
        std::fs::write(&zip_path, b"PK\x03\x04 certainly not a whole zip").unwrap();
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();

        let error = extract_overrides(&zip_path, &dest).unwrap_err();
        assert!(
            matches!(&error, LauncherError::Generic { code, .. } if code == "ERR_OVERRIDE_FAILED"),
            "{error:?}"
        );
    }

    #[test]
    fn permissive_extraction_writes_mods_and_still_refuses_executables() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("o.zip");
        let write_zip = |entries: &[&str]| {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            for name in entries {
                zip.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(b"x").unwrap();
            }
            zip.finish().unwrap();
        };
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();

        write_zip(&["mods/a.jar", "options.txt", "instance_manifest.json"]);
        let standard = extract_overrides(&zip_path, &dest).unwrap();
        assert!(standard.extracted.is_empty());
        let permissive =
            extract_overrides_with(&zip_path, &dest, OverridePolicy::Permissive).unwrap();
        assert_eq!(permissive.extracted, vec!["mods/a.jar", "options.txt"]);
        assert_eq!(permissive.skipped, vec!["instance_manifest.json"]);

        write_zip(&["tools/setup.exe"]);
        let error =
            extract_overrides_with(&zip_path, &dest, OverridePolicy::Permissive).unwrap_err();
        assert!(error.to_string().contains("setup.exe"), "{error}");
    }

    #[test]
    fn test_whitelist_rejects_shaderpacks_jar() {
        // .jar in shaderpacks is still banned — it should go through mods/
        assert!(has_banned_extension("shaderpacks/evil.jar"));
        // .zip is fine
        assert!(!has_banned_extension(
            "shaderpacks/ComplementaryShaders.zip"
        ));
    }

    /// A pack whose entry names are the ones the policy used to judge raw: the same file spelled
    /// with a `.` component or with backslashes, the files Agora owns spelled that way, and a name
    /// Windows would create without its trailing dot.
    fn policy_fixture_zip(dir: &Path) -> std::path::PathBuf {
        use std::io::Write;
        let zip_path = dir.join("pack.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            ("./config/dotted.toml", &b"a"[..]),
            ("config\\backslashed.toml", &b"b"[..]),
            ("./instance_manifest.json", &b"c"[..]),
            ("sub\\.agora\\state.json", &b"d"[..]),
            ("config/run.bat.", &b"e"[..]),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn a_dotted_or_backslashed_config_name_is_the_config_file_it_names() {
        // `./config/x` and `config\x` name the same file as `config/x`, so the folder rule has to
        // admit them: judged on the raw entry name they were dropped as "not a folder a pack's
        // overrides may write", which silently lost a pack's real config files.
        let dir = tempfile::tempdir().unwrap();
        let zip_path = policy_fixture_zip(dir.path());
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Standard,
            NamesPolicy::WithoutThem,
        )
        .expect("what can be written is written");
        assert!(
            dest.join("config/dotted.toml").exists(),
            "`./config/x` is `config/x`: {result:?}"
        );
        assert!(
            dest.join("config/backslashed.toml").exists(),
            "a backslash is a separator: {result:?}"
        );
    }

    #[test]
    fn a_file_agora_owns_is_protected_however_its_name_is_spelled() {
        // Reduced security mode admits every folder, so the protected-file rule is the only thing
        // between a pack and `instance_manifest.json`. Judged on the raw entry name, `./` in front
        // of it was enough to have it written over.
        let dir = tempfile::tempdir().unwrap();
        let zip_path = policy_fixture_zip(dir.path());
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();
        extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Permissive,
            NamesPolicy::WithoutThem,
        )
        .expect("what can be written is written");
        assert!(
            !dest.join("instance_manifest.json").exists(),
            "`./instance_manifest.json` is the manifest Agora owns"
        );
        assert!(
            !dest.join("sub").exists(),
            r"`sub\.agora\x` is inside Agora's own folder"
        );
    }

    #[test]
    fn a_name_a_disk_cannot_hold_is_reported_as_a_name_not_as_a_banned_type() {
        // `config/run.bat.` is written by Windows as `config/run.bat`, so it is a name a disk
        // cannot hold. Asked about the raw name, the pack's banned-extension rule refused the whole
        // archive as a security violation, which is not what the archive is.
        let dir = tempfile::tempdir().unwrap();
        let zip_path = policy_fixture_zip(dir.path());
        let dest = dir.path().join("instance");
        std::fs::create_dir_all(&dest).unwrap();
        let result = extract_overrides_with_inputs(
            &zip_path,
            &dest,
            OverridePolicy::Standard,
            NamesPolicy::WithoutThem,
        )
        .expect("what can be written is written");
        let left_out = result
            .left_out
            .iter()
            .find(|entry| entry.path == "config/run.bat.")
            .unwrap_or_else(|| panic!("the entry is named: {result:?}"));
        assert!(
            left_out.reason.contains("dot"),
            "the reason is about the name: {left_out:?}"
        );

        // And when the caller did not opt out, that same name is what refuses the extraction.
        let refused = dir.path().join("refused");
        std::fs::create_dir_all(&refused).unwrap();
        let error = extract_overrides_with_inputs(
            &zip_path,
            &refused,
            OverridePolicy::Standard,
            NamesPolicy::Refuse,
        )
        .expect_err("a name Windows cannot hold is refused by default");
        assert_eq!(
            error.code(),
            "ERR_ZIP_UNSAFE_NAMES",
            "not a security violation: {error}"
        );
    }
}
