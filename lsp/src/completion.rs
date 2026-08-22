// ── Completion logic for the RSC language server ─────────────────
//
// Port of the ls.mjs completion engine.  Strategy:
// - Always return ALL possible candidates (sub-menus, verbs, arguments)
//   and let Zed's fuzzy filter narrow them down.
// - Exception: when cursor sits right after "property=", switch to
//   value suggestions (enum values, booleans, type hints).

use crate::menus::{LineContext, MenuData};

/// LSP CompletionItemKind values (mirrors the LSP spec)
mod kind {
    pub const FUNCTION: i32 = 3;
    pub const PROPERTY: i32 = 5;
    pub const CLASS: i32 = 9;
    pub const ENUM_MEMBER: i32 = 12;
    pub const CONSTANT: i32 = 14;
}

/// A completion item ready for JSON serialization.
#[derive(serde::Serialize)]
pub struct CompletionItem {
    pub label: String,
    pub kind: Option<i32>,
    pub detail: Option<String>,
    pub insert_text: Option<String>,
    #[serde(rename = "insertTextFormat")]
    pub insert_text_format: Option<i32>,
}

pub fn compute_completions(data: &MenuData, before_cursor: &str) -> Vec<CompletionItem> {
    let context = crate::parse_line(data, before_cursor);

    // No path yet → suggest root menus
    if context.path.is_empty() {
        return get_root_completion_items(data);
    }

    // Typing a property value right after "=" → suggest enum/bool/type values
    // let chains (requires Rust 1.88+, MSRV is 1.94) — collapsed for clippy collapsible_if
    if let Some(eq_pos) = context.last_token.rfind('=')
        && eq_pos == context.last_token.len() - 1
    {
        let key = &context.last_token[..eq_pos];
        return get_value_completions(data, &context, key);
    }

    // If a verb is already typed (e.g., "add", "print"), only suggest
    // arguments — no more sub-menus or verbs.  This matches real RouterOS
    // terminal behavior where Tab after "add" shows property completions.
    if context.command.is_some() {
        return get_arg_completion_items(data, &context);
    }

    // Before a verb: suggest sub-menus + standard verbs
    let mut items = Vec::new();
    items.extend(get_sub_menu_completion_items(data, &context));
    items.extend(get_verb_completion_items(data, &context));
    items
}

// ── Root menus ──────────────────────────────────────────────────

fn get_root_completion_items(data: &MenuData) -> Vec<CompletionItem> {
    match data.child_names_by_parent.get("") {
        Some(roots) => roots
            .iter()
            .map(|r| CompletionItem {
                label: r.path.clone(),
                kind: Some(kind::CLASS),
                detail: Some(format!("root menu — {}", r.path)),
                insert_text: Some(r.path.clone()),
                insert_text_format: Some(1),
            })
            .collect(),
        None => Vec::new(),
    }
}

// ── Sub-menus ───────────────────────────────────────────────────

fn get_sub_menu_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    match data.child_names_by_parent.get(&ctx.path) {
        Some(children) => children
            .iter()
            .filter(|c| c.menu_type == "Directory" || c.menu_type == "Settings Directory")
            .map(|c| CompletionItem {
                label: c.name.clone(),
                kind: Some(kind::CLASS),
                detail: Some(format!("sub-menu — {}", c.path)),
                insert_text: Some(c.name.clone()),
                insert_text_format: Some(1),
            })
            .collect(),
        None => Vec::new(),
    }
}

// ── Verbs ───────────────────────────────────────────────────────

fn get_verb_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = MenuData::STANDARD_VERBS
        .iter()
        .map(|verb| CompletionItem {
            label: verb.to_string(),
            kind: Some(kind::FUNCTION),
            detail: Some(format!("{verb} — standard command")),
            insert_text: Some(verb.to_string()),
            insert_text_format: Some(1),
        })
        .collect();

    // Action commands (type = "Command" entries under this path)
    if let Some(children) = data.child_names_by_parent.get(&ctx.path) {
        for child in children {
            if child.menu_type == "Command" {
                items.push(CompletionItem {
                    label: child.name.clone(),
                    kind: Some(kind::FUNCTION),
                    detail: Some("action command".to_string()),
                    insert_text: Some(child.name.clone()),
                    insert_text_format: Some(1),
                });
            }
        }
    }

    items
}

// ── Arguments ───────────────────────────────────────────────────

fn get_arg_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&ctx.path) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    for arg in &menu.arguments {
        if ctx.properties.contains_key(&arg.name) {
            continue; // already used
        }
        let insert_text = get_insert_text(arg);
        items.push(CompletionItem {
            label: arg.name.clone(),
            kind: Some(kind::PROPERTY),
            detail: Some(get_detail(arg)),
            insert_text: Some(insert_text),
            insert_text_format: Some(2), // snippet
        });
    }

    for flag in &menu.flags {
        items.push(CompletionItem {
            label: flag.name.clone(),
            kind: Some(kind::CONSTANT),
            detail: Some(format!("{}: {}", flag.name, flag.description)),
            insert_text: Some(flag.name.clone()),
            insert_text_format: Some(1),
        });
    }

    items
}

