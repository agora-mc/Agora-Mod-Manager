//! Registry database sync â€” download, verify, and atomically replace the
//! signed `registry.db` from GitHub Releases.
//!
//! This module is pure (no Tauri types) so it can be consumed by the Tauri
//! GUI, the standalone `agora` CLI, and the in-process MCP listener.

use crate::db;
use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::LockManager;
use crate::operation_manager::OperationManager;
use crate::paths;
use crate::registry_bundle::{self, BundleError, CatalogHeader, RegistryTrust, VerifiedBundle};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Resolve the registry repository with priority:
/// 1. CLI override (passed as parameter)
/// 2. `AGORA_REGISTRY_REPO` environment variable
/// 3. Built-in default: `"agora-mc/Agora-Mod-Manager"`
pub fn resolve_registry_repo(cli_override: Option<&str>) -> String {
    cli_override
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("AGORA_REGISTRY_REPO").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agora-mc/Agora-Mod-Manager".into())
}

/// App-side expected schema version for registry.db.
///
/// Bumped to 10 alongside the compiler's crash-signature format v2: the
/// `crash_signatures` table now stores `games_json`, `level`, `cause`, `text`
/// and the action columns, and the app turns those rows into rules in the
/// community crash pack. An older catalog is still readable, but it has no v2
/// community signatures until it is refreshed. Compiler and app ship in
/// lockstep from a single commit, so clients always receive a matching signed
/// db via the update flow.
pub const APP_REGISTRY_SCHEMA_VERSION: i64 = 10;

/// Minimum interval between automatic update checks (1 hour).
const UPDATE_CHECK_INTERVAL_SECS: i64 = 3600;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryStatus {
    pub has_cached_db: bool,
    pub cached_tag: Option<String>,
    pub cached_schema_version: Option<i64>,
    pub latest_tag: Option<String>,
    pub update_available: bool,
    pub checked: bool,
    /// The catalog verified but its signed expiry has passed. It stays
    /// readable; installs and updates wait for a fresh copy.
    pub stale: bool,
    pub expires_at: Option<String>,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check GitHub Releases for a newer registry.db and download + verify it.
///
/// Steps (per section 4.1a):
/// 1. Query GitHub Releases API for the latest tagged release.
/// 2. Compare the tag with the locally cached tag.
/// 3. If newer: download registry.db and registry.db.sig from release assets.
/// 4. Verify the Ed25519 signature.
/// 5. Check schema_version for forward/backward compatibility.
/// 6. Atomically replace the cached DB (write .tmp, rename).
/// 7. Update the cached tag in local_state.db.
///
/// When `operation_manager` is provided, a "Update registry" operation is
/// registered for the duration of the download and verification.
pub async fn check_and_download_update(
    app_data_dir: &Path,
    local_state_path: &Path,
    force: bool,
    github_token: Option<String>,
    operation_manager: Option<&OperationManager>,
    registry_repo: &str,
    lock_manager: &LockManager,
) -> LauncherResult<RegistryStatus> {
    let conn =
        db::local_state_connection(local_state_path).map_err(|e| LauncherError::Generic {
            code: "ERR_DB".into(),
            message: e.to_string(),
        })?;
    if !db::is_network_enabled(&conn, "network_registry_sync_enabled") {
        return Err(LauncherError::Generic {
            code: "ERR_NETWORK_DISABLED".into(),
            message: "Catalog sync is disabled in Privacy settings.".into(),
        });
    }
    drop(conn);
    let cached_tag = get_cached_tag(local_state_path)?;
    let cached = cached_catalog_summary(app_data_dir);
    let has_cached_db = cached.is_some();
    let cached_schema_version = cached.as_ref().and_then(|summary| summary.schema_version);

    if !force {
        if let Some(last_check) = get_last_check_time(local_state_path)? {
            let now = chrono::Utc::now().timestamp();
            if now - last_check < UPDATE_CHECK_INTERVAL_SECS && cached_tag.is_some() {
                return Ok(RegistryStatus {
                    has_cached_db,
                    cached_tag,
                    cached_schema_version,
                    latest_tag: None,
                    update_available: false,
                    checked: false,
                    stale: cached.as_ref().is_some_and(|summary| summary.stale),
                    expires_at: cached.and_then(|summary| summary.expires_at),
                    message: "Catalog is up to date.".to_string(),
                });
            }
        }
    }

    // Acquire cross-process RegistryUpdate lock for the entire
    // check / download / atomic-replace / cached-tag update transaction.
    let _registry_lock = lock_manager.acquire(
        crate::lock_manager::LockResource::RegistryUpdate,
        "registry-sync",
    )?;

    let latest = match query_latest_release(registry_repo, github_token.as_deref()).await {
        Ok(release) => release,
        // With no cached catalog there is nothing to fall back to. Say what
        // failed rather than returning an empty status the UI reads as "no
        // catalog found".
        Err(e) if !has_cached_db => {
            return Err(e.while_attempting("download the catalog"));
        }
        Err(e) => {
            return Ok(RegistryStatus {
                has_cached_db,
                cached_tag,
                cached_schema_version,
                latest_tag: None,
                update_available: false,
                checked: false,
                stale: cached.as_ref().is_some_and(|summary| summary.stale),
                expires_at: cached.and_then(|summary| summary.expires_at),
                message: format!(
                    "{} Using the cached catalog.",
                    e.while_attempting("check for updates")
                ),
            });
        }
    };

    set_last_check_time(local_state_path, chrono::Utc::now().timestamp())?;

    // A merely *different* tag is not grounds to update: a signature stays
    // valid forever, so an older release republished as "latest" (or promoted
    // by release pruning) would otherwise be installed over a newer registry,
    // reinstating entries that curation has since quarantined.
    let latest_asset = latest.find_asset("registry.bundle");
    let cached_asset = get_cached_asset_id(local_state_path)?;
    let update_available = update_is_available(
        cached_tag.as_deref(),
        has_cached_db,
        cached_asset,
        &latest.tag_name,
        latest_asset,
    );

    if !update_available && has_cached_db {
        let cached = cached_catalog_summary(app_data_dir);
        return Ok(RegistryStatus {
            has_cached_db,
            cached_tag,
            cached_schema_version,
            latest_tag: Some(latest.tag_name.clone()),
            update_available: false,
            checked: true,
            stale: cached.as_ref().is_some_and(|summary| summary.stale),
            expires_at: cached.and_then(|summary| summary.expires_at),
            message: "Catalog is up to date.".to_string(),
        });
    }

    // Register operation only when we are about to start network I/O.
    let _op = operation_manager.map(|m| m.register("Update registry"));

    let bundle_id = latest
        .find_asset("registry.bundle")
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_REGISTRY_NO_BUNDLE".into(),
            message: format!(
                "Release {} does not carry a signed catalog bundle (registry.bundle), so it \
                 cannot be installed. The cached catalog has been kept.",
                latest.tag_name
            ),
        })?;

    // Download via the GitHub Assets API endpoint, which 302-redirects to a
    // signed URL. The `browser_download_url` returns 404 for this repo.
    let bundle_url = format!(
        "https://api.github.com/repos/{}/releases/assets/{}",
        registry_repo, bundle_id
    );
    let bundle_bytes = download_file(&bundle_url, github_token.as_deref()).await?;

    let trust = RegistryTrust::production();
    let verified = registry_bundle::verify_bundle(&bundle_bytes, &trust).map_err(bundle_error)?;

    // Anti-rollback, checked only after the signature so the compared stamp is
    // one the signing key vouched for. The tag pre-check above works on
    // unsigned API metadata and can be forged; this cannot.
    let candidate_built_at = Some(verified.header.built_at.clone());
    let cached_built_at = read_cached_built_at_checked(app_data_dir, &trust)?;
    if is_rollback(cached_built_at.as_deref(), candidate_built_at.as_deref()) {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_ROLLBACK".into(),
            message: format!(
                "Refusing registry {}: it was built at {}, older than the registry already \
                 installed ({}). The cached registry has been kept.",
                latest.tag_name,
                candidate_built_at.as_deref().unwrap_or("unknown"),
                cached_built_at.as_deref().unwrap_or("unknown"),
            ),
        });
    }

    // An expired bundle is readable but never installed as current: doing so
    // would replace a good cache with one that already cannot be used for an
    // install or update.
    if verified.header.is_expired_at(Utc::now()) {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_EXPIRED".into(),
            message: format!(
                "Refusing registry {}: it expired at {}. The cached registry has been kept.",
                latest.tag_name, verified.header.expires_at
            ),
        });
    }

    let schema_version = verified.header.schema_version;
    if schema_version > APP_REGISTRY_SCHEMA_VERSION {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_NEWER_SCHEMA".to_string(),
            message: format!(
                "Catalog schema version {} is newer than supported {}. Please update the app.",
                schema_version, APP_REGISTRY_SCHEMA_VERSION
            ),
        });
    }

    install_bundle_bytes(app_data_dir, &verified.header, &bundle_bytes, &trust)?;
    set_cached_tag(local_state_path, &latest.tag_name)?;
    set_cached_asset_id(local_state_path, Some(bundle_id))?;

    if let Some(op) = _op {
        op.complete();
    }

    Ok(RegistryStatus {
        has_cached_db: true,
        cached_tag: Some(latest.tag_name.clone()),
        cached_schema_version: Some(schema_version),
        latest_tag: Some(latest.tag_name.clone()),
        update_available: false,
        checked: true,
        stale: false,
        expires_at: Some(verified.header.expires_at.clone()),
        message: format!("Catalog updated to {}.", latest.tag_name),
    })
}

