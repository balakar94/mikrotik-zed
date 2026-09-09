//! White-box: parser.
use crate::parser::*;
fn events(doc: &str) -> Vec<(StructureEvent, usize, usize)> {
    let mut out = Vec::new();
    walk_structure(doc, |ev| match ev {
        StructureEvent::OpenBrace { line, character } => out.push((ev, line, character)),
        StructureEvent::CloseBrace { line, character } => out.push((ev, line, character)),
        StructureEvent::UnterminatedQuote { line, character } => out.push((ev, line, character)),
    });
    out
}

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
[[menus.arguments]]
name = "interface"
type = "iface_enum"
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
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus]]
path = "/system/clock"
type = "Directory"
"#,
    )
}

// ── tokenize ──────────────────────────────────────────────────

#[test]
fn test_walk_structure_escaped_quotes_do_not_confuse_state() {
    // Escaped quotes stay inside the string, so the brace after them is
    // inert and the string closes normally.
    assert!(events(":put \"a\\\"b{\"\n").is_empty());
}

#[test]
fn test_walk_structure_empty_document_yields_no_events() {
    assert!(events("").is_empty());
    assert!(events("\n\n   \n").is_empty());
}

// ── build_before_cursor ───────────────────────────────────────

#[test]
fn test_build_before_cursor_single_line() {
    let doc = "/ip/address add address=1.1.1.1";
    let s = build_before_cursor(doc, 0, 10);
    assert_eq!(s, "/ip/addres");
}

#[test]
fn test_build_before_cursor_full_line() {
    let doc = "/ip/address add";
    let s = build_before_cursor(doc, 0, doc.len());
    assert_eq!(s, "/ip/address add");
}

#[test]
fn test_build_before_cursor_out_of_bounds_line() {
    let doc = "/ip/address";
    let s = build_before_cursor(doc, 5, 0);
    assert_eq!(s, "");
}

#[test]
fn test_build_before_cursor_empty_current_line() {
    let doc = "/ip/address add\n   \naddress=1.1.1.1";
    let s = build_before_cursor(doc, 1, 3);
    assert_eq!(s, "");
}

#[test]
fn test_build_before_cursor_multiline_continuation() {
    let doc = "/ip/address add\naddress=1.1.1.1 interface=ether1";
    // Cursor on line 1, char beyond line
    let s = build_before_cursor(doc, 1, doc.lines().nth(1).unwrap().len());
    assert!(
        s.contains("/ip/address add"),
        "should include previous line"
    );
    assert!(s.contains("address=1.1.1.1"));
}

#[test]
fn test_build_before_cursor_stops_at_blank_line() {
    let doc = "/ip/route add gateway=1.1.1.1\n\n/ip/address add";
    let s = build_before_cursor(doc, 2, 5);
    // Previous line is blank, so should only return current part
    assert_eq!(s, "/ip/a");
}

#[test]
fn test_build_before_cursor_stops_at_slash_line() {
    let _doc = "/ip/address add address=1.1.1.1\n/ip/route add";
    // When cursor is on second command (starts with /), the function includes that command
    // plus at most one preceding slash-command line as context. It joins them.
    let doc2 = "/ip/address print\n/ip/route add gateway=1.1.1.1";
    let s = build_before_cursor(doc2, 1, 10);
    // Should contain the previous slash line and the current part
    assert!(
        s.contains("/ip/address print"),
        "should include previous slash line: {s}"
    );
    assert!(
        s.contains("/ip/route"),
        "should contain current line start: {s}"
    );
    // Cursor at character 10 sits ON the space after "/ip/route", and
    // that boundary whitespace is preserved by design.
    assert_eq!(s, "/ip/address print /ip/route ");
}

