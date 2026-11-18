//! The signed catalog bundle.
//!
//! A catalog travels and is replaced as one file: a small signed header that
//! names the key which signed it, the schema version, the build and expiry
//! times, and the SHA-256 and length of the database that follows. The database
//! bytes are the payload. There is no second signature file, so a crash can
//! never leave a database and a signature that describe different builds.
//!
//! Header layout (all integers little-endian):
//!
//! ```text
//! magic           16 bytes  b"AGORA-CATALOG-V1"
//! header length    4 bytes  u32
//! header           n bytes  canonical JSON, no signature field
//! signature       64 bytes  Ed25519 over SIGN_DOMAIN || header
//! payload          m bytes  the SQLite catalog
//! ```
//!
//! The client trusts a compiled-in list of public keys, each with an id. A
//! bundle names its signing key by id. A new key can be introduced by a
//! certificate inside the bundle: a statement, signed by a key the client
//! already trusts, that another key id belongs to a given public key.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Marks a catalog bundle. Exactly [`MAGIC_LEN`] bytes.
pub const MAGIC: &[u8; MAGIC_LEN] = b"AGORA-CATALOG-V1";
pub const MAGIC_LEN: usize = 16;
/// A bundle must have room for the magic, header length, header and signature.
const MIN_BUNDLE_LEN: usize = MAGIC_LEN + 4 + 2 + SIGNATURE_LENGTH;
/// Domain separation for the header signature.
pub const SIGN_DOMAIN: &[u8] = b"AGORA-CATALOG-HEADER-V1\n";
/// Domain separation for a key certificate.
pub const CERT_DOMAIN: &[u8] = b"AGORA-CATALOG-KEY-CERT-V1\n";
/// Domain separation for a key revocation statement.
pub const REVOKE_DOMAIN: &[u8] = b"AGORA-CATALOG-KEY-REVOKE-V1\n";
/// The only bundle format this build understands.
pub const BUNDLE_FORMAT: u32 = 1;

/// The keys compiled into the client when no build-time override is set.
///
/// Kept in `registry-keys.json` so `scripts/check_registry_keys.py` can read the
/// same list a release build compiles in. The recovery slot is deliberately a
/// placeholder until the owner generates the offline recovery key; the release
/// check refuses a build with fewer than two valid keys.
const DEFAULT_REGISTRY_KEYS_JSON: &str = include_str!("../registry-keys.json");

/// What a key is allowed to sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryKeyRole {
    /// Signs catalogs in CI; may not certify or revoke another key.
    Primary,
    /// Kept offline; signs certificates and revocation statements, never a
    /// catalog.
    Recovery,
}

impl RegistryKeyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Recovery => "recovery",
        }
    }
}

/// One trusted public key: the id is derived from the key, so operators never
/// have to keep two names in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryKey {
    pub id: String,
    pub public_key_hex: String,
    pub role: RegistryKeyRole,
}

impl RegistryKey {
    pub fn from_hex(public_key_hex: impl Into<String>) -> Self {
        let public_key_hex = public_key_hex.into();
        let id = key_id_for_public_key(&public_key_hex);
        Self {
            id,
            public_key_hex,
            role: RegistryKeyRole::Primary,
        }
    }

    pub fn with_role(mut self, role: RegistryKeyRole) -> Self {
        self.role = role;
        self
    }
}

/// The key ids a client trusts, and whether a debug build may open an unsigned
/// catalog (a test convenience; never true in a release build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryTrust {
    keys: Vec<RegistryKey>,
    /// Verified revocation statements remembered from earlier catalogs. This
    /// list only grows.
    revocations: Vec<KeyRevocation>,
    permit_unsigned_debug: bool,
}

impl Default for RegistryTrust {
    fn default() -> Self {
        Self::production()
    }
}

impl RegistryTrust {
    /// The keys compiled into this build.
    pub fn production() -> Self {
        let keys = compiled_public_keys()
            .into_iter()
            .map(|(role, key)| RegistryKey::from_hex(key).with_role(role))
            .collect();
        Self {
            keys,
            revocations: Vec::new(),
            permit_unsigned_debug: false,
        }
    }

    /// Test support: trust exactly this key.
    pub fn with_key(public_key_hex: impl Into<String>) -> Self {
        Self {
            keys: vec![RegistryKey::from_hex(public_key_hex)],
            revocations: Vec::new(),
            permit_unsigned_debug: false,
        }
    }

