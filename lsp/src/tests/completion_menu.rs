// Root and sub-menu completions.
// Copied (not moved) from `lsp/src/completion.rs`; the original block is
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
// ── Root completions ─────────────────────────────────────────────────────

#[test]
fn test_root_completions_empty_input() {
    let data = synthetic_data();
    let items = compute_completions(&data, "");
    assert!(!items.is_empty(), "root completions should not be empty");
    assert!(items.iter().any(|i| i.label == "/ip"), "should contain /ip");
    assert!(
        items.iter().any(|i| i.label == "/interface"),
        "should contain /interface"
    );
    assert!(
        items.iter().any(|i| i.label == "/system"),
        "should contain /system"
    );
    // Root menus keep their CLASS kind and detail text; root Commands
    // (e.g. /import) are FUNCTION/Command per C3.
    for item in items.iter().filter(|i| i.label.starts_with('/')) {
        if item.kind == Some(kind::CLASS) {
            assert!(item.detail.as_ref().unwrap().contains("root menu"));
        } else {
            assert_eq!(item.kind, Some(kind::FUNCTION));
            assert_eq!(item.detail.as_deref(), Some("Command"));
        }
    }
    // …and statement-start snippets are appended on top of them.
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&":if"));
    assert!(labels.contains(&":foreach"));
    assert!(labels.contains(&":for"));
    assert!(labels.contains(&":do"));
}

#[test]
fn test_root_completions_slash_only() {
    // "/" alone must behave like the empty context: parse_line maps it
    // to path "/" which has no child index entry, so compute_completions
    // special-cases it back to ROOT menu completions instead of verbs.
    //
    // Divergence since statement snippets exist: "" is a statement start
    // (nothing typed yet) so it additionally carries the four snippet
    // items; "/" is mid-path navigation (last token "/") so snippets are
    // withheld there. Root menus themselves must stay identical.
    let data = synthetic_data();
    let items_empty = compute_completions(&data, "");
    let items_slash = compute_completions(&data, "/");
    assert!(!items_slash.is_empty());
    let slash_labels: Vec<&str> = items_slash.iter().map(|i| i.label.as_str()).collect();
    assert!(slash_labels.contains(&"/ip"));
    assert!(slash_labels.contains(&"/interface"));
    assert!(slash_labels.contains(&"/system"));
    assert!(!slash_labels.contains(&":if"), "no snippets after '/'");
    // Same ROOT candidate set as the empty context, and NOT verb completions.
    let empty_roots: Vec<&str> = items_empty
        .iter()
        .map(|i| i.label.as_str())
        .filter(|l| l.starts_with('/'))
        .collect();
    assert_eq!(empty_roots, slash_labels);
    assert!(!slash_labels.contains(&"print"));
}

#[test]
fn test_root_completions_are_only_roots() {
    let data = MenuData::load();
    let items = compute_completions(&data, "");
    // All MENU labels should start with / (snippet labels start with ':').
    for item in items.iter().filter(|i| i.label.starts_with('/')) {
        assert!(
            item.label.starts_with('/')
                && (item.kind == Some(kind::CLASS) || item.kind == Some(kind::FUNCTION)),
            "root label should be a CLASS menu or FUNCTION command: {}",
            item.label
        );
        if item.kind == Some(kind::FUNCTION) {
            assert_eq!(item.detail.as_deref(), Some("Command"));
        }
    }
    // Snippets are the only non-menu additions at statement start.
    let extra: Vec<&str> = items
        .iter()
        .map(|i| i.label.as_str())
        .filter(|l| !l.starts_with('/'))
        .collect();
    assert_eq!(extra, vec![":if", ":foreach", ":for", ":do"]);
    // Should contain all 8 roots at least and root Commands per C3
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"/ip"));
    assert!(labels.contains(&"/interface"));
    assert!(labels.contains(&"/system"));
    assert!(labels.contains(&"/tool"));
    assert!(labels.contains(&"/queue"));
    // Root commands (C3) — /import etc. are Commands, not Directory children
    assert!(labels.contains(&"/import"));
    assert!(labels.contains(&"/quit"));
    assert!(labels.contains(&"/beep"));
}

// ── Sub-menu completions ─────────────────────────────────────────────────

#[test]
fn test_submenu_completions_for_ip() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip ");
    // Should contain sub-menus address, route
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"address"),
        "should contain address sub-menu"
    );
    assert!(labels.contains(&"route"), "should contain route sub-menu");
    // Also contains verb-like path? firewall is implicit? Check child_names_by_parent for /ip
    // should have address, route, firewall
    assert!(
        labels.contains(&"firewall"),
        "should contain implicit firewall child"
    );
}

#[test]
fn test_submenu_completions_include_verbs() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/ip/address ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // When no verb yet, should contain both sub-menus (none for /ip/address) and verbs
    assert!(labels.contains(&"add"), "should contain verb add");
    assert!(labels.contains(&"print"), "should contain verb print");
    assert!(labels.contains(&"remove"), "should contain verb remove");
    // Check kind for verbs
    let add_item = items.iter().find(|i| i.label == "add").unwrap();
    assert_eq!(add_item.kind, Some(kind::FUNCTION));
}

#[test]
fn test_submenu_for_unknown_path_returns_verbs_only() {
    let data = synthetic_data();
    let items = compute_completions(&data, "/unknown/path ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // No sub-menus, but verbs should still be present
    assert!(labels.contains(&"add"));
    assert!(labels.contains(&"print"));
    // No sub-menu specific labels
    assert!(!labels.contains(&"address"));
}

#[test]
fn test_submenu_action_command_included_as_verb() {
    let data = synthetic_data();
    // /ip/route has child /ip/route/check of type Command
    let items = compute_completions(&data, "/ip/route ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"check"),
        "should include action command 'check'"
    );
    let check_item = items.iter().find(|i| i.label == "check").unwrap();
    assert_eq!(check_item.detail.as_deref(), Some("action command"));
}
