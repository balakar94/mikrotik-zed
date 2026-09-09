// Live TLS identity (fingerprint, SPKI, scheme).
// Copied (not moved) from `lsp/src/live.rs` (`mod tests` L3481-3489, L3697-3870); the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::caps::*;
use crate::live::*;
use std::collections::HashMap;

fn cfg_with(mut map: HashMap<&str, &str>) -> LiveConfig {
    // Tests historically use private hosts (192.168.88.1) which would now be denied by default.
    // To keep those fixtures honest while still exercising the new SSRF flag, inject
    // RSC_LS_LIVE_ALLOW_LOOPBACK=1 unless the test explicitly sets it.
    if !map.contains_key("RSC_LS_LIVE_ALLOW_LOOPBACK") {
        map.insert("RSC_LS_LIVE_ALLOW_LOOPBACK", "1");
    }
    LiveConfig::from_env_with(|k| map.get(k).map(|v| v.to_string()))
}
#[test]
fn test_timeout_default_and_clamp() {
    let mut m = HashMap::new();
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE", "1");
    let cfg = cfg_with(m);
    assert_eq!(cfg.timeout_secs, LIVE_TIMEOUT_SECS);
    assert_eq!(cfg.timeout_secs, 5);

    // Clamp low
    let mut m2 = HashMap::new();
    m2.insert("MIKROTIK_TIMEOUT", "0");
    m2.insert("MIKROTIK_HOST", "h");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("RSC_LS_LIVE", "1");
    let cfg2 = cfg_with(m2);
    assert_eq!(cfg2.timeout_secs, 1);

    // Clamp high
    let mut m3 = HashMap::new();
    m3.insert("MIKROTIK_TIMEOUT", "100");
    m3.insert("MIKROTIK_HOST", "h");
    m3.insert("MIKROTIK_PASS", "p");
    m3.insert("RSC_LS_LIVE", "1");
    let cfg3 = cfg_with(m3);
    assert_eq!(cfg3.timeout_secs, 30);

    // Valid middle
    let mut m4 = HashMap::new();
    m4.insert("MIKROTIK_TIMEOUT", "10");
    m4.insert("MIKROTIK_HOST", "h");
    m4.insert("MIKROTIK_PASS", "p");
    m4.insert("RSC_LS_LIVE", "1");
    let cfg4 = cfg_with(m4);
    assert_eq!(cfg4.timeout_secs, 10);

    // Invalid => default
    let mut m5 = HashMap::new();
    m5.insert("MIKROTIK_TIMEOUT", "bogus");
    m5.insert("MIKROTIK_HOST", "h");
    m5.insert("MIKROTIK_PASS", "p");
    m5.insert("RSC_LS_LIVE", "1");
    let cfg5 = cfg_with(m5);
    assert_eq!(cfg5.timeout_secs, LIVE_TIMEOUT_SECS);
}

#[test]
fn test_scheme_resolution() {
    // Default matches Python: no silent downgrade — MIKROTIK_SSL=0 never
    // changes the scheme. Plain HTTP requires explicit force_http.
    assert_eq!(resolve_scheme(443, false, true), "https");
    assert_eq!(resolve_scheme(443, false, false), "https");
    assert_eq!(resolve_scheme(8729, false, false), "https");
    assert_eq!(resolve_scheme(80, false, false), "https");
    assert_eq!(resolve_scheme(8080, false, false), "https");
    assert_eq!(resolve_scheme(80, false, true), "https");
    // force_http overrides
    assert_eq!(resolve_scheme(443, true, true), "http");
    assert_eq!(resolve_scheme(80, true, true), "http");
    // Legacy opt-in restores the historical fallback with a shim flag.
    assert_eq!(
        resolve_scheme_with_legacy(80, false, false, true),
        ("http", true)
    );
    assert_eq!(
        resolve_scheme_with_legacy(8080, false, false, true),
        ("http", true)
    );
    assert_eq!(
        resolve_scheme_with_legacy(443, false, false, true),
        ("https", false)
    );
    // Without the opt-in the legacy condition stays https but reports fired.
    assert_eq!(
        resolve_scheme_with_legacy(80, false, false, false),
        ("https", true)
    );
}

