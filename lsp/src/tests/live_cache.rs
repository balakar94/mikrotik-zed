// Live cache TTL, caps and disabled fallbacks.
// Copied (not moved) from `lsp/src/live.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::caps::*;
use crate::live::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

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
fn test_cache_ttl_fresh_and_stale() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    let now = Instant::now();
    cache.insert_with_time("interfaces".to_string(), vec!["ether1".to_string()], now);
    assert_eq!(
        cache.try_get_cached("interfaces").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string()])
    );

    // Stale: 61 seconds ago
    let mut cache2 = LiveCache::new(Duration::from_secs(60));
    let stale = now - Duration::from_secs(61);
    cache2.insert_with_time("interfaces".to_string(), vec!["ether1".to_string()], stale);
    assert_eq!(cache2.try_get_cached("interfaces"), None);
}

#[test]
fn test_cache_caps_enforcement() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    // Fill to MAX_CACHE_ENTRIES with distinct keys
    for i in 0..MAX_CACHE_ENTRIES {
        cache.insert(format!("k{i}"), vec![format!("v{i}")]);
    }
    assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
    // Inserting a new key should evict oldest
    cache.insert("new_key".to_string(), vec!["new_val".to_string()]);
    assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
    assert!(cache.entries.contains_key("new_key"));
    // Overlong values truncated
    let many: Vec<String> = (0..600).map(|i| format!("iface{i}")).collect();
    cache.insert("interfaces".to_string(), many);
    assert!(cache.entries.get("interfaces").unwrap().values.len() <= MAX_LIVE_ITEMS);
}

#[test]
fn test_cache_max_live_value_len_enforced() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    let overlong = "a".repeat(65);
    cache.insert(
        "interfaces".to_string(),
        vec![overlong.clone(), "ok".to_string()],
    );
    let vals = cache.try_get_cached("interfaces").unwrap();
    assert!(!vals.contains(&overlong));
    assert!(vals.contains(&"ok".to_string()));
    // Ensure Arc is cloned cheaply (pointer equality after clone).
    let vals2 = cache.try_get_cached("interfaces").unwrap();
    assert!(Arc::ptr_eq(&vals, &vals2));
}

#[test]
fn test_live_values_for_property_mapping() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert(
        "interfaces".to_string(),
        vec!["ether1".to_string(), "wlan1".to_string()],
    );

    // Property name match
    assert_eq!(
        live_values_for_property(&cache, "interface", "string").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "wlan1".to_string()])
    );
    assert_eq!(
        live_values_for_property(&cache, "bridge", "string").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "wlan1".to_string()])
    );
    assert_eq!(
        live_values_for_property(&cache, "actual-interface", "string").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "wlan1".to_string()])
    );
    // Type contains iface
    assert_eq!(
        live_values_for_property(&cache, "foo", "iface_enum").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "wlan1".to_string()])
    );
    assert_eq!(
        live_values_for_property(&cache, "foo", "IFACE").map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "wlan1".to_string()])
    );
    // Non-matching property and type => None
    assert_eq!(
        live_values_for_property(&cache, "address", "ipPrefix"),
        None
    );
    assert_eq!(live_values_for_property(&cache, "comment", "string"), None);

    // No cache entry => None even for matching property
    let empty = LiveCache::new(Duration::from_secs(60));
    assert_eq!(
        live_values_for_property(&empty, "interface", "iface_enum"),
        None
    );
}

#[test]
fn test_is_live_property() {
    assert!(is_live_property("interface", "string"));
    assert!(is_live_property("bridge", "foo"));
    assert!(is_live_property("actual-interface", "bar"));
    assert!(is_live_property("myprop", "iface_enum"));
    assert!(is_live_property("address", "ipPrefix"));
    assert!(!is_live_property("comment", "string"));
    // case-insensitive
    assert!(is_live_property("Interface", "string"));
    assert!(is_live_property("foo", "IFACE"));
}

#[test]
fn test_disabled_fallback_fetch_errors() {
    let cfg = cfg_with(HashMap::new()); // disabled
    let res = fetch_interfaces(&cfg);
    assert!(matches!(res, Err(LiveError::Disabled)));

    // Enabled but invalid host
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "");
    m.insert("MIKROTIK_PASS", "p");
    let cfg2 = cfg_with(m);
    let res2 = fetch_interfaces(&cfg2);
    assert!(matches!(
        res2,
        Err(LiveError::Disabled) | Err(LiveError::InvalidHost(_))
    ));
}

#[test]
fn test_get_cached_or_fetch_blocking_disabled_returns_none() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    let cfg = cfg_with(HashMap::new());
    let res = get_cached_or_fetch_background(&cache, &cfg, ResourceKind::Interfaces);
    assert!(res.is_none());
}

#[test]
fn test_get_cached_or_fetch_blocking_returns_cached_without_network() {
    let cache = Arc::new(Mutex::new(LiveCache::with_default_ttl()));
    {
        let mut guard = cache.lock().unwrap();
        guard.insert(
            "interfaces".to_string(),
            vec!["ether1".to_string(), "ether2".to_string()],
        );
    }
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1");
    m.insert("MIKROTIK_PASS", "secret");
    let cfg = cfg_with(m);
    let res = get_cached_or_fetch_background(&cache, &cfg, ResourceKind::Interfaces);
    assert_eq!(
        res.map(|a| a.to_vec()),
        Some(vec!["ether1".to_string(), "ether2".to_string()])
    );
}
