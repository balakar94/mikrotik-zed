// Diagnostic source, ranges and line continuations.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod extra_coverage` L2359-2410, L2793-2907);
// the original block is
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
#[test]
fn test_diagnostics_source_always_rsc_ls() {
    let data = synth();
    let doc = "/foo/bar add x=1\n/ip/address add unknown=1\n/ip/address add address=1 interface=ether1 address=2\n/ip/firewall/filter add chain=bad";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    for d in &diags {
        assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    }
}

#[test]
fn test_diagnostics_range_within_line() {
    let data = synth();
    let line = "/foo/bar add x=1";
    let diags = compute_diagnostics(&data, line, "file:///a.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-menu"))
        .unwrap();
    assert_eq!(d.range.start.line, 0);
    // Path "/foo/bar" starts at 0, ends at 8
    assert_eq!(d.range.start.character, 0);
    assert_eq!(d.range.end.character, 8);
}

// ── RouterOS backslash line continuation ─────────────────────────────────

#[test]
fn test_continuation_quoted_url_no_unknown_menu() {
    let data = synth();
    // Real-world reproduction: /tool/fetch URL split across lines with a
    // trailing backslash inside a quoted string. The second physical line
    // starts with '/' and must NOT be diagnosed as an unknown menu.
    let doc = concat!(
        "/tool/fetch add ssl-verify=no url=\"https://raw.githubusercontent.com",
        "/hagezi/dns-blocklists\\\n/main/hosts/pro.txt\"",
    );
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        diags.is_empty(),
        "joined continuation must not produce diagnostics, got {diags:?}"
    );
}

#[test]
fn test_continuation_property_split_recognized() {
    let data = synth();
    // Property split across lines. Note the space BEFORE the backslash:
    // RouterOS removes the newline without inserting whitespace, so a
    // separating space must be present for the tokens to stay distinct
    // (exactly as on a real router).
    let doc = "/ip/address add address=10.0.0.1/24 \\\ninterface=ether1";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required")),
        "interface must be recognized via continuation, got {diags:?}"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "no unknown property expected, got {diags:?}"
    );
}

#[test]
fn test_continuation_range_maps_to_physical_lines() {
    let data = synth();
    let doc = "/ip/address add bogusprop=x\\\n other=y";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    let ups: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("unknown-property"))
        .collect();
    assert_eq!(ups.len(), 2, "expected two unknown-property, got {ups:?}");

    // 'bogusprop' lives in the first segment: joined offset == physical
    // offset on line 0 ("/ip/address add " is 16 bytes).
    let bogus = ups
        .iter()
        .find(|d| d.message.contains("'bogusprop'"))
        .expect("bogusprop diag");
    assert_eq!(bogus.range.start.line, 0);
    assert_eq!(bogus.range.start.character, 16);
    assert_eq!(bogus.range.end.character, 25);

    // ' other=y' is appended verbatim from physical line 1, so 'other'
    // starts at character 1 of line 1.
    let other = ups
        .iter()
        .find(|d| d.message.contains("Unknown property 'other'"))
        .expect("other diag");
    assert_eq!(other.range.start.line, 1);
    assert_eq!(other.range.start.character, 1);
    assert_eq!(other.range.end.line, 1);
    assert_eq!(other.range.end.character, 6);
}

#[test]
fn test_escaped_backslash_not_continuation() {
    let data = synth();
    // Line 1 ends with an escaped backslash pair ("...with \\"): even run,
    // so it does NOT swallow the next command line.
    let doc = "/ip/address add comment=\"ends with \\\\\n/foo/bar add x=1";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    let menu = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-menu"))
        .expect("/foo/bar must still be flagged as unknown menu");
    assert!(menu.message.contains("/foo/bar"));
    assert_eq!(menu.range.start.line, 1);
}