    /// Test support: trust exactly these keys, all as primaries.
    pub fn with_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            keys: keys
                .into_iter()
                .map(|key| RegistryKey::from_hex(key.into()))
                .collect(),
            revocations: Vec::new(),
            permit_unsigned_debug: false,
        }
    }

    /// Test support: trust these keys with explicit roles.
    pub fn with_roles<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = (RegistryKeyRole, S)>,
        S: Into<String>,
    {
        Self {
            keys: keys
                .into_iter()
                .map(|(role, key)| RegistryKey::from_hex(key.into()).with_role(role))
                .collect(),
            revocations: Vec::new(),
            permit_unsigned_debug: false,
        }
    }

    /// Test/debug support: accept a catalog with no signature.
    ///
    /// In a release build this is still the empty trust set, which trusts
    /// nothing, because the flag only exists under `debug_assertions`.
    pub fn unverified_debug() -> Self {
        Self {
            keys: Vec::new(),
            revocations: Vec::new(),
            permit_unsigned_debug: cfg!(debug_assertions),
        }
    }

    /// Add keys a verified catalog remembered, and the revocations it taught
    /// us. Revocations always win over keys.
    pub fn merge_remembered(
        &mut self,
        keys: impl IntoIterator<Item = RegistryKey>,
        revocations: impl IntoIterator<Item = KeyRevocation>,
    ) {
        for key in keys {
            if !self.keys.iter().any(|known| known.id == key.id) {
                self.keys.push(key);
            }
        }
        for revocation in revocations {
            if !self.revocations.iter().any(|known| known == &revocation) {
                self.revocations.push(revocation);
            }
        }
    }

    pub fn keys(&self) -> &[RegistryKey] {
        &self.keys
    }

    pub fn revocations(&self) -> &[KeyRevocation] {
        &self.revocations
    }

    pub fn revoked_ids(&self) -> Vec<String> {
        self.revocations
            .iter()
            .map(|revocation| revocation.revoked_key_id.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// A stable name for this key set, used to key the verified-header cache.
    pub fn fingerprint(&self) -> String {
        let mut ids: Vec<String> = self
            .keys
            .iter()
            .map(|key| format!("{}:{}", key.id, key.role.as_str()))
            .collect();
        ids.sort_unstable();
        let mut revoked: Vec<String> = self
            .revocations
            .iter()
            .map(|revocation| {
                format!(
                    "{}:{}:{}",
                    revocation.revoked_key_id, revocation.signed_by, revocation.signature
                )
            })
            .collect();
        revoked.sort_unstable();
        format!("{}|{}", ids.join(","), revoked.join(","))
    }

    /// Whether an unsigned legacy catalog may be opened. Only ever true in a
    /// debug build that explicitly asked for it.
    pub fn permits_unsigned_debug(&self) -> bool {
        self.permit_unsigned_debug && cfg!(debug_assertions)
    }
}

fn compiled_public_keys() -> Vec<(RegistryKeyRole, String)> {
    let split = |raw: &str| -> Vec<(RegistryKeyRole, String)> {
        raw.split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .enumerate()
            .map(|(index, key)| {
                let role = if index == 0 {
                    RegistryKeyRole::Primary
                } else {
                    RegistryKeyRole::Recovery
                };
                (role, key.to_string())
            })
            .collect()
    };
    if let Some(raw) = option_env!("AGORA_REGISTRY_PUBKEYS") {
        if !raw.trim().is_empty() {
            return split(raw);
        }
    }
    if let Some(raw) = option_env!("AGORA_REGISTRY_PUBKEY") {
        if !raw.trim().is_empty() {
            return split(raw);
        }
    }
    default_public_keys()
}

fn default_public_keys() -> Vec<(RegistryKeyRole, String)> {
    #[derive(serde::Deserialize)]
    struct CompiledKey {
        role: String,
        public_key_hex: String,
    }
    let keys: Vec<CompiledKey> =
        serde_json::from_str(DEFAULT_REGISTRY_KEYS_JSON).unwrap_or_default();
    keys.into_iter()
        .filter_map(|key| {
            let role = match key.role.as_str() {
                "primary" => RegistryKeyRole::Primary,
                "recovery" => RegistryKeyRole::Recovery,
                _ => return None,
            };
            Some((role, key.public_key_hex))
        })
        .collect()
}

/// The stable id of a public key: the first 16 hex characters of its SHA-256.
///
/// A malformed key still gets an id so a bad configured key fails at
/// verification, not by disappearing from the trust list without a word.
pub fn key_id_for_public_key(public_key_hex: &str) -> String {
    let digest = match hex::decode(public_key_hex.trim()) {
        Ok(bytes) => Sha256::digest(&bytes),
        Err(_) => Sha256::digest(public_key_hex.trim().as_bytes()),
    };
    hex::encode(&digest[..8])
}

/// One statement that `key_id` belongs to `public_key` with `role`, signed by
/// the recovery key `signed_by`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyCertificate {
    pub key_id: String,
    pub public_key: String,
    /// The role the introduced key is trusted with. Part of the signed bytes.
    pub role: RegistryKeyRole,
    pub signed_by: String,
    /// Lowercase hex, 64 bytes.
    pub signature: String,
}

