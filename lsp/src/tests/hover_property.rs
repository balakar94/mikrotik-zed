// Hover — property.
use super::hover_fixtures::*;

#[test]
fn test_hover_property_name() {
    let data = synthetic_data();
    // Full doc is a single line command where property "address" appears
    let line = "/ip/address add address=1.1.1.1";
    // Position of second "address" (property name)
    let prop_start = line.find("add ").unwrap() + 4; // start of "address=..."
    let h = compute_hover(&data, line, prop_start + 2, line, 0).expect("should hover property");
    assert!(h.contents.value.contains("**address**"));
    assert!(h.contents.value.contains("ipPrefix"));
}
#[test]
fn test_hover_property_with_empty_type() {
    let data = synthetic_data();
    let line = "/ip/address add no-type-prop=value";
    let prop_start = line.find("no-type-prop").unwrap();
    let h =
        compute_hover(&data, line, prop_start + 1, line, 0).expect("should hover empty-type prop");
    assert!(h.contents.value.contains("no-type-prop"));
    assert!(h.contents.value.contains("any"));
}
#[test]
fn test_hover_property_wrong_menu_returns_none() {
    let data = synth();
    // chain is not a property of /ip/address
    let line = "/ip/address add chain=input";
    let pos = line.find("chain").unwrap() + 1;
    assert!(hover_at(&data, line, pos).is_none());
}
#[test]
fn test_hover_property_multiline() {
    let data = synthetic_data();
    // RouterOS allows properties on next line (continuation)
    let doc = "/ip/address add\naddress=1.1.1.1";
    let lines: Vec<&str> = doc.lines().collect();
    let line2 = lines[1]; // "address=1.1.1.1"
    // Cursor_line = 1, character inside "address"
    let h = compute_hover(&data, line2, 2, doc, 1).expect("multiline property hover should work");
    assert!(h.contents.value.contains("**address**"));
}
#[test]
fn test_hover_property_includes_description_text() {
    let data = synthetic_data();
    let line = "/ip/address add address=1.1.1.1";
    let prop_pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, prop_pos, line, 0).expect("property hover");
    assert!(h.contents.value.contains("**address**"));
    assert!(h.contents.value.contains("Type: `ipPrefix`"));
    assert!(
        h.contents.value.contains("IP address"),
        "hover must include the description text, got: {}",
        h.contents.value
    );
}
#[test]
fn test_hover_property_without_description_has_no_trailing_gap() {
    // /ip/address interface has no description in this fixture.
    let data = synthetic_data();
    let line = "/ip/address add interface=ether1";
    let prop_pos = line.find("interface").unwrap();
    let h = compute_hover(&data, line, prop_pos, line, 0).expect("property hover");
    // Type line present with humanized gloss, context line appended,
    // but no description section.
    assert!(h.contents.value.contains("Type: `iface_enum`"));
    assert!(h.contents.value.contains("in `/ip/address add`"));
    assert!(
        !h.contents.value.contains("Description:"),
        "no description section, got {:?}",
        h.contents.value
    );
}
#[test]
fn test_hover_property_shows_embedded_enum_values() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/enum"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (on | of..."
enum_values = ["on", "off", "auto"]
"#,
    );
    let line = "/demo/enum set mode=on";
    let pos = line.find("mode").unwrap() + 1;
    let h = compute_hover(&data, line, pos, line, 0).expect("enum property hover");
    assert!(
        h.contents.value.contains("Values: on | off | auto"),
        "complete embedded members shown even with truncated display type: {}",
        h.contents.value
    );
}
#[test]
fn test_hover_real_property() {
    let data = MenuData::load();
    let line = "/ip/address add address=1.1.1.1";
    let _pos = line.find("address").unwrap() + 2; // first "address" is inside path, but word is "/ip/address" there
    // Use second occurrence: the property name after "add "
    let prop_pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, prop_pos, line, 0).expect("real property hover");
    assert!(h.contents.value.contains("address"));
}
#[test]
fn test_hover_property_shows_type() {
    let data = synth();
    let line = "/ip/address add address=1.1.1.1";
    let pos = line.rfind("address=").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("property hover");
    assert!(h.contents.value.contains("**address**"));
    assert!(h.contents.value.contains("ipPrefix"));
    assert_eq!(h.contents.kind, "markdown");
}
#[test]
fn test_hover_property_shows_enum_type() {
    let data = synth();
    let line = "/ip/firewall/filter add chain=input";
    let pos = line.find("chain").unwrap() + 2;
    let h = compute_hover(&data, line, pos, line, 0).expect("enum prop hover");
    assert!(h.contents.value.contains("**chain**"));
    assert!(h.contents.value.contains("enum"));
}
#[test]
fn test_hover_property_for_each_arg_type() {
    let data = synth();
    let cases = [
        ("/ip/address add address=1.1.1.1/24", "address", "ipPrefix"),
        (
            "/ip/address add interface=ether1",
            "interface",
            "iface_enum",
        ),
        ("/ip/firewall/filter add chain=input", "chain", "enum"),
    ];
    for (line, prop, typ_substr) in cases {
        let pos = line.find(prop).unwrap() + 1;
        // Need to ensure we hover over property name, not path: use second occurrence if line
        // contains "/ip/address"
        let doc = line;
        let prop_pos = if doc.matches(prop).count() > 1 {
            doc.rfind(&format!("{}=", prop)).unwrap() + 1
        } else {
            pos
        };
        let h = compute_hover(&data, doc, prop_pos, doc, 0).expect("prop hover");
        assert!(h.contents.value.contains(prop));
        assert!(
            h.contents.value.contains(typ_substr),
            "expected {typ_substr} in {}",
            h.contents.value
        );
    }
}
#[test]
fn test_hover_on_equals_sign_returns_none_or_property() {
    let data = synth();
    let line = "/ip/address add address=1.1.1.1";
    let eq_pos = line.find('=').unwrap();
    // Word extraction at '=': find_word_start looks backwards, includes "address", word_end stops
    // at "="
    // So hovering at "=" will extract "address" -> should hover property
    let h = hover_at(&data, line, eq_pos);
    // Could be property hover or None depending on word extraction; either is acceptable if not
    // panicking
    let _ = h;
    // Ensure no panic and deterministic
    assert!(
        hover_at(&data, line, eq_pos).is_none()
            || hover_at(&data, line, eq_pos)
                .unwrap()
                .contents
                .value
                .contains("address")
    );
}
#[test]
fn test_hover_multiline_property_still_works() {
    let data = synth();
    let doc = "/ip/address add\ninterface=ether1";
    let lines: Vec<&str> = doc.lines().collect();
    let l1 = lines[1];
    let h = compute_hover(&data, l1, 2, doc, 1).expect("multiline");
    assert!(h.contents.value.contains("interface"));
}
#[test]
fn test_hover_real_data_menu_and_property() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter";
    let h = hover_at(&data, line, 5).expect("real menu");
    assert!(h.contents.value.contains("**Type:**"));
    let line2 = "/ip/address add address=1.1.1.1";
    let pos = line2.rfind("address=").unwrap() + 1;
    let h2 = compute_hover(&data, line2, pos, line2, 0).expect("real prop");
    assert!(h2.contents.value.contains("address"));
}
