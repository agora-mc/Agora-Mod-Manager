//! Official plugins: ordinary plugin packages that Agora's signed registry vouches for.
//!
//! An official plugin is **not** a different kind of plugin and not a claim the
//! plugin makes about itself. It is a normal package whose id, version and
//! package SHA-256 are listed in the `official_plugins` table of the Ed25519-signed
//! `registry.db`. The `publisher` half of a plugin id is only a namespace, so
//! nothing in the manifest is ever read to decide this.
//!
//! # What "official" means for an installed plugin
//!
//! A plugin counts as official only while all of these hold, checked against the
//! signed registry each time it matters (never cached across a restart):
//!
//! 1. it was installed by [`PluginService::install_official`] (or `update` /
//!    `rollback`), which recorded the signed entry it matched;
//! 2. the signed registry still lists that id and version, pinning the same
//!    package hash;
//! 3. the files in its folder, in the folder the installer uses, still hash to the
//!    **tree digest the signed entry pins** (not one remembered in `local_state.db`,
//!    which is not signed), and its stored manifest and grants are what that
//!    folder's `agora-plugin.json` says they should be.
//!
//! Lose any one and the plugin is [`OfficialStatus::Unverified`], with the reason.
//! An unverified plugin is **blocked**, not demoted to a community plugin: it is
//! not loaded, whatever the emergency stop says, until it is updated, rolled back
//! or reinstalled. The same goes for any plugin that uses an official plugin's id
//! without being that plugin.
//!
//! # What an official install does differently
//!
//! It fetches the package from the URL the signed entry names, and checks size and
//! SHA-256 against that entry **before a single byte is written** under the
//! plugins folder. After that it is the same installer as any package, with the
//! same capability consent, and the same per-plugin "<name> runs code. Allow it?"
//! for a plugin that has scripts. Official status buys verification and
//! registration order, not exemption from consent or from the emergency stop.

use super::service::{Consent, PluginService, PluginSummary, UpdateOutcome};
use super::store::{self, OfficialProvenance};
use super::{install, InstallPreview};
use crate::error::{LauncherError, LauncherResult};
use agora_plugin_api::manifest::PluginId;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// The version of Agora this build is, compared with an entry's minimum.
const AGORA_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn refused(code: &str, message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: code.into(),
        message: message.into(),
    }
}

fn io_error(message: impl Into<String>) -> LauncherError {
    refused("ERR_PLUGIN_IO", message)
}

// ---------------------------------------------------------------------------
// Trust anchor
// ---------------------------------------------------------------------------

/// Which key the registry must be signed with.
///
/// Production code uses [`RegistryTrust::production`], the key compiled into
/// the app. [`RegistryTrust::with_key`] exists so tests can sign a registry of
/// their own; no setting or environment variable reaches it.
#[derive(Debug, Clone, Default)]
pub struct RegistryTrust {
    key_hex: Option<String>,
}

impl RegistryTrust {
    pub fn production() -> Self {
        Self { key_hex: None }
    }

    /// Test support: trust this Ed25519 public key (hex) instead.
    pub fn with_key(key_hex: impl Into<String>) -> Self {
        Self {
            key_hex: Some(key_hex.into()),
        }
    }

    fn open(&self, app_data_dir: &Path) -> LauncherResult<crate::registry_sync::VerifiedRegistry> {
        match &self.key_hex {
            None => crate::registry_sync::open_verified_registry(app_data_dir),
            Some(key) => crate::registry_sync::open_verified_registry_with_key(app_data_dir, key),
        }
    }
}

// ---------------------------------------------------------------------------
// The signed catalog
// ---------------------------------------------------------------------------

/// Which slot of an official plugin an entry occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryChannel {
    /// What a new install or an update moves to.
    Current,
    /// The version before it, kept so a bad update can be rolled back.
    Previous,
}

/// One version of one official plugin, as the signed registry lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficialEntry {
    pub plugin_id: String,
    pub version: String,
    pub channel: EntryChannel,
    pub name: String,
    pub description: Option<String>,
    /// Games this plugin adds, if any.
    pub game_ids: Vec<String>,
    pub package_url: String,
    /// Lowercase hex SHA-256 of the package zip.
    pub package_sha256: String,
    /// Lowercase hex [`tree_digest`] of the files the package unpacks to.
    ///
    /// Pinned here, in the signed registry, so an installed copy is checked
    /// against something local state cannot rewrite.
    pub package_tree_sha256: String,
    pub package_size: u64,
    pub min_agora_version: String,
}