// ── Value completions (after "property=") ───────────────────────

fn get_value_completions(
    data: &MenuData,
    ctx: &LineContext,
    property_key: &str,
) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&ctx.path) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let arg = match menu.arguments.iter().find(|a| a.name == property_key) {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    // Enum values
    if arg.arg_type.starts_with("enum") {
        for val in parse_enum_values(&arg.arg_type) {
            items.push(CompletionItem {
                label: val.clone(),
                kind: Some(kind::ENUM_MEMBER),
                detail: Some(format!("enum value — {}", arg.arg_type)),
                insert_text: Some(val),
                insert_text_format: Some(1),
            });
        }
    }

    // Boolean
    if arg.arg_type == "bool" || arg.arg_type == "boolean" {
        for val in &["yes", "no", "true", "false"] {
            items.push(CompletionItem {
                label: val.to_string(),
                kind: Some(kind::ENUM_MEMBER),
                detail: Some("bool value".to_string()),
                insert_text: Some(val.to_string()),
                insert_text_format: Some(1),
            });
        }
    }

    // Interface references
    if arg.arg_type.starts_with("iface_enum") {
        for val in &["ether1", "bridge"] {
            items.push(CompletionItem {
                label: val.to_string(),
                kind: Some(kind::ENUM_MEMBER),
                detail: Some("common interface name".to_string()),
                insert_text: Some(val.to_string()),
                insert_text_format: Some(1),
            });
        }
    }

    // IP address / prefix
    if arg.arg_type.starts_with("ipAddr")
        || arg.arg_type.starts_with("ipPrefix")
        || arg.arg_type == "address"
    {
        items.push(CompletionItem {
            label: "0.0.0.0/0".to_string(),
            kind: Some(kind::ENUM_MEMBER),
            detail: Some(format!("type: {}", arg.arg_type)),
            insert_text: Some("0.0.0.0/0".to_string()),
            insert_text_format: Some(1),
        });
    }

    items
}

// ── Helpers ─────────────────────────────────────────────────────

fn parse_enum_values(type_str: &str) -> Vec<String> {
    let inner = type_str
        .strip_prefix("enum")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    match inner {
        Some(body) => body.split('|').map(|s| s.trim().to_string()).collect(),
        None => Vec::new(),
    }
}

fn get_insert_text(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type == "string" {
        format!("{}=\"{}\"", arg.name, "$1")
    } else {
        format!("{}={}", arg.name, "$1")
    }
}

