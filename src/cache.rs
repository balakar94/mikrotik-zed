//! Integrity markers for binaries cached in the extension work dir.
//!
//! Problem this closes: the Zed host writes `download_file` output
//! non-atomically, so a mid-transfer network failure used to leave a
//! truncated file at the final path — and an existence-only reuse gate then
//! served that corrupt file forever (a doomed spawn loop).
//!
//! Mechanism: right after a fresh download passes checksum verification and
//! is made executable, the caller records a marker file
//! (`<stored-name>.verified`) containing the exact SHA-256 digest that was
//! verified. Every later reuse re-hashes the cached bytes and compares them
//! against the marker before spawning, so truncation, on-disk corruption, or
//! silent replacement is detected and healed by a fresh download instead of
//! being respawned indefinitely.
//!
//! Scope guard: this module owns marker I/O and cached-binary integrity
//! checks. Download/verification policy stays in [`crate::verify`],
//! orchestration (status transitions, cleanup calls) in [`crate`], and the
//! hash primitive in [`crate::sha256`].

use crate::sha256;
use crate::verify::{MAX_VERIFIED_BINARY_BYTES, short_digest};

/// Suffix appended to the stored binary name to form the marker file name.
const MARKER_SUFFIX: &str = ".verified";

/// Exact length of a SHA-256 digest rendered as hex.
const DIGEST_HEX_LEN: usize = 64;

/// Largest marker file ever read: one digest plus an optional trailing
/// newline. Bounds the read so a hostile or corrupt file at the marker path
/// cannot force an unbounded allocation.
const MAX_MARKER_BYTES: u64 = (DIGEST_HEX_LEN + 1) as u64;

/// Path of the integrity marker that certifies `stored_name`.
pub(crate) fn marker_path(stored_name: &str) -> String {
    format!("{stored_name}{MARKER_SUFFIX}")
}

/// Writes the integrity marker for `stored_name` as exactly
/// `<64-char lowercase sha256 hex>\n`.
///
/// `digest_hex` must already be the normalized digest produced by the
/// SHA-256 helper ([`crate::sha256::sha256_file_hex`]); the strict validation
/// below guarantees a marker is never persisted that
/// [`read_marker_digest`] would reject.
///
/// Failing here must fail closed: the caller removes both the binary and the
/// marker rather than keeping a cache entry it could not certify.
pub(crate) fn write_marker(stored_name: &str, digest_hex: &str) -> std::result::Result<(), String> {
    if digest_hex.len() != DIGEST_HEX_LEN {
        return Err(format!(
            "digest is {} characters, expected {DIGEST_HEX_LEN}",
            digest_hex.len()
        ));
    }
    if !digest_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("digest contains non-hexadecimal characters".to_string());
    }
    if digest_hex.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err("digest must be lowercase hexadecimal".to_string());
    }
    std::fs::write(marker_path(stored_name), format!("{digest_hex}\n"))
        .map_err(|e| format!("could not write integrity marker for {stored_name}: {e}"))
}

/// Reads the digest recorded in `stored_name`'s marker.
///
/// Returns `None` on a missing file or on any deviation from the written
/// format (`<64-char hex>` plus at most one trailing newline): wrong length,
/// non-hex characters, or trailing junk. Uppercase hex is tolerated on read
/// (comparison downstream is case-insensitive) but is never written.
pub(crate) fn read_marker_digest(stored_name: &str) -> Option<String> {
    read_marker_file(&marker_path(stored_name))
}

/// Bounded reader for a marker file; see [`read_marker_digest`].
fn read_marker_file(path: &str) -> Option<String> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    // Read at most one byte past the cap: `read_to_string` on an
    // unauthenticated local file would otherwise allocate its full length.
    let mut limited = file.take(MAX_MARKER_BYTES + 1);
    let mut raw = String::new();
    if limited.read_to_string(&mut raw).is_err() {
        return None;
    }
    if raw.len() as u64 > MAX_MARKER_BYTES {
        return None;
    }
    parse_marker_content(&raw)
}

/// Strict parser for marker contents; see [`read_marker_digest`].
fn parse_marker_content(raw: &str) -> Option<String> {
    let body = raw.strip_suffix('\n').unwrap_or(raw);
    if body.len() != DIGEST_HEX_LEN || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(body.to_ascii_lowercase())
}

/// True when the cached binary still hashes back to the digest recorded when
/// verification passed, and stays within the verification size cap.
///
/// A missing binary, missing/corrupt marker, oversized file, unreadable
/// bytes, or any digest mismatch all report `false`; callers heal by removing
/// the pair and downloading afresh.
pub(crate) fn cached_binary_is_intact(stored_name: &str) -> bool {
    integrity_problem(stored_name).is_none()
}

