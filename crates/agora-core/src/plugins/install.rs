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
    /// Official dependencies this install will write too, dependencies first.
    ///
    /// Set on the target's preview in a `needsConsent` reply so the player can
    /// see everything one consent covers. Empty for a single package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_installs: Vec<InstallPreview>,
    /// This package comes from the signed official catalog. Set by the official
    /// install paths so the prompt can recommend allowing its code; a community
    /// package is never marked here.
    #[serde(default)]
    pub official: bool,
    /// Identifies the bytes this preview described: SHA-256 of the package, or
    /// of the folder's manifest and files for a development folder.
    ///
    /// The install prompt passes it back, and the installer refuses a package
    /// that no longer matches, so a yes given for one package cannot be used
    /// for a different one that replaced it between preview and install.
    #[serde(default)]
    pub package_digest: String,
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
        package_digest(bytes),
    ))
}

/// Describe what loading a development folder would do.
pub fn preview_folder(
    folder: &Path,
    existing: Option<&ExistingInstall<'_>>,
) -> LauncherResult<InstallPreview> {
    let manifest = read_folder_manifest(folder)?;
    let code_digest = script_digest(&manifest, folder)?;
    let digest = folder_digest(folder)?;
    // A development folder is never enrolled for updates: the author is the
    // one editing it, and an update that overwrote their working copy would be
    // the opposite of helpful.
    Ok(build_preview(
        manifest,
        None,
        code_digest,
        existing,
        0,
        0,
        digest,
    ))
}

fn build_preview(
    manifest: PluginManifest,
    update_source: Option<UpdateSource>,
    new_code_digest: Option<String>,
    existing: Option<&ExistingInstall<'_>>,
    file_count: usize,
    uncompressed_bytes: u64,
    package_digest: String,
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
        also_installs: Vec::new(),
        official: false,
        package_digest,
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

// ---------------------------------------------------------------------------
// The install journal (MASTER_SPEC §32.11)
// ---------------------------------------------------------------------------

/// Where an install's journal lives: in the rollback root, beside the folder it may put back, so a
/// stashed copy and the journal that explains it are found together.
pub fn install_journal_path(paths: &crate::app_paths::AppPaths, id: &PluginId) -> PathBuf {
    paths
        .plugin_rollback_root()
        .join(format!("{}.json", id.as_str()))
}

/// Where the marker that proves a stashed folder is this operation's lives.
///
/// Beside the folder, never inside it: the stashed copy is a whole installed plugin, and a file
/// added to it would come back into the package folder on a restore and change what
/// `folder_digest` and the official plugins' tree digest see.
pub fn install_marker_path(paths: &crate::app_paths::AppPaths, id: &PluginId) -> PathBuf {
    paths
        .plugin_rollback_root()
        .join(format!("{}.owner.json", id.as_str()))
}

/// The states an install journal can be in.
///
/// This kind's own list: an install never inherits a launch's or a file swap's states.
/// [`crate::recovery::JournalState`] ties the list to
/// [`OperationKind::PluginInstall`](crate::recovery::OperationKind::PluginInstall), so a journal
/// carrying another kind's state is not read as this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginInstallState {
    /// The journal is written; nothing has moved yet. The previous version, if there was one, is
    /// still in the package folder.
    Starting,
    /// The previous version is in the rollback folder; the new one is not unpacked yet.
    Stashed,
    /// The new version's files are in place; the record is not written yet.
    Extracted,
}

impl crate::recovery::JournalState for PluginInstallState {
    const KIND: crate::recovery::OperationKind = crate::recovery::OperationKind::PluginInstall;
}

/// An install's crash journal: the envelope every operation shares, plus what an install needs to
/// say about itself (MASTER_SPEC §32.11).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstallJournal {
    /// What any recovery decision needs: the format, the operation id, the lock, the writer, the
    /// owned paths and their proofs, and the state. Flattened, so the envelope's fields are the
    /// file's own.
    #[serde(flatten)]
    pub envelope: crate::recovery::Envelope<PluginInstallState>,
    /// The plugin being installed.
    pub plugin_id: String,
    /// Whether a previous version was moved aside, and therefore has to come back if the install
    /// does not finish.
    pub had_files: bool,
    /// The version that was moved aside, when its manifest could be read. For the message only:
    /// ownership of the stashed folder is proven by its marker, never by this field.
    pub previous_version: Option<String>,
    /// The version being installed.
    pub new_version: String,
}

impl InstallJournal {
    /// The journal for an install that is about to start.
    ///
    /// Owned: the rollback folder, proven by a marker file this operation writes before it moves
    /// anything, and the package folder, proven by its own manifest naming this plugin (see
    /// [`PackageFolderProof`]).
    pub fn begin(
        paths: &crate::app_paths::AppPaths,
        id: &PluginId,
        had_files: bool,
        previous_version: Option<String>,
        new_version: String,
    ) -> LauncherResult<Self> {
        let envelope = crate::recovery::Envelope::new(
            &crate::lock_manager::LockResource::PluginInstall,
            PluginInstallState::Starting,
            vec![
                crate::recovery::OwnedPath {
                    path: paths.plugin_rollback_root().join(id.as_str()),
                    proof: crate::recovery::PathProof::Marker {
                        at: install_marker_path(paths, id),
                    },
                },
                crate::recovery::OwnedPath {
                    path: package_dir(&paths.plugin_packages_root(), id),
                    proof: crate::recovery::PathProof::Content,
                },
            ],
        )?;
        Ok(InstallJournal {
            envelope,
            plugin_id: id.as_str().to_string(),
            had_files,
            previous_version,
            new_version,
        })
    }

