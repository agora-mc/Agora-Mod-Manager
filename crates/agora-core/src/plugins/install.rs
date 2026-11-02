//! Getting a plugin's files onto disk, and taking them off again.
//!
//! Two sources, deliberately different:
//!
//! - a **package** (`.zip`) is unpacked into Agora's own directory after every
//!   entry has been checked. Nothing is written outside the destination, and
//!   the destination is only swapped in once the whole archive has been
//!   validated — a half-extracted plugin never becomes an installed one.
//! - a **development folder** is loaded in place and never copied, because the
//!   entire point is that the author edits it and reloads.
//!
//! Neither path runs plugin code. Validation, capability consent and the
//! install record all complete before anything is activated, so a plugin that
//! crashes on activation is still a plugin the user can see and remove.

use super::store;
use super::store::PluginSource;
use crate::error::{LauncherError, LauncherResult};
use agora_plugin_api::capability::{CapabilityRequest, CapabilitySet};
use agora_plugin_api::distribution::{PublicKey, UpdateSource, UPDATE_SOURCE_FILENAME};
use agora_plugin_api::manifest::{PluginId, PluginManifest, MANIFEST_FILENAME};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// How many files one plugin package may contain.
pub const MAX_ENTRIES: usize = 2_000;

/// How large one file inside a package may be once expanded.
pub const MAX_ENTRY_BYTES: u64 = 16 * 1024 * 1024;

/// How large a whole package may be once expanded.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Largest expansion ratio a package may have before it is treated as hostile.
///
/// A legitimately compressible plugin (JSON, JavaScript) reaches perhaps 20:1.
/// Past 200:1 the archive is not a plugin, it is a decompression bomb.
pub const MAX_EXPANSION_RATIO: u64 = 200;

fn invalid_package(message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_PLUGIN_PACKAGE_INVALID".into(),
        message: message.into(),
    }
}

fn io_error(message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_PLUGIN_IO".into(),
        message: message.into(),
    }
}

/// The distribution block, reduced to what a person can act on.
///
/// The raw key bytes are not useful in a dialog; the host it will contact and
/// a comparable fingerprint are.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSourceSummary {
    pub url: String,
    /// Host part of `url`, so the prompt can name it without the reader
    /// parsing a URL themselves.
    pub host: String,
    /// One entry per pinned key: its id and short fingerprint.
    pub keys: Vec<KeyFingerprint>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyFingerprint {
    pub id: String,
    pub fingerprint: String,
}

impl UpdateSourceSummary {
    /// Build from a stored trust record rather than a package.
    ///
    /// The manager shows what was *pinned*, which is not necessarily what the
    /// currently installed package would declare if it were read again.
    pub fn from_parts(url: &str, keys: &[agora_plugin_api::distribution::PublicKey]) -> Self {
        UpdateSourceSummary {
            url: url.to_string(),
            host: host_of(url),
            keys: keys
                .iter()
                .map(|key| KeyFingerprint {
                    id: key.id.clone(),
                    fingerprint: key.fingerprint(),
                })
                .collect(),
        }
    }

    fn of(source: &UpdateSource) -> Self {
        UpdateSourceSummary {
            url: source.url.clone(),
            host: host_of(&source.url),
            keys: source
                .keys
                .iter()
                .map(|key| KeyFingerprint {
                    id: key.id.clone(),
                    fingerprint: key.fingerprint(),
                })
                .collect(),
        }
    }
}

/// Host part of an https URL, or the whole URL if it cannot be read as one.
///
/// Deliberately string work rather than a URL parser: this is for display, the
/// URL has already been checked to start with `https://`, and the authoritative
/// host check happens in the network layer where it belongs.
fn host_of(url: &str) -> String {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .map(|host| host.split('@').next_back().unwrap_or(host).to_string())
        .unwrap_or_else(|| url.to_string())
}

/// The install already on record for a plugin id, as the preview needs it.
///
/// Grouped rather than passed as three positional options because the three
/// are only ever known together, and a caller that has none of them means
/// something specific: nothing is installed under this id.
#[derive(Debug, Clone, Copy)]
pub struct ExistingInstall<'a> {
    pub version: &'a semver::Version,
    pub data_version: u32,
    /// What the user actually granted, which may be narrower than what the
    /// installed manifest asks for.
    pub granted: &'a CapabilitySet,
    /// The hosts the installed version was allowed to reach.
    ///
    /// Held separately from `granted` because the `network` capability and the
    /// host list are two different agreements. Keeping the capability while
    /// changing the list is still asking for something new.
    pub hosts: &'a [String],
    /// The installed version has code.
    pub runs_code: bool,
    /// The publisher keys pinned when this plugin was installed, if it had an
    /// update source. A package naming different keys is a publisher change.
    pub trusted_keys: Option<&'a [PublicKey]>,
    /// The code digest the script consent was given for, if the user answered.
    pub code_hash: Option<&'a str>,
}

