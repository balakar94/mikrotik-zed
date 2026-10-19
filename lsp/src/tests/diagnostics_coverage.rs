// ── Coverage-driven severity for unknown properties ──────────────────────

use crate::diagnostics::{compute_diagnostics, severity};
use crate::menus::MenuData;

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
"#
    ))
}

#[test]
fn incomplete_property_reference_downgrades_only_unknown_property_checks() {
    for coverage in ["reference", "partial", "unknown", "brand-new-marker"] {
        let data = fixture(coverage);
        let diagnostics = compute_diagnostics(
            &data,
            "/quality/items set 0 typo=yes\n:put \"unclosed",
            "file:///quality.rsc",
        );
        let property = diagnostics
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-property"))
            .unwrap();
        assert_eq!(
            property.severity,
            Some(if coverage == "reference" {
                severity::WARNING
            } else {
                severity::HINT
            }),
            "coverage {coverage} decides the unknown-property severity"
        );
        assert_eq!(
            property.message.contains("incomplete"),
            coverage != "reference"
        );
        assert!(diagnostics.iter().any(|d| {
            d.code.as_deref() == Some("unclosed-quote") && d.severity == Some(severity::ERROR)
        }));
    }
}
