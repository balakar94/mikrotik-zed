// Unknown menu and property diagnostics.
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
#[test]
fn test_unknown_menu_warning() {
    let data = synthetic_data();
    let doc = "/foo/bar add something=1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "should have unknown-menu diagnostic, got {:?}",
        diags
    );
    assert!(diags.iter().any(|d| d.message.contains("/foo/bar")));
    assert!(diags.iter().any(|d| d.severity == Some(severity::WARNING)));
}

#[test]
fn test_known_menu_no_unknown_diag() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1/24 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    // Should not have unknown-menu
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "should not have unknown-menu for known path"
    );
}

#[test]
fn test_unknown_property_warning() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1/24 unknownprop=foo";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property"))
    );
    assert!(diags.iter().any(|d| d.message.contains("unknownprop")));
}

#[test]
fn test_unknown_property_typo_appends_did_you_mean() {
    let data = synthetic_data();
    let doc = "/ip/address add adress=1.1.1.1/24 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-property"))
        .expect("unknown-property must fire for 'adress'");
    assert_eq!(
        d.message,
        "Unknown property 'adress' for '/ip/address'. Did you mean 'address'?"
    );
}

#[test]
fn test_unknown_property_garbage_has_no_suggestion_suffix() {
    let data = synthetic_data();
    let doc = "/ip/address add zzzqqqxxxwww=1 address=1.1.1.1/24 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-property"))
        .expect("unknown-property must fire for garbage key");
    assert!(
        !d.message.contains("Did you mean"),
        "garbage beyond threshold must not suggest, got {:?}",
        d.message
    );
}

#[test]
fn test_unknown_menu_typo_appends_did_you_mean() {
    let data = synthetic_data();
    let doc = "/ip/addres add address=1.1.1.1/24 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-menu"))
        .expect("unknown-menu must fire for '/ip/addres'");
    assert!(
        d.message.contains("Did you mean '/ip/address'?"),
        "menu typo must suggest, got {:?}",
        d.message
    );
}

#[test]
fn test_invalid_enum_typo_appends_did_you_mean() {
    let data = synthetic_data();
    let doc = "/ip/firewall/filter add chain=inpt action=accept";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("invalid-enum-value must fire for 'inpt'");
    assert_eq!(d.severity, Some(severity::WARNING));
    assert!(
        d.message.contains("Did you mean 'input'?"),
        "enum typo must suggest, got {:?}",
        d.message
    );
}

#[test]
fn test_known_property_no_unknown() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1/24 interface=ether1 comment=\"hi\"";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "should not have unknown-property for valid props, got {:?}",
        diags
    );
}

#[test]
fn test_missing_required_info() {
    let data = synthetic_data();
    // /ip/address add requires address and interface
    let doc = "/ip/address add comment=hi";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let missing: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("missing-required"))
        .collect();
    assert!(
        !missing.is_empty(),
        "should have missing-required, got {:?}",
        diags
    );
    assert!(missing.iter().any(|d| d.message.contains("address")));
    assert!(missing.iter().any(|d| d.message.contains("interface")));
    assert!(
        missing
            .iter()
            .all(|d| d.severity == Some(severity::WARNING))
    );
}
