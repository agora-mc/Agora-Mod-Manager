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
    self, BundleError, CatalogHeader, KeyCertificate, KeyRevocation, RegistryKeyRole,
    RegistryTrust, VerifiedBundle,
};
use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
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
    let trust = RegistryTrust::production();
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
    let trust = RegistryTrust::production();
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
    compiled: &RegistryTrust,
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
    // This snapshot only describes the cached catalog for the early status
    // returns below. Nothing is decided with it: another process may change the
    // log before we hold the lock, so the decisions use a snapshot taken under it.
    let display_trust = &TrustSnapshot::load(app_data_dir, compiled)?;
    let cached_tag = get_cached_tag(local_state_path)?;
    let cached = cached_catalog_summary_with_trust(app_data_dir, display_trust);
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
    // The one snapshot for every decision in this check: loaded only now that
    // the lock keeps other writers of the log and the bundle out. It is loaded
    // again only if this check itself installs something.
    let snapshot = TrustSnapshot::load(app_data_dir, compiled)?;
    let trust = &snapshot;

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
    let cached_release_date = rollback_baseline(app_data_dir, trust)
        .ok()
        .flatten()
        .as_ref()
        .and_then(|stamp| cached_release_date_from_stamp(Some(stamp)));
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

        let installed = install_bundle_bytes_checked_with(
            app_data_dir,
            &bundle_bytes,
            trust,
            &latest.tag_name,
        )?;
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

    // An identical candidate skips extraction, and extraction can fail even
    // after a real install. Report the actual readable-database state.
    let after;
    let trust = if changed {
        after = TrustSnapshot::load(app_data_dir, compiled)?;
        &after
    } else {
        trust
    };
    let ready = cached_catalog_summary_with_trust(app_data_dir, trust).is_some();
    Ok(RegistryStatus {
        has_cached_db: ready,
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
    trust: &TrustSnapshot,
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
    registry_bundle::verify_bundle(&bundle, trust.trust()).ok()?;
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
/// v1 record
/// v1 key <key_id> <public_key> <role> <signed_by> <signature>
/// v1 revoke <revoked_key_id> <signed_by> <signature>
/// ```
///
/// `v1 record` starts the statements one verified bundle taught; statements
/// before the first marker (a log written before markers) form one record. Every
/// other line is a signed statement: a key line is a certificate signed by a
/// recovery key, a revocation line a revocation signed by a recovery key. On
/// load each record is re-verified against the trust standing before it; a
/// statement that does not authenticate makes the log an error.
#[derive(Debug, Default)]
struct RememberedTrust {
    certificates: Vec<KeyCertificate>,
    revocations: Vec<KeyRevocation>,
    /// The log exactly as it was read, so a writer can refuse to build on a
    /// log that changed underneath it.
    log_text: String,
}

fn parse_role(raw: &str) -> Option<RegistryKeyRole> {
    match raw {
        "primary" => Some(RegistryKeyRole::Primary),
        "recovery" => Some(RegistryKeyRole::Recovery),
        _ => None,
    }
}

fn trust_state_error(path: &Path, what: &str) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_REGISTRY_TRUST_STATE".into(),
        message: format!(
            "The remembered catalog trust state {what}. The catalog will not be opened on a \
             weaker set of keys; to forget what was remembered and trust only the keys built \
             into Agora, remove `{}` yourself.",
            path.display()
        ),
    }
}

/// Read and authenticate the trust log.
///
/// One rule for every line: a statement counts when its signature chain
/// authenticates from the base keys, regardless of line order and regardless of
/// whether a later statement revokes its signer. Enforcement is separate:
/// every authenticated revocation is in force, and a key survives only if
/// nothing in its certificate chain is revoked. A missing file is "nothing
/// remembered yet". A file that cannot be read, or a statement that does not
/// authenticate, is an error: only a torn final line (no trailing newline) that
/// does not parse or authenticate is dropped.
/// The statements of one record of the log: what one verified bundle taught.
/// A log written before records existed is one implicit record.
#[derive(Default)]
struct LogRecord {
    certificates: Vec<KeyCertificate>,
    revocations: Vec<KeyRevocation>,
}

enum LogStatement {
    Certificate(KeyCertificate),
    Revocation(KeyRevocation),
}

/// One `v1 key` / `v1 revoke` line, or `None` when the line is not one.
fn parse_statement(line: &str) -> Option<LogStatement> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    match fields.as_slice() {
        ["v1", "key", id, public_key, role, signed_by, signature] => {
            Some(LogStatement::Certificate(KeyCertificate {
                key_id: (*id).to_string(),
                public_key: (*public_key).to_string(),
                role: parse_role(role)?,
                signed_by: (*signed_by).to_string(),
                signature: (*signature).to_string(),
            }))
        }
        ["v1", "revoke", revoked_id, signed_by, signature] => {
            Some(LogStatement::Revocation(KeyRevocation {
                revoked_key_id: (*revoked_id).to_string(),
                signed_by: (*signed_by).to_string(),
                signature: (*signature).to_string(),
            }))
        }
        _ => None,
    }
}

/// Split the log into records. The log is only ever replaced whole (see
/// [`remember_bundle_trust`]), so it is always a sequence of complete records:
///
/// ```text
/// v1 record <n>      followed by exactly n statement lines
/// ```
///
/// Lines before the first `v1 record` are a log from before records existed and
/// form one implicit record (other junk there is ignored, as it always was).
/// Anything else, including a missing final newline or a record that is cut
/// short, is damage, never a smaller record.
fn parse_log(text: &str, path: &Path) -> LauncherResult<Vec<LogRecord>> {
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(trust_state_error(
            path,
            "ends in the middle of a line, so it is damaged",
        ));
    }
    let mut records: Vec<LogRecord> = vec![LogRecord::default()];
    let mut seen_marker = false;
    let mut remaining = 0usize;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let fields: Vec<&str> = line.split_whitespace().collect();
        if remaining > 0 {
            match parse_statement(line) {
                Some(LogStatement::Certificate(certificate)) => records
                    .last_mut()
                    .expect("there is always a record")
                    .certificates
                    .push(certificate),
                Some(LogStatement::Revocation(revocation)) => records
                    .last_mut()
                    .expect("there is always a record")
                    .revocations
                    .push(revocation),
                None => {
                    return Err(trust_state_error(
                        path,
                        &format!("has a record cut short before line {number}"),
                    ))
                }
            }
            remaining -= 1;
            continue;
        }
        match fields.as_slice() {
            ["v1", "record", count] => {
                let count = count.parse::<usize>().ok().filter(|count| *count > 0);
                let Some(count) = count else {
                    return Err(trust_state_error(
                        path,
                        &format!("has a record header it cannot read on line {number}"),
                    ));
                };
                seen_marker = true;
                remaining = count;
                records.push(LogRecord::default());
            }
            ["v1", "record", ..] => {
                return Err(trust_state_error(
                    path,
                    &format!("has a record header it cannot read on line {number}"),
                ))
            }
            ["v1", "key" | "revoke", ..] if !seen_marker => match parse_statement(line) {
                Some(LogStatement::Certificate(certificate)) => {
                    records[0].certificates.push(certificate)
                }
                Some(LogStatement::Revocation(revocation)) => {
                    records[0].revocations.push(revocation)
                }
                None => {
                    return Err(trust_state_error(
                        path,
                        &format!("has a malformed statement on line {number}"),
                    ))
                }
            },
            // Junk or another version before any record: ignored, as before.
            _ if !seen_marker => {}
            _ => {
                return Err(trust_state_error(
                    path,
                    &format!("has a line outside any record on line {number}"),
                ))
            }
        }
    }
    if remaining > 0 {
        return Err(trust_state_error(
            path,
            "ends with a record cut short, so it is damaged",
        ));
    }
    Ok(records)
}

/// Read and authenticate the trust log.
///
/// One rule for every statement: within a record (one verified bundle) it counts
/// when its signature chain authenticates against the trust standing *before*
/// that record, whatever the order of its lines and whether or not the same
/// record revokes its signer. Records replay in order, so a key retired by an
/// earlier record cannot authenticate anything in a later one. After each
/// record every authenticated revocation is in force, and a key survives only
/// if nothing in its certificate chain is revoked. Replay always starts from the
/// `base` keys the caller passes, which must never be an already merged set.
///
/// A missing or empty file is "nothing remembered yet". A file that cannot be
/// read, is damaged, or holds a statement that does not authenticate is an
/// error.
fn load_remembered_trust(
    app_data_dir: &Path,
    base: &RegistryTrust,
) -> LauncherResult<RememberedTrust> {
    let path =
        paths::registry_trust_state_path(app_data_dir).map_err(|_| LauncherError::Generic {
            code: "ERR_REGISTRY_TRUST_STATE".into(),
            message: "Could not resolve the catalog trust state path.".into(),
        })?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RememberedTrust::default())
        }
        Err(error) => {
            return Err(trust_state_error(
                &path,
                &format!("could not be read ({error})"),
            ))
        }
    };
    let records = parse_log(&text, &path)?;

    let mut all_certificates: Vec<KeyCertificate> = Vec::new();
    let mut revocations: Vec<KeyRevocation> = Vec::new();
    let mut standing = base.clone();
    for record in &records {
        let authenticated = registry_bundle::authenticate_statements(
            &standing,
            &record.certificates,
            &record.revocations,
        );
        let standing_revoked: BTreeSet<String> = standing.revoked_ids().into_iter().collect();

        for certificate in &record.certificates {
            let explained = authenticated.certificates.contains(certificate)
                || standing_revoked.contains(&certificate.key_id)
                || standing.certificates().contains(certificate)
                || standing
                    .keys()
                    .iter()
                    .any(|key| key.id == certificate.key_id);
            if !explained {
                return Err(trust_state_error(
                    &path,
                    &format!(
                        "holds a key statement for {} that cannot be authenticated",
                        certificate.key_id
                    ),
                ));
            }
        }
        for revocation in &record.revocations {
            let explained = authenticated.revocations.contains(revocation)
                || standing_revoked.contains(&revocation.revoked_key_id);
            if !explained {
                return Err(trust_state_error(
                    &path,
                    &format!(
                        "holds a revocation of {} that cannot be authenticated",
                        revocation.revoked_key_id
                    ),
                ));
            }
        }

        all_certificates.extend(authenticated.certificates);
        for revocation in authenticated.revocations {
            if !revocations.contains(&revocation) {
                revocations.push(revocation);
            }
        }
        // The next record is judged against everything learned so far. Chains
        // through revoked keys fall away inside the trust itself.
        standing = base.clone();
        standing.merge_remembered(all_certificates.clone(), revocations.clone());
    }

    Ok(RememberedTrust {
        certificates: all_certificates,
        revocations,
        log_text: text,
    })
}

