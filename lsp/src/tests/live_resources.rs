// Live resource kinds and multi-resource isolation.
// Copied (not moved) from `lsp/src/live.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::live::*;` for the new location.
use crate::caps::*;
use crate::live::*;
use std::collections::HashMap;
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
fn test_fetch_interfaces_rejects_host_with_slash() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "host/with/slash");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    let res = fetch_interfaces(&cfg);
    assert!(matches!(res, Err(LiveError::InvalidHost(_))));
}

#[test]
fn test_filter_ip_value() {
    assert_eq!(
        filter_ip_value("192.168.88.1"),
        Some("192.168.88.1".to_string())
    );
    assert_eq!(
        filter_ip_value("10.0.0.1/24"),
        Some("10.0.0.1/24".to_string())
    );
    assert_eq!(
        filter_ip_value("2001:db8::1/64"),
        Some("2001:db8::1/64".to_string())
    );
    assert_eq!(filter_ip_value("fe80::1"), Some("fe80::1".to_string()));
    assert_eq!(filter_ip_value(""), None);
    assert_eq!(filter_ip_value("   "), None);
    assert_eq!(filter_ip_value("192.168.1.1 evil"), None);
    assert_eq!(filter_ip_value("192.168.1.1\0"), None);
    assert_eq!(filter_ip_value("192.168.1.1\n"), None);
}

#[test]
fn test_resource_kind_properties() {
    assert_eq!(ResourceKind::all().len(), 11);
    for kind in ResourceKind::all() {
        assert!(!kind.cache_key().is_empty());
        assert!(kind.rest_path().starts_with("/rest/"));
        assert!(!kind.json_field().is_empty());
        assert!(kind.detail_label().starts_with("live — "));
    }
}