/// What the user is agreeing to when they install something.
///
/// Produced by inspecting a package *without* installing it, so the manager
/// and the CLI can show the same prompt before anything touches disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreview {
    pub manifest: PluginManifest,
    /// Capabilities that must be granted for the plugin to work at all.
    pub required_capabilities: Vec<CapabilityDescription>,
    /// Capabilities the plugin will use if granted, and do without if not.
    pub optional_capabilities: Vec<CapabilityDescription>,
    /// Capabilities it asks for that this version of Agora cannot provide.
    pub unsupported_capabilities: Vec<String>,
    /// Set when a plugin with this id is already installed.
    pub replaces_version: Option<String>,
    /// Capabilities this package asks for that are not already granted.
    ///
    /// On a first install that is everything it asks for. On a replacement it
    /// is the *widening* — and it is the widening the user has to agree to,
    /// because agreeing once to `instance:read` is not agreeing later to
    /// `content:write`.
    pub added_capabilities: Vec<String>,
    /// Hosts this package would reach that the installed version could not.
    ///
    /// `network` is not permission to reach the internet, it is permission to
    /// reach a named list. An update that keeps the capability and adds a host
    /// to the list has widened its reach just as surely as one that asked for
    /// a new capability, and is treated the same way.
    pub added_hosts: Vec<String>,
    /// Where this package says its updates will come from, if anywhere.
    ///
    /// Shown at install time because pinning a publisher key is part of what
    /// is being agreed to, and because it is the only moment at which the key
    /// can be compared against something the author published elsewhere.
    pub update_source: Option<UpdateSourceSummary>,
    /// This update is signed by a different publisher key than the install
    /// pinned. The user has to confirm it explicitly, and script consent is
    /// asked again because it was given for the old publisher's code.
    pub publisher_changed: bool,
    /// True when the upgrade changes the plugin's stored data shape.
    pub migrates_data: bool,
    /// It asks to reach any host, or more hosts than can reasonably be
    /// reviewed, so it installs only with Reduced security mode on.
    pub needs_reduced_security: bool,
    pub file_count: usize,
    pub uncompressed_bytes: u64,
    /// The plugin has code (an entrypoint). The install prompt asks, once,
    /// whether to allow it to run ("<name> runs code. Allow it?").
    pub runs_code: bool,
    /// The plugin has code and the installed version did not, or nothing is
    /// installed. An update that adds code does not inherit permission to run it.
    pub adds_code: bool,
    /// This package's code is not the code the user's answer was given for
    /// (new, changed, or a changed publisher). The code prompt is asked again,
    /// and until it is answered the scripts do not run.
    pub code_changed: bool,
}

impl InstallPreview {
    /// Whether installing this manifest requires an explicit capability grant.
    ///
    /// Kept on the core-owned preview so adapters do not duplicate the
    /// consent rule while deciding whether to show their own prompt.
    pub fn requires_capability_consent(&self) -> bool {
        !self.added_capabilities.is_empty()
            || !self.added_hosts.is_empty()
            || self.publisher_changed
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescription {
    pub name: String,
    pub summary: String,
    pub is_mutating: bool,
}

/// Hosts this manifest declares that the installed version did not.
///
/// Compared case-insensitively because hostnames are, and a list differing
/// only in case is the same list.
pub fn added_hosts(previous: Option<&[String]>, manifest: &PluginManifest) -> Vec<String> {
    let known = previous.unwrap_or(&[]);
    manifest
        .network
        .hosts
        .iter()
        .filter(|host| !known.iter().any(|seen| seen.eq_ignore_ascii_case(host)))
        .cloned()
        .collect()
}

/// Capabilities this manifest asks for beyond what is already granted.
///
/// `previous` is the grant on record for a plugin of the same id, and `None`
/// means there is nothing on record — a first install, or a reinstall after an
/// uninstall that kept the data. In that case everything it asks for is new,
/// which is exactly right: an uninstall ends the grant even when the settings
/// survive it.
///
/// Only supported capabilities appear here. One this build cannot provide is
/// not something to ask the user about; it is reported separately and fails
/// the install if it was required.
pub fn added_capabilities(
    previous: Option<&CapabilitySet>,
    manifest: &PluginManifest,
) -> Vec<String> {
    match previous {
        Some(granted) => widens_capabilities(granted, manifest),
        None => manifest
            .capabilities
            .required
            .iter()
            .chain(manifest.capabilities.optional.iter())
            .filter_map(|name| name.parse::<agora_plugin_api::Capability>().ok())
            .map(|cap| cap.as_str().to_string())
            .collect(),
    }
}

fn describe(
    request: &CapabilityRequest,
) -> (
    Vec<CapabilityDescription>,
    Vec<CapabilityDescription>,
    Vec<String>,
) {
    let mut required = Vec::new();
    let mut optional = Vec::new();
    let mut unsupported = Vec::new();
    for name in &request.required {
        match name.parse::<agora_plugin_api::Capability>() {
            Ok(cap) => required.push(CapabilityDescription {
                name: cap.as_str().to_string(),
                summary: cap.summary().to_string(),
                is_mutating: cap.is_mutating(),
            }),
            Err(()) => unsupported.push(name.clone()),
        }
    }
    for name in &request.optional {
        if let Ok(cap) = name.parse::<agora_plugin_api::Capability>() {
            optional.push(CapabilityDescription {
                name: cap.as_str().to_string(),
                summary: cap.summary().to_string(),
                is_mutating: cap.is_mutating(),
            });
        }
    }
    (required, optional, unsupported)
}

// ---------------------------------------------------------------------------
// Reading a manifest
// ---------------------------------------------------------------------------

/// Read and validate the manifest from a development folder.
pub fn read_folder_manifest(folder: &Path) -> LauncherResult<PluginManifest> {
    let path = folder.join(MANIFEST_FILENAME);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        invalid_package(format!(
            "could not read {MANIFEST_FILENAME} in {}: {e}",
            folder.display()
        ))
    })?;
    let manifest = PluginManifest::parse(&text).map_err(|e| invalid_package(e.message))?;
    if let Some(entrypoint) = &manifest.entrypoint {
        if !folder.join(entrypoint).is_file() {
            return Err(invalid_package(format!(
                "the manifest names `{entrypoint}` as the entrypoint, but that file is not there"
            )));
        }
    }
    Ok(manifest)
}

