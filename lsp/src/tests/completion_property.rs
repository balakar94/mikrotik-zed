// Argument and value completions.
// Copied (not moved) from `lsp/src/completion.rs` (`mod tests` L1077-1164, L1325-1515); the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

fn synthetic_data() -> MenuData {
    let toml_str = r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "The IP address"

[[menus.arguments]]
name = "interface"
type = "iface_enum"
description = "Interface name"

[[menus.arguments]]
name = "comment"
type = "string"
description = "Comment"

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = "dynamic"

[[menus]]
path = "/ip/route"
type = "Directory"

[[menus.arguments]]
name = "gateway"
type = "ipAddr"
description = "Gateway address"

[[menus]]
path = "/ip/route/check"
type = "Command"

[[menus]]
path = "/ip/firewall/filter"
type = "Directory"

[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
description = "Chain name"

[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
description = "Action"

[[menus.arguments]]
name = "enabled"
type = "bool"
description = "Enabled flag"

[[menus.arguments]]
name = "src-address"
type = "ipAddr"
description = "Source address"

[[menus]]
path = "/interface/bridge/port"
type = "Directory"

[[menus]]
path = "/system/clock"
type = "Directory"

[[menus.arguments]]
name = "enabled"
type = "bool"

[[menus.arguments]]
name = "time-zone-name"
type = "string"

[[menus]]
path = "/routing/bgp/connection"
type = "Directory"
"#;
    MenuData::from_toml_str(toml_str)
}
// ── Argument completions (after verb) ─────────────────────────

#[test]
fn test_arg_completions_after_verb() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"address"), "should contain address arg");
    assert!(
        labels.contains(&"interface"),
        "should contain interface arg"
    );
    assert!(labels.contains(&"comment"), "should contain comment arg");
    // Should NOT contain verbs
    assert!(
        !labels.contains(&"print"),
        "should not contain verbs when command present"
    );
    assert!(
        !labels.contains(&"add"),
        "should not contain add when command already typed"
    );
    // Check kinds
    let addr_item = items.iter().find(|i| i.label == "address").unwrap();
    assert_eq!(addr_item.kind, Some(kind::PROPERTY));
    assert_eq!(addr_item.insert_text_format, Some(2));
    assert!(addr_item.detail.as_ref().unwrap().contains("ipPrefix"));
}

#[test]
fn test_arg_completions_filter_used_properties() {
    let data = synthetic_data();
    // Already used address=1.1.1.1
    let items = compute_completions(&data, "/ip/address add address=1.1.1.1 ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"address"),
        "already used prop should be filtered"
    );
    assert!(labels.contains(&"interface"), "unused prop should remain");
}

#[test]
fn test_arg_completions_include_flags() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"X"), "should contain flag X");
    assert!(labels.contains(&"D"), "should contain flag D");
    let flag_item = items.iter().find(|i| i.label == "X").unwrap();
    assert_eq!(flag_item.kind, Some(kind::CONSTANT));
}

#[test]
fn test_arg_completions_string_type_has_quoted_insert() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add ");
    let comment_item = items.iter().find(|i| i.label == "comment").unwrap();
    let insert = comment_item.insert_text.as_ref().unwrap();
    assert!(
        insert.contains('"'),
        "string type should have quoted insert_text"
    );
    assert!(insert.contains("$1"), "should be snippet with $1");
}

#[test]
fn test_arg_completions_non_string_type_plain_insert() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add ");
    let addr_item = items.iter().find(|i| i.label == "address").unwrap();
    let insert = addr_item.insert_text.as_ref().unwrap();
    assert_eq!(insert, "address=$1$0");
}

#[test]
fn test_arg_completions_unknown_menu_returns_empty() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/unknown/path add ");
    assert!(items.is_empty(), "unknown menu should return no args");
}

// ── Value completions (after "property=") ──────────────────────

#[test]
fn test_value_completions_enum_chain() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"input"));
    assert!(labels.contains(&"forward"));
    assert!(labels.contains(&"output"));
    for item in &items {
        assert_eq!(item.kind, Some(kind::ENUM_MEMBER));
    }
}

#[test]
fn test_value_completions_enum_action() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/firewall/filter add action=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"accept"));
    assert!(labels.contains(&"drop"));
    assert!(labels.contains(&"reject"));
}

#[test]
fn test_value_completions_bool() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/firewall/filter add enabled=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"yes"));
    assert!(labels.contains(&"no"));
    assert!(labels.contains(&"true"));
    assert!(labels.contains(&"false"));
    assert_eq!(items.len(), 4);
}

#[test]
fn test_value_completions_bool_system_clock() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/system/clock set enabled=");
    assert!(!items.is_empty());
    assert!(items.iter().any(|i| i.label == "yes"));
}

#[test]
fn test_value_completions_iface_enum_zero_items() {
    // Honest placeholders: interface names are device-specific, so an
    // iface_enum property yields ZERO items rather than fabricated
    // suggestions like ether1/bridge.
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add interface=");
    assert!(
        items.is_empty(),
        "iface_enum should produce no fabricated items, got {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
}

#[test]
fn test_value_completions_ipaddr() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add address=");
    // address is ipPrefix -> prefix placeholder
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["0.0.0.0/0"]);
}

#[test]
fn test_value_completions_ipaddr_src_address() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/firewall/filter add src-address=");
    // src-address is ipAddr (NOT ipPrefix) -> host-address placeholder,
    // distinct from the prefix placeholder.
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["0.0.0.0"]);
}

#[test]
fn test_value_completions_unknown_property_empty() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address add unknownprop=");
    assert!(
        items.is_empty(),
        "unknown property should return empty value completions"
    );
}

#[test]
fn test_value_completions_unknown_menu_empty() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/unknown add prop=");
    assert!(items.is_empty());
}

#[test]
fn test_value_completions_with_space_before_equals_not_triggered() {
    let data = synthetic_data();
    // last_token is "chain" not "chain=" -> should be arg completions, not value
    let items = compute_completions(&data, "/ip/firewall/filter add chain");
    // Should be arg completions (not value), so labels contain property names not enum values
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"chain"), "should be arg completions");
    assert!(
        !labels.contains(&"input"),
        "should not be value completions"
    );
}
