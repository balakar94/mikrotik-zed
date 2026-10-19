// ── Hover evidence links and sanitization ────────────────────────────────

use crate::hover::compute_hover;
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
[[menus.arguments]]
name = "unknown"
type = "string"
unset_known = false
source_kind = "narrative"
source_url = "https://manual.mikrotik.com/docs/quality.md"
[[menus.read_only]]
name = "bytes"
type = "num"
description = "Observed traffic."
"#
    ))
}

#[test]
fn read_only_hover_explains_role_and_links_to_inherited_evidence() {
    let data = fixture("partial");
    let line = "/quality/items get 0 bytes";
    let hover = compute_hover(&data, line, line.find("bytes").unwrap() + 1, line, 0).unwrap();
    assert!(hover.contents.value.contains("Read-only output column"));
    assert!(hover.contents.value.contains("Observed traffic."));
    assert!(hover.contents.value.contains(
        "[MikroTik reference](https://manual.mikrotik.com/docs/cli-reference/quality/items/)"
    ));
}

#[test]
fn narrative_property_keeps_its_own_source_in_hover() {
    let data = fixture("reference");
    let line = "/quality/items set 0 unknown=value";
    let hover = compute_hover(&data, line, line.find("unknown").unwrap() + 1, line, 0).unwrap();
    assert!(hover.contents.value.contains("narrative documentation"));
    assert!(
        hover
            .contents
            .value
            .contains("https://manual.mikrotik.com/docs/quality/")
    );
    assert!(
        !hover
            .contents
            .value
            .contains("/cli-reference/quality/items.md")
    );
}

#[test]
fn source_links_cannot_inject_markdown_or_external_hosts() {
    let mut data = fixture("reference");
    let menu = data.menu_by_path.get_mut("/quality/items").unwrap();
    menu.arguments[0].source.source_url = "https://manual.mikrotik.com.evil.test/docs/trap".into();
    menu.arguments[0].source.description_source_url =
        "https://manual.mikrotik.com/docs/x)[trap](https://evil.test)".into();
    let line = "/quality/items set 0 fixed=value";
    let hover = compute_hover(&data, line, line.find("fixed").unwrap() + 1, line, 0).unwrap();
    assert!(!hover.contents.value.contains("evil.test"));
}