/// Everything reading a package tells us, without extracting it.
#[derive(Debug)]
pub struct PackageContents {
    pub manifest: PluginManifest,
    /// Where this plugin says its updates come from, if it says at all.
    ///
    /// Optional on purpose. A plugin that is only ever installed by hand has
    /// nothing to declare, and requiring a distribution block would make the
    /// simplest case carry the most complicated file.
    pub update_source: Option<UpdateSource>,
    /// Digest of the manifest's entrypoint, so a preview can tell whether this
    /// is code the user already answered for.
    pub code_digest: Option<String>,
    pub file_count: usize,
    pub uncompressed_bytes: u64,
}

/// Read a package file into memory, once.
///
/// Every later step (reading the manifest, extracting) works from these bytes
/// rather than re-opening the path, so the file cannot change between the check
/// and the use. A package larger than the expanded-size limit cannot be valid,
/// so it is refused before it is read.
pub fn read_package_file(archive: &Path) -> LauncherResult<Vec<u8>> {
    let length = std::fs::metadata(archive)
        .map_err(|e| io_error(format!("could not open {}: {e}", archive.display())))?
        .len();
    if length > MAX_TOTAL_BYTES {
        return Err(invalid_package(format!(
            "the package file is {length} bytes; a plugin package may not exceed {MAX_TOTAL_BYTES}"
        )));
    }
    std::fs::read(archive)
        .map_err(|e| io_error(format!("could not read {}: {e}", archive.display())))
}

/// Read and validate a package without extracting it.
pub fn read_package_manifest(archive: &Path) -> LauncherResult<PackageContents> {
    read_package_manifest_bytes(&read_package_file(archive)?)
}

/// [`read_package_manifest`] over bytes already in memory.
pub fn read_package_manifest_bytes(bytes: &[u8]) -> LauncherResult<PackageContents> {
    let compressed = (bytes.len() as u64).max(1);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| invalid_package(format!("not a readable plugin package: {e}")))?;

    if zip.len() > MAX_ENTRIES {
        return Err(invalid_package(format!(
            "the package contains {} files; the limit is {MAX_ENTRIES}",
            zip.len()
        )));
    }

    let mut total = 0u64;
    let mut manifest_text: Option<String> = None;
    let mut source_text: Option<String> = None;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| invalid_package(format!("unreadable entry in the package: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| invalid_package(format!("unreadable entry in the package: {e}")))?
            .into_owned();
        if entry.is_dir() {
            continue;
        }
        validate_entry_path(&name)?;
        let size = entry.size();
        if size > MAX_ENTRY_BYTES {
            return Err(invalid_package(format!(
                "`{name}` expands to {size} bytes; the per-file limit is {MAX_ENTRY_BYTES}"
            )));
        }
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err(invalid_package(format!(
                "the package expands past {MAX_TOTAL_BYTES} bytes"
            )));
        }
        if name == MANIFEST_FILENAME {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| invalid_package(format!("could not read {MANIFEST_FILENAME}: {e}")))?;
            manifest_text = Some(text);
        } else if name == UPDATE_SOURCE_FILENAME {
            let mut text = String::new();
            entry.read_to_string(&mut text).map_err(|e| {
                invalid_package(format!("could not read {UPDATE_SOURCE_FILENAME}: {e}"))
            })?;
            source_text = Some(text);
        }
    }

    // Checked after the walk so the ratio is against the real expanded size,
    // not the declared one for a single entry.
    if total / compressed > MAX_EXPANSION_RATIO {
        return Err(invalid_package(format!(
            "the package expands {}x, past the {MAX_EXPANSION_RATIO}x limit",
            total / compressed
        )));
    }

    let Some(text) = manifest_text else {
        return Err(invalid_package(format!(
            "the package has no {MANIFEST_FILENAME} at its root"
        )));
    };
    let manifest = PluginManifest::parse(&text).map_err(|e| invalid_package(e.message))?;

    // The digest covers every file the host could load as a module, not only
    // the entrypoint: `import "./util.js"` is resolved inside the package, so
    // changing util.js must count as changing the code. The manifest and update
    // source are host metadata and are never loaded as modules, so they are
    // left out (their changes are handled by their own rules).
    let code_digest = if let Some(entrypoint) = &manifest.entrypoint {
        use sha2::{Digest, Sha256};
        let mut files = std::collections::BTreeMap::new();
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|e| invalid_package(format!("unreadable entry in the package: {e}")))?;
            if entry.is_dir() {
                continue;
            }
            let name =
                normalize_relative(&entry.name().map_err(|e| {
                    invalid_package(format!("unreadable entry in the package: {e}"))
                })?);
            if name == MANIFEST_FILENAME || name == UPDATE_SOURCE_FILENAME {
                continue;
            }
            let mut hasher = Sha256::new();
            std::io::copy(&mut entry, &mut hasher)
                .map_err(|e| invalid_package(format!("could not read `{name}`: {e}")))?;
            files.insert(name, hex::encode(hasher.finalize()));
        }
        let entrypoint = normalize_relative(entrypoint);
        if !files.contains_key(&entrypoint) {
            return Err(invalid_package(format!(
                "the manifest names `{entrypoint}` as the entrypoint, but the package does not contain it"
            )));
        }
        Some(finish_script_digest(&files))
    } else {
        None
    };

    // A malformed distribution block fails the install rather than being
    // ignored. Silently dropping it would leave a plugin that looks like it
    // has updates, has none, and gives nobody a reason why.
    let update_source = match source_text {
        Some(text) => Some(UpdateSource::parse(&text).map_err(|e| invalid_package(e.message))?),
        None => None,
    };

    Ok(PackageContents {
        manifest,
        update_source,
        code_digest,
        file_count: zip.len(),
        uncompressed_bytes: total,
    })
}

