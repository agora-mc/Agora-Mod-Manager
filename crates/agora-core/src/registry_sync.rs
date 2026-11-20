//! Registry catalog sync: download, verify, install and read the one signed
//! `registry.bundle` from GitHub Releases.
//!
//! This module is pure (no Tauri types) so it can be consumed by the Tauri
//! GUI, the standalone `agora` CLI, and the in-process MCP listener.

use crate::db;
use crate::error::{LauncherError, LauncherResult};
use crate::lock_manager::LockManager;
use crate::operation_manager::OperationManager;
use crate::paths;
use crate::registry_bundle::{
    self, BundleError, CatalogHeader, KeyCertificate, KeyRevocation, RegistryKey, RegistryKeyRole,
    RegistryTrust, VerifiedBundle,
};
use chrono::{DateTime, Datelike, Utc};
use ed25519_dalek::{Signature, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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
/// community signatures until it is refreshed.
///
/// The number lives in `compiler/registry_schema_version`, which the compiler
/// reads too, so the app and the compiler can never name different versions.
/// Bump it there.
pub const APP_REGISTRY_SCHEMA_VERSION: i64 =
    parse_schema_version(include_str!("../../../compiler/registry_schema_version"));

/// Parse the shared schema version file (decimal digits, optional trailing
/// whitespace) at compile time; anything else fails the build.
const fn parse_schema_version(text: &str) -> i64 {
    let bytes = text.as_bytes();
    let mut value: i64 = 0;
    let mut i = 0;
    let mut digits = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            assert!(digits < 9, "compiler/registry_schema_version is too large");
            value = value * 10 + (b - b'0') as i64;
            digits += 1;
        } else if !b.is_ascii_whitespace() {
            panic!("compiler/registry_schema_version must hold only a number");
        }
        i += 1;
    }
    assert!(digits > 0, "compiler/registry_schema_version is empty");
    value
}

/// Minimum interval between automatic update checks after a successful check or install (1 hour).
const UPDATE_CHECK_INTERVAL_SECS: i64 = 3600;
/// Shorter cooldown after a failed update attempt (5 minutes): avoids hammering
/// GitHub if a UI mount or restart loop occurs, while recovering promptly from
/// transient network or asset glitches instead of waiting an hour.
const FAILED_UPDATE_RETRY_INTERVAL_SECS: i64 = 300;

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

/// Check GitHub Releases for a newer catalog and download + verify it.
///
/// Steps (per section 4.1a):
/// 1. Query GitHub Releases API for the latest catalog release.
/// 2. Compare the tag with the locally cached tag.
/// 3. If newer: download the single signed `registry.bundle` asset.
/// 4. Verify the header key id, Ed25519 signature, expiry and schema version.
/// 5. Refuse a bundle built before the installed one.
/// 6. Install the bundle with one rename, keep the previous one, and extract
///    `registry.db` from the signed payload.
/// 7. Remember the keys/revocations it taught us and update the cached tag.
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
    let transport = GitHubTransport;
    let trust = effective_trust(app_data_dir, &RegistryTrust::production());
    check_and_download_update_inner(
        app_data_dir,
        local_state_path,
        force,
        github_token,
        operation_manager,
        registry_repo,
        lock_manager,
        &transport,
        &trust,
        chrono::Utc::now().timestamp(),
    )
    .await
}

/// The parts of a bundle install that do not depend on where the bytes came
/// from: verification, expiry, rollback and schema checks, the single-rename
/// install and remembering the trust the bundle taught us.
#[derive(Debug, Clone)]
pub struct InstalledBundle {
    pub tag: String,
    pub schema_version: i64,
    pub expires_at: String,
}

/// Install a signed `registry.bundle` from a local file.
///
/// Uses the same verification and install path as a downloaded bundle, with
/// the compiled-in keys plus the remembered `registry.trust` state. A local
/// file is just another source of bytes: it takes the same cross-process
/// `RegistryUpdate` lock sync does, so the rollback check and the single-rename
/// commit cannot race a concurrent sync. The tag named by the signed bundle
/// header is kept for display, and no release asset id is recorded.
pub fn install_bundle_from_file(
    app_data_dir: &Path,
    local_state_path: &Path,
    bundle_path: &Path,
    lock_manager: &LockManager,
) -> LauncherResult<InstalledBundle> {
    let trust = effective_trust(app_data_dir, &RegistryTrust::production());
    install_bundle_from_file_with_trust(
        app_data_dir,
        local_state_path,
        bundle_path,
        &trust,
        lock_manager,
    )
}

/// As [`install_bundle_from_file`], with an explicit trust list. This is the
/// entry point tests use to install a locally built signed bundle.
pub fn install_bundle_from_file_with_trust(
    app_data_dir: &Path,
    local_state_path: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
    lock_manager: &LockManager,
) -> LauncherResult<InstalledBundle> {
    install_local_inner(
        app_data_dir,
        local_state_path,
        bundle_path,
        trust,
        lock_manager,
        None,
    )
}

/// The local install body. `timeout` lets a test prove the lock is taken
/// without waiting the production timeout; production always waits.
fn install_local_inner(
    app_data_dir: &Path,
    local_state_path: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
    lock_manager: &LockManager,
    timeout: Option<Duration>,
) -> LauncherResult<InstalledBundle> {
    // The rollback comparison in `install_bundle_bytes_checked` and the commit
    // it guards must see the same catalog. Sync holds this lock across its
    // check-and-commit, so a local install must hold it too.
    let _registry_lock = match timeout {
        Some(duration) => lock_manager.acquire_with_timeout(
            crate::lock_manager::LockResource::RegistryUpdate,
            "registry-local-install",
            duration,
            None,
        )?,
        None => lock_manager.acquire(
            crate::lock_manager::LockResource::RegistryUpdate,
            "registry-local-install",
        )?,
    };

    let bytes = std::fs::read(bundle_path).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_BUNDLE_READ".into(),
        message: format!(
            "Could not read the catalog bundle file {}: {error}.",
            bundle_path.display()
        ),
    })?;
    let installed = install_bundle_bytes_checked(
        app_data_dir,
        &bytes,
        trust,
        &bundle_path.display().to_string(),
    )?;
    // Display metadata is written after the commit; a failure here must not
    // report failure for a bundle that is already installed.
    let _ = set_cached_tag(local_state_path, &installed.tag);
    let _ = set_cached_asset_id(local_state_path, None);
    Ok(installed)
}