    /// Write the journal, then the marker that proves the stashed folder is this operation's.
    ///
    /// Called before anything moves: a crash after this point is recoverable.
    pub fn start(&self, paths: &crate::app_paths::AppPaths) -> LauncherResult<()> {
        crate::recovery::write(&install_journal_path(paths, &self.id()?), self)?;
        if self.had_files {
            crate::recovery::write_marker(
                &install_marker_path(paths, &self.id()?),
                &self.envelope.operation_id,
            )?;
        }
        Ok(())
    }

    /// Record where the install has got to, before it takes the next step.
    ///
    /// A state that is one step behind reality is safe — recovery decides from the record and what
    /// is on disk as well, and every outcome below is still right — so a failed write is reported
    /// and does not undo anything.
    pub fn advance(
        &mut self,
        paths: &crate::app_paths::AppPaths,
        state: PluginInstallState,
    ) -> LauncherResult<()> {
        self.envelope.state = state;
        crate::recovery::write(&install_journal_path(paths, &self.id()?), self)
    }

    /// Delete the journal and its marker: the install is over, however it ended.
    pub fn discard(&self, paths: &crate::app_paths::AppPaths) -> LauncherResult<()> {
        let id = self.id()?;
        remove_if_present(&install_journal_path(paths, &id))?;
        remove_if_present(&install_marker_path(paths, &id))
    }

    fn id(&self) -> LauncherResult<PluginId> {
        PluginId::parse(&self.plugin_id).map_err(|error| {
            io_error(format!(
                "the install journal names an invalid plugin id: {}",
                error.message
            ))
        })
    }
}

/// Remove a file if it is there; a file that is there and cannot be removed is an error.
fn remove_if_present(path: &Path) -> LauncherResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(format!(
            "could not remove {}: {error}",
            path.display()
        ))),
    }
}

/// What a folder's own manifest says.
///
/// Three answers, because they lead to different decisions: a readable manifest says which plugin
/// and version the folder holds, an absent folder has nothing to touch, and a folder whose
/// manifest cannot be read proves nothing at all — it is never "no manifest", which is what turned
/// a readable-file failure into "the install did not finish".
enum FolderManifest {
    Readable { id: String, version: String },
    Absent,
    Unreadable(String),
}

fn folder_manifest(folder: &Path) -> FolderManifest {
    let text = match std::fs::read_to_string(folder.join(MANIFEST_FILENAME)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(folder) {
                Ok(_) => FolderManifest::Unreadable(format!(
                    "{} holds no {MANIFEST_FILENAME}",
                    folder.display()
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    FolderManifest::Absent
                }
                Err(error) => FolderManifest::Unreadable(format!(
                    "{} could not be examined: {error}",
                    folder.display()
                )),
            }
        }
        Err(error) => {
            return FolderManifest::Unreadable(format!(
                "{}/{MANIFEST_FILENAME} could not be read: {error}",
                folder.display()
            ))
        }
    };
    match PluginManifest::parse(&text) {
        Ok(manifest) => FolderManifest::Readable {
            id: manifest.id.as_str().to_string(),
            version: manifest.version.to_string(),
        },
        Err(error) => FolderManifest::Unreadable(format!(
            "{}/{MANIFEST_FILENAME} is not a manifest: {}",
            folder.display(),
            error.message
        )),
    }
}

/// The version a folder's manifest names, when it can be read.
pub fn readable_version(folder: &Path) -> Option<String> {
    match folder_manifest(folder) {
        FolderManifest::Readable { version, .. } => Some(version),
        _ => None,
    }
}

/// The proof of the package folder: it holds this plugin.
///
/// A folder holding another plugin's files is not this install's to replace, and a folder whose
/// manifest cannot be read proves nothing either way. An absent folder has nothing to touch, so it
/// is proven by being absent.
struct PackageFolderProof {
    plugin_id: String,
    destination: PathBuf,
}

impl crate::recovery::ContentProof for PackageFolderProof {
    fn prove(&self, path: &Path) -> crate::recovery::ProofResult {
        use crate::recovery::ProofResult;
        if !crate::game_tool_swap::same_path(path, &self.destination) {
            return ProofResult::NotOwned(format!(
                "{} is not where this plugin's package goes",
                path.display()
            ));
        }
        match folder_manifest(path) {
            FolderManifest::Absent => ProofResult::Proven,
            FolderManifest::Readable { id, .. } if id == self.plugin_id => ProofResult::Proven,
            FolderManifest::Readable { id, .. } => ProofResult::NotOwned(format!(
                "{} holds plugin {id}, not {}",
                path.display(),
                self.plugin_id
            )),
            FolderManifest::Unreadable(why) => ProofResult::CantTell(why),
        }
    }
}

/// Where a test may cut an install short, to see what recovery makes of what it leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallCrashPoint {
    /// Right after the journal and its marker are written, before anything moves.
    JournalWritten,
    /// Right after the previous version is stashed.
    Stashed,
    /// Right after the new version's files are in place, before the record is written.
    Extracted,
    /// Right after the record is written, before the journal is finished.
    Recorded,
}