impl OfficialEntry {
    /// Whether this entry may be acted on at all.
    ///
    /// The compiler enforces the same rules, but the app does not rely on that:
    /// the registry is data, and an entry that fails here is never downloaded.
    pub fn check(&self) -> Result<(), String> {
        if PluginId::parse(&self.plugin_id).is_err() {
            return Err(format!("`{}` is not a valid plugin id", self.plugin_id));
        }
        if semver::Version::parse(&self.version).is_err() {
            return Err(format!("`{}` is not a version", self.version));
        }
        if semver::Version::parse(&self.min_agora_version).is_err() {
            return Err(format!(
                "`{}` is not a valid minimum Agora version",
                self.min_agora_version
            ));
        }
        if package_host(&self.package_url).is_none() {
            return Err(format!(
                "the package URL `{}` is not an https URL with a plain host",
                self.package_url
            ));
        }
        if !is_lower_hex_64(&self.package_sha256) {
            return Err("the package SHA-256 is not 64 lowercase hex characters".into());
        }
        if !is_lower_hex_64(&self.package_tree_sha256) {
            return Err("the package tree digest is not 64 lowercase hex characters".into());
        }
        if self.package_size == 0 || self.package_size > install::MAX_TOTAL_BYTES {
            return Err(format!(
                "the declared package size {} is outside 1..={} bytes",
                self.package_size,
                install::MAX_TOTAL_BYTES
            ));
        }
        Ok(())
    }

    /// Why this build of Agora cannot install the entry, if it cannot.
    pub fn incompatible_reason(&self) -> Option<String> {
        let (Ok(minimum), Ok(running)) = (
            semver::Version::parse(&self.min_agora_version),
            semver::Version::parse(AGORA_VERSION),
        ) else {
            // Unreadable on either side: not provably compatible.
            return Some("its minimum Agora version could not be compared".into());
        };
        // A pre-release build of the same number still counts as that number.
        let running = semver::Version::new(running.major, running.minor, running.patch);
        (running < minimum).then(|| format!("it needs Agora {minimum} or newer"))
    }
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The host of an `https://` URL that has nothing in its authority but a host.
///
/// Anything else (userinfo, a port, a scheme other than https, an empty host)
/// is `None`; the download is pinned to this host and re-validated per redirect.
fn package_host(url: &str) -> Option<String> {
    if url.trim() != url || url.chars().any(char::is_whitespace) {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let valid = !authority.is_empty()
        && authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !authority.starts_with(['.', '-'])
        && !authority.ends_with(['.', '-']);
    valid.then(|| authority.to_ascii_lowercase())
}

/// Every official plugin version the signed registry lists.
#[derive(Debug, Clone, Default)]
pub struct OfficialCatalog {
    entries: Vec<OfficialEntry>,
}

impl OfficialCatalog {
    /// Read the table from a registry that has already been verified.
    ///
    /// A registry without the table predates official plugins and simply lists
    /// none. Any other failure to read it is an error, not an empty catalog.
    pub fn read(conn: &Connection) -> LauncherResult<Self> {
        let unreadable = |e: rusqlite::Error| {
            refused(
                "ERR_REGISTRY_UNREADABLE",
                format!("The official plugin list in the catalog could not be read: {e}"),
            )
        };
        let has_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'official_plugins'",
                [],
                |row| row.get(0),
            )
            .map_err(unreadable)?;
        if has_table == 0 {
            return Ok(Self::default());
        }
        let mut stmt = conn
            .prepare(
                "SELECT plugin_id, version, channel, name, description, game_ids_json,
                        package_url, package_sha256, package_size, min_agora_version,
                        package_tree_sha256
                 FROM official_plugins ORDER BY plugin_id, channel, version",
            )
            .map_err(unreadable)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            })
            .map_err(unreadable)?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, version, channel, name, description, games, url, sha, size, min, tree) =
                row.map_err(unreadable)?;
            let channel = match channel.as_str() {
                "current" => EntryChannel::Current,
                "previous" => EntryChannel::Previous,
                other => {
                    return Err(refused(
                        "ERR_REGISTRY_UNREADABLE",
                        format!("The catalog lists `{id}` with an unknown channel `{other}`"),
                    ))
                }
            };
            let game_ids: Vec<String> = serde_json::from_str(&games).map_err(|e| {
                refused(
                    "ERR_REGISTRY_UNREADABLE",
                    format!("The catalog lists unreadable game ids for `{id}`: {e}"),
                )
            })?;
            entries.push(OfficialEntry {
                plugin_id: id,
                version,
                channel,
                name,
                description,
                game_ids,
                package_url: url,
                package_sha256: sha,
                package_tree_sha256: tree,
                // A negative size is not a size; 0 fails `check`.
                package_size: u64::try_from(size).unwrap_or(0),
                min_agora_version: min,
            });
        }
        Ok(Self { entries })
    }

    pub fn entries(&self) -> &[OfficialEntry] {
        &self.entries
    }

    /// The entry a new install or an update moves to. Exact, case-sensitive id.
    pub fn current(&self, plugin_id: &str) -> Option<&OfficialEntry> {
        self.in_channel(plugin_id, EntryChannel::Current)
    }

    /// The entry a rollback returns to.
    pub fn previous(&self, plugin_id: &str) -> Option<&OfficialEntry> {
        self.in_channel(plugin_id, EntryChannel::Previous)
    }

    fn in_channel(&self, plugin_id: &str, channel: EntryChannel) -> Option<&OfficialEntry> {
        self.entries
            .iter()
            .find(|entry| entry.plugin_id == plugin_id && entry.channel == channel)
    }

    /// Whether the catalog lists this exact id at all, in either channel.
    pub fn has_id(&self, plugin_id: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.plugin_id == plugin_id)
    }

    /// The entry for exactly this id and version, in either channel.
    pub fn find(&self, plugin_id: &str, version: &str) -> Option<&OfficialEntry> {
        self.entries
            .iter()
            .find(|entry| entry.plugin_id == plugin_id && entry.version == version)
    }
}

