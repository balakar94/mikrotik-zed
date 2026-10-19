// ── Dataset coverage vocabulary and unset capability ─────────────────────

use crate::diagnostics::{compute_diagnostics, severity};
use crate::menus::{Coverage, MenuData, parse_enum_values};

fn fixture(coverage: &str) -> MenuData {
    MenuData::from_toml_str(&format!(
        r#"
[[menus]]
path = "/quality/items"
type = "Directory"
coverage = "{coverage}"
source_url = "https://manual.mikrotik.com/docs/cli-reference/quality/items.md"
[[menus.arguments]]
name = "fixed"
type = "string"
unset_known = true
[[menus.arguments]]
name = "unknown"
type = "string"
unset_known = false
[[menus.arguments]]
name = "editable"
type = "string"
unset = true
unset_known = true
[[menus.arguments]]
name = "choice"
type = "enum (one | two | ...)"
unset_known = true
[[menus.read_only]]
name = "bytes"
type = "num"
description = "Observed traffic."
"#
    ))
}

#[test]
fn published_coverage_markers_round_trip_to_the_loader_vocabulary() {
    for (marker, expected) in [
        ("reference", Coverage::Reference),
        ("partial", Coverage::Partial),
        ("unknown", Coverage::Unknown),
    ] {
        let data = fixture(marker);
        assert_eq!(
            data.menu_by_path.get("/quality/items").unwrap().coverage,
            expected
        );
    }
}

#[test]
fn unrecognized_coverage_marker_stays_incomplete_and_is_not_trusted() {
    let data = fixture("curated-ish");
    let menu = data.menu_by_path.get("/quality/items").unwrap();
    assert!(!menu.coverage.is_complete());
    assert_eq!(
        menu.coverage,
        Coverage::Unrecognized("curated-ish".to_string())
    );
    let diagnostics = compute_diagnostics(
        &data,
        "/quality/items set 0 typo=yes",
        "file:///quality.rsc",
    );
    let property = diagnostics
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-property"))
        .unwrap();
    assert_eq!(property.severity, Some(severity::HINT));
    assert!(property.message.contains("incomplete"));
}

#[test]
fn unknown_unset_support_does_not_claim_the_property_is_fixed() {
    let data = fixture("reference");
    for target in ["unknown", "value-name=unknown"] {
        let document = format!("/quality/items unset 0 {target}");
        assert!(
            !compute_diagnostics(&data, &document, "file:///quality.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("non-unsettable-property")),
            "{target} has unknown support and must not be reported as fixed"
        );
    }
    for target in ["fixed", "value-name=fixed"] {
        let document = format!("/quality/items unset 0 {target}");
        assert!(
            compute_diagnostics(&data, &document, "file:///quality.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("non-unsettable-property")),
            "{target} is known non-unsettable and must be reported"
        );
    }
}

#[test]
fn truncated_enum_cannot_reject_an_unlisted_value() {
    let data = fixture("reference");
    let menu = data.menu_by_path.get("/quality/items").unwrap();
    let arg = menu.arguments.iter().find(|a| a.name == "choice").unwrap();
    assert!(arg.enum_members().is_empty());
    assert!(
        !compute_diagnostics(
            &data,
            "/quality/items set 0 choice=three",
            "file:///quality.rsc"
        )
        .iter()
        .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
    );
}

#[test]
fn enum_member_cases_mirror_the_generator() {
    // Shared case table with `scripts/extract_commands.py::extract_enum_values`
    // (also pinned in tests/test_extraction_quality.py). The fallback reads the
    // capped display type, so it accepts exactly one parenthesized body: a
    // bit-map suffix or a cut body is left to `enum_values`.
    assert_eq!(parse_enum_values("enum (none)"), vec!["none".to_string()]);
    assert_eq!(
        parse_enum_values("enum (one | two)"),
        vec!["one".to_string(), "two".to_string()]
    );
    assert!(parse_enum_values("enum (one | two | ...)").is_empty());
    assert!(parse_enum_values("enum (one | two | …)").is_empty());
    assert!(parse_enum_values("enum ()").is_empty());
    assert!(parse_enum_values("enum").is_empty());
    assert!(parse_enum_values("ubit (1Mbps, 2Mbps)").is_empty());
    // Deliberate divergence, safe direction: raw-type parsing (generator)
    // recovers the members, display-type parsing (here) refuses.
    assert!(parse_enum_values("enum (a | b) { a:0, b:1 }").is_empty());
    assert!(parse_enum_values("enum (a | b | c").is_empty());
}

#[test]
fn embedded_dataset_keeps_narrative_read_only_properties() {
    let data = MenuData::load();
    let app = data.menu_by_path.get("/app").unwrap();
    assert!(app.read_only.iter().any(|arg| arg.name == "app-size"));
    assert!(!app.arguments.iter().any(|arg| arg.name == "app-store-urls"));
}