impl KeyCertificate {
    /// The exact bytes a certificate signature covers.
    pub fn signed_bytes(
        key_id: &str,
        public_key: &str,
        role: RegistryKeyRole,
        signed_by: &str,
    ) -> Vec<u8> {
        let mut message = CERT_DOMAIN.to_vec();
        message.extend_from_slice(key_id.trim().as_bytes());
        message.push(b'\n');
        message.extend_from_slice(public_key.trim().as_bytes());
        message.push(b'\n');
        message.extend_from_slice(role.as_str().as_bytes());
        message.push(b'\n');
        message.extend_from_slice(signed_by.trim().as_bytes());
        message
    }
}

/// One statement that `revoked_key_id` must no longer be trusted, signed by the
/// recovery key `signed_by`. A key may not revoke itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyRevocation {
    pub revoked_key_id: String,
    pub signed_by: String,
    /// Lowercase hex, 64 bytes.
    pub signature: String,
}

impl KeyRevocation {
    /// The exact bytes a revocation signature covers.
    pub fn signed_bytes(revoked_key_id: &str, signed_by: &str) -> Vec<u8> {
        let mut message = REVOKE_DOMAIN.to_vec();
        message.extend_from_slice(revoked_key_id.trim().as_bytes());
        message.push(b'\n');
        message.extend_from_slice(signed_by.trim().as_bytes());
        message
    }
}

/// The signed description of a catalog payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogHeader {
    pub format: u32,
    /// The id of the key that signed this header.
    pub key_id: String,
    pub schema_version: i64,
    /// The release tag, for display. Never trusted for ordering.
    #[serde(default)]
    pub tag: String,
    /// When the catalog was compiled, ISO-8601 UTC. Drives rollback refusal.
    pub built_at: String,
    /// When this catalog was signed, ISO-8601 UTC.
    pub issued_at: String,
    /// When this catalog stops being current, ISO-8601 UTC.
    pub expires_at: String,
    pub db_len: u64,
    /// Lowercase hex SHA-256 of the payload.
    pub db_sha256: String,
    /// Keys this bundle introduces, each signed by an already-trusted key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificates: Vec<KeyCertificate>,
    /// Revocations carried by this catalog. Each is a statement signed by a
    /// recovery key, not a bare id the catalog signer can invent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revocations: Vec<KeyRevocation>,
}

impl CatalogHeader {
    pub fn built_at_time(&self) -> Option<DateTime<Utc>> {
        parse_utc(&self.built_at)
    }

    pub fn issued_at_time(&self) -> Option<DateTime<Utc>> {
        parse_utc(&self.issued_at)
    }

    pub fn expires_at_time(&self) -> Option<DateTime<Utc>> {
        parse_utc(&self.expires_at)
    }

    /// A catalog is stale once its signed expiry has passed. It stays readable.
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at_time() {
            Some(expires) => now > expires,
            // An unparseable expiry is not a licence to treat it as current.
            None => true,
        }
    }
}

fn parse_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Why a bundle could not be used. None of these is "the catalog is empty".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    #[error("the catalog bundle is empty")]
    Empty,
    #[error("the catalog bundle is truncated: {0}")]
    Truncated(&'static str),
    #[error("the file is not an Agora catalog bundle")]
    BadMagic,
    #[error("the catalog bundle header is too large")]
    HeaderTooLarge,
    #[error("the catalog bundle header is not UTF-8")]
    HeaderNotUtf8,
    #[error("the catalog bundle header is not readable: {0}")]
    HeaderNotJson(String),
    #[error("catalog bundle format {0} is not supported by this app")]
    UnsupportedFormat(u32),
    #[error("the catalog bundle header is missing {0}")]
    MissingField(&'static str),
    #[error("the catalog bundle header has an invalid {0}")]
    InvalidField(&'static str),
    #[error("the catalog bundle signature does not verify")]
    SignatureInvalid,
    #[error("the catalog was signed by unknown key {0}")]
    UnknownKey(String),
    #[error("the catalog was signed by revoked key {0}")]
    RevokedKey(String),
    #[error("a catalog key certificate is invalid: {0}")]
    CertificateInvalid(String),
    #[error(
        "the catalog was signed by recovery key {0}, which may only sign a certificate or revocation"
    )]
    KeyNotForCatalog(String),
    #[error("the catalog payload length does not match the signed header")]
    PayloadLength,
    #[error("the catalog payload does not match the signed SHA-256")]
    PayloadHash,
}

