//! White-box: parser.
use crate::parser::*;

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
fn test_build_before_cursor_comment_ending_in_backslash_is_inert() {
    // A comment line ending in a backslash does NOT continue: the
    // comment tail (and its backslash) is cut before continuation
    // counting, so the line is inert and the walk keeps going.
    let doc = "/ip/address\n# note \\\nadd address=1.1.1.1/24";
    let line = doc.lines().nth(2).unwrap();
    let s = build_before_cursor(doc, 2, line.len());
    assert!(!s.contains("note"), "comment content must be inert: {s}");
    assert!(s.contains("/ip/address"), "path must survive: {s}");
    assert!(
        s.contains("add address=1.1.1.1/24"),
        "command must be present: {s}"
    );
}

#[test]
fn test_build_before_cursor_strips_inline_comment_tail() {
    // An inline comment tail on a preceding line is cut (quote-aware)
    // before the line is contributed to the joined context.
    let doc = "/ip/address add # starting\naddress=1.1.1.1/24";
    let line = doc.lines().nth(1).unwrap();
    let s = build_before_cursor(doc, 1, line.len());
    assert_eq!(s, "/ip/address add address=1.1.1.1/24");
}

#[test]
fn test_build_before_cursor_lone_backslash_line_is_inert() {
    // A lone-backslash line has empty effective content (the odd run is
    // a continuation marker): inert, skipped, walk keeps going.
    let doc = "/ip/address\n\\\nadd address=1.1.1.1/24";
    let line = doc.lines().nth(2).unwrap();
    let s = build_before_cursor(doc, 2, line.len());
    assert_eq!(s, "/ip/address add address=1.1.1.1/24");
}

// ── effective_content_end ─────────────────────────────────────

#[test]
fn test_effective_content_end_units() {
    // '#' at byte offset 8 of "add x=1 # c".
    assert_eq!(effective_content_end("add x=1 # c"), 8);
    // '#' inside double quotes is literal content: whole line.
    assert_eq!(effective_content_end(r#"a="b#c""#), r#"a="b#c""#.len());
    // Leading comment: no effective content at all.
    assert_eq!(effective_content_end("#x"), 0);
    // No comment: whole line.
    assert_eq!(effective_content_end("print"), 5);
}

// ── parse_line ────────────────────────────────────────────────

#[test]
fn test_parse_line_path_only() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "/ip/address");
    assert_eq!(ctx.path, "/ip/address");
    assert!(ctx.command.is_none());
    assert!(ctx.properties.is_empty());
}

#[test]
fn test_parse_line_path_with_verb() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "/ip/address add");
    assert_eq!(ctx.path, "/ip/address");
    assert_eq!(ctx.command.as_deref(), Some("add"));
}

#[test]
fn test_parse_line_path_submenu_detection() {
    let data = synthetic_data();
    // "/ip" + "address" should be detected as sub-menu, not verb
    let ctx = parse_line(&data, "/ip address");
    assert_eq!(ctx.path, "/ip/address");
    assert!(ctx.command.is_none());
    let ctx2 = parse_line(&data, "/ip firewall filter");
    assert_eq!(ctx2.path, "/ip/firewall/filter");
}

#[test]
fn test_parse_line_verb_after_known_path() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "/ip/firewall/filter add");
    assert_eq!(ctx.path, "/ip/firewall/filter");
    assert_eq!(ctx.command.as_deref(), Some("add"));
}

#[test]
fn test_parse_line_properties() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "/ip/address add address=1.1.1.1 interface=ether1");
    assert_eq!(ctx.path, "/ip/address");
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert_eq!(
        ctx.properties.get("address").map(|s| s.as_str()),
        Some("1.1.1.1")
    );
    assert_eq!(
        ctx.properties.get("interface").map(|s| s.as_str()),
        Some("ether1")
    );
}

#[test]
fn test_parse_line_property_with_empty_value() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "/ip/firewall/filter add chain=");
    assert_eq!(ctx.properties.get("chain").map(|s| s.as_str()), Some(""));
}

#[test]
fn test_parse_line_no_path_command_only() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "print");
    assert_eq!(ctx.path, "");
    assert_eq!(ctx.command.as_deref(), Some("print"));
}

#[test]
fn test_parse_line_empty() {
    let data = synthetic_data();
    let ctx = parse_line(&data, "");
    assert_eq!(ctx.path, "");
    assert!(ctx.command.is_none());
    assert!(ctx.properties.is_empty());
}

#[test]
fn test_parse_line_quoted_value() {
    let data = synthetic_data();
    // Value without space stays as one token; with space the quote-aware
    // tokenizer also keeps it as ONE token (see tokenize docs).
    let ctx = parse_line(&data, r#"/ip/address add comment="hello""#);
    assert_eq!(
        ctx.properties.get("comment").map(|s| s.as_str()),
        Some("\"hello\"")
    );
    // This previously asserted the BROKEN behavior where
    // the tokenizer split quoted values at whitespace (`"\"hello"` plus an
    // orphaned `"world\"" token), which spawned phantom property tokens
    // and false unknown-property / duplicate-property warnings
    // downstream. Bare-word scanning is now quote-aware, so a quoted
    // value containing spaces stays a single token/value.
    let ctx2 = parse_line(&data, r#"/ip/address add comment="hello world""#);
    assert_eq!(
        ctx2.properties.get("comment").map(|s| s.as_str()),
        Some("\"hello world\"")
    );
}

#[test]
fn test_tokenize_inline_comment() {
    let tokens =
        tokenize_with_spans(r#"/ip/address add address=1.1.1.1/24 # comment with foo=bar"#);
    assert_eq!(tokens.len(), 3);
    assert_eq!(tokens[0].text, "/ip/address");
    assert_eq!(tokens[1].text, "add");
    assert_eq!(tokens[2].text, "address=1.1.1.1/24");
}

#[test]
fn test_tokenize_hash_inside_quotes_is_not_comment() {
    let tokens =
        tokenize_with_spans(r##"/ip/address add comment="#1 interface" address=1.1.1.1/24"##);
    assert_eq!(tokens.len(), 4);
    assert_eq!(tokens[0].text, "/ip/address");
    assert_eq!(tokens[1].text, "add");
    assert_eq!(tokens[2].text, r##"comment="#1 interface""##);
    assert_eq!(tokens[3].text, "address=1.1.1.1/24");
}

#[test]
fn test_tokenize_unquoted_hash_stops_token_mid_word() {
    // An unquoted '#' starts a comment at ANY position, even mid-word:
    // the comment tail must never leak into the token.
    assert_eq!(tokenize("add foo=bar#baz"), vec!["add", "foo=bar"]);
    assert_eq!(
        tokenize("add url=https://x#frag"),
        vec!["add", "url=https://x"]
    );
    // A '#' inside single quotes is literal content, not a comment:
    // the whole word stays ONE token.
    let tokens = tokenize(r#"comment='a # b'"#);
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0], r#"comment='a # b'"#);
}

#[test]
fn test_parse_line_with_inline_comment() {
    let data = synthetic_data();
    let ctx = parse_line(
        &data,
        "/ip/address add address=1.1.1.1/24 # comment with extra=prop",
    );
    assert_eq!(ctx.path, "/ip/address");
    assert_eq!(ctx.command.as_deref(), Some("add"));
    assert_eq!(
        ctx.properties.get("address").map(|s| s.as_str()),
        Some("1.1.1.1/24")
    );
    assert_eq!(ctx.properties.get("extra"), None);
}

// ── QuoteState parity ───────────────────────────────────────────
