// Live config parsing (env gate, user, port).
// Copied (not moved) from `lsp/src/live.rs` (`mod tests` L3481-3489, L3491-3696); the original block is
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
// ── Config parsing ───────────────────────────────────────────

#[test]
fn test_disabled_by_default() {
    let cfg = cfg_with(HashMap::new());
    assert!(!cfg.enabled);
    assert!(!cfg.is_active());
    assert_eq!(cfg.user, "admin");
    assert_eq!(cfg.port, 443);
    assert!(cfg.ssl_verify);
    assert!(!cfg.force_http);
    assert_eq!(cfg.timeout_secs, LIVE_TIMEOUT_SECS);
}

#[test]
fn test_enabled_via_rsc_ls_live() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "secret");
    let cfg = cfg_with(m);
    assert!(cfg.enabled);
    assert!(cfg.is_active());
}

#[test]
fn test_enabled_via_mikrotik_live() {
    let mut m = HashMap::new();
    m.insert("MIKROTIK_LIVE", "1");
    m.insert("MIKROTIK_HOST", "router.local");
    m.insert("MIKROTIK_PASS", "pw");
    let cfg = cfg_with(m);
    assert!(cfg.enabled);
    assert!(cfg.is_active());
    // Any value other than "1" is not enabled.
    let mut m2 = HashMap::new();
    m2.insert("MIKROTIK_LIVE", "true");
    m2.insert("MIKROTIK_HOST", "router.local");
    m2.insert("MIKROTIK_PASS", "pw");
    let cfg2 = cfg_with(m2);
    assert!(!cfg2.enabled);
    assert!(!cfg2.is_active());
}

#[test]
fn test_enabled_requires_host_and_pass() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    // missing host and pass
    let cfg = cfg_with(m.clone());
    assert!(cfg.enabled);
    assert!(!cfg.is_active());

    m.insert("MIKROTIK_HOST", "10.0.0.1");
    let cfg2 = cfg_with(m.clone());
    assert!(!cfg2.is_active()); // still missing pass

    m.insert("MIKROTIK_PASS", "x");
    let cfg3 = cfg_with(m);
    assert!(cfg3.is_active());
}

#[test]
fn test_user_defaults_to_admin() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m.clone());
    assert_eq!(cfg.user, "admin");

    m.insert("MIKROTIK_USER", "custom");
    let cfg2 = cfg_with(m);
    assert_eq!(cfg2.user, "custom");

    // empty string => default
    let mut m3 = HashMap::new();
    m3.insert("MIKROTIK_USER", "   ");
    m3.insert("MIKROTIK_HOST", "h");
    m3.insert("MIKROTIK_PASS", "p");
    m3.insert("RSC_LS_LIVE", "1");
    let cfg3 = cfg_with(m3);
    assert_eq!(cfg3.user, "admin");
}

#[test]
fn test_port_default_and_env_override() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    assert_eq!(cfg.port, 443);

    let mut m2 = HashMap::new();
    m2.insert("MIKROTIK_PORT", "8729");
    m2.insert("MIKROTIK_HOST", "h");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("RSC_LS_LIVE", "1");
    let cfg2 = cfg_with(m2);
    assert_eq!(cfg2.port, 8729);
}

#[test]
fn test_port_invalid_falls_back_to_default() {
    let mut m = HashMap::new();
    m.insert("MIKROTIK_PORT", "not-a-number");
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE", "1");
    let cfg = cfg_with(m);
    // Warning is logged; default is used.
    assert_eq!(cfg.port, 443);
}

#[test]
fn test_ssl_verify_respects_mikrotik_ssl() {
    let mut m = HashMap::new();
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE", "1");
    let cfg = cfg_with(m.clone());
    assert!(cfg.ssl_verify);

    let mut m2 = HashMap::new();
    m2.insert("MIKROTIK_SSL", "0");
    m2.insert("MIKROTIK_HOST", "h");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("RSC_LS_LIVE", "1");
    let cfg2 = cfg_with(m2);
    assert!(!cfg2.ssl_verify);

    // Any other value => true
    let mut m3 = HashMap::new();
    m3.insert("MIKROTIK_SSL", "1");
    m3.insert("MIKROTIK_HOST", "h");
    m3.insert("MIKROTIK_PASS", "p");
    m3.insert("RSC_LS_LIVE", "1");
    let cfg3 = cfg_with(m3);
    assert!(cfg3.ssl_verify);
}

#[test]
fn test_ssl_verify_respects_mikrotik_ssl_effective() {
    // Effective verification is false when ssl_verify is false, or when scheme is http.
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    assert!(cfg.ssl_verify);
    assert!(cfg.ssl_verify_effective());
    assert_eq!(cfg.scheme(), "https");

    let mut m2 = HashMap::new();
    m2.insert("RSC_LS_LIVE", "1");
    m2.insert("MIKROTIK_HOST", "192.168.88.1");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("MIKROTIK_SSL", "0");
    let cfg2 = cfg_with(m2);
    assert!(!cfg2.ssl_verify);
    assert!(!cfg2.ssl_verify_effective());

    // Force http also makes effective false even if ssl_verify true.
    let mut m3 = HashMap::new();
    m3.insert("RSC_LS_LIVE", "1");
    m3.insert("MIKROTIK_HOST", "h");
    m3.insert("MIKROTIK_PASS", "p");
    m3.insert("MIKROTIK_HTTP", "1");
    let cfg3 = cfg_with(m3);
    assert!(cfg3.ssl_verify);
    assert!(!cfg3.ssl_verify_effective());
    assert_eq!(cfg3.scheme(), "http");

    // Non-standard port with ssl_verify false stays https by default
    // (legacy shim removed to match Python); effective stays false.
    let mut m4 = HashMap::new();
    m4.insert("RSC_LS_LIVE", "1");
    m4.insert("MIKROTIK_HOST", "h");
    m4.insert("MIKROTIK_PASS", "p");
    m4.insert("MIKROTIK_SSL", "0");
    m4.insert("MIKROTIK_PORT", "80");
    let cfg4 = cfg_with(m4);
    assert!(!cfg4.ssl_verify);
    assert!(!cfg4.ssl_verify_effective());
    assert_eq!(cfg4.scheme(), "https");
}

#[test]
fn test_force_http_respects_mikrotik_http() {
    let mut m = HashMap::new();
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE", "1");
    let cfg = cfg_with(m.clone());
    assert!(!cfg.force_http);

    let mut m2 = HashMap::new();
    m2.insert("MIKROTIK_HTTP", "1");
    m2.insert("MIKROTIK_HOST", "h");
    m2.insert("MIKROTIK_PASS", "p");
    m2.insert("RSC_LS_LIVE", "1");
    let cfg2 = cfg_with(m2);
    assert!(cfg2.force_http);
}
