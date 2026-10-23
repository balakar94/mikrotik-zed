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

use crate::platform;
use crate::sha256;
use crate::verify::{MAX_VERIFIED_BINARY_BYTES, short_digest};

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic tag source for staging and marker temp names. Combined with
/// [`platform::process_seed`] it keeps concurrent downloads and retries from
/// sharing a path, without needing entropy or wall-clock access in the WASM
/// component.
static INSTALL_TAG: AtomicU64 = AtomicU64::new(0);

/// Unique tag for one staging or temp file (`<seed>-<counter>`).
///
/// The seed comes from [`platform::process_seed`] (the process id natively, a
/// fixed constant in the shipped wasm32-wasip2 component, where
/// `std::process::id()` is unimplemented and panics at runtime). Uniqueness
/// within the process comes from the monotonic counter; collisions with
/// residue from a previous instance are safe because both temp-file
/// producers (the staging download in `lib.rs`, [`write_marker`] below)
/// unlink any pre-existing file at the path before writing.
fn unique_tag() -> String {
    format!(
        "{}-{}",
        platform::process_seed(),
        INSTALL_TAG.fetch_add(1, Ordering::Relaxed)
    )
}

/// Staging path for a fresh download (`<stored>.part-<tag>`).
///
/// The host writes downloads non-atomically, so bytes always land here first
/// and move to `stored_name` only after verification and chmod.
pub(crate) fn staging_path(stored_name: &str) -> String {
    format!("{stored_name}.part-{}", unique_tag())
}

/// Best-effort removal of a staging file. Missing files are the common case
/// (a successful install renames the staging file away), so failures stay
/// silent: leftovers cost disk space, never correctness.
pub(crate) fn remove_staging(path: &str) {
    let _ = std::fs::remove_file(path);
}

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
/// The write is atomic: bytes land in `<marker>.tmp-<tag>` (mode
/// 0600 on unix) and are renamed over the marker, so a crash never leaves a
/// half-written marker behind.
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
    let marker = marker_path(stored_name);
    let tmp = format!("{marker}.tmp-{}", unique_tag());
    // Unlink residue first: `write` would otherwise truncate a symlink target
    // through a pre-existing link at the temp path.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, format!("{digest_hex}\n"))
        .map_err(|e| format!("could not write integrity marker for {stored_name}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "could not secure integrity marker for {stored_name}"
            ));
        }
    }
    platform::atomic_replace(&tmp, &marker).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not record integrity marker for {stored_name}: {e}")
    })
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

/// Suffix appended to the stored binary name to form its release-origin file.
const ORIGIN_SUFFIX: &str = ".origin";

/// Longest release tag accepted verbatim. GitHub allows longer refs, but a
/// tag beyond this is outside anything this repository publishes and only
/// widens what the reader has to sanitize.
const MAX_ORIGIN_TAG_LEN: usize = 64;

/// Path of the release-origin record for `stored_name`.
pub(crate) fn origin_path(stored_name: &str) -> String {
    format!("{stored_name}{ORIGIN_SUFFIX}")
}

/// Records which release tag `stored_name` was downloaded from.
///
/// Advisory, never integrity: the `.verified` marker still certifies the
/// bytes, so callers must degrade a failure here to a warning. Its single job
/// is to keep a version mismatch observable — the stored name carries the
/// *extension* version, while the asset can come from the latest-stable
/// fallback, so a file named `rsc-ls-0.7.0` may well hold 0.6.1.
///
/// The write is atomic (`<path>.tmp-<tag>` then rename, mode 0600 on unix),
/// mirroring [`write_marker`]: a crash leaves either the old tag or the new
/// one, never a truncated line.
pub(crate) fn write_origin(stored_name: &str, tag: &str) -> std::result::Result<(), String> {
    if tag.is_empty() || tag.len() > MAX_ORIGIN_TAG_LEN {
        return Err(format!(
            "release tag is {} bytes, expected 1..={MAX_ORIGIN_TAG_LEN}",
            tag.len()
        ));
    }
    if !tag.bytes().all(is_origin_tag_byte) {
        return Err("release tag contains characters outside [A-Za-z0-9._+-]".to_string());
    }
    let path = origin_path(stored_name);
    let tmp = format!("{path}.tmp-{}", unique_tag());
    // Unlink residue first: `write` would otherwise truncate a symlink target
    // through a pre-existing link at the temp path.
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, format!("{tag}\n"))
        .map_err(|e| format!("could not write release origin for {stored_name}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("could not secure release origin for {stored_name}"));
        }
    }
    platform::atomic_replace(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not record release origin for {stored_name}: {e}")
    })
}

