// ── Live network trust ───────────────────────────────────────────────────
//
// Host validation + SSRF deny, DNS revalidation, TLS pin/CA, SHA256/SPKI, `LiveError`.
//
// Split from `live.rs`; re-exported there, `crate::live::…` paths unchanged.

use crate::live_cache::ResourceKind;
use crate::live_config::{CustomResource, LiveConfig};
use crate::live_fetch::parse_pem_certs;
use crate::logging::{log_debug, log_warn, sanitize_for_log};
use std::collections::HashSet;
use std::io::Read;
use std::sync::{Mutex, OnceLock};

// ── Minimal SHA256 + SPKI extraction (no new deps) ───────────────────────
//
// The pin compares `SHA256(DER(subjectPublicKeyInfo))` of the leaf cert
// (RFC 7469 style). `ring` is not a direct dependency, so a compact pure-Rust
// SHA256 is vendored here (~64 rounds, standard constants). SPKI bytes are
// located with a minimal DER TLV walker (fail-closed `None` on malformed).

pub(crate) fn sha256_block(state: &mut [u32; 8], block: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut w = [0u32; 64];
    for (i, c) in block.chunks(4).enumerate().take(16) {
        w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
        state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7],
    );
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
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
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// SHA256 over `data` (pure Rust, no extra dependency).
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks(64) {
        let mut arr = [0u8; 64];
        arr.copy_from_slice(block);
        sha256_block(&mut state, &arr);
    }
    let mut out = [0u8; 32];
    for (i, v) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Read one DER TLV at `data[pos..]`; returns `(tag, header_len, value_len, value_start)`.
fn der_tlv(data: &[u8], pos: usize) -> Option<(u8, usize, usize, usize)> {
    if pos + 2 > data.len() {
        return None;
    }
    let tag = data[pos];
    let first = data[pos + 1];
    if first & 0x80 == 0 {
        let len = (first & 0x7f) as usize;
        let start = pos + 2;
        if start + len > data.len() {
            return None;
        }
        return Some((tag, 2, len, start));
    }
    let nbytes = (first & 0x7f) as usize;
    if nbytes == 0 || nbytes > 4 || pos + 2 + nbytes > data.len() {
        return None;
    }
    let mut len: usize = 0;
    for b in &data[pos + 2..pos + 2 + nbytes] {
        len = len.checked_mul(256)?.checked_add(*b as usize)?;
    }
    let start = pos + 2 + nbytes;
    if start + len > data.len() {
        return None;
    }
    Some((tag, 2 + nbytes, len, start))
}

/// Extract the full DER TLV of `subjectPublicKeyInfo` from a DER certificate.
///
/// Walks `Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... } }` and
/// returns the SPKI SEQUENCE TLV (tag + length + value). Fail-closed `None`
/// on any malformed input.
pub(crate) fn extract_spki_der(cert_der: &[u8]) -> Option<&[u8]> {
    // Outer Certificate SEQUENCE.
    let (tag, hlen, _vlen, vstart) = der_tlv(cert_der, 0)?;
    if tag != 0x30 {
        return None;
    }
    // First child of Certificate is tbsCertificate SEQUENCE.
    let (tbs_tag, _tbs_hlen, tbs_len, tbs_start) = der_tlv(cert_der, vstart)?;
    if tbs_tag != 0x30 {
        return None;
    }
    let _ = hlen;
    let tbs_end = tbs_start + tbs_len;
    if tbs_end > cert_der.len() {
        return None;
    }
    let mut pos = tbs_start;
    // Optional [0] version (context-specific constructed 0xA0).
    if pos < tbs_end && cert_der[pos] == 0xA0 {
        let (_, h, l, s) = der_tlv(cert_der, pos)?;
        pos = s + l;
        let _ = h;
    }
    // serial INTEGER, signature SEQUENCE, issuer Name, validity SEQUENCE,
    // subject Name — skip each generically, then SPKI is next.
    for _ in 0..5 {
        if pos >= tbs_end {
            return None;
        }
        let (_, h, l, s) = der_tlv(cert_der, pos)?;
        pos = s + l;
        let _ = h;
    }
    if pos >= tbs_end || cert_der[pos] != 0x30 {
        return None;
    }
    let (_, h, l, s) = der_tlv(cert_der, pos)?;
    let total = h + l;
    if pos + total > cert_der.len() {
        return None;
    }
    let _ = s;
    Some(&cert_der[pos..pos + total])
}