/// Verify the cached registry's signature and read its official plugins.
pub fn load_signed_catalog(
    trust: &RegistryTrust,
    app_data_dir: &Path,
) -> LauncherResult<OfficialCatalog> {
    let registry = trust.open(app_data_dir)?;
    OfficialCatalog::read(registry.connection())
}

/// Plain-language reason a catalog could not be used.
fn unavailable_reason(error: &LauncherError) -> String {
    match error {
        LauncherError::RegistryMissing => {
            "No catalog has been downloaded yet, so official plugins cannot be listed or verified."
                .into()
        }
        LauncherError::RegistrySignatureInvalid => {
            "The cached catalog is unsigned or its signature does not verify, so official plugins \
             cannot be listed or verified."
                .into()
        }
        other => format!("The catalog could not be used to verify official plugins: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Status of an installed plugin
// ---------------------------------------------------------------------------

/// Whether an installed plugin is currently verified as official.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum OfficialStatus {
    /// Matches a signed entry: shown with the Official badge.
    Official { version: String },
    /// Not installed from a signed entry and not using an official id: a
    /// community plugin.
    Community,
    /// The id is an official plugin's, or the plugin was installed as official,
    /// and it does not verify (edited, revoked, re-pinned, or never was the
    /// official package). It is blocked from loading, and `reason` says why.
    Unverified { reason: String },
}

impl OfficialStatus {
    pub fn is_official(&self) -> bool {
        matches!(self, OfficialStatus::Official { .. })
    }
}

/// Digest of an installed plugin's files: relative path and content hash of
/// every entry, in sorted order.
///
/// Anything that is not a plain file or folder (a link, say) is recorded as such
/// rather than followed, so adding one changes the digest. Bounded by the same
/// limits as a package, because the folder is user-writable.
pub fn tree_digest(root: &Path) -> LauncherResult<String> {
    fn walk(
        root: &Path,
        dir: &Path,
        entries: &mut Vec<DigestEntry>,
        total: &mut u64,
    ) -> LauncherResult<()> {
        let read = std::fs::read_dir(dir)
            .map_err(|e| io_error(format!("could not read {}: {e}", dir.display())))?;
        for item in read {
            let item =
                item.map_err(|e| io_error(format!("could not read {}: {e}", dir.display())))?;
            let path = item.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|_| io_error("a plugin file is outside its folder"))?
                .to_string_lossy()
                .replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| io_error(format!("could not inspect {}: {e}", path.display())))?;
            if entries.len() >= install::MAX_ENTRIES * 2 {
                return Err(io_error("the plugin folder holds too many files to verify"));
            }
            if meta.is_dir() {
                entries.push((rel, "dir", String::new()));
                walk(root, &path, entries, total)?;
            } else if meta.is_file() {
                *total = total.saturating_add(meta.len());
                if *total > install::MAX_TOTAL_BYTES {
                    return Err(io_error("the plugin folder is too large to verify"));
                }
                entries.push((rel, "file", hash_file(&path)?));
            } else {
                entries.push((rel, "other", String::new()));
            }
        }
        Ok(())
    }

    fn hash_file(path: &Path) -> LauncherResult<String> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)
            .map_err(|e| io_error(format!("could not read {}: {e}", path.display())))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buffer)
                .map_err(|e| io_error(format!("could not read {}: {e}", path.display())))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    let mut entries = Vec::new();
    let mut total = 0u64;
    walk(root, root, &mut entries, &mut total)?;
    Ok(finish_digest(entries))
}