/// Outcome of the startup integrity check on the cached registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedRegistryIntegrity {
    /// No cached registry on disk - nothing to verify.
    Absent,
    /// The cached catalog verified and is current.
    Verified,
    /// The cached catalog verified but its signed expiry has passed. It stays
    /// readable; installs and updates wait for a fresh copy.
    Stale(String),
    /// The current bundle could not be used, so the previous verified copy was
    /// put back. The payload says why, for a startup warning.
    Recovered(String),
    /// The cached catalog failed verification and was quarantined. The payload is
    /// a human-readable reason suitable for a startup warning.
    Quarantined(String),
    /// The cached catalog failed verification and could not be moved aside, so it
    /// is still on disk. The payload is a human-readable reason; nothing may open
    /// the file in this run.
    Error(String),
}

/// A compact description of the cached catalog for status displays.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedCatalogSummary {
    schema_version: Option<i64>,
    stale: bool,
    expires_at: Option<String>,
}

/// Move `path` to `<name>.untrusted`, or to a numbered variant when that name
/// is already taken. Quarantine preserves the file for inspection; it never
/// deletes something at the target.
fn quarantine_aside(path: &Path, kind: &str) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut last_error = None;
    for suffix in std::iter::once(kind.to_string()).chain((1..=100).map(|n| format!("{kind}-{n}")))
    {
        let target = path.with_extension(suffix);
        match std::fs::rename(path, &target) {
            Ok(()) => return Ok(()),
            // Something else moved it; the untrusted file is no longer here.
            Err(_) if !path.exists() => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no free quarantine name")
    }))
}

// ---------------------------------------------------------------------------
// Bundle loading, with a per-process header cache
// ---------------------------------------------------------------------------

/// One bundle header that verified in this process, with the file identity it
/// was read from. Re-reading and re-verifying a multi-megabyte bundle for every
/// catalog read would be wasteful; the database itself is still hashed on every
/// open, so a swapped `registry.db` is always caught.
#[derive(Debug, Clone)]
struct CachedHeader {
    len: u64,
    modified: Option<std::time::SystemTime>,
    trust_fingerprint: String,
    header: CatalogHeader,
}

fn header_cache() -> &'static Mutex<HashMap<PathBuf, CachedHeader>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedHeader>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn trust_fingerprint(trust: &RegistryTrust) -> String {
    trust
        .keys()
        .iter()
        .map(|key| key.id.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug)]
enum BundleLoadError {
    Io(std::io::Error),
    Bundle(BundleError),
}

impl std::fmt::Display for BundleLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "it could not be read ({error})"),
            Self::Bundle(error) => write!(f, "{error}"),
        }
    }
}

fn load_header(path: &Path, trust: &RegistryTrust) -> Result<CatalogHeader, BundleLoadError> {
    let metadata = std::fs::metadata(path).map_err(BundleLoadError::Io)?;
    let fingerprint = trust_fingerprint(trust);
    if let Ok(cache) = header_cache().lock() {
        if let Some(entry) = cache.get(path) {
            if entry.len == metadata.len()
                && entry.modified == metadata.modified().ok()
                && entry.trust_fingerprint == fingerprint
            {
                return Ok(entry.header.clone());
            }
        }
    }
    let bytes = std::fs::read(path).map_err(BundleLoadError::Io)?;
    let verified =
        registry_bundle::verify_bundle(&bytes, trust).map_err(BundleLoadError::Bundle)?;
    if let Ok(mut cache) = header_cache().lock() {
        cache.insert(
            path.to_path_buf(),
            CachedHeader {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                trust_fingerprint: fingerprint,
                header: verified.header.clone(),
            },
        );
    }
    Ok(verified.header)
}

fn load_payload(path: &Path, trust: &RegistryTrust) -> Result<VerifiedBundle, BundleLoadError> {
    let bytes = std::fs::read(path).map_err(BundleLoadError::Io)?;
    registry_bundle::verify_bundle(&bytes, trust).map_err(BundleLoadError::Bundle)
}

fn bundle_error(error: BundleError) -> LauncherError {
    let code = match &error {
        BundleError::Empty
        | BundleError::Truncated(_)
        | BundleError::BadMagic
        | BundleError::HeaderTooLarge
        | BundleError::HeaderNotUtf8
        | BundleError::HeaderNotJson(_)
        | BundleError::UnsupportedFormat(_)
        | BundleError::MissingField(_)
        | BundleError::InvalidField(_)
        | BundleError::PayloadLength => "ERR_REGISTRY_BUNDLE_INVALID",
        BundleError::SignatureInvalid
        | BundleError::UnknownKey(_)
        | BundleError::RevokedKey(_)
        | BundleError::CertificateInvalid(_) => "ERR_REGISTRY_SIGNATURE_INVALID",
        BundleError::PayloadHash => "ERR_REGISTRY_BUNDLE_HASH",
    };
    LauncherError::Generic {
        code: code.to_string(),
        message: format!("The catalog bundle could not be used: {error}."),
    }
}

// ---------------------------------------------------------------------------
// Writing the bundle and the readable database beside it
// ---------------------------------------------------------------------------

fn next_temp_path(directory: &Path, stem: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    directory.join(format!(
        "{stem}-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Write `bytes` to a unique temporary file in `directory`, flush it, and move
/// it over `path` in one rename. The unique name is per process and per call,
/// so the app and the CLI cannot collide.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = next_temp_path(directory, "agora-registry");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Make the readable `registry.db` match the signed header, extracting it from
/// the bundle if it is missing or does not hash to the signed value.
fn ensure_database(
    app_data_dir: &Path,
    bundle_path: &Path,
    header: &CatalogHeader,
    payload: Option<&[u8]>,
    trust: &RegistryTrust,
) -> LauncherResult<()> {
    let db_path =
        paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if db_path.exists() {
        if let Ok(hash) = sha256_file(&db_path) {
            if hash == header.db_sha256.to_ascii_lowercase() {
                return Ok(());
            }
        }
    }
    let owned_payload;
    let payload = match payload {
        Some(payload) => payload,
        None => {
            let verified =
                load_payload(bundle_path, trust).map_err(|error| LauncherError::Generic {
                    code: "ERR_REGISTRY_BUNDLE_INVALID".into(),
                    message: format!("The catalog bundle could not be read: {error}."),
                })?;
            owned_payload = verified.payload;
            &owned_payload
        }
    };
    if payload.len() as u64 != header.db_len {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_BUNDLE_INVALID".into(),
            message: "The catalog payload length does not match its signed header.".into(),
        });
    }
    write_atomic(&db_path, payload).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_WRITE".into(),
        message: format!("Could not write the readable catalog: {error}."),
    })
}

/// Install the exact signed bytes of a bundle that has already been verified.
///
/// The bundle is one file, so it is replaced by one rename. Before that, the
/// previous good bundle is copied to `registry.bundle.prev`; a crash at any
/// point leaves either the old current or the old previous usable.
fn install_bundle_bytes(
    app_data_dir: &Path,
    header: &CatalogHeader,
    bundle_bytes: &[u8],
    trust: &RegistryTrust,
) -> LauncherResult<()> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    let previous_path = paths::registry_bundle_previous_path(app_data_dir)
        .map_err(|_| LauncherError::RegistryMissing)?;

    // Keep the previous copy only when the current file looks like a bundle.
    // A corrupt current must not overwrite a good previous one.
    if bundle_path.exists() {
        if let Ok(current) = std::fs::read(&bundle_path) {
            if registry_bundle::parse_bundle(&current).is_ok() {
                write_atomic(&previous_path, &current).map_err(|error| LauncherError::Generic {
                    code: "ERR_REGISTRY_WRITE".into(),
                    message: format!("Could not keep the previous catalog: {error}."),
                })?;
            }
        }
    }

    write_atomic(&bundle_path, bundle_bytes).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_WRITE".into(),
        message: format!("Could not install the catalog bundle: {error}."),
    })?;

    // The signature lives inside the bundle now. An old sidecar beside the new
    // database would describe bytes that are no longer there.
    if let Ok(sig_path) = paths::registry_sig_path(app_data_dir) {
        let _ = std::fs::remove_file(sig_path);
    }

    let payload = registry_bundle::verify_bundle(bundle_bytes, trust).map_err(bundle_error)?;
    ensure_database(
        app_data_dir,
        &bundle_path,
        header,
        Some(&payload.payload),
        trust,
    )
}

/// Verify the cached catalog, recovering from the previous bundle when the
/// current one cannot be used.
///
/// Signature checking at download time only establishes that the bytes were
/// authentic *in transit*. The cached copy lives in a user-writable data
/// directory and supplies every download URL and expected hash the launcher
/// trusts, so it is re-checked before use. A failing bundle is renamed to
/// `registry.bundle.untrusted` rather than deleted: that preserves the evidence,
/// and leaving no current bundle behind is what makes the next sync re-download
/// a good copy.
pub fn verify_cached_registry(app_data_dir: &Path) -> CachedRegistryIntegrity {
    verify_cached_registry_with_trust(app_data_dir, &RegistryTrust::production())
}