/// The trust in force for one operation: the compiled (or test) keys plus the
/// remembered log, computed once by [`TrustSnapshot::load`] and used for every
/// decision in that operation. Internal helpers take a snapshot, never a bare
/// [`RegistryTrust`], so none of them can re-read the log, replay a merged set,
/// or turn a log error into "nothing remembered".
struct TrustSnapshot {
    trust: RegistryTrust,
    /// The log as it was read, so the writer refuses to build on a changed one.
    log_text: String,
}

impl TrustSnapshot {
    /// `compiled` is the compiled (or test) key set: roots only, never the
    /// result of an earlier snapshot. An unreadable, damaged or
    /// unauthenticatable log is an error, never a quiet fall back to the
    /// compiled keys.
    fn load(app_data_dir: &Path, compiled: &RegistryTrust) -> LauncherResult<Self> {
        let remembered = load_remembered_trust(app_data_dir, compiled)?;
        let mut trust = compiled.clone();
        trust.merge_remembered(remembered.certificates, remembered.revocations);
        Ok(Self {
            trust,
            log_text: remembered.log_text,
        })
    }

    fn trust(&self) -> &RegistryTrust {
        &self.trust
    }
}

/// The durable rollback floor: the highest signed build stamp ever committed,
/// with the payload hash of the bundle that carried it. It lives beside the
/// remembered trust state and is raised only after a successful commit, so a
/// bundle that cannot be verified still cannot be used to argue that an older
/// signed bundle is newer.
fn registry_floor_path(app_data_dir: &Path) -> LauncherResult<PathBuf> {
    Ok(app_data_dir.join("registry.floor"))
}

fn floor_error(message: impl Into<String>) -> LauncherError {
    LauncherError::Generic {
        code: "ERR_REGISTRY_FLOOR".into(),
        message: message.into(),
    }
}

/// Read the durable floor. A missing file is an unset floor; an unreadable or
/// malformed one is an error and must never be mistaken for "no floor", because
/// that would drop rollback protection.
fn read_floor_file(app_data_dir: &Path) -> LauncherResult<Option<CachedStamp>> {
    let path = registry_floor_path(app_data_dir)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(floor_error(format!(
                "Could not read the catalog rollback floor: {error}."
            )))
        }
    };
    let mut fields = text.split_whitespace();
    let built_at = fields
        .next()
        .ok_or_else(|| floor_error("The catalog rollback floor is empty."))?
        .to_string();
    let db_sha256 = fields
        .next()
        .ok_or_else(|| floor_error("The catalog rollback floor has no payload hash."))?
        .to_string();
    if parse_utc(&built_at).is_none() || db_sha256.len() != 64 {
        return Err(floor_error("The catalog rollback floor is malformed."));
    }
    Ok(Some(CachedStamp {
        built_at,
        db_sha256,
    }))
}

/// Before any trust statement is appended, capture the verified baseline of a
/// pre-floor cache. Otherwise the append can revoke the installed bundle's
/// signer, a failed swap leaves that bundle unverifiable, and the effective
/// floor is lost with it. The floor is durable before the trust change.
fn ensure_floor_baseline(app_data_dir: &Path, trust: &TrustSnapshot) -> LauncherResult<()> {
    // Reading first also fails closed on a malformed or unreadable floor.
    let _ = read_floor_file(app_data_dir)?;
    // Checkpoint the maximum, not just an unset floor: a previous post-swap
    // write may have failed and left the stored floor lagging the installed
    // bundle. `raise_floor` keeps the higher of the two.
    if let Some(stamp) = read_cached_stamp(app_data_dir, trust) {
        raise_floor(app_data_dir, &stamp.built_at, &stamp.db_sha256)?;
    }
    Ok(())
}

/// The effective rollback floor. A client with a pre-floor trust state takes it
/// from the installed bundle when that bundle still verifies; otherwise the
/// floor is unset and candidates are judged on their own signed stamps.
fn durable_floor(
    app_data_dir: &Path,
    trust: &TrustSnapshot,
) -> LauncherResult<Option<CachedStamp>> {
    // An unreadable or malformed floor is an error everywhere: treating it as
    // unset would drop rollback protection on installs, opens and recovery.
    if let Some(floor) = read_floor_file(app_data_dir)? {
        return Ok(Some(floor));
    }
    // Only a genuinely absent file migrates from the installed bundle.
    let bundle_path = match paths::registry_bundle_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    if !bundle_path.exists() {
        return Ok(None);
    }
    let header = match load_header(&bundle_path, trust) {
        Ok(header) => header,
        Err(_) => return Ok(None),
    };
    Ok(Some(CachedStamp {
        built_at: header.built_at,
        db_sha256: header.db_sha256,
    }))
}

/// Raise the floor after a successful commit. An existing higher floor wins:
/// the value is monotonic and never lowered by a replay or a recovery.
fn raise_floor(app_data_dir: &Path, built_at: &str, db_sha256: &str) -> LauncherResult<()> {
    let candidate = parse_utc(built_at).ok_or_else(|| LauncherError::Generic {
        code: "ERR_REGISTRY_STAMP_INVALID".into(),
        message: format!("The committed catalog's build stamp `{built_at}` cannot be read."),
    })?;
    if let Some(current) = read_floor_file(app_data_dir)? {
        if parse_utc(&current.built_at).is_some_and(|time| time >= candidate) {
            return Ok(());
        }
    }
    let path = registry_floor_path(app_data_dir)?;
    // Test-only failure injection, keyed by data root so parallel tests with
    // their own TempDirs cannot affect each other. Unlike a read-only file,
    // this fails identically on Windows, Linux and macOS: `write_atomic`
    // renames a fresh temp file and would otherwise ignore the old file's
    // attributes on Unix.
    #[cfg(test)]
    if injected_floor_write_failure(app_data_dir) {
        return Err(floor_error(
            "Injected catalog rollback floor write failure.",
        ));
    }
    write_atomic(&path, format!("{built_at} {db_sha256}\n").as_bytes()).map_err(|error| {
        floor_error(format!(
            "Could not save the catalog rollback floor: {error}."
        ))
    })
}

#[cfg(test)]
fn floor_write_failures() -> &'static Mutex<Vec<PathBuf>> {
    static FAILURES: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
fn injected_floor_write_failure(app_data_dir: &Path) -> bool {
    floor_write_failures()
        .lock()
        .map(|dirs| dirs.iter().any(|dir| dir == app_data_dir))
        .unwrap_or(false)
}

#[cfg(test)]
struct FloorWriteFailureGuard(PathBuf);

#[cfg(test)]
impl Drop for FloorWriteFailureGuard {
    fn drop(&mut self) {
        if let Ok(mut dirs) = floor_write_failures().lock() {
            dirs.retain(|dir| dir != &self.0);
        }
    }
}

#[cfg(test)]
fn fail_floor_writes(app_data_dir: &Path) -> FloorWriteFailureGuard {
    floor_write_failures()
        .lock()
        .unwrap()
        .push(app_data_dir.to_path_buf());
    FloorWriteFailureGuard(app_data_dir.to_path_buf())
}

/// The strongest baseline a replacement is compared against: the higher of the
/// installed bundle's signed stamp and the durable floor. A bundle that cannot
/// be verified contributes nothing here, but the floor still applies.
fn rollback_baseline(
    app_data_dir: &Path,
    trust: &TrustSnapshot,
) -> LauncherResult<Option<CachedStamp>> {
    let installed = read_cached_stamp(app_data_dir, trust);
    let floor = durable_floor(app_data_dir, trust)?;
    Ok(match (installed, floor) {
        (Some(installed), Some(floor)) => {
            match (parse_utc(&installed.built_at), parse_utc(&floor.built_at)) {
                (Some(a), Some(b)) if b > a => Some(floor),
                _ => Some(installed),
            }
        }
        (Some(installed), None) => Some(installed),
        (None, floor) => floor,
    })
}

/// Test-only failure injection for the trust log write, keyed by data root.
#[cfg(test)]
fn trust_write_failures() -> &'static Mutex<Vec<PathBuf>> {
    static FAILURES: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
struct TrustWriteFailureGuard(PathBuf);

#[cfg(test)]
impl Drop for TrustWriteFailureGuard {
    fn drop(&mut self) {
        if let Ok(mut dirs) = trust_write_failures().lock() {
            dirs.retain(|dir| dir != &self.0);
        }
    }
}

#[cfg(test)]
fn fail_trust_writes(app_data_dir: &Path) -> TrustWriteFailureGuard {
    trust_write_failures()
        .lock()
        .unwrap()
        .push(app_data_dir.to_path_buf());
    TrustWriteFailureGuard(app_data_dir.to_path_buf())
}