/// One line of a digest: relative path, kind (`file`, `dir`, `other`), content hash.
type DigestEntry = (String, &'static str, String);

fn finish_digest(mut entries: Vec<DigestEntry>) -> String {
    entries.sort();
    let mut hasher = Sha256::new();
    for (rel, kind, hash) in &entries {
        hasher.update(kind.as_bytes());
        hasher.update([0]);
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// The [`tree_digest`] a package will have once it is unpacked, computed from
/// the zip bytes without writing anything.
///
/// This is what a curator pins in `registry/official-plugins/<id>.json`
/// (`scripts/official_plugin_digest.py` computes the same value), and what the
/// installer compares to the signed entry before unpacking. Unpacking writes
/// the files and the folders above them, and nothing else, so the two agree.
pub fn zip_tree_digest(bytes: &[u8]) -> LauncherResult<String> {
    use std::io::Read;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        refused(
            "ERR_PLUGIN_PACKAGE_INVALID",
            format!("not a readable plugin package: {e}"),
        )
    })?;
    if zip.len() > install::MAX_ENTRIES {
        return Err(refused(
            "ERR_PLUGIN_PACKAGE_INVALID",
            "the package holds too many files",
        ));
    }
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut total = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| {
            refused(
                "ERR_PLUGIN_PACKAGE_INVALID",
                format!("unreadable entry in the package: {e}"),
            )
        })?;
        let name = entry
            .name()
            .map_err(|e| {
                refused(
                    "ERR_PLUGIN_PACKAGE_INVALID",
                    format!("unreadable entry in the package: {e}"),
                )
            })?
            .into_owned();
        if entry.is_dir() {
            continue;
        }
        install::validate_entry_path(&name)?;
        let parts: Vec<&str> = name
            .split(['/', '\\'])
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        let Some((_, ancestors)) = parts.split_last() else {
            return Err(refused(
                "ERR_PLUGIN_PACKAGE_INVALID",
                "the package contains an unnamed entry",
            ));
        };
        for depth in 1..=ancestors.len() {
            folders.insert(parts[..depth].join("/"));
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut read_here = 0u64;
        loop {
            let n = entry.read(&mut buffer).map_err(|e| {
                refused(
                    "ERR_PLUGIN_PACKAGE_INVALID",
                    format!("could not read `{name}`: {e}"),
                )
            })?;
            if n == 0 {
                break;
            }
            read_here += n as u64;
            total += n as u64;
            if read_here > install::MAX_ENTRY_BYTES || total > install::MAX_TOTAL_BYTES {
                return Err(refused(
                    "ERR_PLUGIN_PACKAGE_INVALID",
                    "the package expands past its size limit",
                ));
            }
            hasher.update(&buffer[..n]);
        }
        // A later entry with the same path replaces an earlier one, as unpacking does.
        files.insert(parts.join("/"), hex::encode(hasher.finalize()));
    }
    let mut entries: Vec<DigestEntry> = Vec::new();
    for folder in folders {
        entries.push((folder, "dir", String::new()));
    }
    for (path, hash) in files {
        entries.push((path, "file", hash));
    }
    Ok(finish_digest(entries))
}

/// Decide one installed plugin's status against a catalog that was read from a
/// verified registry.
///
/// `local_state.db` is not signed, so nothing in it is believed on its own: the
/// files are hashed against the digest the *signed* entry pins, the manifest the
/// record carries must equal the one in those hashed files, and the grants must
/// be a subset of what that manifest asks for.
fn evaluate(
    provenance: Option<&OfficialProvenance>,
    record: &store::PluginRecord,
    catalog: &OfficialCatalog,
    packages_root: &Path,
) -> OfficialStatus {
    let unverified = |reason: String| OfficialStatus::Unverified { reason };
    let id = record.id().as_str();
    let version = record.manifest.version.to_string();

    if record.source.is_development() {
        return unverified(format!(
            "`{id}` is an official plugin's id, but this is a development folder, not the \
             official package"
        ));
    }
    let Some(provenance) = provenance else {
        return unverified(format!(
            "`{id}` is the id of an official plugin, but this copy was not installed from the \
             official package"
        ));
    };
    if provenance.entry_id != id {
        return unverified("its install record names a different plugin".into());
    }
    if provenance.entry_version != version {
        return unverified(format!(
            "version {version} is installed, but the official install recorded {}",
            provenance.entry_version
        ));
    }
    let Some(entry) = catalog.find(id, &version) else {
        return unverified(format!(
            "the signed catalog no longer lists version {version} of `{id}`"
        ));
    };
    if let Err(problem) = entry.check() {
        return unverified(format!("the signed catalog entry is unusable: {problem}"));
    }
    if entry.package_sha256 != provenance.entry_sha256 {
        return unverified(format!(
            "the signed catalog now pins a different package for version {version}"
        ));
    }
    if record.install_dir != install::package_dir(packages_root, record.id()) {
        return unverified(
            "its files are not in the folder the official installer puts them in".into(),
        );
    }
    match tree_digest(&record.install_dir) {
        Ok(digest) if digest == entry.package_tree_sha256 => {}
        Ok(_) => {
            return unverified(
                "files in the plugin's folder changed after it was installed from the official \
                 package"
                    .into(),
            )
        }
        Err(e) => return unverified(format!("its installed files could not be verified: {e}")),
    }
    // The folder is now known to be the signed one. The record in local state
    // must say what that folder says.
    let on_disk = std::fs::read_to_string(
        record
            .install_dir
            .join(agora_plugin_api::manifest::MANIFEST_FILENAME),
    )
    .map_err(|e| e.to_string())
    .and_then(|text| {
        agora_plugin_api::manifest::PluginManifest::parse(&text).map_err(|e| e.message)
    });
    match on_disk {
        Ok(manifest) if manifest == record.manifest => {}
        Ok(_) => {
            return unverified(
                "the plugin's stored manifest differs from the one in its official package".into(),
            )
        }
        Err(e) => return unverified(format!("the manifest in its folder could not be read: {e}")),
    }
    let asked: std::collections::BTreeSet<agora_plugin_api::Capability> = record
        .manifest
        .capabilities
        .required
        .iter()
        .chain(record.manifest.capabilities.optional.iter())
        .filter_map(|name| name.parse::<agora_plugin_api::Capability>().ok())
        .collect();
    if let Some(extra) = record.granted.iter().find(|cap| !asked.contains(cap)) {
        return unverified(format!(
            "it holds the capability `{}`, which its official manifest never asks for",
            extra.as_str()
        ));
    }
    OfficialStatus::Official { version }
}

