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
//! Lose one of the *file* checks — the files were edited, the catalog explicitly
//! revoked the version, or this copy was never the official package — and the
//! plugin is [`OfficialStatus::Unverified`], with the reason. That is **blocked**,
//! not demoted to a community plugin: it is not loaded, whatever the emergency
//! stop says, until it is updated, rolled back or reinstalled.
//!
//! Not being able to *check* is different. If the catalog is missing, corrupt or
//! offline, or it no longer lists an old version, the installed files are checked
//! against the hashes recorded when they were installed: matching files keep
//! loading ([`OfficialStatus::CatalogUnavailable`] / [`OfficialStatus::NotListed`])
//! with a warning, and any scripts among them wait until a verified catalog says
//! the plugin is official again. A player's game does not disappear because the
//! network did.
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
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

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
    /// Explicitly withdrawn by the signed catalog. Distinct from "no longer
    /// listed", which only means the catalog keeps a short history.
    pub revoked: bool,
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
        // `revoked` is newer than the table. An older cached registry does not
        // have it, which reads as "nothing revoked" rather than an error.
        let has_revoked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('official_plugins') WHERE name = 'revoked'",
                [],
                |row| row.get(0),
            )
            .map_err(unreadable)?;
        let revoked_column = if has_revoked > 0 { "revoked" } else { "0" };
        let sql = format!(
            "SELECT plugin_id, version, channel, name, description, game_ids_json,
                    package_url, package_sha256, package_size, min_agora_version,
                    package_tree_sha256, {revoked_column}
             FROM official_plugins ORDER BY plugin_id, channel, version"
        );
        let mut stmt = conn.prepare(&sql).map_err(unreadable)?;
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
                    row.get::<_, i64>(11)?,
                ))
            })
            .map_err(unreadable)?;
        let mut entries = Vec::new();
        for row in rows {
            let (
                id,
                version,
                channel,
                name,
                description,
                games,
                url,
                sha,
                size,
                min,
                tree,
                revoked,
            ) = row.map_err(unreadable)?;
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
                revoked: revoked != 0,
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
    /// The signed catalog could not be checked (missing, corrupt or offline).
    /// The installed files match the hashes recorded when they were installed,
    /// so the plugin keeps loading with a warning. Its scripts do not run until
    /// the catalog verifies again.
    CatalogUnavailable { reason: String },
    /// The catalog verified, but it no longer lists this installed version
    /// (usually because it is two or more releases behind). The installed files
    /// match the hashes recorded at install, so it keeps loading with a warning
    /// and an update, if the catalog offers one.
    NotListed { reason: String },
}

impl OfficialStatus {
    pub fn is_official(&self) -> bool {
        matches!(self, OfficialStatus::Official { .. })
    }

    /// Whether the plugin's game packages load in this state.
    ///
    /// Only a hard block — tampered files, explicit revocation, or an impostor
    /// id — stops them. A catalog that cannot be checked, or that dropped an
    /// old version, does not make the player's game disappear.
    pub fn loads(&self) -> bool {
        !matches!(self, OfficialStatus::Unverified { .. })
    }

    /// The warning to show next to the plugin and its games, when it loads in a
    /// state that could not be verified.
    pub fn warning(&self) -> Option<&str> {
        match self {
            OfficialStatus::CatalogUnavailable { reason }
            | OfficialStatus::NotListed { reason } => Some(reason),
            _ => None,
        }
    }

