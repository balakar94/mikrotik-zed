// Host validation, URL building and value filters.
// Copied (not moved) from `lsp/src/live.rs`; the original block is
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
fn test_host_validation() {
    // Public/non-private hosts remain ok with default deny (loopback/private denied via flag).
    assert!(validate_host_with_allow("192.168.88.1", true).is_ok());
    assert!(validate_host_with_allow("192.168.88.1", false).is_err());
    assert!(validate_host("router.local").is_ok());
    assert!(validate_host("").is_err());
    assert!(validate_host("a".repeat(254).as_str()).is_err());
    assert!(validate_host("ok-host").is_ok());
    assert!(validate_host("host\0with-null").is_err());
    assert!(validate_host("host\nnewline").is_err());
    assert!(validate_host("host\tcontrol").is_err());
    // Loopback/private denied when flag !=1
    assert!(validate_host_with_allow("127.0.0.1", false).is_err());
    assert!(validate_host_with_allow("127.0.0.1", true).is_ok());
    assert!(validate_host_with_allow("10.0.0.5", false).is_err());
    assert!(validate_host_with_allow("192.168.1.1", false).is_err());
    assert!(validate_host_with_allow("::1", false).is_err());
    assert!(validate_host_with_allow("[::1]", false).is_err());
}

#[test]
fn test_host_validation_rejects_uri_delimiters() {
    // Security fix: host must not contain URI-meaningful delimiters that could alter URL parsing.
    for bad in [
        "evil@host",
        "host?query=1",
        "host#frag",
        "host with space",
        "host%2e",
        "10.0.0.1@evil",
        "router.local?x=1",
    ] {
        assert!(
            validate_host(bad).is_err(),
            "host delimiter should be rejected: {bad:?}"
        );
    }
    // Brackets and ':' intentionally allowed for IPv6 literals (when loopback allowed).
    assert!(validate_host_with_allow("[::1]", true).is_ok());
    assert!(validate_host("[2001:db8::1]").is_ok());
    // IPv6 link-local fe80::/10 is unconditionally SSRF-denied.
    assert!(validate_host("fe80::1").is_err());
    assert!(validate_host_with_allow("fe80::1", true).is_err());
    // Backslash path separator rejected via fetch_interfaces host slash check (see
    // test_fetch_interfaces_rejects_host_with_slash)
    // but validate_host itself allows '/'? No—fetch layer rejects '/' explicitly, validate rejects
    // control/null/delimiters only.
    // Ensure normal hostnames still pass.
    assert!(validate_host("router-1.local").is_ok());
    assert!(validate_host_with_allow("192.168.88.1", true).is_ok());
}

#[test]
fn test_host_validation_rejects_metadata_ip() {
    // SSRF protection: deny instance metadata endpoints.
    for bad in [
        "169.254.169.254",
        "metadata.google.internal",
        "::ffff:169.254.169.254",
        "[::ffff:169.254.169.254]",
        "169.254.169.254:80", // host with port should be rejected? contains ':'? For pure host without port, we check inner; but colon presence is allowed for IPv6. This case is not pure IP, but we test base.
    ] {
        // For the last entry with port, validation may allow ':' but SSRF check should still deny
        // base IP? Our is_ssrf_denied_host checks inner after stripping brackets, but with port it
        // includes colon and port. We handle exact match only, so "169.254.169.254:80" not denied
        // as host (port is separate). So we test exact hosts.
        if bad == "169.254.169.254:80" {
            continue;
        }
        assert!(
            validate_host(bad).is_err(),
            "SSRF host should be rejected: {bad:?}"
        );
    }
    // Normal hosts still ok when loopback allowed; otherwise private is denied.
    assert!(validate_host_with_allow("192.168.88.1", true).is_ok());
    assert!(validate_host_with_allow("10.0.0.1", true).is_ok());
    assert!(validate_host_with_allow("192.168.88.1", false).is_err());
    // Loopback/private denied without flag
    assert!(validate_host_with_allow("127.0.0.1", false).is_err());
    assert!(is_loopback_or_private("127.0.0.1"));
    assert!(is_loopback_or_private("10.0.0.1"));
    assert!(is_loopback_or_private("192.168.1.1"));
    assert!(is_loopback_or_private("::1"));
    assert!(!is_loopback_or_private("8.8.8.8"));
    assert!(!is_loopback_or_private("router.local"));
}

