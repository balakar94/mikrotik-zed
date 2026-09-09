// Value completion and typed-prefix filtering.
// Copied (not moved) from `lsp/src/completion.rs` (`mod extra_coverage` L1694-1747, L1941-2155);
// the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

fn synthetic() -> MenuData {
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
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/route"
type = "Directory"
[[menus.arguments]]
name = "gateway"
type = "ipAddr"
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
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "src-address"
type = "ipAddr"
[[menus]]
path = "/system/clock"
type = "Directory"
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "time-zone-name"
type = "string"
"#,
    )
}
// ── Values after = ───────────────────────────────────────────────────────

#[test]
fn test_values_after_equals_enum() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"input"));
    assert!(labels.contains(&"forward"));
    assert!(labels.contains(&"output"));
    assert!(items.iter().all(|i| i.kind == Some(kind::ENUM_MEMBER)));
}

#[test]
fn test_values_after_equals_bool() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add enabled=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(items.len(), 4);
    assert!(labels.contains(&"yes"));
    assert!(labels.contains(&"no"));
    assert!(labels.contains(&"true"));
    assert!(labels.contains(&"false"));
}

#[test]
fn test_values_after_equals_iface_enum_zero_items() {
    // Honest placeholders: iface_enum yields nothing device-specific.
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add interface=");
    assert!(items.is_empty());
}

#[test]
fn test_values_after_equals_ip_prefix() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add address=");
    assert!(items.iter().any(|i| i.label == "0.0.0.0/0"));
    assert!(items[0].detail.as_ref().unwrap().contains("ipPrefix"));
}

#[test]
fn test_values_after_equals_ip_addr() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/route add gateway=");
    // ipAddr gets the HOST placeholder, distinct from ipPrefix's /0 form.
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["0.0.0.0"]);
    let det = items[0].detail.as_ref().unwrap();
    assert!(det.contains("ipAddr"));
}

#[test]
fn test_values_after_equals_requires_trailing_equals() {
    let data = synthetic();
    // Without "=", should be arg completions, not value
    let arg_items = compute_completions(&data, "/ip/firewall/filter add chain");
    assert!(arg_items.iter().any(|i| i.label == "chain"));
    assert!(!arg_items.iter().any(|i| i.label == "input"));
    // With "=", should be value completions
    let val_items = compute_completions(&data, "/ip/firewall/filter add chain=");
    assert!(val_items.iter().any(|i| i.label == "input"));
    assert!(!val_items.iter().any(|i| i.label == "chain"));
}

#[test]
fn test_values_after_equals_unknown_property_empty() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add unknown=");
    assert!(items.is_empty());
}

#[test]
fn test_values_after_equals_unknown_menu_empty() {
    let data = synthetic();
    let items = compute_completions(&data, "/unknown add prop=");
    assert!(items.is_empty());
}

#[test]
fn test_values_after_equals_bool_also_triggers_iface_check_independent() {
    // Ensure bool and iface_enum are independent: a bool prop should not get iface values
    let data = synthetic();
    let bool_items = compute_completions(&data, "/system/clock set enabled=");
    // enabled is bool -> should have yes/no/true/false but not ether1
    assert!(bool_items.iter().any(|i| i.label == "yes"));
    assert!(!bool_items.iter().any(|i| i.label == "ether1"));
}

#[test]
fn test_completion_deterministic_no_panic_on_weird_input() {
    let data = synthetic();
    let weird = [
        "",
        " ",
        "/",
        "/ ",
        "///",
        "add",
        "===",
        "address===",
        "/ip/address add address= a",
        "/ip/address add \"comment=\"",
    ];
    for w in weird {
        let items = compute_completions(&data, w);
        // Should not panic, and result is Vec (maybe empty)
        let _ = items.len();
    }
}

#[test]
fn test_completion_with_real_data_smoke() {
    let data = MenuData::load();
    let cases = [
        "",
        "/",
        "/ip ",
        "/ip/address ",
        "/ip/address add ",
        "/ip/address add address=",
        "/ip/firewall/filter add chain=",
        "/system/clock set enabled=",
    ];
    for c in cases {
        let items = compute_completions(&data, c);
        // Ensure no panic and items is vec
        assert!(items.len() < 10000, "unexpected huge completion for {c}");
    }
}

// ── Partial value completion ("token contains =") ────────────────────────

#[test]
fn test_partial_value_prefix_filters_enum() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=in");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["input"], "only 'input' matches prefix 'in'");
}

#[test]
fn test_partial_value_prefix_case_insensitive() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=IN");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["input"]);
}

#[test]
fn test_partial_value_no_match_falls_back_to_unfiltered() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=zzz");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"input") && labels.contains(&"forward") && labels.contains(&"output"),
        "non-matching non-empty prefix must fall back to the full set"
    );
}

#[test]
fn test_partial_value_opening_quote_stripped_from_prefix() {
    let data = synthetic();
    // Token ends inside an opened quote: the quote char must not break
    // the case-insensitive prefix filter…
    let items = compute_completions(&data, "/ip/firewall/filter add chain=\"in");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["input"]);

    // …and accepting an item inserts the BARE value so the already-typed
    // opening quote is never doubled.
    assert_eq!(items[0].insert_text.as_deref(), Some("input"));
    let after_quote = compute_completions(&data, "/ip/firewall/filter add chain=\"");
    assert_eq!(after_quote.len(), 3, "empty effective prefix → unfiltered");
    assert!(
        after_quote
            .iter()
            .all(|i| !i.insert_text.as_deref().unwrap_or("").contains('"')),
        "value inserts stay quote-free"
    );
}

#[test]
fn test_partial_value_chain_in_suggests_values_not_args() {
    // Regression guard for the exact scenario in the spec: `chain=in`
    // must suggest chain VALUES, not the argument list.
    let data = synthetic();
    let items = compute_completions(&data, "/ip/firewall/filter add chain=in");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"input"));
    assert!(
        !labels.contains(&"action"),
        "must not be argument completions"
    );
    assert!(!labels.contains(&"enabled"));
}

#[test]
fn test_trailing_space_after_value_stays_argument_completion() {
    let data = synthetic();
    // Cursor AFTER the finished token: value branch must NOT trigger.
    let items = compute_completions(&data, "/ip/firewall/filter add chain=input ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"action"),
        "finished value + space → next property suggestions"
    );
    assert!(
        !labels.contains(&"forward"),
        "must not be value completions"
    );
    // The used property is filtered out of the argument list.
    assert!(!labels.contains(&"chain"));
}