/// Describe what installing a package would do.
pub fn preview_package(
    archive: &Path,
    existing: Option<&ExistingInstall<'_>>,
) -> LauncherResult<InstallPreview> {
    preview_package_bytes(&read_package_file(archive)?, existing)
}

/// [`preview_package`] over bytes already in memory.
pub fn preview_package_bytes(
    bytes: &[u8],
    existing: Option<&ExistingInstall<'_>>,
) -> LauncherResult<InstallPreview> {
    let contents = read_package_manifest_bytes(bytes)?;
    Ok(build_preview(
        contents.manifest,
        contents.update_source,
        contents.code_digest,
        existing,
        contents.file_count,
        contents.uncompressed_bytes,
    ))
}

/// Describe what loading a development folder would do.
pub fn preview_folder(
    folder: &Path,
    existing: Option<&ExistingInstall<'_>>,
) -> LauncherResult<InstallPreview> {
    let manifest = read_folder_manifest(folder)?;
    let code_digest = script_digest(&manifest, folder)?;
    // A development folder is never enrolled for updates: the author is the
    // one editing it, and an update that overwrote their working copy would be
    // the opposite of helpful.
    Ok(build_preview(manifest, None, code_digest, existing, 0, 0))
}

fn build_preview(
    manifest: PluginManifest,
    update_source: Option<UpdateSource>,
    new_code_digest: Option<String>,
    existing: Option<&ExistingInstall<'_>>,
    file_count: usize,
    uncompressed_bytes: u64,
) -> InstallPreview {
    let (required, optional, unsupported) = describe(&manifest.capabilities);
    let runs_code = !manifest.is_declarative_only();
    let publisher_changed = publisher_changed(
        existing.and_then(|current| current.trusted_keys),
        update_source.as_ref(),
    );
    // New code, changed code, or code whose publisher changed: the answer the
    // user gave (if any) does not cover it, so the prompt asks again.
    let code_changed = runs_code
        && (publisher_changed
            || existing.is_none_or(|current| {
                !current.runs_code || current.code_hash != new_code_digest.as_deref()
            }));
    InstallPreview {
        runs_code,
        code_changed,
        adds_code: runs_code && !existing.is_some_and(|e| e.runs_code),
        replaces_version: existing.map(|current| current.version.to_string()),
        added_capabilities: added_capabilities(existing.map(|current| current.granted), &manifest),
        added_hosts: added_hosts(existing.map(|current| current.hosts), &manifest),
        publisher_changed,
        update_source: update_source.as_ref().map(UpdateSourceSummary::of),
        // Only a *change* is a migration. A first install has nothing to
        // migrate, and a reinstall of the same data version does not either.
        migrates_data: existing
            .is_some_and(|current| current.data_version != manifest.data_version),
        required_capabilities: required,
        optional_capabilities: optional,
        unsupported_capabilities: unsupported,
        needs_reduced_security: manifest.network.is_elevated(),
        file_count,
        uncompressed_bytes,
        manifest,
    }
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// Unpack a validated package into `destination`.
///
/// `destination` must not exist. Extraction goes to a sibling staging
/// directory and is renamed into place at the end, so an interrupted install
/// leaves either the old plugin or nothing — never a partial one.
pub fn extract_package(bytes: &[u8], destination: &Path) -> LauncherResult<()> {
    if destination.exists() {
        return Err(io_error(format!(
            "{} already exists",
            destination.display()
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| io_error("the plugin destination has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| io_error(format!("could not create {}: {e}", parent.display())))?;

    let staging = parent.join(format!(
        ".staging-{}",
        destination
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "plugin".into())
    ));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging)
        .map_err(|e| io_error(format!("could not create {}: {e}", staging.display())))?;

    let result = extract_into(bytes, &staging);
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    std::fs::rename(&staging, destination).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        io_error(format!(
            "could not move the plugin into {}: {e}",
            destination.display()
        ))
    })
}

fn extract_into(bytes: &[u8], staging: &Path) -> LauncherResult<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| invalid_package(format!("not a readable plugin package: {e}")))?;

    let mut written = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| invalid_package(format!("unreadable entry in the package: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| invalid_package(format!("unreadable entry in the package: {e}")))?
            .into_owned();
        if entry.is_dir() {
            continue;
        }
        // Re-validated here: this is the pass that actually creates files.
        validate_entry_path(&name)?;
        let target = staging.join(name.replace('\\', "/"));
        if !target.starts_with(staging) {
            return Err(invalid_package(format!("`{name}` escapes the package")));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io_error(format!("could not create {}: {e}", parent.display())))?;
        }
        written = written.saturating_add(entry.size());
        if written > MAX_TOTAL_BYTES {
            return Err(invalid_package("the package expands past its size limit"));
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|e| io_error(format!("could not write {}: {e}", target.display())))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| io_error(format!("could not write {}: {e}", target.display())))?;
    }
    Ok(())
}

