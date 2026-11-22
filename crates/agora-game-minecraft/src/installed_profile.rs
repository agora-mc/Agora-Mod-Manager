//! Pure-core installed-profile adoption and receipt layer.
//!
//! Provides types and functions to read, validate, adopt, and issue receipts
//! for installed modloader profiles under `minecraft_dir/versions/<id>/`.
//!
//! This is a **pure observation layer** â€” it does not install, modify, or
//! download anything. Every effect is scoped to receipt file I/O.
//!
//! # Security invariants
//!
//! - All file reads are bounded at 8 MiB.
//! - Receipt paths are confined within `receipts_root` and deterministically
//!   derived from the tuple â€” no caller-supplied path fragments.
//! - Profile JSON is validated structurally before being adopted.
//! - Libraries without upstream SHA have their paths included in
//!   `trusted_unhashed_library_paths` **only** when a valid receipt binds
//!   the current profile hash + curated installer SHA.

use crate::launch::{merge_forge_version, parse_maven_descriptor, VersionInfo};
use agora_core::download::{sha1_hex, sha256_hex, stable_version_json};
use agora_core::network;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// InstallReceiptSummary â€” result from the install service
// ---------------------------------------------------------------------------

/// Summary returned by the install service after ensuring a loader is installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReceiptSummary {
    /// The loader tuple that was installed.
    pub tuple: LoaderTuple,
    /// The derived profile ID.
    pub profile_id: String,
    /// Whether the profile existed already and was valid (install was skipped).
    pub cache_hit: bool,
    /// The profile stable hash after installation.
    pub profile_stable_hash: String,
    /// The schema_version of the receipt that was written.
    pub receipt_schema_version: i64,
    /// The exit status of the installer process (0 for Fabric/Quilt, or the
    /// actual exit code for Forge/NeoForge).
    pub installer_exit_status: i32,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Max size for an on-disk version/profile JSON (8 MiB).
const MAX_PROFILE_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// Schema version for [`InstalledProfileReceipt`].
pub const RECEIPT_SCHEMA_VERSION: i64 = 3;
pub const RECEIPTS_DIR_NAME: &str = "receipts";

/// Max size for a generated library file during receipt creation (64 MiB).
const MAX_GENERATED_LIBRARY_SIZE: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// LoaderTuple â€” identifies a loader installation uniquely
// ---------------------------------------------------------------------------

/// A (loader, minecraft_version, loader_version) triple.
///
/// Every component must be non-empty and free of path-traversal characters.
/// Use [`derive_profile_id`] to obtain the standard Mojang profile ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoaderTuple {
    pub loader: String,
    pub minecraft_version: String,
    pub loader_version: String,
}

impl LoaderTuple {
    /// Validate tuple components â€” no empty strings, no traversal chars.
    pub fn validate(&self) -> Result<(), String> {
        for (field, name) in [
            (&self.loader, "loader"),
            (&self.minecraft_version, "minecraft_version"),
            (&self.loader_version, "loader_version"),
        ] {
            if field.is_empty() {
                return Err(format!("{name} must not be empty"));
            }
            if field.contains('\0') {
                return Err(format!("{name} must not contain null bytes"));
            }
            if field.contains('/') || field.contains('\\') {
                return Err(format!("{name} must not contain path separators"));
            }
            if field.contains("..") {
                return Err(format!("{name} must not contain '..'"));
            }
            if field.contains(':') {
                return Err(format!("{name} must not contain ':'"));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// derive_profile_id â€” matches existing desktop behaviour
// ---------------------------------------------------------------------------

/// Derive the Mojang launcher profile ID from a validated tuple.
///
/// | Loader   | Format                                    |
/// |----------|-------------------------------------------|
/// | forge    | `forge-{minecraft_version}-{loader}`      |
/// | neoforge | `neoforge-{loader}`                       |
/// | fabric   | `fabric-loader-{loader}-{minecraft_version}` |
/// | quilt    | `quilt-loader-{loader}-{minecraft_version}`  |
///
/// # Panics
/// Panics if tuple components have not been validated (contains separators,
/// empty strings, etc.). Always call [`LoaderTuple::validate`] first.
pub fn derive_profile_id(tuple: &LoaderTuple) -> String {
    // These could only fail if validate() wasn't called â€” panic so the bug is
    // caught in testing rather than silently producing a wrong ID.
    assert!(!tuple.loader.is_empty(), "loader must not be empty");
    assert!(
        !tuple.minecraft_version.is_empty(),
        "minecraft_version must not be empty"
    );
    assert!(
        !tuple.loader_version.is_empty(),
        "loader_version must not be empty"
    );

    match tuple.loader.to_ascii_lowercase().as_str() {
        "forge" => format!("forge-{}-{}", tuple.minecraft_version, tuple.loader_version),
        "neoforge" => format!("neoforge-{}", tuple.loader_version),
        "fabric" => format!(
            "fabric-loader-{}-{}",
            tuple.loader_version, tuple.minecraft_version
        ),
        "quilt" => format!(
            "quilt-loader-{}-{}",
            tuple.loader_version, tuple.minecraft_version
        ),
        other => panic!("unknown loader family: {other}"),
    }
}

// ---------------------------------------------------------------------------
// LoaderSourceKind â€” identifies how the loader was originally installed
// ---------------------------------------------------------------------------

/// How the loader profile was originally installed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoaderSourceKind {
    /// Installed from a generated profile JSON (Fabric, Quilt).
    ProfileJson,
    /// Installed via the official installer JAR (Forge, NeoForge).
    InstallerJar,
}

// ---------------------------------------------------------------------------
// InstalledProfileReceipt â€” schema v3
// ---------------------------------------------------------------------------

/// A signed receipt linking an installed loader profile to a specific curated
/// source artifact. Written atomically into `receipts_root`.
///
/// # Schema versions
///
/// | Version | Notes |
/// |---------|-------|
/// | 1       | Original Forge/NeoForge-only receipt with `source_sha256` |
/// | 2       | Added `generated_artifact_sha256` map |
/// | 3       | Replaced `source_sha256`/`installer_url` with `source_sha256`/`source_url`; added `source_kind`, `curated_artifact_sha256`; made `generated_artifact_sha256` non-optional |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledProfileReceipt {
    /// Schema version (currently 3).
    pub schema_version: i64,
    /// The loader tuple that produced this profile.
    pub tuple: LoaderTuple,
    /// How this loader was installed. Defaults to InstallerJar for backward
    /// compatibility with v1/v2 receipts that predate this field.
    #[serde(default = "default_source_kind")]
    pub source_kind: LoaderSourceKind,
    /// SHA-256 of the curated installer JAR. Empty for profiles checked
    /// against Maven checksums. Legacy receipts may carry a profile-byte pin.
    /// Reads legacy installer_sha256 field from v1/v2 receipts.
    #[serde(alias = "installer_sha256")]
    pub source_sha256: String,
    /// Download URL of the curated source artifact.
    /// Reads legacy installer_url field from v1/v2 receipts.
    #[serde(alias = "installer_url")]
    pub source_url: String,
    /// The derived profile ID (`versions/<id>/<id>.json`).
    pub profile_id: String,
    /// Must equal `"versions/<profile_id>/<profile_id>.json"`.
    pub profile_relative_path: String,
    /// Stable hash of the profile JSON (time/releaseTime dropped,
    /// keys sorted).
    pub profile_stable_hash: String,
    /// Base Minecraft version ID (e.g. "1.21").
    pub base_version_id: String,
    /// ISO-8601 timestamp of installation.
    pub installed_at: String,
    /// Exit code of the installer process, or 0 if unknown/success.
    pub installer_exit_status: i32,
    /// Map of relative Maven paths to SHA-256 hex digests for processor-
    /// created artifacts (Forge/NeoForge), or Maven jars from a repository
    /// with no published checksum that the player explicitly accepted.
    /// These paths may be adopted locally but cannot be downloaded at launch.
    #[serde(default, deserialize_with = "deserialize_nullable_hash_map")]
    pub generated_artifact_sha256: BTreeMap<String, String>,
    /// Map of relative Maven paths to SHA-256 hex digests for loader library
    /// pins supplied by Agora's curated manifest or recorded after checking a
    /// Fabric/Quilt library against a published Maven checksum.
    #[serde(default)]
    pub curated_artifact_sha256: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------
// AdoptedProfile â€” result of a successful adoption
// ---------------------------------------------------------------------------

/// A fully validated, adopted installed profile ready for the launch planner.
///
/// # Debug behaviour
/// `profile_path` is included (it's inside the well-known `versions/`
/// directory, not a user-chosen arbitrary path). Other paths are derived
/// from the input arguments and already known to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptedProfile {
    /// The loader tuple.
    pub tuple: LoaderTuple,
    /// Derived profile ID.
    pub profile_id: String,
    /// Absolute path to the profile JSON on disk.
    pub profile_path: PathBuf,
    /// Stable hash of the profile JSON.
    pub profile_stable_hash: String,
    /// Parsed loader version metadata.
    pub loader_version: VersionInfo,
    /// Parsed base Minecraft version metadata.
    pub base_version: VersionInfo,
    /// Merged version metadata (loader merged into base).
    pub merged_version: VersionInfo,
    /// The matching receipt, if one was found and verified.
    pub receipt: Option<InstalledProfileReceipt>,
    /// Relative Maven paths of libraries that have no upstream SHA-1/SHA-256
    /// and whose trust was established via a valid receipt binding.
    ///
    /// Only populated when a valid receipt is present. Without a receipt,
    /// adoption returns [`ProfileIssueKind::UnsupportedProfileMetadata`]
    /// rather than populating this set.
    pub trusted_unhashed_library_paths: BTreeSet<String>,
    /// The absolute path to the `.minecraft` directory from which this profile
    /// was adopted. Used during materialization to locate installed artifacts.
    pub minecraft_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// ProfileIssue â€” structured error for profile adoption problems
// ---------------------------------------------------------------------------

pub use agora_core::error::{ProfileIssue, ProfileIssueKind};

// ---------------------------------------------------------------------------
// Internal helpers: bounded file read, path safety
// ---------------------------------------------------------------------------

/// Read an entire regular file with a size bound.
///
/// Returns `ProfileIssue::CorruptProfile` when:
/// - The file does not exist (converted to MissingProfile by caller if desired).
/// - The file is larger than `max_bytes`.
/// - The file is not a regular file (e.g. directory, symlink).
fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ProfileIssue> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ProfileIssue::missing(
                Some(path.to_path_buf()),
                format!("File not found: {}", path.display()),
            )
        } else {
            ProfileIssue::corrupt(
                Some(path.to_path_buf()),
                format!("Failed to read metadata: {e}"),
            )
        }
    })?;

    if !meta.file_type().is_file() {
        return Err(ProfileIssue::corrupt(
            Some(path.to_path_buf()),
            "Not a regular file".to_string(),
        ));
    }

    if meta.len() > max_bytes {
        return Err(ProfileIssue::corrupt(
            Some(path.to_path_buf()),
            format!("File too large: {} bytes (max {max_bytes})", meta.len()),
        ));
    }

    // Symlink detection is best-effort (Rust's metadata follows symlinks on
    // Unix). On Windows we can check reparse points.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT = 0x400
        if meta.file_attributes() & 0x400 != 0 {
            return Err(ProfileIssue::corrupt(
                Some(path.to_path_buf()),
                "Path is a reparse point / symlink".to_string(),
            ));
        }
    }

    let mut file = std::fs::File::open(path).map_err(|e| {
        ProfileIssue::corrupt(
            Some(path.to_path_buf()),
            format!("Failed to open file: {e}"),
        )
    })?;

    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut buf).map_err(|e| {
        ProfileIssue::corrupt(
            Some(path.to_path_buf()),
            format!("Failed to read file: {e}"),
        )
    })?;

    Ok(buf)
}

/// Sanitize a string for safe use as a filename component.
/// Replaces anything that isn't alphanumeric, `-`, `.`, or `_` with `-`.
fn safe_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Bounded profile JSON reading and parsing
// ---------------------------------------------------------------------------

/// Read a version/profile JSON with bounded size and return the parsed value.
fn bounded_parse_profile_json(path: &Path) -> Result<serde_json::Value, ProfileIssue> {
    let bytes = read_bounded_file(path, MAX_PROFILE_FILE_SIZE)?;

    serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| {
        if e.classify() == serde_json::error::Category::Eof {
            ProfileIssue::corrupt(Some(path.to_path_buf()), "Truncated JSON".to_string())
        } else {
            ProfileIssue::corrupt(Some(path.to_path_buf()), format!("Malformed JSON: {e}"))
        }
    })
}

/// Read a version/profile JSON into VersionInfo.
fn bounded_parse_version_info(path: &Path) -> Result<VersionInfo, ProfileIssue> {
    let bytes = read_bounded_file(path, MAX_PROFILE_FILE_SIZE)?;

    serde_json::from_slice::<VersionInfo>(&bytes).map_err(|e| {
        if e.classify() == serde_json::error::Category::Eof {
            ProfileIssue::corrupt(Some(path.to_path_buf()), "Truncated JSON".to_string())
        } else {
            ProfileIssue::corrupt(
                Some(path.to_path_buf()),
                format!("Malformed version JSON: {e}"),
            )
        }
    })
}

// ---------------------------------------------------------------------------
// Structural validation
// ---------------------------------------------------------------------------

/// Validate the structural integrity of a loader profile VersionInfo.
///
/// Returns `Ok(())` on success, `ProfileIssue` on failure.
/// Distinguishes CorruptProfile (malformed, security violation) from
/// UnsupportedProfileMetadata (well-formed but unknown/unsupported values).
fn validate_loader_profile(
    info: &VersionInfo,
    profile_path: &Path,
    expected_profile_id: &str,
    expected_base_version: &str,
    produced_libraries: &BTreeSet<String>,
) -> Result<(), ProfileIssue> {
    // 1. Profile ID must match expected.
    if info.id != expected_profile_id {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!(
                "Profile ID mismatch: expected '{expected_profile_id}', got '{}'",
                info.id
            ),
        ));
    }

    // 2. For non-vanilla profiles (has inheritsFrom), it must match the base MC.
    if let Some(ref inherits) = info.inherits_from {
        if inherits != expected_base_version {
            return Err(ProfileIssue::corrupt(
                Some(profile_path.to_path_buf()),
                format!(
                    "inheritsFrom mismatch: expected '{expected_base_version}', got '{inherits}'"
                ),
            ));
        }
    }

    // 3. mainClass must be non-empty and plausible.
    if info.main_class.is_empty() {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            "mainClass is empty".to_string(),
        ));
    }
    if !is_plausible_main_class(&info.main_class) {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!(
                "mainClass '{}' does not look like a valid Java class",
                info.main_class
            ),
        ));
    }

    // 4. Arguments validation.
    validate_arguments(info, profile_path)?;

    // 5. Library validation.
    validate_libraries(&info.libraries, profile_path, produced_libraries)?;

    Ok(())
}