/// Whether a test asked the install to stop here. A crash is a panic: the unwinding is what the
/// files and the journal are left as, exactly as a killed process leaves them.
pub fn crash_point(ctx: &crate::ctx::Ctx, point: InstallCrashPoint) {
    #[cfg(debug_assertions)]
    ctx.test_seams.install_crash_point(point);
    #[cfg(not(debug_assertions))]
    let _ = (ctx, point);
}

/// What recovery did about one interrupted install, or what it found and would not decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRecovery {
    /// There was nothing to recover.
    None,
    /// Recovery finished or undid the install. The line is ready to show.
    Done(String),
    /// A person has to decide; the line says what was found and what to do about it.
    NeedsHand(String),
    /// Another Agora holds the install lock, so nothing here was even looked at.
    Busy(String),
}

/// What a person decided about an install that needs a hand (MASTER_SPEC §32.11: every "needs a
/// hand" has a way out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallResolution {
    /// Do what the journal proves is safe, and report what needs a hand. The startup pass's answer.
    Automatic,
    /// Put the previous version back even though the package folder is not provably this install's:
    /// whatever is in it is replaced by the stashed copy. Only a person can decide that.
    Restore,
    /// Keep what is installed, and drop the stashed copy and the journal.
    Discard,
}

/// Finish or undo an install that a crash interrupted (MASTER_SPEC §32.11).
///
/// An install writes a journal before it moves anything, stashes the previous version, unpacks the
/// new one and writes the record, advancing the journal's state at each step. After a crash the
/// journal says how far it got, its envelope says what recovery may touch, and
/// [`crate::recovery::decide`] decides whether recovery may act at all. Recovery then either drops
/// a stale stashed copy (the record already names what is on disk) or puts the previous version
/// back.
///
/// Runs inside `CoreContext::initialize`, before the plugin records are read to build the game
/// registry: a plugin whose files were stranded has to be back before anything looks for the games
/// it declares. That is why it is not part of `startup_recovery::run_at_startup`, which every
/// adapter calls after the context exists.
///
/// Two processes: an install holds the plugin-install lock for its whole swap, and recovery never
/// waits for it — a held lock means a live install in another Agora process, which is left alone
/// and reported.
pub fn recover_interrupted_installs(
    conn: &rusqlite::Connection,
    paths: &crate::app_paths::AppPaths,
) -> LauncherResult<Vec<String>> {
    let rollback_root = paths.plugin_rollback_root();
    let interrupted = interrupted_installs(&rollback_root)?;
    if interrupted.is_empty() {
        return Ok(Vec::new());
    }
    let locks = crate::lock_manager::LockManager::new(paths.locks_root());
    let mut warnings = Vec::new();
    for (id, has_journal) in interrupted {
        let outcome = if has_journal {
            recover_one(&locks, conn, paths, &id)
        } else {
            // A stash from an Agora that wrote no journal: nothing says which install made it or
            // whether that install finished, so the explicit commands are the way out.
            legacy_stash(&id)
        };
        match outcome {
            InstallRecovery::None => {}
            InstallRecovery::Done(line) | InstallRecovery::NeedsHand(line) => warnings.push(line),
            InstallRecovery::Busy(line) => {
                // The lock is process-wide: one live install means none of these is looked at.
                warnings.push(line);
                return Ok(warnings);
            }
        }
    }
    Ok(warnings)
}

/// Resolve one or every interrupted install, for an explicit command (`agora plugin recover`).
///
/// Opens the local database itself: the adapter has no other way in, and this is the one place the
/// recovery needs a record.
pub fn resolve_installs(
    ctx: &crate::ctx::Ctx,
    id: Option<&PluginId>,
    resolution: InstallResolution,
) -> LauncherResult<Vec<(PluginId, InstallRecovery)>> {
    let conn = crate::db::local_state_connection(&ctx.paths.local_state_db())
        .map_err(|error| io_error(format!("could not open the local database: {error}")))?;
    let ids: Vec<PluginId> = match id {
        Some(id) => vec![id.clone()],
        None => interrupted_installs(&ctx.paths.plugin_rollback_root())?
            .into_iter()
            .map(|(id, _)| id)
            .collect(),
    };
    let mut out = Vec::new();
    for id in ids {
        let outcome = resolve_interrupted_install(&conn, &ctx.paths, &id, resolution)?;
        out.push((id, outcome));
    }
    Ok(out)
}

