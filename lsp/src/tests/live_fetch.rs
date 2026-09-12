// Custom resources, extract-sanitize and fetch errors.
// Copied (not moved) from `lsp/src/live.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::caps::*;
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
#[test]
fn test_negative_cache_cooldown() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    assert!(!cache.is_negative_cooldown("interfaces"));
    cache.insert_negative("interfaces".to_string());
    assert!(cache.is_negative_cooldown("interfaces"));
    // After inserting success, negative cleared
    cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    assert!(!cache.is_negative_cooldown("interfaces"));
    // Not in cooldown for other key
    assert!(!cache.is_negative_cooldown("ip_addresses"));
}

#[test]
fn test_background_fetch_coalescing() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "p");
    let _cfg = cfg_with(m);
    // First call should trigger background fetch (no cache)
    {
        let guard = cache.lock().unwrap();
        assert!(guard.can_spawn_fetch("interfaces"));
    }
    // Simulate a fetch attempt recorded
    {
        let mut guard = cache.lock().unwrap();
        guard.record_fetch_attempt("interfaces".to_string());
        assert!(!guard.can_spawn_fetch("interfaces")); // coalesced within 2s
    }
    // After negative cooldown, still cannot spawn
    {
        let mut guard = cache.lock().unwrap();
        guard.insert_negative("interfaces".to_string());
        assert!(!guard.can_spawn_fetch("interfaces"));
    }
}

#[test]
fn test_custom_resource_parsing() {
    let json = r#"[{"property":"packet-mark","path":"/rest/ip/firewall/mangle","field":"new-packet-mark"}]"#;
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE_RESOURCES", json);
    let cfg = cfg_with(m);
    assert_eq!(cfg.custom_resources.len(), 1);
    assert_eq!(cfg.custom_resources[0].property, "packet-mark");
    assert_eq!(cfg.custom_resources[0].path, "/rest/ip/firewall/mangle");
    assert_eq!(cfg.custom_resources[0].field, "new-packet-mark");
    // Resolve custom via LiveConfig fallback
    assert!(
        cfg.resolve_resource_with_custom("/ip/firewall/mangle", "packet-mark", "string")
            .is_some()
    );
    // Hardcoded still works
    assert_eq!(
        cfg.resolve_resource_with_custom("", "interface", "string"),
        Some(ResourceKind::Interfaces)
    );
    // Unknown without custom returns None
    assert!(
        cfg.resolve_resource_with_custom("", "unknown-prop", "string")
            .is_none()
    );
}

#[test]
fn test_custom_resource_cap() {
    // More than 8 should truncate
    let many: Vec<String> = (0..10)
        .map(|i| format!(r#"{{"property":"p{i}","path":"/rest/interface","field":"name"}}"#))
        .collect();
    let json = format!("[{}]", many.join(","));
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "h");
    m.insert("MIKROTIK_PASS", "p");
    m.insert("RSC_LS_LIVE_RESOURCES", json.as_str());
    let cfg = cfg_with(m);
    assert_eq!(cfg.custom_resources.len(), LIVE_CUSTOM_RESOURCES_MAX);
}

#[test]
fn test_hot_reload_from_settings() {
    let cfg = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    });
    assert_eq!(cfg.host, "192.168.88.1");
    // Simulate settings overlay
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "host": "10.0.0.5",
                "port": 8728
            }
        }
    });
    let _cfg2 = LiveConfig::from_settings_value(&settings);
    // from_settings_value starts from env (which has 192.168.88.1) but overlays 10.0.0.5
    // Note: from_env inside will read real env, not our mocked one. So we test apply directly.
    // F2: host/user overlay requires the transport opt-in — pass it explicitly here.
    let mut cfg3 = cfg.clone();
    LiveConfig::apply_settings_value_with_transport(&mut cfg3, &settings, true);
    assert_eq!(cfg3.host, "10.0.0.5");
    assert_eq!(cfg3.port, 8728);
    assert_eq!(cfg3.hosts, vec!["10.0.0.5".to_string()]);
}

#[test]
fn test_settings_pass_is_ignored_env_pass_wins() {
    // SECURITY: a settings-provided secret must never reach `cfg.pass`;
    // env/keychain stays the sole password source.
    let cfg = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
        "MIKROTIK_PASS" => Some("envpass".to_string()),
        _ => None,
    });
    assert_eq!(cfg.pass, "envpass");
    let settings = serde_json::json!({
        "rsc": {
            "live": {
                "MIKROTIK_PASS": "settings-secret",
                "password": "settings-secret-2",
            }
        }
    });
    let mut overlaid = cfg.clone();
    LiveConfig::apply_settings_value(&mut overlaid, &settings);
    assert_eq!(overlaid.pass, "envpass");
    // Without an env pass, a settings pass must not fill the gap.
    let bare = LiveConfig::from_env_with(|k| match k {
        "RSC_LS_LIVE" => Some("1".to_string()),
        "MIKROTIK_HOST" => Some("192.168.88.1".to_string()),
        _ => None,
    });
    assert!(bare.pass.is_empty());
    let mut overlaid_bare = bare.clone();
    LiveConfig::apply_settings_value(&mut overlaid_bare, &settings);
    assert!(overlaid_bare.pass.is_empty());
}

