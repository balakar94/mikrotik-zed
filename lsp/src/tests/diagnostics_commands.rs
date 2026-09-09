// Command diagnostics and set/add selector suppression.
// Copied (not moved) from `lsp/src/diagnostics.rs`; the original block is
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
fn test_unknown_command_warning() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        "/ip/address adn address=1.1.1.1 interface=ether1",
        "file:///t.rsc",
    );
    let unk: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("unknown-command"))
        .collect();
    assert_eq!(unk.len(), 1, "one unknown-command, got {diags:?}");
    assert!(unk[0].message.contains("adn"));
    assert_eq!(unk[0].severity, Some(severity::WARNING));
}

#[test]
fn test_known_command_no_unknown_command() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(
        &data,
        "/ip/address add address=1.1.1.1 interface=ether1",
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-command")),
        "known verb must stay clean, got {diags:?}"
    );
}

#[test]
fn test_unknown_command_skipped_when_path_unknown() {
    let data = slash_verb_data();
    let diags = compute_diagnostics(&data, "/foo/bar adn x=1", "file:///t.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "Rule 1 must fire, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-command")),
        "no cascade on unknown path, got {diags:?}"
    );
}

#[test]
fn test_set_with_find_selector_suppresses_missing_required() {
    let data = synthetic_data();
    let diags = compute_diagnostics(
        &data,
        "/ip/address set [find interface=ether1] interface=ether1",
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "set with [find] selector must not require creation args, got {diags:?}"
    );
}

#[test]
fn test_set_with_numeric_selector_suppresses_missing_required() {
    let data = synthetic_data();
    let diags = compute_diagnostics(&data, "/ip/address set 0 interface=ether1", "file:///t.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "set with numeric selector must not require creation args, got {diags:?}"
    );
}

#[test]
fn test_add_without_selector_still_requires() {
    let data = synthetic_data();
    let diags = compute_diagnostics(&data, "/ip/address add comment=hi", "file:///t.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "add without required args must still warn, got {diags:?}"
    );
}