#[test]
fn test_legacy_shim_env_gate() {
    // Env opt-in enables the historical downgrade path.
    let on = |k: &str| {
        if k == "RSC_LS_LEGACY_HTTP_SHIM" {
            Some("1".to_string())
        } else {
            None
        }
    };
    assert!(legacy_http_shim_allowed_with(&on));
    let off = |_: &str| None;
    assert!(!legacy_http_shim_allowed_with(&off));
}

#[test]
fn test_fingerprint_parsing() {
    let hex = "ab".repeat(32);
    let raw = format!("sha256:{hex}");
    let (pin, invalid) = parse_fingerprint(Some(&raw));
    assert!(!invalid);
    let bytes = pin.expect("valid pin should parse");
    assert_eq!(bytes, [0xabu8; 32]);
    // Case-insensitive prefix and separators tolerated.
    let spaced = format!("SHA256:{0}:{1}", &hex[..32], &hex[32..]);
    let (pin2, invalid2) = parse_fingerprint(Some(&spaced));
    assert!(!invalid2);
    assert_eq!(pin2, pin);
    // Malformed is fail-closed.
    let (none_pin, invalid3) = parse_fingerprint(Some("sha256:zzzz"));
    assert!(none_pin.is_none());
    assert!(invalid3);
    let (none2, invalid4) = parse_fingerprint(Some("sha256:abcd"));
    assert!(none2.is_none());
    assert!(invalid4);
    // Unset is not invalid.
    assert_eq!(parse_fingerprint(None), (None, false));
    assert_eq!(parse_fingerprint(Some("   ")), (None, false));
}

#[test]
fn test_fingerprint_env_fail_closed_and_effective() {
    let hex = "cd".repeat(32);
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "router.local");
    m.insert("MIKROTIK_PASS", "p");
    m.insert(
        "MIKROTIK_FINGERPRINT",
        Box::leak(format!("sha256:{hex}").into_boxed_str()) as &str,
    );
    let cfg = cfg_with(m);
    assert!(cfg.fingerprint.is_some());
    assert!(!cfg.fingerprint_invalid);
    assert!(cfg.is_active());
    // Pin counts as verification even with MIKROTIK_SSL=0.
    let mut m2 = HashMap::new();
    m2.insert("RSC_LS_LIVE", "1");
    m2.insert("MIKROTIK_HOST", "router.local");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("MIKROTIK_SSL", "0");
    m2.insert(
        "MIKROTIK_FINGERPRINT",
        Box::leak(format!("sha256:{hex}").into_boxed_str()) as &str,
    );
    let cfg2 = cfg_with(m2);
    assert!(!cfg2.ssl_verify);
    assert!(cfg2.ssl_verify_effective());
    // Invalid pin deactivates fail-closed.
    let mut m3 = HashMap::new();
    m3.insert("RSC_LS_LIVE", "1");
    m3.insert("MIKROTIK_HOST", "router.local");
    m3.insert("MIKROTIK_PASS", "p");
    m3.insert("MIKROTIK_FINGERPRINT", "sha256:not-hex");
    let cfg3 = cfg_with(m3);
    assert!(cfg3.fingerprint_invalid);
    assert!(!cfg3.is_active());
}

#[test]
fn test_spki_extract_rejects_malformed() {
    assert!(extract_spki_der(&[]).is_none());
    assert!(extract_spki_der(b"not-der").is_none());
    assert!(spki_sha256(b"not-der").is_none());
    // SHA256 self-consistency (RFC 4231 vector: "abc").
    let digest = sha256(b"abc");
    let expected: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    assert_eq!(digest, expected);
}