/// [`verify_cached_registry`] against an explicit trust set (test support).
pub fn verify_cached_registry_with_trust(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> CachedRegistryIntegrity {
    let bundle_path = match paths::registry_bundle_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return CachedRegistryIntegrity::Absent,
    };
    if bundle_path.exists() {
        return verify_bundle_file(app_data_dir, &bundle_path, trust);
    }

    // Cache written before signed bundles. It is still authentic if its
    // sidecar verifies; it simply has no expiry or key id.
    let db_path = match paths::registry_db_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return CachedRegistryIntegrity::Absent,
    };
    if !db_path.exists() {
        return CachedRegistryIntegrity::Absent;
    }
    let sig_path = match paths::registry_sig_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return CachedRegistryIntegrity::Absent,
    };

    let db_bytes = match std::fs::read(&db_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return CachedRegistryIntegrity::Error(format!(
                "Cached registry could not be read ({error}); it will not be used."
            ))
        }
    };
    let sig_bytes = std::fs::read(&sig_path).ok();
    if verify_legacy_pair(trust, &db_bytes, sig_bytes.as_deref()).is_ok() {
        return CachedRegistryIntegrity::Verified;
    }
    quarantine_legacy(
        &db_path,
        &sig_path,
        "Cached registry failed signature verification",
    )
}

fn verify_bundle_file(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
) -> CachedRegistryIntegrity {
    match load_header(bundle_path, trust) {
        Ok(header) => match ensure_database(app_data_dir, bundle_path, &header, None, trust) {
            Ok(()) => {
                if header.is_expired_at(Utc::now()) {
                    CachedRegistryIntegrity::Stale(stale_message(&header))
                } else {
                    CachedRegistryIntegrity::Verified
                }
            }
            Err(error) => CachedRegistryIntegrity::Error(format!(
                "{error} The catalog will not be used this run."
            )),
        },
        Err(BundleLoadError::Io(error)) => CachedRegistryIntegrity::Error(format!(
            "Cached catalog bundle could not be read ({error}); it will not be used."
        )),
        Err(BundleLoadError::Bundle(error)) => {
            let reason = format!("Cached catalog bundle could not be used: {error}.");
            recover_previous(app_data_dir, bundle_path, trust, reason)
        }
    }
}

fn recover_previous(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
    reason: String,
) -> CachedRegistryIntegrity {
    let previous_path = match paths::registry_bundle_previous_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return quarantine_bundle(bundle_path, reason),
    };
    if previous_path.exists() {
        if let Ok(header) = load_header(&previous_path, trust) {
            if ensure_database(app_data_dir, &previous_path, &header, None, trust).is_ok() {
                let recovered = format!("{reason} The previous verified catalog was restored.");
                if let Ok(previous_bytes) = std::fs::read(&previous_path) {
                    if quarantine_aside(bundle_path, "bundle.untrusted").is_ok()
                        && write_atomic(bundle_path, &previous_bytes).is_ok()
                    {
                        return CachedRegistryIntegrity::Recovered(recovered);
                    }
                }
            }
        }
    }
    quarantine_bundle(bundle_path, reason)
}

fn quarantine_bundle(bundle_path: &Path, reason: String) -> CachedRegistryIntegrity {
    if let Err(error) = quarantine_aside(bundle_path, "bundle.untrusted") {
        return CachedRegistryIntegrity::Error(format!(
            "{reason} It could not be moved aside ({error}), so it will not be used this run."
        ));
    }
    CachedRegistryIntegrity::Quarantined(reason)
}

fn quarantine_legacy(db_path: &Path, sig_path: &Path, reason: &str) -> CachedRegistryIntegrity {
    let reason = format!("{reason}; it was quarantined and will be re-downloaded.");
    if let Err(error) = quarantine_aside(db_path, "db.untrusted") {
        return CachedRegistryIntegrity::Error(format!(
            "{reason} It could not be moved aside ({error}), so it will not be used this run."
        ));
    }
    if let Err(error) = quarantine_aside(sig_path, "sig.untrusted") {
        return CachedRegistryIntegrity::Error(format!(
            "{reason} The database was moved aside, but its signature could not be ({error})."
        ));
    }
    CachedRegistryIntegrity::Quarantined(reason)
}

fn stale_message(header: &CatalogHeader) -> String {
    format!(
        "The catalog expired on {}. It can still be browsed, but installs and updates wait for a fresh catalog.",
        header.expires_at
    )
}

/// Verify a pre-bundle database against any key in the trust set. A missing
/// sidecar is unsigned, which is not trusted (except for the debug-only test
/// convenience).
fn verify_legacy_pair(
    trust: &RegistryTrust,
    db_bytes: &[u8],
    sig_bytes: Option<&[u8]>,
) -> LauncherResult<()> {
    if trust.keys().is_empty() {
        if trust.permits_unsigned_debug() {
            return Ok(());
        }
        return Err(LauncherError::RegistrySignatureInvalid);
    }
    let Some(sig_bytes) = sig_bytes else {
        return Err(LauncherError::RegistrySignatureInvalid);
    };
    let mut last_error = LauncherError::RegistrySignatureInvalid;
    for key in trust.keys() {
        match verify_signature_with_key(&key.public_key_hex, db_bytes, sig_bytes) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// The signed build stamp of the cached catalog, for the rollback check.
///
/// An absent catalog is `None` (nothing to roll back from). A catalog that is
/// present but cannot be read or verified is an error: treating "unknown" as
/// "older" is what lets a replayed old signed catalog replace a newer one.
fn read_cached_built_at_checked(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<Option<String>> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        return match load_header(&bundle_path, trust) {
            Ok(header) => Ok(Some(header.built_at)),
            Err(error) => Err(LauncherError::Generic {
                code: "ERR_REGISTRY_CACHED_STAMP_UNREADABLE".into(),
                message: format!(
                    "The installed catalog's signed build stamp cannot be read ({error}), so an update cannot be proved newer. The cached catalog has been kept."
                ),
            }),
        };
    }

    let db_path =
        paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if !db_path.exists() {
        return Ok(None);
    }
    let db_bytes = std::fs::read(&db_path).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_CACHED_STAMP_UNREADABLE".into(),
        message: format!(
            "The installed catalog cannot be read ({error}), so an update cannot be proved newer."
        ),
    })?;
    let sig_bytes = paths::registry_sig_path(app_data_dir)
        .ok()
        .and_then(|path| std::fs::read(path).ok());
    verify_legacy_pair(trust, &db_bytes, sig_bytes.as_deref()).map_err(|_| {
        LauncherError::Generic {
            code: "ERR_REGISTRY_CACHED_STAMP_UNREADABLE".into(),
            message: "The installed catalog's signature does not verify, so its build stamp cannot be trusted and an update cannot be proved newer."
                .into(),
        }
    })?;
    Ok(read_built_at_from_bytes(app_data_dir, &db_bytes))
}

fn cached_catalog_summary(app_data_dir: &Path) -> Option<CachedCatalogSummary> {
    let bundle_path = paths::registry_bundle_path(app_data_dir).ok()?;
    if bundle_path.exists() {
        let header = load_header(&bundle_path, &RegistryTrust::production()).ok()?;
        return Some(CachedCatalogSummary {
            schema_version: Some(header.schema_version),
            stale: header.is_expired_at(Utc::now()),
            expires_at: Some(header.expires_at),
        });
    }
    let db_path = paths::registry_db_path(app_data_dir).ok()?;
    if !db_path.exists() {
        return None;
    }
    Some(CachedCatalogSummary {
        schema_version: read_cached_schema_version(Some(&db_path)).ok(),
        stale: false,
        expires_at: None,
    })
}

/// The digest of the verified catalog, for cache keys and lockfile revisions.
///
/// A bundle answers with the SHA-256 its signed header names, after checking the
/// readable database against it. A pre-bundle cache is hashed only after its
/// signature verifies. `None` means there is no catalog to digest.
pub fn verified_registry_digest(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<Option<String>> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        let header = load_header(&bundle_path, trust).map_err(|error| LauncherError::Generic {
            code: "ERR_REGISTRY_SIGNATURE_INVALID".into(),
            message: format!("The catalog bundle could not be used: {error}."),
        })?;
        ensure_database(app_data_dir, &bundle_path, &header, None, trust)?;
        return Ok(Some(header.db_sha256.to_ascii_lowercase()));
    }

    let db_path =
        paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if !db_path.exists() {
        return Ok(None);
    }
    let db_bytes = std::fs::read(&db_path).map_err(|_| LauncherError::RegistryMissing)?;
    let sig_bytes = paths::registry_sig_path(app_data_dir)
        .ok()
        .and_then(|path| std::fs::read(path).ok());
    verify_legacy_pair(trust, &db_bytes, sig_bytes.as_deref())?;
    Ok(Some(hex::encode(Sha256::digest(&db_bytes))))
}

/// Return the current registry status without performing a network check.
pub fn get_status(app_data_dir: &Path, local_state_path: &Path) -> RegistryStatus {
    let cached = cached_catalog_summary(app_data_dir);
    let has_cached_db = cached.is_some();
    let cached_tag = get_cached_tag(local_state_path).ok().flatten();
    let cached_schema_version = cached.as_ref().and_then(|summary| summary.schema_version);
    let stale = cached.as_ref().is_some_and(|summary| summary.stale);
    let expires_at = cached.and_then(|summary| summary.expires_at);
    RegistryStatus {
        has_cached_db,
        cached_tag,
        cached_schema_version,
        latest_tag: None,
        update_available: false,
        checked: false,
        stale,
        expires_at,
        message: if has_cached_db {
            if stale {
                "Using cached catalog. It is past its signed expiry; installs and updates are disabled until it refreshes."
                    .to_string()
            } else {
                "Using cached catalog.".to_string()
            }
        } else {
            "No catalog found.".to_string()
        },
    }
}