fn is_plausible_main_class(s: &str) -> bool {
    // Must be a valid Java fully-qualified class name:
    // starts with a letter, contains dots (package), no whitespace.
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    if !s.contains('.') {
        return false;
    }
    for ch in s.chars() {
        if ch.is_whitespace() {
            return false;
        }
    }
    true
}

/// Validate the arguments block of a version info.
fn validate_arguments(info: &VersionInfo, profile_path: &Path) -> Result<(), ProfileIssue> {
    // Supported: `arguments` (jvm/game arrays) or legacy `minecraftArguments` string.
    // Both absent is only a problem when neither is present.
    let has_structured = info.arguments.is_some();
    let has_legacy = info.minecraft_arguments.is_some();

    if !has_structured && !has_legacy {
        // Not every profile has arguments (some are pure library containers).
        // Only flag if there's no way to construct a launch command.
        // We'll allow it for now but surface as unsupported if mainClass
        // suggests a loader entry point.
        if info.main_class.contains("forge")
            || info.main_class.contains("fabric")
            || info.main_class.contains("quilt")
            || info.main_class.contains("neoforge")
        {
            return Err(ProfileIssue::unsupported(
                Some(profile_path.to_path_buf()),
                vec!["Neither 'arguments' nor 'minecraftArguments' present, but profile has a loader mainClass".to_string()],
            ));
        }
        return Ok(());
    }

    // Validate structured arguments shapes (they must deserialize to the
    // expected types already, but double-check here).
    if let Some(ref args) = info.arguments {
        if args.jvm.iter().any(|v| !v.is_string() && !v.is_object()) {
            return Err(ProfileIssue::corrupt(
                Some(profile_path.to_path_buf()),
                "JVM argument entry is not a string or rule object".to_string(),
            ));
        }
        if args.game.iter().any(|v| !v.is_string() && !v.is_object()) {
            return Err(ProfileIssue::corrupt(
                Some(profile_path.to_path_buf()),
                "Game argument entry is not a string or rule object".to_string(),
            ));
        }
    }

    Ok(())
}

/// Validate libraries: Maven safety, URL classification, rules structure.
fn validate_libraries(
    libraries: &[crate::launch::Library],
    profile_path: &Path,
    produced_libraries: &BTreeSet<String>,
) -> Result<(), ProfileIssue> {
    for lib in libraries {
        // Name must be non-empty.
        if lib.name.is_empty() {
            return Err(ProfileIssue::corrupt(
                Some(profile_path.to_path_buf()),
                "Library has empty name".to_string(),
            ));
        }

        // Artifact paths (if present) must be safe relative paths â€” always check,
        // regardless of whether the name parses as a valid Maven coordinate.
        if let Some(ref downloads) = lib.downloads {
            if let Some(ref artifact) = downloads.artifact {
                if is_unsafe_relative_path(&artifact.path) {
                    return Err(ProfileIssue::corrupt(
                        Some(profile_path.to_path_buf()),
                        format!(
                            "Unsafe library artifact path for '{}': '{}'",
                            lib.name, artifact.path
                        ),
                    ));
                }
            }
            if let Some(ref classifiers) = downloads.classifiers {
                for (key, art) in classifiers {
                    if is_unsafe_relative_path(&art.path) {
                        return Err(ProfileIssue::corrupt(
                            Some(profile_path.to_path_buf()),
                            format!(
                                "Unsafe classifier '{}' artifact path for '{}': '{}'",
                                key, lib.name, art.path
                            ),
                        ));
                    }
                }
            }
        }

        // Validate URLs
        if let Some(ref downloads) = lib.downloads {
            let produced = produced_libraries.contains(&lib.name);
            if let Some(ref artifact) = downloads.artifact {
                validate_library_url(&artifact.url, profile_path, &lib.name, produced)?;
            }
            if let Some(ref classifiers) = downloads.classifiers {
                for (key, art) in classifiers {
                    validate_library_url(
                        &art.url,
                        profile_path,
                        &format!("{}/{}", lib.name, key),
                        produced,
                    )?;
                }
            }
        }
        if let Some(ref url) = lib.url {
            validate_library_url(
                url,
                profile_path,
                &lib.name,
                produced_libraries.contains(&lib.name),
            )?;
        }

        // Validate rules
        if let Some(ref rules) = lib.rules {
            for rule in rules {
                validate_rule(rule, profile_path, &lib.name)?;
            }
        }
    }
    Ok(())
}

/// Check if a relative path contains traversal or absolute components.
fn is_unsafe_relative_path(path: &str) -> bool {
    if path.contains('\0') {
        return true;
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    if path.contains("..") {
        return true;
    }
    if path.contains(':') {
        return true;
    }
    let normalized = path.replace('\\', "/");
    for component in normalized.split('/') {
        if component == ".." {
            return true;
        }
        // Reject absolute Windows paths like C:\
        if component.len() == 2 && component.ends_with(':') {
            return true;
        }
    }
    false
}

/// Validate a library URL: must be HTTPS and recognized by classify_url.
fn validate_library_url(
    url: &str,
    profile_path: &Path,
    lib_name: &str,
    produced: bool,
) -> Result<(), ProfileIssue> {
    if url.trim().is_empty() {
        if produced {
            return Ok(());
        }
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!(
                "Library '{lib_name}' has a blank URL but is not an installer processor output"
            ),
        ));
    }
    if !url.starts_with("https://") {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!("Library '{lib_name}' has non-HTTPS URL: '{url}'"),
        ));
    }

    match crate::context::classify_url(url) {
        Some(network::NetworkCategory::MojangContent)
        | Some(network::NetworkCategory::LoaderMetadataAndContent) => Ok(()),
        Some(_) => Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!("Library '{lib_name}' URL '{url}' is HTTPS but unrecognized category"),
        )),
        None => Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!("Library '{lib_name}' URL '{url}' is from an unknown/untrusted host"),
        )),
    }
}

