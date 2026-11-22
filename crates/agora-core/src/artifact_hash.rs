//! What a downloaded file is checked against, and how an unverified file is
//! represented.
//!
//! Agora never hashes files by hand for the catalog. A file is checked against
//! the hash its source published (GitHub's asset digest, Modrinth's version
//! file hashes, or the manifest hash of a `direct_hash` entry, which identifies
//! its one file). Two more expectations ask the user before they are overridden:
//!
//! * a **curator pin**: a `pins` entry on a download source, naming the exact
//!   release tag and asset it is for;
//! * a **previous install**: the SHA-256 Agora recorded when the same release
//!   file was installed before, and the source published no checksum for it;
//! * a **pack pin**: the SHA-256 a shared modpack file lists for this file. It
//!   stands in for a hash the source did not publish, and is never counted as
//!   the source having verified the file.
//!
//! A file whose source published nothing is installed only after the user is
//! told, and its bytes' SHA-256 is recorded with `hash_verified: false`.

use serde::{Deserialize, Serialize};

/// Where a hash expectation came from. Shown to the user when it is not met.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashOrigin {
    /// The curator's `pins` entry for this release and asset.
    CuratorPin,
    /// The SHA-256 recorded when this release file was installed before.
    PreviousInstall,
    /// The SHA-256 the modpack being imported lists for this file.
    PackPin,
}

impl HashOrigin {
    /// The wire name, also used in the error details.
    pub fn as_str(self) -> &'static str {
        match self {
            HashOrigin::CuratorPin => "curator_pin",
            HashOrigin::PreviousInstall => "previous_install",
            HashOrigin::PackPin => "pack_pin",
        }
    }
}

/// A SHA-256 expectation the user can override with an explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmableHash {
    pub origin: HashOrigin,
    pub sha256: String,
}

/// A hash Agora or a pack author asserted is blank. A blank assertion is never
/// "no pin": it would silently drop a check the curator or pack asked for, so
/// it is an error that names the item and the field it came from. A blank hash
/// a third-party source published is *not* this error; that still means "the
/// source published nothing" and is reported as an unverified install.
pub fn blank_asserted_hash(item: &str, field: &str) -> crate::error::LauncherError {
    crate::error::LauncherError::Generic {
        code: "ERR_ASSERTED_HASH_BLANK".into(),
        message: format!(
            "'{item}' has a blank {field}; refusing to install without a valid hash for it."
        ),
    }
}

/// Whether `value` is a well-formed SHA-256 hex digest (64 hex characters).
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The first expectation the downloaded bytes do not meet, if any.
///
/// `actual_sha256` is the lowercase or uppercase hex SHA-256 of the bytes.
/// A malformed expectation never matches, so it is reported rather than
/// silently treated as verified.
pub fn first_unmet<'a>(
    actual_sha256: &str,
    expectations: &'a [ConfirmableHash],
) -> Option<&'a ConfirmableHash> {
    expectations.iter().find(|expected| {
        !(is_sha256_hex(&expected.sha256)
            && expected.sha256.eq_ignore_ascii_case(actual_sha256.trim()))
    })
}

/// Every expectation the downloaded bytes do not meet, in order. The first is
/// what a confirmation names; the rest are listed too, so one "install anyway"
/// never overrides a check the user was not told about.
pub fn all_unmet<'a>(
    actual_sha256: &str,
    expectations: &'a [ConfirmableHash],
) -> Vec<&'a ConfirmableHash> {
    expectations
        .iter()
        .filter(|expected| {
            !(is_sha256_hex(&expected.sha256)
                && expected.sha256.eq_ignore_ascii_case(actual_sha256.trim()))
        })
        .collect()
}

/// What a download was checked against, once it has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadCheck {
    /// `true` when a hash the source published matched the bytes; `false` when
    /// the source published none, so the bytes are recorded as not verified.
    pub verified: bool,
    /// The lowercase SHA-256 of the bytes, which is what the install records.
    pub sha256: String,
}

