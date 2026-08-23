//! Supply-chain verification for auto-downloaded language-server binaries.
//!
//! Policy: a freshly downloaded binary may only be made executable after its
//! SHA-256 digest matches the release's `<asset>.sha256` companion file
//! (`sha256sum` format: `<lowercase-hex>␠␠<filename>`). Any problem along the
//! way — companion fetch error, unparseable companion, unreadable artifact,
//! size cap exceeded, or digest mismatch — fails closed: the caller deletes
//! the artifact and refuses to run it. There is deliberately no fallback to
//! executing unverified bytes.
//!
//! This module owns verification *policy* (what to fetch, what to compare,
//! how failures are worded). Orchestration (installation status transitions,
//! logging, cleanup calls) stays in [`crate`]; the hash primitive lives in
//! [`crate::sha256`].

use zed_extension_api::http_client::{HttpMethod, HttpRequest, RedirectPolicy};

use crate::sha256;

/// Sanity cap for reading a downloaded binary into memory for hashing.
/// Real `rsc-ls` artifacts are a few MiB; anything beyond this is treated as
/// a verification failure rather than hashed (WASM memory is bounded).
pub(crate) const MAX_VERIFIED_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// Number of hex characters shown in logs/status messages for each digest.
/// Full hashes are never emitted (log hygiene).
const DIGEST_LOG_PREFIX: usize = 12;

/// Why checksum verification refused a freshly downloaded binary.
///
/// Variant order mirrors the pipeline stages; every variant fails closed at
/// the call site.
pub(crate) enum VerificationFailure {
    /// The `<asset>.sha256` companion could not be fetched over HTTP.
    CompanionFetch(String),
    /// The companion was fetched but did not contain a valid SHA-256 digest.
    CompanionParse(String),
    /// The downloaded binary could not be read back for hashing.
    BinaryRead(String),
    /// The downloaded binary exceeds [`MAX_VERIFIED_BINARY_BYTES`].
    BinaryTooLarge(u64),
    /// Digest of the downloaded bytes differs from the companion digest.
    Mismatch { expected: String, actual: String },
}

impl VerificationFailure {
    /// Builds the user-facing failure message. Names the stage that failed
    /// and shows only [`DIGEST_LOG_PREFIX`]-character digest prefixes — full
    /// hashes never appear in status/log text.
    pub(crate) fn describe(self, source_url: &str) -> String {
        let companion_url = format!("{source_url}.sha256");
        match self {
            Self::CompanionFetch(e) => format!(
                "Checksum verification failed: could not fetch .sha256 companion \
                from {companion_url}: {e}. Refusing to run unverified {}.",
                crate::BINARY_NAME
            ),
            Self::CompanionParse(detail) => format!(
                "Checksum verification failed: invalid .sha256 companion at \
                {companion_url}: {detail}. Refusing to run unverified {}.",
                crate::BINARY_NAME
            ),
            Self::BinaryRead(detail) => format!(
                "Checksum verification failed: downloaded binary could not be read \
                for hashing: {detail}. Refusing to run unverified binary."
            ),
            Self::BinaryTooLarge(size) => format!(
                "Checksum verification failed: downloaded binary is too large to \
                verify ({size} bytes, limit {MAX_VERIFIED_BINARY_BYTES}). \
                Refusing to run unverified binary."
            ),
            Self::Mismatch { expected, actual } => format!(
                "Checksum verification failed for downloaded binary: expected sha256 \
                {}… got {}… (source {source_url}). The release asset may be corrupt \
                or tampered with. Refusing to run unverified binary.",
                short_digest(&expected),
                short_digest(&actual),
            ),
        }
    }
}

/// Fetches `{download_url}.sha256` through the Zed extension host HTTP client
/// and returns the expected digest parsed from the companion content.
fn fetch_companion_digest(download_url: &str) -> std::result::Result<String, VerificationFailure> {
    let companion_url = format!("{download_url}.sha256");
    let request = HttpRequest::builder()
        .method(HttpMethod::Get)
        .url(companion_url.as_str())
        // GitHub release assets are served behind redirects; the builder
        // default (`NoFollow`) would turn the fetch itself into a failure.
        .redirect_policy(RedirectPolicy::FollowLimit(5))
        .build()
        .map_err(VerificationFailure::CompanionFetch)?;

    let response = request
        .fetch()
        .map_err(VerificationFailure::CompanionFetch)?;
    let content = std::str::from_utf8(&response.body)
        .map_err(|_| VerificationFailure::CompanionParse("companion is not valid UTF-8".into()))?;
    sha256::parse_digest_companion(content).map_err(VerificationFailure::CompanionParse)
}