/// On first run with no cached DB, copy the local registry.db from the repo
/// if it exists. Development convenience for local testing.
#[cfg(debug_assertions)]
pub fn seed_from_local_build(app_data_dir: &Path) -> LauncherResult<bool> {
    let dest = paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;

    // Prefer the signed bundle a local compiler build now emits. Seeding is a
    // debug convenience, so structure is checked but the signature is not (a
    // --skip-sign build has a placeholder one).
    let dest_bundle =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    let cached_bundle_mtime = dest_bundle.metadata().and_then(|m| m.modified()).ok();
    let mut search_dir = std::env::current_dir().ok();
    for _ in 0..5 {
        let Some(dir) = search_dir else { break };
        let candidate = dir.join("registry.bundle");
        if candidate.exists() {
            let local_mtime = candidate.metadata().and_then(|m| m.modified()).ok();
            let newer = match (cached_bundle_mtime, local_mtime) {
                (None, _) => true,
                (Some(cached), Some(local)) => local > cached,
                _ => false,
            };
            if newer {
                let bytes =
                    std::fs::read(&candidate).map_err(|_| LauncherError::RegistryDownloadFailed)?;
                if let Ok(parsed) = registry_bundle::parse_bundle(&bytes) {
                    write_atomic(&dest_bundle, &bytes)
                        .map_err(|_| LauncherError::RegistryDownloadFailed)?;
                    ensure_database(
                        app_data_dir,
                        &dest_bundle,
                        &parsed.header,
                        Some(parsed.payload),
                        &RegistryTrust::production(),
                    )?;
                    if let Ok(sig_path) = paths::registry_sig_path(app_data_dir) {
                        let _ = std::fs::remove_file(sig_path);
                    }
                    eprintln!(
                        "Seeded registry.bundle from local build at {}",
                        candidate.display()
                    );
                    return Ok(true);
                }
            }
            break;
        }
        search_dir = dir.parent().map(std::path::Path::to_path_buf);
    }

    if dest.exists() {
        let cached_version = read_cached_schema_version(Some(&dest)).unwrap_or(0);
        let cached_mtime = dest.metadata().and_then(|m| m.modified()).ok();
        let mut search_dir = std::env::current_dir().ok();
        for _ in 0..5 {
            if let Some(dir) = search_dir {
                let candidate = dir.join("registry.db");
                if candidate.exists() {
                    if let Some(local_version) = read_schema_version_at(&candidate) {
                        // Re-seed when the local build's schema version is
                        // newer, OR when the local registry.db file was
                        // modified more recently than the cached copy. The
                        // version check alone misses content-only recompiles
                        // (added/edited mods at an unchanged schema version),
                        // leaving the dev cache stale.
                        let local_mtime = candidate.metadata().and_then(|m| m.modified()).ok();
                        let newer_mtime = match (cached_mtime, local_mtime) {
                            (Some(c), Some(l)) => l > c,
                            _ => false,
                        };
                        if local_version > cached_version || newer_mtime {
                            let dest_sig = paths::registry_sig_path(app_data_dir)
                                .map_err(|_| LauncherError::RegistryMissing)?;
                            let local_sig = candidate.with_extension("db.sig");
                            std::fs::copy(&candidate, &dest)
                                .map_err(|_| LauncherError::RegistryDownloadFailed)?;
                            if local_sig.exists() {
                                std::fs::copy(&local_sig, &dest_sig)
                                    .map_err(|_| LauncherError::RegistryDownloadFailed)?;
                            }
                            eprintln!(
                                "Re-seeded registry.db (schema {} -> {}) from local build at {}",
                                cached_version,
                                local_version,
                                candidate.display()
                            );
                            return Ok(true);
                        }
                    }
                    break;
                }
                search_dir = dir.parent().map(std::path::Path::to_path_buf);
            } else {
                break;
            }
        }
        return Ok(false);
    }

    let mut search_dir = std::env::current_dir().ok();
    for _ in 0..5 {
        if let Some(dir) = search_dir {
            let candidate = dir.join("registry.db");
            if candidate.exists() {
                std::fs::copy(&candidate, &dest)
                    .map_err(|_| LauncherError::RegistryDownloadFailed)?;
                let local_sig = candidate.with_extension("db.sig");
                if local_sig.exists() {
                    let dest_sig = paths::registry_sig_path(app_data_dir)
                        .map_err(|_| LauncherError::RegistryMissing)?;
                    std::fs::copy(&local_sig, &dest_sig)
                        .map_err(|_| LauncherError::RegistryDownloadFailed)?;
                }
                eprintln!(
                    "Seeded registry.db from local build at {}",
                    candidate.display()
                );
                return Ok(true);
            }
            search_dir = dir.parent().map(std::path::Path::to_path_buf);
        } else {
            break;
        }
    }

    Ok(false)
}

// ---------------------------------------------------------------------------
// Internal helpers â€” GitHub API
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    id: u64,
    name: String,
}

impl GitHubRelease {
    /// Return the asset id for the named asset.
    ///
    /// We resolve downloads through the GitHub Assets API endpoint
    /// (`/releases/assets/{id}`) rather than the `browser_download_url`,
    /// because the latter returns 404 for assets that require authentication.
    /// The Assets endpoint 302-redirects to a signed `objects.githubusercontent.com`
    /// URL that `reqwest` follows automatically and works for public repos.
    fn find_asset(&self, name: &str) -> Option<u64> {
        self.assets.iter().find(|a| a.name == name).map(|a| a.id)
    }
}

async fn query_latest_release(repo: &str, token: Option<&str>) -> LauncherResult<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let _permit = crate::github_ratelimit::acquire_github_permit().await;
    let clients = crate::http_client::HttpClients::new()?;
    let headers = token
        .map(|t| vec![("Authorization".into(), format!("Bearer {t}"))])
        .unwrap_or_default();
    let resp = crate::http_client::checked_request_with_headers(
        &clients,
        crate::http_client::ClientCategory::GitHub,
        &url,
        headers,
    )
    .await?;
    if crate::github_ratelimit::is_rate_limit_response(&resp) {
        let retry = crate::github_ratelimit::parse_retry_after(&resp);
        crate::github_ratelimit::report_rate_limit(retry).await;
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_RATE_LIMITED".into(),
            message: format!("GitHub rate limited (HTTP {}).", resp.status()),
        });
    }
    if !resp.status().is_success() {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_CHECK".into(),
            message: format!("GitHub answered HTTP {}.", resp.status()),
        });
    }
    let body = crate::http_client::checked_response_bytes(
        resp,
        crate::http_client::ClientCategory::GitHub,
    )
    .await?;
    serde_json::from_slice::<GitHubRelease>(&body).map_err(|e| LauncherError::Generic {
        code: "ERR_REGISTRY_CHECK".into(),
        message: format!("Catalog release metadata was not readable: {e}."),
    })
}

async fn download_file(url: &str, token: Option<&str>) -> LauncherResult<Vec<u8>> {
    let _permit = crate::github_ratelimit::acquire_github_permit().await;
    let clients = crate::http_client::HttpClients::new()?;
    let mut headers = vec![("Accept".into(), "application/octet-stream".into())];
    if let Some(t) = token {
        headers.push(("Authorization".into(), format!("Bearer {t}")));
    }
    let resp = crate::http_client::checked_request_with_headers(
        &clients,
        crate::http_client::ClientCategory::GitHub,
        url,
        headers,
    )
    .await?;
    if crate::github_ratelimit::is_rate_limit_response(&resp) {
        let retry = crate::github_ratelimit::parse_retry_after(&resp);
        crate::github_ratelimit::report_rate_limit(retry).await;
        return Err(LauncherError::Generic {
            code: "ERR_RATE_LIMITED".to_string(),
            message: "GitHub rate limit hit during registry download.".to_string(),
        });
    }
    if !resp.status().is_success() {
        return Err(LauncherError::RegistryDownloadFailed);
    }
    crate::http_client::checked_response_bytes(resp, crate::http_client::ClientCategory::GitHub)
        .await
        .map_err(|_| LauncherError::RegistryDownloadFailed)
}

// ---------------------------------------------------------------------------
// Reading the cached registry as trusted data
// ---------------------------------------------------------------------------

/// When the cached catalog was built, issued and expires, as the signed header
/// says. A pre-bundle cache has no such stamps and is never reported stale.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryFreshness {
    pub key_id: Option<String>,
    pub built_at: Option<String>,
    pub issued_at: Option<String>,
    pub expires_at: Option<String>,
}

impl RegistryFreshness {
    fn from_header(header: &CatalogHeader) -> Self {
        Self {
            key_id: Some(header.key_id.clone()),
            built_at: Some(header.built_at.clone()),
            issued_at: Some(header.issued_at.clone()),
            expires_at: Some(header.expires_at.clone()),
        }
    }

    pub fn is_stale(&self) -> bool {
        match self.expires_at.as_deref().and_then(parse_utc) {
            Some(expires) => Utc::now() > expires,
            // No expiry means a pre-bundle cache, which is not called stale.
            None => false,
        }
    }