/// Resolve one plugin's interrupted install.
///
/// [`InstallResolution::Automatic`] is the startup pass's answer. The other two are a person's
/// answer to a "needs a hand", and they are the only way past a journal this build will not act on
/// by itself: `Restore` puts the stashed copy back over whatever the package folder holds, and
/// `Discard` drops the stashed copy and keeps what is installed.
pub fn resolve_interrupted_install(
    conn: &rusqlite::Connection,
    paths: &crate::app_paths::AppPaths,
    id: &PluginId,
    resolution: InstallResolution,
) -> LauncherResult<InstallRecovery> {
    let rollback = paths.plugin_rollback_root().join(id.as_str());
    let destination = package_dir(&paths.plugin_packages_root(), id);
    let locks = crate::lock_manager::LockManager::new(paths.locks_root());
    let journal_path = install_journal_path(paths, id);
    if resolution == InstallResolution::Automatic {
        // No journal and no stash is nothing at all; no journal with a stash is an older Agora's.
        if let crate::recovery::JournalRead::Absent =
            crate::recovery::read::<PluginInstallState>(&journal_path)
        {
            return Ok(match folder_manifest(&rollback) {
                FolderManifest::Absent => InstallRecovery::None,
                _ => legacy_stash(id),
            });
        }
        return Ok(recover_one(&locks, conn, paths, id));
    }
    // A person asked for this one: wait for a live install to let go (the caller is an explicit
    // command, not a startup pass), then do exactly what they asked.
    let _lock = match locks.acquire(
        crate::lock_manager::LockResource::PluginInstall,
        "plugin-recover",
    ) {
        Ok(lock) => lock,
        Err(error) => {
            return Ok(InstallRecovery::Busy(format!(
            "Plugin {id} was left alone: another Agora holds the plugin-install lock ({error}). \
                 Nothing was changed; try again once it finishes."
        )))
        }
    };
    match resolution {
        InstallResolution::Restore => {
            let stashed = match folder_manifest(&rollback) {
                FolderManifest::Absent => None,
                FolderManifest::Readable { version, .. } => Some(version),
                FolderManifest::Unreadable(why) => {
                    return Ok(InstallRecovery::NeedsHand(format!(
                        "Plugin {id} has a stashed copy that cannot be read ({why}), so it was not \
                         put back. Nothing was changed."
                    )))
                }
            };
            let Some(version) = stashed else {
                return Ok(InstallRecovery::NeedsHand(format!(
                    "There is no stashed copy of plugin {id} to put back. Nothing was changed."
                )));
            };
            match std::fs::symlink_metadata(&destination) {
                Ok(_) => match std::fs::remove_dir_all(&destination) {
                    Ok(()) => {}
                    Err(error) => {
                        return Ok(InstallRecovery::NeedsHand(format!(
                            "The installed files of plugin {id} could not be removed ({error}), so \
                             the stashed copy was not put back. Nothing was changed."
                        )))
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Ok(InstallRecovery::NeedsHand(format!(
                        "The folder of plugin {id} could not be examined ({error}), so the stashed \
                         copy was not put back. Nothing was changed."
                    )))
                }
            }
            if let Err(error) = std::fs::rename(&rollback, &destination) {
                return Ok(InstallRecovery::NeedsHand(format!(
                    "The stashed copy of plugin {id} could not be put back ({error})."
                )));
            }
            remove_if_present(&journal_path)?;
            remove_if_present(&install_marker_path(paths, id))?;
            Ok(InstallRecovery::Done(format!(
                "Put plugin {id} back to version {version}; whatever was in its folder was replaced."
            )))
        }
        InstallResolution::Discard => {
            let dropped = match folder_manifest(&rollback) {
                FolderManifest::Absent => None,
                FolderManifest::Readable { version, .. } => Some(version),
                FolderManifest::Unreadable(why) => {
                    return Ok(InstallRecovery::NeedsHand(format!(
                        "Plugin {id} has a stashed copy that cannot be read ({why}), so it was not \
                         dropped. Nothing was changed."
                    )))
                }
            };
            let had_journal = !matches!(
                crate::recovery::read::<PluginInstallState>(&journal_path),
                crate::recovery::JournalRead::Absent
            );
            if dropped.is_none() && !had_journal {
                return Ok(InstallRecovery::NeedsHand(format!(
                    "There is nothing stashed for plugin {id} and no install journal. Nothing was \
                     changed."
                )));
            }
            match std::fs::remove_dir_all(&rollback) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Ok(InstallRecovery::NeedsHand(format!(
                    "The stashed copy of plugin {id} could not be dropped ({error}). Nothing was \
                         changed."
                )))
                }
            }
            remove_if_present(&journal_path)?;
            remove_if_present(&install_marker_path(paths, id))?;
            Ok(InstallRecovery::Done(match dropped {
                Some(version) => format!(
                    "Dropped the stashed copy of plugin {id} version {version}. What is installed \
                     stays."
                ),
                None => format!("Dropped the journal of plugin {id}. Nothing else was changed."),
            }))
        }
        InstallResolution::Automatic => unreachable!("handled above"),
    }
}

/// The line for a stashed copy from an Agora that wrote no journal.
fn legacy_stash(id: &PluginId) -> InstallRecovery {
    InstallRecovery::NeedsHand(format!(
        "A plugin install needs a hand: a stashed copy of plugin {id} is in the rollback folder with \
         no install journal, so Agora cannot tell whether the install that made it finished. \
         Nothing was changed. Put it back with `agora plugin recover {id} --restore`, or drop it \
         with `agora plugin recover {id} --discard`."
    ))
}