/// Reject any archive entry that is not a plain relative path.
///
/// Separate from the manifest's own path check because this runs over every
/// file in the archive, including ones the manifest never mentions.
pub fn validate_entry_path(name: &str) -> LauncherResult<()> {
    if name.is_empty() {
        return Err(invalid_package("the package contains an unnamed entry"));
    }
    if name.contains('\0') {
        return Err(invalid_package("a package entry name contains a NUL byte"));
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(invalid_package(format!("`{name}` is an absolute path")));
    }
    if normalized.len() >= 2 && normalized.as_bytes()[1] == b':' {
        return Err(invalid_package(format!("`{name}` is an absolute path")));
    }
    for component in Path::new(&normalized).components() {
        match component {
            Component::ParentDir => {
                return Err(invalid_package(format!(
                    "`{name}` climbs out of the package"
                )))
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid_package(format!("`{name}` is an absolute path")))
            }
            Component::Normal(part) => {
                if part.to_string_lossy().contains(':') {
                    return Err(invalid_package(format!(
                        "`{name}` contains a `:` in a path segment"
                    )));
                }
            }
            Component::CurDir => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// Move an installed plugin's files aside so a failed replacement can undo.
pub fn stash_for_rollback(install_dir: &Path, rollback_dir: &Path) -> LauncherResult<()> {
    if rollback_dir.exists() {
        std::fs::remove_dir_all(rollback_dir)
            .map_err(|e| io_error(format!("could not clear {}: {e}", rollback_dir.display())))?;
    }
    if let Some(parent) = rollback_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| io_error(format!("could not create {}: {e}", parent.display())))?;
    }
    std::fs::rename(install_dir, rollback_dir)
        .map_err(|e| io_error(format!("could not set aside the previous version: {e}")))
}

/// Put stashed files back after a failed replacement.
pub fn restore_rollback(rollback_dir: &Path, install_dir: &Path) -> LauncherResult<()> {
    if install_dir.exists() {
        let _ = std::fs::remove_dir_all(install_dir);
    }
    std::fs::rename(rollback_dir, install_dir)
        .map_err(|e| io_error(format!("could not restore the previous version: {e}")))
}

/// Finish or undo an install that a crash interrupted.
///
/// An install moves the previous version into the rollback folder, unpacks the
/// new one, then writes the record. If the process stops in between, the old
/// files are stranded in the rollback folder and the record still points at a
/// directory that may be missing, so the plugin (and its game) disappears.
///
/// Runs before the records are read. For each stashed folder: if the record
/// already names the version that is on disk, the install finished and the
/// stashed copy is stale; otherwise the previous version is put back. Both
/// outcomes remove the rollback folder, so running it again does nothing.
pub fn recover_interrupted_installs(
    conn: &rusqlite::Connection,
    paths: &crate::app_paths::AppPaths,
) -> LauncherResult<Vec<String>> {
    let mut warnings = Vec::new();
    let rollback_root = paths.plugin_rollback_root();
    let read = match std::fs::read_dir(&rollback_root) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(warnings),
        Err(error) => {
            return Err(io_error(format!(
                "could not read {}: {error}",
                rollback_root.display()
            )))
        }
    };
    for item in read {
        let item =
            item.map_err(|e| io_error(format!("could not read {}: {e}", rollback_root.display())))?;
        if !item.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = item.file_name();
        let Some(id) = name.to_str().and_then(|name| PluginId::parse(name).ok()) else {
            continue;
        };
        let rollback = rollback_root.join(id.as_str());
        let destination = paths.plugin_packages_root().join(id.as_str());
        let record = store::get(conn, &id)?;
        let finished = record.as_ref().is_some_and(|record| {
            !store::is_tombstone(record)
                && installed_version(&destination)
                    .is_some_and(|version| version == record.manifest.version.to_string())
        });
        if finished {
            // The new version was recorded; the stashed copy did not need to
            // come back.
            std::fs::remove_dir_all(&rollback)
                .map_err(|e| io_error(format!("could not clear {}: {e}", rollback.display())))?;
            continue;
        }
        let restoring = installed_version(&rollback);
        restore_rollback(&rollback, &destination)?;
        warnings.push(format!(
            "Recovered plugin {} from an interrupted install; the previous version{} is back.",
            id,
            restoring
                .map(|version| format!(" ({version})"))
                .unwrap_or_default()
        ));
    }
    Ok(warnings)
}

/// The version a folder's manifest names, if it has a readable one.
fn installed_version(folder: &Path) -> Option<String> {
    let text = std::fs::read_to_string(folder.join(MANIFEST_FILENAME)).ok()?;
    PluginManifest::parse(&text)
        .ok()
        .map(|manifest| manifest.version.to_string())
}

