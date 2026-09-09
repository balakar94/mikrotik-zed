// Menus — lookup.
use crate::menus::*;
fn test_commands_toml() -> &'static str {
    r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "The IP address and network mask"

[[menus.arguments]]
name = "interface"
type = "iface_enum"

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = "dynamic"

[[menus.read_only]]
name = "actual-interface"
type = "iface_enum"
description = "The actual interface"

[[menus]]
path = "/ip/route"
type = "Directory"

[[menus.arguments]]
name = "gateway"
type = "address (flags=46ivL)"

[[menus]]
path = "/ip/route/check"
type = "Command"

[[menus]]
path = "/ip/firewall/filter"
type = "Directory"

[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"

[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"

[[menus]]
path = "/interface/bridge/port"
type = "Directory"

[[menus]]
path = "/routing/bgp/connection"
type = "Directory"

[[menus]]
path = "/system/identity"
type = "Directory"
"#
}

#[test]
fn provenance_parses_generated_header() {
    let text = "# MikroTik RouterOS CLI Command Table\n\
        # RouterOS version: 7.23.2\n\
        # Source hash (sha256[:16]): c043cd8fd9e2215c\n\
        \n\
        [[menus]]\n";
    let prov = parse_provenance(text);
    assert_eq!(prov.version, "7.23.2");
    assert_eq!(prov.src_hash, "c043cd8fd9e2215c");
}

#[test]
fn provenance_degrades_to_unknown_on_missing_header() {
    let prov = parse_provenance("[[menus]]\n");
    assert_eq!(prov.version, "unknown");
    assert_eq!(prov.src_hash, "unknown");
}

#[test]
fn embedded_dataset_provenance_matches_committed_header() {
    // Guards the banner against header renames in extract_commands.py.
    // Value-agnostic on purpose: the version/hash rotate on every
    // `make sync`, so only assert they parsed (not "unknown").
    let prov = dataset_provenance();
    assert!(!prov.version.contains("unknown") && !prov.version.is_empty());
    assert!(!prov.src_hash.contains("unknown"));
    assert_eq!(prov.src_hash.len(), 16);
}

#[test]
fn test_parse_commands_toml() {
    let commands: CommandsFile = toml::from_str(test_commands_toml()).expect("should parse TOML");
    assert!(!commands.menus.is_empty(), "should have menus");
    assert!(commands.menus.len() >= 4, "should have at least 4 menus");
}

#[test]
fn test_empty_commands_toml() {
    let toml_str = "\n[[menus]]\npath = \"/empty\"\ntype = \"Directory\"\n";
    let commands: CommandsFile = toml::from_str(toml_str).unwrap();
    assert_eq!(commands.menus.len(), 1);
    assert_eq!(commands.menus[0].path, "/empty");
}

#[test]
fn test_menus_are_not_empty() {
    let data = MenuData::load();
    assert!(
        !data.menus.is_empty(),
        "embedded commands.toml should have menus"
    );
    assert!(data.menus.len() >= 50, "should have at least 50 menus");
    assert!(
        !data.menu_by_path.is_empty(),
        "menu_by_path should be populated"
    );
}

#[test]
fn test_all_menus_have_path() {
    let data = MenuData::load();
    for menu in &data.menus {
        assert!(!menu.path.is_empty(), "every menu should have a path");
        assert!(
            menu.path.starts_with('/'),
            "paths should start with /: {}",
            menu.path
        );
    }
}

#[test]
fn test_target_root_menus_present() {
    let data = MenuData::load();
    let paths: Vec<&str> = data.menus.iter().map(|m| m.path.as_str()).collect();

    assert!(
        paths.iter().any(|p| p.starts_with("/ip/")),
        "missing /ip entries"
    );
    assert!(
        paths.iter().any(|p| p.starts_with("/ipv6/")),
        "missing /ipv6 entries"
    );
    assert!(
        paths.iter().any(|p| p.starts_with("/interface/")),
        "missing /interface entries"
    );
    assert!(
        paths.iter().any(|p| p.starts_with("/routing/")),
        "missing /routing entries"
    );
}

#[test]
fn test_no_unwanted_root_menus() {
    // Under complete coverage, /certificate and other previously excluded
    // roots are now included. Verify that.
    let data = MenuData::load();
    assert!(
        data.menus
            .iter()
            .any(|m| m.path.starts_with("/certificate")),
        "should contain /certificate under complete coverage, got {} menus",
        data.menus.len()
    );
}

#[test]
fn test_specific_menus_exist() {
    let data = MenuData::load();

    assert!(
        data.menu_by_path.contains_key("/ip/address"),
        "missing /ip/address"
    );
    assert!(
        data.menu_by_path.contains_key("/ip/route"),
        "missing /ip/route"
    );
    assert!(
        data.menu_by_path.contains_key("/ip/firewall/filter"),
        "missing /ip/firewall/filter"
    );
    assert!(data.menu_by_path.contains_key("/ip/dns"), "missing /ip/dns");
    assert!(
        data.menu_by_path.contains_key("/ip/service"),
        "missing /ip/service"
    );
    assert!(
        data.menu_by_path.contains_key("/ipv6/address"),
        "missing /ipv6/address"
    );
    assert!(
        data.menu_by_path.contains_key("/ipv6/route"),
        "missing /ipv6/route"
    );
    assert!(
        data.menu_by_path.contains_key("/interface/bridge"),
        "missing /interface/bridge"
    );
    assert!(
        data.menu_by_path.contains_key("/interface/ethernet"),
        "missing /interface/ethernet"
    );
    assert!(
        data.menu_by_path.contains_key("/routing/ospf"),
        "missing /routing/ospf"
    );
    assert!(
        data.menu_by_path.contains_key("/routing/bgp"),
        "missing /routing/bgp"
    );

    assert!(
        data.menu_by_path.contains_key("/system/clock"),
        "missing /system/clock"
    );
    assert!(
        data.menu_by_path.contains_key("/tool/ping"),
        "missing /tool/ping"
    );
    assert!(
        data.menu_by_path.contains_key("/queue/simple"),
        "missing /queue/simple"
    );
    assert!(
        data.menu_by_path.contains_key("/user/aaa"),
        "missing /user/aaa"
    );
}

// ── Manual-audit pins over the EMBEDDED dataset ──────────────────────────
//
// The tests below guard what actually SHIPS to users (the table baked in
// via include_str!), end to end through the extraction pipeline. They
// mirror the audit that motivated gating bare-root CLI pages behind a
// **Type:** line in scripts/extract_commands.py.

#[test]
fn test_root_user_page_documents_mandatory_credentials() {
    // The regenerated table must capture the bare-root `/user` page with
    // its mandatory credential properties intact; losing them would
    // degrade completions for basic user management.
    let data = MenuData::load();
    let user = data.menu_by_path.get("/user").expect("/user menu embedded");
    for name in ["name", "group", "password"] {
        let arg = user
            .arguments
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("/user argument `{name}` missing"));
        assert!(arg.required, "/user.{name} must stay required=true");
    }
}

#[test]
fn test_log_read_only_columns_present() {
    // /log documents only read-only output columns; they power hover on
    // the most common print workflow, so losing any of them is a silent
    // feature regression.
    let data = MenuData::load();
    let log = data.menu_by_path.get("/log").expect("/log menu embedded");
    for name in ["buffer", "time", "topics", "message"] {
        assert!(
            log.read_only.iter().any(|r| r.name == name),
            "/log read_only column `{name}` missing"
        );
    }
}