/// The plugins whose installs may have been interrupted: those with a journal (a `<id>.json`
/// file), and those with a stashed folder and no journal (an older Agora's).
fn interrupted_installs(rollback_root: &Path) -> LauncherResult<Vec<(PluginId, bool)>> {
    let read = match std::fs::read_dir(rollback_root) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io_error(format!(
                "could not read {}: {error}",
                rollback_root.display()
            )))
        }
    };
    let mut found: std::collections::BTreeMap<String, (PluginId, bool)> =
        std::collections::BTreeMap::new();
    for item in read {
        let item =
            item.map_err(|e| io_error(format!("could not read {}: {e}", rollback_root.display())))?;
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        // `<id>.owner.json` is a marker of a journal that is handled on its own.
        if name.ends_with(".owner.json") {
            continue;
        }
        let (stem, has_journal) = match name.strip_suffix(".json") {
            Some(stem) => (stem, true),
            None => (name, false),
        };
        let Ok(id) = PluginId::parse(stem) else {
            continue;
        };
        let entry = found.entry(id.as_str().to_string()).or_insert((id, false));
        entry.1 |= has_journal;
    }
    Ok(found.into_values().collect())
}

/// One plugin's decision and the recovery that follows from it.
fn recover_one(
    locks: &crate::lock_manager::LockManager,
    conn: &rusqlite::Connection,
    paths: &crate::app_paths::AppPaths,
    id: &PluginId,
) -> InstallRecovery {
    use crate::recovery::{decide, Decision};
    let destination = package_dir(&paths.plugin_packages_root(), id);
    let proof = PackageFolderProof {
        plugin_id: id.as_str().to_string(),
        destination: destination.clone(),
    };
    let decision = decide::<PluginInstallState>(
        locks,
        &crate::lock_manager::LockResource::PluginInstall,
        &install_journal_path(paths, id),
        &proof,
    );
    match decision {
        Decision::Nothing => InstallRecovery::None,
        Decision::Busy(why) => InstallRecovery::Busy(format!(
            "Interrupted plugin installs were left alone: {why} Recovery runs again at the next start."
        )),
        Decision::NeedsHand(why) => InstallRecovery::NeedsHand(format!(
            "A plugin install needs a hand: {why} Put the previous version back with `agora plugin \
             recover {id} --restore`, or keep what is installed with `agora plugin recover {id} \
             --discard`."
        )),
        Decision::Act(capability) => apply(*capability, conn, paths, id),
    }
}

/// Finish or undo one interrupted install, through the capability: only the paths the journal
/// proved this operation owns are touched.
fn apply(
    capability: crate::recovery::RecoveryCapability<PluginInstallState>,
    conn: &rusqlite::Connection,
    paths: &crate::app_paths::AppPaths,
    id: &PluginId,
) -> InstallRecovery {
    let journal = match install_payload(&capability) {
        Ok(journal) => journal,
        Err(why) => {
            return InstallRecovery::NeedsHand(format!(
                "A plugin install needs a hand: {why}. Nothing was changed; the journal was kept."
            ))
        }
    };
    if !capability.left_alone().is_empty() {
        // One of the paths is not provably this install's, so restoring would put the stashed copy
        // back over something else. Nothing is touched, and a person decides.
        let paths: Vec<String> = capability
            .left_alone()
            .iter()
            .map(|(path, why)| format!("{} ({why})", path.display()))
            .collect();
        return InstallRecovery::NeedsHand(format!(
            "A plugin install needs a hand: Agora will not touch {} because it is not provably that \
             install's. Nothing was changed. Put the previous version back with `agora plugin \
             recover {id} --restore`, or keep what is installed with `agora plugin recover {id} \
             --discard`.",
            paths.join(", ")
        ));
    }
    let destination = package_dir(&paths.plugin_packages_root(), id);
    let rollback = paths.plugin_rollback_root().join(id.as_str());
    let record = match store::get(conn, id) {
        Ok(record) => record,
        Err(error) => {
            return InstallRecovery::NeedsHand(format!(
                "A plugin install needs a hand: the record for {id} could not be read ({error}). \
                 Nothing was changed."
            ))
        }
    };
    // Finished: the record already names the version that is on disk. The stashed copy did not
    // need to come back, so it was stale and is dropped. A package folder whose manifest cannot be
    // read never reaches this point — the proof above refused it — so a file that cannot be read
    // is never "not finished".
    let finished = match (&record, folder_manifest(&destination)) {
        (Some(record), FolderManifest::Readable { version, .. })
            if !store::is_tombstone(record) =>
        {
            version == record.manifest.version.to_string()
        }
        _ => false,
    };
    if finished {
        let dropped = match readable_version(&rollback) {
            Some(version) => format!(" ({version})"),
            None => String::new(),
        };
        if let Err(error) = capability.remove_dir(&rollback) {
            return InstallRecovery::NeedsHand(format!(
                "Plugin {id} was installed, but its stashed copy could not be dropped ({error}). \
                 Run `agora plugin recover {id} --discard` to drop it."
            ));
        }
        return match capability.finish() {
            Ok(()) => InstallRecovery::Done(format!(
                "Finished the interrupted install of plugin {id} {}: the new version is in place and \
                 the stashed copy{dropped} was dropped.",
                journal.new_version
            )),
            Err(error) => InstallRecovery::NeedsHand(format!(
                "Plugin {id} was installed, but its journal could not be removed ({error})."
            )),
        };
    }
    // Is the previous version actually stashed? A crash between the journal write and the stash
    // leaves none, and then there is nothing to put back — the package folder still holds what it
    // held. Restoring in that case would delete the only copy there is.
    let stashed = match folder_manifest(&rollback) {
        FolderManifest::Absent => None,
        FolderManifest::Readable { version, .. } => Some(version),
        FolderManifest::Unreadable(why) => {
            return InstallRecovery::NeedsHand(format!(
            "Plugin {id} has a stashed copy that cannot be checked: {why}. Nothing was changed. \
                 Put it back with `agora plugin recover {id} --restore`, or drop it with `agora \
                 plugin recover {id} --discard`."
        ))
        }
    };
    let Some(restoring) = stashed else {
        // Nothing was moved aside, so there is nothing to put back: the install did not finish and
        // the package folder still holds whatever it held.
        return match capability.finish() {
            Ok(()) if journal.had_files => InstallRecovery::Done(format!(
                "Plugin {id} {} did not finish installing, and the previous version was never moved \
                 aside, so nothing was changed. Install it again.",
                journal.new_version
            )),
            Ok(()) => InstallRecovery::Done(format!(
                "Plugin {id} {} did not finish installing and no previous version was stashed, so \
                 nothing was put back. Install it again.",
                journal.new_version
            )),
            Err(error) => InstallRecovery::NeedsHand(format!(
                "Plugin {id} did not finish installing, and its journal could not be removed \
                 ({error})."
            )),
        };
    };
    let restoring = format!(" ({restoring})");
    match capability.restore(&rollback, &destination) {
        Ok(()) => match capability.finish() {
            Ok(()) => InstallRecovery::Done(format!(
                "Recovered plugin {id} from an interrupted install; the previous version{restoring} \
                 is back."
            )),
            Err(error) => InstallRecovery::NeedsHand(format!(
                "Plugin {id} was put back, but its journal could not be removed ({error})."
            )),
        },
        Err(error) => InstallRecovery::NeedsHand(format!(
            "Plugin {id} could not be put back: {error}. Nothing was changed; the journal was kept, \
             and `agora plugin recover {id}` tries again."
        )),
    }
}