    /// Whether this standing permits the plugin's scripts to run. Only a
    /// verified official, or a community plugin (absent from the map), does;
    /// the two warning states are specifically "loads data only until the
    /// catalog verifies again".
    pub fn scripts_allowed_by_status(&self) -> bool {
        matches!(
            self,
            OfficialStatus::Official { .. } | OfficialStatus::Community
        )
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
    // The recorded files are checked before the catalog is consulted, so a
    // tampered install is blocked in every state, including "catalog offline".
    if let Err(reason) = verify_recorded_files(provenance, record, packages_root) {
        return unverified(reason);
    }
    let Some(entry) = catalog.find(id, &version) else {
        return OfficialStatus::NotListed {
            reason: format!(
                "a newer version is available; version {version} is no longer listed by the \
                 catalog"
            ),
        };
    };
    if entry.revoked {
        return unverified(format!(
            "the signed catalog explicitly revokes version {version} of `{id}`"
        ));
    }
    if let Err(problem) = entry.check() {
        return unverified(format!("the signed catalog entry is unusable: {problem}"));
    }
    if entry.package_sha256 != provenance.entry_sha256 {
        return unverified(format!(
            "the signed catalog now pins a different package for version {version}"
        ));
    }
    if entry.package_tree_sha256 != provenance.tree_sha256 {
        return unverified(format!(
            "the signed catalog now pins different files for version {version}"
        ));
    }
    OfficialStatus::Official { version }
}

/// Check the installed files against what was recorded when they were installed.
///
/// Used when there is no signed entry to compare with (the catalog is missing,
/// or has dropped this version). The hashes are local state, not a signature, so
/// this detects an edited install but cannot vouch for its provenance; that is
/// exactly the "keeps loading, with a warning" case.
fn verify_recorded_files(
    provenance: &OfficialProvenance,
    record: &store::PluginRecord,
    packages_root: &Path,
) -> Result<(), String> {
    if record.install_dir != install::package_dir(packages_root, record.id()) {
        return Err("its files are not in the folder the official installer puts them in".into());
    }
    match tree_digest(&record.install_dir) {
        Ok(digest) if digest == provenance.tree_sha256 => {}
        Ok(_) => {
            return Err(
                "files in the plugin's folder changed after it was installed from the official \
                 package"
                    .into(),
            )
        }
        Err(e) => return Err(format!("its installed files could not be verified: {e}")),
    }
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
            return Err(
                "the plugin's stored manifest differs from the one in its official package".into(),
            )
        }
        Err(e) => return Err(format!("the manifest in its folder could not be read: {e}")),
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
        return Err(format!(
            "it holds the capability `{}`, which its official manifest never asks for",
            extra.as_str()
        ));
    }
    Ok(())
}

/// A plugin installed from the signed catalog while the catalog cannot be read.
///
/// The installed files still have to match what was recorded; if they do, the
/// plugin keeps loading with a warning instead of vanishing until the network or
/// the registry comes back.
fn evaluate_unavailable(
    provenance: &OfficialProvenance,
    record: &store::PluginRecord,
    packages_root: &Path,
    reason: &str,
) -> OfficialStatus {
    match verify_recorded_files(provenance, record, packages_root) {
        Ok(()) => OfficialStatus::CatalogUnavailable {
            reason: format!(
                "Agora could not check this plugin against the catalog ({reason}). Its installed \
                 files match what was installed."
            ),
        },
        Err(problem) => OfficialStatus::Unverified { reason: problem },
    }
}

