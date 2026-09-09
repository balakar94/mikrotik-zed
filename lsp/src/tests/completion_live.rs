// Live-cache merged completions.
// Copied (not moved) from `lsp/src/completion.rs` (`mod live_merge` L2553-2753); the original block
// is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::live::{LiveCache, live_values_for_property};
use crate::menus::MenuData;
use std::time::Duration;

fn synth() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "comment"
type = "string"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "bridge"
type = "string"
"#,
    )
}

#[test]
fn test_iface_enum_without_live_returns_empty_honest() {
    let data = synth();
    // Honest placeholder: no fabricated items when live disabled.
    let items = compute_completions(&data, "/ip/address add interface=");
    assert!(
        items.is_empty(),
        "iface_enum without live must be empty, got {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    // Via the live-aware entry point with None.
    let items2 = compute_completions_with_live(&data, "/ip/address add interface=", None);
    assert!(items2.is_empty());
}

#[test]
fn test_iface_enum_with_live_returns_live_items() {
    let data = synth();
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert(
        "interfaces".to_string(),
        vec![
            "ether1".to_string(),
            "wlan1".to_string(),
            "bridge1".to_string(),
        ],
    );
    let items = compute_completions_with_live(&data, "/ip/address add interface=", Some(&cache));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"ether1"));
    assert!(labels.contains(&"wlan1"));
    assert!(labels.contains(&"bridge1"));
    assert_eq!(items.len(), 3);
    for item in &items {
        assert_eq!(item.kind, Some(kind::ENUM_MEMBER));
        assert_eq!(item.detail.as_deref(), Some("live — interface on device"));
        assert_eq!(item.insert_text.as_deref(), Some(item.label.as_str()));
        assert!(
            item.sort_text.as_deref().unwrap().starts_with("0!live_"),
            "sort_text must be 0!live_, got {:?}",
            item.sort_text
        );
        assert_eq!(item.insert_text_format, Some(1));
    }
    // Prefix filter still applies: typing "eth" narrows to ether1.
    let filtered =
        compute_completions_with_live(&data, "/ip/address add interface=eth", Some(&cache));
    let fl: Vec<&str> = filtered.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(fl, vec!["ether1"]);
}

#[test]
fn test_live_dedup_prefers_live_over_static() {
    // Synthetic enum that happens to contain a value also present as a live interface.
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "interface"
type = "enum (ether1 | ether2)"
"#,
    );
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert(
        "interfaces".to_string(),
        vec!["ether1".to_string(), "wlan1".to_string()],
    );
    let items = compute_completions_with_live(&data, "/ip/address add interface=", Some(&cache));
    // ether1 appears only once, with live detail (not enum detail).
    let ether1_items: Vec<_> = items.iter().filter(|i| i.label == "ether1").collect();
    assert_eq!(ether1_items.len(), 1);
    assert_eq!(
        ether1_items[0].detail.as_deref(),
        Some("live — interface on device")
    );
    assert!(
        ether1_items[0]
            .sort_text
            .as_deref()
            .unwrap()
            .starts_with("0!live_")
    );
    // ether2 remains as static enum value, wlan1 as live.
    assert!(items.iter().any(|i| i.label == "ether2"));
    assert!(items.iter().any(|i| i.label == "wlan1"));
}

#[test]
fn test_non_cached_live_property_returns_static_placeholder() {
    let data = synth();
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    // address is mapped to ip_addresses, but cache only has interfaces -> returns static
    // placeholder
    let items = compute_completions_with_live(&data, "/ip/address add address=", Some(&cache));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["0.0.0.0/0"]);
    assert!(!labels.contains(&"ether1"));
}

#[test]
fn test_ip_address_live_completion() {
    let data = synth();
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert(
        "ip_addresses".to_string(),
        vec!["192.168.88.1/24".to_string(), "10.0.0.1/8".to_string()],
    );
    let items = compute_completions_with_live(&data, "/ip/address add address=", Some(&cache));
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"192.168.88.1/24"));
    assert!(labels.contains(&"10.0.0.1/8"));
    let live_item = items.iter().find(|i| i.label == "192.168.88.1/24").unwrap();
    assert_eq!(
        live_item.detail.as_deref(),
        Some("live — IPv4 address on device")
    );
    assert_eq!(
        live_item.sort_text.as_deref(),
        Some("0!live_192.168.88.1/24")
    );
}

#[test]
fn test_bridge_property_uses_live_cache() {
    let data = synth();
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    // /interface/bridge/port interface= is iface_enum -> live
    let items =
        compute_completions_with_live(&data, "/interface/bridge/port add interface=", Some(&cache));
    assert!(items.iter().any(|i| i.label == "ether1"));
    // bridge property name itself is also live-mapped
    let items2 =
        compute_completions_with_live(&data, "/interface/bridge/port add bridge=", Some(&cache));
    assert!(items2.iter().any(|i| i.label == "ether1"));
}

#[test]
fn test_live_values_for_property_direct() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert(
        "interfaces".to_string(),
        vec!["a".to_string(), "b".to_string()],
    );
    cache.insert("ip_addresses".to_string(), vec!["10.0.0.1".to_string()]);
    assert!(live_values_for_property(&cache, "interface", "string").is_some());
    assert!(live_values_for_property(&cache, "bridge", "string").is_some());
    assert!(live_values_for_property(&cache, "actual-interface", "string").is_some());
    assert!(live_values_for_property(&cache, "foo", "iface_enum").is_some());
    assert!(live_values_for_property(&cache, "address", "ipPrefix").is_some());
    assert!(live_values_for_property(&cache, "comment", "string").is_none());
}

#[test]
fn test_stale_cache_returns_empty() {
    let data = synth();
    let mut cache = LiveCache::new(Duration::from_secs(0)); // TTL 0 => always stale
    cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    // Even though an entry exists, it is stale so no live values.
    let items = compute_completions_with_live(&data, "/ip/address add interface=", Some(&cache));
    assert!(items.is_empty(), "stale cache must behave like absent");
}