/// Why [`cached_binary_is_intact`] would reject `stored_name`, or `None` when
/// the binary is intact.
///
/// The reason strings are safe for logs/status text: they carry only short
/// digest prefixes ([`short_digest`]), never full hashes.
pub(crate) fn integrity_problem(stored_name: &str) -> Option<String> {
    integrity_problem_with_cap(stored_name, MAX_VERIFIED_BINARY_BYTES)
}

/// Core integrity check with an injectable size cap (tests shrink it to stay
/// practical; production always passes [`MAX_VERIFIED_BINARY_BYTES`]).
///
/// OOM hardening: the cap is enforced *during* a streaming hash
/// ([`sha256::sha256_file_hex`]) rather than around a whole-file
/// `std::fs::read`. A metadata pre-check alone is TOCTOU-unsafe: a file that
/// grows between `metadata()` and `read()` would still be allocated in full.
/// Streaming bounds peak heap regardless of concurrent writers; a torn or
/// swapped read simply hashes to a different digest and triggers self-healing.
fn integrity_problem_with_cap(stored_name: &str, max_bytes: u64) -> Option<String> {
    let Some(expected) = read_marker_digest(stored_name) else {
        return Some(format!(
            "integrity marker {} is missing or malformed",
            marker_path(stored_name)
        ));
    };
    let actual = match sha256::sha256_file_hex(stored_name, max_bytes) {
        Ok(digest) => digest,
        Err(sha256::FileHashError::TooLarge(observed)) => {
            return Some(format!(
                "cached binary {stored_name} has {observed}+ bytes, above the {max_bytes}-byte verification cap"
            ));
        }
        Err(sha256::FileHashError::Io(e)) => {
            return Some(format!("cached binary {stored_name} is unreadable: {e}"));
        }
    };
    if !sha256::digests_match(&expected, &actual) {
        return Some(format!(
            "cached binary {stored_name} hashes to {}… but its marker records {}…",
            short_digest(&actual),
            short_digest(&expected)
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp path per test tag (platform.rs pattern: unique names,
    /// best-effort cleanup). The Drop guard below makes cleanup hold even
    /// when an assertion fails midway through a test.
    fn temp_path(tag: &str) -> String {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mikrotik-zed-cache-{tag}-{}.tmp",
            std::process::id()
        ));
        path.to_string_lossy().into_owned()
    }

    /// Owns a temp "cached binary" plus its marker; removes both on drop.
    struct CachedFile(String);

    impl CachedFile {
        fn write(&self, bytes: &[u8]) {
            std::fs::write(&self.0, bytes).unwrap();
        }
    }

    impl Drop for CachedFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(marker_path(&self.0));
        }
    }

    const BYTES_A: &[u8] = b"rsc-ls cache integrity vector A";
    const BYTES_B: &[u8] = b"rsc-ls cache integrity vector B -- tampered";

    fn digest_of(bytes: &[u8]) -> String {
        sha256::sha256_hex(bytes)
    }

    #[test]
    fn marker_path_appends_verified_suffix() {
        assert_eq!(marker_path("rsc-ls-0.5.0"), "rsc-ls-0.5.0.verified");
        assert_eq!(marker_path("rsc-ls-0.5.0.exe"), "rsc-ls-0.5.0.exe.verified");
    }

    #[test]
    fn marker_roundtrip_preserves_digest_and_exact_format() {
        let bin = CachedFile(temp_path("roundtrip"));
        bin.write(BYTES_A);
        let digest = digest_of(BYTES_A);

        write_marker(&bin.0, &digest).unwrap();
        assert_eq!(read_marker_digest(&bin.0).as_deref(), Some(digest.as_str()));
        // On-disk format contract: exactly "<64-char lowercase hex>\n".
        let on_disk = std::fs::read_to_string(marker_path(&bin.0)).unwrap();
        assert_eq!(on_disk, format!("{digest}\n"));
        // Matching bytes + matching marker => gate opens.
        assert!(cached_binary_is_intact(&bin.0));
        assert_eq!(integrity_problem(&bin.0), None);
    }

    #[test]
    fn write_marker_rejects_malformed_digests_without_writing() {
        let bin = CachedFile(temp_path("reject"));
        let good = digest_of(BYTES_A);

        assert!(write_marker(&bin.0, &"a".repeat(63)).is_err(), "too short");
        assert!(write_marker(&bin.0, &"a".repeat(65)).is_err(), "too long");
        assert!(
            write_marker(&bin.0, &format!("{}g", "f".repeat(63))).is_err(),
            "non-hex"
        );
        assert!(
            write_marker(&bin.0, &good.to_ascii_uppercase()).is_err(),
            "uppercase"
        );
        assert!(write_marker(&bin.0, "").is_err(), "empty");

        // Nothing may have been written by any of the rejected attempts.
        assert_eq!(read_marker_digest(&bin.0), None);
    }

    #[test]
    fn missing_marker_reads_as_none_and_fails_the_gate() {
        let bin = CachedFile(temp_path("absent-marker"));
        bin.write(BYTES_A);
        assert_eq!(read_marker_digest(&bin.0), None);
        assert!(!cached_binary_is_intact(&bin.0));
    }

    #[test]
    fn corrupt_markers_read_as_none_and_fail_the_gate() {
        let cases: [(String, &str); 7] = [
            (String::new(), "empty"),
            (format!("{}\n", "a".repeat(63)), "short"),
            (format!("{}\n", "a".repeat(65)), "long"),
            (format!("{}\n", format!("{}g", "f".repeat(63))), "non-hex"),
            ("<html><body>404</body></html>\n".to_string(), "garbage"),
            (
                format!("{}extra\n", digest_of(BYTES_A)),
                "trailing junk beyond newline",
            ),
            (format!("{}\r\n", digest_of(BYTES_A)), "crlf counts as junk"),
        ];
        for (content, label) in cases {
            let bin = CachedFile(temp_path("corrupt"));
            bin.write(BYTES_A);
            std::fs::write(marker_path(&bin.0), content).unwrap();
            assert!(
                read_marker_digest(&bin.0).is_none(),
                "{label} marker must read as None"
            );
            assert!(
                !cached_binary_is_intact(&bin.0),
                "{label} marker must fail the reuse gate"
            );
        }
    }

    #[test]
    fn oversized_marker_file_reads_as_none() {
        let bin = CachedFile(temp_path("huge-marker"));
        bin.write(BYTES_A);
        // A marker far larger than any valid digest (e.g. an HTML error page
        // or a hostile file) must be rejected without an unbounded read.
        std::fs::write(marker_path(&bin.0), "a".repeat(10_000)).unwrap();
        assert_eq!(read_marker_digest(&bin.0), None);
        assert!(!cached_binary_is_intact(&bin.0));
    }

    #[test]
    fn tampered_bytes_fail_the_gate_and_reason_hides_full_hashes() {
        let bin = CachedFile(temp_path("tamper"));
        bin.write(BYTES_A);
        write_marker(&bin.0, &digest_of(BYTES_A)).unwrap();
        assert!(cached_binary_is_intact(&bin.0));

        bin.write(BYTES_B);
        assert!(!cached_binary_is_intact(&bin.0));
        let reason = integrity_problem(&bin.0).expect("tampered binary must yield a reason");
        assert!(
            reason.contains("hashes to"),
            "unexpected reason wording: {reason}"
        );
        assert!(
            !reason.contains(&digest_of(BYTES_B)),
            "full hashes must never be logged"
        );
    }

    #[test]
    fn missing_binary_fails_the_gate_even_with_a_marker() {
        let bin = CachedFile(temp_path("missing-bin"));
        // Marker exists; the binary was never written (or was swept away).
        write_marker(&bin.0, &digest_of(BYTES_A)).unwrap();
        assert!(!cached_binary_is_intact(&bin.0));
    }

    #[test]
    fn oversized_binary_is_rejected_against_the_injected_cap() {
        let bin = CachedFile(temp_path("oversize"));
        bin.write(BYTES_A); // 31 bytes
        write_marker(&bin.0, &digest_of(BYTES_A)).unwrap();

        // With a valid marker in place, a tiny injected cap flags the file
        // without needing a 256 MiB fixture.
        let problem =
            integrity_problem_with_cap(&bin.0, 8).expect("tiny cap must flag the oversized file");
        assert!(
            problem.contains("bytes") && problem.contains("cap"),
            "unexpected reason wording: {problem}"
        );

        // The production cap accepts the same small file.
        assert_eq!(integrity_problem(&bin.0), None);
    }

    #[test]
    fn cap_is_enforced_across_streaming_chunks() {
        // A multi-chunk file (well past the 32 KiB streaming chunk) with a cap
        // between one and two chunks: the streaming hash must abort at the cap
        // instead of buffering the whole file. If hashing regressed to a
        // whole-file read, this still passes byte-wise but the memory bound
        // would be lost; the assertion pins the fail-closed verdict and the
        // bounded-read reason.
        let bin = CachedFile(temp_path("multi-chunk"));
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        bin.write(&bytes);
        write_marker(&bin.0, &digest_of(&bytes)).unwrap();

        let problem = integrity_problem_with_cap(&bin.0, 40 * 1024)
            .expect("100 KiB file must exceed a 40 KiB cap");
        assert!(
            problem.contains("above the") && problem.contains("verification cap"),
            "unexpected reason wording: {problem}"
        );
        // The same bytes pass at the production cap.
        assert_eq!(integrity_problem(&bin.0), None);
    }
}