    pub fn stale_reason(&self) -> Option<String> {
        if !self.is_stale() {
            return None;
        }
        Some(format!(
            "The catalog expired on {}. It can still be browsed, but installs and updates wait for a fresh catalog.",
            self.expires_at.as_deref().unwrap_or("an unknown date")
        ))
    }
}

fn parse_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// The cached registry, opened only after the bundle that describes it verified.
///
/// The readable `registry.db` is a derived copy: it is hashed against the
/// signed header on every open, and re-extracted from the bundle if it does not
/// match. A file swapped after startup therefore cannot choose what an install
/// downloads.
pub struct VerifiedRegistry {
    conn: Option<rusqlite::Connection>,
    freshness: RegistryFreshness,
}

impl VerifiedRegistry {
    pub fn connection(&self) -> &rusqlite::Connection {
        self.conn
            .as_ref()
            .expect("the connection is only taken when the registry is consumed")
    }

    pub fn freshness(&self) -> &RegistryFreshness {
        &self.freshness
    }

    /// Refuse to act on a catalog past its signed expiry. Reading and browsing
    /// are still allowed; downloading or replacing software is not.
    pub fn require_fresh(&self) -> LauncherResult<()> {
        match self.freshness.stale_reason() {
            Some(reason) => Err(LauncherError::Generic {
                code: "ERR_REGISTRY_STALE".into(),
                message: reason,
            }),
            None => Ok(()),
        }
    }

    pub fn into_connection(mut self) -> rusqlite::Connection {
        self.conn
            .take()
            .expect("the connection is only taken when the registry is consumed")
    }
}

/// Verify the cached catalog and open it.
///
/// Fails closed: a missing registry, a missing or wrong signature, or an
/// unreadable file is an error, never an empty registry.
pub fn open_verified_registry(app_data_dir: &Path) -> LauncherResult<VerifiedRegistry> {
    open_verified_registry_with_trust(app_data_dir, &RegistryTrust::production())
}

/// [`open_verified_registry`] against an explicit trust set.
pub fn open_verified_registry_with_trust(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<VerifiedRegistry> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        let first = open_bundle_once(app_data_dir, &bundle_path, trust);
        if first.is_ok() {
            return first;
        }
        // A crash between installing the bundle and extracting the database
        // (or a damaged current bundle) is recoverable from the previous copy.
        match verify_cached_registry_with_trust(app_data_dir, trust) {
            CachedRegistryIntegrity::Verified
            | CachedRegistryIntegrity::Stale(_)
            | CachedRegistryIntegrity::Recovered(_) => {
                return open_bundle_once(app_data_dir, &bundle_path, trust)
            }
            _ => return first,
        }
    }

    // A cache written before signed bundles.
    let db_path =
        paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if !db_path.exists() {
        return Err(LauncherError::RegistryMissing);
    }
    let db_bytes = std::fs::read(&db_path).map_err(|_| LauncherError::RegistryMissing)?;
    let sig_bytes = paths::registry_sig_path(app_data_dir)
        .ok()
        .and_then(|path| std::fs::read(path).ok());
    verify_legacy_pair(trust, &db_bytes, sig_bytes.as_deref())?;
    let conn = db::registry_connection(&db_path).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_UNREADABLE".into(),
        message: format!("The verified catalog could not be opened: {error}"),
    })?;
    Ok(VerifiedRegistry {
        conn: Some(conn),
        freshness: RegistryFreshness::default(),
    })
}

fn open_bundle_once(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<VerifiedRegistry> {
    let header = load_header(bundle_path, trust).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_SIGNATURE_INVALID".into(),
        message: format!("The catalog bundle could not be used: {error}."),
    })?;
    ensure_database(app_data_dir, bundle_path, &header, None, trust)?;
    let db_path =
        paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    let conn = db::registry_connection(&db_path).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_UNREADABLE".into(),
        message: format!("The verified catalog could not be opened: {error}"),
    })?;
    Ok(VerifiedRegistry {
        conn: Some(conn),
        freshness: RegistryFreshness::from_header(&header),
    })
}

/// [`open_verified_registry`] against an explicit key.
///
/// Public so integration tests can sign a catalog of their own. Production
/// adapters call [`open_verified_registry`]; no setting or environment variable
/// reaches this function.
pub fn open_verified_registry_with_key(
    app_data_dir: &Path,
    pubkey_hex: &str,
) -> LauncherResult<VerifiedRegistry> {
    if pubkey_hex.trim().is_empty() {
        return Err(LauncherError::RegistrySignatureInvalid);
    }
    open_verified_registry_with_trust(app_data_dir, &RegistryTrust::with_key(pubkey_hex))
}

// ---------------------------------------------------------------------------
// Internal helpers â€” Ed25519 signature verification
// ---------------------------------------------------------------------------

#[cfg(test)]
fn verify_signature(db_bytes: &[u8], sig_bytes: &[u8]) -> LauncherResult<()> {
    verify_legacy_pair(&RegistryTrust::production(), db_bytes, Some(sig_bytes))
}

/// [`verify_signature`] against an explicit key, so a caller that is handed a
/// different trust anchor (a test, never a user setting) shares the one check.
fn verify_signature_with_key(
    pubkey_hex: &str,
    db_bytes: &[u8],
    sig_bytes: &[u8],
) -> LauncherResult<()> {
    // In debug builds only, allow skipping verification when no pubkey is set.
    // Release builds must have AGORA_REGISTRY_PUBKEY compiled in.
    #[cfg(not(debug_assertions))]
    {
        if pubkey_hex.is_empty() {
            return Err(LauncherError::Generic {
                code: "ERR_REGISTRY_PUBKEY_NOT_CONFIGURED".to_string(),
                message: "Catalog public key not compiled in; refusing to verify. \
                          Set AGORA_REGISTRY_PUBKEY (Ed25519 public key, hex) as an \
                          environment variable before building the desktop app: \
                          `$env:AGORA_REGISTRY_PUBKEY='...'; npm run tauri:dev`. \
                          See README.md > \"Environment variables for the Tauri build\"."
                    .to_string(),
            });
        }
    }

    #[cfg(debug_assertions)]
    {
        if pubkey_hex.is_empty() {
            eprintln!(
                "WARNING: AGORA_REGISTRY_PUBKEY not set; skipping signature verification (debug build only).\n\
                 Set `$env:AGORA_REGISTRY_PUBKEY` (Ed25519 public key, hex) before `npm run tauri:dev` to enable verification.\n\
                 See README.md > \"Environment variables for the Tauri build\"."
            );
            if sig_bytes.is_empty() {
                return Err(LauncherError::RegistrySignatureInvalid);
            }
            return Ok(());
        }
    }

    let pubkey_bytes = hex::decode(pubkey_hex).map_err(|_| LauncherError::Generic {
        code: "ERR_REGISTRY_PUBKEY".to_string(),
        message: "Invalid compiled-in registry public key.".to_string(),
    })?;

    if pubkey_bytes.len() != PUBLIC_KEY_LENGTH {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_PUBKEY".to_string(),
            message: "Compiled-in registry public key is wrong length.".to_string(),
        });
    }

    if sig_bytes.len() != SIGNATURE_LENGTH {
        return Err(LauncherError::RegistrySignatureInvalid);
    }

    let pubkey_array: [u8; PUBLIC_KEY_LENGTH] = pubkey_bytes
        .try_into()
        .map_err(|_| LauncherError::RegistrySignatureInvalid)?;

    let sig_array: [u8; SIGNATURE_LENGTH] = sig_bytes
        .try_into()
        .map_err(|_| LauncherError::RegistrySignatureInvalid)?;

    let verifying_key = VerifyingKey::from_bytes(&pubkey_array)
        .map_err(|_| LauncherError::RegistrySignatureInvalid)?;
    let signature = Signature::from_bytes(&sig_array);

    use ed25519_dalek::Verifier;
    verifying_key
        .verify(db_bytes, &signature)
        .map_err(|_| LauncherError::RegistrySignatureInvalid)
}

// ---------------------------------------------------------------------------
// Internal helpers â€” schema version reading
// ---------------------------------------------------------------------------

/// Read the schema version from raw database bytes.
///
/// Writes to the app's private data dir (not shared OS temp) to prevent
/// symlink-based TOCTOU attacks.
#[cfg(test)]
fn read_schema_version_from_bytes(
    app_data_dir: &std::path::Path,
    db_bytes: &[u8],
) -> LauncherResult<i64> {
    // Unique per process and per call: the desktop app and the CLI share this
    // folder, and a fixed name let one delete the other's staging file.
    let temp_db = next_temp_path(app_data_dir, "registry-verify");

    std::fs::write(&temp_db, db_bytes).map_err(|_| LauncherError::RegistryDownloadFailed)?;

    let result = (|| -> LauncherResult<i64> {
        let conn = rusqlite::Connection::open_with_flags(
            &temp_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| LauncherError::RegistryDownloadFailed)?;

        let version: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .map_err(|_| LauncherError::RegistryDownloadFailed)?;

        drop(conn);
        Ok(version)
    })();

    // Always clean up the temp file, even on error.
    let _ = std::fs::remove_file(&temp_db);

    result
}

