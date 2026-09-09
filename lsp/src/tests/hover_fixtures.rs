//! Hover — shared fixtures (both TOML variants + hover_at).
pub(crate) use crate::hover::*;
pub(crate) use crate::menus::MenuData;
pub(crate) fn synthetic_data() -> MenuData {
    let toml_str = r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "IP address"

[[menus.arguments]]
name = "interface"
type = "iface_enum"

[[menus.arguments]]
name = "comment"
type = "string"

[[menus.arguments]]
name = "no-type-prop"
type = ""

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = ""

[[menus]]
path = "/ip/firewall/filter"
type = "Directory"

[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"

[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"

[[menus]]
path = "/interface/bridge"
type = "Directory"

[[menus]]
path = "/empty/menu"
type = "Directory"
"#;
    MenuData::from_toml_str(toml_str)
}

// ── Helpers for hover tests ───────────────────────────────────

pub(crate) fn hover_at(data: &MenuData, line: &str, character: usize) -> Option<Hover> {
    // Single-line doc helper
    compute_hover(data, line, character, line, 0)
}

// ── find_word_start / find_word_end ───────────────────────────

pub(crate) fn synth() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "IP address"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus.flags]]
name = "D"
description = ""
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
    )
}

// ── Menu path with type+args+flags ─────────────────────────────────

fn test_find_word_start_mid_word() {
    let line = "/ip/address";
    // Cursor inside "address" (after "/ip/")
    assert_eq!(find_word_start(line, 5), 0);
    assert_eq!(find_word_start(line, 7), 0);
}

fn test_find_word_start_at_boundary() {
    let line = "/ip/address add";
    // find_word_start looks backwards from pos, so at position 11 (space after "/ip/address")
    // it includes the preceding word "/ip/address" because bytes[10] is alphanumeric.
    // Expected to return 0 (start of menu path), not 11.
    assert_eq!(find_word_start(line, 11), 0);
    // At the space itself, word_end stays at 11 (space is not word char)
    assert_eq!(find_word_end(line, 11), 11);
    // Combined word extracted at space is "/ip/address"
    let start = find_word_start(line, 11);
    let end = find_word_end(line, 11);
    assert_eq!(&line[start..end], "/ip/address");
}

fn test_find_word_end_includes_slash_dash_underscore() {
    let line = "/ip/firewall/filter";
    let start = find_word_start(line, 5);
    let end = find_word_end(line, 5);
    assert_eq!(&line[start..end], "/ip/firewall/filter");
}

fn test_find_word_with_dash_and_underscore() {
    let line = "my-prop_name";
    assert_eq!(find_word_start(line, 5), 0);
    assert_eq!(find_word_end(line, 5), line.len());
}

fn test_find_word_clamps_beyond_len() {
    let line = "/ip/address";
    let start = find_word_start(line, 100);
    let end = find_word_end(line, 100);
    // Beyond len should clamp and return the trailing word
    assert_eq!(&line[start..end], "/ip/address");
}

fn test_find_word_empty_line() {
    let line = "";
    assert_eq!(find_word_start(line, 0), 0);
    assert_eq!(find_word_end(line, 0), 0);
}

// ── Menu hover ────────────────────────────────────────────────

fn test_hover_menu_path_full() {
    let data = synthetic_data();
    let line = "/ip/address";
    let h = hover_at(&data, line, 3).expect("should hover menu");
    assert!(h.contents.value.contains("### /ip/address"));
    assert!(h.contents.value.contains("**Type:** Directory"));
    assert!(h.contents.value.contains("Arguments:"));
    assert!(h.contents.value.contains("- **address** `ipPrefix`"));
    assert!(h.contents.value.contains("Flags:"));
    assert!(h.contents.value.contains("X — disabled"));
    assert_eq!(h.contents.kind, "markdown");
}

fn test_hover_menu_path_partial_inside() {
    let data = synthetic_data();
    let line = "/ip/address print";
    // Hover at position inside "/ip/address" (character 5)
    let h =
        compute_hover(&data, line, 5, line, 0).expect("should hover menu when cursor inside path");
    assert!(h.contents.value.contains("/ip/address"));
}

fn test_hover_menu_path_unknown_returns_none_or_verb() {
    let data = synthetic_data();
    let line = "/ip/unknown";
    // "/ip/unknown" not in menu_by_path, next checks property/verb -> unknown -> None
    let h = hover_at(&data, line, 4);
    // Could be None or verb check (not a verb), so None
    assert!(
        h.is_none(),
        "unknown menu should return None, got {:?}",
        h.map(|x| x.contents.value)
    );
}

fn test_hover_menu_without_args() {
    let data = synthetic_data();
    let line = "/empty/menu";
    let h = hover_at(&data, line, 2).expect("empty menu should still hover");
    assert!(h.contents.value.contains("### /empty/menu"));
    assert!(
        !h.contents.value.contains("Arguments:"),
        "should not contain Arguments section"
    );
}

fn test_hover_menu_type_fallback() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/test/menu"
type = ""
"#,
    );
    let line = "/test/menu";
    let h = hover_at(&data, line, 2).unwrap();
    assert!(
        h.contents.value.contains("**Type:** Directory"),
        "empty type should fallback to Directory"
    );
}

// ── Property hover ────────────────────────────────────────────

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

fn test_hover_property_with_empty_type() {
    let data = synthetic_data();
    let line = "/ip/address add no-type-prop=value";
    let prop_start = line.find("no-type-prop").unwrap();
    let h =
        compute_hover(&data, line, prop_start + 1, line, 0).expect("should hover empty-type prop");
    assert!(h.contents.value.contains("no-type-prop"));
    assert!(h.contents.value.contains("any"));
}

fn test_hover_property_wrong_menu_returns_none() {
    let data = synthetic_data();
    // "chain" is not a property of /ip/address, so hovering over "chain" there should be None
    let line = "/ip/address add chain=input";
    let prop_start = line.find("chain").unwrap();
    let h = compute_hover(&data, line, prop_start + 1, line, 0);
    // Not a property of /ip/address, not a verb, not a menu -> None
    assert!(h.is_none());
}

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

fn test_hover_property_without_description_has_no_trailing_gap() {
    // /ip/address interface has no description in this fixture.
    let data = synthetic_data();
    let line = "/ip/address add interface=ether1";
    let prop_pos = line.find("interface").unwrap();
    let h = compute_hover(&data, line, prop_pos, line, 0).expect("property hover");
    // Type line present with humanized gloss, plus the context line.
    assert!(h.contents.value.contains("Type: `iface_enum`"));
    assert!(
        h.contents.value.contains("interface name"),
        "iface_enum gloss must explain the raw type, got: {}",
        h.contents.value
    );
    assert!(
        h.contents.value.contains("in `/ip/address add`"),
        "property hover must name its command context, got: {}",
        h.contents.value
    );
}

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

// ── Flag hover ────────────────────────────────────────────────