#[allow(clippy::too_many_arguments)]
async fn check_and_download_update_inner(
    app_data_dir: &Path,
    local_state_path: &Path,
    force: bool,
    github_token: Option<String>,
    operation_manager: Option<&OperationManager>,
    registry_repo: &str,
    lock_manager: &LockManager,
    transport: &dyn RegistryTransport,
    trust: &RegistryTrust,
    now: i64,
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
    let cached = cached_catalog_summary_with_trust(app_data_dir, trust);
    let has_cached_db = cached.is_some();
    let cached_schema_version = cached.as_ref().and_then(|summary| summary.schema_version);
    let failed_tag = get_failed_update_tag(local_state_path)?;

    if !force && has_cached_db && cached_tag.is_some() {
        if let Some(failed_tag_name) = failed_tag.as_ref() {
            if let Some(last_failure) = get_last_failure_time(local_state_path)? {
                if now - last_failure < FAILED_UPDATE_RETRY_INTERVAL_SECS {
                    return Ok(RegistryStatus {
                        has_cached_db,
                        cached_tag,
                        cached_schema_version,
                        latest_tag: Some(failed_tag_name.clone()),
                        update_available: true,
                        checked: false,
                        stale: cached.as_ref().is_some_and(|summary| summary.stale),
                        expires_at: cached.and_then(|summary| summary.expires_at),
                        message: format!(
                            "Catalog update to {failed_tag_name} failed recently; retrying soon."
                        ),
                    });
                }
            }
        } else if let Some(last_success) = get_last_success_time(local_state_path)? {
            if now - last_success < UPDATE_CHECK_INTERVAL_SECS {
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
    let _registry_lock = lock_manager
        .acquire_async(
            crate::lock_manager::LockResource::RegistryUpdate,
            "registry-sync",
        )
        .await?;

    let latest =
        match query_latest_catalog_release(transport, registry_repo, github_token.as_deref()).await
        {
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
                    latest_tag: failed_tag.clone(),
                    update_available: failed_tag.is_some(),
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

    set_last_check_time(local_state_path, now)?;

    // A merely *different* tag is not grounds to update: a signature stays
    // valid forever, so an older release republished as "latest" (or promoted
    // by release pruning) would otherwise be installed over a newer registry,
    // reinstating entries that curation has since quarantined.
    let latest_asset = latest.find_asset("registry.bundle");
    let cached_asset = get_cached_asset_id(local_state_path)?;
    // Decide whether to fetch a candidate from the signed build stamp, not the
    // signed display tag. An unverifiable cached bundle yields no date, which
    // makes the pre-filter fetch so the signed checks can have the last word.
    let cached_release_date = read_cached_stamp_checked(app_data_dir, trust)
        .ok()
        .flatten()
        .and_then(|stamp| cached_release_date_from_stamp(Some(&stamp)));
    let update_available = update_is_available(
        cached_release_date.as_deref(),
        has_cached_db,
        cached_asset,
        &latest.tag_name,
        latest_asset,
    );

    if !update_available && has_cached_db {
        let _ = clear_update_failure(local_state_path);
        let _ = set_last_success_time(local_state_path, now);
        let cached = cached_catalog_summary_with_trust(app_data_dir, trust);
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

    let install_result: LauncherResult<(i64, String, bool)> = async {
        let bundle_id =
            latest
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
        // A nightly change is usually a few KB against the catalog already here.
        // Anything that does not line up falls back to the whole bundle.
        let bundle_bytes = match bundle_from_delta(
            app_data_dir,
            &latest,
            registry_repo,
            github_token.as_deref(),
            trust,
            transport,
        )
        .await
        {
            Some(bytes) => bytes,
            None => {
                transport
                    .download_asset(&bundle_url, github_token.as_deref())
                    .await?
            }
        };

        // A same-day republish, or a tag published across UTC midnight, can
        // hand us the bytes we already have. Do not reinstall or claim an
        // update: just learn the asset id so the next check stops fetching it.
        if let Some(current) = unchanged_candidate(app_data_dir, &bundle_bytes, trust)? {
            let _ = set_cached_asset_id(local_state_path, Some(bundle_id));
            return Ok((current.schema_version, current.expires_at, false));
        }

        let installed =
            install_bundle_bytes_checked(app_data_dir, &bundle_bytes, trust, &latest.tag_name)?;
        // Display/asset metadata is written after the commit. The bundle is
        // already current, so a write failure here must not be reported as a
        // failed install; the next sync can set it again.
        let _ = set_cached_tag(local_state_path, &latest.tag_name);
        let _ = set_cached_asset_id(local_state_path, Some(bundle_id));
        Ok((installed.schema_version, installed.expires_at, true))
    }
    .await;

    let (schema_version, expires_at, changed) = match install_result {
        Ok(ok) => ok,
        Err(error) => {
            // Bookkeeping must never mask the real failure or make a committed
            // bundle look like a failed update.
            let _ = set_update_failure(local_state_path, now, &latest.tag_name);
            return Err(error);
        }
    };

    let _ = clear_update_failure(local_state_path);
    let _ = set_last_success_time(local_state_path, now);

    if let Some(op) = _op {
        op.complete();
    }

    Ok(RegistryStatus {
        has_cached_db: true,
        cached_tag: if changed {
            Some(latest.tag_name.clone())
        } else {
            cached_tag
        },
        cached_schema_version: Some(schema_version),
        latest_tag: Some(latest.tag_name.clone()),
        update_available: false,
        checked: true,
        stale: false,
        expires_at: Some(expires_at),
        message: if changed {
            format!("Catalog updated to {}.", latest.tag_name)
        } else {
            "Catalog is up to date.".to_string()
        },
    })
}

/// Rebuild the latest bundle from the cached catalog and a published delta
/// (MASTER_SPEC §33), or `None` when that is not possible.
///
/// The reference is the cached bundle's payload, verified before use, and the
/// rebuilt bundle must pass the same [`registry_bundle::verify_bundle`] check
/// as a full download, so a delta can save bytes but never change what is
/// trusted. Every failure returns `None` and the caller downloads the bundle.
async fn bundle_from_delta(
    app_data_dir: &Path,
    latest: &GitHubRelease,
    registry_repo: &str,
    token: Option<&str>,
    trust: &RegistryTrust,
    transport: &dyn RegistryTransport,
) -> Option<Vec<u8>> {
    let bundle_path = paths::registry_bundle_path(app_data_dir).ok()?;
    if !bundle_path.exists() {
        return None;
    }
    let current = load_payload(&bundle_path, trust).ok()?;
    let delta_name = registry_bundle::delta_asset_name(&current.header.db_sha256)?;
    let header_id = latest.find_asset(registry_bundle::HEADER_ASSET)?;
    let delta_id = latest.find_asset(&delta_name)?;
    let asset_url =
        |id: u64| format!("https://api.github.com/repos/{registry_repo}/releases/assets/{id}");

    let mut bundle = transport
        .download_asset(&asset_url(header_id), token)
        .await
        .ok()?;
    let db_len = registry_bundle::signed_prefix_db_len(&bundle).ok()?;
    if db_len > registry_bundle::MAX_DELTA_PAYLOAD {
        return None;
    }
    let delta = transport
        .download_asset(&asset_url(delta_id), token)
        .await
        .ok()?;
    let payload = registry_bundle::apply_payload_delta(&current.payload, &delta, db_len).ok()?;
    bundle.extend_from_slice(&payload);
    registry_bundle::verify_bundle(&bundle, trust).ok()?;
    Some(bundle)
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

// ---------------------------------------------------------------------------
// What this client remembers about catalog keys
// ---------------------------------------------------------------------------

/// Keys and revocations learned from bundles that verified here before. The
/// file is a line log, so a truncated final line only loses that line.
///
/// ```text
/// v1 key <key_id> <public_key> <role> <signed_by> <signature>
/// v1 revoke <revoked_key_id> <signed_by> <signature>
/// ```
///
/// Every line is a signed statement. On load each one is re-verified against the
/// keys trusted at that point in the log: a key line is a certificate signed by
/// a recovery key, and a revocation line is a revocation statement signed by a
/// recovery key. A forged line is ignored.
#[derive(Debug, Default)]
struct RememberedTrust {
    keys: Vec<RegistryKey>,
    revocations: Vec<KeyRevocation>,
}

fn parse_role(raw: &str) -> Option<RegistryKeyRole> {
    match raw {
        "primary" => Some(RegistryKeyRole::Primary),
        "recovery" => Some(RegistryKeyRole::Recovery),
        _ => None,
    }
}

fn load_remembered_trust(app_data_dir: &Path, base: &RegistryTrust) -> RememberedTrust {
    let path = match paths::registry_trust_state_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return RememberedTrust::default(),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Missing or unreadable: the compiled keys are the whole trust set.
        // No key introduced by certificate is trusted in this state.
        return RememberedTrust::default();
    };

    let mut trusted: BTreeMap<String, ed25519_dalek::VerifyingKey> = BTreeMap::new();
    let mut roles: BTreeMap<String, RegistryKeyRole> = BTreeMap::new();
    let mut revoked: BTreeSet<String> = base.revoked_ids().into_iter().collect();
    for key in base.keys() {
        if revoked.contains(&key.id) {
            continue;
        }
        if let Some(verifying) = verifying_key_hex(&key.public_key_hex) {
            trusted.insert(key.id.clone(), verifying);
            roles.insert(key.id.clone(), key.role);
        }
    }

    let mut remembered_keys: Vec<RegistryKey> = Vec::new();
    let mut remembered_revocations: Vec<KeyRevocation> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            ["v1", "key", id, public_key, role, signed_by, signature] => {
                let Some(role) = parse_role(role) else {
                    continue;
                };
                let certificate = KeyCertificate {
                    key_id: (*id).to_string(),
                    public_key: (*public_key).to_string(),
                    role,
                    signed_by: (*signed_by).to_string(),
                    signature: (*signature).to_string(),
                };
                // Revocations win: a revoked id cannot be introduced again.
                if revoked.contains(&certificate.key_id) {
                    continue;
                }
                let Some(signer) = trusted.get(&certificate.signed_by).copied() else {
                    continue;
                };
                if roles.get(&certificate.signed_by) != Some(&RegistryKeyRole::Recovery) {
                    continue;
                }
                if revoked.contains(&certificate.signed_by) {
                    continue;
                }
                if registry_bundle::key_id_for_public_key(&certificate.public_key)
                    != certificate.key_id
                {
                    continue;
                }
                let Some(signature) = decode_hex_signature(&certificate.signature) else {
                    continue;
                };
                use ed25519_dalek::Verifier;
                let message = KeyCertificate::signed_bytes(
                    &certificate.key_id,
                    &certificate.public_key,
                    certificate.role,
                    &certificate.signed_by,
                );
                if signer.verify(&message, &signature).is_err() {
                    continue;
                }
                let Some(verifying) = verifying_key_hex(&certificate.public_key) else {
                    continue;
                };
                trusted.insert(certificate.key_id.clone(), verifying);
                roles.insert(certificate.key_id.clone(), certificate.role);
                remembered_keys.push(
                    RegistryKey::from_hex(certificate.public_key.clone())
                        .with_role(certificate.role),
                );
            }
            ["v1", "revoke", revoked_id, signed_by, signature] => {
                if revoked_id == signed_by {
                    continue;
                }
                let Some(signer) = trusted.get(*signed_by).copied() else {
                    continue;
                };
                if roles.get(*signed_by) != Some(&RegistryKeyRole::Recovery) {
                    continue;
                }
                if revoked.contains(*signed_by) {
                    continue;
                }
                let Some(decoded) = decode_hex_signature(signature) else {
                    continue;
                };
                use ed25519_dalek::Verifier;
                let message = KeyRevocation::signed_bytes(revoked_id, signed_by);
                if signer.verify(&message, &decoded).is_err() {
                    continue;
                }
                revoked.insert((*revoked_id).to_string());
                trusted.remove(*revoked_id);
                roles.remove(*revoked_id);
                remembered_revocations.push(KeyRevocation {
                    revoked_key_id: (*revoked_id).to_string(),
                    signed_by: (*signed_by).to_string(),
                    signature: (*signature).to_string(),
                });
            }
            // A truncated line, an older version, or junk: ignore it.
            _ => {}
        }
    }

    RememberedTrust {
        keys: remembered_keys,
        revocations: remembered_revocations,
    }
}

/// Compiled keys plus what this client remembered, minus what it remembered as
/// revoked.
fn effective_trust(app_data_dir: &Path, base: &RegistryTrust) -> RegistryTrust {
    let remembered = load_remembered_trust(app_data_dir, base);
    let mut trust = base.clone();
    trust.merge_remembered(remembered.keys, remembered.revocations);
    trust
}

/// Verify the bundle at `bundle_path` and durably reconcile the trust statements
/// it carries into `registry.trust`. This must succeed before a replacement can
/// be validated: a revocation that was committed but whose append failed would
/// otherwise be forgotten, letting a later bundle signed by the retired key
/// through. An expired bundle is reconciled too — retirement is permanent.
fn reconcile_bundle_trust_at(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<()> {
    let verified = load_payload(bundle_path, trust).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_TRUST_STATE".into(),
        message: format!(
            "The installed catalog's trust statements could not be verified ({error}); a \
             replacement cannot be proved safe."
        ),
    })?;
    remember_bundle_trust(app_data_dir, &verified)
}

/// Reconcile the current bundle's trust statements under the caller's lock.
fn reconcile_current_trust(app_data_dir: &Path, trust: &RegistryTrust) -> LauncherResult<()> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        reconcile_bundle_trust_at(app_data_dir, &bundle_path, trust)?;
    }
    Ok(())
}

/// Persist the keys a verified bundle introduced and the revocations a recovery
/// key signed. Only complete lines are appended, and the file is never
/// rewritten, so a crash cannot lose an earlier revocation. Duplicate lines are
/// harmless.
fn remember_bundle_trust(app_data_dir: &Path, verified: &VerifiedBundle) -> LauncherResult<()> {
    // Expiry is not a reason to skip a revocation: a retired key stays retired
    // even if the bundle that carried the statement has since expired.
    let path =
        paths::registry_trust_state_path(app_data_dir).map_err(|_| LauncherError::Generic {
            code: "ERR_REGISTRY_TRUST_STATE".into(),
            message: "Could not resolve the catalog trust state path.".into(),
        })?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines = String::new();
    for certificate in &verified.newly_trusted {
        let marker = format!("v1 key {} ", certificate.key_id);
        if existing.lines().any(|line| line.starts_with(&marker)) {
            continue;
        }
        lines.push_str(&format!(
            "v1 key {} {} {} {} {}\n",
            certificate.key_id,
            certificate.public_key,
            certificate.role.as_str(),
            certificate.signed_by,
            certificate.signature
        ));
    }
    for revocation in &verified.accepted_revocations {
        let marker = format!(
            "v1 revoke {} {} {}",
            revocation.revoked_key_id, revocation.signed_by, revocation.signature
        );
        if existing.lines().any(|line| line == marker) {
            continue;
        }
        lines.push_str(&format!(
            "v1 revoke {} {} {}\n",
            revocation.revoked_key_id, revocation.signed_by, revocation.signature
        ));
    }
    if lines.is_empty() {
        return Ok(());
    }
    if existing.is_empty() {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).ok();
    } else if !existing.ends_with('\n') {
        lines.insert(0, '\n');
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| LauncherError::Generic {
            code: "ERR_REGISTRY_TRUST_STATE".into(),
            message: format!("Could not write the catalog trust state: {error}."),
        })?;
    file.write_all(lines.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| LauncherError::Generic {
            code: "ERR_REGISTRY_TRUST_STATE".into(),
            message: format!("Could not save the catalog trust state: {error}."),
        })
}

