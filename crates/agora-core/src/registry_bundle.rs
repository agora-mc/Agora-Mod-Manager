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
/// The only bundle format this build understands.
pub const BUNDLE_FORMAT: u32 = 1;

/// The public keys compiled into the client when no build-time override is set.
///
/// A release build may set `AGORA_REGISTRY_PUBKEYS` (comma-separated hex) or
/// the older single-key `AGORA_REGISTRY_PUBKEY`. Keep an old key here for the
/// length of a rotation, then remove it in the next client release.
const DEFAULT_REGISTRY_PUBKEYS: &[&str] =
    &["47adee76cf587ee618f79eb2fa5bde003824d3bfc2dbb5080d33073c5a8f8c18"];

/// One trusted public key: the id is derived from the key, so operators never
/// have to keep two names in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryKey {
    pub id: String,
    pub public_key_hex: String,
}

impl RegistryKey {
    pub fn from_hex(public_key_hex: impl Into<String>) -> Self {
        let public_key_hex = public_key_hex.into();
        let id = key_id_for_public_key(&public_key_hex);
        Self { id, public_key_hex }
    }
}

/// The key ids a client trusts, and whether a debug build may open an unsigned
/// catalog (a test convenience; never true in a release build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryTrust {
    keys: Vec<RegistryKey>,
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
            .map(RegistryKey::from_hex)
            .collect();
        Self {
            keys,
            permit_unsigned_debug: false,
        }
    }

    /// Test support: trust exactly this key.
    pub fn with_key(public_key_hex: impl Into<String>) -> Self {
        Self {
            keys: vec![RegistryKey::from_hex(public_key_hex)],
            permit_unsigned_debug: false,
        }
    }

    /// Test support: trust exactly these keys.
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
            permit_unsigned_debug: cfg!(debug_assertions),
        }
    }

    pub fn keys(&self) -> &[RegistryKey] {
        &self.keys
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether an unsigned legacy catalog may be opened. Only ever true in a
    /// debug build that explicitly asked for it.
    pub fn permits_unsigned_debug(&self) -> bool {
        self.permit_unsigned_debug && cfg!(debug_assertions)
    }
}

fn compiled_public_keys() -> Vec<String> {
    let split = |raw: &str| -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
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
    DEFAULT_REGISTRY_PUBKEYS
        .iter()
        .map(|key| (*key).to_string())
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

/// One statement that `key_id` belongs to `public_key`, signed by `signed_by`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyCertificate {
    pub key_id: String,
    pub public_key: String,
    pub signed_by: String,
    /// Lowercase hex, 64 bytes.
    pub signature: String,
}

impl KeyCertificate {
    /// The exact bytes a certificate signature covers.
    pub fn signed_bytes(key_id: &str, public_key: &str, signed_by: &str) -> Vec<u8> {
        let mut message = CERT_DOMAIN.to_vec();
        message.extend_from_slice(key_id.trim().as_bytes());
        message.push(b'\n');
        message.extend_from_slice(public_key.trim().as_bytes());
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
    /// Key ids the signer says must no longer be used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked_key_ids: Vec<String>,
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
}

/// Verify a bundle against the client's trust set.
pub fn verify_bundle(bytes: &[u8], trust: &RegistryTrust) -> Result<VerifiedBundle, BundleError> {
    let parsed = parse_bundle(bytes)?;
    let trusted = trusted_keys(&parsed.header, trust)?;
    let signing_key = trusted
        .get(&parsed.header.key_id)
        .ok_or_else(|| BundleError::UnknownKey(parsed.header.key_id.clone()))?;

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
    })
}