/// Verifies the just-downloaded binary against its `.sha256` companion.
///
/// `binary_name` is the work-dir-relative path `download_file` wrote to.
/// Read-back stays within the extension work dir (repo hard rule #7).
///
/// On success returns the verified digest prefix (for logging); on any
/// failure returns a [`VerificationFailure`] — callers must fail closed.
pub(crate) fn verify_downloaded_binary(
    binary_name: &str,
    download_url: &str,
) -> std::result::Result<String, VerificationFailure> {
    let expected = fetch_companion_digest(download_url)?;

    // Cap before reading: refuse absurdly large artifacts instead of loading
    // them into memory inside the WASM component.
    let size = std::fs::metadata(binary_name)
        .map_err(|e| VerificationFailure::BinaryRead(e.to_string()))?
        .len();
    if size > MAX_VERIFIED_BINARY_BYTES {
        return Err(VerificationFailure::BinaryTooLarge(size));
    }

    let bytes =
        std::fs::read(binary_name).map_err(|e| VerificationFailure::BinaryRead(e.to_string()))?;
    let actual = sha256::sha256_hex(&bytes);

    if !sha256::digests_match(&expected, &actual) {
        return Err(VerificationFailure::Mismatch { expected, actual });
    }
    Ok(short_digest(&actual).to_string())
}

/// First [`DIGEST_LOG_PREFIX`] hex characters of a digest, for log/status text.
pub(crate) fn short_digest(hex: &str) -> &str {
    &hex[..hex.len().min(DIGEST_LOG_PREFIX)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_A: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const DIGEST_B: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn short_digest_caps_at_twelve_characters() {
        assert_eq!(short_digest(DIGEST_A), "e3b0c44298fc");
        assert_eq!(short_digest("abc"), "abc");
        assert_eq!(short_digest(""), "");
    }

    #[test]
    fn mismatch_message_shows_prefixes_but_never_full_hashes() {
        // Log-hygiene contract: status/log text must not leak complete digests.
        let msg = VerificationFailure::Mismatch {
            expected: DIGEST_A.to_string(),
            actual: DIGEST_B.to_string(),
        }
        .describe("https://example.com/rsc-ls-x86_64-unknown-linux-gnu");
        assert!(msg.contains("e3b0c44298fc"));
        assert!(msg.contains("ba7816bf8f01"));
        assert!(!msg.contains(DIGEST_A));
        assert!(!msg.contains(DIGEST_B));
    }

    #[test]
    fn failure_messages_name_the_stage_that_failed() {
        let url = "https://github.com/x/y/releases/download/v0.0.0/asset";
        let cases = [
            (
                VerificationFailure::CompanionFetch("timeout".into()),
                "could not fetch .sha256 companion",
            ),
            (
                VerificationFailure::CompanionParse(
                    "digest token is 10 characters, expected 64".into(),
                ),
                "invalid .sha256 companion",
            ),
            (
                VerificationFailure::BinaryRead("permission denied".into()),
                "read for hashing",
            ),
            (
                VerificationFailure::BinaryTooLarge(999),
                "too large to verify",
            ),
        ];
        for (failure, needle) in cases {
            let msg = failure.describe(url);
            assert!(
                msg.contains(needle),
                "message for {needle:?} did not contain expected text: {msg}"
            );
            assert!(
                msg.contains("Refusing to run unverified"),
                "fail-closed wording missing: {msg}"
            );
        }
    }

    #[test]
    fn mismatch_message_includes_source_url() {
        let msg = VerificationFailure::Mismatch {
            expected: DIGEST_A.to_string(),
            actual: DIGEST_B.to_string(),
        }
        .describe("https://github.com/x/y/releases/download/v1.2.3/rsc-ls-aarch64-apple-darwin");
        assert!(msg.contains("v1.2.3/rsc-ls-aarch64-apple-darwin"));
    }
}
