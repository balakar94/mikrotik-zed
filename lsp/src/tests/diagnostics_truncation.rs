// Empty docs, truncation caps and incremental fixes.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod extra_coverage` L2359-2410, L2616-2792); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::caps::*;
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
// ── Empty and comment-only docs ────────────────────────────────────

#[test]
fn test_empty_doc_no_diags() {
    let data = synth();
    let diags = compute_diagnostics(&data, "", "file:///a.rsc");
    assert!(diags.is_empty());
}

#[test]
fn test_whitespace_only_no_diags() {
    let data = synth();
    let diags = compute_diagnostics(&data, "   \n\n\t\n  ", "file:///a.rsc");
    assert!(diags.is_empty());
}

#[test]
fn test_comment_only_no_diags() {
    let data = synth();
    let doc = "# comment\n# another\n   # indented\n";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(diags.is_empty());
}

#[test]
fn test_global_and_brace_lines_no_diags() {
    let data = synth();
    let doc = ":global x 1\n:local y 2\n{\n}\n..\n";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(diags.is_empty());
}

#[test]
fn test_mixed_valid_and_comments() {
    let data = synth();
    let doc = "# comment\n\n/ip/address add address=1.1.1.1 interface=ether1\n# trailing\n";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property"))
    );
}

// ── Large doc caps ─────────────────────────────────────────────────

#[test]
fn test_large_doc_capped_at_max_diag_lines() {
    let data = synth();
    let doc = "/unknown/menu add x=1\n".repeat(4000);
    let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
    assert!(diags.len() <= MAX_DIAG_LINES + 1);
    assert!(diags.len() <= 3001);
    assert!(!diags.is_empty());
    // All except the truncation hint should be unknown-menu
    assert!(
        diags
            .iter()
            .filter(|d| d.code.as_deref() != Some("truncated"))
            .all(|d| d.code.as_deref() == Some("unknown-menu"))
    );
}

#[test]
fn test_large_doc_capped_at_max_diag_bytes() {
    let data = synth();
    // Each line ~30 bytes, need >500KB => ~17000 lines, but MAX_DIAG_LINES is 3000 so lines cap hits first
    // To test bytes cap, use long lines
    let long_line = format!("/unknown/menu add x={}\n", "a".repeat(500));
    let doc = long_line.repeat(2000); // ~1M bytes
    assert!(doc.len() > MAX_DIAG_BYTES);
    let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
    // Should be capped (either lines or bytes) plus truncation hint
    assert!(diags.len() <= MAX_DIAG_LINES + 1);
    assert!(!diags.is_empty());
    // Ensure first diags preserved
    assert_eq!(diags[0].range.start.line, 0);
}

#[test]
fn test_large_doc_truncation_preserves_first_n() {
    let data = synth();
    // First 5 lines are errors, then 5000 more errors beyond cap
    let mut doc = String::new();
    for i in 0..5 {
        doc.push_str(&format!("/unknown{}/menu add x=1\n", i));
    }
    doc.push_str(&"/unknown/menu add x=1\n".repeat(5000));
    let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
    assert!(diags.len() <= 3001);
    // First 5 should be present
    for i in 0..5 {
        let needle = format!("/unknown{}/menu", i);
        assert!(
            diags.iter().any(|d| d.message.contains(&needle)),
            "missing {needle}"
        );
    }
}

#[test]
fn test_large_doc_bytes_truncation_preserves_first() {
    let data = synth();
    let first = "/unknown/first add x=1\n";
    let tail = "/unknown/tail add x=1\n".repeat(50_000); // huge
    let doc = format!("{}{}", first, tail);
    assert!(doc.len() > MAX_DIAG_BYTES);
    let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
    assert!(!diags.is_empty());
    assert!(diags.iter().any(|d| d.message.contains("/unknown/first")));
}

// ── Incremental edits simulation ───────────────────────────────────

#[test]
fn test_incremental_fix_removes_diag() {
    let data = synth();
    let before = "/ip/address add comment=hi"; // missing required
    let after = "/ip/address add address=1.1.1.1 interface=ether1";
    assert!(
        compute_diagnostics(&data, before, "file:///a.rsc")
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
    assert!(
        !compute_diagnostics(&data, after, "file:///a.rsc")
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
}

#[test]
fn test_incremental_introduces_duplicate() {
    let data = synth();
    let before = "/ip/address add address=1.1.1.1 interface=ether1";
    let after = "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2";
    assert!(
        !compute_diagnostics(&data, before, "file:///a.rsc")
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property"))
    );
    assert!(
        compute_diagnostics(&data, after, "file:///a.rsc")
            .iter()
            .any(|d| d.code.as_deref() == Some("duplicate-property"))
    );
}

#[test]
fn test_unknown_menu_does_not_cascade_property_errors() {
    let data = synth();
    // Unknown menu should not also emit unknown-property for same line
    let doc = "/unknown/menu add bogus=1";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-property")),
        "should not cascade"
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
}