/// SHA256 of the leaf certificate SPKI (fail-closed `None` on malformed DER).
pub(crate) fn spki_sha256(cert_der: &[u8]) -> Option<[u8; 32]> {
    let spki = extract_spki_der(cert_der)?;
    Some(sha256(spki))
}

/// Check if a host is denied by SSRF protection.
///
/// Exact-match denials run after stripping exactly one trailing `.` (the DNS
/// root / FQDN form): `169.254.169.254.` and `metadata.google.internal.`
/// resolve to the same host but would otherwise evade an exact comparison.
/// Numeric literals with a trailing dot are additionally rejected fail-closed
/// by `is_non_canonical_numeric_host` in `validate_host`.
pub(crate) fn is_ssrf_denied_host(host: &str) -> bool {
    // Normalize: lowercase, strip a single trailing `.` (DNS root / FQDN
    // form) BEFORE bracket stripping so `[169.254.169.254].` is handled too,
    // then strip IPv6 brackets for comparison.
    let trimmed = host.trim().to_ascii_lowercase();
    let lower = trimmed.strip_suffix('.').unwrap_or(trimmed.as_str());
    let inner = if lower.starts_with('[') && lower.ends_with(']') {
        &lower[1..lower.len() - 1]
    } else {
        lower
    };
    // Exact denials
    if inner == "169.254.169.254" {
        return true;
    }
    if inner == "metadata.google.internal" {
        return true;
    }
    // Minimal alias denials (exact hosts only, no DNS resolution): bare
    // `metadata.google` and `metadata.goog` are well-known metadata
    // endpoints. No suffix matching to avoid over-blocking normal hosts.
    if inner == "metadata.google" {
        return true;
    }
    if inner == "metadata.goog" {
        return true;
    }
    // Note: the zone-id form "::ffff:169.254.169.254%lo0" is intentionally not
    // listed here because validate_host rejects '%' in hosts, making such a
    // literal unreachable. If SSRF checks were moved before validation, this
    // branch would need reconsideration, but after validation it is dead code.
    if inner == "::ffff:169.254.169.254" {
        return true;
    }
    // Also deny the IPv6 bracketed form already handled via inner, but check original with brackets
    if lower == "[169.254.169.254]" {
        return true;
    }
    // Unspecified addresses are never valid RouterOS hosts — deny them as SSRF.
    if inner == "0.0.0.0" || inner == "::" {
        return true;
    }
    if lower == "[0.0.0.0]" || lower == "[::]" {
        return true;
    }
    false
}

/// Normalize `host` via WHATWG URL parsing and return the IP when numeric.
///
/// The HTTP client connects to the WHATWG-normalized host, so all range
/// checks must run against this value — not the raw string. Lexical checks
/// alone miss decimal (`2130706433`), hex (`0x7f000001`), short
/// (`127.1`), octal (`0177.0.0.1`), and mapped (`::ffff:127.0.0.1`) forms.
/// Returns `None` for domain names or unparsable hosts (callers fall back
/// to lexical hostname checks).
pub(crate) fn normalized_host_ip(host: &str) -> Option<std::net::IpAddr> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    let host_for_url = format_host_for_url(trimmed);
    let url_str = format!("http://{host_for_url}/");
    let parsed = url::Url::parse(&url_str).ok()?;
    match parsed.host()? {
        url::Host::Ipv4(v4) => Some(std::net::IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => Some(std::net::IpAddr::V6(v6)),
        url::Host::Domain(_) => None,
    }
}