/// Build the trusted key set for a header: compiled-in keys, then certificates
/// signed by an already-trusted key, then the header's revocations.
fn trusted_keys(
    header: &CatalogHeader,
    trust: &RegistryTrust,
) -> Result<BTreeMap<String, VerifyingKey>, BundleError> {
    let mut trusted: BTreeMap<String, VerifyingKey> = BTreeMap::new();
    for key in trust.keys() {
        if let Some(verifying) = verifying_key(&key.public_key_hex) {
            trusted.insert(key.id.clone(), verifying);
        }
    }

    // Retire keys before they can vouch for anything else. A bundle signed by a
    // trusted key is allowed to say an old key is compromised; certificates
    // that key signed must stop counting from that point.
    for revoked in &header.revoked_key_ids {
        trusted.remove(revoked);
    }
    if header.revoked_key_ids.iter().any(|id| id == &header.key_id) {
        return Err(BundleError::RevokedKey(header.key_id.clone()));
    }

    // Certificates can chain, so repeat until nothing new is trusted. The
    // certificate count bounds the loop, so a cycle cannot spin.
    for _ in 0..=header.certificates.len() {
        let mut added = false;
        for certificate in &header.certificates {
            if trusted.contains_key(&certificate.key_id) {
                continue;
            }
            let Some(signer) = trusted.get(&certificate.signed_by).copied() else {
                continue;
            };
            // Re-checked here because a certificate earlier in this list can
            // name a revoked signer and the set was filtered before the loop.
            if header
                .revoked_key_ids
                .iter()
                .any(|revoked| revoked == &certificate.signed_by)
            {
                continue;
            }
            let expected_id = key_id_for_public_key(&certificate.public_key);
            if expected_id != certificate.key_id {
                return Err(BundleError::CertificateInvalid(format!(
                    "key id {} does not match its public key",
                    certificate.key_id
                )));
            }
            let signature = decode_signature(&certificate.signature).ok_or_else(|| {
                BundleError::CertificateInvalid(format!(
                    "key {} has an unreadable signature",
                    certificate.key_id
                ))
            })?;
            use ed25519_dalek::Verifier;
            let message = KeyCertificate::signed_bytes(
                &certificate.key_id,
                &certificate.public_key,
                &certificate.signed_by,
            );
            if signer.verify(&message, &signature).is_err() {
                return Err(BundleError::CertificateInvalid(format!(
                    "key {} was not signed by {}",
                    certificate.key_id, certificate.signed_by
                )));
            }
            let verifying = verifying_key(&certificate.public_key).ok_or_else(|| {
                BundleError::CertificateInvalid(format!(
                    "key {} is not a valid Ed25519 public key",
                    certificate.key_id
                ))
            })?;
            trusted.insert(certificate.key_id.clone(), verifying);
            added = true;
        }
        if !added {
            break;
        }
    }

    Ok(trusted)
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
            revoked_key_ids: Vec::new(),
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

    #[test]
    fn a_new_key_signed_by_an_old_trusted_key_is_accepted() {
        let payload = b"the signed catalog";
        let old = key(8);
        let new = key(9);
        let mut header = header_for(&new, payload);
        let cert_message = KeyCertificate::signed_bytes(
            &key_id_for_public_key(&public(&new)),
            &public(&new),
            &key_id_for_public_key(&public(&old)),
        );
        header.certificates.push(KeyCertificate {
            key_id: key_id_for_public_key(&public(&new)),
            public_key: public(&new),
            signed_by: key_id_for_public_key(&public(&old)),
            signature: hex::encode(old.sign(&cert_message).to_bytes()),
        });
        let bundle = sign(&new, &header, payload);

        let verified = verify_bundle(&bundle, &RegistryTrust::with_key(public(&old)))
            .expect("the old trusted key vouched for the new one");
        assert_eq!(verified.header.key_id, header.key_id);
    }

    #[test]
    fn a_new_key_without_a_certificate_is_refused() {
        let payload = b"the signed catalog";
        let new = key(10);
        let header = header_for(&new, payload);
        let bundle = sign(&new, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&key(11)))),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_certificate_signed_by_an_old_key_is_still_refused_when_the_old_key_is_revoked() {
        let payload = b"the signed catalog";
        let old = key(12);
        let new = key(13);
        let mut header = header_for(&new, payload);
        let cert_message = KeyCertificate::signed_bytes(
            &key_id_for_public_key(&public(&new)),
            &public(&new),
            &key_id_for_public_key(&public(&old)),
        );
        header.certificates.push(KeyCertificate {
            key_id: key_id_for_public_key(&public(&new)),
            public_key: public(&new),
            signed_by: key_id_for_public_key(&public(&old)),
            signature: hex::encode(old.sign(&cert_message).to_bytes()),
        });
        header
            .revoked_key_ids
            .push(key_id_for_public_key(&public(&old)));
        let bundle = sign(&new, &header, payload);

        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&old))),
            Err(BundleError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_revoked_signing_key_is_refused() {
        let payload = b"the signed catalog";
        let signing = key(14);
        let mut header = header_for(&signing, payload);
        header.revoked_key_ids.push(header.key_id.clone());
        let bundle = sign(&signing, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&signing))),
            Err(BundleError::RevokedKey(_))
        ));
    }

    #[test]
    fn a_certificate_whose_id_does_not_match_its_key_is_refused() {
        let payload = b"the signed catalog";
        let old = key(15);
        let new = key(16);
        let mut header = header_for(&new, payload);
        let wrong_id = "0000000000000000".to_string();
        let cert_message = KeyCertificate::signed_bytes(
            &wrong_id,
            &public(&new),
            &key_id_for_public_key(&public(&old)),
        );
        header.certificates.push(KeyCertificate {
            key_id: wrong_id,
            public_key: public(&new),
            signed_by: key_id_for_public_key(&public(&old)),
            signature: hex::encode(old.sign(&cert_message).to_bytes()),
        });
        let bundle = sign(&new, &header, payload);
        assert!(matches!(
            verify_bundle(&bundle, &RegistryTrust::with_key(public(&old))),
            Err(BundleError::CertificateInvalid(_))
        ));
    }
}
