// Enrichment triggers and SSRF bypass vectors.
// Copied (not moved) from `lsp/src/live.rs` (`mod tests` L3481-3489, L4650-4834); the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::live::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

fn cfg_with(mut map: HashMap<&str, &str>) -> LiveConfig {
    // Tests historically use private hosts (192.168.88.1) which would now be denied by default.
    // To keep those fixtures honest while still exercising the new SSRF flag, inject
    // RSC_LS_LIVE_ALLOW_LOOPBACK=1 unless the test explicitly sets it.
    if !map.contains_key("RSC_LS_LIVE_ALLOW_LOOPBACK") {
        map.insert("RSC_LS_LIVE_ALLOW_LOOPBACK", "1");
    }
    LiveConfig::from_env_with(|k| map.get(k).map(|v| v.to_string()))
}
// ── Completion enrichment trigger (D2) ───────────────────────
//
// These tests exercise the synchronous coalescing logic of
// `trigger_enrichment_for_completion`. The host is loopback with a closed
// port so any background fetch fails fast; the network outcome is not
// asserted (it runs on a detached thread).

fn active_test_cfg() -> LiveConfig {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "127.0.0.1");
    m.insert("MIKROTIK_PORT", "1");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("MIKROTIK_TIMEOUT", "1");
    cfg_with(m)
}

#[test]
fn test_trigger_enrichment_inactive_config_does_nothing() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let cfg = cfg_with(HashMap::new()); // disabled
    trigger_enrichment_for_completion(&cache, &cfg, None, "/", "");
    assert!(cache.lock().unwrap().last_fetch_attempt.is_empty());
}

#[test]
fn test_trigger_enrichment_without_property_prefetches_interfaces() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let cfg = active_test_cfg();
    trigger_enrichment_for_completion(&cache, &cfg, None, "/", "");
    let guard = cache.lock().unwrap();
    // Coalescing marker recorded for the interfaces key...
    assert!(guard.last_fetch_attempt.contains_key("interfaces"));
    // ...so a second trigger is coalesced, not re-spawned.
    assert!(!guard.can_spawn_fetch("interfaces"));
}

#[test]
fn test_trigger_enrichment_with_property_resolves_resource() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let cfg = active_test_cfg();
    trigger_enrichment_for_completion(&cache, &cfg, Some("interface"), "/ip/address", "iface");
    let guard = cache.lock().unwrap();
    assert!(guard.last_fetch_attempt.contains_key("interfaces"));
}

#[test]
fn test_trigger_enrichment_unknown_property_fetches_nothing() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let cfg = active_test_cfg();
    trigger_enrichment_for_completion(&cache, &cfg, Some("comment"), "", "");
    let guard = cache.lock().unwrap();
    // Unresolvable property fetches nothing; Interfaces prefetch is
    // reserved for interface-like/empty context.
    assert!(guard.last_fetch_attempt.is_empty());
}

#[test]
fn test_trigger_enrichment_custom_resource_uses_custom_key() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "127.0.0.1");
    m.insert("MIKROTIK_PORT", "1");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("MIKROTIK_TIMEOUT", "1");
    m.insert(
            "RSC_LS_LIVE_RESOURCES",
            r#"[{"property":"packet-mark","path":"/rest/ip/firewall/mangle","field":"new-packet-mark"}]"#,
        );
    let cfg = cfg_with(m);
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    trigger_enrichment_for_completion(
        &cache,
        &cfg,
        Some("packet-mark"),
        "/ip/firewall/mangle",
        "string",
    );
    let guard = cache.lock().unwrap();
    // Custom match fetches ONLY the custom key (no generic prefetch).
    assert!(!guard.last_fetch_attempt.contains_key("interfaces"));
    // ...the custom resource is tracked under `custom:<property>`.
    assert!(guard.last_fetch_attempt.contains_key("custom:packet-mark"));
    assert_eq!(guard.last_fetch_attempt.len(), 1);
    let recorded_at = guard.last_fetch_attempt["custom:packet-mark"];
    drop(guard);
    // Second call within the coalescing window must not re-record.
    trigger_enrichment_for_completion(
        &cache,
        &cfg,
        Some("packet-mark"),
        "/ip/firewall/mangle",
        "string",
    );
    let guard = cache.lock().unwrap();
    assert_eq!(
        guard.last_fetch_attempt["custom:packet-mark"], recorded_at,
        "custom fetch attempt must be coalesced within the window"
    );
}

// ── SSRF normalize-then-check regression ───────────────────────

