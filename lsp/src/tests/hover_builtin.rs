// Hover — builtin.
use super::hover_fixtures::*;

#[test]
fn test_hover_flag() {
    let data = synthetic_data();
    let line = "/ip/address add X";
    let flag_pos = line.find('X').unwrap();
    let h = compute_hover(&data, line, flag_pos, line, 0).expect("should hover flag X");
    assert!(h.contents.value.contains("**X**"));
    assert!(h.contents.value.contains("disabled"));
}
#[test]
fn test_hover_flag_empty_description() {
    let data = synthetic_data();
    let line = "/ip/address add D";
    let flag_pos = line.find('D').unwrap();
    let h = compute_hover(&data, line, flag_pos, line, 0).expect("should hover flag D");
    assert!(h.contents.value.contains("**D**"));
}
#[test]
fn test_hover_colon_put() {
    let data = synthetic_data();
    let line = ":put hello";
    let h = hover_at(&data, line, 2).expect(":put should hover");
    assert!(h.contents.value.contains(":put"));
    assert!(h.contents.value.contains("console"));
}
#[test]
fn test_hover_colon_foreach() {
    let data = synthetic_data();
    let line = ":foreach i in=[find] do={ :put $i }";
    let pos = line.find("foreach").unwrap() + 1;
    let h = hover_at(&data, line, pos).expect(":foreach should hover");
    assert!(h.contents.value.contains(":foreach"));
}
#[test]
fn test_hover_colon_unknown_fallback() {
    let data = synthetic_data();
    let line = ":frobnicate 1s";
    let pos = line.find("frobnicate").unwrap() + 1;
    let h = hover_at(&data, line, pos).expect("unknown :keyword gets fallback");
    assert!(h.contents.value.contains(":frobnicate"));
    assert!(h.contents.value.contains("Script command"));
}
#[test]
fn test_hover_flag_shows_description() {
    let data = synth();
    let line = "/ip/address add X";
    let pos = line.find('X').unwrap();
    let h = hover_at(&data, line, pos).unwrap();
    assert!(h.contents.value.contains("**X**"));
    assert!(h.contents.value.contains("disabled"));
}

#[test]
fn test_hover_property_sanitizes_upstream_markdown_links() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "See [docs](http://evil/x) ![img](http://e/i.png) for details"
"#,
    );
    let line = "/ip/address add address=1.1.1.1";
    let pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("property hover");
    assert!(
        !h.contents.value.contains("]("),
        "links rewritten, got {:?}",
        h.contents.value
    );
}

#[test]
fn test_hover_property_collapses_control_newlines() {
    let data = MenuData::from_toml_str(
        "[[menus]]\npath = \"/ip/address\"\ntype = \"Directory\"\n[[menus.arguments]]\nname = \"address\"\ntype = \"ipPrefix\"\ndescription = \"Bell here \\u0007 done\"\n",
    );
    let line = "/ip/address add address=1.1.1.1";
    let pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("property hover");
    assert!(
        !h.contents.value.contains('\u{7}'),
        "controls stripped, got {:?}",
        h.contents.value
    );
}

#[test]
fn test_hover_property_strips_multiline_script_payload() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "Addr <script>alert(1)</script> value"
"#,
    );
    let line = "/ip/address add address=1.1.1.1";
    let pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("property hover");
    assert!(
        !h.contents.value.contains("<script"),
        "tags stripped, got {:?}",
        h.contents.value
    );
}

#[test]
fn test_hover_property_rewrites_long_link_text() {
    let long = "x".repeat(600);
    let toml = format!(
        "[[menus]]\npath = \"/ip/address\"\ntype = \"Directory\"\n[[menus.arguments]]\nname = \"address\"\ntype = \"ipPrefix\"\ndescription = \"See [{long}](http://evil/x) ok\"\n"
    );
    let data = MenuData::from_toml_str(&toml);
    let line = "/ip/address add address=1.1.1.1";
    let pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("property hover");
    assert!(
        !h.contents.value.contains("http://evil"),
        "link target dropped, got {:?}",
        h.contents.value
    );
}