/// The standing of every installed plugin that is official, or uses an official
/// plugin's id.
///
/// A plugin that is neither is absent from the map (community). The signed
/// registry is read only when something is installed, and only the plugins that
/// were installed as official, or whose id the catalog lists, are evaluated.
///
/// If the registry cannot be verified, every plugin installed as official is
/// reported [`OfficialStatus::Unverified`] with that reason; the error is not
/// swallowed into "no official plugins". Plugins that were never installed as
/// official cannot be told from community plugins without a catalog, so they stay
/// community.
pub fn verify_installed(
    conn: &Connection,
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<BTreeMap<String, OfficialStatus>> {
    let records: Vec<store::PluginRecord> = store::list(conn)?
        .into_iter()
        .filter(|record| !store::is_tombstone(record))
        .collect();
    if records.is_empty() {
        return Ok(BTreeMap::new());
    }
    let provenance = store::list_official(conn)?;
    let catalog = load_signed_catalog(trust, app_data_dir);
    let packages_root =
        crate::app_paths::AppPaths::from_root(app_data_dir.to_path_buf()).plugin_packages_root();
    let mut out = BTreeMap::new();
    for record in &records {
        let id = record.id().as_str();
        let recorded = provenance.get(id);
        match &catalog {
            Ok(catalog) => {
                if recorded.is_some() || catalog.has_id(id) {
                    out.insert(
                        id.to_string(),
                        evaluate(recorded, record, catalog, &packages_root),
                    );
                }
            }
            Err(e) => {
                if recorded.is_some() {
                    out.insert(
                        id.to_string(),
                        OfficialStatus::Unverified {
                            reason: unavailable_reason(e),
                        },
                    );
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// How a package's bytes are obtained. Injected so tests never touch a network.
#[async_trait::async_trait]
pub trait PackageFetcher: Send + Sync {
    /// Fetch `url`, which the signed registry named; `pinned_host` is that URL's host.
    async fn fetch(&self, url: &str, pinned_host: &str) -> LauncherResult<Vec<u8>>;
}

/// Fetches through the core HTTP client: HTTPS only, pinned to the signed
/// entry's host, same-host redirects, Lockdown Mode and the private-address
/// floor all still applying.
pub struct HttpPackageFetcher<'a> {
    pub clients: &'a crate::http_client::HttpClients,
}

#[async_trait::async_trait]
impl PackageFetcher for HttpPackageFetcher<'_> {
    async fn fetch(&self, url: &str, pinned_host: &str) -> LauncherResult<Vec<u8>> {
        crate::download::download_pinned_bytes(self.clients, url, pinned_host).await
    }
}

/// Size, then hash, against the signed entry. Nothing has been written yet.
fn verify_package_bytes(entry: &OfficialEntry, bytes: &[u8]) -> LauncherResult<()> {
    if bytes.len() as u64 != entry.package_size {
        return Err(refused(
            "ERR_OFFICIAL_PLUGIN_SIZE",
            format!(
                "The download for `{}` is {} bytes, but the signed catalog says {}. \
                 Nothing was installed.",
                entry.plugin_id,
                bytes.len(),
                entry.package_size
            ),
        ));
    }
    if crate::download::sha256_hex(bytes) != entry.package_sha256 {
        return Err(refused(
            "ERR_OFFICIAL_PLUGIN_HASH",
            format!(
                "The download for `{}` does not match the SHA-256 in the signed catalog, so it \
                 was refused. Nothing was installed.",
                entry.plugin_id
            ),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What the manager and the CLI show
// ---------------------------------------------------------------------------

/// Where an official plugin stands on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OfficialInstallState {
    NotInstalled,
    /// Installed, verified official, at the current version.
    UpToDate,
    /// Installed, verified official, at another version than the current one.
    UpdateAvailable,
    /// A plugin with this id is installed but is not verified official (a
    /// community copy, or one that lost its standing). Updating replaces it
    /// with the official package.
    NotOfficial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficialPluginInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Games this plugin adds, if any.
    pub game_ids: Vec<String>,
    /// The version an install or update moves to.
    pub version: String,
    /// The version a rollback returns to, if the catalog keeps one.
    pub previous_version: Option<String>,
    pub size_bytes: u64,
    pub min_agora_version: String,
    pub state: OfficialInstallState,
    pub installed_version: Option<String>,
    /// A rollback is possible: the installed copy is verified official and the
    /// catalog keeps a previous version other than the one installed.
    pub can_roll_back: bool,
    /// The installed copy's standing; `None` when nothing is installed.
    pub installed_status: Option<OfficialStatus>,
    /// Why it cannot be installed or updated right now, if it cannot.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficialPluginList {
    pub plugins: Vec<OfficialPluginInfo>,
    /// Set when the catalog could not be verified or read. The list is then
    /// empty on purpose: an unverified catalog offers nothing.
    pub unavailable_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

impl PluginService {
    /// The standing of one installed plugin right now.
    pub fn official_status(&self, plugin_id: &PluginId) -> LauncherResult<OfficialStatus> {
        let conn = self.inner.conn()?;
        let statuses = verify_installed(
            &conn,
            self.inner.ctx.paths.root(),
            &self.inner.registry_trust,
        )?;
        Ok(statuses
            .get(plugin_id.as_str())
            .cloned()
            .unwrap_or(OfficialStatus::Community))
    }

    /// The official plugins the signed catalog offers, and where each stands.
    pub fn official_plugins(&self) -> LauncherResult<OfficialPluginList> {
        let catalog =
            match load_signed_catalog(&self.inner.registry_trust, self.inner.ctx.paths.root()) {
                Ok(catalog) => catalog,
                Err(e) => {
                    return Ok(OfficialPluginList {
                        plugins: Vec::new(),
                        unavailable_reason: Some(unavailable_reason(&e)),
                    })
                }
            };
        let conn = self.inner.conn()?;
        let records = store::list(&conn)?;
        let statuses = verify_installed(
            &conn,
            self.inner.ctx.paths.root(),
            &self.inner.registry_trust,
        )?;

        let mut plugins = Vec::new();
        for current in catalog
            .entries()
            .iter()
            .filter(|entry| entry.channel == EntryChannel::Current)
        {
            let installed = records.iter().find(|record| {
                record.id().as_str() == current.plugin_id && !store::is_tombstone(record)
            });
            let previous = catalog.previous(&current.plugin_id);
            let installed_version = installed.map(|record| record.manifest.version.to_string());
            let installed_status = installed.map(|record| {
                statuses
                    .get(record.id().as_str())
                    .cloned()
                    .unwrap_or(OfficialStatus::Community)
            });
            let state = match (&installed_version, &installed_status) {
                (Some(version), Some(status)) if status.is_official() => {
                    if *version == current.version {
                        OfficialInstallState::UpToDate
                    } else {
                        OfficialInstallState::UpdateAvailable
                    }
                }
                (Some(_), _) => OfficialInstallState::NotOfficial,
                (None, _) => OfficialInstallState::NotInstalled,
            };
            let problem = current
                .check()
                .err()
                .map(|p| format!("the catalog entry is unusable: {p}"))
                .or_else(|| current.incompatible_reason());
            plugins.push(OfficialPluginInfo {
                id: current.plugin_id.clone(),
                name: current.name.clone(),
                description: current.description.clone(),
                game_ids: current.game_ids.clone(),
                version: current.version.clone(),
                previous_version: previous.map(|entry| entry.version.clone()),
                size_bytes: current.package_size,
                min_agora_version: current.min_agora_version.clone(),
                state,
                can_roll_back: matches!(
                    state,
                    OfficialInstallState::UpToDate | OfficialInstallState::UpdateAvailable
                ) && previous.is_some_and(|entry| {
                    installed_version.as_deref() != Some(entry.version.as_str())
                }),
                installed_version,
                installed_status,
                problem,
            });
        }
        Ok(OfficialPluginList {
            plugins,
            unavailable_reason: None,
        })
    }

    /// The fetcher the apps use: the core HTTP client, pinned to the signed
    /// entry's host. Tests pass their own [`PackageFetcher`] instead.
    pub fn http_fetcher(&self) -> HttpPackageFetcher<'_> {
        HttpPackageFetcher {
            clients: &self.inner.ctx.http_clients,
        }
    }

    /// Install an official plugin that is not installed yet.
    pub async fn install_official(
        &self,
        plugin_id: &str,
        consent: impl Into<Consent>,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        let catalog = self.signed_catalog()?;
        let entry = known_entry(catalog.current(plugin_id), plugin_id)?;
        if self.live_version(&entry.plugin_id)?.is_some() {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_INSTALLED",
                format!(
                    "`{plugin_id}` is already installed. Use update to move it to the official \
                     package."
                ),
            ));
        }
        self.install_entry(entry, consent, fetcher).await
    }

    /// Move an installed plugin to the catalog's current official version.
    ///
    /// A same-id plugin that is not verified official (a community copy, or one
    /// that lost its standing) is replaced by the official package.
    pub async fn update_official(
        &self,
        plugin_id: &str,
        consent: impl Into<Consent>,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        let catalog = self.signed_catalog()?;
        let entry = known_entry(catalog.current(plugin_id), plugin_id)?;
        let Some(installed) = self.live_version(&entry.plugin_id)? else {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_NOT_INSTALLED",
                format!("`{plugin_id}` is not installed. Use install."),
            ));
        };
        let id = PluginId::parse(&entry.plugin_id)
            .map_err(|e| refused("ERR_OFFICIAL_PLUGIN_ENTRY", e.message))?;
        if installed == entry.version && self.official_status(&id)?.is_official() {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_CURRENT",
                format!("`{plugin_id}` is already at the current official version {installed}."),
            ));
        }
        self.install_entry(entry, consent, fetcher).await
    }

    /// Return an official plugin to the version before the current one.
    ///
    /// Only for a plugin that is verified official, and only when the catalog
    /// still keeps a previous entry. Goes through the same download, hash check
    /// and installer as an update.
    pub async fn rollback_official(
        &self,
        plugin_id: &str,
        consent: impl Into<Consent>,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        let catalog = self.signed_catalog()?;
        // Existence is checked through the current entry so an unknown id gets
        // the same answer here as everywhere else.
        known_entry(catalog.current(plugin_id), plugin_id)?;
        let previous = catalog.previous(plugin_id).ok_or_else(|| {
            refused(
                "ERR_OFFICIAL_PLUGIN_NO_PREVIOUS",
                format!("The catalog keeps no previous version of `{plugin_id}` to return to."),
            )
        })?;
        let Some(installed) = self.live_version(&previous.plugin_id)? else {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_NOT_INSTALLED",
                format!("`{plugin_id}` is not installed."),
            ));
        };
        let id = PluginId::parse(&previous.plugin_id)
            .map_err(|e| refused("ERR_OFFICIAL_PLUGIN_ENTRY", e.message))?;
        if !self.official_status(&id)?.is_official() {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_NOT_OFFICIAL",
                format!(
                    "`{plugin_id}` is not a verified official install, so there is nothing \
                     official to roll back. Use update to replace it with the official package."
                ),
            ));
        }
        if installed == previous.version {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_CURRENT",
                format!("`{plugin_id}` is already at the previous version {installed}."),
            ));
        }
        self.install_entry(previous, consent, fetcher).await
    }

    /// The signed catalog, or the reason there is not one. Never an empty fallback.
    fn signed_catalog(&self) -> LauncherResult<OfficialCatalog> {
        load_signed_catalog(&self.inner.registry_trust, self.inner.ctx.paths.root()).map_err(|e| {
            refused(
                "ERR_OFFICIAL_PLUGIN_CATALOG",
                format!(
                    "Official plugins are unavailable: {}",
                    unavailable_reason(&e)
                ),
            )
        })
    }

    /// Version of the live (non-tombstone) install under this id, if any.
    fn live_version(&self, plugin_id: &str) -> LauncherResult<Option<String>> {
        let conn = self.inner.conn()?;
        Ok(store::list(&conn)?
            .into_iter()
            .find(|record| record.id().as_str() == plugin_id && !store::is_tombstone(record))
            .map(|record| record.manifest.version.to_string()))
    }

    /// Download one signed entry, verify it, and install it.
    async fn install_entry(
        &self,
        entry: &OfficialEntry,
        consent: impl Into<Consent>,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        entry.check().map_err(|problem| {
            refused(
                "ERR_OFFICIAL_PLUGIN_ENTRY",
                format!(
                    "The catalog entry for `{}` {} cannot be used: {problem}",
                    entry.plugin_id, entry.version
                ),
            )
        })?;
        if let Some(reason) = entry.incompatible_reason() {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_INCOMPATIBLE",
                format!(
                    "`{}` {} cannot be installed on this version of Agora: {reason}.",
                    entry.plugin_id, entry.version
                ),
            ));
        }
        // `check` proved the URL has a plain host; this is that host.
        let host = package_host(&entry.package_url).ok_or_else(|| {
            refused(
                "ERR_OFFICIAL_PLUGIN_ENTRY",
                "the package URL has no usable host",
            )
        })?;

        let bytes = fetcher.fetch(&entry.package_url, &host).await?;
        // Before anything is written anywhere.
        verify_package_bytes(entry, &bytes)?;
        // The bytes are what the entry pins; the files they unpack to must be
        // what it pins too, or the installed copy could never verify.
        if zip_tree_digest(&bytes)? != entry.package_tree_sha256 {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_TREE",
                format!(
                    "The files in the package for `{}` {} are not the ones the signed catalog \
                     pins. Nothing was installed.",
                    entry.plugin_id, entry.version
                ),
            ));
        }
        self.install_verified(entry, &bytes, consent)
    }

    /// Install bytes that have been checked against `entry`, from memory.
    ///
    /// The manifest is read from, and the files are unpacked from, these same
    /// bytes: no file is written and re-opened by path in between.
    fn install_verified(
        &self,
        entry: &OfficialEntry,
        bytes: &[u8],
        consent: impl Into<Consent>,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        let preview: InstallPreview = self.preview_package_bytes(bytes)?;
        // The signed entry decides what these bytes are meant to be. A package
        // that says it is something else is refused even though its hash matched.
        if preview.manifest.id.as_str() != entry.plugin_id
            || preview.manifest.version.to_string() != entry.version
        {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_MISMATCH",
                format!(
                    "The package for `{}` {} identifies itself as `{}` {}. It was refused.",
                    entry.plugin_id, entry.version, preview.manifest.id, preview.manifest.version
                ),
            ));
        }
        // Official plugins are data-only: they ship a game definition, not a
        // script. A package with an entrypoint is Agora's own release mistake,
        // not something a user could meaningfully consent to, so it is refused.
        if preview.runs_code {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_HAS_CODE",
                format!(
                    "The official package for `{}` {} contains code (`{}`). Official plugins \
                     are data-only, so nothing was installed.",
                    entry.plugin_id,
                    entry.version,
                    preview
                        .manifest
                        .entrypoint
                        .as_deref()
                        .unwrap_or("an entrypoint")
                ),
            ));
        }
        if preview.requires_capability_consent() && !consent.capabilities {
            return Ok(UpdateOutcome::NeedsConsent {
                preview: Box::new(preview),
            });
        }

        // The ordinary installer: same consent comparison, data checkpoint,
        // staging and rollback as any package. It also clears any earlier
        // official record, which is rewritten below only on success.
        let installed: PluginSummary = self.install_package_bytes(bytes, consent)?;
        let id = PluginId::parse(&installed.id)
            .map_err(|e| refused("ERR_OFFICIAL_PLUGIN_ENTRY", e.message))?;

        let conn = self.inner.conn()?;
        let record = store::get(&conn, &id)?.ok_or_else(|| {
            refused(
                "ERR_PLUGIN_NOT_FOUND",
                format!("`{id}` is not installed after installing it"),
            )
        })?;
        let unpacked = tree_digest(&record.install_dir).map_err(|e| {
            refused(
                "ERR_OFFICIAL_PLUGIN_RECORD",
                format!(
                    "`{id}` was installed but its files could not be verified, so it is \
                     blocked: {e}"
                ),
            )
        })?;
        if unpacked != entry.package_tree_sha256 {
            // Not recorded as official: it will not verify, and says so.
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_TREE",
                format!(
                    "`{id}` unpacked to files that are not the ones the signed catalog pins, so \
                     it is blocked. Update or reinstall it."
                ),
            ));
        }
        store::set_official(
            &conn,
            &id,
            &OfficialProvenance {
                entry_id: entry.plugin_id.clone(),
                entry_version: entry.version.clone(),
                entry_sha256: entry.package_sha256.clone(),
                // Informational: the check uses the digest the signed entry pins.
                tree_sha256: unpacked,
                recorded_at: chrono::DateTime::<chrono::Utc>::from(self.inner.ctx.clock.now())
                    .to_rfc3339(),
            },
        )?;
        drop(conn);
        // Resolution was last computed before the record above existed.
        self.reload()?;

        Ok(UpdateOutcome::Installed {
            plugin: Box::new(self.summary(&id)?),
        })
    }
}