#[test]
fn test_live_resource_for_property_all_kinds() {
    // Interfaces
    assert_eq!(
        live_resource_for_property("interface", "string"),
        Some(ResourceKind::Interfaces)
    );
    assert_eq!(
        live_resource_for_property("bridge", "string"),
        Some(ResourceKind::Interfaces)
    );
    assert_eq!(
        live_resource_for_property("in-interface", "string"),
        Some(ResourceKind::Interfaces)
    );
    assert_eq!(
        live_resource_for_property("foo", "iface_enum"),
        Some(ResourceKind::Interfaces)
    );

    // IPv4 Addresses
    assert_eq!(
        live_resource_for_property("address", "ipPrefix"),
        Some(ResourceKind::IpAddresses)
    );
    assert_eq!(
        live_resource_for_property("network", "ipAddr"),
        Some(ResourceKind::IpAddresses)
    );
    assert_eq!(
        live_resource_for_property("src-address", "string"),
        Some(ResourceKind::IpAddresses)
    );
    assert_eq!(
        live_resource_for_property("dst-address", "string"),
        Some(ResourceKind::IpAddresses)
    );
    assert_eq!(
        live_resource_for_property("gateway", "string"),
        Some(ResourceKind::IpAddresses)
    );

    // IPv6 Addresses
    assert_eq!(
        live_resource_for_menu_property("/ipv6/address", "address", "string"),
        Some(ResourceKind::Ipv6Addresses)
    );
    assert_eq!(
        live_resource_for_property("address", "ipv6Prefix"),
        Some(ResourceKind::Ipv6Addresses)
    );

    // Address lists (IPv4 & IPv6)
    assert_eq!(
        live_resource_for_property("src-address-list", "string"),
        Some(ResourceKind::AddressLists)
    );
    assert_eq!(
        live_resource_for_property("address-list", "string"),
        Some(ResourceKind::AddressLists)
    );
    assert_eq!(
        live_resource_for_property("list", "string"),
        Some(ResourceKind::AddressLists)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/address-list", "list", "string"),
        Some(ResourceKind::Ipv6AddressLists)
    );

    // Firewall chains (filter, mangle, nat, raw)
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/filter", "chain", "string"),
        Some(ResourceKind::FirewallFilterChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/mangle", "chain", "string"),
        Some(ResourceKind::FirewallMangleChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/nat", "chain", "string"),
        Some(ResourceKind::FirewallNatChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/raw", "chain", "string"),
        Some(ResourceKind::FirewallRawChains)
    );
    assert_eq!(
        live_resource_for_property("jump-target", "string"),
        Some(ResourceKind::FirewallFilterChains)
    );

    // IP Pools (IPv4 & IPv6)
    assert_eq!(
        live_resource_for_property("pool", "string"),
        Some(ResourceKind::IpPools)
    );
    assert_eq!(
        live_resource_for_property("address-pool", "string"),
        Some(ResourceKind::IpPools)
    );
    assert_eq!(
        live_resource_for_property("foo", "ip_pool"),
        Some(ResourceKind::IpPools)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/pool", "pool", "string"),
        Some(ResourceKind::Ipv6Pools)
    );

    // Unrelated
    assert_eq!(live_resource_for_property("comment", "string"), None);
    assert_eq!(live_resource_for_property("disabled", "bool"), None);
}

#[test]
fn test_multi_resource_cache_isolation() {
    let mut cache = LiveCache::new(Duration::from_secs(60));
    cache.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    cache.insert(
        "ip_addresses".to_string(),
        vec!["192.168.88.1/24".to_string()],
    );
    cache.insert("address_lists".to_string(), vec!["allowed_ips".to_string()]);
    cache.insert(
        "firewall_filter_chains".to_string(),
        vec!["forward".to_string(), "input".to_string()],
    );
    cache.insert("ip_pools".to_string(), vec!["dhcp-pool".to_string()]);

    assert_eq!(
        live_resource_values_for_property(&cache, "", "interface", "string")
            .map(|(k, v)| (k, v.to_vec())),
        Some((ResourceKind::Interfaces, vec!["ether1".to_string()]))
    );
    assert_eq!(
        live_resource_values_for_property(&cache, "", "address", "ipPrefix")
            .map(|(k, v)| (k, v.to_vec())),
        Some((
            ResourceKind::IpAddresses,
            vec!["192.168.88.1/24".to_string()]
        ))
    );
    assert_eq!(
        live_resource_values_for_property(&cache, "", "src-address-list", "string")
            .map(|(k, v)| (k, v.to_vec())),
        Some((ResourceKind::AddressLists, vec!["allowed_ips".to_string()]))
    );
    assert_eq!(
        live_resource_values_for_property(&cache, "/ip/firewall/filter", "chain", "string")
            .map(|(k, v)| (k, v.to_vec())),
        Some((
            ResourceKind::FirewallFilterChains,
            vec!["forward".to_string(), "input".to_string()]
        ))
    );
    assert_eq!(
        live_resource_values_for_property(&cache, "", "pool", "string")
            .map(|(k, v)| (k, v.to_vec())),
        Some((ResourceKind::IpPools, vec!["dhcp-pool".to_string()]))
    );
}

#[test]
fn test_multi_host_parsing() {
    let mut m = HashMap::new();
    m.insert("RSC_LS_LIVE", "1");
    m.insert("MIKROTIK_HOST", "192.168.88.1,10.0.0.2,  192.168.1.1");
    m.insert("MIKROTIK_PASS", "p");
    let cfg = cfg_with(m);
    assert_eq!(cfg.host, "192.168.88.1");
    assert_eq!(cfg.hosts, vec!["192.168.88.1", "10.0.0.2", "192.168.1.1"]);
    assert_eq!(cfg.host.as_str(), "192.168.88.1");
    assert!(cfg.is_active());
    // Cap at 4
    let mut m2 = HashMap::new();
    m2.insert("RSC_LS_LIVE", "1");
    m2.insert("MIKROTIK_HOST", "a,b,c,d,e,f");
    m2.insert("MIKROTIK_PASS", "p");
    let cfg2 = cfg_with(m2);
    assert_eq!(cfg2.hosts.len(), LIVE_MAX_HOSTS);
    assert_eq!(cfg2.hosts.len(), 4);
}