fn verifying_key_hex(public_key_hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes = hex::decode(public_key_hex.trim()).ok()?;
    let array: [u8; PUBLIC_KEY_LENGTH] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&array).ok()
}

fn decode_hex_signature(raw: &str) -> Option<Signature> {
    let bytes = hex::decode(raw.trim()).ok()?;
    let array: [u8; SIGNATURE_LENGTH] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&array))
}

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
    trust.fingerprint()
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
    let app_data_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let trust = effective_trust(app_data_dir, trust);
    let metadata = std::fs::metadata(path).map_err(BundleLoadError::Io)?;
    let fingerprint = trust_fingerprint(&trust);
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
        registry_bundle::verify_bundle(&bytes, &trust).map_err(BundleLoadError::Bundle)?;
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
    let app_data_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let trust = effective_trust(app_data_dir, trust);
    let bytes = std::fs::read(path).map_err(BundleLoadError::Io)?;
    registry_bundle::verify_bundle(&bytes, &trust).map_err(BundleLoadError::Bundle)
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
        | BundleError::CertificateInvalid(_)
        | BundleError::KeyNotForCatalog(_) => "ERR_REGISTRY_SIGNATURE_INVALID",
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

/// A hash of the readable database, keyed by the file identity it was computed
/// from. The registry connection is opened `SQLITE_OPEN_READ_ONLY`, so the only
/// writer is `ensure_database` itself (install/recovery) and the debug seed.
/// Re-hashing a multi-megabyte catalog on every open is what this avoids.
///
/// Process-global so every opener shares it. Move it into `Ctx` with the planned
/// `Ctx` cleanup (MASTER_SPEC section 32.9 when that section lands).
#[derive(Debug, Clone)]
struct DatabaseHashMemo {
    len: u64,
    modified: Option<std::time::SystemTime>,
    sha256: String,
}

fn database_hash_memo() -> &'static Mutex<HashMap<PathBuf, DatabaseHashMemo>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, DatabaseHashMemo>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_database_hash(path: &Path, sha256: &str) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if let Ok(mut memo) = database_hash_memo().lock() {
        memo.insert(
            path.to_path_buf(),
            DatabaseHashMemo {
                len: metadata.len(),
                modified: metadata.modified().ok(),
                sha256: sha256.to_ascii_lowercase(),
            },
        );
    }
}

fn database_hash_memoized(path: &Path) -> std::io::Result<String> {
    let metadata = std::fs::metadata(path)?;
    if let Ok(memo) = database_hash_memo().lock() {
        if let Some(entry) = memo.get(path) {
            if entry.len == metadata.len() && entry.modified == metadata.modified().ok() {
                return Ok(entry.sha256.clone());
            }
        }
    }
    let hash = sha256_file(path)?;
    remember_database_hash(path, &hash);
    Ok(hash)
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
        if let Ok(hash) = database_hash_memoized(&db_path) {
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
    })?;
    remember_database_hash(&db_path, &header.db_sha256);
    Ok(())
}

/// Verify candidate bundle bytes and install them only when they are current,
/// newer than the installed catalog and supported by this app.
///
/// Both the GitHub release sync and the local-file install call this after the
/// bytes are obtained, so expiry, rollback and schema rules cannot drift
/// between the two sources. `label` names the source in refusal messages: a
/// release tag for sync, a file path for a local install.
fn install_bundle_bytes_checked(
    app_data_dir: &Path,
    bundle_bytes: &[u8],
    trust: &RegistryTrust,
    label: &str,
) -> LauncherResult<InstalledBundle> {
    // Both callers hold RegistryUpdate here. Reconcile what the current bundle
    // taught us *before* validating the replacement, so a revocation committed
    // by an earlier bundle is applied even if its append originally failed.
    // If that cannot be done, refuse the replacement rather than risk reviving
    // a retired signer.
    reconcile_current_trust(app_data_dir, trust)?;
    let trust = effective_trust(app_data_dir, trust);
    let verified = registry_bundle::verify_bundle(bundle_bytes, &trust).map_err(bundle_error)?;

    // Anti-rollback is checked only after the signature, so the stamp being
    // compared is one the signing key vouched for. The release tag is display
    // metadata and never takes part in this decision.
    let cached_stamp = read_cached_stamp_checked(app_data_dir, &trust)?;
    match compare_stamps(
        cached_stamp.as_ref(),
        &verified.header.built_at,
        &verified.header.db_sha256,
    )? {
        StampOrder::Older => {
            return Err(LauncherError::Generic {
                code: "ERR_REGISTRY_ROLLBACK".into(),
                message: format!(
                    "Refusing registry {}: it was built at {}, older than the registry already \
                     installed ({}). The cached registry has been kept.",
                    label,
                    verified.header.built_at,
                    cached_stamp
                        .as_ref()
                        .map(|stamp| stamp.built_at.as_str())
                        .unwrap_or("unknown"),
                ),
            })
        }
        StampOrder::EqualDifferentBytes => {
            return Err(LauncherError::Generic {
                code: "ERR_REGISTRY_ROLLBACK".into(),
                message: format!(
                    "Refusing registry {}: it carries a different signed payload with the same \
                     build stamp ({}) as the registry already installed. The cached registry has \
                     been kept.",
                    label, verified.header.built_at
                ),
            })
        }
        StampOrder::Newer | StampOrder::EqualSameBytes => {}
    }

    // An expired bundle stays readable but is never installed as current: doing
    // so would replace a good cache with one that cannot be used for an install
    // or update.
    if verified.header.is_expired_at(Utc::now()) {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_EXPIRED".into(),
            message: format!(
                "Refusing registry {}: it expired at {}. The cached registry has been kept.",
                label, verified.header.expires_at
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

    install_bundle_bytes(app_data_dir, &verified.header, bundle_bytes, &trust)?;

    Ok(InstalledBundle {
        tag: verified.header.tag,
        schema_version,
        expires_at: verified.header.expires_at,
    })
}

/// If `bundle_bytes` are exactly the installed bundle, return its install
/// metadata without writing anything. The installed bundle's trust statements
/// are reconciled first, so reporting "up to date" can never skip a reconciliation
/// that is still owed. Anything else — including equal stamps over different
/// bytes — is not unchanged and goes through the normal checks.
fn unchanged_candidate(
    app_data_dir: &Path,
    bundle_bytes: &[u8],
    trust: &RegistryTrust,
) -> LauncherResult<Option<InstalledBundle>> {
    reconcile_current_trust(app_data_dir, trust)?;
    let trust = effective_trust(app_data_dir, trust);
    let verified = registry_bundle::verify_bundle(bundle_bytes, &trust).map_err(bundle_error)?;
    let cached = read_cached_stamp_checked(app_data_dir, &trust)?;
    if compare_stamps(
        cached.as_ref(),
        &verified.header.built_at,
        &verified.header.db_sha256,
    )? != StampOrder::EqualSameBytes
    {
        return Ok(None);
    }
    let current =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if !std::fs::read(current)
        .map(|bytes| bytes == bundle_bytes)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    Ok(Some(InstalledBundle {
        tag: verified.header.tag,
        schema_version: verified.header.schema_version,
        expires_at: verified.header.expires_at,
    }))
}

/// Install the exact signed bytes of a bundle that has already been verified.
///
/// The bundle is one file, so it is replaced by one rename. Everything that can
/// fail before that rename stops the install without changing trust; the
/// previous good bundle is copied to `registry.bundle.prev` first, so a crash
/// at any point leaves either the old current or the old previous usable.
///
/// The bundle rename is the single commit point. Revocations and key trust are
/// persisted only *after* it: writing a revocation first could untrust the
/// current bundle while it is still current, and an abort would strand the
/// cache on an unreadable stamp. Anything that fails after the rename is
/// best-effort — the bundle is installed and carries its own certificates and
/// revocations, so it still verifies — and is not reported as a failed install.
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
    let trust = effective_trust(app_data_dir, trust);
    let payload = registry_bundle::verify_bundle(bundle_bytes, &trust).map_err(bundle_error)?;

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

    // COMMIT: one rename replaces the current bundle.
    write_atomic(&bundle_path, bundle_bytes).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_WRITE".into(),
        message: format!("Could not install the catalog bundle: {error}."),
    })?;

    // Post-commit, best-effort. The signature lives inside the bundle now, so
    // an old sidecar would describe bytes that are no longer there.
    if let Ok(sig_path) = paths::registry_sig_path(app_data_dir) {
        let _ = std::fs::remove_file(sig_path);
    }
    // The readable database is a derived copy: if this fails, the next open
    // extracts it again from the bundle that is now current.
    let _ = ensure_database(
        app_data_dir,
        &bundle_path,
        header,
        Some(&payload.payload),
        &trust,
    );
    // A revocation may revoke the key of the bundle we just replaced. Persist
    // it only now that the replacement is in place. Trust statements are not
    // bookkeeping: if a statement exists and the durable write fails, report
    // the failure so it is not mistaken for a clean install. The next install
    // reconciles the installed bundle's statements before it validates anything.
    remember_bundle_trust(app_data_dir, &payload)?;
    Ok(())
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

    // A pre-bundle cache is not a catalog. It has no signed expiry, no key id
    // and no rotation, so this client ignores it and the next sync downloads a
    // bundle. The next successful install replaces `registry.db`.
    CachedRegistryIntegrity::Absent
}

