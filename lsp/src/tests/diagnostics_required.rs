// Missing required, duplicate and enum diagnostics.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod tests` L1564-1614, L1761-1908); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::caps::*;
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
fn test_no_missing_when_required_present() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1/24 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "should not have missing-required when all present"
    );
}

#[test]
fn test_missing_not_emitted_for_print_verb() {
    let data = synthetic_data();
    // print does not require address/interface
    let doc = "/ip/address print";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "print should not trigger missing-required"
    );
}

#[test]
fn test_duplicate_property_warning() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property")),
        "should have duplicate-property, got {:?}",
        diags
    );
    assert!(diags.iter().any(|d| d.message.contains("address")));
}

#[test]
fn test_no_duplicate_when_unique() {
    let data = synthetic_data();
    let doc = "/ip/address add address=1.1.1.1 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property")),
        "should not have duplicate when unique"
    );
}

#[test]
fn test_invalid_enum_hint() {
    let data = synthetic_data();
    let doc = "/ip/firewall/filter add chain=invalid action=accept";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    let hints: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .collect();
    assert!(
        !hints.is_empty(),
        "should have invalid-enum-value, got {:?}",
        diags
    );
    assert!(hints.iter().any(|d| d.message.contains("invalid")));
    assert!(hints.iter().all(|d| d.severity == Some(severity::WARNING)));
}

#[test]
fn test_valid_enum_no_hint() {
    let data = synthetic_data();
    let doc = "/ip/firewall/filter add chain=input action=accept";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "should not have hint for valid enum values"
    );
}

#[test]
fn test_multiple_rules_together() {
    let data = synthetic_data();
    let doc = "/foo/bar add unknown=foo chain=bad\n/ip/address add address=1.1.1.1 interface=ether1 address=1.1.1.1\n/ip/firewall/filter add chain=bad";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property"))
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn test_empty_and_comment_lines_no_diags() {
    let data = synthetic_data();
    let doc = "# comment\n\n   \n:global x 1\n/ip/address add address=1.1.1.1 interface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    // Only last line should be checked, and it's valid
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "comments and empty should not produce diagnostics"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "valid line should not have unknown-property"
    );
}

#[test]
fn test_large_doc_capped() {
    let data = synthetic_data();
    // Generate large doc beyond cap
    let mut doc = String::new();
    for i in 0..4000 {
        doc.push_str(&format!("/foo/unknown{} add badprop=1\n", i));
    }
    let diags = compute_diagnostics(&data, &doc, "file:///test.rsc");
    // Should be capped at MAX_DIAG_LINES (3000) -> at most 3000 diagnostics (one per line)
    // plus one truncation hint (Information) when truncated.
    assert!(
        diags.len() <= MAX_DIAG_LINES + 1,
        "diagnostics should be capped, got {}",
        diags.len()
    );
    // Should still have some diagnostics
    assert!(!diags.is_empty());
}