// ── Shared fetch tail (D1) ───────────────────────────────────────────────

#[test]
fn test_extract_and_sanitize_extracts_field_and_filters() {
    let arr: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"name":"ether2"},{"name":"ether1"},{"name":"ether1"},{"name":"bad val"},{"nope":"x"},{"name":"wlan1"}]"#,
        )
        .unwrap();
    let out = extract_and_sanitize(&arr, "name", ResourceKind::Interfaces);
    assert_eq!(out, vec!["ether1", "ether2", "wlan1"]);
}

#[test]
fn test_extract_and_sanitize_ip_kind_uses_ip_filter() {
    let arr: Vec<serde_json::Value> = serde_json::from_str(
            r#"[{"address":"10.0.0.1/24"},{"address":"192.168.1.1 evil"},{"address":"2001:db8::1/64"}]"#,
        )
        .unwrap();
    let out = extract_and_sanitize(&arr, "address", ResourceKind::IpAddresses);
    assert_eq!(out, vec!["10.0.0.1/24", "2001:db8::1/64"]);
}

#[test]
fn test_extract_and_sanitize_caps_raw_values() {
    // More than 2*MAX_LIVE_ITEMS entries: extraction stops early and the
    // sanitized output is capped to MAX_LIVE_ITEMS.
    let n = MAX_LIVE_ITEMS * 2 + 10;
    let entries: Vec<String> = (0..n)
        .map(|i| format!("{{\"name\":\"iface{i:04}\"}}"))
        .collect();
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(&format!("[{}]", entries.join(","))).unwrap();
    let out = extract_and_sanitize(&arr, "name", ResourceKind::Interfaces);
    assert_eq!(out.len(), MAX_LIVE_ITEMS);
}

fn custom_test_resource() -> CustomResource {
    CustomResource {
        property: "packet-mark".to_string(),
        path: "/rest/ip/firewall/mangle".to_string(),
        field: "new-packet-mark".to_string(),
    }
}

#[test]
fn test_fetch_custom_resource_disabled() {
    let cfg = cfg_with(HashMap::new()); // disabled
    let res = fetch_custom_resource(&cfg, &custom_test_resource());
    assert!(matches!(res, Err(LiveError::Disabled)));
}

#[test]
fn test_fetch_custom_resource_rejects_host_with_slash() {
    // Same shared validation as the built-in fetchers (via build_base_url_with_allow).
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "host/with/slash");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    let res = fetch_custom_resource(&cfg, &custom_test_resource());
    assert!(matches!(res, Err(LiveError::InvalidHost(_))));
}

// ── F1: DNS pinning (resolve-then-connect TOCTOU) ────────────────────────

#[test]
fn test_resolve_and_validate_returns_validated_addrs() {
    use std::net::SocketAddr;
    // IP literals resolve locally (no DNS) and come back for pinning.
    let v4 = resolve_and_validate_host("8.8.8.8", 443, false).unwrap();
    assert_eq!(v4, vec!["8.8.8.8:443".parse::<SocketAddr>().unwrap()]);
    let v6 = resolve_and_validate_host("[2001:db8::1]", 8443, false).unwrap();
    assert_eq!(
        v6,
        vec!["[2001:db8::1]:8443".parse::<SocketAddr>().unwrap()]
    );
    // Denied addresses fail closed and return no addresses to pin.
    assert!(resolve_and_validate_host("169.254.169.254", 443, true).is_err());
    assert!(resolve_and_validate_host("127.0.0.1", 443, false).is_err());
}

#[test]
fn test_pinned_resolver_never_consults_dns() {
    use std::net::SocketAddr;
    use ureq::Resolver;
    let addrs: Vec<SocketAddr> = vec![
        "8.8.8.8:443".parse().unwrap(),
        "[2001:db8::1]:443".parse().unwrap(),
    ];
    let resolver = PinnedAddrs(addrs.clone());
    // Regardless of the requested netloc (including a name that would rebound
    // to loopback), the resolver returns exactly the validated set.
    assert_eq!(resolver.resolve("attacker.invalid:443").unwrap(), addrs);
    assert_eq!(
        resolver.resolve("metadata.google.internal:80").unwrap(),
        addrs
    );
    assert_eq!(resolver.resolve("other.example:443").unwrap(), addrs);
}