/// Read the compiler's `built_at` stamp out of an open registry database.
///
/// Returns `None` when the key is absent, which is the normal case for any
/// database compiled before the stamp was introduced.
fn read_built_at(conn: &rusqlite::Connection) -> Option<String> {
    let raw: String = conn
        .query_row(
            "SELECT value_json FROM system_config WHERE key = 'built_at'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    // Stored JSON-encoded, matching `runtime_catalog_source`.
    serde_json::from_str::<String>(&raw).ok()
}

/// Read `built_at` from raw database bytes, using the same private-directory
/// staging as [`read_schema_version_from_bytes`].
fn read_built_at_from_bytes(app_data_dir: &std::path::Path, db_bytes: &[u8]) -> Option<String> {
    let temp_db = next_temp_path(app_data_dir, "registry-rollback");
    std::fs::write(&temp_db, db_bytes).ok()?;

    let built_at = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            &temp_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        let value = read_built_at(&conn);
        drop(conn);
        value
    })();

    let _ = std::fs::remove_file(&temp_db);
    built_at
}

/// Whether `latest` names a strictly newer registry release than `cached`.
///
/// Registry releases are tagged `registry-YYYY-MM-DD` (UTC) by
/// `scripts/deploy_release_assets.py`. This is a cheap pre-filter on *unsigned*
/// GitHub API metadata — it stops the realistic accident, where pruning
/// releases promotes an older one to "latest" — but it proves nothing on its
/// own, because a forged API response can claim any tag. The binding check is
/// [`is_rollback`], which compares a stamp carried inside the signed bytes.
///
/// An unparseable *cached* tag is treated as replaceable so a client can never
/// wedge itself permanently; an unparseable *remote* tag is never accepted over
/// a known-good one.
fn tag_is_newer(cached: &str, latest: &str) -> bool {
    match (parse_registry_tag(cached), parse_registry_tag(latest)) {
        (Some(cached_date), Some(latest_date)) => latest_date > cached_date,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Whether the latest release is worth downloading.
///
/// The tag is the date, so two releases published the same UTC day carry the
/// same tag, and `deploy_release_assets.py` re-publishes into it by deleting and
/// re-uploading the assets. A same-day re-publish is how a bad official plugin
/// is revoked within the day, so a client that already synced today must still
/// notice it. The tag cannot show it, but the `registry.db` asset's id does: a
/// re-upload gets a new id. A client with no recorded id (one that synced before
/// this was recorded) downloads once to learn it.
///
/// This is only a filter on unsigned API metadata. What decides whether a
/// downloaded registry is installed is the signed `built_at` stamp
/// ([`is_rollback`]); a forged id can at worst cost one wasted download per check.
fn update_is_available(
    cached_tag: Option<&str>,
    has_cached_db: bool,
    cached_asset: Option<u64>,
    latest_tag: &str,
    latest_asset: Option<u64>,
) -> bool {
    let Some(cached_tag) = cached_tag.filter(|_| has_cached_db) else {
        // No cached tag, or a tag with no database behind it: nothing to
        // roll back from.
        return true;
    };
    if tag_is_newer(cached_tag, latest_tag) {
        return true;
    }
    // Same release tag (same UTC day): newer only if its database was re-uploaded.
    match (
        parse_registry_tag(cached_tag),
        parse_registry_tag(latest_tag),
    ) {
        (Some(cached), Some(latest)) if cached == latest => {
            latest_asset.is_some() && latest_asset != cached_asset
        }
        _ => false,
    }
}

/// Parse `registry-YYYY-MM-DD` into a comparable tuple.
fn parse_registry_tag(tag: &str) -> Option<(u16, u8, u8)> {
    let mut parts = tag.strip_prefix("registry-")?.split('-');
    let year: u16 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Whether installing `candidate` over `cached` would move the registry
/// backwards in time.
///
/// Both stamps come from inside signature-verified database bytes, so unlike
/// the release tag they cannot be forged without the signing key. ISO-8601 UTC
/// timestamps from the compiler compare correctly as strings.
///
/// An unstamped candidate over a stamped cache is a rollback: it predates the stamp, so it is
/// older than anything stamped. Refusing it keeps the stamped registry. A *cached* database
/// predating the stamp has no value to compare, and refusing every update in that state would
/// strand clients on exactly the registry we want them to move off, so that direction still
/// allows the update.
fn is_rollback(cached_built_at: Option<&str>, candidate_built_at: Option<&str>) -> bool {
    match (cached_built_at, candidate_built_at) {
        (Some(cached), Some(candidate)) => candidate < cached,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Read the schema version from the cached registry database at `path`.
fn read_cached_schema_version(path: Option<&std::path::Path>) -> LauncherResult<i64> {
    let path = path.ok_or(LauncherError::RegistryMissing)?;
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| LauncherError::RegistryMissing)?;

    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .map_err(|_| LauncherError::RegistryMissing)
}

/// Open a read-only connection at an arbitrary path and read the schema version.
#[cfg(debug_assertions)]
fn read_schema_version_at(path: &std::path::Path) -> Option<i64> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.pragma_update(None, "query_only", "ON");
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .ok()
}

// ---------------------------------------------------------------------------
// Internal helpers â€” local state (cached tag, check time)
// ---------------------------------------------------------------------------

fn get_cached_tag(local_state_path: &std::path::Path) -> LauncherResult<Option<String>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "cached_registry_tag")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_str().map(|s| s.to_string())))
}

fn set_cached_tag(local_state_path: &std::path::Path, tag: &str) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "cached_registry_tag",
        &serde_json::Value::String(tag.to_string()),
    )
    .map_err(|_| LauncherError::LocalStateFailed)
}

/// The id of the `registry.db` release asset the cached registry came from.
fn get_cached_asset_id(local_state_path: &std::path::Path) -> LauncherResult<Option<u64>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "cached_registry_asset_id")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_u64()))
}

fn set_cached_asset_id(
    local_state_path: &std::path::Path,
    asset_id: Option<u64>,
) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    let value = match asset_id {
        Some(id) => serde_json::Value::from(id),
        None => serde_json::Value::Null,
    };
    db::set_setting(&conn, "cached_registry_asset_id", &value)
        .map_err(|_| LauncherError::LocalStateFailed)
}

fn get_last_check_time(local_state_path: &std::path::Path) -> LauncherResult<Option<i64>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "last_registry_check")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_i64()))
}