/// Remember what a verified bundle taught, as one record.
///
/// The whole log is replaced atomically (temp file, fsync, rename), so a record
/// is either entirely present or entirely absent and no prefix of a record can
/// ever change trust. `remembered` is the log as this operation read it; if the
/// file has changed since, the write is refused rather than built on a stale
/// view. Returns whether anything was written.
fn remember_bundle_trust(
    app_data_dir: &Path,
    remembered_log: &str,
    verified: &VerifiedBundle,
) -> LauncherResult<bool> {
    // Expiry is not a reason to skip a revocation: a retired key stays retired
    // even if the bundle that carried the statement has since expired.
    let path =
        paths::registry_trust_state_path(app_data_dir).map_err(|_| LauncherError::Generic {
            code: "ERR_REGISTRY_TRUST_STATE".into(),
            message: "Could not resolve the catalog trust state path.".into(),
        })?;
    let on_disk = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(trust_state_error(
                &path,
                &format!("could not be read ({error})"),
            ))
        }
    };
    if on_disk != remembered_log {
        return Err(trust_state_error(
            &path,
            "changed while the catalog was being installed",
        ));
    }
    let mut statements: Vec<String> = Vec::new();
    for certificate in &verified.newly_trusted {
        let line = format!(
            "v1 key {} {} {} {} {}",
            certificate.key_id,
            certificate.public_key,
            certificate.role.as_str(),
            certificate.signed_by,
            certificate.signature
        );
        if !remembered_log.lines().any(|known| known == line) {
            statements.push(line);
        }
    }
    for revocation in &verified.accepted_revocations {
        let line = format!(
            "v1 revoke {} {} {}",
            revocation.revoked_key_id, revocation.signed_by, revocation.signature
        );
        if !remembered_log.lines().any(|known| known == line) {
            statements.push(line);
        }
    }
    if statements.is_empty() {
        return Ok(false);
    }
    let mut text = remembered_log.to_string();
    text.push_str(&format!("v1 record {}\n", statements.len()));
    for line in &statements {
        text.push_str(line);
        text.push('\n');
    }
    #[cfg(test)]
    if trust_write_failures()
        .lock()
        .map(|dirs| dirs.iter().any(|dir| dir == app_data_dir))
        .unwrap_or(false)
    {
        return Err(trust_state_error(&path, "could not be saved (injected)"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            trust_state_error(&path, &format!("could not be created ({error})"))
        })?;
    }
    write_atomic(&path, text.as_bytes())
        .map_err(|error| trust_state_error(&path, &format!("could not be saved ({error})")))?;
    Ok(true)
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