/// A parsed but not yet trusted bundle. The slices point into the input.
#[derive(Debug)]
pub struct ParsedBundle<'a> {
    pub header: CatalogHeader,
    pub header_bytes: &'a [u8],
    pub signature: [u8; SIGNATURE_LENGTH],
    pub payload: &'a [u8],
}

/// Read the structure of a bundle without checking its signature.
pub fn parse_bundle(bytes: &[u8]) -> Result<ParsedBundle<'_>, BundleError> {
    if bytes.is_empty() {
        return Err(BundleError::Empty);
    }
    if bytes.len() < MIN_BUNDLE_LEN {
        return Err(BundleError::Truncated("header"));
    }
    if &bytes[..MAGIC_LEN] != MAGIC {
        return Err(BundleError::BadMagic);
    }
    let len_at = MAGIC_LEN;
    let header_len = u32::from_le_bytes(
        bytes[len_at..len_at + 4]
            .try_into()
            .map_err(|_| BundleError::Truncated("header length"))?,
    ) as usize;
    if header_len == 0 {
        return Err(BundleError::MissingField("header"));
    }
    // A header is a few hundred bytes. Refuse an absurd length before slicing.
    if header_len > 1024 * 1024 {
        return Err(BundleError::HeaderTooLarge);
    }
    let header_start = len_at + 4;
    let signature_start = header_start
        .checked_add(header_len)
        .ok_or(BundleError::Truncated("header"))?;
    let payload_start = signature_start + SIGNATURE_LENGTH;
    if payload_start > bytes.len() {
        return Err(BundleError::Truncated("signature"));
    }
    let header_bytes = &bytes[header_start..signature_start];
    let signature: [u8; SIGNATURE_LENGTH] = bytes[signature_start..payload_start]
        .try_into()
        .map_err(|_| BundleError::Truncated("signature"))?;
    let payload = &bytes[payload_start..];

    let header: CatalogHeader = serde_json::from_slice(header_bytes)
        .map_err(|error| BundleError::HeaderNotJson(error.to_string()))?;
    if header.format != BUNDLE_FORMAT {
        return Err(BundleError::UnsupportedFormat(header.format));
    }
    if header.key_id.trim().is_empty() {
        return Err(BundleError::MissingField("key id"));
    }
    if header.built_at.trim().is_empty() {
        return Err(BundleError::MissingField("built_at"));
    }
    if header.issued_at.trim().is_empty() {
        return Err(BundleError::MissingField("issued_at"));
    }
    if header.expires_at.trim().is_empty() {
        return Err(BundleError::MissingField("expires_at"));
    }
    if header.built_at_time().is_none() || header.issued_at_time().is_none() {
        return Err(BundleError::InvalidField("timestamp"));
    }
    if header.expires_at_time().is_none() {
        return Err(BundleError::InvalidField("expiry"));
    }
    if header.db_sha256.len() != 64 || !header.db_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(BundleError::InvalidField("payload SHA-256"));
    }
    if header.db_len != payload.len() as u64 {
        return Err(BundleError::PayloadLength);
    }
    Ok(ParsedBundle {
        header,
        header_bytes,
        signature,
        payload,
    })
}

/// The exact bytes a bundle header signature covers.
pub fn header_signing_bytes(header_bytes: &[u8]) -> Vec<u8> {
    let mut message = SIGN_DOMAIN.to_vec();
    message.extend_from_slice(header_bytes);
    message
}

/// Serialize a header for signing. The compiler writes this JSON itself; the
/// bytes it produces are what gets signed, so this is a test convenience.
pub fn header_bytes(header: &CatalogHeader) -> Vec<u8> {
    serde_json::to_vec(header).expect("a catalog header always serializes")
}

/// Wrap a signed header and its payload into the on-disk single-file form.
pub fn encode_bundle(
    header_bytes: &[u8],
    signature: &[u8; SIGNATURE_LENGTH],
    payload: &[u8],
) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(MAGIC_LEN + 4 + header_bytes.len() + SIGNATURE_LENGTH + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(header_bytes);
    out.extend_from_slice(signature);
    out.extend_from_slice(payload);
    out
}

/// A bundle whose signature and payload hash have both been checked.
#[derive(Debug, Clone)]
pub struct VerifiedBundle {
    pub header: CatalogHeader,
    pub payload: Vec<u8>,
    /// Certificates this verification actually accepted, so the client can
    /// remember the keys they introduced. A certificate signed by an unknown,
    /// primary or revoked key is not in this list.
    pub newly_trusted: Vec<KeyCertificate>,
    /// Revocations a recovery key signed, to remember with their signatures.
    pub accepted_revocations: Vec<KeyRevocation>,
}