fn known_entry<'a>(
    entry: Option<&'a OfficialEntry>,
    plugin_id: &str,
) -> LauncherResult<&'a OfficialEntry> {
    entry.ok_or_else(|| {
        refused(
            "ERR_OFFICIAL_PLUGIN_UNKNOWN",
            format!("The signed catalog lists no official plugin `{plugin_id}`."),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_host_accepts_a_plain_https_host_only() {
        assert_eq!(
            package_host("https://example.com/a/b.zip").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            package_host("https://Example.COM").as_deref(),
            Some("example.com")
        );
        for bad in [
            "",
            "http://example.com/a.zip",
            "ftp://example.com/a.zip",
            "https://",
            "https:///a.zip",
            "https://user@example.com/a.zip",
            "https://example.com:8443/a.zip",
            "https://exa mple.com/a.zip",
            " https://example.com/a.zip",
            "https://-bad.example/a.zip",
            "//example.com/a.zip",
            "example.com/a.zip",
        ] {
            assert_eq!(package_host(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn tree_digest_notices_a_change_an_addition_and_a_rename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.json"), b"one").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.json"), b"two").unwrap();
        let base = tree_digest(dir.path()).unwrap();
        assert_eq!(base, tree_digest(dir.path()).unwrap(), "stable");

        std::fs::write(dir.path().join("a.json"), b"ONE").unwrap();
        let changed = tree_digest(dir.path()).unwrap();
        assert_ne!(base, changed);

        std::fs::write(dir.path().join("a.json"), b"one").unwrap();
        assert_eq!(base, tree_digest(dir.path()).unwrap());

        std::fs::write(dir.path().join("extra.txt"), b"").unwrap();
        assert_ne!(base, tree_digest(dir.path()).unwrap());
        std::fs::remove_file(dir.path().join("extra.txt")).unwrap();

        std::fs::rename(dir.path().join("sub/b.json"), dir.path().join("sub/c.json")).unwrap();
        assert_ne!(base, tree_digest(dir.path()).unwrap());
    }

    #[test]
    fn tree_digest_of_a_missing_folder_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(tree_digest(&dir.path().join("gone")).is_err());
    }
}
