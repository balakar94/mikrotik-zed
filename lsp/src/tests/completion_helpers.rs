// Completion helper pins and real-data sanity.
// Copied (not moved) from `lsp/src/completion.rs` (`mod tests` L1077-1164, L1516-1686); the original block is
// left untouched. Adapted imports for the new location.
use crate::completion::*;
use crate::menus::MenuData;
use crate::text_util::{
    MAX_DETAIL_CHARS, MAX_DETAIL_TYPE_CHARS, sanitize_detail_text, sanitize_label_segment,
};

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
// ── Helpers ───────────────────────────────────────────────────

use crate::menus::parse_enum_values;

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
        enum_values: Vec::new(),
        description: "".to_string(),
        required: false,
        unset: false,
    };
    assert_eq!(get_insert_text(&arg), "comment=\"$1\"$0");
}

#[test]
fn test_get_insert_text_non_string() {
    let arg = crate::menus::ArgEntry {
        name: "address".to_string(),
        arg_type: "ipPrefix".to_string(),
        enum_values: Vec::new(),
        description: "".to_string(),
        required: false,
        unset: false,
    };
    assert_eq!(get_insert_text(&arg), "address=$1$0");
}

#[test]
fn test_get_detail_empty_type() {
    let arg = crate::menus::ArgEntry {
        name: "foo".to_string(),
        arg_type: "".to_string(),
        enum_values: Vec::new(),
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
        enum_values: Vec::new(),
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
fn test_real_data_value_completions_chain_and_action() {
    // Real embedded data: `chain` is a bare "enum" upstream (chains are
    // user-definable) → no documented members, so the curated common
    // hints (input/forward/output) apply instead of silence. `action`
    // embeds the complete member list extracted from the raw docs type
    // string, so value completions work even though its display type is
    // truncated.
    let data = MenuData::load();
    let chain_items = compute_completions(&data, "/ip/firewall/filter add chain=");
    let chain_labels: Vec<&str> = chain_items.iter().map(|i| i.label.as_str()).collect();
    // Unfiltered menus keep curated order (stable sort over tier-only
    // keys) — this pins construction order, not alphabetical order.
    assert_eq!(chain_labels, vec!["input", "forward", "output"]);
    for item in &chain_items {
        assert_eq!(item.detail.as_deref(), Some(COMMON_HINT_DETAIL));
        assert!(
            item.sort_text.as_deref().unwrap_or("").starts_with('5'),
            "common hint tier 5, got {:?}",
            item.sort_text
        );
    }

    let action_items = compute_completions(&data, "/ip/firewall/filter add action=");
    assert!(
        !action_items.is_empty(),
        "action should complete via embedded enum_values"
    );
    let labels: Vec<&str> = action_items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"accept"));
    assert!(labels.contains(&"drop"));
}

// ── Stream B: sanitize reuse ────────────────────────────────────

#[test]
fn test_documentation_from_sanitizes_markdown() {
    let doc = documentation_from("see [docs](https://example.com/x) <b>y</b>".to_string())
        .expect("non-empty");
    assert_eq!(doc.kind, "markdown");
    assert!(doc.value.contains("see docs"), "got {}", doc.value);
    assert!(!doc.value.contains("https://example.com"));
    assert!(!doc.value.contains("<b>"));
}

#[test]
fn test_detail_strings_are_single_line_and_capped() {
    let nasty = "a\nb\rc\td".to_string();
    let clean = sanitize_detail_text(&nasty);
    assert!(!clean.contains('\n'));
    assert!(!clean.contains('\r'));
    assert!(!clean.contains('\t'));
    let long = "x".repeat(400);
    assert!(sanitize_detail_text(&long).chars().count() <= MAX_DETAIL_CHARS + 1);
    assert_eq!(sanitize_label_segment("n", "t"), "n=t");
}

#[test]
fn test_arg_detail_embeds_capped_type() {
    let arg = crate::menus::ArgEntry {
        name: "p".to_string(),
        arg_type: "t".repeat(100),
        enum_values: Vec::new(),
        description: String::new(),
        required: false,
        unset: false,
    };
    let detail = get_detail(&arg);
    assert!(detail.starts_with("type: "));
    assert!(!detail.contains('\n'));
    let typ = detail.strip_prefix("type: ").unwrap();
    assert!(typ.chars().count() <= MAX_DETAIL_TYPE_CHARS);
}
