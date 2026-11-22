//! Downloads from the loader catalog's allowlist. Installer jars use
//! catalog pins, and generated profile libraries use Maven checksums.

use crate::launch::LibraryArtifact;
use crate::loader_manifests;
use agora_core::{
    artifact_hash::verify_download,
    download::{sha1_hex, sha256_hex, sha512_hex},
    error::{LauncherError, LauncherResult},
    http_client::{self, ClientCategory, HttpClients},
    network::sanitized_url_for_log,
};

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

/// Download one library of a generated loader profile and check it against
/// every hash its source published: the repository's `.jar.sha512`,
/// `.jar.sha256` or `.jar.sha1` sidecars (strongest first), and the digests
/// the profile itself carries on the library.
///
/// Only a sidecar 404 permits trying a weaker one, falling back to the
/// profile's own digests, or accepting an explicit missing-checksum override.
pub(crate) async fn download_maven_library(
    clients: &HttpClients,
    artifact: &LibraryArtifact,
    allow_unverified: bool,
) -> LauncherResult<(Vec<u8>, bool)> {
    let url = artifact.url.trim();
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
        verify_profile_digests(artifact, &bytes)?;
        return Ok((bytes, true));
    }

    // No sidecar. A profile that carries the library's own digests (Fabric
    // publishes SHA-1, SHA-256 and SHA-512; Quilt publishes nothing) has
    // already published a hash for it, so the bytes are checked against those
    // rather than treated as unhashed.
    if !DeclaredDigests::of(artifact).is_empty() {
        let bytes = download_bytes_with_clients(clients, url).await?;
        verify_profile_digests(artifact, &bytes)?;
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

/// The non-blank digests a profile publishes for a library.
///
/// Every algorithm a generated profile can carry is kept, because a profile
/// that publishes more than one is checked against all of them: a SHA-1 alone
/// is weak, and the resolution already merged the digests the library entry and
/// its `downloads.artifact` publish. A blank digest is not a hash — the same
/// rule [`agora_core::artifact_hash`] applies to a blank published digest
/// anywhere else.
#[derive(Clone, Copy, Default)]
struct DeclaredDigests<'a> {
    sha1: Option<&'a str>,
    sha256: Option<&'a str>,
    sha512: Option<&'a str>,
}

impl<'a> DeclaredDigests<'a> {
    fn of(artifact: &'a LibraryArtifact) -> Self {
        fn non_blank(value: &Option<String>) -> Option<&str> {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        }
        Self {
            sha1: non_blank(&artifact.sha1),
            sha256: non_blank(&artifact.sha256),
            sha512: non_blank(&artifact.sha512),
        }
    }

    fn is_empty(&self) -> bool {
        self.sha1.is_none() && self.sha256.is_none() && self.sha512.is_none()
    }

    /// The published algorithms, for a refusal that says what it checked.
    fn names(&self) -> String {
        [
            (self.sha1, "SHA-1"),
            (self.sha256, "SHA-256"),
            (self.sha512, "SHA-512"),
        ]
        .into_iter()
        .filter_map(|(digest, name)| digest.map(|_| name))
        .collect::<Vec<_>>()
        .join("/")
    }
}

/// Check downloaded bytes against the digests the loader profile publishes.
///
/// These are the source's own published hashes, so a mismatch is never
/// overridable: `--allow-unverified-loader` is about a source that published
/// nothing, not about one whose digest disagrees with its bytes.
fn verify_profile_digests(artifact: &LibraryArtifact, bytes: &[u8]) -> LauncherResult<()> {
    let declared = DeclaredDigests::of(artifact);
    if declared.is_empty() {
        return Ok(());
    }
    verify_download(
        bytes,
        declared.sha1,
        declared.sha256,
        declared.sha512,
        &[],
        false,
        &artifact.path,
        None,
    )
    .map(|_| ())
    .map_err(|error| match error {
        LauncherError::HashMismatch => LauncherError::Generic {
            code: "ERR_HASH_MISMATCH".into(),
            message: format!(
                "{} does not match the {} its loader profile publishes",
                artifact.url,
                declared.names()
            ),
        },
        other => other,
    })
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