/// The directory a package version is installed into.
pub fn package_dir(packages_root: &Path, plugin_id: &agora_plugin_api::PluginId) -> PathBuf {
    // `PluginId` is already constrained to lowercase ASCII, digits, hyphens
    // and one dot, so it is a safe single path component by construction.
    packages_root.join(plugin_id.as_str())
}

/// Whether an update changes the publisher key it is signed by.
///
/// The keys are pinned when the plugin is first installed. If a later package
/// under the same id names different keys, the person has to be told: the new
/// key has not been vouched for by anyone they trusted.
pub fn publisher_changed(
    previous: Option<&[agora_plugin_api::distribution::PublicKey]>,
    source: Option<&UpdateSource>,
) -> bool {
    use std::collections::BTreeSet;
    let (Some(previous), Some(source)) = (previous, source) else {
        return false;
    };
    let keys = |keys: &[agora_plugin_api::distribution::PublicKey]| -> BTreeSet<(String, String)> {
        keys.iter()
            .map(|key| (key.id.clone(), key.fingerprint()))
            .collect()
    };
    let previous = keys(previous);
    let next = keys(&source.keys);
    !previous.is_empty() && !next.is_empty() && previous != next
}

/// A digest of the code the manifest names, for tying script consent to it.
///
/// The entrypoint is the file Agora starts, so it identifies the code the user
/// allowed. A version that brings a different entrypoint file is new code and
/// must be asked about again.
pub fn script_digest(
    manifest: &PluginManifest,
    plugin_dir: &Path,
) -> LauncherResult<Option<String>> {
    let Some(entrypoint) = &manifest.entrypoint else {
        return Ok(None);
    };
    // The host's module loader reads and parses any file inside the package
    // that a module names, not just `.js` files. So every file except the two
    // host metadata files is part of the code identity; excluding data by
    // extension would leave a way to smuggle a module behind an unchanged
    // entrypoint.
    let mut files = std::collections::BTreeMap::new();
    let mut entries = 0usize;
    let mut total = 0u64;
    collect_script_files(plugin_dir, plugin_dir, &mut files, &mut entries, &mut total)?;
    let entrypoint = normalize_relative(entrypoint);
    if !files.contains_key(&entrypoint) {
        return Err(io_error(format!(
            "the plugin entrypoint `{entrypoint}` is missing"
        )));
    }
    Ok(Some(finish_script_digest(&files)))
}

fn collect_script_files(
    root: &Path,
    dir: &Path,
    files: &mut std::collections::BTreeMap<String, String>,
    entries: &mut usize,
    total: &mut u64,
) -> LauncherResult<()> {
    use sha2::{Digest, Sha256};
    let read = std::fs::read_dir(dir)
        .map_err(|e| io_error(format!("could not read {}: {e}", dir.display())))?;
    for item in read {
        let item = item.map_err(|e| io_error(format!("could not read {}: {e}", dir.display())))?;
        let path = item.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|_| io_error("a plugin file is outside its folder"))?
            .to_string_lossy()
            .replace('\\', "/");
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| io_error(format!("could not inspect {}: {e}", path.display())))?;
        if meta.is_dir() {
            collect_script_files(root, &path, files, entries, total)?;
            continue;
        }
        if rel == MANIFEST_FILENAME || rel == UPDATE_SOURCE_FILENAME {
            continue;
        }
        *entries += 1;
        if *entries > MAX_ENTRIES {
            return Err(io_error(
                "the plugin folder holds too many files to fingerprint",
            ));
        }
        if meta.is_file() {
            *total = total.saturating_add(meta.len());
            if *total > MAX_TOTAL_BYTES {
                return Err(io_error("the plugin folder is too large to fingerprint"));
            }
            let bytes = std::fs::read(&path)
                .map_err(|e| io_error(format!("could not read {}: {e}", path.display())))?;
            files.insert(rel, hex::encode(Sha256::digest(&bytes)));
        } else {
            // A link or device is not read as code here, but adding or removing
            // one must still change the identity. The loader rejects links out
            // of the package; a link inside it resolves to a file hashed above.
            files.insert(rel, "other".into());
        }
    }
    Ok(())
}