fn load_header(path: &Path, trust: &TrustSnapshot) -> Result<CatalogHeader, BundleLoadError> {
    let trust = trust.trust();
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

fn load_payload(path: &Path, trust: &TrustSnapshot) -> Result<VerifiedBundle, BundleLoadError> {
    let bytes = std::fs::read(path).map_err(BundleLoadError::Io)?;
    registry_bundle::verify_bundle(&bytes, trust.trust()).map_err(BundleLoadError::Bundle)
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
    trust: &TrustSnapshot,
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
    compiled: &RegistryTrust,
    label: &str,
) -> LauncherResult<InstalledBundle> {
    let trust = TrustSnapshot::load(app_data_dir, compiled)?;
    install_bundle_bytes_checked_with(app_data_dir, bundle_bytes, &trust, label)
}

/// The install body: every decision uses the one `trust` snapshot.
fn install_bundle_bytes_checked_with(
    app_data_dir: &Path,
    bundle_bytes: &[u8],
    trust: &TrustSnapshot,
    label: &str,
) -> LauncherResult<InstalledBundle> {
    let verified =
        registry_bundle::verify_bundle(bundle_bytes, trust.trust()).map_err(bundle_error)?;

    // Anti-rollback is checked only after the signature, so the stamp being
    // compared is one the signing key vouched for. The release tag is display
    // metadata and never takes part in this decision. An installed bundle that
    // no longer verifies contributes nothing, but the durable floor still does.
    let cached_stamp = rollback_baseline(app_data_dir, trust)?;
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

    install_bundle_bytes_with(app_data_dir, &verified.header, bundle_bytes, trust)?;

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
    trust: &TrustSnapshot,
) -> LauncherResult<Option<InstalledBundle>> {
    let verified =
        registry_bundle::verify_bundle(bundle_bytes, trust.trust()).map_err(bundle_error)?;
    let cached = rollback_baseline(app_data_dir, trust)?;
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
/// The remembered trust state is the single durable authority. As soon as the
/// candidate verifies, its revocations and keys are appended and synced *before*
/// any bundle file is touched; a failed append refuses the install and changes
/// nothing. A revocation may untrust the bundle currently installed — that is
/// safe now, because an unverifiable bundle contributes no stamp and the
/// rollback floor keeps the catalogue from being replayed.
///
/// The bundle is one file, so it is replaced by one rename; the previous good
/// bundle is copied to `registry.bundle.prev` first. The rollback floor is
/// raised after the rename. The readable database is a derived copy that the
/// next open can repair.
///
/// If a swap fails after a trust append, the catalog can be left offline: the
/// verified revocation is kept while the bundle it revoked is still current.
/// That is deliberate — a genuine revocation must not be discarded to keep a
/// retired signer usable — and the error says exactly what happened. When no
/// statement was appended it says the trust state did not change.
fn swap_error(error: LauncherError, trust_written: bool) -> LauncherError {
    let code = error.code();
    let detail = if trust_written {
        "The catalog trust update was kept; reinstall or sync the catalog to restore it."
    } else {
        "The catalog trust state was not changed."
    };
    LauncherError::Generic {
        code,
        message: format!("{error}. {detail}"),
    }
}

/// A floor write that fails after the bundle swap is a different case: the new
/// catalog is already installed and the floor can be checkpointed on the next
/// install, so do not advise reinstalling or claim a trust update was kept.
fn floor_after_swap_error(error: LauncherError) -> LauncherError {
    LauncherError::Generic {
        code: error.code(),
        message: format!(
            "{error}. The new catalog is installed; its rollback floor will be re-checkpointed next time."
        ),
    }
}

/// [`install_bundle_bytes_with`] for a caller that has no snapshot yet.
#[cfg(test)]
fn install_bundle_bytes(
    app_data_dir: &Path,
    header: &CatalogHeader,
    bundle_bytes: &[u8],
    compiled: &RegistryTrust,
) -> LauncherResult<()> {
    let trust = TrustSnapshot::load(app_data_dir, compiled)?;
    install_bundle_bytes_with(app_data_dir, header, bundle_bytes, &trust)
}

fn install_bundle_bytes_with(
    app_data_dir: &Path,
    header: &CatalogHeader,
    bundle_bytes: &[u8],
    trust: &TrustSnapshot,
) -> LauncherResult<()> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    let previous_path = paths::registry_bundle_previous_path(app_data_dir)
        .map_err(|_| LauncherError::RegistryMissing)?;
    let payload =
        registry_bundle::verify_bundle(bundle_bytes, trust.trust()).map_err(bundle_error)?;

    // Capture the pre-existing verified baseline before changing trust: the
    // append below may revoke the installed bundle's signer, so its stamp must
    // already be durable. A malformed or unreadable floor refuses the install.
    ensure_floor_baseline(app_data_dir, trust)?;
    // Signed and verified trust facts only ever add, so writing them ahead of
    // the swap is always safe and no revocation can be lost by a later failure.
    let trust_written = remember_bundle_trust(app_data_dir, &trust.log_text, &payload)?;

    // Keep the previous copy only when the current file looks like a bundle.
    // A corrupt current must not overwrite a good previous one.
    if bundle_path.exists() {
        if let Ok(current) = std::fs::read(&bundle_path) {
            if registry_bundle::parse_bundle(&current).is_ok() {
                write_atomic(&previous_path, &current).map_err(|error| {
                    swap_error(
                        LauncherError::Generic {
                            code: "ERR_REGISTRY_WRITE".into(),
                            message: format!("Could not keep the previous catalog: {error}."),
                        },
                        trust_written,
                    )
                })?;
            }
        }
    }

    // COMMIT: one rename replaces the current bundle.
    write_atomic(&bundle_path, bundle_bytes).map_err(|error| {
        swap_error(
            LauncherError::Generic {
                code: "ERR_REGISTRY_WRITE".into(),
                message: format!("Could not install the catalog bundle: {error}."),
            },
            trust_written,
        )
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
        trust,
    );
    raise_floor(app_data_dir, &header.built_at, &header.db_sha256)
        .map_err(floor_after_swap_error)?;
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
    compiled: &RegistryTrust,
) -> CachedRegistryIntegrity {
    let bundle_path = match paths::registry_bundle_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return CachedRegistryIntegrity::Absent,
    };
    if bundle_path.exists() {
        // A log that cannot be trusted is not the bundle's fault: report it, and
        // neither recover from the previous copy nor move the bundle aside.
        return match TrustSnapshot::load(app_data_dir, compiled) {
            Ok(trust) => verify_bundle_file(app_data_dir, &bundle_path, &trust),
            Err(error) => CachedRegistryIntegrity::Error(format!(
                "{error} The catalog will not be used this run."
            )),
        };
    }

    // A pre-bundle cache is not a catalog. It has no signed expiry, no key id
    // and no rotation, so this client ignores it and the next sync downloads a
    // bundle. The next successful install replaces `registry.db`.
    CachedRegistryIntegrity::Absent
}

fn verify_bundle_file(
    app_data_dir: &Path,
    bundle_path: &Path,
    trust: &TrustSnapshot,
) -> CachedRegistryIntegrity {
    match load_header(bundle_path, trust) {
        Ok(header) => match ensure_database(app_data_dir, bundle_path, &header, None, trust) {
            Ok(()) => match below_floor(&header, app_data_dir, trust) {
                Ok(true) => CachedRegistryIntegrity::Stale(floor_stale_message()),
                Ok(false) => {
                    if header.is_expired_at(Utc::now()) {
                        CachedRegistryIntegrity::Stale(stale_message(&header))
                    } else {
                        CachedRegistryIntegrity::Verified
                    }
                }
                // A floor we cannot read is not a floor we can ignore.
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
    trust: &TrustSnapshot,
    reason: String,
) -> CachedRegistryIntegrity {
    let previous_path = match paths::registry_bundle_previous_path(app_data_dir) {
        Ok(path) => path,
        Err(_) => return quarantine_bundle(bundle_path, reason),
    };
    if previous_path.exists() {
        if let Ok(header) = load_header(&previous_path, trust) {
            // A backup below the durable floor is a superseded catalog. Keep it
            // for inspection but never promote it: a read-only mode would still
            // expose stale curated data to installs, and needs new status state.
            let is_below_floor = below_floor(&header, app_data_dir, trust).unwrap_or(true);
            if !is_below_floor
                && ensure_database(app_data_dir, &previous_path, &header, None, trust).is_ok()
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

/// Whether a verified bundle's signed stamp is below the durable floor. The
/// comparison is between parsed UTC instants, never between the stamp strings.
fn below_floor(
    header: &CatalogHeader,
    app_data_dir: &Path,
    trust: &TrustSnapshot,
) -> LauncherResult<bool> {
    Ok(durable_floor(app_data_dir, trust)?.is_some_and(|floor| {
        parse_utc(&header.built_at)
            .zip(parse_utc(&floor.built_at))
            .is_some_and(|(candidate, floor)| candidate < floor)
    }))
}

fn floor_stale_message() -> String {
    "The cached catalog is older than a previously committed catalog. It is shown for browsing \
     only; sync for a fresh catalog before installing or updating."
        .to_string()
}

/// The signed build stamp and payload hash of the cached catalog, for the
/// rollback check. The hash lets an equal stamp over different bytes be
/// refused instead of silently replacing the current catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedStamp {
    built_at: String,
    db_sha256: String,
}

/// An installed bundle that cannot be verified contributes no stamp: neither
/// trust statements nor a signed build time can be taken from it. The durable
/// floor is the authority that still prevents replay in that state.
fn read_cached_stamp(app_data_dir: &Path, trust: &TrustSnapshot) -> Option<CachedStamp> {
    let bundle_path = paths::registry_bundle_path(app_data_dir).ok()?;
    if !bundle_path.exists() {
        return None;
    }
    let header = load_header(&bundle_path, trust).ok()?;
    Some(CachedStamp {
        built_at: header.built_at,
        db_sha256: header.db_sha256,
    })
}

fn cached_catalog_summary_with_trust(
    app_data_dir: &Path,
    trust: &TrustSnapshot,
) -> Option<CachedCatalogSummary> {
    let bundle_path = paths::registry_bundle_path(app_data_dir).ok()?;
    if bundle_path.exists() {
        let header = load_header(&bundle_path, trust).ok()?;
        // Readiness is not "a header verified": the readable database must be
        // present and be the bytes the header signed. A crash between the
        // bundle rename and extraction, or a swapped registry.db, must show as
        // not ready rather than a catalog the desktop thinks it can use.
        let db_path = paths::registry_db_path(app_data_dir).ok()?;
        if !db_path.exists() {
            return None;
        }
        let digest = database_hash_memoized(&db_path).ok()?;
        if !digest.eq_ignore_ascii_case(&header.db_sha256) {
            return None;
        }
        return Some(CachedCatalogSummary {
            schema_version: Some(header.schema_version),
            // A below-floor catalog is stale in the same user-facing sense as
            // an expired one: it can be browsed but not used for installations.
            stale: header.is_expired_at(Utc::now())
                || below_floor(&header, app_data_dir, trust).unwrap_or(true),
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
    compiled: &RegistryTrust,
) -> LauncherResult<Option<String>> {
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        let trust = &TrustSnapshot::load(app_data_dir, compiled)?;
        let header = load_header(&bundle_path, trust).map_err(|error| LauncherError::Generic {
            code: "ERR_REGISTRY_SIGNATURE_INVALID".into(),
            message: format!("The catalog bundle could not be used: {error}."),
        })?;
        // The digest is used as a cache key for the readable database, so it
        // must not materialize one for a bundle a plain open would refuse.
        if below_floor(&header, app_data_dir, trust)? {
            return Err(LauncherError::Generic {
                code: "ERR_REGISTRY_ROLLBACK".into(),
                message: floor_stale_message(),
            });
        }
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
    compiled: &RegistryTrust,
) -> RegistryStatus {
    // A log that cannot be trusted is shown as such, never as an empty catalog
    // that silently trusts only the compiled keys.
    let trust = match TrustSnapshot::load(app_data_dir, compiled) {
        Ok(trust) => trust,
        Err(error) => {
            return RegistryStatus {
                has_cached_db: false,
                cached_tag: get_cached_tag(local_state_path).ok().flatten(),
                cached_schema_version: None,
                latest_tag: None,
                update_available: false,
                checked: false,
                stale: false,
                expires_at: None,
                message: error.to_string(),
            }
        }
    };
    let cached = cached_catalog_summary_with_trust(app_data_dir, &trust);
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
                        &TrustSnapshot::load(app_data_dir, &RegistryTrust::production())?,
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
    compiled: &RegistryTrust,
) -> LauncherResult<VerifiedRegistry> {
    let trust = compiled;
    let bundle_path =
        paths::registry_bundle_path(app_data_dir).map_err(|_| LauncherError::RegistryMissing)?;
    if bundle_path.exists() {
        // One snapshot for the open and for any recovery it triggers. A log that
        // cannot be trusted refuses the open, whatever the bundle looks like.
        let snapshot = TrustSnapshot::load(app_data_dir, compiled)?;
        let first = open_bundle_once(app_data_dir, &bundle_path, &snapshot);
        if first.is_ok() {
            return first;
        }
        // A crash between installing the bundle and extracting the database
        // (or a damaged current bundle) is recoverable from the previous copy.
        match verify_bundle_file(app_data_dir, &bundle_path, &snapshot) {
            CachedRegistryIntegrity::Verified
            | CachedRegistryIntegrity::Stale(_)
            | CachedRegistryIntegrity::Recovered(_) => {
                return open_bundle_once(app_data_dir, &bundle_path, &snapshot)
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
    trust: &TrustSnapshot,
) -> LauncherResult<VerifiedRegistry> {
    let header = load_header(bundle_path, trust).map_err(|error| LauncherError::Generic {
        code: "ERR_REGISTRY_SIGNATURE_INVALID".into(),
        message: format!("The catalog bundle could not be used: {error}."),
    })?;
    if below_floor(&header, app_data_dir, trust)? {
        return Err(LauncherError::Generic {
            code: "ERR_REGISTRY_ROLLBACK".into(),
            message: floor_stale_message(),
        });
    }
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

    /// A candidate's trust statements are appended durably before any bundle
    /// file changes. If that append fails, nothing at all is changed.
    #[test]
    fn a_failed_trust_append_refuses_before_any_file_change() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(100);
        let recovery = test_key(101);
        let replacement = test_key(102);
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
        // Fail the log write itself (not the read), identically on every OS.
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        let _failing = fail_trust_writes(dir.path());

        let error = install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate")
            .expect_err("a failed trust append must refuse the install");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
        );
        assert!(!paths::registry_bundle_previous_path(dir.path())
            .unwrap()
            .exists());
        assert!(!trust_path.exists(), "a failed write leaves no log behind");
    }

    /// Recovery cannot read the remembered trust state: a backup signed by a
    /// key that might since have been retired must not be promoted.
    #[test]
    fn recovery_fails_closed_when_the_trust_authority_cannot_be_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let signer = test_key(103);
        let trust = RegistryTrust::with_key(test_public(&signer));
        let payload = sample_db();
        let header = fresh_header(&signer, &payload);
        let previous = signed_bundle(&signer, &header, &payload);
        fs::write(
            paths::registry_bundle_previous_path(dir.path()).unwrap(),
            &previous,
        )
        .unwrap();
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), b"damaged").unwrap();
        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            !matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "an unreadable trust authority must block recovery: {result:?}"
        );
    }

    /// A backup signed by a key the remembered trust has retired is never
    /// restored, even though the compiled keys still know it.
    #[test]
    fn recovery_does_not_restore_a_prev_signed_by_a_revoked_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(104);
        let recovery = test_key(105);
        let replacement = test_key(106);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let header = fresh_header(&stolen, &payload);
        let previous = signed_bundle(&stolen, &header, &payload);
        fs::write(
            paths::registry_bundle_previous_path(dir.path()).unwrap(),
            &previous,
        )
        .unwrap();
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), b"damaged").unwrap();
        let statement = revocation(&recovery, &stolen);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 revoke {} {} {}\n",
                statement.revoked_key_id, statement.signed_by, statement.signature
            ),
        )
        .unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            !matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "a retired signer's backup must not be restored: {result:?}"
        );
    }

    /// An installed bundle signed by a now-retired key contributes neither
    /// trust statements nor a stamp, but it must not block a valid replacement.
    #[test]
    fn a_retired_signer_installed_bundle_does_not_block_a_valid_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(107);
        let recovery = test_key(108);
        let replacement = test_key(109);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
        ]);
        let payload = sample_db();
        let old_header = fresh_header(&stolen, &payload);
        install_bundle_bytes(
            dir.path(),
            &old_header,
            &signed_bundle(&stolen, &old_header, &payload),
            &trust,
        )
        .unwrap();
        let statement = revocation(&recovery, &stolen);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 revoke {} {} {}\n",
                statement.revoked_key_id, statement.signed_by, statement.signature
            ),
        )
        .unwrap();

        let new_payload = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let new_header = header_for(
            &replacement,
            &new_payload,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let new_bundle = signed_bundle(&replacement, &new_header, &new_payload);
        install_bundle_bytes_checked(dir.path(), &new_bundle, &trust, "candidate")
            .expect("an unverifiable installed bundle must not block a valid replacement");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            new_bundle
        );
    }

    /// Once the installed bundle is unverifiable, only the durable floor keeps
    /// an older signed bundle from being replayed.
    #[test]
    fn a_replay_is_refused_against_the_durable_floor() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(110);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes(
            dir.path(),
            &header,
            &signed_bundle(&key, &header, &payload),
            &trust,
        )
        .unwrap();
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), b"damaged").unwrap();

        let replay_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let replay_header = header_for(
            &key,
            &replay_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&key, &replay_header, &replay_payload),
            &trust,
            "replay",
        )
        .expect_err("the durable floor must refuse an older signed bundle");
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
    }

    /// A healthy open must not use a cached bundle below the durable floor, and
    /// status shows the same stale state an expired catalog gets.
    #[test]
    fn a_bundle_below_the_floor_is_not_opened_and_looks_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(117);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-17T12:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let bundle = signed_bundle(&key, &header, &payload);
        install_bundle_bytes(dir.path(), &header, &bundle, &trust).unwrap();
        set_cached_tag(&ls_path, &header.tag).unwrap();

        // A signed older bundle is swapped in below the persisted floor.
        let older_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let older_header = header_for(
            &key,
            &older_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let older = signed_bundle(&key, &older_header, &older_payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &older).unwrap();
        fs::write(paths::registry_db_path(dir.path()).unwrap(), &older_payload).unwrap();

        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_err(),
            "a below-floor bundle must not be opened"
        );
        assert!(
            get_status_with_trust(dir.path(), &ls_path, &trust).stale,
            "a below-floor bundle must show the expired catalog's stale state"
        );
    }

    /// Recovery compares the backup's stamp with the floor as an instant. This
    /// stamp is textually later but an earlier moment, so it must not be restored.
    #[test]
    fn recovery_refuses_a_prev_below_the_floor_by_instant_not_text() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(118);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = header_for(
            &key,
            &payload,
            "2026-11-16T13:00:00+02:00",
            "2099-01-01T00:00:00+00:00",
        );
        fs::write(
            paths::registry_bundle_previous_path(dir.path()).unwrap(),
            signed_bundle(&key, &header, &payload),
        )
        .unwrap();
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), b"damaged").unwrap();
        fs::write(
            registry_floor_path(dir.path()).unwrap(),
            format!("2026-11-16T12:00:00+00:00 {}\n", "0".repeat(64)),
        )
        .unwrap();

        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            !matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "13:00+02:00 is 11:00 UTC, below the floor: {result:?}"
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

    /// A candidate's revocation is durable before any bundle file changes, so
    /// it survives a failed commit. The bundle files are untouched and a retry
    /// works; the old bundle may now be untrusted, which is the intended
    /// consequence of a committed revocation.
    #[test]
    fn a_failed_commit_keeps_the_trust_fact_and_the_old_bundle_files() {
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
        assert!(
            error.to_string().contains("trust update was kept")
                && error.to_string().contains("reinstall or sync"),
            "the failure must say the trust fact survived and how to repair: {error}"
        );

        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        assert!(
            fs::read_to_string(&trust_path)
                .unwrap()
                .contains("v1 revoke"),
            "a verified revocation is durable before the commit"
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
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

    /// A pre-floor cache must persist its baseline before the append revokes its
    /// signer. If the swap then fails, an older bundle from a still-trusted key
    /// must still be refused by the floor.
    #[test]
    fn a_pre_floor_baseline_survives_a_failed_revoking_swap() {
        let dir = tempfile::TempDir::new().unwrap();
        let stolen = test_key(112);
        let recovery = test_key(113);
        let replacement = test_key(114);
        let trusted = test_key(115);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&stolen)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
            (RegistryKeyRole::Primary, test_public(&trusted)),
        ]);
        assert!(!registry_floor_path(dir.path()).unwrap().exists());

        let payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let current_header = header_for(
            &stolen,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let current = signed_bundle(&stolen, &current_header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();

        let mut revoking = header_for(
            &replacement,
            &payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        revoking.revocations.push(revocation(&recovery, &stolen));
        let revoking_bytes = signed_bundle(&replacement, &revoking, &payload);
        let prev = paths::registry_bundle_previous_path(dir.path()).unwrap();
        fs::create_dir_all(&prev).unwrap();
        install_bundle_bytes_checked(dir.path(), &revoking_bytes, &trust, "revoking")
            .expect_err("the obstructed swap must fail");

        // The floor was captured from the installed bundle before its signer was
        // revoked, so it outlives the failed swap.
        let floor = read_floor_file(dir.path()).unwrap().unwrap();
        assert_eq!(floor.built_at, "2026-11-16T12:00:00+00:00");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
        );

        let older_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let older = header_for(
            &trusted,
            &older_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&trusted, &older, &older_payload),
            &trust,
            "older",
        )
        .expect_err("the persisted floor must refuse an older signed bundle");
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
    }

    /// A floor read error (as opposed to an absent floor) must refuse an install.
    #[test]
    fn an_unreadable_floor_refuses_an_install() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(116);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        let current = signed_bundle(&key, &header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &current).unwrap();
        fs::create_dir_all(registry_floor_path(dir.path()).unwrap()).unwrap();

        let error = install_bundle_bytes_checked(dir.path(), &current, &trust, "candidate")
            .expect_err("a floor read error is not an absent floor");
        assert_eq!(error.code(), "ERR_REGISTRY_FLOOR");
        assert!(
            error
                .to_string()
                .contains("Could not read the catalog rollback floor"),
            "the refusal must be the read error, not a later write failure: {error}"
        );
        assert!(
            !paths::registry_bundle_previous_path(dir.path())
                .unwrap()
                .exists(),
            "the install must fail before any backup is created"
        );
    }

    /// A malformed but readable floor is an error, not an unset floor: treating
    /// it as unset would silently drop rollback protection.
    #[test]
    fn a_malformed_floor_refuses_an_install() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(119);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        let bundle = signed_bundle(&key, &header, &payload);
        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), &bundle).unwrap();
        fs::write(
            registry_floor_path(dir.path()).unwrap(),
            "not a stamp not a hash\n",
        )
        .unwrap();

        let error = install_bundle_bytes_checked(dir.path(), &bundle, &trust, "candidate")
            .expect_err("a malformed floor must refuse the install");
        assert_eq!(error.code(), "ERR_REGISTRY_FLOOR");
    }

    /// The floor must be checkpointed to the verified maximum, not only when it
    /// is unset: a lagging floor plus a revoking append must not expose an older
    /// bundle again.
    #[test]
    fn a_lagging_floor_is_checkpointed_before_a_revoking_append() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = test_key(120);
        let recovery = test_key(121);
        let replacement = test_key(122);
        let trusted = test_key(123);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&old)),
            (RegistryKeyRole::Primary, test_public(&replacement)),
            (RegistryKeyRole::Primary, test_public(&trusted)),
        ]);

        let first_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let first_header = header_for(
            &old,
            &first_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let first = signed_bundle(&old, &first_header, &first_payload);
        install_bundle_bytes(dir.path(), &first_header, &first, &trust).unwrap();
        assert_eq!(
            read_floor_file(dir.path()).unwrap().unwrap().built_at,
            "2026-11-15T12:00:00+00:00"
        );

        // Install a newer bundle but make its post-swap floor write fail, so the
        // stored floor lags behind the installed bundle.
        let floor_failure = fail_floor_writes(dir.path());
        let second_payload = sample_db_with_built_at("2026-11-17T12:00:00+00:00");
        let second_header = header_for(
            &old,
            &second_payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let second = signed_bundle(&old, &second_header, &second_payload);
        let error = install_bundle_bytes_checked(dir.path(), &second, &trust, "newer")
            .expect_err("the lagging floor write must fail");
        assert_eq!(error.code(), "ERR_REGISTRY_FLOOR");
        drop(floor_failure);
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            second
        );
        assert_eq!(
            read_floor_file(dir.path()).unwrap().unwrap().built_at,
            "2026-11-15T12:00:00+00:00"
        );

        // A revoking append whose swap fails: the installed bundle's signer is
        // retired, so only a checkpoint of its stamp can still block the replay.
        let mut revoking = header_for(
            &replacement,
            &second_payload,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        revoking.revocations.push(revocation(&recovery, &old));
        let prev = paths::registry_bundle_previous_path(dir.path()).unwrap();
        let _ = fs::remove_file(&prev);
        fs::create_dir_all(&prev).unwrap();
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&replacement, &revoking, &second_payload),
            &trust,
            "revoking",
        )
        .expect_err("the obstructed swap must fail");
        fs::remove_dir(&prev).unwrap();
        assert_eq!(
            read_floor_file(dir.path()).unwrap().unwrap().built_at,
            "2026-11-17T12:00:00+00:00",
            "the verified maximum must be checkpointed before the revoking append"
        );

        let replay_payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let replay_header = header_for(
            &trusted,
            &replay_payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&trusted, &replay_header, &replay_payload),
            &trust,
            "replay",
        )
        .expect_err("the checkpointed floor must refuse the older signed bundle");
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
    }

    /// A floor write that fails after a successful swap must say the new catalog
    /// is installed, not claim a trust update was kept or advise a restore.
    #[test]
    fn a_failed_floor_raise_after_swap_says_the_catalog_is_installed() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(124);
        let trust = RegistryTrust::with_key(test_public(&key));
        let first_payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let first_header = header_for(
            &key,
            &first_payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes(
            dir.path(),
            &first_header,
            &signed_bundle(&key, &first_header, &first_payload),
            &trust,
        )
        .unwrap();
        let _floor_failure = fail_floor_writes(dir.path());

        let second_payload = sample_db_with_built_at("2026-11-17T12:00:00+00:00");
        let second_header = header_for(
            &key,
            &second_payload,
            "2026-11-17T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let second = signed_bundle(&key, &second_header, &second_payload);
        let error = install_bundle_bytes_checked(dir.path(), &second, &trust, "newer")
            .expect_err("the post-swap floor write must fail");
        assert_eq!(error.code(), "ERR_REGISTRY_FLOOR");
        let message = error.to_string();
        assert!(
            message.contains("new catalog is installed") && message.contains("re-checkpointed"),
            "message must describe the installed catalog: {message}"
        );
        assert!(
            !message.contains("trust update was kept") && !message.contains("reinstall or sync"),
            "no trust statement was appended and the bundle is installed: {message}"
        );
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            second
        );
    }

    /// A swap failure without a trust append must not claim a trust update.
    #[test]
    fn a_swap_failure_without_a_trust_append_says_trust_was_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(125);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&key, &header, &payload),
        )
        .unwrap();
        fs::create_dir_all(paths::registry_bundle_previous_path(dir.path()).unwrap()).unwrap();

        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&key, &header, &payload),
            &trust,
            "candidate",
        )
        .expect_err("the obstructed backup must fail the install");
        assert!(
            error.to_string().contains("trust state was not changed"),
            "message must not claim a trust update: {error}"
        );
    }

    /// Floor errors fail closed on a plain open and on backup recovery too.
    #[test]
    fn an_unreadable_floor_refuses_a_plain_open_and_recovery() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(126);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&key, &header, &payload),
        )
        .unwrap();
        fs::create_dir_all(registry_floor_path(dir.path()).unwrap()).unwrap();

        let error = match open_verified_registry_with_trust(dir.path(), &trust) {
            Ok(_) => panic!("a floor read error must refuse a plain open"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "ERR_REGISTRY_FLOOR");

        fs::write(paths::registry_bundle_path(dir.path()).unwrap(), b"damaged").unwrap();
        fs::write(
            paths::registry_bundle_previous_path(dir.path()).unwrap(),
            signed_bundle(&key, &header, &payload),
        )
        .unwrap();
        let result = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            !matches!(result, CachedRegistryIntegrity::Recovered(_)),
            "a floor read error must block recovery: {result:?}"
        );
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

    /// Readiness means the readable database is present and matches the
    /// installed bundle, not merely that a bundle header verifies.
    #[test]
    fn has_cached_db_requires_a_readable_matching_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let ls_path = local_state_in(dir.path());
        let key = test_key(98);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&key, &header, &payload),
        )
        .unwrap();

        assert!(
            !get_status_with_trust(dir.path(), &ls_path, &trust).has_cached_db,
            "a bundle with no extracted database is not ready"
        );
        let db_path = paths::registry_db_path(dir.path()).unwrap();
        fs::write(&db_path, &payload).unwrap();
        assert!(get_status_with_trust(dir.path(), &ls_path, &trust).has_cached_db);
        fs::write(&db_path, b"not the database the bundle signed").unwrap();
        assert!(
            !get_status_with_trust(dir.path(), &ls_path, &trust).has_cached_db,
            "a database that does not match the bundle is not ready"
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

    /// An unreadable remembered trust state may be hiding revocations, so no
    /// candidate is trusted against it — even one with no statements of its own.
    #[test]
    fn an_unreadable_trust_state_refuses_an_install() {
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
        let error = install_bundle_bytes_checked(dir.path(), &candidate, &trust, "candidate")
            .expect_err("an unreadable trust state must refuse the install");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            current
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

    /// An expired installed bundle still has a signed stamp, so it can be a
    /// rollback baseline, but it contributes no trust statements: a valid
    /// replacement is not blocked by what an expired bundle claims.
    #[test]
    fn an_expired_current_bundle_does_not_block_a_valid_replacement() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(87);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let expired = header_for(
            &key,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2020-01-01T00:00:00+00:00",
        );
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&key, &expired, &payload),
        )
        .unwrap();

        let newer_payload = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let newer = header_for(
            &key,
            &newer_payload,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&key, &newer, &newer_payload),
            &trust,
            "candidate",
        )
        .expect("an expired installed bundle must not block a valid replacement");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            signed_bundle(&key, &newer, &newer_payload)
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

    /// An unverifiable installed bundle contributes no stamp; the durable
    /// floor, not the unreadable bundle, is what still blocks a replay.
    #[test]
    fn an_unreadable_cached_stamp_contributes_no_stamp() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(7);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db();
        let header = fresh_header(&key, &payload);
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);
        assert_eq!(
            read_cached_stamp(
                dir.path(),
                &TrustSnapshot::load(dir.path(), &trust).unwrap()
            )
            .map(|stamp| stamp.built_at),
            Some(header.built_at)
        );

        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            b"corrupt current bundle",
        )
        .unwrap();
        assert_eq!(
            read_cached_stamp(
                dir.path(),
                &TrustSnapshot::load(dir.path(), &trust).unwrap()
            ),
            None
        );
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

    /// The trust one operation would use: compiled keys plus the log.
    fn effective_trust(
        app_data_dir: &Path,
        compiled: &RegistryTrust,
    ) -> LauncherResult<RegistryTrust> {
        TrustSnapshot::load(app_data_dir, compiled).map(|snapshot| snapshot.trust)
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
    /// recovery key. The log is no longer trustworthy, so the open is refused
    /// rather than the line quietly skipped.
    #[test]
    fn a_forged_revocation_line_is_refused() {
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
            open_verified_registry_with_trust(dir.path(), &trust).is_err(),
            "a revocation not signed by a recovery key means the log cannot be trusted"
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

    fn recovery_certificate(
        signer: &ed25519_dalek::SigningKey,
        new_public: &str,
    ) -> KeyCertificate {
        use ed25519_dalek::Signer;
        let signed_by = registry_bundle::key_id_for_public_key(&test_public(signer));
        let new_id = registry_bundle::key_id_for_public_key(new_public);
        let message = KeyCertificate::signed_bytes(
            &new_id,
            new_public,
            RegistryKeyRole::Recovery,
            &signed_by,
        );
        KeyCertificate {
            key_id: new_id,
            public_key: new_public.to_string(),
            role: RegistryKeyRole::Recovery,
            signed_by,
            signature: hex::encode(signer.sign(&message).to_bytes()),
        }
    }

    /// Compiled keys for the rotation tests: one primary, one stolen primary,
    /// one compiled recovery key.
    fn rotation_trust(
        primary: &ed25519_dalek::SigningKey,
        stolen: &ed25519_dalek::SigningKey,
        recovery: &ed25519_dalek::SigningKey,
    ) -> RegistryTrust {
        RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(recovery)),
            (RegistryKeyRole::Primary, test_public(primary)),
            (RegistryKeyRole::Primary, test_public(stolen)),
        ])
    }

    /// The open must fail because the signer is no longer trusted, not because
    /// of a rollback floor or an unreadable log.
    fn assert_refused_as_untrusted_signer(dir: &Path, trust: &RegistryTrust) {
        let error = open_verified_registry_with_trust(dir, trust)
            .err()
            .expect("a retired signer's bundle must not open");
        assert_eq!(error.code(), "ERR_REGISTRY_SIGNATURE_INVALID", "{error:?}");
    }

    /// One bundle introduces a recovery key, uses it to revoke the stolen
    /// primary, and then retires it. The remembered log must still enforce the
    /// stolen-key revocation after a reload, however the bundle orders them.
    #[test]
    fn a_revocation_survives_its_signer_being_revoked_in_the_same_bundle() {
        for reversed in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let primary = test_key(140);
            let stolen = test_key(141);
            let recovery = test_key(142);
            let second_recovery = test_key(143);
            let trust = rotation_trust(&primary, &stolen, &recovery);
            let payload = sample_db();
            let mut header = fresh_header(&primary, &payload);
            header.certificates.push(recovery_certificate(
                &recovery,
                &test_public(&second_recovery),
            ));
            header
                .revocations
                .push(revocation(&second_recovery, &stolen));
            header
                .revocations
                .push(revocation(&recovery, &second_recovery));
            if reversed {
                header.revocations.reverse();
            }
            let bundle = signed_bundle(&primary, &header, &payload);
            install_bundle_bytes_checked(dir.path(), &bundle, &trust, "rotation").unwrap();

            // A reload: the stolen key must stay retired.
            let stale = fresh_header(&stolen, &payload);
            fs::write(
                paths::registry_bundle_path(dir.path()).unwrap(),
                signed_bundle(&stolen, &stale, &payload),
            )
            .unwrap();
            assert_refused_as_untrusted_signer(dir.path(), &trust);
            let reloaded = effective_trust(dir.path(), &trust).unwrap();
            let revoked = reloaded.revoked_ids();
            for key in [&stolen, &second_recovery] {
                assert!(
                    revoked.contains(&registry_bundle::key_id_for_public_key(&test_public(key))),
                    "both revocations are enforced after a reload (reversed: {reversed})"
                );
            }
        }
    }

    /// The log's order is not a security fact: a revocation line written before
    /// the line that introduces its signer still counts.
    #[test]
    fn the_order_of_remembered_lines_does_not_change_what_they_mean() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(144);
        let stolen = test_key(145);
        let recovery = test_key(146);
        let second_recovery = test_key(147);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let cert = recovery_certificate(&recovery, &test_public(&second_recovery));
        let by_second = revocation(&second_recovery, &stolen);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 revoke {} {} {}\nv1 key {} {} recovery {} {}\n",
                by_second.revoked_key_id,
                by_second.signed_by,
                by_second.signature,
                cert.key_id,
                cert.public_key,
                cert.signed_by,
                cert.signature
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
        assert_refused_as_untrusted_signer(dir.path(), &trust);
    }

    /// A trust log that cannot be read is an error on an ordinary open, not a
    /// quiet fall back to the compiled keys, and it does not quarantine a good
    /// bundle.
    #[test]
    fn an_unreadable_trust_log_refuses_an_ordinary_open_without_quarantining() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(148);
        let trust = RegistryTrust::with_key(test_public(&primary));
        let payload = sample_db();
        let header = fresh_header(&primary, &payload);
        let bundle_path = paths::registry_bundle_path(dir.path()).unwrap();
        fs::write(&bundle_path, signed_bundle(&primary, &header, &payload)).unwrap();
        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();

        let error = open_verified_registry_with_trust(dir.path(), &trust)
            .err()
            .expect("an unreadable trust log must not open the catalog");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
        let integrity = verify_cached_registry_with_trust(dir.path(), &trust);
        assert!(
            matches!(integrity, CachedRegistryIntegrity::Error(_)),
            "{integrity:?}"
        );
        assert!(
            bundle_path.exists(),
            "a good bundle must not be moved aside"
        );
    }

    /// A statement line that cannot be authenticated is an error, not an ignored
    /// line; removing the log is the user's way to start over.
    #[test]
    fn an_unauthenticatable_trust_line_refuses_an_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(149);
        let recovery = test_key(150);
        let attacker = test_key(151);
        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, test_public(&recovery)),
            (RegistryKeyRole::Primary, test_public(&primary)),
        ]);
        let forged = revocation(&attacker, &primary);
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        fs::write(
            &trust_path,
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
        let error = open_verified_registry_with_trust(dir.path(), &trust)
            .err()
            .expect("an unauthenticatable log must not open the catalog");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");

        fs::remove_file(&trust_path).unwrap();
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
    }

    /// One rule for chains: revoking a recovery key also retires every key it
    /// certified, including keys remembered from an earlier bundle.
    #[test]
    fn revoking_a_signer_retires_the_keys_it_certified_after_a_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(152);
        let stolen = test_key(153);
        let recovery = test_key(154);
        let second_recovery = test_key(155);
        let child = test_key(156);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();

        let mut first = fresh_header(&primary, &payload);
        first.certificates.push(recovery_certificate(
            &recovery,
            &test_public(&second_recovery),
        ));
        first
            .certificates
            .push(certificate(&second_recovery, &test_public(&child)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();
        let child_bundle = signed_bundle(&child, &fresh_header(&child, &payload), &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            &child_bundle,
        )
        .unwrap();
        assert!(
            open_verified_registry_with_trust(dir.path(), &trust).is_ok(),
            "the certified child is trusted until its signer is revoked"
        );

        let newer = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut second = header_for(
            &primary,
            &newer,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        second
            .revocations
            .push(revocation(&recovery, &second_recovery));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &second, &newer),
            &trust,
            "second",
        )
        .unwrap();

        // Newer than the floor, so only the signer's standing can refuse it.
        let later = sample_db_with_built_at("2026-11-19T12:00:00+00:00");
        let later_header = header_for(
            &child,
            &later,
            "2026-11-19T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&child, &later_header, &later),
        )
        .unwrap();
        assert_refused_as_untrusted_signer(dir.path(), &trust);
    }

    fn key_line(certificate: &KeyCertificate) -> String {
        format!(
            "v1 key {} {} {} {} {}\n",
            certificate.key_id,
            certificate.public_key,
            certificate.role.as_str(),
            certificate.signed_by,
            certificate.signature
        )
    }

    fn revoke_line(revocation: &KeyRevocation) -> String {
        format!(
            "v1 revoke {} {} {}\n",
            revocation.revoked_key_id, revocation.signed_by, revocation.signature
        )
    }

    /// A recovery key revoked in one record cannot authenticate a statement that
    /// first appears in a later record: the log keeps per-bundle boundaries.
    #[test]
    fn a_retired_recovery_key_cannot_sign_in_a_later_record() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(160);
        let stolen = test_key(161);
        let recovery = test_key(162);
        let second_recovery = test_key(163);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let cert = recovery_certificate(&recovery, &test_public(&second_recovery));
        let retire = revocation(&recovery, &second_recovery);
        let late = revocation(&second_recovery, &primary);
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 record 2\n{}{}v1 record 1\n{}",
                key_line(&cert),
                revoke_line(&retire),
                revoke_line(&late)
            ),
        )
        .unwrap();
        let error = effective_trust(dir.path(), &trust)
            .expect_err("a statement signed by a retired key in a later record is not authentic");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");

        // The same statements in one record are one bundle: authentic.
        fs::write(
            paths::registry_trust_state_path(dir.path()).unwrap(),
            format!(
                "v1 record 3\n{}{}{}",
                key_line(&cert),
                revoke_line(&retire),
                revoke_line(&late)
            ),
        )
        .unwrap();
        assert!(effective_trust(dir.path(), &trust).is_ok());
    }

    /// After a cascade, a later ordinary install must still succeed: replay
    /// starts from the compiled keys, never from an already merged set.
    #[test]
    fn an_install_after_a_cascade_still_succeeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(164);
        let stolen = test_key(165);
        let recovery = test_key(166);
        let second_recovery = test_key(167);
        let child = test_key(168);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();

        let mut first = fresh_header(&primary, &payload);
        first.certificates.push(recovery_certificate(
            &recovery,
            &test_public(&second_recovery),
        ));
        first
            .certificates
            .push(certificate(&second_recovery, &test_public(&child)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();

        let second_db = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut second = header_for(
            &primary,
            &second_db,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        second
            .revocations
            .push(revocation(&recovery, &second_recovery));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &second, &second_db),
            &trust,
            "second",
        )
        .unwrap();

        let third_db = sample_db_with_built_at("2026-11-19T12:00:00+00:00");
        let third = header_for(
            &primary,
            &third_db,
            "2026-11-19T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &third, &third_db),
            &trust,
            "third",
        )
        .expect("a normal install after a cascade must not be blocked");
    }

    /// A log that stops in the middle of a line, or in the middle of a record,
    /// is damage: it is never read as a smaller record, and an install refuses to
    /// build on it. Removing the log is the explicit override.
    #[test]
    fn a_log_cut_mid_line_or_mid_record_is_damage() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(169);
        let stolen = test_key(170);
        let recovery = test_key(171);
        let first_new = test_key(172);
        let second_new = test_key(173);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();

        let mut first = fresh_header(&primary, &payload);
        first
            .certificates
            .push(certificate(&recovery, &test_public(&first_new)));
        first.revocations.push(revocation(&recovery, &stolen));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        let intact = fs::read_to_string(&trust_path).unwrap();
        assert!(effective_trust(dir.path(), &trust).is_ok());

        // Cut between two complete lines of the record: a prefix would trust the
        // new key but lose the revocation of the stolen one.
        let lines: Vec<&str> = intact.lines().collect();
        assert!(lines.len() >= 3, "{intact}");
        let cut_between_lines = format!("{}\n{}\n", lines[0], lines[1]);
        // Cut in the middle of a line.
        let cut_mid_line = &intact[..intact.len() - 5];
        for damaged in [cut_between_lines.as_str(), cut_mid_line] {
            fs::write(&trust_path, damaged).unwrap();
            let error = effective_trust(dir.path(), &trust)
                .expect_err("a cut record must be refused, not read as a smaller one");
            assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");

            let second_db = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
            let mut second = header_for(
                &primary,
                &second_db,
                "2026-11-18T12:00:00+00:00",
                "2099-01-01T00:00:00+00:00",
            );
            second
                .certificates
                .push(certificate(&recovery, &test_public(&second_new)));
            let error = install_bundle_bytes_checked(
                dir.path(),
                &signed_bundle(&primary, &second, &second_db),
                &trust,
                "second",
            )
            .expect_err("an install must not build on a damaged log");
            assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
        }

        fs::remove_file(&trust_path).unwrap();
        assert!(effective_trust(dir.path(), &trust).is_ok());
    }

    /// The log is replaced whole, so a failed write leaves the earlier log
    /// byte for byte as it was.
    #[test]
    fn a_failed_log_write_leaves_the_earlier_log_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(176);
        let stolen = test_key(177);
        let recovery = test_key(178);
        let first_new = test_key(179);
        let second_new = test_key(180);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();
        let mut first = fresh_header(&primary, &payload);
        first
            .certificates
            .push(certificate(&recovery, &test_public(&first_new)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        let before = fs::read(&trust_path).unwrap();

        let second_db = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut second = header_for(
            &primary,
            &second_db,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        second
            .certificates
            .push(certificate(&recovery, &test_public(&second_new)));
        let _failing = fail_trust_writes(dir.path());
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &second, &second_db),
            &trust,
            "second",
        )
        .expect_err("a failed write must refuse the install");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
        assert_eq!(fs::read(&trust_path).unwrap(), before);
    }

    /// An empty log is "nothing remembered", not an error.
    #[test]
    fn an_empty_trust_log_means_nothing_remembered() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(174);
        let trust = RegistryTrust::with_key(test_public(&primary));
        fs::write(paths::registry_trust_state_path(dir.path()).unwrap(), b"").unwrap();
        let payload = sample_db();
        let header = fresh_header(&primary, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&primary, &header, &payload),
        )
        .unwrap();
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
    }

    /// The digest path keeps a trust-state failure as a trust-state failure.
    #[test]
    fn the_digest_keeps_the_trust_state_error_code() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(175);
        let trust = RegistryTrust::with_key(test_public(&primary));
        let payload = sample_db();
        let header = fresh_header(&primary, &payload);
        fs::write(
            paths::registry_bundle_path(dir.path()).unwrap(),
            signed_bundle(&primary, &header, &payload),
        )
        .unwrap();
        fs::create_dir_all(paths::registry_trust_state_path(dir.path()).unwrap()).unwrap();
        let error = verified_registry_digest(dir.path(), &trust)
            .expect_err("an unreadable trust log must refuse the digest");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
    }

    /// A remembered descendant must not be able to sign the very bundle that
    /// revokes its ancestor: install and a later reopen have to agree.
    #[test]
    fn a_remembered_descendant_cannot_sign_the_bundle_that_revokes_its_ancestor() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(181);
        let stolen = test_key(182);
        let recovery = test_key(183);
        let second_recovery = test_key(184);
        let descendant = test_key(185);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();

        let mut first = fresh_header(&primary, &payload);
        first.certificates.push(recovery_certificate(
            &recovery,
            &test_public(&second_recovery),
        ));
        first
            .certificates
            .push(certificate(&second_recovery, &test_public(&descendant)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();
        let installed = fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap();

        let newer = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut revoking = header_for(
            &descendant,
            &newer,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        revoking
            .revocations
            .push(revocation(&recovery, &second_recovery));
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&descendant, &revoking, &newer),
            &trust,
            "revoking",
        )
        .expect_err("a key certified through the revoked key cannot sign its revocation");
        assert_eq!(error.code(), "ERR_REGISTRY_SIGNATURE_INVALID");
        assert_eq!(
            fs::read(paths::registry_bundle_path(dir.path()).unwrap()).unwrap(),
            installed,
            "nothing was installed"
        );
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
    }

    /// A broken log during an install is an error, even when the rollback floor
    /// is absent and the installed stamp is all that would have protected us.
    #[test]
    fn a_broken_log_refuses_an_install_rather_than_hiding_the_installed_stamp() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(186);
        let stolen = test_key(187);
        let recovery = test_key(188);
        let new_key = test_key(189);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let newer = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut current = header_for(
            &primary,
            &newer,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        current
            .certificates
            .push(certificate(&recovery, &test_public(&new_key)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &current, &newer),
            &trust,
            "current",
        )
        .unwrap();
        fs::remove_file(dir.path().join("registry.floor")).unwrap();
        let trust_path = paths::registry_trust_state_path(dir.path()).unwrap();
        let intact = fs::read_to_string(&trust_path).unwrap();
        fs::write(&trust_path, &intact[..intact.len() - 5]).unwrap();

        let older = sample_db_with_built_at("2026-11-10T12:00:00+00:00");
        let older_header = header_for(
            &primary,
            &older,
            "2026-11-10T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let error = install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &older_header, &older),
            &trust,
            "older",
        )
        .expect_err("an older bundle must not install over a log we cannot trust");
        assert_eq!(error.code(), "ERR_REGISTRY_TRUST_STATE");
    }

    /// A signer remembered from the log can sign a catalog that arrives as a
    /// delta: the delta check uses the same snapshot as everything else, so a
    /// remembered key does not force a full download.
    #[tokio::test]
    async fn a_remembered_signer_can_sign_a_delta_catalog() {
        let (dir, ls_path, locks) = setup_sync_env();
        let primary = test_key(190);
        let stolen = test_key(191);
        let recovery = test_key(192);
        let remembered = test_key(193);
        let trust = rotation_trust(&primary, &stolen, &recovery);

        let first_payload = sample_db_with_built_at("2026-11-17T02:00:00+00:00");
        let mut first = header_for(
            &primary,
            &first_payload,
            "2026-11-17T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        first
            .certificates
            .push(certificate(&recovery, &test_public(&remembered)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &first_payload),
            &trust,
            "first",
        )
        .unwrap();
        let old_payload = sample_db_with_built_at("2026-11-18T02:00:00+00:00");
        let old_header = header_for(
            &remembered,
            &old_payload,
            "2026-11-18T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&remembered, &old_header, &old_payload),
            &trust,
            "second",
        )
        .unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-18").unwrap();
        set_cached_asset_id(&ls_path, Some(2999)).unwrap();

        let new_payload = sample_db_with_built_at("2026-11-19T02:00:00+00:00");
        let new_header = header_for(
            &remembered,
            &new_payload,
            "2026-11-19T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let new_bundle = signed_bundle(&remembered, &new_header, &new_payload);
        let prefix = new_bundle[..new_bundle.len() - new_payload.len()].to_vec();
        let mut encoder =
            zstd::stream::write::Encoder::with_dictionary(Vec::new(), 19, &old_payload).unwrap();
        encoder.include_contentsize(true).unwrap();
        encoder
            .set_pledged_src_size(Some(new_payload.len() as u64))
            .unwrap();
        std::io::Write::write_all(&mut encoder, &new_payload).unwrap();
        let delta = encoder.finish().unwrap();
        let delta_name = registry_bundle::delta_asset_name(&old_header.db_sha256).unwrap();

        let recorded = format!(
            r#"[{{"tag_name": "registry-2026-11-19", "draft": false, "prerelease": false,
                "assets": [{{"id": 3001, "name": "registry.bundle"}},
                           {{"id": 3002, "name": "registry.header"}},
                           {{"id": 3003, "name": "{delta_name}"}}]}}]"#
        );
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(&recorded))]);
        // The full bundle is never queued: asking for it would panic.
        transport.push_asset_ok(3002, prefix);
        transport.push_asset_ok(3003, delta);

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
        .expect("a remembered signer's delta must install without a full download");
        assert_eq!(status.cached_tag.as_deref(), Some("registry-2026-11-19"));
    }

    /// A surviving recovery key re-certifies a remembered key and revokes the
    /// key that first certified it, in one bundle. The new certificate must be
    /// remembered too, or the key stops chaining after a reload.
    #[test]
    fn a_re_certified_key_still_chains_after_its_old_ancestor_is_revoked() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = test_key(194);
        let stolen = test_key(195);
        let recovery = test_key(196);
        let second_recovery = test_key(197);
        let signer = test_key(198);
        let trust = rotation_trust(&primary, &stolen, &recovery);
        let payload = sample_db();

        let mut first = fresh_header(&primary, &payload);
        first.certificates.push(recovery_certificate(
            &recovery,
            &test_public(&second_recovery),
        ));
        first
            .certificates
            .push(certificate(&second_recovery, &test_public(&signer)));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &payload),
            &trust,
            "first",
        )
        .unwrap();

        let second_db = sample_db_with_built_at("2026-11-18T12:00:00+00:00");
        let mut second = header_for(
            &signer,
            &second_db,
            "2026-11-18T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        second
            .certificates
            .push(certificate(&recovery, &test_public(&signer)));
        second
            .revocations
            .push(revocation(&recovery, &second_recovery));
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&signer, &second, &second_db),
            &trust,
            "second",
        )
        .unwrap();

        let third_db = sample_db_with_built_at("2026-11-19T12:00:00+00:00");
        let third = header_for(
            &signer,
            &third_db,
            "2026-11-19T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&signer, &third, &third_db),
            &trust,
            "third",
        )
        .expect("the re-certified key still chains after a reload");
        assert!(open_verified_registry_with_trust(dir.path(), &trust).is_ok());
    }

    /// Sync loads its trust only once it holds `RegistryUpdate`: a local install
    /// that learns a signer while sync waits must be visible to sync.
    #[tokio::test]
    async fn sync_judges_a_catalog_with_the_trust_learned_while_it_waited() {
        let (dir, ls_path, locks) = setup_sync_env();
        let primary = test_key(199);
        let stolen = test_key(200);
        let recovery = test_key(201);
        let learned = test_key(202);
        let trust = rotation_trust(&primary, &stolen, &recovery);

        let first_payload = sample_db_with_built_at("2026-11-17T02:00:00+00:00");
        let first = header_for(
            &primary,
            &first_payload,
            "2026-11-17T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        install_bundle_bytes_checked(
            dir.path(),
            &signed_bundle(&primary, &first, &first_payload),
            &trust,
            "first",
        )
        .unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-17").unwrap();
        set_cached_asset_id(&ls_path, Some(2999)).unwrap();

        // The catalog sync will fetch is signed by a key only the local install
        // below teaches us.
        let newest = sample_db_with_built_at("2026-11-19T02:00:00+00:00");
        let newest_header = header_for(
            &learned,
            &newest,
            "2026-11-19T02:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let newest_bundle = signed_bundle(&learned, &newest_header, &newest);
        let recorded = r#"[{"tag_name": "registry-2026-11-19", "draft": false,
            "prerelease": false, "assets": [{"id": 3001, "name": "registry.bundle"}]}]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(3001, newest_bundle);

        let held = locks
            .acquire(crate::lock_manager::LockResource::RegistryUpdate, "test")
            .unwrap();
        let sync = check_and_download_update_inner(
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
        );
        let local_install = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let second_payload = sample_db_with_built_at("2026-11-18T02:00:00+00:00");
            let mut second = header_for(
                &primary,
                &second_payload,
                "2026-11-18T02:00:00+00:00",
                "2099-01-01T00:00:00+00:00",
            );
            second
                .certificates
                .push(certificate(&recovery, &test_public(&learned)));
            install_bundle_bytes_checked(
                dir.path(),
                &signed_bundle(&primary, &second, &second_payload),
                &trust,
                "second",
            )
            .unwrap();
            drop(held);
        };
        let (status, ()) = tokio::join!(sync, local_install);
        let status = status.expect("sync must use the trust learned while it waited");
        assert_eq!(status.cached_tag.as_deref(), Some("registry-2026-11-19"));
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

    /// The digest helper must not materialize the database of a below-floor
    /// bundle, any more than a plain open may use one.
    #[test]
    fn the_verified_digest_refuses_a_below_floor_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = test_key(127);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-15T12:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-15T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        write_bundle(dir.path(), "registry.bundle", &key, &header, &payload);
        fs::write(
            registry_floor_path(dir.path()).unwrap(),
            format!("2026-11-17T12:00:00+00:00 {}\n", "0".repeat(64)),
        )
        .unwrap();
        assert!(!paths::registry_db_path(dir.path()).unwrap().exists());

        let error = match verified_registry_digest(dir.path(), &trust) {
            Ok(_) => panic!("a below-floor bundle has no usable digest"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "ERR_REGISTRY_ROLLBACK");
        assert!(
            !paths::registry_db_path(dir.path()).unwrap().exists(),
            "the digest helper must not extract a below-floor database"
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

    /// An identical download skips extraction. Sync must report the real
    /// readiness, not a hardcoded `true`, when the readable database is gone.
    #[tokio::test]
    async fn an_identical_download_reports_the_real_readiness() {
        let (dir, ls_path, locks) = setup_sync_env();
        let key = test_key(111);
        let trust = RegistryTrust::with_key(test_public(&key));
        let payload = sample_db_with_built_at("2026-11-16T12:00:00+00:00");
        let header = header_for(
            &key,
            &payload,
            "2026-11-16T12:00:00+00:00",
            "2099-01-01T00:00:00+00:00",
        );
        let bundle = signed_bundle(&key, &header, &payload);
        install_bundle_bytes(dir.path(), &header, &bundle, &trust).unwrap();
        set_cached_tag(&ls_path, "registry-2026-11-16").unwrap();
        set_cached_asset_id(&ls_path, Some(7000)).unwrap();
        fs::remove_file(paths::registry_db_path(dir.path()).unwrap()).unwrap();
        assert!(!get_status_with_trust(dir.path(), &ls_path, &trust).has_cached_db);

        let recorded = r#"[
            {"tag_name": "registry-2026-11-17", "draft": false, "prerelease": false, "assets": [{"id": 7001, "name": "registry.bundle"}]}
        ]"#;
        let transport =
            FakeRegistryTransport::from_pages(vec![(1, release_json_to_list(recorded))]);
        transport.push_asset_ok(7001, bundle.clone());

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
        .expect("an identical candidate is not a failure");
        assert!(
            !status.has_cached_db,
            "sync must not claim readiness when extraction was skipped and the DB is missing"
        );
        assert!(status.message.contains("up to date"), "{}", status.message);
    }
}