fn set_last_check_time(local_state_path: &std::path::Path, timestamp: i64) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "last_registry_check",
        &serde_json::Value::Number(timestamp.into()),
    )
    .map_err(|_| LauncherError::LocalStateFailed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_db() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agora-registry-sync-test-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = fs::remove_file(&path);
        db::init_local_state_db(&path).expect("failed to init test db");
        path
    }

    #[test]
    fn test_get_cached_tag_absent() {
        let path = temp_db();
        let result = get_cached_tag(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_set_and_get_cached_tag() {
        let path = temp_db();
        set_cached_tag(&path, "v1.2.3").unwrap();
        let result = get_cached_tag(&path).unwrap();
        assert_eq!(result, Some("v1.2.3".to_string()));
    }

    #[test]
    fn test_set_and_get_last_check_time() {
        let path = temp_db();
        set_last_check_time(&path, 1_000_000_000).unwrap();
        let result = get_last_check_time(&path).unwrap();
        assert_eq!(result, Some(1_000_000_000));
    }

    #[test]
    fn test_schema_version_from_bytes() {
        let tmp_dir =
            std::env::temp_dir().join(format!("agora-test-schema-version-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Build a minimal registry.db with schema_version table.
        let db_path = tmp_dir.join("registry.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             INSERT INTO schema_version (version) VALUES (5);
             CREATE TABLE registry_items (id TEXT PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);

        let bytes = fs::read(&db_path).unwrap();
        let version = read_schema_version_from_bytes(&tmp_dir, &bytes).unwrap();
        assert_eq!(version, 5);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_schema_version_missing_table() {
        let tmp_dir =
            std::env::temp_dir().join(format!("agora-test-schema-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let db_path = tmp_dir.join("registry.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE foo (id TEXT)", []).unwrap();
        drop(conn);

        let bytes = fs::read(&db_path).unwrap();
        // The query uses COALESCE(MAX(version), 0) so a missing table
        // will error (table doesn't exist), not return 0.
        let result = read_schema_version_from_bytes(&tmp_dir, &bytes);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_schema_version_from_cached() {
        let tmp_dir =
            std::env::temp_dir().join(format!("agora-test-schema-cached-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let db_path = tmp_dir.join("registry.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             INSERT INTO schema_version (version) VALUES (3);",
        )
        .unwrap();
        drop(conn);

        let version = read_cached_schema_version(Some(&db_path)).unwrap();
        assert_eq!(version, 3);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_schema_version_zero_when_missing() {
        let err = read_cached_schema_version(None).unwrap_err();
        assert!(matches!(err, LauncherError::RegistryMissing));
    }

    fn test_key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn test_public(key: &ed25519_dalek::SigningKey) -> String {
        hex::encode(key.verifying_key().to_bytes())
    }

    fn sample_db_with_built_at(built_at: &str) -> Vec<u8> {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             INSERT INTO schema_version (version) VALUES (10);
             CREATE TABLE system_config (key TEXT PRIMARY KEY, value_json TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO system_config (key, value_json) VALUES ('built_at', ?)",
            [serde_json::Value::String(built_at.to_string()).to_string()],
        )
        .unwrap();
        drop(conn);
        fs::read(&path).unwrap()
    }

    fn sample_db() -> Vec<u8> {
        sample_db_with_built_at("2026-11-16T12:00:00+00:00")
    }

    fn header_for(
        key: &ed25519_dalek::SigningKey,
        payload: &[u8],
        built_at: &str,
        expires_at: &str,
    ) -> CatalogHeader {
        CatalogHeader {
            format: registry_bundle::BUNDLE_FORMAT,
            key_id: registry_bundle::key_id_for_public_key(&test_public(key)),
            schema_version: 10,
            tag: "registry-2026-11-16".into(),
            built_at: built_at.into(),
            issued_at: built_at.into(),
            expires_at: expires_at.into(),
            db_len: payload.len() as u64,
            db_sha256: hex::encode(Sha256::digest(payload)),
            certificates: Vec::new(),
            revoked_key_ids: Vec::new(),
        }
    }

    fn signed_bundle(
        key: &ed25519_dalek::SigningKey,
        header: &CatalogHeader,
        payload: &[u8],
    ) -> Vec<u8> {
        use ed25519_dalek::Signer;
        let header_bytes = registry_bundle::header_bytes(header);
        let signature = key
            .sign(&registry_bundle::header_signing_bytes(&header_bytes))
            .to_bytes();
        registry_bundle::encode_bundle(&header_bytes, &signature, payload)
    }

    fn fresh_header(key: &ed25519_dalek::SigningKey, payload: &[u8]) -> CatalogHeader {
        header_for(
            key,
            payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        )
    }

    fn write_bundle(
        dir: &Path,
        file: &str,
        key: &ed25519_dalek::SigningKey,
        header: &CatalogHeader,
        payload: &[u8],
    ) -> Vec<u8> {
        let bytes = signed_bundle(key, header, payload);
        fs::write(dir.join(file), &bytes).unwrap();
        bytes
    }

    #[test]
    fn test_registry_status_default() {
        let tmp_dir =
            std::env::temp_dir().join(format!("agora-test-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let ls_path = tmp_dir.join("local_state.db");
        db::init_local_state_db(&ls_path).unwrap();

        let status = get_status(&tmp_dir, &ls_path);
        assert!(!status.has_cached_db);
        assert!(status.cached_tag.is_none());
        assert!(status.cached_schema_version.is_none());
        assert!(!status.update_available);
        assert!(!status.checked);
        assert!(!status.stale);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_signature_debug_skips_when_no_key() {
        let db_bytes = b"test data";
        let sig_bytes = b"00000000000000000000000000000000\
                         00000000000000000000000000000000"; // 64 zero bytes
        let result = verify_signature(db_bytes, sig_bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_signature_wrong_length() {
        let db_bytes = b"test data";
        let short_sig = b"deadbeef"; // 8 bytes, not 64
        let result = verify_signature(db_bytes, short_sig);
        assert!(result.is_err());
    }

    #[test]
    fn parse_registry_tag_accepts_only_the_release_format() {
        assert_eq!(
            parse_registry_tag("registry-2026-08-27"),
            Some((2026, 8, 27))
        );
        assert_eq!(
            parse_registry_tag("registry-2026-01-01"),
            Some((2026, 1, 1))
        );
        assert_eq!(parse_registry_tag("v0.9.0"), None);
        assert_eq!(parse_registry_tag("site-assets"), None);
        assert_eq!(parse_registry_tag("registry-2026-08-27-evil"), None);
        assert_eq!(parse_registry_tag("registry-2026-13-01"), None);
        assert_eq!(parse_registry_tag("registry-2026-08-32"), None);
        assert_eq!(parse_registry_tag("registry-not-a-date"), None);
    }

    #[test]
    fn tag_is_newer_rejects_an_older_release() {
        assert!(tag_is_newer("registry-2026-08-26", "registry-2026-08-27"));
        assert!(!tag_is_newer("registry-2026-08-27", "registry-2026-08-26"));
        assert!(!tag_is_newer("registry-2026-08-27", "registry-2026-08-27"));
        assert!(!tag_is_newer("registry-2026-08-27", "registry-2025-12-31"));
        assert!(!tag_is_newer("registry-2026-08-27", "v0.9.0"));
        assert!(tag_is_newer("some-legacy-tag", "registry-2026-08-27"));
    }

    #[test]
    fn a_same_day_republish_is_noticed_by_its_new_asset_id() {
        let today = "registry-2026-10-31";
        assert!(update_is_available(
            Some(today),
            true,
            Some(7),
            today,
            Some(9)
        ));
        assert!(!update_is_available(
            Some(today),
            true,
            Some(7),
            today,
            Some(7)
        ));
        assert!(update_is_available(Some(today), true, None, today, Some(9)));
        assert!(!update_is_available(
            Some(today),
            true,
            Some(7),
            today,
            None
        ));
        assert!(!update_is_available(
            Some("registry-2026-11-01"),
            true,
            Some(7),
            today,
            Some(9)
        ));
        assert!(!update_is_available(
            Some(today),
            true,
            Some(7),
            "v0.9.0",
            Some(9)
        ));
        assert!(update_is_available(
            Some(today),
            true,
            Some(7),
            "registry-2026-11-01",
            Some(7)
        ));
        assert!(update_is_available(None, false, None, today, Some(9)));
        assert!(update_is_available(
            Some(today),
            false,
            Some(7),
            today,
            Some(7)
        ));
    }

    #[test]
    fn is_rollback_compares_signed_build_stamps() {
        let older = "2026-08-26T03:19:02+00:00";
        let newer = "2026-08-27T12:20:15+00:00";
        assert!(is_rollback(Some(newer), Some(older)));
        assert!(!is_rollback(Some(older), Some(newer)));
        assert!(!is_rollback(Some(newer), Some(newer)));
        assert!(!is_rollback(None, Some(newer)));
        assert!(is_rollback(Some(newer), None));
        assert!(!is_rollback(None, None));
    }

    #[test]
    fn built_at_round_trips_through_system_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("stamped.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE system_config (key TEXT PRIMARY KEY, value_json TEXT NOT NULL);",
        )
        .unwrap();
        assert_eq!(read_built_at(&conn), None, "absent key must read as None");

        conn.execute(
            "INSERT INTO system_config (key, value_json) VALUES ('built_at', ?)",
            ["\"2026-08-27T12:20:15+00:00\""],
        )
        .unwrap();
        assert_eq!(
            read_built_at(&conn).as_deref(),
            Some("2026-08-27T12:20:15+00:00")
        );
    }

    #[test]
    fn verify_cached_registry_reports_absent_when_no_db() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            verify_cached_registry(dir.path()),
            CachedRegistryIntegrity::Absent
        );
    }

    /// An empty file is not an empty catalog. It is a bundle that cannot be
    /// used, and it must be said so rather than reported as a clean cache.
    #[test]
    fn an_empty_cached_bundle_is_an_error_not_a_clean_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let bundle_path = paths::registry_bundle_path(dir.path()).unwrap();
        fs::write(&bundle_path, b"").unwrap();

        let result = verify_cached_registry_with_trust(
            dir.path(),
            &RegistryTrust::with_key(test_public(&test_key(1))),
        );
        assert!(
            matches!(result, CachedRegistryIntegrity::Quarantined(_)),
            "an empty bundle must not verify, got {result:?}"
        );
        assert!(!bundle_path.exists());
        let error = open_verified_registry_with_key(dir.path(), &test_public(&test_key(1)))
            .err()
            .expect("an empty bundle cannot be opened");
        assert!(matches!(
            error,
            LauncherError::RegistryMissing | LauncherError::Generic { .. }
        ));
    }

    /// One file, one rename: the signed bundle installs, and the readable
    /// database is extracted from exactly the bytes the signature covers.
    #[test]
    fn a_signed_bundle_installs_and_opens() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(2);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);

        assert_eq!(
            verify_cached_registry_with_trust(dir.path(), &trust),
            CachedRegistryIntegrity::Verified
        );
        assert_eq!(
            fs::read(paths::registry_db_path(dir.path()).unwrap()).unwrap(),
            payload
        );

        let registry = open_verified_registry_with_key(dir.path(), &test_public(&key)).unwrap();
        let version: i64 = registry
            .connection()
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 10);
        assert!(!registry.freshness().is_stale());
        registry.require_fresh().unwrap();
    }

    /// A crash after the bundle is in place but before the readable database is
    /// extracted leaves a usable catalog: the next open extracts it again.
    #[test]
    fn a_crash_between_install_and_extract_is_recovered_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(3);
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        assert!(!db_path.exists());

        let registry = open_verified_registry_with_key(dir.path(), &test_public(&key)).unwrap();
        assert_eq!(fs::read(&db_path).unwrap(), payload);
        let version: i64 = registry
            .connection()
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 10);
    }

    /// A crash between replacing the previous bundle and the current one leaves
    /// no current bundle. The previous verified copy is promoted and used.
    #[test]
    fn a_crash_between_bundle_files_recovers_the_previous_copy() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(4);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle.prev", &key, &header, &payload);
        // A current file that is only half written.
        let current = paths::registry_bundle_path(dir.path()).unwrap();
        fs::write(&current, b"AGORA-CATALOG-V1half").unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "the previous bundle should be restored, got {result:?}"
        );
        assert_eq!(
            fs::read(&current).unwrap(),
            signed_bundle(&key, &header, &payload)
        );
        assert_eq!(
            fs::read(paths::registry_db_path(dir.path()).unwrap()).unwrap(),
            payload
        );
    }

    /// Without a previous copy, a damaged current bundle is quarantined and the
    /// catalog is reported unavailable, never opened.
    #[test]
    fn a_damaged_bundle_without_a_previous_copy_is_quarantined() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(5);
        let trust = RegistryTrust::with_key(test_public(&key));
        let bundle_path = paths::registry_bundle_path(dir.path()).unwrap();
        fs::write(&bundle_path, b"not a bundle").unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(matches!(result, CachedRegistryIntegrity::Quarantined(_)));
        assert!(!bundle_path.exists());
        assert!(!paths::registry_db_path(dir.path()).unwrap().exists());
    }

    /// An expired catalog is readable, but a freshness-sensitive action refuses
    /// it and says why.
    #[test]
    fn an_expired_bundle_is_readable_but_not_fresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(6);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = header_for(
            &key,
            &payload,
            "2025-01-01T00:00:00+00:00",
            "2025-02-01T00:00:00+00:00",
        );
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(matches!(result, CachedRegistryIntegrity::Stale(_)));
        let registry = open_verified_registry_with_key(dir.path(), &test_public(&key)).unwrap();
        assert!(registry.freshness().is_stale());
        let error = registry.require_fresh().unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_STALE");
        // It is still readable.
        let version: i64 = registry
            .connection()
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 10);
    }

    /// A build stamp that cannot be read must refuse an update: treating it as
    /// "older" lets a replayed old signed catalog replace a newer one.
    #[test]
    fn an_unreadable_cached_stamp_refuses_the_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(7);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);
        assert_eq!(
            read_cached_built_at_checked(dir.path(), &trust).unwrap(),
            Some(header.built_at)
        );

        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            b"corrupt current bundle",
        )
        .unwrap();
        let error = read_cached_built_at_checked(dir.path(), &trust).unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_CACHED_STAMP_UNREADABLE");
    }

    /// A new key introduced by a certificate signed by an old trusted key is
    /// accepted; the same bundle without the certificate is not.
    #[test]
    fn a_new_key_is_accepted_through_an_old_key_and_refused_without_it() {
        use ed25519_dalek::Signer;
        let dir = tempfile::TempDir::new().unwrap();
        let old = test_key(8);
        let new = test_key(9);
        let payload = sample_db();
        let mut header = fresh_header(&new, &payload);
        let old_id = registry_bundle::key_id_for_public_key(&test_public(&old));
        let new_id = registry_bundle::key_id_for_public_key(&test_public(&new));
        let certificate_message =
            registry_bundle::KeyCertificate::signed_bytes(&new_id, &test_public(&new), &old_id);
        header.certificates.push(registry_bundle::KeyCertificate {
            key_id: new_id,
            public_key: test_public(&new),
            signed_by: old_id,
            signature: hex::encode(old.sign(&certificate_message).to_bytes()),
        });
        let bundle = signed_bundle(&new, &header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &bundle).unwrap();

        let trust_old = RegistryTrust::with_key(test_public(&old));
        assert!(open_verified_registry_with_trust(dir.path(), &trust_old).is_ok());

        let mut without_certificate = header.clone();
        without_certificate.certificates.clear();
        let unsigned_chain = signed_bundle(&new, &without_certificate, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            &unsigned_chain,
        )
        .unwrap();
        assert!(open_verified_registry_with_trust(dir.path(), &trust_old).is_err());
    }

    /// The app and the CLI share one data folder. Their staging files must never
    /// be the same name.
    #[test]
    fn temporary_names_are_unique_and_leave_nothing_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = next_temp_path(dir.path(), "agora-registry");
        let second = next_temp_path(dir.path(), "agora-registry");
        assert_ne!(first, second);
        assert_ne!(first.file_name(), second.file_name());

        let target = dir.path().join("registry.db");
        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files were left behind");
    }

    /// A cache written before signed bundles is still accepted when its sidecar
    /// verifies, so an upgrade does not orphan an installed catalog.
    #[test]
    fn a_legacy_signed_pair_is_still_accepted() {
        use ed25519_dalek::Signer;
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(10);
        let payload = sample_db();
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        let sig_path = paths::registry_sig_path(dir.path()).unwrap();
        fs::write(&db_path, &payload).unwrap();
        fs::write(&sig_path, key.sign(&payload).to_bytes()).unwrap();

        let trust = RegistryTrust::with_key(test_public(&key));
        assert_eq!(
            verify_cached_registry_with_trust(dir.path(), &trust),
            CachedRegistryIntegrity::Verified
        );
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
    }

    /// A cached db whose signature does not verify must be quarantined, and the
    /// quarantine must leave no `registry.db` behind — that absence is what
    /// makes the next sync re-download a good copy instead of reporting
    /// "up to date".
    #[test]
    fn verify_cached_registry_quarantines_a_tampered_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        let sig_path = paths::registry_sig_path(dir.path()).unwrap();
        std::fs::write(&db_path, b"tampered registry contents").unwrap();
        std::fs::write(&sig_path, [0u8; SIGNATURE_LENGTH]).unwrap();

        let result = verify_cached_registry(dir.path());
        assert!(
            matches!(result, CachedRegistryIntegrity::Quarantined(_)),
            "a db that fails signature verification must be quarantined, got {result:?}"
        );
        assert!(
            !db_path.exists(),
            "the untrusted db must not remain in place"
        );
        assert!(
            db_path.with_extension("db.untrusted").exists(),
            "the untrusted db should be preserved for inspection"
        );
    }

    /// A missing sidecar is treated the same as a bad signature: an unsigned db
    /// is not a trusted one.
    #[test]
    fn verify_cached_registry_quarantines_a_db_with_no_signature() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        std::fs::write(&db_path, b"unsigned registry contents").unwrap();

        let result = verify_cached_registry(dir.path());
        assert!(
            matches!(result, CachedRegistryIntegrity::Quarantined(_)),
            "a db with no signature sidecar must be quarantined, got {result:?}"
        );
        assert!(!db_path.exists());
    }

    #[test]
    fn test_github_release_find_asset() {
        let release = GitHubRelease {
            tag_name: "v1.0.0".to_string(),
            assets: vec![
                GitHubAsset {
                    id: 1,
                    name: "registry.bundle".to_string(),
                },
                GitHubAsset {
                    id: 2,
                    name: "registry.db".to_string(),
                },
            ],
        };
        assert_eq!(release.find_asset("registry.bundle"), Some(1));
        assert_eq!(release.find_asset("registry.db"), Some(2));
        assert_eq!(release.find_asset("nonexistent"), None);
    }

    #[test]
    fn test_app_schema_version_constant() {
        const { assert!(APP_REGISTRY_SCHEMA_VERSION >= 1) };
    }

    /// A quarantine whose documented target is occupied must still move the tampered
    /// `registry.db` away: leaving it at `registry.db` lets later readers open it.
    #[test]
    fn a_failed_quarantine_does_not_leave_the_unsigned_db_in_place() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        let sig_path = paths::registry_sig_path(dir.path()).unwrap();
        std::fs::write(&db_path, b"tampered registry contents").unwrap();
        std::fs::write(&sig_path, [0u8; SIGNATURE_LENGTH]).unwrap();
        let blocker = db_path.with_extension("db.untrusted");
        std::fs::create_dir(&blocker).unwrap();
        std::fs::write(blocker.join("keep"), b"x").unwrap();

        let result = verify_cached_registry(dir.path());
        assert!(matches!(result, CachedRegistryIntegrity::Quarantined(_)));
        assert!(
            !db_path.exists(),
            "reported as quarantined, but the tampered db is still at registry.db"
        );
    }

    /// Cache keys use the signed payload hash, so they cannot be made to
    /// disagree with the bytes the launcher is allowed to read.
    #[test]
    fn the_verified_digest_is_the_signed_payload_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(12);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);

        assert_eq!(
            verified_registry_digest(dir.path(), &trust).unwrap(),
            Some(header.db_sha256.clone())
        );
        assert_eq!(
            verified_registry_digest(tempfile::TempDir::new().unwrap().path(), &trust,).unwrap(),
            None
        );
    }

    /// Replacing a bundle keeps the one it replaced, and a second install opens
    /// the new bytes.
    #[test]
    fn installing_a_bundle_replaces_it_and_keeps_the_previous_copy() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(11);
        let trust = RegistryTrust::with_key(test_public(&key));
        let first_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let first_header = header_for(
            &key,
            &first_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let first = signed_bundle(&key, &first_header, &first_payload);
        install_bundle_bytes(dir.path(), &first_header, &first, &trust).unwrap();

        let second_payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let second_header = header_for(
            &key,
            &second_payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let second = signed_bundle(&key, &second_header, &second_payload);
        install_bundle_bytes(dir.path(), &second_header, &second, &trust).unwrap();

        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            second
        );
        assert_eq!(
            fs::read(paths::registry_bundle_previous_path(dir.path()).unwrap()).unwrap(),
            first
        );
        let registry = open_verified_registry_with_trust(dir.path(), &trust).unwrap();
        let built_at = registry
            .connection()
            .query_row(
                "SELECT value_json FROM system_config WHERE key = 'built_at'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(built_at, "\"2026-11-16T12:00:00+00:00\"");
    }
}
