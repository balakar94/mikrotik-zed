// Diagnostics — severity pins.
use crate::diagnostics;
use crate::diagnostics::severity;
use crate::diagnostics::*;
use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;
fn severity_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
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
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
"#,
    ))
}

fn code_of(
    diags: &[crate::diagnostics::Diagnostic],
    code: &str,
) -> Vec<crate::diagnostics::Diagnostic>
where
    crate::diagnostics::Diagnostic: Clone,
{
    diags
        .iter()
        .filter(|d| d.code.as_deref() == Some(code))
        .cloned()
        .collect()
}

// ── (d) diagnostics severity matrix ──────────────────────────────────────

fn synth() -> MenuData {
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
path = "/interface/list"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus]]
path = "/tool/ping"
type = "Command"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
[[menus.arguments]]
name = "ssl-verify"
type = "bool"
"#,
    )
}
// ── Explicit 5 rules with severity ───────────────────────────────────────

#[test]
fn test_rule3_missing_required_info_for_add_on_directory() {
    let data = synth();
    let diags = compute_diagnostics(&data, "/ip/address add comment=hi", "file:///a.rsc");
    let missing: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("missing-required"))
        .collect();
    assert_eq!(
        missing.len(),
        2,
        "should have 2 missing (address, interface)"
    );
    for m in &missing {
        assert_eq!(m.severity, Some(severity::WARNING));
        assert!(m.message.contains("Missing required"));
    }
    assert!(missing.iter().any(|d| d.message.contains("address")));
    assert!(missing.iter().any(|d| d.message.contains("interface")));
}

#[test]
fn test_rule3_missing_required_for_set_on_directory() {
    let data = synth();
    let diags = compute_diagnostics(&data, "/ip/address set comment=hi", "file:///a.rsc");
    // set on Directory should also require
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
    assert!(diags.iter().any(|d| d.severity == Some(severity::WARNING)));
}

#[test]
fn test_rule3_not_for_command_type() {
    let data = synth();
    // /tool/ping is Command, not Directory, so missing-required should not trigger
    let diags = compute_diagnostics(&data, "/tool/ping address=1.1.1.1", "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
}

#[test]
fn test_rule3_not_for_print_verb() {
    let data = synth();
    let diags = compute_diagnostics(&data, "/ip/address print", "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
}

#[test]
fn test_rule4_duplicate_property_warning_severity() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2",
        "file:///a.rsc",
    );
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("duplicate-property"))
        .expect("duplicate");
    assert_eq!(d.severity, Some(severity::WARNING));
    assert!(d.message.contains("address"));
    // Second occurrence range should be after first
    assert!(d.range.start.character > 0);
}

#[test]
fn test_rule4_duplicate_with_three_occurrences_still_warns() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/address add address=1 interface=ether1 address=2 address=3",
        "file:///a.rsc",
    );
    // Should have at least one duplicate diagnostic
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property"))
    );
}

#[test]
fn test_inline_comment_does_not_spawn_unknown_property_diagnostic() {
    let data = synth();
    let doc = "/ip/address add address=1.1.1.1/24 interface=ether1 # inline note: foo=bar invalid_key=123";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        diags.is_empty(),
        "inline comments must not trigger unknown property diagnostics: {diags:?}"
    );
}

#[test]
fn test_rule5_invalid_enum_hint_severity() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=invalid",
        "file:///a.rsc",
    );
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("hint");
    assert_eq!(d.severity, Some(severity::WARNING));
    assert!(d.message.contains("Invalid value"));
    assert!(d.message.contains("input | forward | output"));
    assert!(d.message.contains("invalid"));
}

#[test]
fn test_rule5_valid_enum_no_hint() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=input",
        "file:///a.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn test_rule5_empty_value_not_hint() {
    let data = synth();
    let diags = compute_diagnostics(&data, "/ip/firewall/filter add chain=", "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn test_rule5_quoted_value_stripped_then_checked() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=\"invalid\"",
        "file:///a.rsc",
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
    let diags2 = compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=\"input\"",
        "file:///a.rsc",
    );
    assert!(
        !diags2
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}