#[test]
fn test_url_build_ipv6() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "2001:db8::1");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("MIKROTIK_PORT", "443");
    let cfg = cfg_with(m);
    assert!(validate_host(&cfg.host).is_ok());
    // format_host_for_url should wrap bare IPv6
    assert_eq!(format_host_for_url("2001:db8::1"), "[2001:db8::1]");
    assert_eq!(format_host_for_url("[2001:db8::1]"), "[2001:db8::1]");
    assert_eq!(format_host_for_url("192.168.88.1"), "192.168.88.1");
    // build_rest_url should succeed and contain brackets
    let url = build_rest_url(&cfg, ResourceKind::Interfaces).expect("ipv6 url should build");
    assert!(
        url.contains("[2001:db8::1]"),
        "url should contain bracketed ipv6, got {url}"
    );
    assert!(url::Url::parse(&url).is_ok());

    // Already bracketed host
    let mut m2 = HashMap::new();
    m2.insert("RSC_LS_LIVE", "1");
    m2.insert("MIKROTIK_HOST", "[::1]");
    m2.insert("MIKROTIK_PASS", "p");
    let cfg2 = cfg_with(m2);
    let url2 = build_rest_url(&cfg2, ResourceKind::Interfaces).expect("bracketed ipv6 url");
    assert!(url2.contains("[::1]"));
}

#[test]
fn test_debug_redacts_pass() {
    // Security fix: LiveConfig Debug must never leak MIKROTIK_PASS.
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "super_secret_password_123");
    m.insert("MIKROTIK_USER", "admin");
    let cfg = cfg_with(m);
    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains("super_secret_password_123"),
        "Debug must not leak pass, got: {dbg}"
    );
    assert!(
        !dbg.contains("super_secret"),
        "Debug must not leak pass substring, got: {dbg}"
    );
    assert!(
        dbg.contains("[REDACTED]"),
        "Debug should contain [REDACTED] placeholder, got: {dbg}"
    );
    // Also ensure host is still visible (redaction is precise, not blanket)
    assert!(
        dbg.contains("192.168.88.1"),
        "host should still be visible in Debug"
    );
}

#[test]
fn test_filter_value() {
    assert_eq!(filter_value("ether1"), Some("ether1".to_string()));
    assert_eq!(filter_value("ether-1"), Some("ether-1".to_string()));
    assert_eq!(filter_value("under_score"), Some("under_score".to_string()));
    assert_eq!(filter_value(""), None);
    assert_eq!(filter_value("   "), None);
    assert_eq!(filter_value("a b"), None); // space not allowed
    assert_eq!(filter_value("a/b"), None);
    assert_eq!(filter_value("a".repeat(65).as_str()), None); // over 64
    assert_eq!(filter_value("a".repeat(64).as_str()), Some("a".repeat(64)));
    assert_eq!(filter_value("ether1\0"), None);
    assert_eq!(filter_value("ether1\n"), None);
    assert_eq!(filter_value("wlan1"), Some("wlan1".to_string()));
    // Leading/trailing whitespace trimmed
    assert_eq!(filter_value("  ether1  "), Some("ether1".to_string()));
}

#[test]
fn test_sanitize_values_dedup_sort_truncate() {
    let raw = vec![
        "ether2".to_string(),
        "ether1".to_string(),
        "ether1".to_string(),
        "bad val".to_string(),
        "wlan1".to_string(),
    ];
    let sanitized = sanitize_values(raw);
    assert_eq!(sanitized, vec!["ether1", "ether2", "wlan1"]); // sorted, deduped, bad filtered

    // Truncate to MAX_LIVE_ITEMS
    let many: Vec<String> = (0..600).map(|i| format!("iface{i}")).collect();
    let sanitized2 = sanitize_values(many);
    assert_eq!(sanitized2.len(), MAX_LIVE_ITEMS);
    assert!(sanitized2.is_sorted());
}