/// Whether `host` is a non-canonical numeric literal.
///
/// When WHATWG normalization yields an IP whose canonical string differs
/// from the raw literal (modulo brackets and ASCII case), the input used a
/// decimal/hex/octal/short or otherwise non-canonical encoding and is
/// rejected fail-closed — even when the normalized address itself would be
/// public. Canonical forms (`127.0.0.1`, `8.8.8.8`, `2001:db8::1`) are
/// unaffected because they already match their canonical string.
pub(crate) fn is_non_canonical_numeric_host(host: &str) -> bool {
    let trimmed = host.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    let Some(normalized) = normalized_host_ip(trimmed) else {
        return false;
    };
    let canonical = normalized.to_string().to_ascii_lowercase();
    inner.to_ascii_lowercase() != canonical
}

/// Whether a normalized IP is unconditionally SSRF-denied.
///
/// Covers whole `169.254.0.0/16` link-local (not just `.169.254`),
/// IPv6 `fe80::/10` link-local, unspecified addresses, and the IPv6
/// transition prefixes that tunnel IPv4 regardless of the loopback opt-in:
/// NAT64 well-known `64:ff9b::/96`, Teredo `2001::/32`, and 6to4
/// `2002::/16`. IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is mapped to IPv4
/// before the check so `[::ffff:a9fe:a9fe]` (metadata IP) is denied as
/// link-local; where a transition prefix carries an extractable embedded
/// IPv4, the IPv4 deny/private policy is re-run on it as well.
pub(crate) fn is_normalized_ssrf_denied(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 169 && o[1] == 254 {
                return true;
            }
            if v4.is_unspecified() {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_normalized_ssrf_denied(std::net::IpAddr::V4(mapped));
            }
            if v6.is_unspecified() {
                return true;
            }
            // NAT64 / Teredo / 6to4 are unconditional denials: they tunnel
            // IPv4 (including link-local/metadata and private space) and are
            // not reachable through the `ALLOW_LOOPBACK` opt-in.
            if is_ipv6_transition_prefix(v6) {
                return true;
            }
            // Best-effort: re-run the IPv4 deny/private checks on an
            // extractable embedded address (defense in depth).
            if let Some(embedded) = embedded_ipv4(v6) {
                let v4 = std::net::IpAddr::V4(embedded);
                if is_normalized_ssrf_denied(v4) || is_normalized_loopback_or_private(v4) {
                    return true;
                }
            }
            // fe80::/10: first 10 bits are 1111111010.
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            false
        }
    }
}

/// Whether `v6` is a NAT64/Teredo/6to4 IPv6 transition prefix.
///
/// Exact prefixes: NAT64 well-known `64:ff9b::/96`, Teredo `2001::/32`,
/// and 6to4 `2002::/16`. Keep the literal strings in sync with
/// `scripts/_mikrotik_shared.py::is_normalized_ssrf_denied`.
pub(crate) fn is_ipv6_transition_prefix(v6: std::net::Ipv6Addr) -> bool {
    let s = v6.segments();
    // NAT64 well-known prefix 64:ff9b::/96.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return true;
    }
    // Teredo 2001::/32.
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return true;
    }
    // 6to4 2002::/16.
    if s[0] == 0x2002 {
        return true;
    }
    false
}

