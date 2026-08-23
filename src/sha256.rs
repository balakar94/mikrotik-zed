//! Pure-Rust SHA-256 (FIPS 180-4) and release-asset digest helpers.
//!
//! Deliberately dependency-free (repo rule: `zed_extension_api` + std only).
//! Used by the auto-download path in [`crate`] to verify GitHub Release
//! binaries against their `<asset>.sha256` companions before execution.
//!
//! Scope guard: this module owns everything digest-shaped — the hash itself,
//! parsing of `sha256sum`-format companion files, and digest comparison.
//! Anything involving I/O, the network, or the Zed API stays in [`crate`].

/// Round constants: fractional parts of cube roots of the first 64 primes
/// (FIPS 180-4, section 4.2.2).
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Initial hash value: fractional parts of square roots of the first 8 primes
/// (FIPS 180-4, section 5.3.3).
const INITIAL_STATE: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Computes the SHA-256 digest of `data` as lowercase hex (64 characters).
///
/// One-shot by design: callers hold the full artifact in memory behind an
/// explicit size cap, so a streaming incremental API would be unused surface.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut state = INITIAL_STATE;

    let mut blocks = data.chunks_exact(64);
    for block in blocks.by_ref() {
        compress(
            &mut state,
            block.try_into().expect("chunks_exact yields 64 bytes"),
        );
    }
    let remainder = blocks.remainder();
    debug_assert!(remainder.len() < 64);

    // Padding (FIPS 180-4, section 5.1.1): append 0x80, zero-fill up to the
    // last 8 bytes of a block boundary, then the big-endian bit length.
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let padded_len = if remainder.len() + 9 <= 64 { 64 } else { 128 };
    let bit_len: u64 = u64::try_from(data.len())
        .ok()
        .and_then(|len| len.checked_mul(8))
        .expect("caller enforces a size cap far below the SHA-256 length encoding limit");
    tail[padded_len - 8..padded_len].copy_from_slice(&bit_len.to_be_bytes());

    for block in tail[..padded_len].chunks_exact(64) {
        compress(
            &mut state,
            block.try_into().expect("tail blocks are exactly 64 bytes"),
        );
    }

    let mut out = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            out.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
            out.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

/// Runs the 64-round compression function over one 512-bit block.
fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_be_bytes(chunk.try_into().expect("chunks_exact(4) yields 4 bytes"));
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (slot, round) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(round);
    }
}

/// Parses the expected digest out of a `sha256sum`-format companion file.
///
/// Accepts the standard layout `<lowercase-hex>␠␠<filename>` and tolerates a
/// trailing newline or CRLF, any run of whitespace around the digest, and
/// uppercase hex digits (normalized to lowercase). Anything else — empty
/// content, whitespace-only content, wrong length, non-hex characters — is
/// rejected so that HTML error pages and truncated downloads fail closed.
///
/// Returns the normalized 64-character lowercase hex digest.
pub(crate) fn parse_digest_companion(content: &str) -> Result<String, String> {
    let Some(token) = content.split_whitespace().next() else {
        return Err("companion content is empty or whitespace-only".to_string());
    };
    if token.len() != 64 {
        return Err(format!(
            "digest token is {} characters, expected 64",
            token.len()
        ));
    }
    if !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("digest token contains non-hexadecimal characters".to_string());
    }
    Ok(token.to_ascii_lowercase())
}

