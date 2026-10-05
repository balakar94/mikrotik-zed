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
    assert_eq!(ResourceKind::all().len(), 15);
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
    // Interface *lists* are a distinct menu (`/interface/list`), not the
    // interface table: `in-interface-list=` must not suggest interface names.
    assert_eq!(
        live_resource_for_property("in-interface-list", "string"),
        Some(ResourceKind::InterfaceLists)
    );
    assert_eq!(
        live_resource_for_property("out-interface-list", "string"),
        Some(ResourceKind::InterfaceLists)
    );
    assert_eq!(
        live_resource_for_menu_property("/interface/list/member", "list", "string"),
        Some(ResourceKind::InterfaceLists)
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
    // The bare `list` property is menu-scoped: firewall menus map to firewall
    // address-lists, `/interface/list/member` to interface lists, and an
    // unscoped `list=` suggests nothing (it must not offer firewall names).
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/filter", "list", "string"),
        Some(ResourceKind::AddressLists)
    );
    assert_eq!(
        live_resource_for_menu_property("/ip/firewall/address-list", "list", "string"),
        Some(ResourceKind::AddressLists)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/address-list", "list", "string"),
        Some(ResourceKind::Ipv6AddressLists)
    );
    assert_eq!(live_resource_for_property("list", "string"), None);

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
    // Family-aware chains: `/ipv6/firewall/*` must read the IPv6 tables
    // (IPv6 has no mangle table).
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/filter", "chain", "string"),
        Some(ResourceKind::Ipv6FirewallFilterChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/nat", "chain", "string"),
        Some(ResourceKind::Ipv6FirewallNatChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/raw", "chain", "string"),
        Some(ResourceKind::Ipv6FirewallRawChains)
    );
    assert_eq!(
        live_resource_for_menu_property("/ipv6/firewall/filter", "jump-target", "string"),
        Some(ResourceKind::Ipv6FirewallFilterChains)
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
fn test_cmr_addresses_do_not_borrow_local_resources() {
    // `/cmr` addresses point at remote managed devices and the CMR server, so
    // local `/ip/address` values would be invalid there. The `wifi-logs` MAC
    // filter is a macAddr, never an IP either.
    for (menu, prop, ty) in [
        ("/cmr/device", "address", "address (flags=46)"),
        ("/cmr/client", "controller-address", "address"),
        ("/cmr/client/pair", "address", "address (flags=46)"),
        ("/cmr/device/wifi-logs", "address", "macAddr"),
    ] {
        assert_eq!(
            live_resource_for_menu_property(menu, prop, ty),
            None,
            "{menu} {prop} must not map to a local resource"
        );
    }

    // `/cmr/*/push-button` `interface` names the LOCAL interfaces to scan for
    // CMR peers, so that mapping stays.
    assert_eq!(
        live_resource_for_menu_property(
            "/cmr/push-button",
            "interface",
            "multi { array-id, interface: iface_enum }"
        ),
        Some(ResourceKind::Interfaces)
    );

    // Non-`/cmr` menus keep mapping addresses exactly as before.
    assert_eq!(
        live_resource_for_menu_property("/ip/address", "address", "ipAddr"),
        Some(ResourceKind::IpAddresses)
    );
}

#[test]
fn test_mac_only_types_are_never_ip_addresses() {
    // A MAC-only property takes a peer/device MAC (scan, monitor, romon,
    // mac-server, bluetooth), so suggesting local `/ip/address` entries would
    // be invalid on the device.
    for (menu, prop, ty) in [
        ("/interface/wifi/scan", "address", "macAddr"),
        ("/tool/romon/discover", "address", "macAddr"),
        ("/iot/bluetooth", "address", "MAC address"),
        ("/interface/w60g/station", "remote-address", "macAddr"),
        ("/tool/mac-server/sessions", "src-address", "macAddr"),
        (
            "/interface/wireless/snooper/flat-snoop",
            "address",
            "alt { station-address: macAddr , network-address: macAddr }",
        ),
    ] {
        assert_eq!(
            live_resource_for_menu_property(menu, prop, ty),
            None,
            "{menu} {prop} [{ty}] must not map to IP addresses"
        );
    }

    // A type that also offers an IP alternative keeps the address mapping.
    assert_eq!(
        live_resource_for_menu_property(
            "/interface/ethernet/switch/multicast-fdb",
            "address",
            "alt { mac-address: macAddr , ip-address: ipAddr }"
        ),
        Some(ResourceKind::IpAddresses)
    );
    assert_eq!(
        live_resource_for_menu_property(
            "/user/active",
            "address",
            "alt { ip: ipAddr , ip6: ip6Addr , address: macAddr }"
        ),
        Some(ResourceKind::IpAddresses)
    );
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