/// Best-effort embedded IPv4 extraction from an IPv6 transition prefix.
///
/// - NAT64 well-known `64:ff9b::/96`: last 32 bits.
/// - 6to4 `2002::/16`: bits 16..48 (segments 1 and 2).
/// - Teredo `2001::/32`: last 32 bits, bitwise-inverted (obfuscated client).
///
/// Returns `None` when `v6` is not one of these prefixes. The caller only
/// uses the result to re-run IPv4 policy; the prefix itself is already
/// unconditionally denied.
pub(crate) fn embedded_ipv4(v6: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let s = v6.segments();
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return Some(std::net::Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ));
    }
    if s[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new(
            (s[1] >> 8) as u8,
            (s[1] & 0xff) as u8,
            (s[2] >> 8) as u8,
            (s[2] & 0xff) as u8,
        ));
    }
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return Some(std::net::Ipv4Addr::new(
            ((!s[6]) >> 8) as u8,
            ((!s[6]) & 0xff) as u8,
            ((!s[7]) >> 8) as u8,
            ((!s[7]) & 0xff) as u8,
        ));
    }
    None
}

/// Whether a normalized IP is loopback or RFC1918/ULA/CGNAT private.
///
/// IPv4-mapped IPv6 is mapped to IPv4 first so `[::ffff:127.0.0.1]` and
/// `[::ffff:10.0.0.1]` are judged as their IPv4 equivalents. CGNAT
/// `100.64.0.0/10` and ULA `fc00::/7` are treated as private (denied
/// unless `RSC_LS_LIVE_ALLOW_LOOPBACK=1`), because operators legitimately
/// use them on the LAN — they are not in the unconditional deny set.
pub(crate) fn is_normalized_loopback_or_private(addr: std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return true;
            }
            let o = v4.octets();
            if o[0] == 10 {
                return true;
            }
            if o[0] == 192 && o[1] == 168 {
                return true;
            }
            if o[0] == 172 && (16..=31).contains(&o[1]) {
                return true;
            }
            // CGNAT 100.64.0.0/10 (100.64.0.0 - 100.127.255.255).
            if o[0] == 100 && (64..=127).contains(&o[1]) {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_normalized_loopback_or_private(std::net::IpAddr::V4(mapped));
            }
            if v6.is_loopback() {
                return true;
            }
            // ULA fc00::/7: first 7 bits are 1111110.
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            false
        }
    }
}

