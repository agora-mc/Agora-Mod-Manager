//! Downloads from the loader catalog's allowlist. Installer jars use
//! catalog pins, and generated profile libraries use Maven checksums.

use crate::loader_manifests;
use agora_core::download::{sha1_hex, sha256_hex, sha512_hex};
use agora_core::error::{LauncherError, LauncherResult};
use agora_core::http_client::{self, ClientCategory, HttpClients};
use agora_core::network::sanitized_url_for_log;

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_LOADER_ORIGIN: (reqwest::Client, String);
}

/// Compute a raw SHA-256 for a pinned loader file.
pub fn compute_loader_hash(
    _loader: &str,
    _file_name: &str,
    _file_type: &str,
    data: &[u8],
) -> String {
    sha256_hex(data)
}

/// Download bytes from a URL using a redirect-safe client.
///
/// Redirects are only followed when the target host is on the embedded loader
/// domain allowlist, preventing SSRF via compromised/malicious pinned hosts.
pub async fn download_bytes(url: &str) -> LauncherResult<Vec<u8>> {
    loader_manifests::ensure_allowed_domain(url).inspect_err(|_error| {
        eprintln!(
            "[loader-download] rejected stage=initial-allowlist url={}",
            sanitized_url_for_log(url)
        );
    })?;
    let clients = HttpClients::new()?;
    http_client::checked_get_bytes(&clients, ClientCategory::Loader, url).await
}

/// Download bytes through an already initialized category-aware client set.
pub async fn download_bytes_with_clients(
    clients: &HttpClients,
    url: &str,
) -> LauncherResult<Vec<u8>> {
    profile_response_bytes(clients, url, false)
        .await?
        .ok_or_else(|| LauncherError::Generic {
            code: "ERR_HTTP_STATUS".into(),
            message: format!("Missing loader download from {url}"),
        })
}

async fn profile_response_bytes(
    clients: &HttpClients,
    url: &str,
    optional: bool,
) -> LauncherResult<Option<Vec<u8>>> {
    loader_manifests::ensure_allowed_domain(url)?;
    let parsed = reqwest::Url::parse(url).map_err(|_| LauncherError::UntrustedSource)?;
    let host = parsed.host_str().ok_or(LauncherError::UntrustedSource)?;
    #[cfg(test)]
    let test_origin = TEST_LOADER_ORIGIN.try_with(Clone::clone).ok();
    #[cfg(not(test))]
    let test_origin: Option<(reqwest::Client, String)> = None;
    let response = if let Some((client, origin)) = &test_origin {
        client
            .get(format!("{origin}{}", parsed.path()))
            .send()
            .await
            .map_err(|error| LauncherError::Generic {
                code: "ERR_TEST_HTTP".into(),
                message: error.to_string(),
            })?
    } else {
        let response = http_client::checked_request_with_policy(
            clients,
            ClientCategory::Loader,
            url,
            http_client::HostPolicy::SignedManifest(host),
        )
        .await?;
        loader_manifests::ensure_allowed_domain(response.url().as_str())?;
        response
    };
    if optional && response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(LauncherError::Generic {
            code: "ERR_HTTP_STATUS".into(),
            message: format!(
                "HTTP {} for {}",
                response.status(),
                sanitized_url_for_log(url)
            ),
        });
    }
    let bytes = http_client::checked_response_bytes(response, ClientCategory::Loader).await?;
    if bytes.is_empty() {
        return Err(LauncherError::Generic {
            code: "ERR_LOADER_EMPTY_DOWNLOAD".into(),
            message: format!("Empty loader download from {url}"),
        });
    }
    Ok(Some(bytes))
}

/// Use the strongest published checksum. Only a 404 permits trying a weaker
/// checksum or accepting an explicit missing-checksum override.
pub(crate) async fn download_maven_library(
    clients: &HttpClients,
    url: &str,
    allow_unverified: bool,
) -> LauncherResult<(Vec<u8>, bool)> {
    let algorithms = [
        ("sha512", "SHA-512", 128, sha512_hex as fn(&[u8]) -> String),
        ("sha256", "SHA-256", 64, sha256_hex),
        ("sha1", "SHA-1", 40, sha1_hex),
    ];
    for (extension, name, length, digest) in algorithms {
        let sidecar = format!("{url}.{extension}");
        let Some(text) = profile_response_bytes(clients, &sidecar, true).await? else {
            continue;
        };
        let invalid = || LauncherError::Generic {
            code: "ERR_LOADER_LIBRARY_CHECKSUM".into(),
            message: format!("Invalid published {name} checksum in {sidecar}"),
        };
        let mut fields = std::str::from_utf8(&text)
            .map_err(|_| invalid())?
            .split_whitespace();
        let expected = fields
            .next()
            .filter(|hash| hash.len() == length && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(invalid)?;
        let filename = url.rsplit('/').next().unwrap_or(url);
        if fields
            .next()
            .is_some_and(|field| field.trim_start_matches('*') != filename)
            || fields.next().is_some()
        {
            return Err(invalid());
        }
        let bytes = download_bytes_with_clients(clients, url).await?;
        if !digest(&bytes).eq_ignore_ascii_case(expected) {
            return Err(LauncherError::Generic {
                code: "ERR_HASH_MISMATCH".into(),
                message: format!("{name} mismatch for {url}"),
            });
        }
        return Ok((bytes, true));
    }

    if !allow_unverified {
        return Err(LauncherError::Generic {
            code: "ERR_UNVERIFIED_LOADER_LIBRARY".into(),
            message: format!(
                "{url} has no published hash. Use --allow-unverified-loader to proceed with this download."
            ),
        });
    }
    Ok((download_bytes_with_clients(clients, url).await?, false))
}

/// Download a loader file and verify its hash against the pinned value.
pub async fn download_verified(
    loader: &str,
    file_name: &str,
    file_type: &str,
    url: &str,
    expected_sha: &str,
) -> LauncherResult<Vec<u8>> {
    loader_manifests::ensure_allowed_domain(url).inspect_err(|_error| {
        eprintln!(
            "[loader-download] rejected stage=verified-initial loader={loader} file={file_name} url={}",
            sanitized_url_for_log(url)
        );
    })?;
    let data = download_bytes(url).await?;
    let actual = compute_loader_hash(loader, file_name, file_type, &data);

    if actual != loader_manifests::strip_sha_prefix(expected_sha) {
        return Err(LauncherError::HashMismatch);
    }
    Ok(data)
}

/// Download and verify a pinned loader file with shared core clients.
pub async fn download_verified_with_clients(
    clients: &HttpClients,
    loader: &str,
    file_name: &str,
    file_type: &str,
    url: &str,
    expected_sha: &str,
) -> LauncherResult<Vec<u8>> {
    loader_manifests::ensure_allowed_domain(url)?;
    let data = http_client::checked_get_bytes(clients, ClientCategory::Loader, url).await?;
    let actual = compute_loader_hash(loader, file_name, file_type, &data);
    if actual != loader_manifests::strip_sha_prefix(expected_sha) {
        return Err(LauncherError::HashMismatch);
    }
    Ok(data)
}
