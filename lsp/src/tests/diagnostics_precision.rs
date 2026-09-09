// Diagnostic range precision and prefix parity.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod extra_coverage` L2359-2410, L3027-3127); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::diagnostics::*;
use crate::menus::MenuData;

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
fn demo_menu_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/alpha"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"

[[menus]]
path = "/demo/enum"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (on | off)"
"#,
    )
}

#[test]
fn test_duplicate_property_highlights_second_occurrence_precisely() {
    // Ranges come from tokenization, so the flagged occurrence
    // is the SECOND property occurrence (bytes 33..40), never the "address"
    // substring inside the menu path (bytes 4..11).
    let md = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
required = true
"#,
    );
    let line = "/ip/address add address=1.1.1.1 address=2.2.2.2";
    let diags = compute_diagnostics(&md, line, "file:///a.rsc");
    let dup = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("duplicate-property"))
        .expect("duplicate-property expected");
    assert_eq!(dup.range.start.line, 0);
    assert_eq!(dup.range.start.character, 32, "must flag second occurrence");
    assert_eq!(dup.range.end.character, 39, "range covers the KEY only");
}

#[test]
fn test_unknown_property_key_inside_menu_path_is_not_misranged() {
    // Key text also appears inside the menu path ("alpha" in "/demo/alpha");
    // the diagnostic must point at the PROPERTY occurrence after "add".
    let md = demo_menu_data();
    let line = "/demo/alpha add alpha=1 name=x";
    let diags = compute_diagnostics(&md, line, "file:///a.rsc");
    let up = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-property"))
        .expect("unknown-property expected for 'alpha'");
    assert_eq!(up.range.start.character, 16, "'alpha' after 'add'");
    assert_eq!(up.range.end.character, 21);
}

#[test]
fn test_quoted_value_with_keylike_substring_no_phantom_diagnostics() {
    // Quote-aware tokenization keeps this as ONE value token, so "alpha="
    // inside the quoted string can no longer fabricate properties.
    let md = demo_menu_data();
    let line = r#"/demo/alpha add name="x alpha=9 y""#;
    let diags = compute_diagnostics(&md, line, "file:///a.rsc");
    assert!(
        diags.is_empty(),
        "quoted key-like substrings must not warn, got {diags:?}"
    );
}

#[test]
fn test_enum_value_range_points_at_value_part_only() {
    let md = demo_menu_data();
    let line = "/demo/enum set mode=bogus";
    let diags = compute_diagnostics(&md, line, "file:///a.rsc");
    let hint = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("invalid-enum-value expected");
    // "/demo/enum set mode=bogus": token "mode=bogus" starts at 15;
    // value part starts after "key=" (15+5=20), ends at token end (25).
    assert_eq!(hint.range.start.character, 20);
    assert_eq!(hint.range.end.character, 25);
}

// ── Known-prefix O(1) parity ──────────────────────────────────

#[test]
fn test_known_prefix_parity_root_deep_implicit_unknown() {
    let md = demo_menu_data();
    // Root prefix of a known menu ("/demo") → known, no warning.
    assert!(
        !compute_diagnostics(&md, "/demo print", "f")
            .iter()
            .any(|d| { d.code.as_deref() == Some("unknown-menu") })
    );
    // Implicit parent with no direct entry but known children → known.
    assert!(
        !compute_diagnostics(&md, "/demo/enum print", "f")
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
    // Exact deep menu → known.
    assert!(
        !compute_diagnostics(&md, "/demo/alpha print", "f")
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
    // Genuinely unknown → still warned, same message shape as before.
    let diags = compute_diagnostics(&md, "/foo/bar add x=1", "f");
    let unk = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-menu"))
        .expect("unknown menu must still be flagged");
    assert!(unk.message.contains("/foo/bar"));
}