/// Compares two digest strings after trimming surrounding whitespace and
/// ignoring hex case. Both sides are expected to come from validated sources
/// ([`parse_digest_companion`] and [`sha256_hex`] respectively).
pub(crate) fn digests_match(expected: &str, actual: &str) -> bool {
    expected.trim().eq_ignore_ascii_case(actual.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_digest(input: &[u8], expected: &str) {
        assert_eq!(sha256_hex(input), expected);
    }

    #[test]
    fn empty_input_matches_official_vector() {
        // FIPS 180-4 / NIST: SHA256("") — also `shasum -a 256 /dev/null`.
        assert_digest(
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
    }

    #[test]
    fn abc_matches_official_vector() {
        // FIPS 180-4 / NIST: SHA256("abc").
        assert_digest(
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn two_block_nist_vector_matches() {
        // FIPS 180-4 / NIST two-block (448-bit message) vector.
        assert_digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        );
    }

    #[test]
    fn million_a_matches_official_vector() {
        // FIPS 180-4 / NIST: SHA256('a' repeated 1,000,000 times).
        assert_digest(
            &[b'a'; 1_000_000],
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        );
    }

    #[test]
    fn multi_block_padding_boundary_matches_shasum() {
        // 504 bytes = 7 full blocks + 56-byte remainder, forcing padding to
        // spill into a second block (the boundary most likely to be off-by-one).
        //
        // Input:      'rsc-ls supply-chain verification vector (mikrotik-zed). '
        //             repeated 9 times (504 bytes total).
        // Cross-check (agrees):
        //   $ shasum -a 256   -> c910817d7f4143c1bfcdf1f154e8eeb07e25806f5647527e773494c2ba735749
        //   $ python hashlib  -> c910817d7f4143c1bfcdf1f154e8eeb07e25806f5647527e773494c2ba735749
        assert_digest(
            b"rsc-ls supply-chain verification vector (mikrotik-zed). "
                .repeat(9)
                .as_slice(),
            "c910817d7f4143c1bfcdf1f154e8eeb07e25806f5647527e773494c2ba735749",
        );
    }

    #[test]
    fn output_is_always_lowercase_hex_of_fixed_length() {
        // Deterministic byte pattern spanning the full u8 range (incl. 0x00/0xFF).
        let input: Vec<u8> = core::iter::repeat_n([0x00u8, 0x7f, 0x80, 0xff], 257)
            .flatten()
            .collect();
        let digest = sha256_hex(&input);
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(digest.chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn parses_standard_sha256sum_layout_with_two_spaces() {
        // Standard sha256sum output: "<hex>␠␠<filename>".
        assert_eq!(
            parse_digest_companion(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  rsc-ls-aarch64-apple-darwin"
            )
            .unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn parses_companion_with_trailing_newline_and_crlf() {
        let lf = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  rsc-ls\n";
        let crlf = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  rsc-ls\r\n";
        assert_eq!(
            parse_digest_companion(lf).unwrap(),
            parse_digest_companion(crlf).unwrap()
        );
    }

    #[test]
    fn parses_companion_with_single_space_and_surrounding_whitespace() {
        assert_eq!(
            parse_digest_companion(
                "  \n 248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1 rsc-ls\t"
            )
            .unwrap(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn normalizes_uppercase_hex_to_lowercase() {
        let upper = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855  rsc-ls";
        assert_eq!(
            parse_digest_companion(upper).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn rejects_empty_companion() {
        assert!(parse_digest_companion("").is_err());
    }

    #[test]
    fn rejects_whitespace_only_companion() {
        assert!(parse_digest_companion(" \r\n\t ").is_err());
    }

    #[test]
    fn rejects_wrong_length_digest() {
        let short = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8  x";
        assert!(parse_digest_companion(short).is_err());
    }

    #[test]
    fn rejects_non_hex_token() {
        // What a GitHub HTML error page would look like if fetched by mistake.
        let garbage = "<!DOCTYPE html>  404";
        assert!(parse_digest_companion(garbage).is_err());
    }

    #[test]
    fn comparison_is_case_and_whitespace_insensitive() {
        let upper = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        let lower = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(digests_match(upper, lower));
        assert!(digests_match(&format!("{lower}\n"), upper));
    }

    #[test]
    fn comparison_detects_single_character_difference() {
        let base = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let flipped = "f3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(!digests_match(base, flipped));
    }
}