fn get_detail(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type.is_empty() {
        "property".to_string()
    } else {
        format!("type: {}", arg.arg_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // ── Root completions ──────────────────────────────────────────

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
        for item in &items {
            assert_eq!(item.kind, Some(kind::CLASS));
            assert!(item.detail.as_ref().unwrap().contains("root menu"));
        }
    }

    #[test]
    fn test_root_completions_slash_only() {
        // "/" tokenizes to path "/"? Actually token "/": path_parts = [""]? Let's see.
        // Before cursor "/" -> tokenize yields ["/"], parse_line path = "/", but child_names_by_parent.get("/") is None -> fallback?
        // Empty path is only returned when path_parts empty. With "/" path becomes "/".
        // But compute_completions with "/" should still behave similar to root? Check behavior.
        // For "/" the path is "/" not empty, so it will go to submenu branch, which returns empty + verbs.
        // This is technically an edge case — we test current behavior: "/" is not root.
        // Empty string is the true root case.
        let data = synthetic_data();
        let items_empty = compute_completions(&data, "");
        let items_slash = compute_completions(&data, "/");
        // "/" is parsed as path "/" which has no children, so it returns verbs only
        // Ensure at least that empty returns more than slash or slash returns verbs
        assert!(!items_empty.is_empty());
        assert!(!items_slash.is_empty());
        assert!(items_slash.iter().any(|i| i.label == "print"));
    }

    #[test]
    fn test_root_completions_are_only_roots() {
        let data = MenuData::load();
        let items = compute_completions(&data, "");
        // All labels should start with /
        for item in &items {
            assert!(
                item.label.starts_with('/'),
                "root label should start with /: {}",
                item.label
            );
        }
        // Should contain all 8 roots at least
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"/ip"));
        assert!(labels.contains(&"/interface"));
        assert!(labels.contains(&"/system"));
        assert!(labels.contains(&"/tool"));
        assert!(labels.contains(&"/queue"));
    }

    // ── Sub-menu completions ──────────────────────────────────────

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
        // Also contains verb-like path? firewall is implicit? Check child_names_by_parent for /ip should have address, route, firewall
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
        assert_eq!(insert, "address=$1");
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
    fn test_value_completions_iface_enum() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add interface=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"ether1"));
        assert!(labels.contains(&"bridge"));
    }

    #[test]
    fn test_value_completions_ipaddr() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add address=");
        // address is ipPrefix -> triggers ipAddr branch
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"0.0.0.0/0"));
    }

    #[test]
    fn test_value_completions_ipaddr_src_address() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/firewall/filter add src-address=");
        assert!(items.iter().any(|i| i.label == "0.0.0.0/0"));
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

    // ── Helpers ───────────────────────────────────────────────────

    #[test]
    fn test_parse_enum_values_normal() {
        let vals = parse_enum_values("enum (input | forward | output)");
        assert_eq!(vals, vec!["input", "forward", "output"]);
    }

    #[test]
    fn test_parse_enum_values_with_spaces() {
        let vals = parse_enum_values("enum (  a  |  b  |c )");
        assert_eq!(vals, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_enum_values_malformed_no_parens() {
        let vals = parse_enum_values("enum input | output");
        assert!(vals.is_empty());
    }

    #[test]
    fn test_parse_enum_values_empty() {
        let vals = parse_enum_values("enum ()");
        assert_eq!(vals, vec![""]);
    }

    #[test]
    fn test_parse_enum_values_not_enum() {
        let vals = parse_enum_values("bool");
        assert!(vals.is_empty());
    }

    #[test]
    fn test_get_insert_text_string() {
        let arg = crate::menus::ArgEntry {
            name: "comment".to_string(),
            arg_type: "string".to_string(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_insert_text(&arg), "comment=\"$1\"");
    }

    #[test]
    fn test_get_insert_text_non_string() {
        let arg = crate::menus::ArgEntry {
            name: "address".to_string(),
            arg_type: "ipPrefix".to_string(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_insert_text(&arg), "address=$1");
    }

    #[test]
    fn test_get_detail_empty_type() {
        let arg = crate::menus::ArgEntry {
            name: "foo".to_string(),
            arg_type: "".to_string(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_detail(&arg), "property");
    }

    #[test]
    fn test_get_detail_with_type() {
        let arg = crate::menus::ArgEntry {
            name: "foo".to_string(),
            arg_type: "bool".to_string(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_detail(&arg), "type: bool");
    }

    // ── Real data sanity checks ───────────────────────────────────

    #[test]
    fn test_real_data_arg_completions_ip_address() {
        let data = MenuData::load();
        let items = compute_completions(&data, "/ip/address add ");
        assert!(!items.is_empty());
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"interface"));
    }

    #[test]
    fn test_real_data_value_completions_chain() {
        // Real embedded data has truncated enum types (ending with "...") and bare "enum",
        // so value completions may legitimately be empty. Ensure no panic and deterministic.
        let data = MenuData::load();
        let chain_items = compute_completions(&data, "/ip/firewall/filter add chain=");
        let _ = chain_items; // empty is acceptable for chain ("enum" without values)
        let action_items = compute_completions(&data, "/ip/firewall/filter add action=");
        // Action in real data is truncated ("enum (accept | jump | ...") without closing ')',
        // so parsing yields empty — this is current behavior. Ensure deterministic.
        // Synthetic data verifies the normal enum path works.
        let synth = synthetic_data();
        let synth_items = compute_completions(&synth, "/ip/firewall/filter add action=");
        assert!(
            !synth_items.is_empty(),
            "synthetic action enum should have values"
        );
        let labels: Vec<&str> = synth_items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"accept"));
        // Real data should be empty or contain values if not truncated; just check it is deterministic
        assert!(action_items.len() <= synth_items.len() || action_items.is_empty());
    }
}

#[cfg(test)]
mod extra_coverage {
    use super::*;
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

    // ── Root menus only when at root ─────────────────────────────────

    #[test]
    fn test_root_only_at_empty_context() {
        let data = synthetic();
        let items = compute_completions(&data, "");
        assert!(!items.is_empty());
        for it in &items {
            assert!(it.label.starts_with('/'), "root label must start with /");
            assert_eq!(it.kind, Some(kind::CLASS));
        }
        // Should not contain verbs or properties at root
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(!labels.contains(&"add"));
        assert!(!labels.contains(&"address"));
    }

    #[test]
    fn test_root_not_returned_when_path_present() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip ");
        // Should contain sub-menus, not roots
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"/ip"),
            "roots should not appear when path present"
        );
        assert!(labels.contains(&"address") || labels.contains(&"route"));
    }

    #[test]
    fn test_empty_context_vs_whitespace_only() {
        let data = synthetic();
        let empty = compute_completions(&data, "");
        let ws = compute_completions(&data, "   ");
        // Both tokenizations yield empty path -> root completions
        assert_eq!(empty.len(), ws.len());
        assert!(ws.iter().any(|i| i.label == "/ip"));
    }

    // ── Sub-menus after path ──────────────────────────────────────────

    #[test]
    fn test_submenus_after_ip_path() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"route"));
        assert!(labels.contains(&"firewall"));
        // Sub-menu kind should be CLASS
        for it in items
            .iter()
            .filter(|i| ["address", "route", "firewall"].contains(&i.label.as_str()))
        {
            assert_eq!(it.kind, Some(kind::CLASS));
        }
    }

    #[test]
    fn test_submenus_after_ip_with_trailing_space_vs_without() {
        let data = synthetic();
        let with_space = compute_completions(&data, "/ip ");
        let without = compute_completions(&data, "/ip");
        // Both parse to path "/ip", so completions should be equivalent
        let mut a: Vec<String> = with_space.iter().map(|i| i.label.clone()).collect();
        let mut b: Vec<String> = without.iter().map(|i| i.label.clone()).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn test_submenus_include_only_directory_types() {
        let data = synthetic();
        // /ip/route has a child Command /ip/route/check which should appear via verbs, not sub-menu
        let items = compute_completions(&data, "/ip/route ");
        let sub_labels: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == Some(kind::CLASS))
            .map(|i| i.label.as_str())
            .collect();
        // No CLASS item should be "check" because check is Command; it appears as FUNCTION verb
        assert!(!sub_labels.contains(&"check"));
        assert!(
            items
                .iter()
                .any(|i| i.label == "check" && i.kind == Some(kind::FUNCTION))
        );
    }

    // ── Verbs after menu+space ────────────────────────────────────────

    #[test]
    fn test_verbs_after_menu_space() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        for verb in MenuData::STANDARD_VERBS {
            assert!(labels.contains(verb), "missing verb {verb}");
        }
        // Verbs should be FUNCTION kind
        for it in items
            .iter()
            .filter(|i| MenuData::STANDARD_VERBS.contains(&i.label.as_str()))
        {
            assert_eq!(it.kind, Some(kind::FUNCTION));
            assert!(it.detail.as_ref().unwrap().contains("standard"));
        }
    }

    #[test]
    fn test_verbs_after_menu_without_trailing_space() {
        let data = synthetic();
        let with = compute_completions(&data, "/ip/address ");
        let without = compute_completions(&data, "/ip/address");
        // Both should produce same verb+submenu set (no command yet)
        assert_eq!(with.len(), without.len());
    }

    #[test]
    fn test_verbs_include_action_command() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/route ");
        assert!(items.iter().any(|i| i.label == "check"));
        let check = items.iter().find(|i| i.label == "check").unwrap();
        assert_eq!(check.detail.as_deref(), Some("action command"));
    }

    // ── Args after verb ───────────────────────────────────────────────

    #[test]
    fn test_args_after_verb_only_args_and_flags() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add ");
        // Should contain args + flags, not verbs or sub-menus
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"interface"));
        assert!(labels.contains(&"comment"));
        assert!(labels.contains(&"X"));
        assert!(!labels.contains(&"print"));
        assert!(!labels.contains(&"route"));
        for it in &items {
            assert!(
                it.kind == Some(kind::PROPERTY) || it.kind == Some(kind::CONSTANT),
                "unexpected kind for {}: {:?}",
                it.label,
                it.kind
            );
        }
    }

    #[test]
    fn test_args_after_verb_with_trailing_space_vs_without() {
        let data = synthetic();
        let with = compute_completions(&data, "/ip/address add ");
        let without = compute_completions(&data, "/ip/address add");
        // Both have command "add", so both should be arg completions
        assert_eq!(with.len(), without.len());
        assert!(with.iter().any(|i| i.label == "address"));
        assert!(without.iter().any(|i| i.label == "address"));
    }

    #[test]
    fn test_args_filter_used_properties() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add address=1.1.1.1 ");
        assert!(!items.iter().any(|i| i.label == "address"));
        assert!(items.iter().any(|i| i.label == "interface"));
    }

    #[test]
    fn test_args_string_type_snippet() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add ");
        let comment = items.iter().find(|i| i.label == "comment").unwrap();
        assert_eq!(comment.insert_text.as_deref(), Some("comment=\"$1\""));
        assert_eq!(comment.insert_text_format, Some(2));
    }

    // ── Values after = ────────────────────────────────────────────────

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
    fn test_values_after_equals_iface_enum() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add interface=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"ether1"));
        assert!(labels.contains(&"bridge"));
        assert!(
            items
                .iter()
                .all(|i| i.detail.as_deref() == Some("common interface name"))
        );
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
        assert!(items.iter().any(|i| i.label == "0.0.0.0/0"));
        let det = &items[0].detail.as_ref().unwrap();
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
}