/// Verify a bundle against the client's trust set.
pub fn verify_bundle(bytes: &[u8], trust: &RegistryTrust) -> Result<VerifiedBundle, BundleError> {
    let parsed = parse_bundle(bytes)?;
    let mut revoked: std::collections::BTreeSet<String> = trust.revoked_ids().into_iter().collect();
    let mut accepted_revocations: Vec<KeyRevocation> = Vec::new();

    // A revocation is a separately signed statement. Collect every one a
    // recovery key vouches for, then rebuild trust without the revoked keys.
    // Revocations only ever grow this set, so the loop terminates.
    loop {
        let current = build_trusted(trust, &parsed.header.certificates, &revoked);
        let mut grew = false;
        for revocation in &parsed.header.revocations {
            if verify_revocation(revocation, &current, &revoked) {
                if revoked.insert(revocation.revoked_key_id.clone()) {
                    grew = true;
                }
                if !accepted_revocations.iter().any(|known| known == revocation) {
                    accepted_revocations.push(revocation.clone());
                }
            }
        }
        if !grew {
            break;
        }
    }

    let final_trust = build_trusted(trust, &parsed.header.certificates, &revoked);
    let signing_key = final_trust
        .keys
        .get(&parsed.header.key_id)
        .ok_or_else(|| BundleError::UnknownKey(parsed.header.key_id.clone()))?;
    if final_trust.roles.get(&parsed.header.key_id) == Some(&RegistryKeyRole::Recovery) {
        return Err(BundleError::KeyNotForCatalog(parsed.header.key_id));
    }

    use ed25519_dalek::Verifier;
    let signature = Signature::from_bytes(&parsed.signature);
    signing_key
        .verify(&header_signing_bytes(parsed.header_bytes), &signature)
        .map_err(|_| BundleError::SignatureInvalid)?;

    let digest = Sha256::digest(parsed.payload);
    let expected = parsed.header.db_sha256.to_ascii_lowercase();
    if hex::encode(digest) != expected {
        return Err(BundleError::PayloadHash);
    }

    Ok(VerifiedBundle {
        header: parsed.header,
        payload: parsed.payload.to_vec(),
        newly_trusted: final_trust.newly_trusted,
        accepted_revocations,
    })
}

struct TrustedKeys {
    keys: BTreeMap<String, VerifyingKey>,
    roles: BTreeMap<String, RegistryKeyRole>,
    newly_trusted: Vec<KeyCertificate>,
}

/// Build the keys trusted under `revoked`: compiled/remembered keys, then
/// certificates signed by a recovery key that is still trusted.
///
/// A certificate signed by a primary is ignored: a primary may sign catalogs
/// and nothing else.
fn build_trusted(
    trust: &RegistryTrust,
    certificates: &[KeyCertificate],
    revoked: &std::collections::BTreeSet<String>,
) -> TrustedKeys {
    let mut keys: BTreeMap<String, VerifyingKey> = BTreeMap::new();
    let mut roles: BTreeMap<String, RegistryKeyRole> = BTreeMap::new();
    for key in trust.keys() {
        if revoked.contains(&key.id) {
            continue;
        }
        if let Some(verifying) = verifying_key(&key.public_key_hex) {
            keys.insert(key.id.clone(), verifying);
            roles.insert(key.id.clone(), key.role);
        }
    }

    let mut newly_trusted = Vec::new();
    for _ in 0..=certificates.len() {
        let mut added = false;
        for certificate in certificates {
            if keys.contains_key(&certificate.key_id) || revoked.contains(&certificate.key_id) {
                continue;
            }
            let Some(signer) = keys.get(&certificate.signed_by).copied() else {
                continue;
            };
            if roles.get(&certificate.signed_by) != Some(&RegistryKeyRole::Recovery) {
                continue;
            }
            if revoked.contains(&certificate.signed_by) {
                continue;
            }
            if key_id_for_public_key(&certificate.public_key) != certificate.key_id {
                continue;
            }
            let Some(signature) = decode_signature(&certificate.signature) else {
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
            let Some(verifying) = verifying_key(&certificate.public_key) else {
                continue;
            };
            keys.insert(certificate.key_id.clone(), verifying);
            roles.insert(certificate.key_id.clone(), certificate.role);
            newly_trusted.push(certificate.clone());
            added = true;
        }
        if !added {
            break;
        }
    }

    TrustedKeys {
        keys,
        roles,
        newly_trusted,
    }
}

/// A revocation counts only when a trusted recovery key signed it, it does not
/// revoke its own signer, and neither is already revoked.
fn verify_revocation(
    revocation: &KeyRevocation,
    trusted: &TrustedKeys,
    revoked: &std::collections::BTreeSet<String>,
) -> bool {
    if revocation.revoked_key_id == revocation.signed_by {
        return false;
    }
    if trusted.roles.get(&revocation.signed_by) != Some(&RegistryKeyRole::Recovery) {
        return false;
    }
    if revoked.contains(&revocation.signed_by) || revoked.contains(&revocation.revoked_key_id) {
        return false;
    }
    let Some(signer) = trusted.keys.get(&revocation.signed_by) else {
        return false;
    };
    let Some(signature) = decode_signature(&revocation.signature) else {
        return false;
    };
    use ed25519_dalek::Verifier;
    signer
        .verify(
            &KeyRevocation::signed_bytes(&revocation.revoked_key_id, &revocation.signed_by),
            &signature,
        )
        .is_ok()
}

fn verifying_key(public_key_hex: &str) -> Option<VerifyingKey> {
    let bytes = hex::decode(public_key_hex.trim()).ok()?;
    let array: [u8; PUBLIC_KEY_LENGTH] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&array).ok()
}

