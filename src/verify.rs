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

/// Sanity cap for hashing a downloaded binary for verification.
/// Real `rsc-ls` artifacts are a few MiB; 64 MiB leaves an order of
/// magnitude of headroom while keeping WASM linear memory bounded.
/// Anything beyond this fails closed rather than being hashed.
pub(crate) const MAX_VERIFIED_BINARY_BYTES: u64 = 64 * 1024 * 1024;

/// Number of hex characters shown in logs/status messages for each digest.
/// Full hashes are never emitted (log hygiene).
const DIGEST_LOG_PREFIX: usize = 12;

/// Why checksum verification refused a freshly downloaded binary.
///
/// Variant order mirrors the pipeline stages; every variant fails closed at
/// the call site.
#[derive(Debug)]
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
        let companion_url = companion_url(source_url);
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

/// Builds the `<asset>.sha256` companion URL for a release download URL.
///
/// Pure constructor (`{url}.sha256`); fetching and parsing stay with the
/// callers so this stays unit-testable without HTTP.
pub(crate) fn companion_url(download_url: &str) -> String {
    format!("{download_url}.sha256")
}

/// Fetches `{download_url}.sha256` through the Zed extension host HTTP client
/// and returns the expected digest parsed from the companion content.
fn fetch_companion_digest(download_url: &str) -> std::result::Result<String, VerificationFailure> {
    let companion_url = companion_url(download_url);
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

/// Hashes `binary_name` in streaming chunks under `max_bytes`.
///
/// Thin fail-closed wrapper over [`crate::sha256::sha256_file_hex`]:
/// oversized artifacts map to [`VerificationFailure::BinaryTooLarge`],
/// unreadable ones to [`VerificationFailure::BinaryRead`]. The injectable
/// cap keeps tests practical (tiny caps flag small fixtures); production
/// always passes [`MAX_VERIFIED_BINARY_BYTES`].
fn hash_binary_capped(
    binary_name: &str,
    max_bytes: u64,
) -> std::result::Result<String, VerificationFailure> {
    match sha256::sha256_file_hex(binary_name, max_bytes) {
        Ok(digest) => Ok(digest),
        Err(sha256::FileHashError::TooLarge(observed)) => {
            Err(VerificationFailure::BinaryTooLarge(observed))
        }
        Err(sha256::FileHashError::Io(detail)) => Err(VerificationFailure::BinaryRead(detail)),
    }
}

/// Verifies the just-downloaded binary against its `.sha256` companion.
///
/// `binary_name` is the work-dir-relative path `download_file` wrote to.
/// Read-back stays within the extension work dir (repo hard rule #7).
///
/// On success returns the verified digest (full lowercase hex); on any
/// failure returns a [`VerificationFailure`] — callers must fail closed.
pub(crate) fn verify_downloaded_binary(
    binary_name: &str,
    download_url: &str,
) -> std::result::Result<String, VerificationFailure> {
    let expected = fetch_companion_digest(download_url)?;

    // Cap before reading: refuse absurdly large artifacts instead of
    // buffering them inside the WASM component. The streaming hash below
    // re-enforces the same cap during the read (TOCTOU-safe).
    let size = std::fs::metadata(binary_name)
        .map_err(|e| VerificationFailure::BinaryRead(e.to_string()))?
        .len();
    if size > MAX_VERIFIED_BINARY_BYTES {
        return Err(VerificationFailure::BinaryTooLarge(size));
    }

    let actual = hash_binary_capped(binary_name, MAX_VERIFIED_BINARY_BYTES)?;

    if !sha256::digests_match(&expected, &actual) {
        return Err(VerificationFailure::Mismatch { expected, actual });
    }
    Ok(actual)
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
    fn companion_url_appends_sha256_suffix() {
        assert_eq!(
            companion_url(
                "https://github.com/x/y/releases/download/v1.2.3/rsc-ls-aarch64-apple-darwin"
            ),
            "https://github.com/x/y/releases/download/v1.2.3/rsc-ls-aarch64-apple-darwin.sha256"
        );
        assert_eq!(
            companion_url("https://example.com/asset"),
            "https://example.com/asset.sha256"
        );
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

    #[test]
    fn verification_cap_is_64_mib() {
        // Real rsc-ls artifacts are a few MiB; 64 MiB leaves headroom while
        // keeping WASM linear memory bounded (was 256 MiB).
        assert_eq!(MAX_VERIFIED_BINARY_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn streaming_hash_matches_one_shot_hash() {
        // Chunked file hashing must agree byte-for-byte with the in-memory
        // one-shot helper, including multi-block inputs.
        let path = std::env::temp_dir().join(format!(
            "mikrotik-zed-verify-stream-{}.tmp",
            std::process::id()
        ));
        let path_str = path.to_string_lossy().into_owned();
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let streamed = hash_binary_capped(&path_str, MAX_VERIFIED_BINARY_BYTES).unwrap();
        assert_eq!(streamed, sha256::sha256_hex(&bytes));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn oversized_binary_is_rejected_against_the_injected_cap() {
        // Mirrors cache.rs oversize pattern: a tiny injected cap flags a
        // small fixture without needing a 64 MiB file on disk.
        let path = std::env::temp_dir().join(format!(
            "mikrotik-zed-verify-oversize-{}.tmp",
            std::process::id()
        ));
        let path_str = path.to_string_lossy().into_owned();
        std::fs::write(&path, b"rsc-ls verify size vector").unwrap();
        let err = hash_binary_capped(&path_str, 8).expect_err("tiny cap must flag the file");
        match err {
            VerificationFailure::BinaryTooLarge(size) => assert!(size > 8),
            other => panic!(
                "expected BinaryTooLarge, got {}",
                other.describe("https://example.com/asset")
            ),
        }
        // The production cap accepts the same small file.
        assert!(hash_binary_capped(&path_str, MAX_VERIFIED_BINARY_BYTES).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_binary_fails_closed_as_binary_read() {
        let missing = std::env::temp_dir().join(format!(
            "mikrotik-zed-verify-missing-{}-{}.tmp",
            std::process::id(),
            "absent"
        ));
        let err = hash_binary_capped(&missing.to_string_lossy(), MAX_VERIFIED_BINARY_BYTES)
            .expect_err("missing file must fail");
        let msg = err.describe("https://example.com/asset");
        assert!(msg.contains("could not be read"));
        assert!(msg.contains("Refusing to run unverified"));
    }
}
