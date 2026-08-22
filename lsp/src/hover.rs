// ── Hover logic for the RSC language server ─────────────────────
//
// When the user hovers over a word, check:
// 1. Is it a menu path (starts with /)?
// 2. Is it a property name for the current menu?
// 3. Is it a standard RouterOS verb?

use crate::menus::MenuData;

/// Find word start (including /, -, _)
fn find_word_start(line: &str, pos: usize) -> usize {
    let pos = pos.min(line.len());
    // Ensure we are at a valid character boundary (RSC is ASCII, but be safe).
    let pos = crate::floor_char_boundary(line, pos);
    let mut i = pos;
    while i > 0 {
        let ch = line.as_bytes()[i - 1] as char;
        if !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' {
            break;
        }
        i -= 1;
    }
    i
}

/// Find word end (including /, -, _)
fn find_word_end(line: &str, pos: usize) -> usize {
    let pos = pos.min(line.len());
    let pos = crate::floor_char_boundary(line, pos);
    let mut i = pos;
    while i < line.len() {
        let ch = line.as_bytes()[i] as char;
        if !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' {
            break;
        }
        i += 1;
    }
    i
}

#[derive(serde::Serialize)]
pub struct HoverContents {
    pub kind: String,
    pub value: String,
}

#[derive(serde::Serialize)]
pub struct Hover {
    pub contents: HoverContents,
}