fn verify_bundle_file(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &RegistryTrust,
) -> CachedRegistryIntegrity {
    match load_header(bundle_path, trust) {
        Ok(header) => match ensure_database(app_data_dir, bundle_path, &header, None, trust) {
            Ok(()) => match reconcile_bundle_trust_at(app_data_dir, bundle_path, trust) {
                Ok(()) => {
                    if header.is_expired_at(Utc::now()) {
                        CachedRegistryIntegrity::Stale(stale_message(&header))
                    } else {
                        CachedRegistryIntegrity::Verified
                    }
                }
                // An unaccounted-for revocation is not a usable catalog.
                Err(error) => CachedRegistryIntegrity::Error(format!(
                    "{error} The catalog will not be used this run."
                )),
            },
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
            if ensure_database(app_data_dir, &previous_path, &header, None, trust).is_ok()
                && reconcile_bundle_trust_at(app_data_dir, &previous_path, trust).is_ok()
            {
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

fn stale_message(header: &CatalogHeader) -> String {
    format!(
        "The catalog expired on {}. It can still be browsed, but installs and updates wait for a fresh catalog.",
        header.expires_at
    )
}

/// The signed build stamp and payload hash of the cached catalog, for the
/// rollback check. The hash lets an equal stamp over different bytes be
/// refused instead of silently replacing the current catalog.
#[derive(Debug, Clone)]
struct CachedStamp {
    built_at: String,
    db_sha256: String,
}

/// An absent catalog is `None` (nothing to roll back from). A catalog that is
/// present but cannot be read or verified is an error: treating "unknown" as
/// "older" is what lets a replayed old signed catalog replace a newer one.
fn read_cached_stamp_checked(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> LauncherResult<Option<CachedStamp>> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        return match load_header(&bundle_path, trust) {
            Ok(header) => Ok(Some(CachedStamp {
                built_at: header.built_at,
                db_sha256: header.db_sha256,
            })),
            Err(error) => Err(LauncherError::Generic {
                code: "ERR_REGISTRY_CACHED_STAMP_UNREADABLE".into(),
                message: format!(
                    "The installed catalog's signed build stamp cannot be read ({error}), so an update cannot be proved newer. The cached catalog has been kept."
                ),
            }),
        };
    }

    // No bundle is no catalog: there is no signed stamp to roll back from.
    Ok(None)
}

fn cached_catalog_summary_with_trust(
    app_data_dir: &Path,
    trust: &RegistryTrust,
) -> Option<CachedCatalogSummary> {
    let bundle_path = paths::registry_bundle_path(app_data_dir).ok()?;
    if bundle_path.exists() {
        let header = load_header(&bundle_path, trust).ok()?;
        return Some(CachedCatalogSummary {
            schema_version: Some(header.schema_version),
            stale: header.is_expired_at(Utc::now()),
            expires_at: Some(header.expires_at),
        });
    }
    None
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

    // No bundle is no catalog.
    Ok(None)
}

/// Return the current registry status without performing a network check.
pub fn get_status(app_data_dir: &Path, local_state_path: &Path) -> RegistryStatus {
    get_status_with_trust(app_data_dir, local_state_path, &RegistryTrust::production())
}

fn get_status_with_trust(
    app_data_dir: &Path,
    local_state_path: &Path,
    trust: &RegistryTrust,
) -> RegistryStatus {
    let cached = cached_catalog_summary_with_trust(app_data_dir, trust);
    let has_cached_db = cached.is_some();
    let cached_tag = get_cached_tag(local_state_path).ok().flatten();
    let failed_tag = if has_cached_db {
        get_failed_update_tag(local_state_path).ok().flatten()
    } else {
        None
    };
    let cached_schema_version = cached.as_ref().and_then(|summary| summary.schema_version);
    let stale = cached.as_ref().is_some_and(|summary| summary.stale);
    let expires_at = cached.and_then(|summary| summary.expires_at);
    RegistryStatus {
        has_cached_db,
        cached_tag,
        cached_schema_version,
        latest_tag: failed_tag.clone(),
        update_available: failed_tag.is_some(),
        checked: false,
        stale,
        expires_at,
        message: if has_cached_db {
            if stale {
                "Using cached catalog. It is past its signed expiry; installs and updates are disabled until it refreshes."
                    .to_string()
            } else if let Some(tag) = failed_tag {
                format!("Using cached catalog; update to {tag} failed and will retry.")
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

    Ok(false)
}

// ---------------------------------------------------------------------------
// Internal helpers â€” GitHub API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[async_trait::async_trait]
trait RegistryTransport: Send + Sync {
    async fn releases_page(
        &self,
        repo: &str,
        page: u32,
        token: Option<&str>,
    ) -> LauncherResult<Vec<GitHubRelease>>;

    async fn download_asset(&self, url: &str, token: Option<&str>) -> LauncherResult<Vec<u8>>;
}

struct GitHubTransport;

const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: u32 = 10;

#[async_trait::async_trait]
impl RegistryTransport for GitHubTransport {
    async fn releases_page(
        &self,
        repo: &str,
        page: u32,
        token: Option<&str>,
    ) -> LauncherResult<Vec<GitHubRelease>> {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page={}&page={}",
            repo, RELEASES_PER_PAGE, page
        );
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
        serde_json::from_slice::<Vec<GitHubRelease>>(&body).map_err(|e| LauncherError::Generic {
            code: "ERR_REGISTRY_CHECK".into(),
            message: format!("Catalog release metadata was not readable: {e}."),
        })
    }

    async fn download_asset(&self, url: &str, token: Option<&str>) -> LauncherResult<Vec<u8>> {
        download_file(url, token).await
    }
}

fn select_latest_catalog_release(releases: &[GitHubRelease]) -> Option<&GitHubRelease> {
    let mut best: Option<((u16, u8, u8), &GitHubRelease)> = None;
    for release in releases {
        if release.draft || release.prerelease {
            continue;
        }
        let Some(date) = parse_registry_tag(&release.tag_name) else {
            continue;
        };
        if release.find_asset("registry.bundle").is_none() {
            continue;
        }
        if best.is_none_or(|(best_date, _)| date > best_date) {
            best = Some((date, release));
        }
    }
    best.map(|(_, release)| release)
}

async fn query_latest_catalog_release(
    transport: &dyn RegistryTransport,
    repo: &str,
    token: Option<&str>,
) -> LauncherResult<GitHubRelease> {
    for page in 1..=MAX_RELEASE_PAGES {
        let releases = transport.releases_page(repo, page, token).await?;
        if let Some(found) = select_latest_catalog_release(&releases) {
            return Ok(found.clone());
        }
        if releases.len() < RELEASES_PER_PAGE {
            break;
        }
    }
    Err(LauncherError::Generic {
        code: "ERR_REGISTRY_NOT_FOUND".into(),
        message: "No catalog release found.".into(),
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

    // A debug-only test fixture: `Ctx::for_testing` may open a hand-built
    // unsigned database. This is never reachable from a release build because
    // `permits_unsigned_debug` is false there. A pre-bundle `registry.db` +
    // `.sig` is not accepted; it has no signed expiry, no key id and no
    // rotation, so the next sync downloads a bundle.
    #[cfg(debug_assertions)]
    if trust.permits_unsigned_debug() {
        let db_path =
            paths::registry_db_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
        if db_path.exists() {
            let conn =
                db::registry_connection(&db_path).map_err(|_| LauncherError::RegistryMissing)?;
            return Ok(VerifiedRegistry {
                conn: Some(conn),
                freshness: RegistryFreshness::default(),
            });
        }
    }
    Err(LauncherError::RegistryMissing)
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
#[cfg(test)]
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
    // A release published just after UTC midnight can carry a tag a day ahead
    // of its signed build stamp. If this is the exact asset we already have,
    // the tag/date mismatch is not a reason to fetch it again; otherwise sync
    // redownloads the same bytes on every check.
    if let (Some(cached), Some(latest)) = (cached_asset, latest_asset) {
        if cached == latest {
            return false;
        }
    }
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

/// How a candidate's signed build stamp compares with the cached one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StampOrder {
    /// The candidate is older; installing it would roll the catalog back.
    Older,
    /// Same instant and same signed payload: a harmless no-op.
    EqualSameBytes,
    /// Same instant but different signed bytes: refuse rather than pick a winner.
    EqualDifferentBytes,
    Newer,
}

/// Compare signed build stamps as parsed UTC instants, not as text: RFC 3339
/// offsets can sort differently from the moments they represent. An unparseable
/// stamp is an error, never permission to replace the current catalog.
///
/// Both stamps come from signature-verified bytes, so unlike the release tag
/// they cannot be forged without the signing key. A cached database predating
/// the stamp has no value to compare, and refusing every update in that state
/// would strand clients, so that direction allows the update.
fn compare_stamps(
    cached: Option<&CachedStamp>,
    candidate_built_at: &str,
    candidate_db_sha256: &str,
) -> LauncherResult<StampOrder> {
    let candidate_time = parse_utc(candidate_built_at).ok_or_else(|| LauncherError::Generic {
        code: "ERR_REGISTRY_STAMP_INVALID".into(),
        message: format!(
            "The candidate catalog's signed build stamp `{candidate_built_at}` cannot be read."
        ),
    })?;
    let Some(cached) = cached else {
        return Ok(StampOrder::Newer);
    };
    let cached_time = parse_utc(&cached.built_at).ok_or_else(|| LauncherError::Generic {
        code: "ERR_REGISTRY_CACHED_STAMP_UNREADABLE".into(),
        message: "The installed catalog's signed build stamp cannot be read.".into(),
    })?;
    match candidate_time.cmp(&cached_time) {
        std::cmp::Ordering::Less => Ok(StampOrder::Older),
        std::cmp::Ordering::Greater => Ok(StampOrder::Newer),
        std::cmp::Ordering::Equal => {
            if candidate_db_sha256.eq_ignore_ascii_case(&cached.db_sha256) {
                Ok(StampOrder::EqualSameBytes)
            } else {
                Ok(StampOrder::EqualDifferentBytes)
            }
        }
    }
}

/// The UTC release date carried by the cached catalog's signed stamp, in the
/// `registry-YYYY-MM-DD` form `update_is_available` compares. This is the
/// authority for "is the remote candidate newer"; the signed display tag can
/// be future-dated or unparseable and must not decide that question.
fn cached_release_date_from_stamp(stamp: Option<&CachedStamp>) -> Option<String> {
    let time = parse_utc(&stamp?.built_at)?;
    Some(format!(
        "registry-{:04}-{:02}-{:02}",
        time.year(),
        time.month(),
        time.day()
    ))
}

/// Read the schema version from the cached registry database at `path`.
#[cfg(test)]
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

#[cfg(test)]
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

fn get_last_success_time(local_state_path: &std::path::Path) -> LauncherResult<Option<i64>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "last_registry_update_success")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_i64()))
}

fn set_last_success_time(local_state_path: &std::path::Path, timestamp: i64) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "last_registry_update_success",
        &serde_json::Value::Number(timestamp.into()),
    )
    .map_err(|_| LauncherError::LocalStateFailed)
}

fn get_last_failure_time(local_state_path: &std::path::Path) -> LauncherResult<Option<i64>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "last_registry_update_failure")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_i64()))
}

fn get_failed_update_tag(local_state_path: &std::path::Path) -> LauncherResult<Option<String>> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::get_setting(&conn, "failed_registry_update_tag")
        .map_err(|_| LauncherError::LocalStateFailed)
        .map(|v| v.and_then(|v| v.as_str().map(|s| s.to_string())))
}

fn set_update_failure(
    local_state_path: &std::path::Path,
    timestamp: i64,
    tag: &str,
) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "last_registry_update_failure",
        &serde_json::Value::Number(timestamp.into()),
    )
    .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "failed_registry_update_tag",
        &serde_json::Value::String(tag.to_string()),
    )
    .map_err(|_| LauncherError::LocalStateFailed)
}