/// Whether `host` is loopback or private (RFC1918 / CGNAT / ULA / loopback).
///
/// Used for `RSC_LS_LIVE_ALLOW_LOOPBACK` gating: when loopback is not
/// allowed, these hosts are SSRF-denied. Handles IPv4 and IPv6 literals,
/// bracketed IPv6 (`[::1]`), and the hostname `localhost`. Range checks run
/// against the WHATWG-normalized IP (via `normalized_host_ip` with IPv4
///-mapped unmapping) so decimal/hex/short/mapped bypasses are closed.
pub(crate) fn is_loopback_or_private(host: &str) -> bool {
    // Normalize-then-check: the HTTP client connects to the normalized host.
    if let Some(normalized) = normalized_host_ip(host)
        && is_normalized_loopback_or_private(normalized)
    {
        return true;
    }
    let lower = host.trim().to_ascii_lowercase();
    let inner = if lower.starts_with('[') && lower.ends_with(']') {
        &lower[1..lower.len() - 1]
    } else {
        &lower
    };
    if inner == "localhost" {
        return true;
    }
    // Strip zone id / scope (e.g. fe80::1%lo0) for parsing.
    let host_no_zone = inner.split('%').next().unwrap_or(inner);
    if let Ok(addr) = host_no_zone.parse::<std::net::IpAddr>() {
        if addr.is_loopback() {
            return true;
        }
        // RFC1918 private for IPv4; CGNAT treated as private too. ULA
        // (fc00::/7) is private as well.
        match addr {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                if o[0] == 10 {
                    return true;
                }
                if o[0] == 192 && o[1] == 168 {
                    return true;
                }
                if o[0] == 172 && (16..=31).contains(&o[1]) {
                    return true;
                }
                if o[0] == 100 && (64..=127).contains(&o[1]) {
                    return true;
                }
            }
            std::net::IpAddr::V6(v6) => {
                // Loopback already handled; ULA fc00::/7 is private.
                if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether loopback/private hosts are allowed via `RSC_LS_LIVE_ALLOW_LOOPBACK=1`.
fn live_allow_loopback() -> bool {
    std::env::var("RSC_LS_LIVE_ALLOW_LOOPBACK")
        .ok()
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Validate `host` with explicit loopback allowance.
pub fn validate_host_with_allow(host: &str, allow_loopback: bool) -> Result<(), LiveError> {
    if host.is_empty() {
        return Err(LiveError::InvalidHost("empty".to_string()));
    }
    if host.len() > 253 {
        return Err(LiveError::InvalidHost("exceeds 253 chars".to_string()));
    }
    if host.contains('\0') {
        return Err(LiveError::InvalidHost("contains null byte".to_string()));
    }
    if host.chars().any(|c| c.is_control()) {
        return Err(LiveError::InvalidHost(
            "contains control characters".to_string(),
        ));
    }
    if host.contains('@')
        || host.contains('?')
        || host.contains('#')
        || host.contains(' ')
        || host.contains('%')
    {
        return Err(LiveError::InvalidHost("contains URI delimiter".to_string()));
    }
    if is_ssrf_denied_host(host) {
        return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
    }
    // Normalize-then-check: run ALL range checks against the WHATWG-normalized
    // host the HTTP client actually connects to. Rejects non-canonical numeric
    // literals fail-closed, then whole 169.254.0.0/16 and fe80::/10, then
    // loopback/RFC1918 against the normalized IP with IPv4-mapped unmapping.
    if is_non_canonical_numeric_host(host) {
        return Err(LiveError::InvalidHost(
            "non-canonical numeric host".to_string(),
        ));
    }
    if let Some(normalized) = normalized_host_ip(host) {
        if is_normalized_ssrf_denied(normalized) {
            return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
        }
        if !allow_loopback && is_normalized_loopback_or_private(normalized) {
            log_warn!(
                "live host denied (loopback/private) without RSC_LS_LIVE_ALLOW_LOOPBACK=1: {:?}",
                sanitize_for_log(host)
            );
            return Err(LiveError::InvalidHost(
                "loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1".to_string(),
            ));
        }
    }
    if !allow_loopback && is_loopback_or_private(host) {
        log_warn!(
            "live host denied (loopback/private) without RSC_LS_LIVE_ALLOW_LOOPBACK=1: {:?}",
            sanitize_for_log(host)
        );
        return Err(LiveError::InvalidHost(
            "loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1".to_string(),
        ));
    }
    Ok(())
}

/// Validate `host` per defensive rules.
///
/// - non-empty, max 253 chars
/// - no null bytes, no control chars
/// - no URI delimiters that would alter URL parsing (`@`, `?`, `#`, ` `, `%`)
/// - SSRF denials for whole `169.254.0.0/16`, IPv6 `fe80::/10`, unspecified,
///   the NAT64/Teredo/6to4 transition prefixes (`64:ff9b::/96`, `2001::/32`,
///   `2002::/16`), and `metadata.google.internal` (lexical plus
///   WHATWG-normalized checks)
/// - non-canonical numeric literals rejected fail-closed
/// - loopback/private denied unless `RSC_LS_LIVE_ALLOW_LOOPBACK=1` (via
///   `is_loopback_or_private` against the normalized IP with IPv4-mapped
///   unmapping; RFC1918, CGNAT `100.64.0.0/10`, and ULA `fc00::/7` count as
///   private)
pub fn validate_host(host: &str) -> Result<(), LiveError> {
    validate_host_with_allow(host, live_allow_loopback())
}

/// F4: TLS-identity portion of the connection-invalidation predicate.
///
/// Returns true when the pin / pin-validity / CA-bundle selection changed.
/// The caller (`server::live_connection_changed`) ORs this with the
/// transport fields so entries fetched under a previous trust anchor are
/// never reused after a pin/CA rotation.
pub(crate) fn live_identity_changed(old: &LiveConfig, new: &LiveConfig) -> bool {
    old.fingerprint != new.fingerprint
        || old.fingerprint_invalid != new.fingerprint_invalid
        || old.ca_file != new.ca_file
}

// ── F1: resolve-then-revalidate (DNS TOCTOU) ─────────────────────────────
//
// Lexical + normalized-literal checks run at config time, but a hostname
// can resolve to a denied address at fetch time (DNS rebinding / split
// horizon). Re-resolve at fetch time and re-run the deny policy against
// EVERY returned IP, fail-closed: any denied IP, or any resolution
// failure, refuses the fetch before credentials are sent.

/// Classify one resolved IP against the SSRF deny policy.
///
/// Returns `Some(reason)` when the address is denied, `None` when allowed.
/// Unconditionally denied: whole `169.254.0.0/16`, IPv6 `fe80::/10`,
/// unspecified, and — unless `allow_loopback` — loopback/RFC1918.
pub(crate) fn denied_reason_for_ip(
    addr: std::net::IpAddr,
    allow_loopback: bool,
) -> Option<&'static str> {
    if is_normalized_ssrf_denied(addr) {
        return Some("SSRF denied resolved IP");
    }
    if !allow_loopback && is_normalized_loopback_or_private(addr) {
        return Some("loopback/private denied without RSC_LS_LIVE_ALLOW_LOOPBACK=1");
    }
    None
}

/// Resolve `host:port` and deny the fetch when ANY resolved IP is denied.
///
/// - IP literals resolve locally (no DNS) via `ToSocketAddrs`.
/// - Hostnames resolve via the system resolver (`getaddrinfo`).
/// - Empty results or resolution failures are fail-closed (`InvalidHost`).
/// - Every returned IP is checked with [`denied_reason_for_ip`]; the first
///   denial fails the whole fetch (an attacker controls only one record to
///   win a race).
pub(crate) fn resolve_and_validate_host(
    host: &str,
    port: u16,
    allow_loopback: bool,
) -> Result<(), LiveError> {
    use std::net::ToSocketAddrs;
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::IpAddr> = (bare, port)
        .to_socket_addrs()
        .map(|iter| iter.map(|s| s.ip()).collect())
        .map_err(|e| LiveError::InvalidHost(format!("dns resolution failed: {e}")))?;
    if addrs.is_empty() {
        return Err(LiveError::InvalidHost(
            "dns resolution returned no addresses".to_string(),
        ));
    }
    for addr in addrs {
        if let Some(reason) = denied_reason_for_ip(addr, allow_loopback) {
            log_warn!(
                "live fetch denied: host {:?} resolved to denied IP {addr} ({reason})",
                sanitize_for_log(host)
            );
            return Err(LiveError::InvalidHost(format!(
                "resolved IP denied: {addr}"
            )));
        }
    }
    Ok(())
}

// ── F8: bounded CA-bundle loading ────────────────────────────────────────

/// Max bytes read from `MIKROTIK_CA_FILE` (256 KiB — PEM bundles are small;
/// anything larger is a misconfiguration, not a trust anchor).
pub(crate) const MAX_CA_FILE_BYTES: u64 = 256 * 1024;

/// Negative cache of CA paths that failed to parse, so every completion
/// keystroke does not re-read a broken bundle. Keyed by the canonical path
/// when available, else the raw path. Entries are never evicted within the
/// process lifetime (a bundle fix requires a restart — documented in the
/// WARN at insertion).
fn bad_ca_cache() -> &'static Mutex<HashSet<String>> {
    static BAD_CA: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    BAD_CA.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn is_bad_ca(key: &str) -> bool {
    bad_ca_cache()
        .lock()
        .map(|g| g.contains(key))
        .unwrap_or(false)
}

fn mark_bad_ca(key: String) {
    if let Ok(mut g) = bad_ca_cache().lock() {
        g.insert(key);
    }
}

/// Read a CA bundle with a 256 KiB cap, symlink warning, and negative cache.
///
/// - Resolves `canonicalize()` for the cache key; warns when the canonical
///   path differs (symlink or `..`/curdir indirection) so a swapped link
///   target is visible in logs.
/// - Warns (once per path) when the file is a symlink.
/// - Returns `None` fail-closed on missing/unreadable/oversize/undecodable
///   input; callers fall back to the default verifier (never insecure).
/// - The size cap is enforced again at read time (`take(cap + 1)`) so a
///   swapped symlink or a special file cannot force an unbounded read.
pub(crate) fn read_ca_bundle(ca_file: &str) -> Option<String> {
    if ca_file.trim().is_empty() {
        return None;
    }
    let raw_path = std::path::Path::new(ca_file);
    let canonical = std::fs::canonicalize(raw_path).ok();
    let key = canonical
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ca_file.to_string());
    if is_bad_ca(&key) {
        return None;
    }
    let fail = |why: &str| {
        log_warn!(
            "live CA file unreadable ({why}): {:?}",
            sanitize_for_log(ca_file)
        );
        mark_bad_ca(key.clone());
        None
    };
    let meta = std::fs::symlink_metadata(raw_path).ok()?;
    if meta.file_type().is_symlink() {
        log_warn!(
            "live CA file is a symlink (target swap risk): {:?} -> {:?}",
            sanitize_for_log(ca_file),
            sanitize_for_log(
                &canonical
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            )
        );
    }
    if canonical.as_ref().is_some_and(|c| {
        c.to_string_lossy() != raw_path.to_string_lossy().replace('\\', "/").as_str()
            && c.to_string_lossy() != ca_file
    }) {
        log_debug!(
            "live CA file canonicalized {:?} -> {:?}",
            sanitize_for_log(ca_file),
            sanitize_for_log(&key)
        );
    }
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return fail("not a regular file");
    }
    if meta.len() > MAX_CA_FILE_BYTES {
        return fail("exceeds 256KiB cap");
    }
    // Hard cap at read time: `symlink_metadata` is TOCTOU-racy (a symlink can
    // be swapped after the size check) and a special file (`/dev/zero`, FIFO)
    // has no meaningful `len()`. `take(limit + 1)` bounds the read and detects
    // overflow in a single pass; anything over the cap fails closed.
    let mut file = match std::fs::File::open(raw_path) {
        Ok(f) => f,
        Err(_) => return None,
    };
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(MAX_CA_FILE_BYTES + 1);
    if limited.read_to_end(&mut bytes).is_err() {
        return None;
    }
    if bytes.len() as u64 > MAX_CA_FILE_BYTES {
        return fail("exceeds 256KiB cap");
    }
    let text = String::from_utf8(bytes).ok()?;
    if parse_pem_certs(&text).is_empty() {
        return fail("no decodable PEM certificates");
    }
    Some(text)
}

/// Format host for URL: wrap bare IPv6 literals with brackets if needed.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::format_host_for_url`.
pub(crate) fn format_host_for_url(host: &str) -> String {
    // Already bracketed? keep as is.
    if host.starts_with('[') && host.ends_with(']') {
        return host.to_string();
    }
    // Contains colon => likely IPv6 literal without brackets -> wrap.
    if host.contains(':') {
        return format!("[{host}]");
    }
    host.to_string()
}

/// Shared base URL builder — single source for host/port/scheme validation.
///
/// Validates host (`validate_host_with_allow`, SSRF, slash, port), wraps bare
/// IPv6, parses via `url::Url::parse`, and checks scheme. Path is left as `/`
/// for callers to set via `Url::set_path`. Keeps caps single source.
///
/// Keep in sync with `scripts/_mikrotik_shared.py::validate_host` /
/// `format_host_for_url` / `resolve_scheme`.
pub(crate) fn build_base_url_with_allow(
    host: &str,
    port: u16,
    scheme: &str,
    allow_loopback: bool,
) -> Result<url::Url, LiveError> {
    validate_host_with_allow(host, allow_loopback)?;
    if port == 0 {
        return Err(LiveError::InvalidPort("port 0".to_string()));
    }
    if host.contains('/') || host.contains('\\') {
        return Err(LiveError::InvalidHost(
            "host contains path separator".to_string(),
        ));
    }
    if is_ssrf_denied_host(host) {
        return Err(LiveError::InvalidHost("SSRF denied host".to_string()));
    }
    // Loopback/private check already done via validate_host_with_allow above, but
    // keep SSRF deny above for explicitness.
    let host_for_url = format_host_for_url(host);
    let url_str = format!("{scheme}://{host_for_url}:{port}/");
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != scheme {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    Ok(parsed)
}

/// Build and validate the REST URL for a given resource.
///
/// Uses `build_base_url_with_allow` for shared validation, then appends the resource path.
/// Handles IPv6 bracket wrapping via `format_host_for_url`.
pub(crate) fn build_rest_url(
    config: &LiveConfig,
    resource: ResourceKind,
) -> Result<String, LiveError> {
    let mut base = build_base_url_with_allow(
        &config.host,
        config.port,
        config.scheme(),
        config.allow_loopback,
    )?;
    base.set_path(resource.rest_path());
    let url_str = base.to_string();
    // Re-validate full URL (scheme + host + path) via Url crate.
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != config.scheme() {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    Ok(url_str)
}

/// Build URL for a custom resource.
pub(crate) fn build_custom_rest_url(
    config: &LiveConfig,
    custom: &CustomResource,
) -> Result<String, LiveError> {
    let mut base = build_base_url_with_allow(
        &config.host,
        config.port,
        config.scheme(),
        config.allow_loopback,
    )?;
    // Ensure custom path starts with /
    let path = if custom.path.starts_with('/') {
        custom.path.clone()
    } else {
        format!("/{}", custom.path)
    };
    base.set_path(&path);
    let url_str = base.to_string();
    let parsed = url::Url::parse(&url_str)
        .map_err(|e| LiveError::InvalidHost(format!("invalid url: {e}")))?;
    if parsed.scheme() != config.scheme() {
        return Err(LiveError::InvalidHost("scheme mismatch".to_string()));
    }
    // F7: re-validate the parsed path — `Url::set_path` normalizes
    // percent-encoding and dot-segments, so the allowlist must hold on the
    // final parsed form, not just the raw configured string.
    if parsed.path() != "/rest" && !parsed.path().starts_with("/rest/") {
        return Err(LiveError::InvalidHost(
            "custom path escapes /rest".to_string(),
        ));
    }
    Ok(url_str)
}

// ── LiveError ────────────────────────────────────────────────────────────

/// Errors from live fetching, never containing `pass`.
#[derive(Debug, Clone)]
pub enum LiveError {
    /// Live is disabled (opt-in not set or missing host/pass).
    Disabled,
    InvalidHost(String),
    InvalidPort(String),
    /// Network / transport error (sanitized, no pass).
    Network(String),
    /// HTTP status error.
    Status(u16),
    /// Response exceeded `MAX_LIVE_RESPONSE_BYTES`.
    ResponseTooLarge(usize),
    /// JSON parse or shape error.
    Parse(String),
    /// Request timed out.
    Timeout,
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "live disabled"),
            Self::InvalidHost(r) => write!(f, "invalid host: {r}"),
            Self::InvalidPort(r) => write!(f, "invalid port: {r}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Status(code) => write!(f, "http status {code}"),
            Self::ResponseTooLarge(n) => write!(f, "response too large ({n} bytes)"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::Timeout => write!(f, "request timed out"),
        }
    }
}

impl std::error::Error for LiveError {}