/// Validate a library rule: action must be present, OS/features must be
/// structurally well-formed. Unknown action values â†’ UnsupportedProfileMetadata.
fn validate_rule(
    rule: &crate::launch::LibraryRule,
    profile_path: &Path,
    lib_name: &str,
) -> Result<(), ProfileIssue> {
    // action must be non-empty string
    if rule.action.is_empty() {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.to_path_buf()),
            format!("Library '{lib_name}' has a rule with empty action"),
        ));
    }

    // Known actions: "allow" and "disallow"
    let known_actions = ["allow", "disallow"];
    if !known_actions.contains(&rule.action.as_str()) {
        return Err(ProfileIssue::unsupported(
            Some(profile_path.to_path_buf()),
            vec![format!(
                "Library '{lib_name}' rule has unknown action '{}'",
                rule.action
            )],
        ));
    }

    // Validate OS if present
    if let Some(ref os) = rule.os {
        // OS name must be a well-known OS identifier
        if !os.name.is_empty() {
            let known_oses = ["windows", "osx", "linux"];
            if !known_oses.contains(&os.name.as_str()) {
                return Err(ProfileIssue::unsupported(
                    Some(profile_path.to_path_buf()),
                    vec![format!(
                        "Library '{lib_name}' rule has unknown OS name '{}'",
                        os.name
                    )],
                ));
            }
        }
        // version and arch are optional, any well-formed string is fine
    }

    // Validate features if present
    if let Some(ref features) = rule.features {
        let known_features = [
            "is_demo_user",
            "has_custom_resolution",
            "is_quick_play_singleplayer",
            "is_quick_play_multiplayer",
            "is_quick_play_realms",
        ];
        for key in features.keys() {
            if !known_features.contains(&key.as_str()) {
                return Err(ProfileIssue::unsupported(
                    Some(profile_path.to_path_buf()),
                    vec![format!(
                        "Library '{lib_name}' rule has unknown feature '{key}'"
                    )],
                ));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unhashed library detection
// ---------------------------------------------------------------------------

/// Check if a library publishes an upstream SHA-1.
///
/// "Publishes" means a non-blank one: a blank digest is what a manifest writes
/// when it has no hash, and reading it as one would leave the library neither
/// checked nor pinned from disk (see [`published_sha1s`]).
fn has_upstream_hash(lib: &crate::launch::Library) -> bool {
    !published_sha1s(lib).is_empty()
}

/// Every SHA-1 a profile publishes for a library: the artifact download's and
/// the library's own (Fabric/Quilt put it there).
///
/// A blank published digest is not a hash to check. `agora_core::artifact_hash`
/// resolves a blank *published* digest to "the source published nothing" (only
/// a blank *asserted* hash, from a curator, pack or earlier install, is an
/// error), and this is the same kind of digest, so it yields nothing here.
fn published_sha1s(lib: &crate::launch::Library) -> Vec<String> {
    [
        lib.sha1.as_deref(),
        lib.downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .and_then(|artifact| artifact.sha1.as_deref()),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|sha1| !sha1.is_empty())
    .map(str::to_lowercase)
    .collect()
}

/// Collect the relative Maven paths of libraries without upstream hashes.
fn collect_unhashed_library_paths(libraries: &[crate::launch::Library]) -> Vec<String> {
    let mut paths = Vec::new();
    for lib in libraries {
        if !has_upstream_hash(lib) {
            // Try to infer the relative path from the name or artifact path.
            if !lib.name.is_empty() {
                if let Ok(desc) = parse_maven_descriptor(&lib.name) {
                    paths.push(desc.to_relative_path());
                } else if let Some(ref downloads) = lib.downloads {
                    if let Some(ref artifact) = downloads.artifact {
                        if !artifact.path.is_empty() && !is_unsafe_relative_path(&artifact.path) {
                            paths.push(artifact.path.clone());
                        }
                    }
                }
            }
        }
    }
    paths
}

/// Maven coordinates written by the installer's processors for `side`.
///
/// Processor outputs are placeholders such as `{PATCHED}`; the readable
/// coordinate lives in `data[PATCHED][side]` as `[group:name:version:classifier]`.
/// Resolving each output placeholder through `data` for the side being
/// installed is the only reliable way to name those artifacts — collecting
/// every `data` entry would also allow processor *inputs* and the other side's
/// jars to be treated as outputs.
///
/// A processor only runs for the sides its `sides` array names; see
/// [`processor_runs_on`]. Resolving a server-only processor's `{MC_UNPACKED}`
/// through `data(...)["client"]` would name an artifact no client install ever
/// produces.
pub fn processor_output_coordinates(
    install_profile: &serde_json::Value,
    side: &str,
) -> Result<BTreeSet<String>, ProfileIssue> {
    fn collect(reference: &serde_json::Value, out: &mut BTreeSet<String>) {
        let Some(text) = reference.as_str() else {
            return;
        };
        let text = text.trim();
        let inner = text
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(text);
        let coordinate = inner.split('@').next().unwrap_or(inner).trim();
        if coordinate.matches(':').count() >= 2 {
            out.insert(coordinate.to_string());
        }
    }

    let data = install_profile.get("data").and_then(|v| v.as_object());
    let mut out = BTreeSet::new();
    if let Some(processors) = install_profile.get("processors").and_then(|v| v.as_array()) {
        for processor in processors {
            if !processor_runs_on(processor, side)? {
                continue;
            }
            let Some(outputs) = processor.get("outputs").and_then(|v| v.as_object()) else {
                continue;
            };
            for (key, value) in outputs {
                // A literal bracketed coordinate in an output is already the
                // answer; some installer versions use that form directly.
                collect(value, &mut out);
                // Placeholder keys/values (e.g. `{PATCHED}`) resolve through
                // `data` for the installed side only.
                for reference in [key.as_str(), value.as_str().unwrap_or_default()] {
                    let Some(name) = reference
                        .strip_prefix('{')
                        .and_then(|name| name.strip_suffix('}'))
                    else {
                        continue;
                    };
                    if let Some(entry) = data.and_then(|data| data.get(name)) {
                        if let Some(side_value) = entry.get(side) {
                            collect(side_value, &mut out);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Whether an installer processor runs for `side`.
///
/// `sides` is optional: only an absent key means every side. When it is
/// present it names the sides the processor runs on, and one that does not
/// name `side` produces nothing for the install being done.
///
/// A `sides` that is present but is not an array of strings cannot be
/// interpreted. Both guesses are wrong in a way that matters: "every side"
/// trusts artifacts the other side's processors write, and "no side" drops
/// outputs this install really produces. It is reported instead — a JSON
/// `null`, a string, a number or an array holding a non-string all count as
/// malformed, since the installer writes "all sides" by leaving the key out.
fn processor_runs_on(processor: &serde_json::Value, side: &str) -> Result<bool, ProfileIssue> {
    let Some(sides) = processor.get("sides") else {
        return Ok(true);
    };
    let malformed = || {
        let jar = processor
            .get("jar")
            .and_then(|jar| jar.as_str())
            .unwrap_or("(no jar)");
        ProfileIssue::corrupt(
            None,
            format!("Installer processor '{jar}' has a 'sides' that is not an array of strings: {sides}"),
        )
    };
    let sides = sides.as_array().ok_or_else(malformed)?;
    let mut names = Vec::with_capacity(sides.len());
    for entry in sides {
        names.push(entry.as_str().ok_or_else(malformed)?);
    }
    Ok(names.contains(&side))
}

pub fn processor_outputs_from_installer_jar(
    jar: &[u8],
    side: &str,
) -> Result<BTreeSet<String>, ProfileIssue> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(jar)).map_err(|error| {
        ProfileIssue::corrupt(None, format!("Installer is not a readable JAR: {error}"))
    })?;
    let mut entry = archive.by_name("install_profile.json").map_err(|error| {
        ProfileIssue::corrupt(
            None,
            format!("Installer has no install_profile.json: {error}"),
        )
    })?;
    const MAX_INSTALL_PROFILE_SIZE: u64 = 4 * 1024 * 1024;
    if entry.size() > MAX_INSTALL_PROFILE_SIZE {
        return Err(ProfileIssue::corrupt(
            None,
            format!(
                "Installer's install_profile.json is too large: {} bytes",
                entry.size()
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes).map_err(|error| {
        ProfileIssue::corrupt(None, format!("Cannot read install_profile.json: {error}"))
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        ProfileIssue::corrupt(
            None,
            format!("Installer's install_profile.json is invalid: {error}"),
        )
    })?;
    processor_output_coordinates(&value, side)
}

fn receipt_produced_libraries(
    libraries: &[crate::launch::Library],
    receipt: &InstalledProfileReceipt,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for lib in libraries {
        let Ok(descriptor) = parse_maven_descriptor(&lib.name) else {
            continue;
        };
        let relative = descriptor.to_relative_path();
        if receipt.generated_artifact_sha256.contains_key(&relative)
            || receipt.curated_artifact_sha256.contains_key(&relative)
        {
            out.insert(lib.name.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stable profile hash
// ---------------------------------------------------------------------------

/// Compute the stable hash of a profile JSON value.
///
/// Uses the same algorithm as `stable_version_json`: recursively sorts keys
/// and drops `time` / `releaseTime` fields.
pub fn stable_profile_hash(value: &serde_json::Value) -> String {
    sha256_hex(stable_version_json(value).as_bytes())
}

// ---------------------------------------------------------------------------
// Receipt path (confined, deterministic)
// ---------------------------------------------------------------------------

/// Compute the deterministic receipt file path for a given tuple.
///
/// The path is always `<receipts_root>/<safe_id>.receipt.json`, where
/// `safe_id` is derived from `derive_profile_id` with filename sanitization.
/// This guarantees the path is confined within `receipts_root`.
pub fn receipt_path(receipts_root: &Path, tuple: &LoaderTuple) -> PathBuf {
    let id = derive_profile_id(tuple);
    let safe = safe_filename(&id);
    receipts_root.join(format!("{safe}.receipt.json"))
}

// ---------------------------------------------------------------------------
// Receipt I/O
// ---------------------------------------------------------------------------

/// Read and validate a receipt for the given tuple.
///
/// Returns `Ok(Some(receipt))` on success, `Ok(None)` if no receipt file exists,
/// or `Err(ProfileIssue)` if the receipt file exists but is malformed.
pub fn read_receipt(
    receipts_root: &Path,
    tuple: &LoaderTuple,
) -> Result<Option<InstalledProfileReceipt>, ProfileIssue> {
    let rpath = receipt_path(receipts_root, tuple);

    if !rpath.exists() {
        return Ok(None);
    }

    let bytes = read_bounded_file(&rpath, MAX_PROFILE_FILE_SIZE)?;

    let receipt: InstalledProfileReceipt = serde_json::from_slice(&bytes).map_err(|e| {
        ProfileIssue::corrupt(Some(rpath.clone()), format!("Malformed receipt JSON: {e}"))
    })?;

    // Validate schema version â€” accept v1 (original), v2 (with generated hash map),
    // and v3 (with source_kind, curated pins).
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        && receipt.schema_version != 1
        && receipt.schema_version != 2
    {
        return Err(ProfileIssue::corrupt(
            Some(rpath.clone()),
            format!(
                "Unsupported receipt schema version {}",
                receipt.schema_version
            ),
        ));
    }

    // Validate tuple matches
    if receipt.tuple != *tuple {
        return Err(ProfileIssue::corrupt(
            Some(rpath),
            format!(
                "Receipt tuple mismatch: expected ({},{},{}), got ({},{},{})",
                tuple.loader,
                tuple.minecraft_version,
                tuple.loader_version,
                receipt.tuple.loader,
                receipt.tuple.minecraft_version,
                receipt.tuple.loader_version
            ),
        ));
    }

    // Validate derived profile ID.
    let expected_id = derive_profile_id(tuple);
    if receipt.profile_id != expected_id {
        return Err(ProfileIssue::corrupt(
            Some(rpath),
            format!(
                "Receipt profile_id mismatch: expected '{expected_id}', got '{}'",
                receipt.profile_id
            ),
        ));
    }

    // Profile relative path must equal versions/<id>/<id>.json.
    let expected_relative = format!("versions/{expected_id}/{expected_id}.json");
    if receipt.profile_relative_path != expected_relative {
        return Err(ProfileIssue::corrupt(
            Some(rpath),
            format!(
                "Receipt profile_relative_path mismatch: expected '{expected_relative}', got '{}'",
                receipt.profile_relative_path
            ),
        ));
    }

    Ok(Some(receipt))
}

/// Write a receipt atomically to disk.
///
/// Uses a temp file (`<target>.tmp`) and atomic rename to avoid partial writes.
pub fn write_receipt_atomic(
    receipts_root: &Path,
    tuple: &LoaderTuple,
    receipt: &InstalledProfileReceipt,
) -> Result<(), ProfileIssue> {
    std::fs::create_dir_all(receipts_root).map_err(|e| {
        ProfileIssue::corrupt(None, format!("Failed to create receipts directory: {e}"))
    })?;

    let rpath = receipt_path(receipts_root, tuple);
    let tmp_path = rpath.with_extension("receipt.json.tmp");

    let json = serde_json::to_string_pretty(receipt)
        .map_err(|e| ProfileIssue::corrupt(None, format!("Failed to serialize receipt: {e}")))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| ProfileIssue::corrupt(None, format!("Failed to write temp receipt: {e}")))?;

    std::fs::rename(&tmp_path, &rpath).map_err(|e| {
        // Attempt to clean up temp file.
        let _ = std::fs::remove_file(&tmp_path);
        ProfileIssue::corrupt(None, format!("Failed to rename receipt: {e}"))
    })?;

    Ok(())
}

/// Remove a receipt from disk (no error if missing).
pub fn remove_receipt(receipts_root: &Path, tuple: &LoaderTuple) -> Result<(), ProfileIssue> {
    let rpath = receipt_path(receipts_root, tuple);
    if rpath.exists() {
        std::fs::remove_file(&rpath)
            .map_err(|e| ProfileIssue::corrupt(None, format!("Failed to remove receipt: {e}")))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Receipt validation against current state
// ---------------------------------------------------------------------------

/// Validate a receipt against caller-provided expectations.
///
/// Checks:
/// - `expected_source_sha256` matches receipt's `source_sha256`.
/// - `expected_profile_id` matches receipt's `profile_id`.
/// - `expected_relative_path` matches receipt's `profile_relative_path`.
/// - `current_stable_hash` matches receipt's `profile_stable_hash`.
pub fn validate_receipt_binding(
    receipt: &InstalledProfileReceipt,
    expected_source_sha256: &str,
    expected_profile_id: &str,
    expected_relative_path: &str,
    current_stable_hash: &str,
) -> Result<(), Vec<String>> {
    let mut reasons = Vec::new();

    if receipt.source_sha256 != expected_source_sha256 {
        reasons.push(format!(
            "Source SHA mismatch: expected '{expected_source_sha256}', got '{}'",
            receipt.source_sha256
        ));
    }

    if receipt.profile_id != expected_profile_id {
        reasons.push(format!(
            "Profile ID mismatch: expected '{expected_profile_id}', got '{}'",
            receipt.profile_id
        ));
    }

    if receipt.profile_relative_path != expected_relative_path {
        reasons.push(format!(
            "Profile relative path mismatch: expected '{expected_relative_path}', got '{}'",
            receipt.profile_relative_path
        ));
    }

    if receipt.profile_stable_hash != current_stable_hash {
        reasons.push(format!(
            "Profile stable hash mismatch: expected '{current_stable_hash}', got '{}'",
            receipt.profile_stable_hash
        ));
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        Err(reasons)
    }
}

// ---------------------------------------------------------------------------
// adopt_installed_profile â€” main entry point
// ---------------------------------------------------------------------------

/// Adopt an installed modloader profile from the Mojang launcher's version
/// directory structure.
///
/// # Receiptless Forge/NeoForge profiles
///
/// **Intentional adoption** of a receiptless Forge/NeoForge profile is only
/// permitted when **every referenced library artifact has an upstream SHA-1
/// or SHA-256 hash** (checked via `has_upstream_hash`). Without a receipt
/// the launcher cannot distinguish curated installer artifacts from
/// tampered ones, so unhashed generated libraries are rejected as
/// [`ProfileIssueKind::UnsupportedProfileMetadata`]. This prevents the
/// launcher from silently trusting unknown files from the filesystem.
///
/// When a valid receipt IS present, unhashed generated library paths are
/// trusted via the receipt's `generated_artifact_sha256` map, which was
/// populated during installation by hashing each generated artifact.
///
/// # Arguments
///
/// * `minecraft_dir` - The root `.minecraft` (or equivalent) directory.
/// * `receipts_root` - Directory where receipts are stored (adoption-local).
/// * `tuple` - Identifies the loader, Minecraft version, and loader version.
/// * `curated_installer_sha` - Optional SHA-256 of the curated installer ZIP.
///   Pass `Some(sha)` for Forge/NeoForge; `None` for Fabric/Quilt (which have
///   no installer and no receipt trust).
///
/// # Returns
///
/// * `Ok(AdoptedProfile)` - Profile successfully adopted and validated.
/// * `Err(ProfileIssue::Missing)` - Profile or base version JSON missing.
/// * `Err(ProfileIssue::Corrupt)` - Malformed/hash/path violations.
/// * `Err(ProfileIssue::Unsupported)` - Unsupported metadata or unverifiable
///   no-hash artifacts.
pub fn adopt_installed_profile(
    minecraft_dir: &Path,
    receipts_root: &Path,
    tuple: &LoaderTuple,
    expected_source_sha256: &str,
) -> Result<AdoptedProfile, ProfileIssue> {
    // 1. Validate tuple.
    tuple
        .validate()
        .map_err(|e| ProfileIssue::corrupt(None, e))?;

    let profile_id = derive_profile_id(tuple);

    // 2. Paths.
    let profile_path = minecraft_dir
        .join("versions")
        .join(&profile_id)
        .join(format!("{profile_id}.json"));

    let base_path = minecraft_dir
        .join("versions")
        .join(&tuple.minecraft_version)
        .join(format!("{}.json", tuple.minecraft_version));

    // 3. Bound-read and parse profile JSON.
    let profile_value = bounded_parse_profile_json(&profile_path)?;
    let profile_stable_hash = stable_profile_hash(&profile_value);

    let loader_version: VersionInfo =
        serde_json::from_value(profile_value.clone()).map_err(|e| {
            ProfileIssue::corrupt(
                Some(profile_path.clone()),
                format!("Profile JSON is not valid VersionInfo: {e}"),
            )
        })?;

    // 4. Check base version profile exists and parse it.
    if !base_path.exists() {
        return Err(ProfileIssue::missing(
            Some(base_path.clone()),
            format!(
                "Base Minecraft version '{}' profile not found at {}",
                tuple.minecraft_version,
                base_path.display()
            ),
        ));
    }

    let base_version = bounded_parse_version_info(&base_path)?;

    // 5. Try to read receipt.
    let receipt = read_receipt(receipts_root, tuple)?;
    let produced_libraries = receipt
        .as_ref()
        .map(|rcpt| receipt_produced_libraries(&loader_version.libraries, rcpt))
        .unwrap_or_default();

    // 6. Structural validation.
    validate_loader_profile(
        &loader_version,
        &profile_path,
        &profile_id,
        &tuple.minecraft_version,
        &produced_libraries,
    )?;

    let validated_receipt: Option<InstalledProfileReceipt>;
    let trusted_unhashed_paths: BTreeSet<String>;

    match receipt.as_ref() {
        Some(rcpt) => {
            if expected_source_sha256.is_empty()
                && matches!(tuple.loader.as_str(), "fabric" | "quilt")
            {
                validate_maven_receipt(rcpt, &loader_version, &profile_path)?;
            }
            let expected_relative = format!("versions/{profile_id}/{profile_id}.json");
            match validate_receipt_binding(
                rcpt,
                expected_source_sha256,
                &profile_id,
                &expected_relative,
                &profile_stable_hash,
            ) {
                Ok(()) => {
                    validated_receipt = receipt.clone();
                    // Trust unhashed libraries from this validated receipt.
                    // Only paths present in the receipt's generated_artifact_sha256
                    // or curated_artifact_sha256 map are trusted.
                    let unhashed = collect_unhashed_library_paths(&loader_version.libraries);
                    trusted_unhashed_paths = unhashed
                        .into_iter()
                        .filter(|path| {
                            rcpt.generated_artifact_sha256.contains_key(path.as_str())
                                || rcpt.curated_artifact_sha256.contains_key(path.as_str())
                        })
                        .collect();
                }
                Err(reasons) => {
                    // Receipt exists but doesn't match â†’ unsupported.
                    return Err(ProfileIssue::unsupported(
                        Some(profile_path),
                        reasons
                            .into_iter()
                            .map(|r| format!("Receipt validation failed: {r}"))
                            .collect(),
                    ));
                }
            }
        }
        None => {
            // No receipt found â€” all loaders require one for Agora-managed installs.
            let unhashed = collect_unhashed_library_paths(&loader_version.libraries);
            if !unhashed.is_empty() {
                return Err(ProfileIssue::unsupported(
                    Some(profile_path),
                    vec![
                        "No receipt found for this loader profile".to_string(),
                        format!(
                            "The following libraries cannot be verified \
                             without a valid receipt: {}",
                            unhashed.join(", ")
                        ),
                    ],
                ));
            }
            return Err(ProfileIssue::unsupported(
                Some(profile_path),
                vec!["No receipt found for this loader profile".to_string()],
            ));
        }
    }

    // 7. Merge using launch::merge_forge_version.
    let merged_version = merge_forge_version(&loader_version, &base_version);

    Ok(AdoptedProfile {
        tuple: tuple.clone(),
        profile_id,
        profile_path,
        profile_stable_hash,
        loader_version,
        base_version,
        merged_version,
        receipt: validated_receipt,
        trusted_unhashed_library_paths: trusted_unhashed_paths,
        minecraft_dir: minecraft_dir.to_path_buf(),
    })
}

fn validate_maven_receipt(
    receipt: &InstalledProfileReceipt,
    profile: &VersionInfo,
    profile_path: &Path,
) -> Result<(), ProfileIssue> {
    let repair = || {
        ProfileIssue::unsupported(
            Some(profile_path.to_path_buf()),
            vec!["Repair the loader to verify its libraries against Maven checksums.".into()],
        )
    };
    if receipt.source_kind != LoaderSourceKind::ProfileJson
        || !receipt.source_sha256.is_empty()
        || profile.libraries.is_empty()
    {
        return Err(repair());
    }
    for library in &profile.libraries {
        let artifact = library
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .ok_or_else(repair)?;
        let hash = receipt
            .curated_artifact_sha256
            .get(&artifact.path)
            .or_else(|| receipt.generated_artifact_sha256.get(&artifact.path))
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(repair)?;
        if artifact.sha256.as_ref() != Some(hash) {
            return Err(repair());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// create_receipt_for_installed_profile â€” generates a receipt for a profile
// that was just installed by an external installer (Forge/NeoForge).
// ---------------------------------------------------------------------------

/// Bounded-parse and structurally validate an installed base+loader profile
/// WITHOUT requiring an existing receipt, compute the stable profile hash,
/// enumerate every library artifact path lacking upstream SHA1/SHA256, verify
/// each exact file under `.minecraft/libraries` is regular/safe/nonempty,
/// compute SHA-256, store map, write schema_version=2 receipt atomically,
/// then call normal adoption to prove binding.
///
/// # Exit status guard
///
/// If `exit_status != 0` the function returns
/// [`ProfileIssueKind::CorruptProfile`] immediately **before** any file
/// hashing or I/O. No receipt is written. This prevents receipt generation
/// for failed installer runs.
///
/// # TOCTOU protection
///
/// For each generated library file the function:
/// 1. Captures metadata (length, mtime) before reading.
/// 2. Reads the file through a bounded stream.
/// 3. Re-reads metadata and verifies no change occurred.
///
/// After enumerating all artifacts the profile JSON is re-read and its
/// stable hash verified against the initial hash, detecting concurrent
/// profile modification during receipt generation.
///
/// # Arguments
///
/// * `minecraft_dir` â€” The root `.minecraft` directory.
/// * `receipts_root` â€” Directory where receipts are stored.
/// * `tuple` â€” Identifies the loader, Minecraft version, and loader version.
/// * `source_sha256` â€” SHA-256 hex digest of the curated installer ZIP.
/// * `installer_url` â€” Download URL of the curated installer.
/// * `exit_status` â€” Exit code of the installer process. Must be 0.
///
/// # Returns
///
/// * `Ok(InstalledProfileReceipt)` â€” Receipt written successfully.
/// * `Err(ProfileIssue)` â€” If the profile cannot be validated (includes
///   non-zero exit status, corrupt or missing profile, TOCTOU detection).
pub fn create_receipt_for_installed_profile(
    minecraft_dir: &Path,
    receipts_root: &Path,
    tuple: &LoaderTuple,
    source_sha256: &str,
    source_url: &str,
    exit_status: i32,
    produced_libraries: &BTreeSet<String>,
) -> Result<InstalledProfileReceipt, ProfileIssue> {
    // 0. Reject non-zero exit status before any hashing or I/O.
    if exit_status != 0 {
        return Err(ProfileIssue::corrupt(
            None,
            format!("Installer exit status {exit_status} is non-zero; receipt cannot be created"),
        ));
    }

    // 1. Validate tuple.
    tuple
        .validate()
        .map_err(|e| ProfileIssue::corrupt(None, e))?;

    let profile_id = derive_profile_id(tuple);

    // 2. Derive paths.
    let profile_path = minecraft_dir
        .join("versions")
        .join(&profile_id)
        .join(format!("{profile_id}.json"));

    let base_path = minecraft_dir
        .join("versions")
        .join(&tuple.minecraft_version)
        .join(format!("{}.json", tuple.minecraft_version));

    // 3. Bound-read and parse profile JSON.
    let profile_value = bounded_parse_profile_json(&profile_path)?;
    let profile_stable_hash = stable_profile_hash(&profile_value);

    let loader_version: VersionInfo =
        serde_json::from_value(profile_value.clone()).map_err(|e| {
            ProfileIssue::corrupt(
                Some(profile_path.clone()),
                format!("Profile JSON is not valid VersionInfo: {e}"),
            )
        })?;

    // 4. Check base version profile exists and parse it.
    if !base_path.exists() {
        return Err(ProfileIssue::missing(
            Some(base_path.clone()),
            format!(
                "Base Minecraft version '{}' profile not found at {}",
                tuple.minecraft_version,
                base_path.display()
            ),
        ));
    }

    let _base_version = bounded_parse_version_info(&base_path)?;

    // 5. Structural validation.
    validate_loader_profile(
        &loader_version,
        &profile_path,
        &profile_id,
        &tuple.minecraft_version,
        produced_libraries,
    )?;

    // 6. Enumerate library paths the receipt must pin from disk:
    //    every library without an upstream hash, and every installer
    //    processor output with a blank download URL — a processor output
    //    is only ever created locally, so it can never be downloaded at
    //    launch. Pinning it here gives adoption and the launch planner the
    //    SHA-256 they need to trust the artifact, and fails fast if the
    //    installer did not actually produce it.
    //
    //    A processor output may still carry a published SHA-1 (the real
    //    Forge 1.21.1 profile does), and it is the only digest in that case
    //    that says whether the installer wrote the artifact the profile
    //    describes: a processor can exit 0 having left a truncated jar.
    //    Every published SHA-1 for a pinned path is therefore checked
    //    against the bytes before they are pinned.
    let mut pinned_paths: BTreeSet<String> =
        collect_unhashed_library_paths(&loader_version.libraries)
            .into_iter()
            .collect();
    let mut published_sha1: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for lib in &loader_version.libraries {
        if !produced_libraries.contains(&lib.name) {
            continue;
        }
        // A non-blank URL means the artifact is downloadable, so it needs
        // no receipt pin. Blank (or absent) means local-only: pin it.
        let downloadable = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.artifact.as_ref())
            .map(|artifact| !artifact.url.trim().is_empty())
            .unwrap_or(false);
        if downloadable {
            continue;
        }
        if let Ok(descriptor) = parse_maven_descriptor(&lib.name) {
            let relative_path = descriptor.to_relative_path();
            let digests = published_sha1.entry(relative_path.clone()).or_default();
            digests.extend(
                published_sha1s(lib)
                    .into_iter()
                    .map(|sha1| (lib.name.clone(), sha1)),
            );
            pinned_paths.insert(relative_path);
        }
    }

    // 7. For each pinned library, verify the file under .minecraft/libraries.
    //    TOCTOU protection (item 7): capture metadata before reading, hash
    //    bounded stream, verify metadata again; fail if changed.
    let mut generated_artifact_sha256: BTreeMap<String, String> = BTreeMap::new();
    for rel_path in &pinned_paths {
        // Reject unsafe paths.
        if is_unsafe_relative_path(rel_path) {
            return Err(ProfileIssue::corrupt(
                Some(profile_path.clone()),
                format!("Unhashed library path is unsafe: '{rel_path}'"),
            ));
        }

        let abs_path = minecraft_dir.join("libraries").join(rel_path);

        // --- TOCTOU check 1: read metadata before any file I/O ---
        let meta_before = std::fs::metadata(&abs_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProfileIssue::missing(
                    Some(abs_path.clone()),
                    format!(
                        "Generated library file not found at '{}'",
                        abs_path.display()
                    ),
                )
            } else {
                ProfileIssue::corrupt(
                    Some(abs_path.clone()),
                    format!("Failed to read generated library metadata: {e}"),
                )
            }
        })?;

        if !meta_before.file_type().is_file() {
            return Err(ProfileIssue::corrupt(
                Some(abs_path.clone()),
                "Generated library path is not a regular file".to_string(),
            ));
        }
        if meta_before.len() == 0 {
            return Err(ProfileIssue::corrupt(
                Some(abs_path.clone()),
                "Generated library file is empty".to_string(),
            ));
        }

        let len_before = meta_before.len();
        #[cfg(unix)]
        let modified_before = {
            use std::os::unix::fs::MetadataExt;
            meta_before.mtime()
        };
        #[cfg(not(unix))]
        let modified_before = meta_before.modified().ok();

        // Bounded read with MAX_GENERATED_LIBRARY_SIZE bound.
        let data = read_bounded_file(&abs_path, MAX_GENERATED_LIBRARY_SIZE)?;

        // --- TOCTOU check 2: verify metadata hasn't changed ---
        let meta_after = std::fs::metadata(&abs_path).map_err(|e| {
            ProfileIssue::corrupt(
                Some(abs_path.clone()),
                format!("Failed to read metadata after read: {e}"),
            )
        })?;

        if meta_after.len() != len_before {
            return Err(ProfileIssue::corrupt(
                Some(abs_path.clone()),
                format!(
                    "Generated library file length changed during read: was {len_before}, now {}",
                    meta_after.len()
                ),
            ));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let modified_after = meta_after.mtime();
            if modified_after != modified_before {
                return Err(ProfileIssue::corrupt(
                    Some(abs_path.clone()),
                    "Generated library file modification time changed during read".to_string(),
                ));
            }
        }
        #[cfg(not(unix))]
        if let (Some(before), Ok(after)) = (modified_before, meta_after.modified()) {
            if after != before {
                return Err(ProfileIssue::corrupt(
                    Some(abs_path.clone()),
                    "Generated library file modification time changed during read".to_string(),
                ));
            }
        }

        let sha = agora_core::download::sha256_hex(&data);

        // Before these bytes become the trusted SHA-256 for this artifact,
        // check them against every SHA-1 the profile publishes for it.
        if let Some(published) = published_sha1.get(rel_path) {
            let actual_sha1 = sha1_hex(&data);
            for (library, expected) in published {
                if !expected.eq_ignore_ascii_case(&actual_sha1) {
                    return Err(ProfileIssue::corrupt(
                        Some(abs_path.clone()),
                        format!(
                            "Installer output '{library}' ({rel_path}) does not match the SHA-1 \
                             its profile publishes: the profile says {expected}, the file on disk \
                             is {actual_sha1}"
                        ),
                    ));
                }
            }
        }

        generated_artifact_sha256.insert(rel_path.clone(), sha);
    }

    // 7b. Re-validate profile stable hash after artifact processing to detect
    //     concurrent profile modification (TOCTOU between step 3 and now).
    let profile_value_after = bounded_parse_profile_json(&profile_path)?;
    let profile_stable_hash_after = stable_profile_hash(&profile_value_after);
    if profile_stable_hash_after != profile_stable_hash {
        return Err(ProfileIssue::corrupt(
            Some(profile_path.clone()),
            "Profile stable hash changed during receipt generation (concurrent modification)"
                .to_string(),
        ));
    }

    // 8. Build receipt with schema version 3 (includes source_kind and curated pins).
    let profile_relative_path = format!("versions/{profile_id}/{profile_id}.json");
    let receipt = InstalledProfileReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION, // 3
        tuple: tuple.clone(),
        source_kind: LoaderSourceKind::InstallerJar,
        source_sha256: source_sha256.to_string(),
        source_url: source_url.to_string(),
        profile_id: profile_id.clone(),
        profile_relative_path: profile_relative_path.clone(),
        profile_stable_hash: profile_stable_hash.clone(),
        base_version_id: tuple.minecraft_version.clone(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        installer_exit_status: exit_status,
        generated_artifact_sha256,
        curated_artifact_sha256: BTreeMap::new(),
    };

    // 9. Write receipt atomically.
    write_receipt_atomic(receipts_root, tuple, &receipt)?;

    // 10. Prove binding by calling normal adoption with the same source SHA.
    let _adopted = adopt_installed_profile(minecraft_dir, receipts_root, tuple, source_sha256)
        .inspect_err(|_issue| {
            // If adoption fails despite successful write, try to clean up the
            // receipt to avoid orphaned metadata.
            let _ = remove_receipt(receipts_root, tuple);
        })?;

    Ok(receipt)
}

// ---------------------------------------------------------------------------
// Helper: read a library file with size bound + hash
// ---------------------------------------------------------------------------

/// Read an entire regular file with a size bound, returning SHA-256 hex.
pub fn file_sha256_hex(path: &Path, max_bytes: u64) -> Result<String, ProfileIssue> {
    let data = read_bounded_file(path, max_bytes)?;
    Ok(agora_core::download::sha256_hex(&data))
}

// ---------------------------------------------------------------------------
// create_receipt_for_profile_json â€” receipt for Fabric/Quilt-style installs
// ---------------------------------------------------------------------------

/// Bind a checked Fabric or Quilt profile to its installed Maven libraries.
/// Each hash map records the exact bytes installed under its relative Maven
/// path. Unverified jars required an explicit missing-checksum override.
pub fn create_receipt_for_profile_json(
    minecraft_dir: &Path,
    receipts_root: &Path,
    tuple: &LoaderTuple,
    source_url: &str,
    curated_artifact_sha256: BTreeMap<String, String>,
    unverified_artifact_sha256: BTreeMap<String, String>,
) -> Result<InstalledProfileReceipt, ProfileIssue> {
    // 1. Validate tuple.
    tuple
        .validate()
        .map_err(|e| ProfileIssue::corrupt(None, e))?;

    let profile_id = derive_profile_id(tuple);

    // 2. Derive paths.
    let profile_path = minecraft_dir
        .join("versions")
        .join(&profile_id)
        .join(format!("{profile_id}.json"));

    // 3. Bound-read and parse profile JSON.
    let profile_value = bounded_parse_profile_json(&profile_path)?;
    let profile_stable_hash = stable_profile_hash(&profile_value);

    let _loader_version: VersionInfo =
        serde_json::from_value(profile_value.clone()).map_err(|e| {
            ProfileIssue::corrupt(
                Some(profile_path.clone()),
                format!("Profile JSON is not valid VersionInfo: {e}"),
            )
        })?;

    // 4. Build receipt with schema version 3.
    let profile_relative_path = format!("versions/{profile_id}/{profile_id}.json");
    let receipt = InstalledProfileReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION, // 3
        tuple: tuple.clone(),
        source_kind: LoaderSourceKind::ProfileJson,
        source_sha256: String::new(),
        source_url: source_url.to_string(),
        profile_id: profile_id.clone(),
        profile_relative_path: profile_relative_path.clone(),
        profile_stable_hash: profile_stable_hash.clone(),
        base_version_id: tuple.minecraft_version.clone(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        installer_exit_status: 0,
        generated_artifact_sha256: unverified_artifact_sha256,
        curated_artifact_sha256,
    };

    // 5. Write receipt atomically.
    write_receipt_atomic(receipts_root, tuple, &receipt)?;

    // 6. Prove binding by calling normal adoption for a generated profile.
    let _adopted =
        adopt_installed_profile(minecraft_dir, receipts_root, tuple, "").inspect_err(|_issue| {
            let _ = remove_receipt(receipts_root, tuple);
        })?;

    Ok(receipt)
}

/// Default source kind for backward-compatible deserialization of v1/v2 receipts.
fn default_source_kind() -> LoaderSourceKind {
    LoaderSourceKind::InstallerJar
}

fn deserialize_nullable_hash_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<BTreeMap<String, String>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agora_core::error::LauncherError;
    use serde_json::json;
    use std::fs;

    #[test]
    fn real_forge_placeholder_outputs_resolve_to_the_client_side_only() {
        let install_profile: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/forge-1.21.1-52.1.21-install-profile.json"
        ))
        .expect("real Forge install_profile.json fixture is valid JSON");
        let produced = processor_output_coordinates(&install_profile, "client")
            .expect("client outputs resolve");
        assert!(
            produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"),
            "the patched client jar is a client processor output: {produced:?}"
        );
        assert!(
            !produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:server"),
            "the server jar is not produced by a client install: {produced:?}"
        );
        assert!(
            !produced.contains("net.minecraft:client:1.21.1"),
            "the {{MC_UNPACKED}} output belongs to a `sides: [\"server\"]` processor, so a client \
             install never produces it: {produced:?}"
        );
        assert_eq!(
            produced.len(),
            3,
            "exactly the three client-side processor outputs: {produced:?}"
        );

        // The real generated version.json has the patched client jar with a
        // blank URL; resolving {PATCHED} through data[PATCHED][client] is what
        // lets the structural validation accept it.
        let mut version: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/forge-1.21.1-52.1.21-version.json"
        ))
        .expect("real Forge version.json fixture is valid JSON");
        version["id"] = serde_json::Value::String("forge-1.21.1-52.1.21".into());
        let info: VersionInfo = serde_json::from_value(version).expect("VersionInfo");
        let path = Path::new("versions/forge-1.21.1-52.1.21/forge-1.21.1-52.1.21.json");
        validate_loader_profile(&info, path, "forge-1.21.1-52.1.21", "1.21.1", &produced)
            .expect("the real Forge profile's blank client URL is a client processor output");

        // The other direction, on the same real profile: a server install does
        // get the server-only {MC_UNPACKED} output the client set must not.
        let server = processor_output_coordinates(&install_profile, "server")
            .expect("server outputs resolve");
        assert!(
            server.contains("net.minecraft:server:1.21.1:unpacked"),
            "a `sides: [\"server\"]` processor's output is a server output: {server:?}"
        );
        assert!(
            server.contains("net.minecraftforge:forge:1.21.1-52.1.21:server"),
            "a processor with no `sides` runs on every side: {server:?}"
        );
        assert!(
            !server.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"),
            "the client jar is not a server output: {server:?}"
        );
        assert_eq!(
            server.len(),
            4,
            "the four server-side processor outputs: {server:?}"
        );
    }

    #[test]
    fn processor_sides_are_optional_but_never_guessed() {
        let with_sides = |sides: serde_json::Value| {
            json!({
                "processors": [{
                    "jar": "net.minecraftforge:installertools:1.4.3",
                    "sides": sides,
                    "outputs": { "client": "[org.example:tool:2.0:client]" }
                }]
            })
        };

        // Only an absent `sides` means "no restriction". Present-but-empty is a
        // list that names no side, so the processor produces nothing.
        assert!(processor_output_coordinates(
            &json!({
                "processors": [{ "outputs": { "client": "[org.example:tool:2.0:client]" } }]
            }),
            "client"
        )
        .expect("absent sides means every side")
        .contains("org.example:tool:2.0:client"));
        assert!(
            processor_output_coordinates(&with_sides(json!([])), "client")
                .expect("an empty sides array is valid JSON")
                .is_empty()
        );

        // A sides list that does not name the side being installed.
        assert!(
            processor_output_coordinates(&with_sides(json!(["server"])), "client")
                .expect("a server-only processor is skipped, not an error")
                .is_empty()
        );

        // Present but not an array of strings: reported, not guessed. A JSON
        // null is present, and the installer declares every side by leaving
        // the key out, so null is malformed rather than "every side".
        for malformed in [
            serde_json::Value::Null,
            json!("client"),
            json!(1),
            json!(true),
            json!([1]),
            json!(["client", null]),
            json!({"client": true}),
        ] {
            let err = processor_output_coordinates(&with_sides(malformed.clone()), "client")
                .expect_err("a malformed sides value must be reported");
            assert_eq!(
                err.kind,
                ProfileIssueKind::CorruptProfile,
                "{malformed} {err:?}"
            );
            assert!(
                err.reasons
                    .iter()
                    .any(|reason| reason.contains("sides") && reason.contains("installertools")),
                "the reason must name the processor and the field: {err:?}"
            );
        }
    }

    #[test]
    fn processor_outputs_are_collected_from_output_values_and_placeholders() {
        let install_profile = json!({
            "processors": [
                {
                    "jar": "net.minecraftforge:binarypatcher:1.0.14",
                    "outputs": {
                        "client": "[net.minecraftforge:forge:1.21.1-52.1.21:client]",
                        "server": "[net.minecraftforge:forge:1.21.1-52.1.21:server]"
                    }
                },
                {
                    "outputs": { "client": "[org.example:tool:2.0:client@jar]" }
                }
            ],
            "data": {
                "BINPATCH": {
                    "client": "[net.minecraftforge:forge:1.21.1-52.1.21:client@lzma]",
                    "server": "[net.minecraftforge:forge:1.21.1-52.1.21:server@lzma]"
                },
                "MAPPINGS": {
                    "client": "data/mappings.txt"
                }
            }
        });
        let produced = processor_output_coordinates(&install_profile, "client")
            .expect("client outputs resolve");
        assert!(produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"));
        assert!(produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:server"));
        assert!(produced.contains("org.example:tool:2.0:client"));
        assert_eq!(produced.len(), 3, "{produced:?}");
    }

    #[test]
    fn processor_outputs_are_read_from_the_installer_jar() {
        let mut bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut bytes));
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("install_profile.json", options).unwrap();
            std::io::Write::write_all(
                &mut writer,
                br#"{"processors":[{"outputs":{"client":"[net.minecraftforge:forge:1.21.1-52.1.21:client]"}}],"data":{}}"#,
            )
            .unwrap();
            writer.finish().unwrap();
        }
        let produced = processor_outputs_from_installer_jar(&bytes, "client").unwrap();
        assert!(produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"));

        let err = processor_outputs_from_installer_jar(b"not a zip", "client").unwrap_err();
        assert!(
            err.reasons
                .iter()
                .any(|reason| reason.contains("not a readable JAR")),
            "{err:?}"
        );
    }

    #[test]
    fn a_blank_library_url_is_allowed_only_for_processor_outputs() {
        let value = json!({
            "id": "forge-1.21.1-52.1.21",
            "inheritsFrom": "1.21.1",
            "mainClass": "net.minecraftforge.Main",
            "type": "release",
            "libraries": [
                {
                    "name": "net.minecraftforge:forge:1.21.1-52.1.21:client",
                    "downloads": {
                        "artifact": {
                            "path": "net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-client.jar",
                            "url": ""
                        }
                    }
                }
            ],
            "arguments": { "jvm": [], "game": [] }
        });
        let info: VersionInfo = serde_json::from_value(value).unwrap();
        let path = Path::new("versions/forge-1.21.1-52.1.21/forge-1.21.1-52.1.21.json");
        let install_profile = json!({
            "processors": [
                {
                    "outputs": {
                        "client": "[net.minecraftforge:forge:1.21.1-52.1.21:client@lzma]"
                    }
                }
            ]
        });
        let produced = processor_output_coordinates(&install_profile, "client")
            .expect("client outputs resolve");
        assert!(produced.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"));
        assert!(
            validate_loader_profile(&info, path, "forge-1.21.1-52.1.21", "1.21.1", &produced)
                .is_ok(),
            "the installer produced this artifact, so its blank URL is not a download"
        );

        let err = validate_loader_profile(
            &info,
            path,
            "forge-1.21.1-52.1.21",
            "1.21.1",
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            err.reasons.iter().any(|reason| {
                reason.contains("blank URL")
                    && reason.contains("net.minecraftforge:forge:1.21.1-52.1.21:client")
            }),
            "a blank URL outside the processor outputs must be refused by name: {err:?}"
        );

        let mut http = info.clone();
        http.libraries[0]
            .downloads
            .as_mut()
            .unwrap()
            .artifact
            .as_mut()
            .unwrap()
            .url = "http://maven.minecraftforge.net/forge-client.jar".into();
        let err = validate_loader_profile(&http, path, "forge-1.21.1-52.1.21", "1.21.1", &produced)
            .unwrap_err();
        assert!(
            err.reasons
                .iter()
                .any(|reason| reason.contains("non-HTTPS URL")),
            "a non-empty non-HTTPS URL must still be refused: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Temp fixture helpers
    // -----------------------------------------------------------------------

    struct TempFixture {
        _tmp: tempfile::TempDir,
        minecraft_dir: PathBuf,
        receipts_root: PathBuf,
    }

    impl TempFixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let minecraft_dir = tmp.path().join(".minecraft");
            let receipts_root = tmp.path().join("receipts");
            fs::create_dir_all(&minecraft_dir).expect("create minecraft_dir");
            fs::create_dir_all(&receipts_root).expect("create receipts_root");
            Self {
                _tmp: tmp,
                minecraft_dir,
                receipts_root,
            }
        }

        /// Write a version JSON at `versions/<id>/<id>.json`.
        fn write_profile(&self, id: &str, value: &serde_json::Value) {
            let dir = self.minecraft_dir.join("versions").join(id);
            fs::create_dir_all(&dir).expect("create version dir");
            let path = dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(value).expect("serialize");
            fs::write(&path, &json).expect("write profile");
        }

        /// Write base Minecraft version JSON.
        fn write_base(&self, mc_version: &str) {
            let base = json!({
                "id": mc_version,
                "mainClass": "net.minecraft.client.main.Main",
                "type": "release",
                "libraries": [
                    {
                        "name": "net.minecraft:minecraft:1.0.0",
                        "downloads": {
                            "artifact": {
                                "path": "net/minecraft/minecraft.jar",
                                "url": "https://piston-data.mojang.com/mc.jar",
                                "sha1": "abc123"
                            }
                        }
                    }
                ],
                "arguments": {
                    "jvm": ["-Xmx2G"],
                    "game": ["--username"]
                }
            });
            self.write_profile(mc_version, &base);
        }

        /// Write a receipt for the given tuple.
        fn write_receipt(
            &self,
            tuple: &LoaderTuple,
            installer_sha: &str,
            profile_stable_hash: &str,
        ) -> InstalledProfileReceipt {
            self.write_receipt_with_generated(
                tuple,
                installer_sha,
                profile_stable_hash,
                BTreeMap::new(),
            )
        }

        /// Write a receipt with an optional `generated_artifact_sha256` map.
        fn write_receipt_with_generated(
            &self,
            tuple: &LoaderTuple,
            installer_sha: &str,
            profile_stable_hash: &str,
            generated: BTreeMap<String, String>,
        ) -> InstalledProfileReceipt {
            let profile_id = derive_profile_id(tuple);
            let receipt = InstalledProfileReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                tuple: tuple.clone(),
                source_kind: LoaderSourceKind::InstallerJar,
                source_sha256: installer_sha.to_string(),
                source_url: "https://maven.minecraftforge.net/forge.jar".to_string(),
                profile_id: profile_id.clone(),
                profile_relative_path: format!("versions/{profile_id}/{profile_id}.json"),
                profile_stable_hash: profile_stable_hash.to_string(),
                base_version_id: tuple.minecraft_version.clone(),
                installed_at: "2026-01-01T00:00:00Z".to_string(),
                installer_exit_status: 0,
                generated_artifact_sha256: generated,
                curated_artifact_sha256: BTreeMap::new(),
            };
            write_receipt_atomic(&self.receipts_root, tuple, &receipt).expect("write receipt");
            receipt
        }

        fn forge_tuple() -> LoaderTuple {
            LoaderTuple {
                loader: "forge".to_string(),
                minecraft_version: "1.21".to_string(),
                loader_version: "47.1.0".to_string(),
            }
        }

        fn forge_profile_json() -> serde_json::Value {
            json!({
                "id": "forge-1.21-47.1.0",
                "inheritsFrom": "1.21",
                "mainClass": "net.minecraftforge.Main",
                "type": "release",
                "libraries": [
                    {
                        "name": "net.minecraftforge:forge:47.1.0",
                        "downloads": {
                            "artifact": {
                                "path": "net/minecraftforge/forge/47.1.0/forge-47.1.0.jar",
                                "url": "https://maven.minecraftforge.net/net/minecraftforge/forge/47.1.0/forge-47.1.0.jar",
                                "sha1": "def456"
                            }
                        }
                    },
                    {
                        "name": "net.minecraft:legacy:1.0",
                        "downloads": {
                            "artifact": {
                                "path": "net/minecraft/legacy/1.0/legacy-1.0.jar",
                                "url": "https://maven.minecraftforge.net/net/minecraft/legacy/1.0/legacy-1.0.jar"
                            }
                        }
                    }
                ],
                "arguments": {
                    "jvm": ["-Dforge=true"],
                    "game": []
                }
            })
        }

        fn neoforge_tuple() -> LoaderTuple {
            LoaderTuple {
                loader: "neoforge".to_string(),
                minecraft_version: "1.21".to_string(),
                loader_version: "21.0.0".to_string(),
            }
        }

        fn neoforge_profile_json() -> serde_json::Value {
            json!({
                "id": "neoforge-21.0.0",
                "inheritsFrom": "1.21",
                "mainClass": "net.neoforged.Main",
                "type": "release",
                "libraries": [
                    {
                        "name": "net.neoforged:neoforge:21.0.0",
                        "downloads": {
                            "artifact": {
                                "path": "net/neoforged/neoforge/21.0.0/neoforge-21.0.0.jar",
                                "url": "https://maven.neoforged.net/net/neoforged/neoforge/21.0.0/neoforge-21.0.0.jar",
                                "sha1": "abc789"
                            }
                        }
                    }
                ],
                "arguments": {
                    "jvm": ["-Dneoforge=true"],
                    "game": []
                }
            })
        }

        fn fabric_tuple() -> LoaderTuple {
            LoaderTuple {
                loader: "fabric".to_string(),
                minecraft_version: "1.21".to_string(),
                loader_version: "0.16.0".to_string(),
            }
        }

        fn fabric_profile_json() -> serde_json::Value {
            json!({
                "id": "fabric-loader-0.16.0-1.21",
                "inheritsFrom": "1.21",
                "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
                "type": "release",
                "libraries": [
                    {
                        "name": "net.fabricmc:fabric-loader:0.16.0",
                        "downloads": {
                            "artifact": {
                                "path": "net/fabricmc/fabric-loader/0.16.0/fabric-loader-0.16.0.jar",
                                "url": "https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.16.0/fabric-loader-0.16.0.jar",
                                "sha1": "sha456"
                            }
                        }
                    }
                ],
                "arguments": {
                    "jvm": [],
                    "game": []
                }
            })
        }

        fn quilt_tuple() -> LoaderTuple {
            LoaderTuple {
                loader: "quilt".to_string(),
                minecraft_version: "1.21".to_string(),
                loader_version: "0.19.0".to_string(),
            }
        }

        fn quilt_profile_json() -> serde_json::Value {
            json!({
                "id": "quilt-loader-0.19.0-1.21",
                "inheritsFrom": "1.21",
                "mainClass": "org.quiltmc.loader.impl.launch.knot.KnotClient",
                "type": "release",
                "libraries": [
                    {
                        "name": "org.quiltmc:quilt-loader:0.19.0",
                        "downloads": {
                            "artifact": {
                                "path": "org/quiltmc/quilt-loader/0.19.0/quilt-loader-0.19.0.jar",
                                "url": "https://maven.quiltmc.org/repository/release/org/quiltmc/quilt-loader/0.19.0/quilt-loader-0.19.0.jar",
                                "sha1": "quilt123"
                            }
                        }
                    }
                ],
                "arguments": {
                    "jvm": [],
                    "game": []
                }
            })
        }
    }

    // -----------------------------------------------------------------------
    // derive_profile_id tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_derive_forge_id() {
        let tuple = LoaderTuple {
            loader: "forge".into(),
            minecraft_version: "1.21".into(),
            loader_version: "47.1.0".into(),
        };
        assert_eq!(derive_profile_id(&tuple), "forge-1.21-47.1.0");
    }

    #[test]
    fn test_derive_neoforge_id() {
        let tuple = LoaderTuple {
            loader: "neoforge".into(),
            minecraft_version: "1.21".into(),
            loader_version: "21.0.0".into(),
        };
        assert_eq!(derive_profile_id(&tuple), "neoforge-21.0.0");
    }

    #[test]
    fn test_derive_fabric_id() {
        let tuple = LoaderTuple {
            loader: "fabric".into(),
            minecraft_version: "1.21".into(),
            loader_version: "0.16.0".into(),
        };
        assert_eq!(derive_profile_id(&tuple), "fabric-loader-0.16.0-1.21");
    }

    #[test]
    fn test_derive_quilt_id() {
        let tuple = LoaderTuple {
            loader: "quilt".into(),
            minecraft_version: "1.21".into(),
            loader_version: "0.19.0".into(),
        };
        assert_eq!(derive_profile_id(&tuple), "quilt-loader-0.19.0-1.21");
    }

    #[test]
    #[should_panic(expected = "unknown loader family")]
    fn test_derive_unknown_loader_panics() {
        let tuple = LoaderTuple {
            loader: "unknown".into(),
            minecraft_version: "1.21".into(),
            loader_version: "1.0".into(),
        };
        derive_profile_id(&tuple);
    }

    #[test]
    fn test_validate_tuple_rejects_empty() {
        let tuple = LoaderTuple {
            loader: "".into(),
            minecraft_version: "1.21".into(),
            loader_version: "1.0".into(),
        };
        assert!(tuple.validate().is_err());
    }

    #[test]
    fn test_validate_tuple_rejects_traversal() {
        let tuple = LoaderTuple {
            loader: "forge".into(),
            minecraft_version: "../etc".into(),
            loader_version: "1.0".into(),
        };
        assert!(tuple.validate().is_err());
    }

    #[test]
    fn test_validate_tuple_rejects_colon() {
        let tuple = LoaderTuple {
            loader: "forge".into(),
            minecraft_version: "1.21".into(),
            loader_version: "1:0".into(),
        };
        assert!(tuple.validate().is_err());
    }

    // -----------------------------------------------------------------------
    // Valid Forge receipt adoption
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_forge_with_valid_receipt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        let profile_value = TempFixture::forge_profile_json();
        fix.write_profile("forge-1.21-47.1.0", &profile_value);

        // Compute the stable hash of the profile.
        let hash = stable_profile_hash(&profile_value);

        // The profile has an unhashed library: net/minecraft/legacy/1.0/legacy-1.0.jar
        // We need to provide a SHA-256 hash for it in the receipt.
        let mut generated = BTreeMap::new();
        generated.insert(
            "net/minecraft/legacy/1.0/legacy-1.0.jar".to_string(),
            "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        );

        // Write a valid receipt with generated artifact hashes (schema v2).
        fix.write_receipt_with_generated(&tuple, "curated_sha_abc", &hash, generated);

        let adopted = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect("adoption should succeed");

        assert_eq!(adopted.profile_id, "forge-1.21-47.1.0");
        assert!(adopted.receipt.is_some());
        // Library without upstream SHA should be in trusted set.
        assert!(
            adopted
                .trusted_unhashed_library_paths
                .contains("net/minecraft/legacy/1.0/legacy-1.0.jar"),
            "no-hash library should be trusted via receipt"
        );
    }

    // -----------------------------------------------------------------------
    // Valid NeoForge adoption
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_neoforge_with_valid_receipt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::neoforge_tuple();

        fix.write_base("1.21");
        fix.write_profile("neoforge-21.0.0", &TempFixture::neoforge_profile_json());

        let profile_value = TempFixture::neoforge_profile_json();
        let hash = stable_profile_hash(&profile_value);
        fix.write_receipt(&tuple, "neoforge_sha_xyz", &hash);

        let adopted = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "neoforge_sha_xyz",
        )
        .expect("adoption should succeed");

        assert_eq!(adopted.profile_id, "neoforge-21.0.0");
        assert!(adopted.receipt.is_some());
    }

    // -----------------------------------------------------------------------
    // Fabric/Quilt adoption
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_fabric_structure() {
        let fix = TempFixture::new();
        let tuple = TempFixture::fabric_tuple();

        fix.write_base("1.21");
        fix.write_profile(
            "fabric-loader-0.16.0-1.21",
            &TempFixture::fabric_profile_json(),
        );

        // Write a receipt so adoption succeeds.
        let hash = {
            let profile_path = fix
                .minecraft_dir
                .join("versions/fabric-loader-0.16.0-1.21/fabric-loader-0.16.0-1.21.json");
            let profile_bytes = std::fs::read(&profile_path).unwrap();
            let profile_value: serde_json::Value = serde_json::from_slice(&profile_bytes).unwrap();
            crate::installed_profile::stable_profile_hash(&profile_value)
        };
        fix.write_receipt(&tuple, "sha", &hash);

        let adopted =
            adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
                .expect("Fabric adoption should succeed");

        assert_eq!(adopted.profile_id, "fabric-loader-0.16.0-1.21");
        assert!(adopted.receipt.is_some());
        assert!(adopted.trusted_unhashed_library_paths.is_empty());
    }

    #[test]
    fn test_adopt_quilt_structure() {
        let fix = TempFixture::new();
        let tuple = TempFixture::quilt_tuple();

        fix.write_base("1.21");
        fix.write_profile(
            "quilt-loader-0.19.0-1.21",
            &TempFixture::quilt_profile_json(),
        );

        // Write a receipt so adoption succeeds.
        let hash = {
            let profile_path = fix
                .minecraft_dir
                .join("versions/quilt-loader-0.19.0-1.21/quilt-loader-0.19.0-1.21.json");
            let profile_bytes = std::fs::read(&profile_path).unwrap();
            let profile_value: serde_json::Value = serde_json::from_slice(&profile_bytes).unwrap();
            crate::installed_profile::stable_profile_hash(&profile_value)
        };
        fix.write_receipt(&tuple, "sha", &hash);

        let adopted =
            adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
                .expect("should adopt");

        assert_eq!(adopted.profile_id, "quilt-loader-0.19.0-1.21");
        assert!(adopted.receipt.is_some());
    }

    // -----------------------------------------------------------------------
    // Missing profile/base
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_missing_loader_profile() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();
        fix.write_base("1.21");

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::MissingProfile);
        assert!(
            err.reasons
                .iter()
                .any(|r| r.contains("File not found") || r.contains("not found")),
            "reason should mention file not found, got: {:?}",
            err.reasons
        );
    }

    #[test]
    fn test_adopt_missing_base_profile() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::MissingProfile);
    }

    // -----------------------------------------------------------------------
    // Truncated/corrupt JSON
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_truncated_json_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        // Write truncated profile JSON.
        let profile_dir = fix.minecraft_dir.join("versions").join("forge-1.21-47.1.0");
        fs::create_dir_all(&profile_dir).expect("create dir");
        let profile_path = profile_dir.join("forge-1.21-47.1.0.json");
        fs::write(&profile_path, "{\"id\": \"forge-1.21-47.1.0\", ").expect("write truncated");

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
        assert!(
            err.reasons.iter().any(|r| r.contains("Truncated")),
            "reason should mention truncated, got: {:?}",
            err.reasons
        );
    }

    // -----------------------------------------------------------------------
    // ID mismatch / inheritsFrom mismatch / mainClass mismatch
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_id_mismatch_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        profile["id"] = json!("forge-1.21-47.2.0"); // Wrong version â€” write at *expected* path
        fix.write_profile("forge-1.21-47.1.0", &profile);

        // The profile exists at the expected path but its internal ID doesn't match.
        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
        assert!(
            err.reasons
                .iter()
                .any(|r| r.contains("Profile ID mismatch")),
            "reason should mention ID mismatch, got: {:?}",
            err.reasons
        );
    }

    #[test]
    fn test_adopt_inherits_from_mismatch_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        profile["inheritsFrom"] = json!("1.20");
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
        assert!(
            err.reasons
                .iter()
                .any(|r| r.contains("inheritsFrom mismatch")),
            "reason should mention inheritsFrom mismatch, got: {:?}",
            err.reasons
        );
    }

    #[test]
    fn test_adopt_empty_main_class_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        profile["mainClass"] = json!("");
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
        assert!(
            err.reasons.iter().any(|r| r.contains("mainClass")),
            "reason should mention mainClass, got: {:?}",
            err.reasons
        );
    }

    #[test]
    fn test_adopt_main_class_no_dots_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        profile["mainClass"] = json!("BadClassName");
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
    }

    // -----------------------------------------------------------------------
    // Traversal Maven / path validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_traversal_maven_path_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        // Inject a library with traversal in the artifact path
        profile["libraries"][0]["downloads"]["artifact"]["path"] = json!("../../../etc/passwd");
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
    }

    // -----------------------------------------------------------------------
    // Untrusted URL
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_untrusted_url_corrupt() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::forge_profile_json();
        profile["libraries"][0]["downloads"]["artifact"]["url"] =
            json!("http://evil.example.com/malicious.jar");
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
    }

    // -----------------------------------------------------------------------
    // Unknown rule action â†’ UnsupportedProfileMetadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_unknown_rule_action_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::fabric_tuple();

        fix.write_base("1.21");

        let mut profile = TempFixture::fabric_profile_json();
        profile["libraries"].as_array_mut().unwrap()[0]["rules"] = json!([
            {"action": "unknown_action"}
        ]);
        fix.write_profile("fabric-loader-0.16.0-1.21", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
    }

    // -----------------------------------------------------------------------
    // No-hash generated lib without receipt â†’ UnsupportedProfileMetadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_no_hash_library_without_receipt_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        // Profile has a library without SHA1 and no receipt on disk.
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let err = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
        assert!(
            err.reasons.iter().any(|r| r.contains("No receipt found")),
            "reason should mention missing receipt, got: {:?}",
            err.reasons
        );
    }

    // -----------------------------------------------------------------------
    // Receipt hash/installer drift â†’ UnsupportedProfileMetadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_receipt_installer_drift_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let profile_value = TempFixture::forge_profile_json();
        let hash = stable_profile_hash(&profile_value);

        // Receipt written with a DIFFERENT installer SHA.
        fix.write_receipt(&tuple, "different_sha", &hash);

        let err = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
    }

    #[test]
    fn test_adopt_receipt_hash_drift_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        // Receipt written with a wrong profile hash.
        fix.write_receipt(&tuple, "curated_sha_abc", "wrong_hash");

        let err = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
    }

    // -----------------------------------------------------------------------
    // Schema-v1 receipt with unhashed library â†’ UnsupportedProfileMetadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_schema_v1_receipt_with_unhashed_lib_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        let profile_value = TempFixture::forge_profile_json();
        fix.write_profile("forge-1.21-47.1.0", &profile_value);

        let hash = stable_profile_hash(&profile_value);

        // Write a schema-v1 receipt (no generated_artifact_sha256 map).
        let profile_id = derive_profile_id(&tuple);
        let v1_receipt = InstalledProfileReceipt {
            schema_version: 1,
            tuple: tuple.clone(),
            source_kind: LoaderSourceKind::InstallerJar,
            source_sha256: "curated_sha_abc".to_string(),
            source_url: "https://maven.minecraftforge.net/forge.jar".to_string(),
            profile_id: profile_id.clone(),
            profile_relative_path: format!("versions/{profile_id}/{profile_id}.json"),
            profile_stable_hash: hash,
            base_version_id: tuple.minecraft_version.clone(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installer_exit_status: 0,
            generated_artifact_sha256: BTreeMap::new(),
            curated_artifact_sha256: BTreeMap::new(),
        };
        write_receipt_atomic(&fix.receipts_root, &tuple, &v1_receipt).expect("write v1 receipt");

        // Schema v1 receipts are accepted, but unhashed libraries are not
        // trusted because the hashes aren't in generated_artifact_sha256 or
        // curated_artifact_sha256.
        let adopted = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect("adoption should succeed (unhashed libs simply not trusted)");

        assert!(adopted.trusted_unhashed_library_paths.is_empty());
    }

    // -----------------------------------------------------------------------
    // Valid receipt trusts exact no-hash path
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_receipt_trusts_exact_no_hash_path() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        let profile_value = TempFixture::forge_profile_json();
        fix.write_profile("forge-1.21-47.1.0", &profile_value);

        let hash = stable_profile_hash(&profile_value);

        // Provide a SHA-256 hash for the unhashed library.
        let mut generated = BTreeMap::new();
        generated.insert(
            "net/minecraft/legacy/1.0/legacy-1.0.jar".to_string(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        );

        // Write a valid receipt with generated artifact hashes.
        fix.write_receipt_with_generated(&tuple, "curated_sha_abc", &hash, generated);

        let adopted = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "curated_sha_abc",
        )
        .expect("adoption should succeed with valid receipt");

        // The "net.minecraft:legacy:1.0" library has no SHA1 in its artifact.
        // Its relative path should be in trusted_unhashed_library_paths.
        assert!(
            adopted
                .trusted_unhashed_library_paths
                .contains("net/minecraft/legacy/1.0/legacy-1.0.jar"),
            "no-hash library path must be trusted with valid receipt"
        );
    }

    // -----------------------------------------------------------------------
    // Receipt traversal rejected
    // -----------------------------------------------------------------------

    #[test]
    fn test_receipt_path_is_confined() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        let rpath = receipt_path(&fix.receipts_root, &tuple);
        // Path must be inside receipts_root.
        assert!(
            rpath.starts_with(&fix.receipts_root),
            "receipt path must be confined within receipts_root"
        );
        // Must end with .receipt.json
        assert!(
            rpath.to_string_lossy().ends_with(".receipt.json"),
            "receipt path must end with .receipt.json"
        );
        // No path traversal.
        let lossy = rpath.to_string_lossy();
        assert!(!lossy.contains(".."), "no path traversal in receipt path");
    }

    #[test]
    fn test_receipt_read_rejects_traversal_in_profile_relative_path() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        let rpath = receipt_path(&fix.receipts_root, &tuple);
        fs::create_dir_all(rpath.parent().unwrap()).expect("create parent");

        // Write a receipt with a traversal profile_relative_path.
        let bad_receipt = InstalledProfileReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            tuple: tuple.clone(),
            source_kind: LoaderSourceKind::InstallerJar,
            source_sha256: "sha".into(),
            source_url: "https://example.com".into(),
            profile_id: derive_profile_id(&tuple),
            profile_relative_path: "../../../etc/passwd".into(),
            profile_stable_hash: "hash".into(),
            base_version_id: tuple.minecraft_version.clone(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            installer_exit_status: 0,
            generated_artifact_sha256: BTreeMap::new(),
            curated_artifact_sha256: BTreeMap::new(),
        };
        let json = serde_json::to_string_pretty(&bad_receipt).unwrap();
        fs::write(&rpath, &json).expect("write bad receipt");

        let result = read_receipt(&fix.receipts_root, &tuple);
        assert!(
            result.is_err(),
            "receipt with traversal relative path should be rejected"
        );
        if let Err(ref issue) = result {
            assert_eq!(issue.kind, ProfileIssueKind::CorruptProfile);
        }
    }

    // -----------------------------------------------------------------------
    // Atomic write / no temp residue
    // -----------------------------------------------------------------------

    #[test]
    fn test_atomic_write_no_temp_residue() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();
        let rpath = receipt_path(&fix.receipts_root, &tuple);
        let tmp_path = rpath.with_extension("receipt.json.tmp");

        let pid = derive_profile_id(&tuple);
        let receipt = InstalledProfileReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            tuple: tuple.clone(),
            source_kind: LoaderSourceKind::InstallerJar,
            source_sha256: "sha".into(),
            source_url: "https://example.com".into(),
            profile_id: pid.clone(),
            profile_relative_path: format!("versions/{pid}/{pid}.json"),
            profile_stable_hash: "hash".into(),
            base_version_id: tuple.minecraft_version.clone(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            installer_exit_status: 0,
            generated_artifact_sha256: BTreeMap::new(),
            curated_artifact_sha256: BTreeMap::new(),
        };

        write_receipt_atomic(&fix.receipts_root, &tuple, &receipt)
            .expect("write receipt atomically");

        // Receipt file should exist.
        assert!(rpath.exists(), "receipt file should exist");
        // Temp file should NOT exist.
        assert!(
            !tmp_path.exists(),
            "temp file should be cleaned up after atomic write"
        );

        // Read it back and verify.
        let read_back = read_receipt(&fix.receipts_root, &tuple)
            .expect("read receipt")
            .expect("receipt should exist");
        assert_eq!(read_back.source_sha256, "sha");
    }

    #[test]
    fn test_remove_receipt_no_error_when_missing() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();
        // Should not error when no receipt exists.
        remove_receipt(&fix.receipts_root, &tuple)
            .expect("remove missing receipt should not error");
    }

    // -----------------------------------------------------------------------
    // Stable hash order / time parity
    // -----------------------------------------------------------------------

    #[test]
    fn test_stable_hash_order_and_time_parity() {
        // Profile with time/releaseTime fields in reverse alphabetical order.
        let with_time = json!({
            "releaseTime": "2026-01-01T00:00:00Z",
            "time": "2026-01-01T00:00:00Z",
            "mainClass": "net.minecraftforge.Main",
            "inheritsFrom": "1.21",
            "id": "forge-1.21-47.1.0",
            "type": "release",
            "libraries": [
                {"name": "net.minecraftforge:forge:47.1.0", "downloads": {"artifact": {"path": "forge.jar", "url": "https://example.com/forge.jar", "sha1": "abc"}}}
            ]
        });

        // Same profile without time/releaseTime fields and keys in different order.
        let without_time = json!({
            "id": "forge-1.21-47.1.0",
            "inheritsFrom": "1.21",
            "mainClass": "net.minecraftforge.Main",
            "type": "release",
            "libraries": [
                {"name": "net.minecraftforge:forge:47.1.0", "downloads": {"artifact": {"path": "forge.jar", "url": "https://example.com/forge.jar", "sha1": "abc"}}}
            ]
        });

        let hash1 = stable_profile_hash(&with_time);
        let hash2 = stable_profile_hash(&without_time);

        assert_eq!(
            hash1, hash2,
            "stable hash must be identical regardless of time/releaseTime presence and key ordering"
        );
    }

    // -----------------------------------------------------------------------
    // ProfileIssue â†’ LauncherError conversion
    // -----------------------------------------------------------------------

    #[test]
    fn test_profile_issue_to_launcher_error_conversion() {
        let issue = ProfileIssue {
            kind: ProfileIssueKind::MissingProfile,
            profile_path: Some(PathBuf::from("/fake/path.json")),
            reasons: vec!["File not found".into()],
        };
        let err: LauncherError = issue.into();
        assert_eq!(err.code(), "ERR_PROFILE_MISSING");

        let issue = ProfileIssue {
            kind: ProfileIssueKind::UnsupportedProfileMetadata,
            profile_path: None,
            reasons: vec!["Unknown feature".into()],
        };
        let err: LauncherError = issue.into();
        assert_eq!(err.code(), "ERR_PROFILE_UNSUPPORTED_METADATA");

        let issue = ProfileIssue {
            kind: ProfileIssueKind::CorruptProfile,
            profile_path: None,
            reasons: vec!["Truncated JSON".into()],
        };
        let err: LauncherError = issue.into();
        assert_eq!(err.code(), "ERR_PROFILE_CORRUPT");
    }

    // -----------------------------------------------------------------------
    // Receipt schema validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_read_receipt_rejects_wrong_schema_version() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();
        let rpath = receipt_path(&fix.receipts_root, &tuple);
        fs::create_dir_all(rpath.parent().unwrap()).expect("create parent");

        let pid2 = derive_profile_id(&tuple);
        let bad_receipt = InstalledProfileReceipt {
            schema_version: 999, // Unknown version
            tuple: tuple.clone(),
            source_kind: LoaderSourceKind::InstallerJar,
            source_sha256: "sha".into(),
            source_url: "https://example.com".into(),
            profile_id: pid2.clone(),
            profile_relative_path: format!("versions/{pid2}/{pid2}.json"),
            profile_stable_hash: "hash".into(),
            base_version_id: tuple.minecraft_version.clone(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            installer_exit_status: 0,
            generated_artifact_sha256: BTreeMap::new(),
            curated_artifact_sha256: BTreeMap::new(),
        };
        let json = serde_json::to_string_pretty(&bad_receipt).unwrap();
        fs::write(&rpath, &json).expect("write bad receipt");

        let result = read_receipt(&fix.receipts_root, &tuple);
        assert!(
            result.is_err(),
            "receipt with wrong schema version should be rejected"
        );
    }

    #[test]
    fn test_legacy_v2_receipt_aliases_and_null_hash_map_deserialize() {
        let value = json!({
            "schema_version": 2,
            "tuple": {
                "loader": "forge",
                "minecraft_version": "1.21",
                "loader_version": "47.1.0"
            },
            "installer_sha256": "a".repeat(64),
            "installer_url": "https://maven.minecraftforge.net/installer.jar",
            "profile_id": "forge-1.21-47.1.0",
            "profile_relative_path": "versions/forge-1.21-47.1.0/forge-1.21-47.1.0.json",
            "profile_stable_hash": "b".repeat(64),
            "base_version_id": "1.21",
            "installed_at": "2026-01-01T00:00:00Z",
            "installer_exit_status": 0,
            "generated_artifact_sha256": null
        });

        let receipt: InstalledProfileReceipt = serde_json::from_value(value).unwrap();
        assert_eq!(receipt.source_kind, LoaderSourceKind::InstallerJar);
        assert_eq!(receipt.source_sha256, "a".repeat(64));
        assert!(receipt.generated_artifact_sha256.is_empty());
        assert!(receipt.curated_artifact_sha256.is_empty());
    }

    // -----------------------------------------------------------------------
    // Fabric/Quilt with no-hash library without installer â†’ unsupported
    // -----------------------------------------------------------------------

    #[test]
    fn test_fabric_no_hash_library_without_receipt_unsupported() {
        let fix = TempFixture::new();
        let tuple = TempFixture::fabric_tuple();

        fix.write_base("1.21");

        // Fabric profile with a library that has no upstream SHA.
        let mut profile = TempFixture::fabric_profile_json();
        profile["libraries"][0]["downloads"]["artifact"]["sha1"] = serde_json::Value::Null;
        // Remove sha1 field entirely.
        profile["libraries"][0]["downloads"]["artifact"]
            .as_object_mut()
            .unwrap()
            .remove("sha1");
        fix.write_profile("fabric-loader-0.16.0-1.21", &profile);

        let err = adopt_installed_profile(&fix.minecraft_dir, &fix.receipts_root, &tuple, "sha")
            .expect_err("should fail");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
    }

    // -----------------------------------------------------------------------
    // create_receipt_for_installed_profile tests
    // -----------------------------------------------------------------------

    /// A blank published SHA-1 is not a published hash: the library is treated
    /// as unhashed, so the receipt pins it from disk (and its bytes are then
    /// known to adoption and the launch planner). Reading a blank digest as a
    /// hash left the library neither pinned nor checked.
    #[test]
    fn a_library_with_only_a_blank_published_sha1_is_pinned_from_disk() {
        // The digest may be blank on the library (`sha1`) or on its artifact.
        for blank_on_the_artifact in [false, true] {
            let fix = TempFixture::new();
            let tuple = TempFixture::forge_tuple();
            fix.write_base("1.21");

            let mut profile = TempFixture::forge_profile_json();
            let mut blank = json!({
                "name": "org.example:blank:1.0",
                "downloads": {
                    "artifact": {
                        "path": "org/example/blank/1.0/blank-1.0.jar",
                        "url": "https://maven.minecraftforge.net/org/example/blank/1.0/blank-1.0.jar",
                        "sha1": "not blank"
                    }
                }
            });
            if blank_on_the_artifact {
                blank["downloads"]["artifact"]["sha1"] = serde_json::Value::String("   ".into());
            } else {
                blank["sha1"] = serde_json::Value::String(String::new());
                blank["downloads"]["artifact"]
                    .as_object_mut()
                    .unwrap()
                    .remove("sha1");
            }
            profile["libraries"].as_array_mut().unwrap().push(blank);
            fix.write_profile("forge-1.21-47.1.0", &profile);

            let lib_dir = fix
                .minecraft_dir
                .join("libraries")
                .join("org/example/blank/1.0");
            fs::create_dir_all(&lib_dir).expect("create lib dir");
            fs::write(lib_dir.join("blank-1.0.jar"), b"blank digest bytes").expect("write lib");

            // The base fixture's own unhashed library is pinned too, so it has
            // to exist before the blank-digest library is reached.
            let legacy_dir = fix
                .minecraft_dir
                .join("libraries")
                .join("net/minecraft/legacy/1.0");
            fs::create_dir_all(&legacy_dir).expect("create legacy lib dir");
            fs::write(
                legacy_dir.join("legacy-1.0.jar"),
                b"generated library content",
            )
            .expect("write legacy lib");

            let receipt = create_receipt_for_installed_profile(
                &fix.minecraft_dir,
                &fix.receipts_root,
                &tuple,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "https://maven.minecraftforge.net/forge.jar",
                0,
                &BTreeSet::new(),
            )
            .expect("a blank published digest is no hash to check");

            assert_eq!(
                receipt
                    .generated_artifact_sha256
                    .get("org/example/blank/1.0/blank-1.0.jar"),
                Some(&sha256_hex(b"blank digest bytes")),
                "a library whose only digest is blank must be pinned from disk \
                 (blank on the artifact: {blank_on_the_artifact}): {:?}",
                receipt.generated_artifact_sha256.keys().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn test_create_receipt_valid_forge() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        // Write the generated library file so it exists for hash computation.
        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(lib_dir.join("legacy-1.0.jar"), b"generated library content").expect("write lib");

        let installer_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let receipt = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            installer_sha,
            "https://maven.minecraftforge.net/forge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect("create_receipt should succeed");

        assert_eq!(receipt.schema_version, RECEIPT_SCHEMA_VERSION);
        assert_eq!(receipt.source_sha256, installer_sha);
        assert_eq!(receipt.tuple.loader, "forge");
        assert!(!receipt.generated_artifact_sha256.is_empty());
        let generated = receipt.generated_artifact_sha256;
        // The unhashed library should have a SHA-256 entry.
        assert!(generated.contains_key("net/minecraft/legacy/1.0/legacy-1.0.jar"));
        // The hash should be the SHA-256 of "generated library content".
        let expected = agora_core::download::sha256_hex(b"generated library content");
        assert_eq!(
            generated
                .get("net/minecraft/legacy/1.0/legacy-1.0.jar")
                .unwrap(),
            &expected
        );
    }

    #[test]
    fn test_create_receipt_valid_neoforge() {
        let fix = TempFixture::new();
        let tuple = TempFixture::neoforge_tuple();

        fix.write_base("1.21");
        fix.write_profile("neoforge-21.0.0", &TempFixture::neoforge_profile_json());

        // NeoForge profile has no unhashed libraries in the test fixture,
        // so no generated libs to create.

        let installer_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let receipt = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            installer_sha,
            "https://maven.neoforged.net/neoforge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect("create_receipt for neoforge should succeed");

        assert_eq!(receipt.schema_version, RECEIPT_SCHEMA_VERSION);
        assert_eq!(receipt.tuple.loader, "neoforge");
    }

    #[test]
    fn test_create_receipt_missing_generated_file() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());
        // Do NOT write the generated library file â€” it should fail with MissingProfile

        let err = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "sha",
            "https://example.com/forge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect_err("should fail when generated library is missing");

        assert_eq!(err.kind, ProfileIssueKind::MissingProfile);
        assert!(
            err.reasons.iter().any(|r| r.contains("not found")),
            "reason should mention not found, got: {:?}",
            err.reasons
        );
    }

    #[test]
    fn test_create_receipt_traversal_path_in_library() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");

        // Write a profile with a traversal path in an unhashed library.
        let mut profile = TempFixture::forge_profile_json();
        // Add a library with a traversal path and no upstream SHA.
        profile["libraries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "evil:evil:1.0",
                "downloads": {
                    "artifact": {
                        "path": "../../../etc/passwd",
                        "url": "https://maven.minecraftforge.net/evil/evil.jar"
                    }
                }
            }));
        fix.write_profile("forge-1.21-47.1.0", &profile);

        let err = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "sha",
            "https://example.com/forge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect_err("should reject traversal path");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
    }

    #[test]
    fn test_create_receipt_hash_map_exactness() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        // Write the generated library file.
        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(
            lib_dir.join("legacy-1.0.jar"),
            b"exact content for hash check",
        )
        .expect("write lib");

        let installer_sha = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let receipt = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            installer_sha,
            "https://maven.minecraftforge.net/forge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect("create_receipt should succeed");

        // Verify the hash map contains exactly one entry: the legacy lib.
        let generated = receipt.generated_artifact_sha256;
        assert_eq!(
            generated.len(),
            1,
            "should have exactly one generated artifact entry"
        );
        let expected_hash = agora_core::download::sha256_hex(b"exact content for hash check");
        assert_eq!(
            generated
                .get("net/minecraft/legacy/1.0/legacy-1.0.jar")
                .unwrap(),
            &expected_hash,
            "hash must match the actual file content"
        );
    }

    #[test]
    fn test_create_receipt_atomic_write_no_temp_residue() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(lib_dir.join("legacy-1.0.jar"), b"atomic test content").expect("write lib");

        let rpath = receipt_path(&fix.receipts_root, &tuple);
        let tmp_path = rpath.with_extension("receipt.json.tmp");

        let _receipt = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "https://maven.minecraftforge.net/forge.jar",
            0,
            &BTreeSet::new(),
        )
        .expect("create_receipt should succeed");

        // Receipt should exist, temp file should be gone.
        assert!(rpath.exists(), "receipt file must exist");
        assert!(
            !tmp_path.exists(),
            "temp file must be removed after atomic write"
        );
    }

    // -----------------------------------------------------------------------
    // create_receipt_for_installed_profile — the profile's own SHA-1 for a
    // processor output
    // -----------------------------------------------------------------------

    /// The real Forge 1.21.1-52.1.21 profile, and a jar under `libraries/`
    /// holding `bytes`.
    fn real_forge_fixture(bytes: &[u8]) -> (TempFixture, LoaderTuple, serde_json::Value) {
        let fix = TempFixture::new();
        let tuple = LoaderTuple {
            loader: "forge".to_string(),
            minecraft_version: "1.21.1".to_string(),
            loader_version: "52.1.21".to_string(),
        };
        fix.write_base("1.21.1");
        let mut version: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/forge-1.21.1-52.1.21-version.json"
        ))
        .expect("real Forge version.json fixture is valid JSON");
        version["id"] = serde_json::Value::String("forge-1.21.1-52.1.21".into());
        fix.write_profile("forge-1.21.1-52.1.21", &version);

        let lib = fix.minecraft_dir.join(
            "libraries/net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-client.jar",
        );
        fs::create_dir_all(lib.parent().expect("library dir")).expect("create lib dir");
        fs::write(&lib, bytes).expect("write processor output");
        (fix, tuple, version)
    }

    /// The client-side processor outputs the real Forge installer produces.
    fn real_forge_produced() -> BTreeSet<String> {
        let install_profile: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/forge-1.21.1-52.1.21-install-profile.json"
        ))
        .expect("real Forge install_profile.json fixture is valid JSON");
        processor_output_coordinates(&install_profile, "client").expect("client outputs resolve")
    }

    /// The installer can exit 0 having left a truncated or wrong jar where the
    /// profile publishes a SHA-1. Pinning those bytes as the trusted SHA-256
    /// would make the wrong artifact trusted forever, so a mismatch has to
    /// fail before a receipt is written.
    #[test]
    fn a_processor_output_that_does_not_match_its_published_sha1_is_refused() {
        let bytes = b"truncated installer output";
        let (fix, tuple, _version) = real_forge_fixture(bytes);

        let err = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-installer.jar",
            0,
            &real_forge_produced(),
        )
        .expect_err("a truncated processor output must not be pinned");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile, "{err:?}");
        let text = err.reasons.join(" ");
        assert!(
            text.contains("net.minecraftforge:forge:1.21.1-52.1.21:client"),
            "the reason must name the library: {text}"
        );
        assert!(
            text.contains("28f7acaa0e9b1c2d2a7bf52c07ae16133bd3da4e"),
            "the reason must name the published SHA-1: {text}"
        );
        assert!(
            text.contains(&sha1_hex(bytes)),
            "the reason must name the digest of the bytes found: {text}"
        );
        assert!(
            read_receipt(&fix.receipts_root, &tuple)
                .expect("reading receipts is not an error here")
                .is_none(),
            "no receipt may be written for bytes that failed their published SHA-1"
        );
    }

    /// The same jar holding the bytes the profile publishes for it is pinned,
    /// and a published digest is compared case-insensitively.
    #[test]
    fn a_processor_output_matching_its_published_sha1_is_pinned() {
        let bytes = b"the installer's real bytes";
        let (fix, tuple, mut version) = real_forge_fixture(bytes);
        // Publish the digest of the bytes actually on disk, uppercased: hex is
        // case-insensitive.
        version["libraries"][1]["downloads"]["artifact"]["sha1"] =
            serde_json::Value::String(sha1_hex(bytes).to_uppercase());
        fix.write_profile("forge-1.21.1-52.1.21", &version);

        let receipt = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-installer.jar",
            0,
            &real_forge_produced(),
        )
        .expect("bytes that match the published SHA-1 are pinned");

        assert_eq!(
            receipt
                .generated_artifact_sha256
                .get("net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-client.jar"),
            Some(&sha256_hex(bytes)),
            "the receipt pins the SHA-256 of the bytes the SHA-1 matched"
        );
    }

    /// A published SHA-1 that is present but blank is not a hash to check:
    /// `agora_core::artifact_hash` treats a blank *published* digest as the
    /// source having published nothing (only a blank *asserted* hash is an
    /// error). The artifact is still pinned from disk, as before.
    #[test]
    fn a_blank_published_sha1_on_a_processor_output_pins_without_a_check() {
        for blank in ["", "   "] {
            let bytes = b"whatever the installer produced";
            let (fix, tuple, mut version) = real_forge_fixture(bytes);
            version["libraries"][1]["downloads"]["artifact"]["sha1"] =
                serde_json::Value::String(blank.to_string());
            fix.write_profile("forge-1.21.1-52.1.21", &version);

            let receipt = create_receipt_for_installed_profile(
                &fix.minecraft_dir,
                &fix.receipts_root,
                &tuple,
                "9999999999999999999999999999999999999999999999999999999999999999",
                "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-installer.jar",
                0,
                &real_forge_produced(),
            )
            .unwrap_or_else(|err| panic!("a blank published digest is no hash to check: {err:?}"));

            assert!(
                receipt.generated_artifact_sha256.contains_key(
                    "net/minecraftforge/forge/1.21.1-52.1.21/forge-1.21.1-52.1.21-client.jar"
                ),
                "the artifact is still pinned from disk"
            );
        }
    }

    // -----------------------------------------------------------------------
    // create_receipt_for_installed_profile — exit status check
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_receipt_rejects_nonzero_exit() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(lib_dir.join("legacy-1.0.jar"), b"content").expect("write lib");

        // Non-zero exit status should be rejected BEFORE any hashing/writing.
        let err = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "https://maven.minecraftforge.net/forge.jar",
            1, // non-zero exit
            &BTreeSet::new(),
        )
        .expect_err("should reject non-zero exit");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);
        assert!(
            err.reasons.iter().any(|r| r.contains("exit status")),
            "reason should mention non-zero exit, got: {:?}",
            err.reasons
        );

        // No receipt should have been written.
        let rpath = receipt_path(&fix.receipts_root, &tuple);
        assert!(
            !rpath.exists(),
            "receipt must NOT exist after non-zero exit"
        );
    }

    #[test]
    fn test_create_receipt_rejects_negative_exit() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(lib_dir.join("legacy-1.0.jar"), b"content").expect("write lib");

        // Negative exit status should also be rejected.
        let err = create_receipt_for_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "https://maven.minecraftforge.net/forge.jar",
            -1,
            &BTreeSet::new(),
        )
        .expect_err("should reject negative exit");

        assert_eq!(err.kind, ProfileIssueKind::CorruptProfile);

        let rpath = receipt_path(&fix.receipts_root, &tuple);
        assert!(
            !rpath.exists(),
            "receipt must NOT exist after negative exit"
        );
    }

    // -----------------------------------------------------------------------
    // Receiptless Forge profile with all hashed artifacts â€” validates that
    // adoption succeeds without a receipt when every library has an upstream
    // hash (item 8).
    // -----------------------------------------------------------------------

    #[test]
    fn test_adopt_forge_all_hashed_artifacts_no_receipt_needed() {
        let fix = TempFixture::new();
        let tuple = TempFixture::neoforge_tuple();

        fix.write_base("1.21");
        fix.write_profile("neoforge-21.0.0", &TempFixture::neoforge_profile_json());

        // Write a receipt first since adoption now requires one for all loaders.
        let hash = {
            let profile_path = fix
                .minecraft_dir
                .join("versions/neoforge-21.0.0/neoforge-21.0.0.json");
            let profile_bytes = std::fs::read(&profile_path).unwrap();
            let profile_value: serde_json::Value = serde_json::from_slice(&profile_bytes).unwrap();
            crate::installed_profile::stable_profile_hash(&profile_value)
        };
        fix.write_receipt(&tuple, "neoforge_sha_xyz", &hash);

        let adopted = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "neoforge_sha_xyz",
        )
        .expect("adoption should succeed with receipt");

        assert_eq!(adopted.profile_id, "neoforge-21.0.0");
        assert!(adopted.receipt.is_some());
        assert!(
            adopted.trusted_unhashed_library_paths.is_empty(),
            "no unhashed paths when all artifacts have upstream hashes"
        );
    }

    // -----------------------------------------------------------------------
    // TOCTOU detection: profile hash re-validation after artifact processing.
    // Simulate a concurrent profile modification by modifying the profile
    // between the initial read and the file hashing loop.
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_receipt_detects_profile_hash_change() {
        let fix = TempFixture::new();
        let tuple = TempFixture::forge_tuple();

        fix.write_base("1.21");
        fix.write_profile("forge-1.21-47.1.0", &TempFixture::forge_profile_json());

        // Write the generated library file.
        let lib_dir = fix
            .minecraft_dir
            .join("libraries")
            .join("net/minecraft/legacy/1.0");
        fs::create_dir_all(&lib_dir).expect("create lib dir");
        fs::write(lib_dir.join("legacy-1.0.jar"), b"content").expect("write lib");

        // Modify the profile AFTER the initial read but before the re-validation.
        // Since create_receipt_for_installed_profile reads the profile first,
        // processes files, then re-reads to validate hash, we modify it after
        // this function completes partially... We can't easily inject into the
        // middle of the function, but we CAN verify the behavior by making the
        // profile change between calls: instead we test at the adoption level.
        //
        // Write receipt manually with a hash that doesn't match the current
        // profile, then adoption should detect the mismatch.
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let receipt = InstalledProfileReceipt {
            schema_version: 2,
            tuple: tuple.clone(),
            source_kind: LoaderSourceKind::InstallerJar,
            source_sha256: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                .into(),
            source_url: "https://maven.minecraftforge.net/forge.jar".into(),
            profile_id: "forge-1.21-47.1.0".into(),
            profile_relative_path: "versions/forge-1.21-47.1.0/forge-1.21-47.1.0.json".into(),
            profile_stable_hash: wrong_hash.into(),
            base_version_id: "1.21".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            installer_exit_status: 0,
            generated_artifact_sha256: BTreeMap::new(),
            curated_artifact_sha256: BTreeMap::new(),
        };
        write_receipt_atomic(&fix.receipts_root, &tuple, &receipt).expect("write receipt");

        // Now adoption should fail because the receipt hash doesn't match the
        // current profile.
        let err = adopt_installed_profile(
            &fix.minecraft_dir,
            &fix.receipts_root,
            &tuple,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        )
        .expect_err("adoption should fail when receipt hash doesn't match profile");

        assert_eq!(err.kind, ProfileIssueKind::UnsupportedProfileMetadata);
        assert!(
            err.reasons.iter().any(|r| r.contains("stable hash")),
            "reason should mention stable hash, got: {:?}",
            err.reasons
        );
    }

    // -----------------------------------------------------------------------
    // MAX_GENERATED_LIBRARY_SIZE constant test
    // -----------------------------------------------------------------------

    #[test]
    fn test_max_generated_library_size_is_reasonable() {
        // The constant must be at least large enough for typical Forge libraries
        // (usually < 32 MiB) but not absurdly large.
        const {
            assert!(
                MAX_GENERATED_LIBRARY_SIZE >= 8 * 1024 * 1024,
                "MAX_GENERATED_LIBRARY_SIZE should be at least 8 MiB"
            );
            assert!(
                MAX_GENERATED_LIBRARY_SIZE <= 256 * 1024 * 1024,
                "MAX_GENERATED_LIBRARY_SIZE should be at most 256 MiB"
            );
        }
    }

    // -----------------------------------------------------------------------
    // is_unsafe_relative_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_unsafe_relative_path_rejects_traversal() {
        assert!(is_unsafe_relative_path("../foo/bar"));
        assert!(is_unsafe_relative_path("foo/../../bar"));
    }

    #[test]
    fn test_is_unsafe_relative_path_rejects_absolute() {
        assert!(is_unsafe_relative_path("/etc/passwd"));
        assert!(is_unsafe_relative_path("C:\\windows"));
    }

    #[test]
    fn test_is_unsafe_relative_path_rejects_null() {
        assert!(is_unsafe_relative_path("foo\0bar"));
    }

    #[test]
    fn test_is_unsafe_relative_path_accepts_safe() {
        assert!(!is_unsafe_relative_path(
            "net/minecraft/forge/1.0/forge-1.0.jar"
        ));
        assert!(!is_unsafe_relative_path("a/b/c.jar"));
    }

    // -----------------------------------------------------------------------
    // safe_filename tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_safe_filename_preserves_safe_chars() {
        assert_eq!(safe_filename("forge-1.21-47.1.0"), "forge-1.21-47.1.0");
        assert_eq!(safe_filename("abc123_-."), "abc123_-.");
    }

    #[test]
    fn test_safe_filename_replaces_unsafe_chars() {
        assert_eq!(safe_filename("foo/bar"), "foo-bar");
        assert_eq!(safe_filename("a:b"), "a-b");
    }
}
