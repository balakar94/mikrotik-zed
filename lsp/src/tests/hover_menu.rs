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
    assert!(
        h.contents.value.contains("Source: published reference"),
        "menu card carries its source, got: {}",
        h.contents.value
    );
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
    let footers = value
        .matches("(+3 more — type Space after verb to list)")
        .count();
    assert_eq!(footers, 2, "one footer per capped section, got {footers}");
    assert!(
        value.contains("Source: published reference"),
        "menu card carries its source, got: {value}"
    );
}
#[test]
fn test_hover_menu_arg_shows_description() {
    let data = synthetic_data();
    let h = hover_at(&data, "/ip/address", 2).expect("menu hover");
    let value = &h.contents.value;
    assert!(
        value.contains("- **address** `ipPrefix` — IP address"),
        "argument with docs must show its description, got: {value}"
    );
    assert_eq!(h.contents.kind, "markdown");
}
#[test]
fn test_hover_menu_arg_without_description_fallback() {
    let data = synthetic_data();
    let h = hover_at(&data, "/ip/address", 2).expect("menu hover");
    let value = &h.contents.value;
    let line = value
        .lines()
        .find(|l| l.starts_with("- **interface**"))
        .expect("interface bullet must exist");
    assert_eq!(
        line, "- **interface** `iface_enum`",
        "argument without docs keeps the bare fallback, got: {line}"
    );
}
#[test]
fn test_hover_menu_arg_description_truncated_single_line() {
    let long = "a".repeat(200);
    let toml = format!(
        "[[menus]]\npath = \"/demo/desc\"\ntype = \"Directory\"\n\
         [[menus.arguments]]\nname = \"token\"\ntype = \"string\"\n\
         description = \"{long} [x](http://example.com/a)\\nsecond line\"\n"
    );
    let data = MenuData::from_toml_str(&toml);
    let h = hover_at(&data, "/demo/desc", 2).expect("menu hover");
    let value = &h.contents.value;
    let line = value
        .lines()
        .find(|l| l.starts_with("- **token**"))
        .expect("token bullet must exist");
    assert!(
        line.contains("…"),
        "long description must be truncated, got: {line}"
    );
    assert!(
        !line.contains("http://"),
        "URLs must not survive sanitization, got: {line}"
    );
    let desc = line.split_once('—').expect("bullet must carry a suffix").1;
    assert!(
        desc.chars().count() <= 120 + 8,
        "per-arg description stays near the 120-char cap, got: {line}"
    );
}
#[test]
fn test_hover_menu_required_block_before_optional() {
    // Required entries render under their own block ahead of optional ones.
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/req"
type = "Directory"
[[menus.arguments]]
name = "zeta"
type = "string"
required = true
[[menus.arguments]]
name = "alpha"
type = "string"
"#,
    );
    let h = hover_at(&data, "/demo/req", 2).expect("menu hover");
    let value = &h.contents.value;
    let req_pos = value.find("**Required:**").expect("required block");
    let opt_pos = value.find("**Optional:**").expect("optional block");
    assert!(req_pos < opt_pos, "required block first, got: {value}");
    assert!(value.contains("- **zeta** `string` (required)"));
    assert!(value.contains("- **alpha** `string`"));
    assert!(value.contains("Source: published reference"));
}