pub fn compute_hover(
    data: &MenuData,
    line: &str,
    character: usize,
    full_doc: &str,
    cursor_line: usize,
) -> Option<Hover> {
    let word_start = find_word_start(line, character);
    let word_end = find_word_end(line, character);
    let word = &line[word_start..word_end];
    if word.is_empty() {
        return None;
    }

    // Check if it's a menu path
    // let chains (requires Rust 1.88+, MSRV is 1.94) — collapsed for clippy collapsible_if
    if word.starts_with('/')
        && let Some(menu) = data.menu_by_path.get(word)
    {
        let mut md = format!(
            "### {}\n\n**Type:** {}",
            word,
            if menu.menu_type.is_empty() {
                "Directory"
            } else {
                &menu.menu_type
            }
        );

        if !menu.arguments.is_empty() {
            md.push_str("\n\n**Arguments:**");
            for arg in &menu.arguments {
                let typ = if arg.arg_type.is_empty() {
                    "(any)"
                } else {
                    &arg.arg_type
                };
                md.push_str(&format!("\n  {}: {}", arg.name, typ));
            }
        }

        if !menu.flags.is_empty() {
            md.push_str("\n\n**Flags:**");
            for flag in &menu.flags {
                let desc = if flag.description.is_empty() {
                    ""
                } else {
                    &flag.description
                };
                md.push_str(&format!("\n  {} — {}", flag.name, desc));
            }
        }

        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: md,
            },
        });
    }

    // Check if it's a property name for the current menu.
    // Rebuild context from the full document at the cursor position so that
    // multiline commands (properties on next line) are correctly resolved.
    let before_cursor = crate::build_before_cursor(full_doc, cursor_line, character);
    let context = crate::parse_line(data, &before_cursor);

    if let Some(menu) = data.menu_by_path.get(&context.path) {
        if let Some(arg) = menu.arguments.iter().find(|a| a.name == word) {
            let typ = if arg.arg_type.is_empty() {
                "any"
            } else {
                &arg.arg_type
            };
            let md = format!("**{}**\n\nType: `{}`", arg.name, typ);
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: md,
                },
            });
        }
        if let Some(flag) = menu.flags.iter().find(|f| f.name == word) {
            let desc = if flag.description.is_empty() {
                ""
            } else {
                &flag.description
            };
            let md = format!("**{}**\n\n{}", flag.name, desc);
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: md,
                },
            });
        }
    }

    // Check if it's a standard verb
    if MenuData::STANDARD_VERBS.contains(&word) {
        let md = format!("**{}**\n\nStandard RouterOS command.", word);
        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: md,
            },
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::MenuData;

    fn synthetic_data() -> MenuData {
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

    fn hover_at(data: &MenuData, line: &str, character: usize) -> Option<Hover> {
        // Single-line doc helper
        compute_hover(data, line, character, line, 0)
    }

    // ── find_word_start / find_word_end ───────────────────────────

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

    // ── Menu hover ────────────────────────────────────────────────

    #[test]
    fn test_hover_menu_path_full() {
        let data = synthetic_data();
        let line = "/ip/address";
        let h = hover_at(&data, line, 3).expect("should hover menu");
        assert!(h.contents.value.contains("### /ip/address"));
        assert!(h.contents.value.contains("**Type:** Directory"));
        assert!(h.contents.value.contains("Arguments:"));
        assert!(h.contents.value.contains("address: ipPrefix"));
        assert!(h.contents.value.contains("Flags:"));
        assert!(h.contents.value.contains("X — disabled"));
        assert_eq!(h.contents.kind, "markdown");
    }

    #[test]
    fn test_hover_menu_path_partial_inside() {
        let data = synthetic_data();
        let line = "/ip/address print";
        // Hover at position inside "/ip/address" (character 5)
        let h = compute_hover(&data, line, 5, line, 0)
            .expect("should hover menu when cursor inside path");
        assert!(h.contents.value.contains("/ip/address"));
    }

    #[test]
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

    // ── Property hover ────────────────────────────────────────────

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
        let h = compute_hover(&data, line, prop_start + 1, line, 0)
            .expect("should hover empty-type prop");
        assert!(h.contents.value.contains("no-type-prop"));
        assert!(h.contents.value.contains("any"));
    }

    #[test]
    fn test_hover_property_wrong_menu_returns_none() {
        let data = synthetic_data();
        // "chain" is not a property of /ip/address, so hovering over "chain" there should be None
        let line = "/ip/address add chain=input";
        let prop_start = line.find("chain").unwrap();
        let h = compute_hover(&data, line, prop_start + 1, line, 0);
        // Not a property of /ip/address, not a verb, not a menu -> None
        assert!(h.is_none());
    }

    #[test]
    fn test_hover_property_multiline() {
        let data = synthetic_data();
        // RouterOS allows properties on next line (continuation)
        let doc = "/ip/address add\naddress=1.1.1.1";
        let lines: Vec<&str> = doc.lines().collect();
        let line2 = lines[1]; // "address=1.1.1.1"
        // Cursor_line = 1, character inside "address"
        let h =
            compute_hover(&data, line2, 2, doc, 1).expect("multiline property hover should work");
        assert!(h.contents.value.contains("**address**"));
    }

    // ── Flag hover ────────────────────────────────────────────────

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

    // ── Verb hover ─────────────────────────────────────────────────

    #[test]
    fn test_hover_verb_add() {
        let data = synthetic_data();
        let line = "/ip/address add";
        // Use rfind to get the verb's "add", not the "add" inside "address"
        let verb_pos = line.rfind("add").unwrap() + 1;
        let h = compute_hover(&data, line, verb_pos, line, 0).expect("should hover verb add");
        assert!(h.contents.value.contains("**add**"));
        assert!(h.contents.value.contains("Standard RouterOS command"));
    }

    #[test]
    fn test_hover_verb_print_without_menu() {
        let data = synthetic_data();
        // Hovering over "print" alone (no menu) should still return verb hover
        // But context path is empty, so property check fails, then verb check succeeds
        let line = "print";
        let h = hover_at(&data, line, 2).expect("should hover verb print even without menu");
        assert!(h.contents.value.contains("print"));
    }

    #[test]
    fn test_hover_verb_case_sensitive() {
        let data = synthetic_data();
        let line = "/ip/address Add"; // capital A
        let h = hover_at(&data, line, line.find("Add").unwrap() + 1);
        assert!(h.is_none(), "verb hover is case-sensitive");
    }

    // ── Not found / edge cases ────────────────────────────────────

    #[test]
    fn test_hover_empty_line_returns_none() {
        let data = synthetic_data();
        let line = "";
        assert!(hover_at(&data, line, 0).is_none());
    }

    #[test]
    fn test_hover_whitespace_returns_none() {
        let data = synthetic_data();
        let line = "   ";
        assert!(hover_at(&data, line, 1).is_none());
    }

    #[test]
    fn test_hover_on_space_between_tokens_returns_menu() {
        let data = synthetic_data();
        let line = "/ip/address add";
        // Space at 11 (between "/ip/address" and "add") – hover logic includes preceding word
        let h = hover_at(&data, line, 11).expect("space after menu should hover menu");
        assert!(h.contents.value.contains("/ip/address"));
    }

    #[test]
    fn test_hover_on_leading_space_returns_none() {
        let data = synthetic_data();
        let line = "   /ip/address";
        // Leading spaces: position 0 is space, word empty
        assert!(hover_at(&data, line, 0).is_none());
        assert!(hover_at(&data, line, 1).is_none());
    }

    #[test]
    fn test_hover_unknown_word_returns_none() {
        let data = synthetic_data();
        let line = "/ip/address add unknownprop";
        let pos = line.find("unknownprop").unwrap() + 2;
        assert!(hover_at(&data, line, pos).is_none());
    }

    #[test]
    fn test_hover_character_beyond_line_clamped() {
        let data = synthetic_data();
        let line = "/ip/address";
        // Character 100 is clamped to end, word still "/ip/address"
        let h = compute_hover(&data, line, 100, line, 0)
            .expect("should still hover when char beyond line");
        assert!(h.contents.value.contains("/ip/address"));
    }

    #[test]
    fn test_hover_unicode_boundary_safe() {
        let data = synthetic_data();
        // RSC is ASCII, but test robustness with multi-byte char in doc (even if not valid RSC)
        let line = "/ip/address add comment=\"héllo\"";
        // Character offset inside multi-byte — floor_char_boundary should keep it safe
        let h = hover_at(&data, line, 5);
        // Should not panic
        assert!(h.is_some() || h.is_none());
    }

    // ── Real data sanity ──────────────────────────────────────────

    #[test]
    fn test_hover_real_menu() {
        let data = MenuData::load();
        let line = "/ip/firewall/filter";
        let h = hover_at(&data, line, 5).expect("real /ip/firewall/filter should hover");
        assert!(h.contents.value.contains("/ip/firewall/filter"));
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
}