/// Reads the release tag recorded for `stored_name`.
///
/// Returns `None` on a missing file, unreadable bytes, or any content the
/// writer would never produce (empty, oversize, foreign characters). Absence
/// is not an error: entries cached before this record existed simply have no
/// origin, and erroring would make every legacy cache entry look tampered.
pub(crate) fn read_origin_tag(stored_name: &str) -> Option<String> {
    use std::io::Read;

    let file = std::fs::File::open(origin_path(stored_name)).ok()?;
    // Read one byte past the cap so trailing junk is detected, not truncated
    // into a plausible tag.
    let mut limited = file.take((MAX_ORIGIN_TAG_LEN + 2) as u64);
    let mut raw = String::new();
    if limited.read_to_string(&mut raw).is_err() {
        return None;
    }
    let tag = raw.strip_suffix('\n').unwrap_or(&raw);
    if tag.is_empty() || tag.len() > MAX_ORIGIN_TAG_LEN {
        return None;
    }
    if !tag.bytes().all(is_origin_tag_byte) {
        return None;
    }
    Some(tag.to_string())
}

/// Byte class accepted in a recorded release tag: what `v0.7.0`,
/// `v0.7.0-rc.1` and `v0.7.0+build.5` need, nothing that could alter a path or
/// a log line.
fn is_origin_tag_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_')
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
            let _ = std::fs::remove_file(origin_path(&self.0));
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
    fn origin_path_appends_its_own_suffix() {
        assert_eq!(origin_path("rsc-ls-0.7.0"), "rsc-ls-0.7.0.origin");
        assert_eq!(origin_path("rsc-ls-0.7.0.exe"), "rsc-ls-0.7.0.exe.origin");
        // The two sidecars must never collide: the origin is written after the
        // marker, and a shared path would clobber the digest gate.
        assert_ne!(origin_path("rsc-ls-0.7.0"), marker_path("rsc-ls-0.7.0"));
    }

    #[test]
    fn origin_roundtrip_preserves_prerelease_tags() {
        let bin = CachedFile(temp_path("origin-roundtrip"));
        assert_eq!(read_origin_tag(&bin.0), None, "no record before a download");
        for tag in ["v0.6.1", "v0.7.0-rc.1", "v0.7.0+build.5"] {
            write_origin(&bin.0, tag).unwrap();
            assert_eq!(read_origin_tag(&bin.0).as_deref(), Some(tag));
        }
    }

    #[test]
    fn write_origin_rejects_tags_it_cannot_round_trip() {
        let bin = CachedFile(temp_path("origin-reject"));
        let long = "v".repeat(MAX_ORIGIN_TAG_LEN + 1);
        for tag in [
            "",
            "v1.0.0 extra",
            "v1.0.0\nv2.0.0",
            "v1.0.0;rm",
            long.as_str(),
        ] {
            assert!(write_origin(&bin.0, tag).is_err(), "must reject {tag:?}");
            assert_eq!(read_origin_tag(&bin.0), None, "rejected {tag:?} was kept");
        }
    }

    #[test]
    fn read_origin_rejects_content_the_writer_never_emits() {
        let bin = CachedFile(temp_path("origin-foreign"));
        for body in [
            "\n".to_string(),
            "v1.0.0\nv2.0.0\n".to_string(),
            format!("{}\n", "v".repeat(MAX_ORIGIN_TAG_LEN + 1)),
            "v1.0.0\t".to_string(),
        ] {
            std::fs::write(origin_path(&bin.0), body.as_bytes()).unwrap();
            assert_eq!(read_origin_tag(&bin.0), None, "accepted {body:?}");
        }
        // Non-UTF-8 bytes are unreadable as text and must not become a tag.
        std::fs::write(origin_path(&bin.0), [0xff, 0xfe]).unwrap();
        assert_eq!(read_origin_tag(&bin.0), None);
    }

    #[test]
    fn staging_paths_are_unique_per_call() {
        let first = staging_path("rsc-ls-0.5.0");
        let second = staging_path("rsc-ls-0.5.0");
        assert!(first.starts_with("rsc-ls-0.5.0.part-"));
        assert!(second.starts_with("rsc-ls-0.5.0.part-"));
        assert_ne!(first, second);
    }

    #[test]
    fn staging_path_matches_the_seed_counter_contract() {
        let path = staging_path("rsc-ls-0.5.0");
        let tag = path
            .strip_prefix("rsc-ls-0.5.0.part-")
            .expect("staging path must keep the .part- prefix");
        let (seed, counter) = tag.split_once('-').expect("tag must be <seed>-<counter>");
        assert!(seed.parse::<u32>().is_ok(), "seed must be a u32: {seed}");
        assert!(
            counter.parse::<u64>().is_ok(),
            "counter must be a u64: {counter}"
        );
    }

    #[test]
    fn remove_staging_tolerates_missing_files() {
        remove_staging("definitely-missing-rsc-ls-part-file");
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
            (format!("{}g\n", "f".repeat(63)), "non-hex"),
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
