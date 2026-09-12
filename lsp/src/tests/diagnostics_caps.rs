// Diagnostic caps, incremental edits and enum edge cases.
// Copied (not moved) from `lsp/src/diagnostics.rs`; the original block is
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
fn test_single_line_semantic_count_capped_with_hint() {
    // Regression: one logical line with thousands of distinct unknown
    // keys must not yield one Diagnostic per key. The semantic loop is
    // bounded by MAX_DIAGNOSTICS; the count-only truncation still emits
    // the "truncated" hint. The input stays under MAX_DIAG_BYTES/LINES
    // so only the count cap applies (no syntax findings on this line).
    let data = synthetic_data();
    let mut doc = String::from("/ip/address add address=1.1.1.1/24 interface=ether1");
    for i in 0..(MAX_DIAGNOSTICS + 1000) {
        doc.push_str(&format!(" unknownkey{i}=1"));
    }
    assert!(
        doc.len() < MAX_DIAG_BYTES,
        "test input must stay under the byte cap, got {} bytes",
        doc.len()
    );
    let diags = compute_diagnostics(&data, &doc, "file:///test.rsc");
    assert!(
        diags.iter().any(|d| d.code.as_deref() == Some("truncated")),
        "count truncation must emit a hint, got {} diagnostics",
        diags.len()
    );
    let non_hint = diags
        .iter()
        .filter(|d| d.code.as_deref() != Some("truncated"))
        .count();
    assert!(
        non_hint <= MAX_DIAGNOSTICS,
        "semantic findings must be capped at {MAX_DIAGNOSTICS}, got {non_hint}"
    );
    assert!(
        diags.len() <= MAX_DIAGNOSTICS + 1 + MAX_SYNTAX_DIAGNOSTICS,
        "total stays bounded (semantic + hint + syntax), got {}",
        diags.len()
    );
}

#[test]
fn test_semantic_cap_enforced_during_accumulation() {
    // The semantic loop stops at MAX_DIAGNOSTICS as diagnostics are built
    // (not merely truncated afterwards) and still emits the count footer.
    let data = synthetic_data();
    let mut doc = String::from("/ip/address add address=1.1.1.1/24 interface=ether1");
    for i in 0..(MAX_DIAGNOSTICS + 500) {
        doc.push_str(&format!(" unknownkey{i}=1"));
    }
    let diags = compute_diagnostics(&data, &doc, "file:///test.rsc");
    let semantic: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() != Some("truncated"))
        .collect();
    assert_eq!(
        semantic.len(),
        MAX_DIAGNOSTICS,
        "accumulation must stop exactly at the cap"
    );
    assert!(
        diags.iter().any(|d| d.code.as_deref() == Some("truncated")),
        "count truncation must still emit the footer"
    );
}

#[test]
fn test_incremental_edit_simulation() {
    let data = synthetic_data();
    // Simulate incremental edits: initial doc has error, then fix
    let doc1 = "/ip/address add comment=hi"; // missing required
    let diags1 = compute_diagnostics(&data, doc1, "file:///test.rsc");
    assert!(
        diags1
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );

    let doc2 = "/ip/address add address=1.1.1.1/24 interface=ether1"; // fixed
    let diags2 = compute_diagnostics(&data, doc2, "file:///test.rsc");
    assert!(
        !diags2
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
}

#[test]
fn test_implicit_parent_not_unknown() {
    let data = synthetic_data();
    // /ip/firewall is implicit parent (no direct entry but has children), should not be unknown
    // synthetic data has /ip/firewall/filter, so /ip/firewall should be considered known via
    // child_names
    let doc = "/ip/firewall print";
    let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "implicit parent /ip/firewall should not be unknown, got {:?}",
        diags
    );
}

// ── Rule 5 via embedded enum_values ──────────────────────────────────────

fn truncated_display_data() -> MenuData {
    // Mirrors real generated data: display type truncated by the
    // generator's 100-char cap, complete members in enum_values.
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/interface/wireless"
type = "Directory"
[[menus.arguments]]
name = "band"
type = "enum (2ghz-b | 2ghz-onlyg | 2ghz-b/g | 5ghz-a | 5ghz-onlyn | 5ghz-a/n | 2ghz-on..."
enum_values = ["2ghz-b", "5ghz-a", "5ghz-onlyac"]
[[menus.arguments]]
name = "legacy-no-values"
type = "enum (a | b"
"#,
    )
}

#[test]
fn test_invalid_enum_hint_fires_via_enum_values_despite_truncated_display() {
    let data = truncated_display_data();
    let diags = compute_diagnostics(&data, "/interface/wireless set band=bogus", "file:///t.rsc");
    let hint = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("hint must fire using embedded enum_values");
    assert!(hint.message.contains("bogus"));
    assert!(hint.message.contains("2ghz-b | 5ghz-a | 5ghz-onlyac"));
}

#[test]
fn test_valid_embedded_enum_value_no_hint() {
    let data = truncated_display_data();
    for good in ["2ghz-b", "5ghz-a", "5ghz-onlyac"] {
        let doc = format!("/interface/wireless set band={good}");
        let diags = compute_diagnostics(&data, &doc, "file:///t.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
            "{good} is a documented member — no hint, got {diags:?}"
        );
    }
}

#[test]
fn test_truncated_type_without_values_stays_silent() {
    // No enum_values AND unparsable display string → no hint (never guess).
    let data = truncated_display_data();
    let diags = compute_diagnostics(
        &data,
        "/interface/wireless set legacy-no-values=bogus",
        "file:///t.rsc",
    );
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn test_real_data_action_enum_hint_fires() {
    // End-to-end on the regenerated table: action's display type is
    // truncated, but its embedded enum_values make the rule live again.
    let data = MenuData::load();
    let doc = "/ip/firewall/filter add chain=input action=frobnicate";
    let diags = compute_diagnostics(&data, doc, "file:///real.rsc");
    let hint = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
        .expect("invalid-enum-value must fire on real data now");
    assert!(hint.message.contains("frobnicate"));
    assert!(hint.message.contains("accept"));

    // A documented value stays clean.
    let ok = compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=input action=accept",
        "file:///real.rsc",
    );
    assert!(
        !ok.iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn test_comma_separated_enum_list_valid() {
    let data = synthetic_data();
    // Single valid member in list should be considered valid (lenient: any matches)
    let doc = "/ip/firewall/filter add chain=input,forward action=accept";
    let diags = compute_diagnostics(&data, doc, "file:///t.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "comma list with at least one valid member must not hint, got {diags:?}"
    );

    // Variants with spaces and mixed valid/invalid: still lenient (any valid => no hint)
    let doc2 = "/ip/firewall/filter add chain=input , forward action=accept,drop";
    let diags2 = compute_diagnostics(&data, doc2, "file:///t.rsc");
    assert!(
        !diags2
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "comma list with spaces and valid members must not hint, got {diags2:?}"
    );

    // No member matches -> must hint
    let doc3 = "/ip/firewall/filter add chain=bogus,also-bogus action=accept";
    let diags3 = compute_diagnostics(&data, doc3, "file:///t.rsc");
    assert!(
        diags3
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "comma list with zero valid members must hint, got {diags3:?}"
    );

    // Empty value stays silent (existing early return)
    let doc4 = "/ip/firewall/filter add chain= action=accept";
    let diags4 = compute_diagnostics(&data, doc4, "file:///t.rsc");
    assert!(
        !diags4
            .iter()
            .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
        "empty value must not hint"
    );
}
