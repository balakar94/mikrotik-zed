// Comma-separated enums and shorthand commands.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod tests` L1564-1614, L2119-2260); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::diagnostics::*;
use crate::menus::MenuData;

fn synthetic_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
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
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus]]
path = "/system/clock"
type = "Directory"
[[menus.arguments]]
name = "time-zone-name"
type = "string"
[[menus.arguments]]
name = "enabled"
type = "bool"
"#,
    )
}
#[test]
fn test_comma_separated_enum_single_value_still_strict() {
    let data = synthetic_data();
    let doc = "/ip/firewall/filter add chain=invalid action=accept";
    let diags = compute_diagnostics(&data, doc, "file:///t.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "single invalid value must still hint"
    );
}

// ── Centralized parser fix (slash-verb, quote-aware =, brackets) ──

fn slash_verb_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ipv6/nd/prefix"
type = "Directory"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "comment"
type = "string"
[[menus]]
path = "/log"
type = "Directory"
[[menus]]
path = "/system/scheduler"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
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
"#,
    )
}

#[test]
fn test_slash_verb_shorthand_no_unknown_menu() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        "/ipv6/nd/prefix/add interface=bridge",
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "slash-verb shorthand must resolve via Rule 1 parse_line fix, got {diags:?}"
    );
}

#[test]
fn test_log_info_quoted_equals_no_bogus_properties() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(&data, r#"/log info ("digi prevPd=" . $x)"#, "file:///t.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "quoted `=` must not spawn unknown-property keys, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "got {diags:?}"
    );
}

#[test]
fn test_bracket_find_inner_keys_ignored() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        "/ip/address set [find pool-name=digi-ipv6] address=1.1.1.1 interface=ether1",
        "file:///t.rsc",
    );
    assert!(
        !diags.iter().any(|d| d
            .code
            .as_deref()
            .is_some_and(|c| c == "unknown-property" && d.message.contains("pool-name"))),
        "inner bracket key must stay invisible, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property")),
        "got {diags:?}"
    );
}

#[test]
fn test_quoted_comment_with_equals_still_property_no_warning() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        r#"/ip/address add address=1.1.1.1 interface=ether1 comment="a=b c=d""#,
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "comment must stay a known property, got {diags:?}"
    );
}

#[test]
fn test_concat_comment_still_property_no_warning() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        r#"/ip/address add address=1.1.1.1 interface=ether1 comment=("X old=" . $y)"#,
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "concat comment must stay property comment, got {diags:?}"
    );
}