#[test]
fn test_build_before_cursor_preserves_cursor_boundary_whitespace() {
    let doc = "  /ip/address add  ";
    let s = build_before_cursor(doc, 0, doc.len());
    // Whitespace before the cursor is PRESERVED (both sides): the
    // trailing part is the signal that the cursor sits after a finished
    // token (property completions) rather than inside one (value
    // completions). Leading indentation is irrelevant to the tokenizer.
    assert_eq!(s, "  /ip/address add  ");
}

#[test]
fn test_build_before_cursor_boundary_distinguishes_token_modes() {
    // Inside a token: no trailing whitespace…
    let doc = "/ip/firewall/filter add chain=in";
    assert_eq!(build_before_cursor(doc, 0, doc.len()), doc);
    // …after whitespace: boundary preserved for completion gating.
    let doc2 = "/ip/firewall/filter add chain=input ";
    assert_eq!(
        build_before_cursor(doc2, 0, doc2.len()),
        "/ip/firewall/filter add chain=input "
    );
}

#[test]
fn test_build_before_cursor_utf8_safe() {
    let doc = "/ip/address add comment=\"héllo\"";
    let s = build_before_cursor(doc, 0, doc.len());
    assert!(s.contains("héllo"));
}

#[test]
fn test_build_before_cursor_clamps_char_beyond() {
    let doc = "/ip/address";
    let s = build_before_cursor(doc, 0, 100);
    assert_eq!(s, "/ip/address");
}

#[test]
fn test_build_before_cursor_comment_between_path_and_command_is_inert() {
    // A full-line comment between the path line and the command line
    // must not break the walk — the path context survives.
    let doc = "/ip/address\n# some note\nadd address=1.1.1.1/24";
    let line = doc.lines().nth(2).unwrap();
    let s = build_before_cursor(doc, 2, line.len());
    assert!(
        s.contains("/ip/address"),
        "path line must survive the comment: {s}"
    );
    assert!(
        s.contains("add address=1.1.1.1/24"),
        "command line must be present: {s}"
    );
    let data = synthetic_data();
    let ctx = parse_line(&data, &s);
    assert_eq!(ctx.path, "/ip/address");
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert_eq!(
        ctx.properties.get("address").map(|v| v.as_str()),
        Some("1.1.1.1/24")
    );
}

#[test]
fn test_build_before_cursor_comment_inert_equivalence() {
    // Inserting a full-line comment changes nothing: the joined context
    // is identical with or without it.
    let plain = "/ip/address\nadd address=1.1.1.1/24";
    let with_comment = "/ip/address\n# some note\nadd address=1.1.1.1/24";
    let a = build_before_cursor(plain, 1, plain.lines().nth(1).unwrap().len());
    let b = build_before_cursor(with_comment, 2, with_comment.lines().nth(2).unwrap().len());
    assert_eq!(a, b);
}

#[test]
fn test_build_before_cursor_strips_continuation_backslash() {
    // A trailing continuation backslash on the preceding line must not
    // survive into the joined text as a bare '\' token.
    let doc = "/ip/address add \\\naddress=1.1.1.1/24";
    let line = doc.lines().nth(1).unwrap();
    let s = build_before_cursor(doc, 1, line.len());
    assert_eq!(s, "/ip/address add address=1.1.1.1/24");
    for token in tokenize(&s) {
        assert!(!token.contains('\\'), "no backslash token allowed: {s}");
    }
    let data = synthetic_data();
    let ctx = parse_line(&data, &s);
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert_eq!(
        ctx.properties.get("address").map(|v| v.as_str()),
        Some("1.1.1.1/24")
    );
}

#[test]
fn test_build_before_cursor_keeps_escaped_backslash_pair() {
    // Two trailing backslashes are an escaped literal pair, NOT a
    // continuation marker: both must be kept in the joined text.
    let doc = "/ip/address add comment=x\\\\\naddress=1.1.1.1/24";
    let line = doc.lines().nth(1).unwrap();
    let s = build_before_cursor(doc, 1, line.len());
    assert_eq!(s, "/ip/address add comment=x\\\\ address=1.1.1.1/24");
}
