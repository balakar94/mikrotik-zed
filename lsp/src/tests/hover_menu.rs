// Hover — menu.
use super::hover_fixtures::*;

#[test]
fn test_find_word_start_mid_word() {
    let line = "/ip/address";
    // Cursor inside "address" (after "/ip/")
    assert_eq!(find_word_start(line, 5), 0);
    assert_eq!(find_word_start(line, 7), 0);
}
#[test]
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
#[test]
fn test_find_word_end_includes_slash_dash_underscore() {
    let line = "/ip/firewall/filter";
    let start = find_word_start(line, 5);
    let end = find_word_end(line, 5);
    assert_eq!(&line[start..end], "/ip/firewall/filter");
}
#[test]
fn test_find_word_with_dash_and_underscore() {
    let line = "my-prop_name";
    assert_eq!(find_word_start(line, 5), 0);
    assert_eq!(find_word_end(line, 5), line.len());
}
#[test]
fn test_find_word_clamps_beyond_len() {
    let line = "/ip/address";
    let start = find_word_start(line, 100);
    let end = find_word_end(line, 100);
    // Beyond len should clamp and return the trailing word
    assert_eq!(&line[start..end], "/ip/address");
}
#[test]
fn test_find_word_empty_line() {
    let line = "";
    assert_eq!(find_word_start(line, 0), 0);
    assert_eq!(find_word_end(line, 0), 0);
}
#[test]
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
#[test]
fn test_hover_menu_path_partial_inside() {
    let data = synthetic_data();
    let line = "/ip/address print";
    // Hover at position inside "/ip/address" (character 5)
    let h =
        compute_hover(&data, line, 5, line, 0).expect("should hover menu when cursor inside path");
    assert!(h.contents.value.contains("/ip/address"));
}
#[test]
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
#[test]
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
#[test]
fn test_hover_real_menu() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter";
    let h = hover_at(&data, line, 5).expect("real /ip/firewall/filter should hover");
    assert!(h.contents.value.contains("/ip/firewall/filter"));
}
#[test]
fn test_hover_menu_shows_type_args_flags() {
    let data = synth();
    let line = "/ip/address";
    let h = hover_at(&data, line, 4).expect("menu hover");
    assert!(h.contents.value.contains("### /ip/address"));
    assert!(h.contents.value.contains("**Type:** Directory"));
    assert!(h.contents.value.contains("**Arguments:**"));
    assert!(h.contents.value.contains("- **address** `ipPrefix`"));
    assert!(h.contents.value.contains("- **interface** `iface_enum`"));
    assert!(h.contents.value.contains("**Flags:**"));
    assert!(h.contents.value.contains("X — disabled"));
    // Flag D with empty description should still appear
    assert!(h.contents.value.contains("D —"));
    assert_eq!(h.contents.kind, "markdown");
}
#[test]
fn test_hover_menu_shows_correct_type_for_custom() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/tool/ping"
type = "Command"
"#,
    );
    let h = hover_at(&data, "/tool/ping", 2).unwrap();
    assert!(h.contents.value.contains("**Type:** Command"));
}
#[test]
fn test_hover_menu_without_args_no_arguments_section() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/empty/menu"
type = "Directory"
"#,
    );
    let h = hover_at(&data, "/empty/menu", 2).unwrap();
    assert!(!h.contents.value.contains("Arguments:"));
    assert!(!h.contents.value.contains("Flags:"));
}
#[test]
fn test_hover_menu_shows_read_only() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/ro"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus.read_only]]
name = "creation-time"
type = "string"
description = "when created"
[[menus.read_only]]
name = "dynamic-id"
type = "string"
description = ""
"#,
    );
    let h = hover_at(&data, "/demo/ro", 2).expect("menu hover with read-only");
    assert!(h.contents.value.contains("**Arguments:**"));
    assert!(h.contents.value.contains("**Flags:**"));
    assert!(h.contents.value.contains("**Read-only:**"));
    assert!(h.contents.value.contains("creation-time — when created"));
    // Empty description fallback: "name — " (same as flags)
    assert!(h.contents.value.contains("dynamic-id —"));
    assert_eq!(h.contents.kind, "markdown");
}
#[test]
fn test_hover_menu_without_read_only_no_section() {
    let data = synth();
    // synth has no read_only, so section must be absent
    let h = hover_at(&data, "/ip/address", 2).expect("menu hover");
    assert!(!h.contents.value.contains("Read-only:"));
}

#[test]
fn test_hover_caps_flags_and_read_only() {
    // MAX_HOVER_PROPERTIES is applied consistently to flags and read_only
    // too: the surplus folds into a per-section footer, never rendered.
    let mut toml = String::from("[[menus]]\npath = \"/demo/many\"\ntype = \"Directory\"\n");
    for i in 0..15 {
        toml.push_str(&format!(
            "[[menus.flags]]\nname = \"f{i}\"\ndescription = \"flag {i}\"\n"
        ));
    }
    for i in 0..15 {
        toml.push_str(&format!(
            "[[menus.read_only]]\nname = \"r{i}\"\ndescription = \"ro {i}\"\n"
        ));
    }
    let data = MenuData::from_toml_str(&toml);
    let h = hover_at(&data, "/demo/many", 2).expect("menu hover");
    let value = &h.contents.value;
    assert!(value.contains("f11"), "12th flag must be shown");
    assert!(!value.contains("f12"), "13th flag must be omitted");
    assert!(value.contains("r11"), "12th read-only must be shown");
    assert!(!value.contains("r12"), "13th read-only must be omitted");
    let footers = value.matches("(+3 more — see completion)").count();
    assert_eq!(footers, 2, "one footer per capped section, got {footers}");
}