#[test]
fn test_ssrf_bypass_vectors_denied_by_default() {
    // Independent review proved lexical checks miss WHATWG-normalized
    // equivalents. All must be denied with default policy (allow=false).
    for bad in [
        "127.1",
        "2130706433",
        "0x7f000001",
        "0177.0.0.1",
        "[::ffff:127.0.0.1]",
        "[::ffff:a9fe:a9fe]",
        "[::ffff:10.0.0.1]",
        "169.254.1.1",
        "169.254.20.24",
    ] {
        assert!(
            validate_host_with_allow(bad, false).is_err(),
            "bypass vector should be denied by default: {bad:?}"
        );
        assert!(
            is_non_canonical_numeric_host(bad)
                || normalized_host_ip(bad)
                    .map(is_normalized_ssrf_denied)
                    .unwrap_or(false)
                || normalized_host_ip(bad)
                    .map(is_normalized_loopback_or_private)
                    .unwrap_or(false)
                || is_loopback_or_private(bad),
            "bypass vector should hit a normalized deny path: {bad:?}"
        );
    }
}

#[test]
fn test_ssrf_controls_stay_denied() {
    for bad in ["169.254.169.254", "127.0.0.1", "localhost"] {
        assert!(
            validate_host_with_allow(bad, false).is_err(),
            "control should stay denied: {bad:?}"
        );
    }
    // Link-local stays denied even when loopback is allowed (unconditional SSRF).
    assert!(validate_host_with_allow("169.254.169.254", true).is_err());
    assert!(validate_host_with_allow("169.254.1.1", true).is_err());
    assert!(validate_host_with_allow("[::ffff:a9fe:a9fe]", true).is_err());
}

#[test]
fn test_legitimate_hosts_still_accepted() {
    // No regression for normal hosts: public DNS names, public IPv4/IPv6,
    // and private hosts when explicitly allowed.
    assert!(validate_host_with_allow("router.local", false).is_ok());
    assert!(validate_host_with_allow("router-1.local", false).is_ok());
    assert!(validate_host_with_allow("8.8.8.8", false).is_ok());
    assert!(validate_host_with_allow("[2001:db8::1]", false).is_ok());
    assert!(validate_host_with_allow("2001:db8::1", false).is_ok());
    assert!(validate_host_with_allow("192.168.88.1", true).is_ok());
    assert!(validate_host_with_allow("10.0.0.1", true).is_ok());
    assert!(validate_host_with_allow("127.0.0.1", true).is_ok());
    assert!(!is_non_canonical_numeric_host("8.8.8.8"));
    assert!(!is_non_canonical_numeric_host("router.local"));
}

#[test]
fn test_redirects_disabled_on_agents() {
    // Unit-testable without network: the Debug rendering of ureq agents
    // exposes the configured `redirects` count.
    let normal = get_cached_agent(Duration::from_secs(5), true);
    let normal_dbg = format!("{normal:?}");
    assert!(
        normal_dbg.contains("redirects: 0"),
        "normal agent must disable redirects, got: {normal_dbg}"
    );
    let insecure =
        build_insecure_agent(Duration::from_secs(5)).expect("insecure agent should build");
    let insecure_dbg = format!("{insecure:?}");
    assert!(
        insecure_dbg.contains("redirects: 0"),
        "insecure agent must disable redirects, got: {insecure_dbg}"
    );
}

#[test]
fn test_ssrf_shared_vector_table_denied() {
    // Shared table with scripts/_mikrotik_shared.py: every encoding of a
    // denied address fails closed under the default policy. Loopback-mapped
    // vectors use allow=false (loopback is gated, not unconditional);
    // link-local/non-canonical vectors are unconditional (see second loop).
    for bad in [
        "2130706433",       // decimal 127.0.0.1
        "0x7f000001",       // hex 127.0.0.1
        "0177.0.0.1",       // octal 127.0.0.1
        "127.1",            // short 127.0.0.1
        "::ffff:127.0.0.1", // unbracketed IPv4-mapped loopback
        "169.254.0.0",      // link-local range floor
        "169.254.0.1",
        "169.254.255.254", // range ceiling edge
        "fe80::1",         // IPv6 link-local
        "FE80::abcd",      // case-insensitive link-local
    ] {
        assert!(
            validate_host_with_allow(bad, false).is_err(),
            "shared vector must be denied by default: {bad:?}"
        );
        assert!(
            is_non_canonical_numeric_host(bad)
                || normalized_host_ip(bad)
                    .map(is_normalized_ssrf_denied)
                    .unwrap_or(false)
                || normalized_host_ip(bad)
                    .map(is_normalized_loopback_or_private)
                    .unwrap_or(false)
                || is_loopback_or_private(bad),
            "shared vector must hit a normalized deny path: {bad:?}"
        );
    }
    // Unconditional denials stay denied even when loopback is allowed.
    for bad in [
        "2130706433",
        "0x7f000001",
        "0177.0.0.1",
        "127.1",
        "169.254.0.0",
        "169.254.255.254",
        "fe80::1",
    ] {
        assert!(
            validate_host_with_allow(bad, true).is_err(),
            "unconditional vector must stay denied with loopback allowed: {bad:?}"
        );
    }
}