/// Collapse `.` and empty segments, so the same file has one name whether it
/// came from a zip entry or from walking a folder.
fn normalize_relative(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// A stable digest over the file map: relative paths and content hashes, sorted.
fn finish_script_digest(files: &std::collections::BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (rel, hash) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Resolve which capabilities a fresh install should be granted.
pub fn grant_for(manifest: &PluginManifest) -> LauncherResult<CapabilitySet> {
    CapabilitySet::resolve(&manifest.capabilities).map_err(|e| LauncherError::Generic {
        code: "ERR_PLUGIN_INCOMPATIBLE".into(),
        message: e.message,
    })
}

/// Whether an upgrade asks for more than the user previously granted.
///
/// A plugin update that wants a new capability does not get it silently; the
/// caller must re-prompt. This is the whole reason grants are stored rather
/// than re-read from the manifest.
pub fn widens_capabilities(granted: &CapabilitySet, manifest: &PluginManifest) -> Vec<String> {
    manifest
        .capabilities
        .required
        .iter()
        .chain(manifest.capabilities.optional.iter())
        .filter_map(|name| name.parse::<agora_plugin_api::Capability>().ok())
        .filter(|cap| !granted.contains(*cap))
        .map(|cap| cap.as_str().to_string())
        .collect()
}

/// Remove an installed package's files. Development folders are never touched.
pub fn remove_package_files(source: &PluginSource, install_dir: &Path) -> LauncherResult<()> {
    if source.is_development() {
        // The user's own working copy. Removing the plugin from Agora must not
        // remove the code they are writing.
        return Ok(());
    }
    if install_dir.exists() {
        std::fs::remove_dir_all(install_dir)
            .map_err(|e| io_error(format!("could not remove {}: {e}", install_dir.display())))?;
    }
    Ok(())
}

/// Re-export so callers do not have to reach into `store` for this one type.
pub use super::store::PluginSource as Source;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn manifest_json(id: &str) -> String {
        serde_json::json!({
            "manifest": 1,
            "id": id,
            "name": "Test",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
        })
        .to_string()
    }

    fn write_package(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.zip");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        // Stored rather than deflated so the expansion-ratio guard does not
        // trip on tiny highly-compressible fixtures.
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        (dir, path)
    }

    #[test]
    fn reads_a_well_formed_package() {
        let (_dir, path) = write_package(&[
            (MANIFEST_FILENAME, manifest_json("acme.one").as_bytes()),
            ("main.js", b"export function run() {}"),
        ]);
        let contents = read_package_manifest(&path).unwrap();
        let (manifest, count, bytes) = (
            contents.manifest,
            contents.file_count,
            contents.uncompressed_bytes,
        );
        assert_eq!(manifest.id.as_str(), "acme.one");
        assert_eq!(count, 2);
        assert!(bytes > 0);
    }

    #[test]
    fn a_package_without_a_manifest_is_rejected() {
        let (_dir, path) = write_package(&[("main.js", b"export function run() {}")]);
        let err = read_package_manifest(&path).unwrap_err();
        assert!(err.to_string().contains(MANIFEST_FILENAME), "{err}");
    }

    #[test]
    fn a_package_whose_entrypoint_is_missing_is_rejected_before_install() {
        let (_dir, path) =
            write_package(&[(MANIFEST_FILENAME, manifest_json("acme.one").as_bytes())]);
        let err = read_package_manifest(&path).unwrap_err();
        assert!(err.to_string().contains("does not contain it"), "{err}");
    }

    #[test]
    fn an_entry_that_climbs_out_of_the_package_is_rejected() {
        let (_dir, path) = write_package(&[
            (MANIFEST_FILENAME, manifest_json("acme.one").as_bytes()),
            ("main.js", b"x"),
            ("../escape.js", b"x"),
        ]);
        let err = read_package_manifest(&path).unwrap_err();
        assert!(err.to_string().contains("climbs out"), "{err}");
    }

    #[test]
    fn absolute_and_traversing_entry_paths_are_all_refused() {
        for bad in [
            "/etc/passwd",
            "C:/windows/system32/evil.dll",
            "..",
            "a/../../b.js",
            "a/b:stream.js",
        ] {
            assert!(
                validate_entry_path(bad).is_err(),
                "`{bad}` should have been rejected"
            );
        }
    }

    #[test]
    fn ordinary_relative_entry_paths_are_accepted() {
        for good in ["main.js", "dist/main.js", "assets/icon.png", "./main.js"] {
            assert!(
                validate_entry_path(good).is_ok(),
                "`{good}` should have been accepted"
            );
        }
    }

    #[test]
    fn extraction_lands_every_file_inside_the_destination() {
        let (_dir, path) = write_package(&[
            (MANIFEST_FILENAME, manifest_json("acme.one").as_bytes()),
            ("main.js", b"export function run() {}"),
            ("lib/util.js", b"export const x = 1;"),
        ]);
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("acme.one");
        extract_package(&std::fs::read(&path).unwrap(), &destination).unwrap();
        assert!(destination.join(MANIFEST_FILENAME).is_file());
        assert!(destination.join("main.js").is_file());
        assert!(destination.join("lib/util.js").is_file());
    }

    #[test]
    fn extraction_refuses_to_overwrite_an_existing_directory() {
        let (_dir, path) = write_package(&[
            (MANIFEST_FILENAME, manifest_json("acme.one").as_bytes()),
            ("main.js", b"x"),
        ]);
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("acme.one");
        std::fs::create_dir_all(&destination).unwrap();
        assert!(extract_package(&std::fs::read(&path).unwrap(), &destination).is_err());
    }

    #[test]
    fn a_failed_extraction_leaves_no_partial_directory_behind() {
        let (_dir, path) = write_package(&[
            (MANIFEST_FILENAME, manifest_json("acme.one").as_bytes()),
            ("main.js", b"x"),
            ("../escape.js", b"x"),
        ]);
        let target = tempfile::tempdir().unwrap();
        let destination = target.path().join("acme.one");
        assert!(extract_package(&std::fs::read(&path).unwrap(), &destination).is_err());
        assert!(!destination.exists());
        // The staging directory goes too.
        let leftovers: Vec<_> = std::fs::read_dir(target.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_development_folder_is_read_in_place() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            manifest_json("acme.dev"),
        )
        .unwrap();
        std::fs::write(dir.path().join("main.js"), "export function run() {}").unwrap();
        let manifest = read_folder_manifest(dir.path()).unwrap();
        assert_eq!(manifest.id.as_str(), "acme.dev");
    }

    #[test]
    fn a_development_folder_missing_its_entrypoint_says_which_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(MANIFEST_FILENAME),
            manifest_json("acme.dev"),
        )
        .unwrap();
        let err = read_folder_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("main.js"), "{err}");
    }

    #[test]
    fn removing_a_development_plugin_never_deletes_the_authors_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.js"), "x").unwrap();
        let source = PluginSource::Development {
            path: dir.path().to_path_buf(),
        };
        remove_package_files(&source, dir.path()).unwrap();
        assert!(dir.path().join("main.js").is_file());
    }

    #[test]
    fn removing_an_installed_package_deletes_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("acme.one");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("main.js"), "x").unwrap();
        remove_package_files(&PluginSource::Package, &install).unwrap();
        assert!(!install.exists());
    }

    #[test]
    fn an_update_asking_for_a_new_capability_is_reported_as_widening() {
        let granted =
            CapabilitySet::from_capabilities([agora_plugin_api::Capability::InstanceRead]);
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "manifest": 1,
            "id": "acme.one",
            "name": "Test",
            "version": "2.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
            "capabilities": { "required": ["instance:read", "content:write"] }
        }))
        .unwrap();
        assert_eq!(
            widens_capabilities(&granted, &manifest),
            vec!["content:write"]
        );
    }

    #[test]
    fn an_update_asking_for_no_more_than_before_does_not_need_a_new_prompt() {
        let granted = CapabilitySet::from_capabilities([
            agora_plugin_api::Capability::InstanceRead,
            agora_plugin_api::Capability::ContentRead,
        ]);
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "manifest": 1,
            "id": "acme.one",
            "name": "Test",
            "version": "2.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
            "capabilities": { "required": ["instance:read"] }
        }))
        .unwrap();
        assert!(widens_capabilities(&granted, &manifest).is_empty());
    }

    #[test]
    fn a_first_install_is_not_a_data_migration() {
        let manifest: PluginManifest = serde_json::from_str(&manifest_json("acme.one")).unwrap();
        let preview = build_preview(manifest, None, None, None, 2, 100);
        assert!(!preview.migrates_data);
        assert!(preview.replaces_version.is_none());
    }

    #[test]
    fn an_upgrade_that_changes_the_data_version_is_flagged_as_migrating() {
        let mut value: serde_json::Value =
            serde_json::from_str(&manifest_json("acme.one")).unwrap();
        value["dataVersion"] = serde_json::json!(2);
        let manifest: PluginManifest = serde_json::from_value(value).unwrap();
        let installed = semver::Version::new(1, 0, 0);
        let granted = CapabilitySet::resolve(&CapabilityRequest::default()).unwrap();
        let preview = build_preview(
            manifest,
            None,
            None,
            Some(&ExistingInstall {
                version: &installed,
                data_version: 1,
                granted: &granted,
                hosts: &[],
                runs_code: false,
                trusted_keys: None,
                code_hash: None,
            }),
            2,
            100,
        );
        assert!(preview.migrates_data);
        assert_eq!(preview.replaces_version.as_deref(), Some("1.0.0"));
    }

    /// The point of storing the grant: an update that wants more than the
    /// user agreed to has to ask again, and one that wants the same or less
    /// must not.
    #[test]
    fn only_a_widening_update_asks_for_consent_again() {
        let granted = CapabilitySet::resolve(&CapabilityRequest {
            required: vec!["instance:read".into()],
            optional: vec![],
        })
        .unwrap();
        let installed = semver::Version::new(1, 0, 0);
        let existing = ExistingInstall {
            version: &installed,
            data_version: 1,
            granted: &granted,
            hosts: &[],
            runs_code: false,
            trusted_keys: None,
            code_hash: None,
        };

        let same: PluginManifest = serde_json::from_value(serde_json::json!({
            "manifest": 1,
            "id": "acme.one",
            "name": "One",
            "version": "2.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
            "capabilities": { "required": ["instance:read"] }
        }))
        .unwrap();
        let preview = build_preview(same, None, None, Some(&existing), 2, 100);
        assert!(
            !preview.requires_capability_consent(),
            "re-granting what is already granted is not a new decision"
        );

        let wider: PluginManifest = serde_json::from_value(serde_json::json!({
            "manifest": 1,
            "id": "acme.one",
            "name": "One",
            "version": "3.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
            "capabilities": { "required": ["instance:read", "content:write"] }
        }))
        .unwrap();
        let preview = build_preview(wider, None, None, Some(&existing), 2, 100);
        assert_eq!(
            preview.added_capabilities,
            vec!["content:write".to_string()]
        );
        assert!(preview.requires_capability_consent());
    }

    /// A first install has no grant to compare against, so everything it asks
    /// for is new.
    #[test]
    fn a_first_install_counts_every_requested_capability_as_added() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "manifest": 1,
            "id": "acme.one",
            "name": "One",
            "version": "1.0.0",
            "license": "MIT",
            "apiRange": ">=0.1, <0.2",
            "entrypoint": "main.js",
            "capabilities": { "required": ["instance:read"], "optional": ["content:read"] }
        }))
        .unwrap();
        let preview = build_preview(manifest, None, None, None, 2, 100);
        assert_eq!(
            preview.added_capabilities,
            vec!["instance:read".to_string(), "content:read".to_string()]
        );
        assert!(preview.requires_capability_consent());
    }
}