/// Decide whether downloaded bytes may be installed.
///
/// * A hash the source published (`published_sha1`, `published_sha256` or
///   `published_sha512`) must match the bytes. A mismatch fails with
///   `HashMismatch`, and nothing overrides it. An empty value counts as nothing
///   published; a malformed one never matches.
/// * Every `expectations` entry (a curator pin, a hash remembered from an
///   earlier install) must match too. A mismatch fails with
///   `HashConfirmationRequired`, unless `accept_confirmation` is set.
/// * When the source published no hash at all, the bytes are accepted and the
///   result is `verified: false`, so the caller can tell the user.
// Three published digests are positional, in algorithm order and all
// `Option<&str>`: a caller that names its own values at the call site stays
// readable, and one aggregating them already holds them together.
#[allow(clippy::too_many_arguments)]
pub fn verify_download(
    bytes: &[u8],
    published_sha1: Option<&str>,
    published_sha256: Option<&str>,
    published_sha512: Option<&str>,
    expectations: &[ConfirmableHash],
    accept_confirmation: bool,
    file: &str,
    release: Option<&str>,
) -> crate::error::LauncherResult<DownloadCheck> {
    use crate::error::{HashConfirmation, LauncherError};

    // Published blanks mean "the source published no hash" and are handled
    // below. An expectation is different: it was asserted by a curator, a pack
    // author or an earlier install, and a blank one is an error, not a check
    // that can be skipped or a confirmation the user can answer.
    for expected in expectations {
        if expected.sha256.trim().is_empty() {
            return Err(blank_asserted_hash(file, expected.origin.as_str()));
        }
    }

    let sha1 = published_sha1.unwrap_or("").trim().to_lowercase();
    let sha256 = published_sha256.unwrap_or("").trim().to_lowercase();
    let sha512 = published_sha512.unwrap_or("").trim().to_lowercase();
    let actual_sha256 = crate::download::sha256_hex(bytes);

    // Every hash the source published must match: a SHA-1 alone is weak, so when the source also
    // published a SHA-256 or a SHA-512 that is checked too, never one in place of the other.
    if !sha256.is_empty() && actual_sha256 != sha256 {
        return Err(LauncherError::HashMismatch);
    }
    if !sha512.is_empty() && crate::download::sha512_hex(bytes) != sha512 {
        return Err(LauncherError::HashMismatch);
    }
    if !sha1.is_empty() && crate::download::sha1_hex(bytes) != sha1 {
        return Err(LauncherError::HashMismatch);
    }
    let verified = !sha256.is_empty() || !sha1.is_empty() || !sha512.is_empty();

    if !accept_confirmation {
        let unmet = all_unmet(&actual_sha256, expectations);
        if let Some((first, rest)) = unmet.split_first() {
            return Err(LauncherError::HashConfirmationRequired(Box::new(
                HashConfirmation {
                    file: file.to_string(),
                    release: release.map(str::to_string),
                    expected: first.sha256.clone(),
                    actual: actual_sha256,
                    expected_from: first.origin,
                    also_unmet: rest.iter().map(|other| (*other).clone()).collect(),
                },
            )));
        }
    }

    Ok(DownloadCheck {
        verified,
        sha256: actual_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(sha256: &str) -> ConfirmableHash {
        ConfirmableHash {
            origin: HashOrigin::CuratorPin,
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn a_matching_expectation_is_met_in_either_case() {
        let actual = "ab".repeat(32);
        assert!(first_unmet(&actual, &[pin(&actual.to_uppercase())]).is_none());
        assert!(first_unmet(&actual, &[pin(&actual)]).is_none());
    }

    #[test]
    fn a_different_expectation_is_reported_with_its_origin() {
        let actual = "ab".repeat(32);
        let expected = ConfirmableHash {
            origin: HashOrigin::PreviousInstall,
            sha256: "cd".repeat(32),
        };
        let unmet = first_unmet(&actual, std::slice::from_ref(&expected)).expect("unmet");
        assert_eq!(unmet, &expected);
        assert_eq!(unmet.origin.as_str(), "previous_install");
    }

    #[test]
    fn empty_and_malformed_expectations_are_never_met() {
        let actual = "ab".repeat(32);
        assert!(first_unmet(&actual, &[pin("")]).is_some());
        assert!(first_unmet(&actual, &[pin("ab")]).is_some());
        assert!(first_unmet(&actual, &[pin(&"zz".repeat(32))]).is_some());
    }

    fn digest_of(bytes: &[u8]) -> String {
        crate::download::sha256_hex(bytes)
    }

    #[test]
    fn a_published_digest_that_matches_verifies() {
        let bytes = b"mod bytes";
        let check = verify_download(
            bytes,
            None,
            Some(&digest_of(bytes)),
            None,
            &[],
            false,
            "a.jar",
            Some("v1"),
        )
        .expect("matches");
        assert!(check.verified);
        assert_eq!(check.sha256, digest_of(bytes));
    }

    #[test]
    fn a_matching_sha1_does_not_cover_for_a_wrong_sha256() {
        let bytes = b"mod bytes";
        let sha1 = crate::download::sha1_hex(bytes);
        let wrong = "cd".repeat(32);
        let error = verify_download(
            bytes,
            Some(&sha1),
            Some(&wrong),
            None,
            &[],
            true,
            "a.jar",
            None,
        )
        .expect_err("both published hashes must match");
        assert_eq!(error.code(), "ERR_HASH_MISMATCH");
        let both = verify_download(
            bytes,
            Some(&sha1),
            Some(&digest_of(bytes)),
            None,
            &[],
            false,
            "a.jar",
            None,
        )
        .expect("both match");
        assert!(both.verified);
    }

    #[test]
    fn a_published_sha512_is_checked_too() {
        let bytes = b"mod bytes";
        let sha512 = crate::download::sha512_hex(bytes);
        let check = verify_download(bytes, None, None, Some(&sha512), &[], false, "a.jar", None)
            .expect("a SHA-512 alone is a published hash");
        assert!(
            check.verified,
            "the source published a hash, so the bytes count as verified"
        );

        // A matching SHA-256 does not stand in for a published SHA-512: every
        // published digest describes the one file.
        let wrong = "ab".repeat(64);
        let error = verify_download(
            bytes,
            None,
            Some(&digest_of(bytes)),
            Some(&wrong),
            &[],
            true,
            "a.jar",
            None,
        )
        .expect_err("a SHA-512 mismatch is never overridable");
        assert_eq!(error.code(), "ERR_HASH_MISMATCH");
    }

    #[test]
    fn a_published_digest_that_differs_is_a_hard_mismatch_even_when_accepted() {
        let bytes = b"mod bytes";
        let wrong = "cd".repeat(32);
        let error = verify_download(
            bytes,
            None,
            Some(&wrong),
            None,
            &[],
            true,
            "a.jar",
            Some("v1"),
        )
        .expect_err("mismatch");
        assert_eq!(error.code(), "ERR_HASH_MISMATCH");
    }

    #[test]
    fn no_published_hash_installs_as_not_verified_with_the_bytes_hash_recorded() {
        let bytes = b"old release file";
        let check = verify_download(bytes, None, None, None, &[], false, "a.jar", Some("v1"))
            .expect("accepted");
        assert!(!check.verified);
        assert_eq!(check.sha256, digest_of(bytes));
    }

    #[test]
    fn an_empty_published_hash_is_nothing_published_not_a_match() {
        let check = verify_download(b"x", Some(""), Some("   "), None, &[], false, "a.jar", None)
            .expect("accepted");
        assert!(!check.verified);
    }

    #[test]
    fn a_blank_asserted_expectation_is_refused_not_treated_as_no_pin() {
        for blank in ["", "   "] {
            // `accept_confirmation` is true on purpose: a blank assertion is
            // not something the user can confirm. It must be an error, while
            // the blank published hashes stay "nothing published".
            let error = verify_download(
                b"x",
                Some(""),
                Some("   "),
                None,
                std::slice::from_ref(&pin(blank)),
                true,
                "a.jar",
                Some("v1"),
            )
            .expect_err("a blank assertion cannot be ignored or confirmed");
            assert_eq!(error.code(), "ERR_ASSERTED_HASH_BLANK");
            let message = error.to_string();
            assert!(message.contains("a.jar"), "names the item: {message}");
            assert!(
                message.contains("curator_pin"),
                "names the field: {message}"
            );
        }
    }

    #[test]
    fn a_malformed_published_hash_is_refused_not_treated_as_verified() {
        let error = verify_download(b"x", None, Some("not-hex"), None, &[], false, "a.jar", None)
            .expect_err("refused");
        assert_eq!(error.code(), "ERR_HASH_MISMATCH");
    }

    #[test]
    fn a_curator_pin_that_differs_asks_the_user_and_is_installed_when_accepted() {
        let bytes = b"new bytes";
        let pin = ConfirmableHash {
            origin: HashOrigin::CuratorPin,
            sha256: "ef".repeat(32),
        };
        let error = verify_download(
            bytes,
            None,
            None,
            None,
            std::slice::from_ref(&pin),
            false,
            "a.jar",
            Some("v1.2.0"),
        )
        .expect_err("asks");
        match error {
            crate::error::LauncherError::HashConfirmationRequired(detail) => {
                assert_eq!(detail.file, "a.jar");
                assert_eq!(detail.release.as_deref(), Some("v1.2.0"));
                assert_eq!(detail.expected, pin.sha256);
                assert_eq!(detail.actual, digest_of(bytes));
                assert_eq!(detail.expected_from, HashOrigin::CuratorPin);
            }
            other => panic!("expected a confirmation, got {other:?}"),
        }
        let accepted = verify_download(
            bytes,
            None,
            None,
            None,
            std::slice::from_ref(&pin),
            true,
            "a.jar",
            Some("v1.2.0"),
        )
        .expect("accepted");
        assert!(!accepted.verified);
    }

    #[test]
    fn a_remembered_hash_that_differs_asks_the_user() {
        let remembered = ConfirmableHash {
            origin: HashOrigin::PreviousInstall,
            sha256: digest_of(b"first download"),
        };
        let error = verify_download(
            b"second download",
            None,
            None,
            None,
            std::slice::from_ref(&remembered),
            false,
            "a.jar",
            Some("v1"),
        )
        .expect_err("asks");
        assert_eq!(error.code(), "ERR_HASH_CONFIRMATION_REQUIRED");
    }

    #[test]
    fn every_unmet_expectation_is_listed_not_just_the_first() {
        let bytes = b"new bytes";
        let curator = pin(&"ef".repeat(32));
        let pack = ConfirmableHash {
            origin: HashOrigin::PackPin,
            sha256: "12".repeat(32),
        };
        let met = ConfirmableHash {
            origin: HashOrigin::PreviousInstall,
            sha256: digest_of(bytes),
        };
        let expectations = [pack.clone(), met, curator.clone()];
        let error = verify_download(bytes, None, None, None, &expectations, false, "a.jar", None)
            .expect_err("asks");
        match error {
            crate::error::LauncherError::HashConfirmationRequired(detail) => {
                assert_eq!(detail.expected_from, HashOrigin::PackPin);
                assert_eq!(
                    detail.also_unmet.to_vec(),
                    vec![curator],
                    "the met one is not listed"
                );
                let text = detail.to_string();
                assert!(
                    text.contains(&pack.sha256) && text.contains(&"ef".repeat(32)),
                    "{text}"
                );
            }
            other => panic!("expected a confirmation, got {other:?}"),
        }
        assert_eq!(all_unmet(&digest_of(bytes), &[]).len(), 0);
    }

    #[test]
    fn a_pin_that_matches_passes_without_confirmation() {
        let bytes = b"the file";
        let pin = ConfirmableHash {
            origin: HashOrigin::CuratorPin,
            sha256: digest_of(bytes),
        };
        let check = verify_download(bytes, None, None, None, &[pin], false, "a.jar", Some("v1"))
            .expect("passes");
        assert!(!check.verified);
    }
}