fn decode_signature(raw: &str) -> Option<Signature> {
    let bytes = hex::decode(raw.trim()).ok()?;
    let array: [u8; SIGNATURE_LENGTH] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&array))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public(key: &SigningKey) -> String {
        hex::encode(key.verifying_key().to_bytes())
    }

    fn header_for(key: &SigningKey, payload: &[u8]) -> CatalogHeader {
        CatalogHeader {
            format: BUNDLE_FORMAT,
            key_id: key_id_for_public_key(&public(key)),
            schema_version: 10,
            tag: "registry-2026-11-16".into(),
            built_at: "2026-11-16T12:00:00+00:00".into(),
            issued_at: "2026-11-16T12:00:00+00:00".into(),
            expires_at: "2027-02-14T12:00:00+00:00".into(),
            db_len: payload.len() as u64,
            db_sha256: hex::encode(Sha256::digest(payload)),
            certificates: Vec::new(),
            revocations: Vec::new(),
        }
    }

    fn sign(key: &SigningKey, header: &CatalogHeader, payload: &[u8]) -> Vec<u8> {
        let bytes = header_bytes(header);
        let signature = key.sign(&header_signing_bytes(&bytes)).to_bytes();
        encode_bundle(&bytes, &signature, payload)
    }

    #[test]
    fn an_empty_bundle_is_an_error_not_an_empty_catalog() {
        assert!(matches!(parse_bundle(b""), Err(BundleError::Empty)));
        assert!(verify_bundle(b"", &RegistryTrust::with_key("00")).is_err());
    }

    #[test]
    fn a_short_bundle_is_truncated_not_accepted() {
        assert!(matches!(
            parse_bundle(MAGIC),
            Err(BundleError::Truncated(_))
        ));
    }

    #[test]
    fn a_signed_bundle_round_trips() {
        let payload = b"not really a database, but the bytes are covered";
        let signing = key(1);
        let header = header_for(&signing, payload);
        let bundle = sign(&signing, &header, payload);

        let verified = verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing)))
            .expect("the bundle was signed by the trusted key");
        assert_eq!(verified.header.key_id, header.key_id);
        assert_eq!(verified.payload, payload);
        assert!(!verified.header.is_expired_at(Utc::now()));
    }

    #[test]
    fn a_changed_payload_is_refused() {
        let payload = b"the signed catalog";
        let signing = key(2);
        let header = header_for(&signing, payload);
        let mut bundle = sign(&signing, &header, payload);
        let last = bundle.len() - 1;
        bundle[last] ^= 0xff;
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing))),
            Err(BundleError::PayloadHash)
        ));
    }

    #[test]
    fn a_changed_header_is_refused() {
        let payload = b"the signed catalog";
        let signing = key(3);
        let header = header_for(&signing, payload);
        let mut bundle = sign(&signing, &header, payload);
        // Change a byte inside the header JSON (the tag) without re-signing.
        let header_start = MAGIC_LEN + 4;
        let at = header_start
            + bundle[header_start..]
                .iter()
                .position(|b| *b == b'2')
                .unwrap();
        bundle[at] = b'3';
        assert!(verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing))).is_err());
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let payload = b"the signed catalog";
        let signing = key(4);
        let header = header_for(&signing, payload);
        let bundle = sign(&signing, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&key(5)))),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn an_expired_bundle_stays_readable_but_is_not_fresh() {
        let payload = b"the signed catalog";
        let signing = key(6);
        let mut header = header_for(&signing, payload);
        header.issued_at = "2025-01-01T00:00:00+00:00".into();
        header.expires_at = "2025-02-01T00:00:00+00:00".into();
        let bundle = sign(&signing, &header, payload);

        let verified = verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing)))
            .expect("expiry does not make the signature invalid");
        assert!(verified.header.is_expired_at(Utc::now()));
    }

    #[test]
    fn a_missing_built_at_is_not_an_unknown_stamp() {
        let payload = b"the signed catalog";
        let signing = key(7);
        let mut header = header_for(&signing, payload);
        header.built_at.clear();
        let bundle = sign(&signing, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing))),
            Err(BundleError::MissingField("built_at"))
        ));
    }

    fn certificate(signer: &SigningKey, new: &SigningKey, role: RegistryKeyRole) -> KeyCertificate {
        let key_id = key_id_for_public_key(&public(new));
        let signed_by = key_id_for_public_key(&public(signer));
        let message = KeyCertificate::signed_bytes(&key_id, &public(new), role, &signed_by);
        KeyCertificate {
            key_id,
            public_key: public(new),
            role,
            signed_by,
            signature: hex::encode(signer.sign(&message).to_bytes()),
        }
    }

    fn revocation(signer: &SigningKey, revoked: &SigningKey) -> KeyRevocation {
        let revoked_key_id = key_id_for_public_key(&public(revoked));
        let signed_by = key_id_for_public_key(&public(signer));
        let message = KeyRevocation::signed_bytes(&revoked_key_id, &signed_by);
        KeyRevocation {
            revoked_key_id,
            signed_by,
            signature: hex::encode(signer.sign(&message).to_bytes()),
        }
    }

    #[test]
    fn a_recovery_key_certifies_a_new_primary() {
        let payload = b"the signed catalog";
        let recovery = key(8);
        let new = key(9);
        let mut header = header_for(&new, payload);
        header
            .certificates
            .push(certificate(&recovery, &new, RegistryKeyRole::Primary));
        let bundle = sign(&new, &header, payload);

        let trust = RegistryTrust::with_roles([(RegistryKeyRole::Recovery, public(&recovery))]);
        let verified =
            verify_bundle(&bundle, &trust).expect("a recovery key may introduce a primary");
        assert_eq!(verified.newly_trusted.len(), 1);
        assert_eq!(verified.newly_trusted[0].role, RegistryKeyRole::Primary);
    }

    #[test]
    fn a_primary_key_cannot_certify_another_key() {
        let payload = b"the signed catalog";
        let primary = key(10);
        let attacker = key(11);
        let mut header = header_for(&primary, payload);
        header
            .certificates
            .push(certificate(&primary, &attacker, RegistryKeyRole::Primary));
        let bundle = sign(&primary, &header, payload);

        let trust = RegistryTrust::with_roles([(RegistryKeyRole::Primary, public(&primary))]);
        let verified = verify_bundle(&bundle, &trust)
            .expect("the catalog itself is validly signed by the compiled primary");
        assert!(
            verified.newly_trusted.is_empty(),
            "a primary-signed certificate must be ignored"
        );

        let later = header_for(&attacker, payload);
        let later_bundle = sign(&attacker, &later, payload);
        assert!(matches!(
            verify_bundle(&later_bundle, &trust),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_primary_key_cannot_certify_or_revoke_a_recovery_key() {
        let payload = b"the signed catalog";
        let primary = key(12);
        let recovery = key(13);
        let mut header = header_for(&primary, payload);
        header.revocations.push(revocation(&primary, &recovery));
        let bundle = sign(&primary, &header, payload);

        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Primary, public(&primary)),
            (RegistryKeyRole::Recovery, public(&recovery)),
        ]);
        let verified = verify_bundle(&bundle, &trust).unwrap();
        assert!(
            verified.accepted_revocations.is_empty(),
            "a primary-signed revocation must be ignored"
        );

        // The recovery key is still able to certify the next primary.
        let mut next = header_for(&primary, payload);
        next.certificates
            .push(certificate(&recovery, &key(14), RegistryKeyRole::Primary));
        let next_bundle = sign(&primary, &next, payload);
        let next_verified = verify_bundle(&next_bundle, &trust)
            .expect("the recovery key was not revoked by the primary");
        assert_eq!(next_verified.newly_trusted.len(), 1);
    }

    #[test]
    fn a_recovery_key_can_revoke_a_primary_and_it_is_reported() {
        let payload = b"the signed catalog";
        let recovery = key(15);
        let old_primary = key(16);
        let current = key(17);
        let mut header = header_for(&current, payload);
        header.revocations.push(revocation(&recovery, &old_primary));
        let bundle = sign(&current, &header, payload);

        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, public(&recovery)),
            (RegistryKeyRole::Primary, public(&old_primary)),
            (RegistryKeyRole::Primary, public(&current)),
        ]);
        let verified = verify_bundle(&bundle, &trust).unwrap();
        assert_eq!(verified.accepted_revocations.len(), 1);
        assert_eq!(
            verified.accepted_revocations[0].revoked_key_id,
            key_id_for_public_key(&public(&old_primary))
        );
    }

    #[test]
    fn a_revoked_signing_key_is_refused() {
        let payload = b"the signed catalog";
        let signing = key(18);
        let recovery = key(19);
        let mut header = header_for(&signing, payload);
        header.revocations.push(revocation(&recovery, &signing));
        let bundle = sign(&signing, &header, payload);

        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Recovery, public(&recovery)),
            (RegistryKeyRole::Primary, public(&signing)),
        ]);
        assert!(matches!(
            verify_bundle(&bundle, &trust),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_recovery_key_cannot_revoke_itself() {
        let payload = b"the signed catalog";
        let primary = key(20);
        let recovery = key(21);
        let mut header = header_for(&primary, payload);
        header.revocations.push(revocation(&recovery, &recovery));
        let bundle = sign(&primary, &header, payload);

        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Primary, public(&primary)),
            (RegistryKeyRole::Recovery, public(&recovery)),
        ]);
        let verified = verify_bundle(&bundle, &trust).unwrap();
        assert!(verified.accepted_revocations.is_empty());

        // Still able to certify.
        let mut next = header_for(&primary, payload);
        next.certificates
            .push(certificate(&recovery, &key(22), RegistryKeyRole::Primary));
        assert!(verify_bundle(&sign(&primary, &next, payload), &trust).is_ok());
    }

    #[test]
    fn one_recovery_key_can_revoke_another() {
        let payload = b"the signed catalog";
        let primary = key(23);
        let first = key(24);
        let second = key(25);
        let mut header = header_for(&primary, payload);
        header.revocations.push(revocation(&first, &second));
        let bundle = sign(&primary, &header, payload);

        let trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Primary, public(&primary)),
            (RegistryKeyRole::Recovery, public(&first)),
            (RegistryKeyRole::Recovery, public(&second)),
        ]);
        let verified = verify_bundle(&bundle, &trust).unwrap();
        assert_eq!(verified.accepted_revocations.len(), 1);

        // The revoked recovery key can no longer certify.
        let mut next = header_for(&primary, payload);
        next.certificates
            .push(certificate(&second, &key(26), RegistryKeyRole::Primary));
        let next_bundle = sign(&primary, &next, payload);
        let next_trust = RegistryTrust::with_roles([
            (RegistryKeyRole::Primary, public(&primary)),
            (RegistryKeyRole::Recovery, public(&first)),
        ]);
        let mut remembered = next_trust.clone();
        remembered.merge_remembered(Vec::new(), vec![revocation(&first, &second)]);
        let verified = verify_bundle(&next_bundle, &remembered).unwrap();
        assert!(
            verified.newly_trusted.is_empty(),
            "a certificate signed by a revoked recovery key must be ignored"
        );

        let later = header_for(&key(26), payload);
        let later_bundle = sign(&key(26), &later, payload);
        assert!(matches!(
            verify_bundle(&later_bundle, &remembered),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_new_key_without_a_certificate_is_refused() {
        let payload = b"the signed catalog";
        let new = key(27);
        let header = header_for(&new, payload);
        let bundle = sign(&new, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&key(28)))),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_certificate_whose_id_does_not_match_its_key_is_ignored() {
        let payload = b"the signed catalog";
        let recovery = key(29);
        let new = key(30);
        let mut header = header_for(&new, payload);
        let wrong_id = "0000000000000000".to_string();
        let message = KeyCertificate::signed_bytes(
            &wrong_id,
            &public(&new),
            RegistryKeyRole::Primary,
            &key_id_for_public_key(&public(&recovery)),
        );
        header.certificates.push(KeyCertificate {
            key_id: wrong_id,
            public_key: public(&new),
            role: RegistryKeyRole::Primary,
            signed_by: key_id_for_public_key(&public(&recovery)),
            signature: hex::encode(recovery.sign(&message).to_bytes()),
        });
        let bundle = sign(&new, &header, payload);
        let trust = RegistryTrust::with_roles([(RegistryKeyRole::Recovery, public(&recovery))]);
        assert!(matches!(
            verify_bundle(&bundle, &trust),
            Err(BundleError::UnknownKey(_))
        ));
    }
}