/// The standing of every installed plugin that is official, or uses an official
/// plugin's id.
///
/// A plugin that is neither is absent from the map (community). The signed
/// registry is read only when something is installed, and only the plugins that
/// were installed as official, or whose id the catalog lists, are evaluated.
///
/// If the registry cannot be verified, every plugin installed as official keeps
/// loading in [`OfficialStatus::CatalogUnavailable`] when its files still match
/// what was installed, and is [`OfficialStatus::Unverified`] when they do not;
/// the error is not swallowed into "no official plugins". Plugins that were never
/// installed as official cannot be told from community plugins without a
/// catalog, so they stay community.
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
                if let Some(recorded) = recorded {
                    out.insert(
                        id.to_string(),
                        evaluate_unavailable(
                            recorded,
                            record,
                            &packages_root,
                            &unavailable_reason(e),
                        ),
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
                    // The catalog cannot offer anything, but an installed
                    // official plugin still has a standing and a warning, and
                    // the Game support list needs it to warn on the game.
                    let reason = unavailable_reason(&e);
                    let conn = self.inner.conn()?;
                    let records = store::list(&conn)?;
                    let statuses = verify_installed(
                        &conn,
                        self.inner.ctx.paths.root(),
                        &self.inner.registry_trust,
                    )?;
                    let plugins = records
                        .iter()
                        .filter(|record| !store::is_tombstone(record))
                        .filter_map(|record| {
                            let status = statuses.get(record.id().as_str())?.clone();
                            let installed_version = Some(record.manifest.version.to_string());
                            Some(OfficialPluginInfo {
                                id: record.id().to_string(),
                                name: record.manifest.name.clone(),
                                description: record.manifest.description.clone(),
                                game_ids: Vec::new(),
                                version: installed_version.clone().unwrap_or_default(),
                                previous_version: None,
                                size_bytes: 0,
                                min_agora_version: "0.0.0".into(),
                                state: OfficialInstallState::NotOfficial,
                                can_roll_back: false,
                                installed_version,
                                installed_status: Some(status),
                                problem: None,
                            })
                        })
                        .collect();
                    return Ok(OfficialPluginList {
                        plugins,
                        unavailable_reason: Some(reason),
                    });
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
    ///
    /// Missing or outdated official dependencies named by its manifest are
    /// installed (or updated) first, from the same signed catalog, under the
    /// same consent. Nothing is written until every package in the set has been
    /// fetched, checked and covered by the consent.
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
        let plans = self.plan_official_install(entry, &catalog, fetcher).await?;
        self.finish_official_install(plans, consent)
    }

    /// Move an installed plugin to the catalog's current official version.
    ///
    /// A same-id plugin that is not verified official (a community copy, or one
    /// that lost its standing) is replaced by the official package. Dependencies
    /// the new version needs are brought up to date first, as on install.
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
        let plans = self.plan_official_install(entry, &catalog, fetcher).await?;
        self.finish_official_install(plans, consent)
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

    /// Download one signed entry and verify it, writing nothing.
    async fn fetch_verified_entry(
        &self,
        entry: &OfficialEntry,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<Vec<u8>> {
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
        Ok(bytes)
    }

    /// Download one signed entry, verify it, and install it.
    async fn install_entry(
        &self,
        entry: &OfficialEntry,
        consent: impl Into<Consent>,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<UpdateOutcome> {
        let consent: Consent = consent.into();
        let bytes = self.fetch_verified_entry(entry, fetcher).await?;
        self.install_verified(entry, &bytes, consent)
    }

    /// Fetch and check every package an official install will write, dependencies
    /// first, and return them in that order. Nothing is written here.
    ///
    /// Dependencies come from each package's own manifest and are resolved
    /// against the signed catalog: installed and satisfying means untouched;
    /// installed but too old means the catalog's current version, if it
    /// satisfies; missing means the catalog's current version, if it satisfies.
    /// A dependency the catalog cannot offer, a version that does not satisfy,
    /// or a cycle refuses the whole install before any file is written.
    async fn plan_official_install(
        &self,
        entry: &OfficialEntry,
        catalog: &OfficialCatalog,
        fetcher: &dyn PackageFetcher,
    ) -> LauncherResult<Vec<PlannedInstall>> {
        let mut state = PlanState::default();
        self.plan_install(entry, catalog, fetcher, false, &mut state)
            .await?;
        Ok(state.plans)
    }

    fn plan_install<'a>(
        &'a self,
        entry: &'a OfficialEntry,
        catalog: &'a OfficialCatalog,
        fetcher: &'a dyn PackageFetcher,
        is_dependency: bool,
        state: &'a mut PlanState,
    ) -> Pin<Box<dyn Future<Output = LauncherResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let id = entry.plugin_id.clone();
            if state.planned.contains(&id) {
                return Ok(());
            }
            state.visiting.insert(id.clone());
            let bytes = self.fetch_verified_entry(entry, fetcher).await?;
            let preview = self.preview_package_bytes(&bytes)?;
            if preview.manifest.id.as_str() != entry.plugin_id
                || preview.manifest.version.to_string() != entry.version
            {
                return Err(refused(
                    "ERR_OFFICIAL_PLUGIN_MISMATCH",
                    format!(
                        "The package for `{}` {} identifies itself as `{}` {}. It was refused.",
                        entry.plugin_id,
                        entry.version,
                        preview.manifest.id,
                        preview.manifest.version
                    ),
                ));
            }
            let dependencies: Vec<(PluginId, VersionReq)> = preview
                .manifest
                .dependencies
                .iter()
                .map(|(dependency, requirement)| (dependency.clone(), requirement.clone()))
                .collect();
            for (dependency, requirement) in dependencies {
                let dependency_id = dependency.as_str().to_string();
                let installed_satisfies = self
                    .live_version(&dependency_id)?
                    .and_then(|version| Version::parse(&version).ok())
                    .is_some_and(|version| requirement.matches(&version));
                if installed_satisfies {
                    continue;
                }
                let dependency_entry = catalog.current(&dependency_id).ok_or_else(|| {
                    dependency_refused(&dependency_id, &requirement, "is not in the signed catalog")
                })?;
                let dependency_version =
                    Version::parse(&dependency_entry.version).map_err(|_| {
                        dependency_refused(
                            &dependency_id,
                            &requirement,
                            &format!(
                                "the catalog lists the version `{}`, which cannot be read",
                                dependency_entry.version
                            ),
                        )
                    })?;
                if !requirement.matches(&dependency_version) {
                    return Err(dependency_refused(
                        &dependency_id,
                        &requirement,
                        &format!(
                            "the catalog's current version {} does not satisfy it",
                            dependency_entry.version
                        ),
                    ));
                }
                if state.visiting.contains(&dependency_id) {
                    return Err(dependency_refused(
                        &dependency_id,
                        &requirement,
                        "forms a dependency cycle",
                    ));
                }
                let dependency_entry = dependency_entry.clone();
                self.plan_install(&dependency_entry, catalog, fetcher, true, state)
                    .await?;
            }
            state.visiting.remove(&id);
            state.planned.insert(id);
            state.plans.push(PlannedInstall {
                entry: entry.clone(),
                preview,
                bytes,
                is_dependency,
            });
            Ok(())
        })
    }

    /// Apply a plan the consent covers, dependencies first.
    fn finish_official_install(
        &self,
        plans: Vec<PlannedInstall>,
        consent: Consent,
    ) -> LauncherResult<UpdateOutcome> {
        let Some(target) = plans.last() else {
            return Err(refused(
                "ERR_OFFICIAL_PLUGIN_ENTRY",
                "there is nothing to install",
            ));
        };
        let mut needs_consent = false;
        for plan in &plans {
            // A dependency that runs code is a new answer exactly as it would be
            // on its own: the one consent has to cover running it.
            let dependency_code = plan.is_dependency
                && plan.preview.replaces_version.is_none()
                && plan.preview.runs_code
                && !consent.scripts;
            if (plan.preview.requires_capability_consent() && !consent.capabilities)
                || (plan.preview.replaces_version.is_some()
                    && plan.preview.adds_code
                    && !consent.scripts)
                || dependency_code
            {
                needs_consent = true;
            }
        }
        if needs_consent {
            let mut preview = target.preview.clone();
            preview.also_installs = plans[..plans.len() - 1]
                .iter()
                .map(|plan| plan.preview.clone())
                .collect();
            return Ok(UpdateOutcome::NeedsConsent {
                preview: Box::new(preview),
            });
        }

        let mut installed: Vec<String> = Vec::new();
        let mut target_summary = None;
        for plan in &plans {
            match self.install_verified(&plan.entry, &plan.bytes, consent) {
                Ok(UpdateOutcome::Installed { plugin }) => {
                    installed.push(plan.entry.plugin_id.clone());
                    if !plan.is_dependency {
                        target_summary = Some(plugin);
                    }
                }
                Ok(UpdateOutcome::NeedsConsent { .. }) => {
                    return Err(partial_failure(
                        &installed,
                        &plan.entry.plugin_id,
                        "it asked for consent again after the install had started",
                    ));
                }
                Err(error) => {
                    if installed.is_empty() {
                        return Err(error);
                    }
                    return Err(partial_failure(
                        &installed,
                        &plan.entry.plugin_id,
                        &error.to_string(),
                    ));
                }
            }
        }
        let plugin = target_summary.ok_or_else(|| {
            refused(
                "ERR_OFFICIAL_PLUGIN_ENTRY",
                "the target plugin was not installed",
            )
        })?;
        Ok(UpdateOutcome::Installed { plugin })
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
        // Official plugins may carry a script. The signed entry pins the whole
        // package tree (checked above and again after unpacking), so the script
        // is covered by the same hash as its data. An official plugin gets no
        // capability a community plugin could not request, and it goes through
        // the same script consent below.
        if preview.requires_capability_consent() && !consent.capabilities {
            return Ok(UpdateOutcome::NeedsConsent {
                preview: Box::new(preview),
            });
        }
        // An update that adds a script where there was none is a new decision
        // even when it asks for no new capability: the prompt names that it now
        // runs code. A first install's script consent is handled by the
        // ordinary installer below.
        if preview.replaces_version.is_some() && preview.adds_code && !consent.scripts {
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

/// One package an official install will write, fetched and checked already.
struct PlannedInstall {
    entry: OfficialEntry,
    preview: InstallPreview,
    bytes: Vec<u8>,
    /// True for a dependency of the target, false for the target itself.
    is_dependency: bool,
}

#[derive(Default)]
struct PlanState {
    /// Packages being resolved right now, to detect a cycle.
    visiting: BTreeSet<String>,
    /// Packages already added to the plan, so a shared dependency is planned once.
    planned: BTreeSet<String>,
    /// Dependencies before the packages that need them.
    plans: Vec<PlannedInstall>,
}

/// A dependency that cannot be resolved: the refusal names it and what it asked for.
fn dependency_refused(dependency: &str, requirement: &VersionReq, problem: &str) -> LauncherError {
    refused(
        "ERR_OFFICIAL_PLUGIN_DEPENDENCY",
        format!(
            "The official dependency `{dependency}` (`{requirement}`) {problem}. Nothing was \
             installed."
        ),
    )
}

/// A failure after at least one package of the set was written.
///
/// The installed ones are valid official plugins and are kept, so the error
/// says which stayed and which failed rather than pretending nothing happened.
fn partial_failure(installed: &[String], failed: &str, cause: &str) -> LauncherError {
    let kept = if installed.is_empty() {
        "nothing was installed".to_string()
    } else {
        format!("already installed: {}", installed.join(", "))
    };
    refused(
        "ERR_OFFICIAL_PLUGIN_PARTIAL",
        format!("`{failed}` failed to install: {cause}. {kept}."),
    )
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