fn clear_update_failure(local_state_path: &std::path::Path) -> LauncherResult<()> {
    let conn = db::local_state_connection(local_state_path)
        .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "last_registry_update_failure",
        &serde_json::Value::Null,
    )
    .map_err(|_| LauncherError::LocalStateFailed)?;
    db::set_setting(
        &conn,
        "failed_registry_update_tag",
        &serde_json::Value::Null,
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
            revocations: Vec::new(),
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

    fn local_state_in(dir: &Path) -> PathBuf {
        let path = dir.join("local_state.db");
        db::init_local_state_db(&path).unwrap();
        path
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
        // A tag published after UTC midnight can be a day ahead of the signed
        // stamp. The same asset id is still the same bytes, so do not fetch it.
        assert!(!update_is_available(
            Some(today),
            true,
            Some(7),
            "registry-2026-11-01",
            Some(7)
        ));
        // A different asset id in that newer release is a real republish.
        assert!(update_is_available(
            Some(today),
            true,
            Some(7),
            "registry-2026-11-01",
            Some(8)
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
    fn compare_stamps_orders_signed_build_stamps() {
        let older = "2026-08-26T03:19:02+00:00";
        let newer = "2026-08-27T12:20:15+00:00";
        let hash = "0".repeat(64);
        let cached = |built_at: &str, sha: &str| CachedStamp {
            built_at: built_at.into(),
            db_sha256: sha.into(),
        };
        assert_eq!(
            compare_stamps(Some(&cached(newer, &hash)), older, &hash).unwrap(),
            StampOrder::Older
        );
        assert_eq!(
            compare_stamps(Some(&cached(older, &hash)), newer, &hash).unwrap(),
            StampOrder::Newer
        );
        assert_eq!(
            compare_stamps(Some(&cached(newer, &hash)), newer, &hash).unwrap(),
            StampOrder::EqualSameBytes
        );
        assert_eq!(
            compare_stamps(Some(&cached(newer, &hash)), newer, &"1".repeat(64)).unwrap(),
            StampOrder::EqualDifferentBytes
        );
        assert_eq!(
            compare_stamps(None, newer, &hash).unwrap(),
            StampOrder::Newer
        );
        assert_eq!(
            compare_stamps(Some(&cached(newer, &hash)), "not a time", &hash)
                .unwrap_err()
                .code(),
            "ERR_REGISTRY_STAMP_INVALID"
        );
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

    /// A bundle supplied as a local file goes through the same verification
    /// and install as a downloaded one, and the installed catalog opens.
    #[test]
    fn a_local_bundle_file_installs_and_opens() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(60);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        let source_name = "supplied-registry.bundle";
        write_bundle(dir.path(), source_name, &key, &header, &payload);
        let locks = LockManager::new(dir.path().join("locks"));
        // A previous online asset id must not survive a local install: it would
        // make the next same-day sync look up to date.
        set_cached_asset_id(&ls_path, Some(4242)).unwrap();

        let installed = install_bundle_from_file_with_trust(
            dir.path(),
            &ls_path,
            &dir.path().join(source_name),
            &trust,
            &locks,
        )
        .unwrap();
        assert_eq!(installed.tag, header.tag);
        assert_eq!(installed.schema_version, 10);
        assert_eq!(installed.expires_at, header.expires_at);
        // The signed header's tag is remembered exactly like a release tag, so
        // status shows it and a later sync compares against it.
        assert_eq!(
            get_cached_tag(&ls_path).unwrap().as_deref(),
            Some("registry-2026-11-16")
        );
        assert_eq!(get_cached_asset_id(&ls_path).unwrap(), None);
        let status = get_status(dir.path(), &ls_path);
        assert_eq!(status.cached_tag.as_deref(), Some("registry-2026-11-16"));
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            signed_bundle(&key, &header, &payload)
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
        registry.require_fresh().unwrap();
    }

    /// A tampered local bundle is refused with the exact message the download
    /// path gives for a payload whose hash does not match its signed header.
    #[test]
    fn a_tampered_local_bundle_is_refused_with_the_download_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(61);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        let mut bytes = signed_bundle(&key, &header, &payload);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let source = dir.path().join("tampered.bundle");
        fs::write(&source, &bytes).unwrap();
        let locks = LockManager::new(dir.path().join("locks"));

        let error =
            install_bundle_from_file_with_trust(dir.path(), &ls_path, &source, &trust, &locks)
                .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_BUNDLE_HASH");
        assert_eq!(
            error.to_string(),
            "The catalog bundle could not be used: the catalog payload does not match the signed SHA-256."
        );
        assert!(!paths::registry_bundle_path(dir.path()).unwrap().exists());
    }

    /// A bundle signed by an unknown key is refused with the download path's
    /// unknown-key message.
    #[test]
    fn an_unknown_key_local_bundle_is_refused_with_the_download_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let signer = test_key(62);
        let trust = RegistryTrust::with_key(test_public(&test_key(63)));
        let payload = sample_db();
        let header = header_for(
            &signer,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let source_name = "unknown-key.bundle";
        write_bundle(dir.path(), source_name, &signer, &header, &payload);
        let locks = LockManager::new(dir.path().join("locks"));

        let error = install_bundle_from_file_with_trust(
            dir.path(),
            &ls_path,
            &dir.path().join(source_name),
            &trust,
            &locks,
        )
        .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_SIGNATURE_INVALID");
        assert_eq!(
            error.to_string(),
            format!(
                "The catalog bundle could not be used: the catalog was signed by unknown key {}.",
                header.key_id
            )
        );
    }

    /// An expired local bundle is refused even though its signature verifies,
    /// with the same expiry message the download path gives.
    #[test]
    fn an_expired_local_bundle_is_refused_with_the_download_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(64);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = header_for(
            &key,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2025-02-01T00:00:00+00:00",
        );
        let source = dir.path().join("expired.bundle");
        write_bundle(dir.path(), "expired.bundle", &key, &header, &payload);
        let locks = LockManager::new(dir.path().join("locks"));

        let error =
            install_bundle_from_file_with_trust(dir.path(), &ls_path, &source, &trust, &locks)
                .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_EXPIRED");
        assert_eq!(
            error.to_string(),
            format!(
                "Refusing registry {}: it expired at 2025-02-01T00:00:00+00:00. The cached registry has been kept.",
                source.display()
            )
        );
        assert!(!paths::registry_bundle_path(dir.path()).unwrap().exists());
    }

    /// A bundle built before the installed one is refused; there is deliberately
    /// no override, because the installed stamp is signed.
    #[test]
    fn an_older_local_bundle_is_refused_with_the_download_message() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(65);
        let trust = RegistryTrust::with_key(test_public(&key));
        let new_payload = sample_db();
        let new_header = fresh_header(&key, &new_payload);
        let new_source = dir.path().join("new.bundle");
        write_bundle(dir.path(), "new.bundle", &key, &new_header, &new_payload);
        let locks = LockManager::new(dir.path().join("locks"));
        install_bundle_from_file_with_trust(dir.path(), &ls_path, &new_source, &trust, &locks)
            .unwrap();

        let old_payload = sample_db_with_built_at("2026-08-26T03:19:02+00:00");
        let old_built_at = "2026-08-26T03:19:02+00:00";
        let old_header = header_for(
            &key,
            &old_payload,
            old_built_at,
            "2099-01-01T00:00:00+00:00",
        );
        let old_source = dir.path().join("old.bundle");
        write_bundle(dir.path(), "old.bundle", &key, &old_header, &old_payload);

        let error =
            install_bundle_from_file_with_trust(dir.path(), &ls_path, &old_source, &trust, &locks)
                .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
        assert_eq!(
            error.to_string(),
            format!(
                "Refusing registry {}: it was built at {old_built_at}, older than the registry already installed (2026-11-16T12:00:00+00:00). The cached registry has been kept.",
                old_source.display()
            )
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            signed_bundle(&key, &new_header, &new_payload)
        );
    }

    /// A locally supplied bundle with a schema newer than this app supports is
    /// refused before anything is installed.
    #[test]
    fn a_local_bundle_with_an_unsupported_schema_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(80);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let mut header = fresh_header(&key, &payload);
        header.schema_version = APP_REGISTRY_SCHEMA_VERSION + 1;
        let source = dir.path().join("future.bundle");
        write_bundle(dir.path(), "future.bundle", &key, &header, &payload);
        let locks = LockManager::new(dir.path().join("locks"));

        let error =
            install_bundle_from_file_with_trust(dir.path(), &ls_path, &source, &trust, &locks)
                .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_NEWER_SCHEMA");
        assert!(!paths::registry_bundle_path(dir.path()).unwrap().exists());
        assert!(!paths::registry_db_path(dir.path()).unwrap().exists());
    }

    /// A revocation remembered from an earlier local install keeps blocking a
    /// later local bundle signed by the retired key, even when that bundle does
    /// not carry the revocation itself.
    #[test]
    fn a_remembered_revocation_blocks_a_later_local_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let stolen = test_key(81);
        let recovery = test_key(82);
        let replacement = test_key(83);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let locks = LockManager::new(dir.path().join("locks"));
        let payload = sample_db();

        let mut revoking = header_for(
            &replacement,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        revoking.revocations.push(revocation(&recovery, &stolen));
        let revoking_source = dir.path().join("revoking.bundle");
        write_bundle(
            dir.path(),
            "revoking.bundle",
            &replacement,
            &revoking,
            &payload,
        );
        install_bundle_from_file_with_trust(dir.path(), &ls_path, &revoking_source, &trust, &locks)
            .unwrap();
        assert!(
            fs::read_to_string(paths::registry_trust_state_path(dir.path()).unwrap())
                .unwrap()
                .contains("v1 revoke")
        );

        let stale_header = header_for(
            &stolen,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let stale_source = dir.path().join("stale.bundle");
        write_bundle(dir.path(), "stale.bundle", &stolen, &stale_header, &payload);
        let error = install_bundle_from_file_with_trust(
            dir.path(),
            &ls_path,
            &stale_source,
            &trust,
            &locks,
        )
        .unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_SIGNATURE_INVALID");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            signed_bundle(&replacement, &revoking, &payload)
        );
    }

    /// A local install contends on the same cross-process `RegistryUpdate` lock
    /// sync holds. While that lock is held, the local install must not commit.
    #[test]
    fn a_local_install_takes_the_registry_update_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(66);
        let trust = RegistryTrust::with_key(test_public(&key));
        let locks = LockManager::new(dir.path().join("locks"));
        let _holder = locks
            .acquire(crate::lock_manager::LockResource::RegistryUpdate, "sync")
            .unwrap();
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        let source = dir.path().join("source.bundle");
        write_bundle(dir.path(), "source.bundle", &key, &header, &payload);

        let result = install_local_inner(
            dir.path(),
            &ls_path,
            &source,
            &trust,
            &locks,
            Some(Duration::from_millis(200)),
        );
        let error =
            result.expect_err("a local install must not commit while sync holds RegistryUpdate");
        assert_eq!(error.code(), "ERR_LOCK_TIMEOUT");
        assert!(!paths::registry_bundle_path(dir.path()).unwrap().exists());
    }

    /// A revocation learned from a candidate is not persisted until the bundle
    /// is committed. A failure before the commit leaves the old bundle current
    /// and trusted, and a retry works.
    #[test]
    fn a_failed_commit_does_not_persist_the_new_revocation() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(70);
        let recovery = test_key(71);
        let replacement = test_key(72);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let current_header = fresh_header(&stolen, &payload);
        let current = signed_bundle(&stolen, &current_header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();

        let mut candidate_header = header_for(
            &replacement,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        candidate_header
            .revocations
            .push(revocation(&recovery, &stolen));
        let candidate = signed_bundle(&replacement, &candidate_header, &payload);

        // Break the pre-commit backup so the bundle cannot be replaced.
        let prev = paths::registry_bundle_previous_path(dir.path()).unwrap();
        fs::create_dir_all(&prev).unwrap();
        let error = install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate")
            .expect_err("the commit must fail while the backup path is obstructed");
        assert_eq!(error.code(), "ERR_REGISTRY_WRITE");

        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        let remembered = fs::read_to_string(&trust_path).unwrap_or_default();
        assert!(
            !remembered.contains("v1 revoke"),
            "a failed commit must not leave the old bundle revoked: {remembered}"
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
        );
        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_ok(),
            "the previous bundle stays trusted"
        );

        // Retry once the obstruction is gone.
        fs::remove_dir(&prev).unwrap();
        install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate").unwrap();
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            candidate
        );
        assert!(fs::read_to_string(&trust_path)
            .unwrap()
            .contains("v1 revoke"));
    }

    /// Offsets are compared as UTC instants, not as text. 11:00-02:00 is 13:00
    /// UTC, so it is newer than a cached 12:00+00:00 even though it sorts lower.
    #[test]
    fn a_built_at_offset_is_compared_as_an_instant() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(76);
        let trust = RegistryTrust::with_key(test_public(&key));
        let current_payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let current_header = fresh_header(&key, &current_payload);
        let current = signed_bundle(&key, &current_header, &current_payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();

        let newer_payload = sample_db_with_built_at("2026-11-16T11:00:00-02:00");
        let mut newer_header = header_for(
            &key,
            &newer_payload,
            "2026-11-16T11:00:00-02:00",
            "2099-01-01T00:00:00+00:00",
        );
        newer_header.tag = "registry-2026-11-16".into();
        let newer = signed_bundle(&key, &newer_header, &newer_payload);

        install_bundle_bytes_checked(dir.path(), &newer, &trust, "newer")
            .expect("a later UTC instant must be accepted even if its text sorts earlier");
    }

    /// An equal signed build stamp with different signed bytes must not replace
    /// the installed catalog.
    #[test]
    fn an_equal_stamp_with_different_bytes_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(77);
        let trust = RegistryTrust::with_key(test_public(&key));
        let stamp = "2026-11-16T12:00:00+00:00";
        let current_payload = sample_db_with_built_at(stamp);
        let current_header = fresh_header(&key, &current_payload);
        let current = signed_bundle(&key, &current_header, &current_payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();

        let other_payload = sample_db_with_built_at("2026-11-17T12:00:00+00:00");
        let mut other_header = header_for(&key, &other_payload, stamp, "2099-01-01T00:00:00+00:00");
        other_header.tag = "registry-2026-11-16".into();
        let other = signed_bundle(&key, &other_header, &other_payload);

        let error = install_bundle_bytes_checked(dir.path(), &other, &trust, "other")
            .expect_err("an equal stamp over different bytes must be refused");
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
        );
    }

    /// A signed display tag must not make an explicit sync report "up to date".
    #[tokio::test]
    async fn a_future_signed_tag_cannot_suppress_an_explicit_sync() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(78);
        let trust = RegistryTrust::with_key(test_public(&key));
        let local_payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let mut local_header = fresh_header(&key, &local_payload);
        local_header.tag = "registry-2099-01-01".into();
        let local_bundle = signed_bundle(&key, &local_header, &local_payload);
        let source = dir.path().join("local.bundle");
        fs::write(&source, &local_bundle).unwrap();
        install_bundle_from_file_with_trust(dir.path(), &ls_path, &source, &trust, &locks).unwrap();
        assert_eq!(
            get_cached_tag(&ls_path).unwrap().as_deref(),
            Some("registry-2099-01-01")
        );

        let remote_payload = sample_db_with_built_at("2026-11-20T02:00:00+00:00");
        let mut remote_header = header_for(
            &key,
            &remote_payload,
            "2026-11-20T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        remote_header.tag = "registry-2026-11-20".into();
        let remote_bundle = signed_bundle(&key, &remote_header, &remote_payload);
        let recorded = r#"[
            {"tag_name": "registry-2026-11-20", "draft": false, "prerelease": false, "assets": [{"id": 5001, "name": "registry.bundle"}]}
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(5001, remote_bundle.clone());

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("explicit sync must check the remote, not trust the signed tag");

        assert!(status.update_available || status.message.contains("updated"));
        assert!(!status.message.contains("up to date"), "{}", status.message);
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            remote_bundle
        );
    }

    /// When neither the current nor the candidate bundle carries a trust
    /// statement, an unwritable trust-state path loses nothing, so the install
    /// still reports success. Revocations are covered by the next test.
    #[test]
    fn a_post_commit_write_failure_with_no_trust_statements_is_not_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(73);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let current_header = fresh_header(&key, &payload);
        let current = signed_bundle(&key, &current_header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();

        let candidate_payload = sample_db_with_built_at("2026-11-17T12:00:00+00:00");
        let candidate_header = header_for(
            &key,
            &candidate_payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let candidate = signed_bundle(&key, &candidate_header, &candidate_payload);

        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();
        install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate")
            .expect("no trust statement can be lost, so the install is still a success");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            candidate
        );
    }

    /// A bundle that carries a trust statement is not a clean install if the
    /// durable append fails: a lost revocation must surface as a failure.
    #[test]
    fn a_post_commit_trust_write_failure_with_statements_is_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(90);
        let recovery = test_key(91);
        let replacement = test_key(92);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let current_header = fresh_header(&stolen, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&stolen, &current_header, &payload),
        )
        .unwrap();

        let mut candidate_header = header_for(
            &replacement,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        candidate_header
            .revocations
            .push(revocation(&recovery, &stolen));
        let candidate = signed_bundle(&replacement, &candidate_header, &payload);

        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();
        let error = install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate")
            .expect_err("a lost revocation must not be reported as a clean install");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
    }

    /// Recovery from `.prev` is refused when its revocations cannot be
    /// accounted for durably: an untrusted catalog is better than one whose
    /// committed revocations were silently dropped.
    #[test]
    fn recovery_fails_closed_when_the_previous_bundles_trust_cannot_be_saved() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(93);
        let recovery = test_key(94);
        let replacement = test_key(95);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let mut previous_header = fresh_header(&replacement, &payload);
        previous_header
            .revocations
            .push(revocation(&recovery, &stolen));
        let previous = signed_bundle(&replacement, &previous_header, &payload);
        fs::write(
            paths::registry_bundle_previous_path(dir.path()).unwrap(),
            &previous,
        )
        .unwrap();
        let current = paths::registry_bundle_path(dir.path()).unwrap();
        fs::write(&current, b"damaged current bundle").unwrap();
        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            !matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "recovery must not promote a bundle whose revocation cannot be saved: {result:?}"
        );
        assert!(fs::read(&current)
            .map(|bytes| bytes != previous)
            .unwrap_or(true));
    }

    /// A failed trust write after a commit must not let a later bundle signed
    /// by the revoked key through. The next install reconciles the current
    /// bundle's revocations before it validates the replacement.
    #[test]
    fn a_failed_trust_write_cannot_revive_a_revoked_signer_later() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(84);
        let recovery = test_key(85);
        let replacement = test_key(86);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let first = fresh_header(&stolen, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&stolen, &first, &payload),
        )
        .unwrap();

        let mut revoking = header_for(
            &replacement,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        revoking.revocations.push(revocation(&recovery, &stolen));
        let revoking_bytes = signed_bundle(&replacement, &revoking, &payload);
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        fs::create_dir_all(&trust_path).unwrap();
        install_bundle_bytes_checked(dir.path(), &revoking_bytes, &trust, "revoking").unwrap_err();
        fs::remove_dir(&trust_path).unwrap();
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            revoking_bytes,
            "the replacement was committed even though its revocation was lost"
        );

        // A bundle signed by the retired key must now be refused: reconciliation
        // of the current bundle writes its revocation before validating this one.
        let stale_payload = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let stale_header = header_for(
            &stolen,
            &stale_payload,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let stale = signed_bundle(&stolen, &stale_header, &stale_payload);
        let error = install_bundle_bytes_checked(dir.path(), &stale, &trust, "stale")
            .expect_err("the revoked signer must not be accepted again");
        assert!(
            matches!(
                error.code().as_str(),
                "ERR_REGISTRY_SIGNATURE_INVALID" | "ERR_REGISTRY_TRUST_STATE"
            ),
            "unexpected refusal: {error}"
        );
        assert!(fs::read_to_string(&trust_path)
            .unwrap()
            .contains("v1 revoke"));
    }

    /// Reconciliation is not skipped just because the installed bundle is past
    /// its signed expiry: a committed revocation must still be accounted for.
    #[test]
    fn an_expired_current_bundle_still_reconciles_its_revocations() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(87);
        let recovery = test_key(88);
        let replacement = test_key(89);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let mut expired_revoking = header_for(
            &replacement,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2020-01-01T00:00:00+00:00",
        );
        expired_revoking
            .revocations
            .push(revocation(&recovery, &stolen));
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&replacement, &expired_revoking, &payload),
        )
        .unwrap();

        let newer_payload = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let newer = header_for(
            &stolen,
            &newer_payload,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&stolen, &newer, &newer_payload),
            &trust,
            "stale",
        )
        .expect_err("the expired current bundle's revocation must still be applied");
        assert_eq!(error.code(), "ERR_REGISTRY_SIGNATURE_INVALID");
        assert!(
            fs::read_to_string(paths::registry_trust_state_path(dir.path()).unwrap())
                .unwrap()
                .contains("v1 revoke")
        );
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
            read_cached_stamp_checked(dir.path(), &trust)
                .unwrap()
                .map(|stamp| stamp.built_at),
            Some(header.built_at)
        );

        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            b"corrupt current bundle",
        )
        .unwrap();
        let error = read_cached_stamp_checked(dir.path(), &trust).unwrap_err();
        assert_eq!(error.code(), "ERR_REGISTRY_CACHED_STAMP_UNREADABLE");
    }

    /// A new key introduced by a certificate signed by an old trusted key is
    /// accepted; the same bundle without the certificate is not.
    #[test]
    fn a_new_key_is_accepted_through_an_old_key_and_refused_without_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = test_key(8);
        let new = test_key(9);
        let payload = sample_db();
        let mut header = fresh_header(&new, &payload);
        header
            .certificates
            .push(certificate(&old, &test_public(&new)));
        let bundle = signed_bundle(&new, &header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &bundle).unwrap();

        let trust_old = RegistryTrust::with_roles([(RegistryKeyRole::Recovery, test_public(&old))]);
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

    fn certificate(recovery: &ed25519_dalek::SigningKey, new_public: &str) -> KeyCertificate {
        use ed25519_dalek::Signer;
        let signed_by = registry_bundle::key_id_for_public_key(&test_public(recovery));
        let new_id = registry_bundle::key_id_for_public_key(new_public);
        let message =
            KeyCertificate::signed_bytes(&new_id, new_public, RegistryKeyRole::Primary, &signed_by);
        KeyCertificate {
            key_id: new_id,
            public_key: new_public.to_string(),
            role: RegistryKeyRole::Primary,
            signed_by,
            signature: hex::encode(recovery.sign(&message).to_bytes()),
        }
    }

    fn revocation(
        recovery: &ed25519_dalek::SigningKey,
        revoked: &ed25519_dalek::SigningKey,
    ) -> KeyRevocation {
        use ed25519_dalek::Signer;
        let revoked_key_id = registry_bundle::key_id_for_public_key(&test_public(revoked));
        let signed_by = registry_bundle::key_id_for_public_key(&test_public(recovery));
        let message = KeyRevocation::signed_bytes(&revoked_key_id, &signed_by);
        KeyRevocation {
            revoked_key_id,
            signed_by,
            signature: hex::encode(recovery.sign(&message).to_bytes()),
        }
    }

    /// A revocation learned from one verified bundle is remembered, so a later
    /// bundle signed by the stolen key cannot simply leave it out.
    #[test]
    fn a_revocation_survives_into_a_later_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(20);
        let primary = test_key(21);
        let recovery = test_key(28);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&primary)),
        ]);
        let payload = sample_db();

        let mut revoking = fresh_header(&primary, &payload);
        revoking.revocations.push(revocation(&recovery, &stolen));
        let revoking_bundle = signed_bundle(&primary, &revoking, &payload);
        install_bundle_bytes(dir.path(), &revoking, &revoking_bundle, &trust).unwrap();

        let stale = fresh_header(&stolen, &payload);
        let stale_bundle = signed_bundle(&stolen, &stale, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            stale_bundle,
        )
        .unwrap();

        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_err(),
            "a bundle signed by the revoked key must be refused even without its own revocation list"
        );
    }

    /// A key introduced by a certificate is remembered, so a later bundle signed
    /// by it needs no certificate of its own.
    #[test]
    fn an_introduced_key_is_remembered_without_its_certificate() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = test_key(22);
        let new = test_key(23);
        let payload = sample_db();
        let old_trust = RegistryTrust::with_roles([(RegistryKeyRole::Recovery, test_public(&old))]);

        let mut introduced = fresh_header(&new, &payload);
        introduced
            .certificates
            .push(certificate(&old, &test_public(&new)));
        let first = signed_bundle(&new, &introduced, &payload);
        install_bundle_bytes(dir.path(), &introduced, &first, &old_trust).unwrap();

        let later = fresh_header(&new, &payload);
        let later_bundle = signed_bundle(&new, &later, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            later_bundle,
        )
        .unwrap();

        assert!(
            open_verified_registry_with_trust(dir.path(), &old_trust).is_ok(),
            "the introduced key should stay trusted without carrying its certificate again"
        );
    }

    /// A garbage state file is not a new trust root: a key may only be remembered
    /// through a certificate signed by a key the client already trusts.
    #[test]
    fn a_garbage_state_file_never_widens_trust() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = test_key(24);
        let attacker = test_key(25);
        let payload = sample_db();
        let trust = RegistryTrust::with_roles([(RegistryKeyRole::Recovery, test_public(&old))]);
        let mut forged = fresh_header(&attacker, &payload);
        forged
            .certificates
            .push(certificate(&attacker, &test_public(&attacker)));
        let forged_bundle = signed_bundle(&attacker, &forged, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            forged_bundle,
        )
        .unwrap();

        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            "this is not a trust state