/// The journal's own payload, read under the lock the capability holds.
fn install_payload(
    capability: &crate::recovery::RecoveryCapability<PluginInstallState>,
) -> Result<InstallJournal, String> {
    let bytes = std::fs::read(capability.journal_path())
        .map_err(|error| format!("its journal could not be read ({error})"))?;
    let journal: InstallJournal = serde_json::from_slice(&bytes)
        .map_err(|error| format!("its journal is not a complete install journal ({error})"))?;
    if journal.envelope.operation_id != capability.operation_id() {
        return Err("its journal changed while recovery held the lock".into());
    }
    Ok(journal)
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
            return Err(io_error("the plugin folder holds too many files to check"));
        }
        if meta.is_file() {
            *total = total.saturating_add(meta.len());
            if *total > MAX_TOTAL_BYTES {
                return Err(io_error("the plugin folder is too large to check"));
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

/// SHA-256 of a package's bytes, as the preview read them.
///
/// Consent names the package it was given for. The installer hashes the bytes
/// it is about to unpack and refuses when it is not the previewed package, so
/// a file swapped between the prompt and the install cannot ride on the old
/// answer.
pub fn package_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// The folder counterpart of [`package_digest`]: the manifest plus every file
/// the loader could read, by relative path and content.
///
/// A development folder is edited in place. The author's answer covers the
/// manifest and files they previewed, not whatever the folder holds when the
/// install runs.
pub fn folder_digest(folder: &Path) -> LauncherResult<String> {
    use sha2::{Digest, Sha256};
    let manifest = std::fs::read(folder.join(MANIFEST_FILENAME)).map_err(|e| {
        io_error(format!(
            "could not read {MANIFEST_FILENAME} in {}: {e}",
            folder.display()
        ))
    })?;
    let mut files = std::collections::BTreeMap::new();
    let mut entries = 0usize;
    let mut total = 0u64;
    collect_script_files(folder, folder, &mut files, &mut entries, &mut total)?;
    let mut hasher = Sha256::new();
    hasher.update(&manifest);
    hasher.update([0]);
    hasher.update(finish_script_digest(&files).as_bytes());
    Ok(hex::encode(hasher.finalize()))
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
        let preview = build_preview(manifest, None, None, None, 2, 100, String::new());
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
            String::new(),
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
        let preview = build_preview(same, None, None, Some(&existing), 2, 100, String::new());
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
        let preview = build_preview(wider, None, None, Some(&existing), 2, 100, String::new());
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
        let preview = build_preview(manifest, None, None, None, 2, 100, String::new());
        assert_eq!(
            preview.added_capabilities,
            vec!["instance:read".to_string(), "content:read".to_string()]
        );
        assert!(preview.requires_capability_consent());
    }

    /// A package folder whose manifest cannot be read proves nothing: it may be the new version,
    /// complete, or something else entirely. Recovery must not read that as "the install did not
    /// finish" and put the stashed copy back over it — that deletes an install the user asked for
    /// on the strength of a file that could not be read.
    #[test]
    fn an_unreadable_manifest_in_the_package_folder_is_not_replaced_by_the_stashed_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();

        // A crash after the new version was unpacked: the journal says so, the previous version is
        // stashed, and the new package folder is there.
        crashed_install(&paths, "acme.one", Some("0.9.0"), true, "1.0.0");
        let destination = paths.plugin_packages_root().join("acme.one");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("main.js"), "// new version").unwrap();
        std::fs::write(destination.join(MANIFEST_FILENAME), "{ not a manifest").unwrap();
        let rollback = paths.plugin_rollback_root().join("acme.one");

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();

        assert!(
            destination.join("main.js").is_file(),
            "the package folder was replaced on the strength of a manifest that could not be read: \
             {warnings:?}"
        );
        assert!(
            rollback.is_dir(),
            "the stashed copy was consumed: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("needs a hand")),
            "the case is reported as needing a hand: {warnings:?}"
        );
    }

    /// A stashed copy from an Agora that wrote no journal: nothing says which install made it or
    /// whether that install finished, so it is not recovered automatically.
    #[test]
    fn a_stashed_copy_with_no_journal_needs_a_hand() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();
        let rollback = paths.plugin_rollback_root().join("acme.one");
        std::fs::create_dir_all(&rollback).unwrap();
        std::fs::write(
            rollback.join(MANIFEST_FILENAME),
            manifest_json("acme.one").replace("\"1.0.0\"", "\"0.9.0\""),
        )
        .unwrap();

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();

        assert!(
            warnings
                .iter()
                .any(|w| w.contains("needs a hand") && w.contains("--restore")),
            "the way out is named: {warnings:?}"
        );
        assert!(rollback.is_dir(), "nothing was touched");
        assert!(
            !paths.plugin_packages_root().join("acme.one").exists(),
            "nothing was restored"
        );
    }

    /// The crash this recovers from most often: the journal is written and the previous version is
    /// stashed, and the new one is not unpacked. The previous version comes back.
    #[test]
    fn a_crash_before_the_stash_leaves_the_previous_version_where_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();
        // The journal was written and nothing moved: the package folder still holds the previous
        // version, and restoring over it would delete the only copy there is.
        crashed_install(&paths, "acme.one", Some("0.9.0"), false, "1.0.0");
        let destination = paths.plugin_packages_root().join("acme.one");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(
            destination.join(MANIFEST_FILENAME),
            manifest_json("acme.one").replace("\"1.0.0\"", "\"0.9.0\""),
        )
        .unwrap();

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();

        assert!(
            readable_version(&destination).as_deref() == Some("0.9.0"),
            "the previous version is still there: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("did not finish")),
            "{warnings:?}"
        );
    }

    /// A crash after the previous version was stashed and before the new one was unpacked: the
    /// previous version comes back, never neither.
    #[test]
    fn a_crash_after_the_stash_puts_the_previous_version_back() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();
        let journal = crashed_install(&paths, "acme.one", Some("0.9.0"), true, "1.0.0");
        let destination = paths.plugin_packages_root().join("acme.one");

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();

        assert!(
            readable_version(&destination).as_deref() == Some("0.9.0"),
            "the previous version is back: {warnings:?}"
        );
        assert!(
            !journal.exists() && !paths.plugin_rollback_root().join("acme.one").exists(),
            "the journal and the stash are gone: {warnings:?}"
        );
    }

    /// A crash after the record was written: the new version is installed and recorded, so the
    /// stashed copy is stale and is dropped, leaving the new version.
    #[test]
    fn a_crash_after_the_record_keeps_the_new_version() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();
        crashed_install(&paths, "acme.one", Some("0.9.0"), true, "1.0.0");
        let destination = paths.plugin_packages_root().join("acme.one");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(
            destination.join(MANIFEST_FILENAME),
            manifest_json("acme.one"),
        )
        .unwrap();
        // The record the install writes last.
        let manifest = PluginManifest::parse(&manifest_json("acme.one")).unwrap();
        store::upsert(
            &conn,
            &manifest,
            &grant_for(&manifest).unwrap(),
            &PluginSource::Package,
            &destination,
            true,
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();

        assert!(
            readable_version(&destination).as_deref() == Some("1.0.0"),
            "the new version stays: {warnings:?}"
        );
        assert!(
            !paths.plugin_rollback_root().join("acme.one").exists(),
            "the stale stash is dropped: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Finished the interrupted install")),
            "{warnings:?}"
        );
    }

    /// The on-disk state a crash leaves: a journal in its first state, its marker, the previous
    /// version stashed, and a writer that no longer exists (the process that died; a test process
    /// is still running, so the identity is pointed at a pid no process has).
    /// A package as `install_package` receives it, for the crash test below.
    fn installable_package(version: &str) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(MANIFEST_FILENAME, options).unwrap();
            zip.write_all(
                manifest_json("acme.one")
                    .replace("\"1.0.0\"", &format!("\"{version}\""))
                    .as_bytes(),
            )
            .unwrap();
            zip.start_file("main.js", options).unwrap();
            zip.write_all(format!("// version {version}").as_bytes())
                .unwrap();
            zip.finish().unwrap();
        }
        buffer.into_inner()
    }

    /// The journal a killed process leaves points at a process that no longer exists. This one is
    /// still running, so its journal is repointed at a pid the system does not have.
    fn writer_is_gone(paths: &crate::app_paths::AppPaths, id: &str) {
        let path = install_journal_path(paths, &PluginId::parse(id).unwrap());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["writer"] = serde_json::json!({
            "pid": u32::MAX,
            "start_time": 1,
            "expected_exe": null,
        });
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    /// The crash a killed process leaves at each boundary of a real install, through the seam on
    /// the context: recovery lands on the previous version or the new one, never on neither, and
    /// leaves no journal behind (MASTER_SPEC §32.11).
    #[test]
    fn a_crash_at_each_boundary_of_an_install_recovers_to_the_old_or_the_new_version() {
        use crate::plugins::service::PluginService;
        let points = [
            InstallCrashPoint::JournalWritten,
            InstallCrashPoint::Stashed,
            InstallCrashPoint::Extracted,
            InstallCrashPoint::Recorded,
        ];
        for point in points {
            let tmp = tempfile::tempdir().unwrap();
            let ctx = crate::ctx::CoreContext::for_testing(tmp.path().join("data"));
            ctx.paths.create_required_dirs().unwrap();
            crate::db::init_local_state_db(&ctx.paths.local_state_db()).unwrap();
            let service = PluginService::headless(
                ctx.clone(),
                std::sync::Arc::new(agora_plugin_host::QuickJsHost::new()),
            );
            service
                .install_package_bytes(&installable_package("1.0.0"), true)
                .expect("the first version installs");

            // The crash: a panic where the process would have died.
            ctx.test_seams.set_install_crash(Some(point));
            let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = service.install_package_bytes(&installable_package("2.0.0"), true);
            }));
            ctx.test_seams.set_install_crash(None);
            assert!(crashed.is_err(), "the seam cut the install at {point:?}");

            writer_is_gone(&ctx.paths, "acme.one");
            let conn = crate::db::local_state_connection(&ctx.paths.local_state_db()).unwrap();
            let warnings = recover_interrupted_installs(&conn, &ctx.paths).unwrap();

            let destination = package_dir(
                &ctx.paths.plugin_packages_root(),
                &PluginId::parse("acme.one").unwrap(),
            );
            let version = readable_version(&destination);
            assert!(
                matches!(version.as_deref(), Some("1.0.0") | Some("2.0.0")),
                "an install cut short at {point:?} recovered to neither version: {version:?} \
                 ({warnings:?})"
            );
            assert!(
                matches!(
                    crate::recovery::read::<PluginInstallState>(&install_journal_path(
                        &ctx.paths,
                        &PluginId::parse("acme.one").unwrap()
                    )),
                    crate::recovery::JournalRead::Absent
                ),
                "the journal was left behind after a crash at {point:?}: {warnings:?}"
            );
            assert!(
                matches!(
                    folder_manifest(&ctx.paths.plugin_rollback_root().join("acme.one")),
                    FolderManifest::Absent
                ),
                "a stashed copy was left behind after a crash at {point:?}: {warnings:?}"
            );
        }
    }

    fn crashed_install(
        paths: &crate::app_paths::AppPaths,
        id: &str,
        previous: Option<&str>,
        stashed: bool,
        new: &str,
    ) -> PathBuf {
        let plugin_id = PluginId::parse(id).unwrap();
        let mut journal = InstallJournal::begin(
            paths,
            &plugin_id,
            previous.is_some(),
            previous.map(str::to_string),
            new.to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(paths.plugin_packages_root()).unwrap();
        journal.start(paths).unwrap();
        if let Some(version) = previous {
            if stashed {
                let rollback = paths.plugin_rollback_root().join(id);
                std::fs::create_dir_all(&rollback).unwrap();
                std::fs::write(
                    rollback.join(MANIFEST_FILENAME),
                    manifest_json(id).replace("\"1.0.0\"", &format!("\"{version}\"")),
                )
                .unwrap();
                journal.advance(paths, PluginInstallState::Stashed).unwrap();
            }
        }
        let path = install_journal_path(paths, &plugin_id);
        // The writer died: nothing in the process table answers to this identity.
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["writer"] = serde_json::json!({
            "pid": u32::MAX,
            "start_time": 1,
            "expected_exe": null,
        });
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        path
    }

    /// Two processes: while another Agora process holds the plugin-install lock (it is mid
    /// install), startup recovery leaves the stashed previous version alone and says so; once
    /// the lock is free, the next pass puts it back.
    #[test]
    fn recovery_leaves_stashed_plugins_alone_while_another_process_installs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::app_paths::AppPaths::from_root(tmp.path().join("data"));
        paths.create_required_dirs().unwrap();
        crate::db::init_local_state_db(&paths.local_state_db()).unwrap();
        let conn = crate::db::local_state_connection(&paths.local_state_db()).unwrap();

        // The state a crashed install leaves: journal, marker, previous version stashed.
        crashed_install(&paths, "acme.one", Some("0.9.0"), true, "1.0.0");
        let rollback = paths.plugin_rollback_root().join("acme.one");
        let destination = paths.plugin_packages_root().join("acme.one");

        // A second lock manager on the same locks root is what another process sees.
        let other_process = crate::lock_manager::LockManager::new(paths.locks_root());
        let held = other_process
            .acquire(
                crate::lock_manager::LockResource::PluginInstall,
                "plugin-install",
            )
            .expect("the live install takes the lock");

        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("left alone")),
            "the skip is reported: {warnings:?}"
        );
        assert!(rollback.is_dir(), "the stashed version is untouched");
        assert!(
            !destination.exists(),
            "nothing was restored underneath the live install"
        );

        drop(held);
        let warnings = recover_interrupted_installs(&conn, &paths).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Recovered plugin acme.one")),
            "recovered once the lock is free: {warnings:?}"
        );
        assert!(!rollback.exists(), "the rollback folder is gone");
        assert!(
            readable_version(&destination).as_deref() == Some("0.9.0"),
            "the previous version is back"
        );
    }
}