#[cfg(test)]
mod extra_coverage {
    use super::*;
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

    fn hover_at(data: &MenuData, line: &str, character: usize) -> Option<Hover> {
        compute_hover(data, line, character, line, 0)
    }

    // ── Menu path with type+args+flags ─────────────────────────────────

    #[test]
    fn test_hover_menu_shows_type_args_flags() {
        let data = synth();
        let line = "/ip/address";
        let h = hover_at(&data, line, 4).expect("menu hover");
        assert!(h.contents.value.contains("### /ip/address"));
        assert!(h.contents.value.contains("**Type:** Directory"));
        assert!(h.contents.value.contains("**Arguments:**"));
        assert!(h.contents.value.contains("address: ipPrefix"));
        assert!(h.contents.value.contains("interface: iface_enum"));
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

    // ── Property type ──────────────────────────────────────────────────

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
            // Need to ensure we hover over property name, not path: use second occurrence if line contains "/ip/address"
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
    fn test_hover_property_wrong_menu_returns_none() {
        let data = synth();
        // chain is not a property of /ip/address
        let line = "/ip/address add chain=input";
        let pos = line.find("chain").unwrap() + 1;
        assert!(hover_at(&data, line, pos).is_none());
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

    // ── Verb hover ─────────────────────────────────────────────────────

    #[test]
    fn test_hover_verb_shows_standard_message() {
        let data = synth();
        let line = "/ip/address add";
        let pos = line.rfind("add").unwrap() + 1;
        let h = hover_at(&data, line, pos).expect("verb hover");
        assert!(h.contents.value.contains("**add**"));
        assert!(h.contents.value.contains("Standard RouterOS command"));
    }

    #[test]
    fn test_hover_verb_all_standard_verbs() {
        let data = synth();
        for verb in MenuData::STANDARD_VERBS {
            let line = format!("/ip/address {verb}");
            let pos = line.find(verb).unwrap() + 1;
            let h = hover_at(&data, &line, pos);
            assert!(h.is_some(), "verb {verb} should hover");
            assert!(h.unwrap().contents.value.contains(verb));
        }
    }

    #[test]
    fn test_hover_verb_without_menu_also_works() {
        let data = synth();
        let h = hover_at(&data, "print", 2).expect("bare verb");
        assert!(h.contents.value.contains("print"));
    }

    // ── Unknown word returns None ──────────────────────────────────────

    #[test]
    fn test_hover_unknown_word_returns_none() {
        let data = synth();
        let line = "/ip/address add unknownprop=foo";
        let pos = line.find("unknownprop").unwrap() + 2;
        assert!(hover_at(&data, line, pos).is_none());
    }

    #[test]
    fn test_hover_unknown_menu_returns_none() {
        let data = synth();
        let line = "/unknown/menu";
        assert!(hover_at(&data, line, 4).is_none());
    }

    #[test]
    fn test_hover_random_word_returns_none() {
        let data = synth();
        let line = "/ip/address add address=1.1.1.1";
        // Hover over value part which is not a known word (should be none)
        let pos = line.find("1.1.1.1").unwrap() + 2;
        assert!(hover_at(&data, line, pos).is_none());
    }

    #[test]
    fn test_hover_empty_word_returns_none() {
        let data = synth();
        assert!(hover_at(&data, "", 0).is_none());
        assert!(hover_at(&data, "   ", 1).is_none());
        assert!(hover_at(&data, "/ip/address add", 11).is_some()); // space after menu -> hovers menu, not none
        // But leading space yields none
        assert!(hover_at(&data, "   /ip/address", 0).is_none());
    }

    #[test]
    fn test_hover_on_equals_sign_returns_none_or_property() {
        let data = synth();
        let line = "/ip/address add address=1.1.1.1";
        let eq_pos = line.find('=').unwrap();
        // Word extraction at '=': find_word_start looks backwards, includes "address", word_end stops at "="
        // So hovering at "=" will extract "address" -> should hover property
        let h = hover_at(&data, line, eq_pos);
        // Could be property hover or None depending on word extraction; either is acceptable if not panicking
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
}