v1 key deadbeef deadbeef deadbeef deadbeef
",
        )
        .unwrap();

        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_err(),
            "a forged certificate signed by an unknown key must not introduce the attacker's key"
        );
    }

    /// A truncated final line loses only that line; complete lines before it
    /// still count.
    #[test]
    fn a_truncated_state_file_keeps_the_complete_revocations_it_can_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(26);
        let stolen = test_key(27);
        let recovery = test_key(29);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&primary)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
        ]);
        let statement = revocation(&recovery, &stolen);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 revoke {} {} {}\nv1 rev",
                statement.revoked_key_id, statement.signed_by, statement.signature
            ),
        )
        .unwrap();

        let payload = sample_db();
        let stale = fresh_header(&stolen, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&stolen, &stale, &payload),
        )
        .unwrap();

        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_err(),
            "the complete revocation line before the truncation still applies"
        );
    }

    /// A forged revocation line cannot remove a key: the signer is not a trusted
    /// recovery key, so load ignores the line.
    #[test]
    fn a_forged_revocation_line_is_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(30);
        let recovery = test_key(31);
        let attacker = test_key(32);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&primary)),
        ]);
        let forged = revocation(&attacker, &primary);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 revoke {} {} {}\n",
                forged.revoked_key_id, forged.signed_by, forged.signature
            ),
        )
        .unwrap();

        let payload = sample_db();
        let header = fresh_header(&primary, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&primary, &header, &payload),
        )
        .unwrap();
        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_ok(),
            "a revocation not signed by a recovery key must not remove the primary"
        );
    }

    /// A primary-signed revocation is ignored and never remembered.
    #[test]
    fn a_primary_signed_revocation_is_not_remembered() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(33);
        let recovery = test_key(34);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&primary)),
        ]);
        let payload = sample_db();
        let mut header = fresh_header(&primary, &payload);
        // The primary signs the revocation message itself. The signature is
        // valid, but the role forbids it.
        header.revocations.push(revocation(&primary, &recovery));
        let bundle = signed_bundle(&primary, &header, &payload);
        install_bundle_bytes(dir.path(), &header, &bundle, &trust).unwrap();

        let state = fs::read_to_string(paths::registry_trust_state_path(dir.path()).unwrap())
            .unwrap_or_default();
        assert!(
            !state.lines().any(|line| line.starts_with("v1 revoke ")),
            "an ignored revocation must not be written to the state: {state}"
        );

        // The recovery key can still certify the next primary.
        let new = test_key(35);
        let mut next = fresh_header(&new, &payload);
        next.certificates
            .push(certificate(&recovery, &test_public(&new)));
        let next_bundle = signed_bundle(&new, &next, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            &next_bundle,
        )
        .unwrap();
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
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

    /// A pre-bundle cache is not a catalog: no signed expiry, no key id, no
    /// rotation. The client ignores it and the next sync downloads a bundle.
    #[test]
    fn a_legacy_pair_is_no_catalog() {
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
            CachedRegistryIntegrity::Absent
        );
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_err());
    }

    #[test]
    fn test_github_release_find_asset() {
        let release = GitHubRelease {
            tag_name: "v1.0.0".to_string(),
            draft: false,
            prerelease: false,
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

    struct FakeRegistryTransport {
        pages: HashMap<u32, Vec<GitHubRelease>>,
        assets: Mutex<HashMap<u64, std::collections::VecDeque<LauncherResult<Vec<u8>>>>>,
        pages_requested: Mutex<Vec<u32>>,
    }

    impl FakeRegistryTransport {
        fn from_pages(pages: Vec<(u32, Vec<GitHubRelease>)>) -> Self {
            Self {
                pages: pages.into_iter().collect(),
                assets: Mutex::new(HashMap::new()),
                pages_requested: Mutex::new(Vec::new()),
            }
        }

        fn push_asset_ok(&self, asset_id: u64, bytes: Vec<u8>) {
            self.assets
                .lock()
                .unwrap()
                .entry(asset_id)
                .or_default()
                .push_back(Ok(bytes));
        }

        fn push_asset_err(&self, asset_id: u64, err: LauncherError) {
            self.assets
                .lock()
                .unwrap()
                .entry(asset_id)
                .or_default()
                .push_back(Err(err));
        }
    }

    #[async_trait::async_trait]
    impl RegistryTransport for FakeRegistryTransport {
        async fn releases_page(
            &self,
            _repo: &str,
            page: u32,
            _token: Option<&str>,
        ) -> LauncherResult<Vec<GitHubRelease>> {
            self.pages_requested.lock().unwrap().push(page);
            Ok(self.pages.get(&page).cloned().unwrap_or_default())
        }

        async fn download_asset(&self, url: &str, _token: Option<&str>) -> LauncherResult<Vec<u8>> {
            let asset_id: u64 = url
                .rsplit('/')
                .next()
                .and_then(|s| s.parse().ok())
                .expect("asset url must end with numeric id");
            let mut assets = self.assets.lock().unwrap();
            let queue = assets
                .get_mut(&asset_id)
                .expect("unexpected asset download");
            queue.pop_front().expect("no queued response for asset")
        }
    }

    fn setup_sync_env() -> (tempfile::TempDir, PathBuf, LockManager) {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = dir.path().join("local_state.db");
        db::init_local_state_db(&ls_path).unwrap();
        let locks = LockManager::new(dir.path().join("locks"));
        (dir, ls_path, locks)
    }

    fn release_json_to_list(raw: &str) -> Vec<GitHubRelease> {
        serde_json::from_str(raw).unwrap()
    }

    #[tokio::test]
    async fn catalog_sync_selects_catalog_release_past_desktop_and_plugin_releases() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(40);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-18T02:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-18T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let bundle = signed_bundle(&key, &header, &payload);

        // Recorded GitHub /releases payload: newest is desktop v0.9.5, then official plugin, then catalog.
        let recorded = r#"[
            {
                "tag_name": "v0.9.5",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {"id": 1001, "name": "agora-desktop-v0.9.5-windows-x64-setup.exe"},
                    {"id": 1002, "name": "latest.json"}
                ]
            },
            {
                "tag_name": "official-skyrim-se-v0.1.0",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {"id": 2001, "name": "agora.skyrim-se-0.1.0.zip"}
                ]
            },
            {
                "tag_name": "registry-2026-11-18",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {"id": 3001, "name": "registry.bundle"},
                    {"id": 3002, "name": "registry.db"}
                ]
            }
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(3001, bundle);

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("catalog sync should skip desktop and plugin releases and install catalog");

        assert!(status.has_cached_db);
        assert_eq!(status.cached_tag.as_deref(), Some("registry-2026-11-18"));
        assert_eq!(status.latest_tag.as_deref(), Some("registry-2026-11-18"));
    }

    #[tokio::test]
    async fn catalog_sync_with_cached_db_does_not_say_up_to_date_when_only_desktop_and_plugin_exist(
    ) {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(41);
        let trust = RegistryTrust::with_key(test_public(&key));
        let old_payload = sample_db_with_built_at("2026-11-17T02:00:00+00:00");
        let old_header = header_for(
            &key,
            &old_payload,
            "2026-11-17T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let old_bundle = signed_bundle(&key, &old_header, &old_payload);
        install_bundle_bytes(dir.path(), &old_header, &old_bundle, &trust).unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-17").unwrap();
        set_cached_asset_id(&ls_path, Some(2999)).unwrap();

        let recorded = r#"[
            {"tag_name": "v0.9.5", "draft": false, "prerelease": false, "assets": [{"id": 1001, "name": "latest.json"}]},
            {"tag_name": "official-skyrim-se-v0.1.0", "draft": false, "prerelease": false, "assets": [{"id": 2001, "name": "agora.skyrim-se-0.1.0.zip"}]}
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("cached catalog stays readable when no remote catalog release qualifies");

        assert!(!status.checked);
        assert!(
            status
                .message
                .to_ascii_lowercase()
                .contains("no catalog release found"),
            "expected 'no catalog release found' message, got: {}",
            status.message
        );
        assert!(
            !status.message.to_ascii_lowercase().contains("up to date"),
            "must never say 'up to date' when no catalog release qualifies: {}",
            status.message
        );
    }

    #[tokio::test]
    async fn catalog_sync_pages_past_first_page_of_non_catalog_releases() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(42);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-18T02:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-18T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let bundle = signed_bundle(&key, &header, &payload);

        let page1: Vec<GitHubRelease> = (0..RELEASES_PER_PAGE)
            .map(|i| GitHubRelease {
                tag_name: format!("official-plugin-v0.1.{i}"),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 10_000 + i as u64,
                    name: "plugin.zip".into(),
                }],
            })
            .collect();
        let page2 = vec![GitHubRelease {
            tag_name: "registry-2026-11-18".into(),
            draft: false,
            prerelease: false,
            assets: vec![GitHubAsset {
                id: 4001,
                name: "registry.bundle".into(),
            }],
        }];
        let transport = FakeRegistryTransport::from_pages(vec![(1, page1), (2, page2)]);
        transport.push_asset_ok(4001, bundle);

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("should page past page 1 to find catalog release on page 2");

        assert_eq!(status.cached_tag.as_deref(), Some("registry-2026-11-18"));
        assert_eq!(*transport.pages_requested.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn catalog_sync_ignores_drafts_prereleases_and_bundleless_catalog_releases() {
        let releases = vec![
            GitHubRelease {
                tag_name: "registry-2026-11-20".into(),
                draft: true,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 1,
                    name: "registry.bundle".into(),
                }],
            },
            GitHubRelease {
                tag_name: "registry-2026-11-19".into(),
                draft: false,
                prerelease: true,
                assets: vec![GitHubAsset {
                    id: 2,
                    name: "registry.bundle".into(),
                }],
            },
            GitHubRelease {
                tag_name: "registry-2026-11-18".into(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 3,
                    name: "registry.db".into(),
                }],
            },
            GitHubRelease {
                tag_name: "registry-2026-11-16".into(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 4,
                    name: "registry.bundle".into(),
                }],
            },
            GitHubRelease {
                tag_name: "registry-2026-11-17".into(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 5,
                    name: "registry.bundle".into(),
                }],
            },
        ];
        let picked = select_latest_catalog_release(&releases).expect("should pick 2026-11-17");
        assert_eq!(picked.tag_name, "registry-2026-11-17");
    }

    #[tokio::test]
    async fn catalog_sync_caps_paging_and_reports_no_catalog_release_found() {
        let (dir, ls_path, locks) = setup_sync_env();
        let trust = RegistryTrust::with_key(test_public(&test_key(44)));
        let non_catalog_page: Vec<GitHubRelease> = (0..RELEASES_PER_PAGE)
            .map(|i| GitHubRelease {
                tag_name: format!("official-plugin-v0.1.{i}"),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: i as u64,
                    name: "plugin.zip".into(),
                }],
            })
            .collect();
        // 11 pages of non-catalog releases, catalog only on page 11 (beyond MAX_RELEASE_PAGES=10).
        let mut pages: Vec<(u32, Vec<GitHubRelease>)> =
            (1..=10).map(|p| (p, non_catalog_page.clone())).collect();
        pages.push((
            11,
            vec![GitHubRelease {
                tag_name: "registry-2026-11-18".into(),
                draft: false,
                prerelease: false,
                assets: vec![GitHubAsset {
                    id: 9999,
                    name: "registry.bundle".into(),
                }],
            }],
        ));
        let transport = FakeRegistryTransport::from_pages(pages);

        let err = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect_err("should stop at page cap and report no catalog release found");

        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("no catalog release found"),
            "unexpected error: {err}"
        );
        assert_eq!(
            *transport.pages_requested.lock().unwrap(),
            (1..=MAX_RELEASE_PAGES).collect::<Vec<u32>>()
        );
    }

    #[tokio::test]
    async fn failed_catalog_download_retries_sooner_and_never_claims_up_to_date() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(43);
        let trust = RegistryTrust::with_key(test_public(&key));

        let old_payload = sample_db_with_built_at("2026-11-17T02:00:00+00:00");
        let old_header = header_for(
            &key,
            &old_payload,
            "2026-11-17T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let old_bundle = signed_bundle(&key, &old_header, &old_payload);
        install_bundle_bytes(dir.path(), &old_header, &old_bundle, &trust).unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-17").unwrap();
        set_cached_asset_id(&ls_path, Some(2999)).unwrap();

        let new_payload = sample_db_with_built_at("2026-11-18T02:00:00+00:00");
        let new_header = header_for(
            &key,
            &new_payload,
            "2026-11-18T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let new_bundle = signed_bundle(&key, &new_header, &new_payload);

        let catalog_release = GitHubRelease {
            tag_name: "registry-2026-11-18".into(),
            draft: false,
            prerelease: false,
            assets: vec![GitHubAsset {
                id: 5001,
                name: "registry.bundle".into(),
            }],
        };
        let transport = FakeRegistryTransport::from_pages(vec![(1, vec![catalog_release])]);
        // First download fails, second returns corrupt bytes, third succeeds.
        transport.push_asset_err(5001, LauncherError::RegistryDownloadFailed);
        transport.push_asset_ok(5001, b"corrupt-bundle".to_vec());
        transport.push_asset_ok(5001, new_bundle);

        let err = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect_err("first download fails");
        assert!(matches!(err, LauncherError::RegistryDownloadFailed));
        assert_eq!(get_last_check_time(&ls_path).unwrap(), Some(1_000_000));
        assert_eq!(get_last_success_time(&ls_path).unwrap(), None);

        let offline_status = get_status_with_trust(dir.path(), &ls_path, &trust);
        assert!(offline_status.has_cached_db);
        assert!(offline_status.update_available);
        assert_eq!(
            offline_status.latest_tag.as_deref(),
            Some("registry-2026-11-18")
        );
        assert!(!offline_status
            .message
            .to_ascii_lowercase()
            .contains("up to date"));

        // 1 minute later (within retry cooldown): must NOT claim "Catalog is up to date."
        let immediate_next = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_060,
        )
        .await
        .expect("cooldown check returns status");
        assert!(
            !immediate_next
                .message
                .to_ascii_lowercase()
                .contains("up to date"),
            "must never claim 'up to date' while a known newer catalog failed to install: {}",
            immediate_next.message
        );
        assert!(immediate_next.update_available);
        assert_eq!(
            immediate_next.latest_tag.as_deref(),
            Some("registry-2026-11-18")
        );

        // 6 minutes later (well under 1 hour, past the 5-minute retry window):
        // retries automatically; second attempt fails verification.
        let verify_err = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_360,
        )
        .await
        .expect_err("corrupt bundle must fail verification");
        assert_eq!(verify_err.code(), "ERR_REGISTRY_BUNDLE_INVALID");

        // 12 minutes later (still under 1 hour, past another 5-minute retry window):
        // retries again and succeeds!
        let retried = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_720,
        )
        .await
        .expect("should retry after shorter failure interval and succeed");
        assert_eq!(retried.cached_tag.as_deref(), Some("registry-2026-11-18"));
        assert!(!retried.update_available);
        assert!(retried.checked);

        // 1 minute after the successful update: normal 1-hour throttle applies.
        let after_success = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            false,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_780,
        )
        .await
        .expect("post-success check should throttle cleanly");
        assert!(!after_success.checked);
        assert!(!after_success.update_available);
        assert_eq!(after_success.message, "Catalog is up to date.");
    }

    /// A release tag published just after UTC midnight is a day ahead of the
    /// signed build stamp. Once that asset id is recorded, sync must not fetch
    /// the same bytes again just because the tag date looks newer.
    #[tokio::test]
    async fn a_tag_ahead_of_the_signed_stamp_does_not_redownload_the_same_asset() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(96);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-17T23:30:00+00:00");
        let mut header = header_for(
            &key,
            &payload,
            "2026-11-17T23:30:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        header.tag = "registry-2026-11-17".into();
        let bundle = signed_bundle(&key, &header, &payload);
        install_bundle_bytes(dir.path(), &header, &bundle, &trust).unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-17").unwrap();
        set_cached_asset_id(&ls_path, Some(6001)).unwrap();

        let recorded = r#"[
            {"tag_name": "registry-2026-11-18", "draft": false, "prerelease": false, "assets": [{"id": 6001, "name": "registry.bundle"}]}
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(6001, bundle.clone());

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("sync should not redownload the asset it already has");
        assert!(!status.update_available);
        assert!(status.message.contains("up to date"), "{}", status.message);
        assert_eq!(
            transport.assets.lock().unwrap()[&6001].len(),
            1,
            "the identical asset must not be fetched again"
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            bundle
        );
    }

    /// A candidate with the same signed stamp and the same bytes is "up to
    /// date": record its asset id rather than reinstalling and claiming an
    /// update (or refusing it as an equal-stamp conflict).
    #[tokio::test]
    async fn an_identical_candidate_records_its_asset_id_and_reports_up_to_date() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(97);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-17T23:30:00+00:00");
        let mut header = header_for(
            &key,
            &payload,
            "2026-11-17T23:30:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        header.tag = "registry-2099-01-01".into();
        let bundle = signed_bundle(&key, &header, &payload);
        install_bundle_bytes(dir.path(), &header, &bundle, &trust).unwrap();
        set_cached_tag(&ls_path, "registry-2099-01-01").unwrap();
        // A local install clears the online asset id; the next sync learns it.
        set_cached_asset_id(&ls_path, None).unwrap();

        let recorded = r#"[
            {"tag_name": "registry-2026-11-18", "draft": false, "prerelease": false, "assets": [{"id": 6002, "name": "registry.bundle"}]}
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(6002, bundle.clone());

        let status = check_and_download_update_inner(
            dir.path(),
            &ls_path,
            true,
            None,
            None,
            "agora-mc/Agora-Mod-Manager",
            &locks,
            &transport,
            &trust,
            1_000_000,
        )
        .await
        .expect("an identical candidate must not be a failure");
        assert_eq!(get_cached_asset_id(&ls_path).unwrap(), Some(6002));
        assert!(status.message.contains("up to date"), "{}", status.message);
        assert_eq!(
            get_cached_tag(&ls_path).unwrap().as_deref(),
            Some("registry-2099-01-01")
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            bundle
        );
    }
}
