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
fn test_tokenize_simple() {
    assert_eq!(
        tokenize("/ip address print"),
        vec!["/ip", "address", "print"]
    );
}

#[test]
fn test_tokenize_with_equals() {
    assert_eq!(
        tokenize("/ip/address add address=1.1.1.1 interface=ether1"),
        vec!["/ip/address", "add", "address=1.1.1.1", "interface=ether1"]
    );
}

#[test]
fn test_tokenize_quoted_string() {
    let tokens = tokenize(r#":put "hello world""#);
    assert_eq!(tokens, vec![":put", "\"hello world\""]);
}

#[test]
fn test_tokenize_quoted_with_escaped_quotes() {
    let tokens = tokenize(r#":put "say \"hello\"""#);
    assert_eq!(tokens.len(), 2);
    assert!(tokens[1].contains("hello"));
}

#[test]
fn test_tokenize_path_token() {
    let tokens = tokenize("/ip/firewall/filter add chain=input");
    assert_eq!(tokens[0], "/ip/firewall/filter");
    assert_eq!(tokens[1], "add");
    assert_eq!(tokens[2], "chain=input");
}

#[test]
fn test_tokenize_empty_and_whitespace() {
    assert!(tokenize("").is_empty());
    assert!(tokenize("   ").is_empty());
    assert!(tokenize("\t\n  ").is_empty());
}

#[test]
fn test_tokenize_multiple_spaces() {
    assert_eq!(
        tokenize("  /ip   address   add  "),
        vec!["/ip", "address", "add"]
    );
}

#[test]
fn test_tokenize_escaped_backslash_in_quote() {
    let tokens = tokenize(r#":put "a\\b""#);
    assert_eq!(tokens, vec![":put", "\"a\\\\b\""]);
}

#[test]
fn test_tokenize_bare_word_with_equals_and_no_value() {
    assert_eq!(tokenize("chain="), vec!["chain="]);
}

#[test]
fn test_tokenize_quoted_value_with_spaces_and_equals() {
    // '=' and spaces inside quotes must not create phantom properties.
    let tokens = tokenize(r#"comment="a=b c=d""#);
    assert_eq!(tokens, vec![r#"comment="a=b c=d""#]);
}

#[test]
fn test_tokenize_escaped_quotes_inside_string() {
    // Escaped quotes do not terminate the string…
    let tokens = tokenize(r#"comment="say \"hi\" now""#);
    assert_eq!(tokens.len(), 1);
    assert!(tokens[0].contains("\\\"hi\\\""));
    assert!(tokens[0].ends_with("now\""));
    // …and escaped backslashes are skipped pairwise.
    let tokens = tokenize(r#"comment="a\\b c""#);
    assert_eq!(tokens.len(), 1);
}

#[test]
fn test_tokenize_unterminated_quote_terminates_at_eof() {
    // Unterminated quote: scan runs to end-of-input without looping
    // forever or panicking.
    let tokens = tokenize(r#"comment="unterminated"#);
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0], r#"comment="unterminated"#);
    // Lone trailing backslash inside an open quote must clamp, not
    // overshoot the buffer.
    let tokens = tokenize("comment=\"oops\\");
    assert_eq!(tokens.len(), 1);
}

#[test]
fn test_tokenize_spans_record_exact_offsets() {
    let text = "/ip/address add chain=input";
    let spans = tokenize_with_spans(text);
    assert_eq!(spans.len(), 3);
    assert_eq!(&text[spans[0].start..spans[0].end], "/ip/address");
    assert_eq!(&text[spans[1].start..spans[1].end], "add");
    assert_eq!(&text[spans[2].start..spans[2].end], "chain=input");
}

// ── walk_structure ────────────────────────────────────────────

fn events(doc: &str) -> Vec<(StructureEvent, usize, usize)> {
    let mut out = Vec::new();
    walk_structure(doc, |ev| match ev {
        StructureEvent::OpenBrace { line, character } => out.push((ev, line, character)),
        StructureEvent::CloseBrace { line, character } => out.push((ev, line, character)),
        StructureEvent::UnterminatedQuote { line, character } => out.push((ev, line, character)),
    });
    out
}

#[test]
fn test_walk_structure_reports_brace_events_in_document_order() {
    let doc = ":do {\nx\n}\n}\n";
    // Open at ":do {" col 4; close on line 2 matches it; the close on
    // line 3 has an empty stack but the WALKER still reports it —
    // matching is the consumer's job.
    assert_eq!(
        events(doc),
        vec![
            (
                StructureEvent::OpenBrace {
                    line: 0,
                    character: 4
                },
                0,
                4
            ),
            (
                StructureEvent::CloseBrace {
                    line: 2,
                    character: 0
                },
                2,
                0
            ),
            (
                StructureEvent::CloseBrace {
                    line: 3,
                    character: 0
                },
                3,
                0
            ),
        ]
    );
}

#[test]
fn test_walk_structure_ignores_strings_and_comments() {
    // Braces inside double quotes, single quotes, and comments are all
    // inert. The UNCLOSED single-quoted string IS reported at its
    // opening quote.
    let doc = ":put \"}{\" # }\n'open brace { stays inert\n";
    assert_eq!(
        events(doc),
        vec![(
            StructureEvent::UnterminatedQuote {
                line: 1,
                character: 0
            },
            1,
            0
        )]
    );
}

#[test]
fn test_walk_structure_unterminated_quote_points_at_opening_quote() {
    // Quote state carries across a RAW newline (no continuation): the
    // event points at the OPENING quote on line 0, not EOF.
    let doc = ":put \"abc\ndef\n";
    assert_eq!(
        events(doc),
        vec![(
            StructureEvent::UnterminatedQuote {
                line: 0,
                character: 5
            },
            0,
            5
        )]
    );
}

#[test]
fn test_walk_structure_continuation_keeps_string_alive() {
    // Split string via trailing backslash that DOES close → silent.
    assert!(events(":put \"ab\\\ncd\"\n").is_empty());
    // Split string via continuation that never closes → opening quote.
    assert_eq!(
        events(":put \"ab\\\ncd\n"),
        vec![(
            StructureEvent::UnterminatedQuote {
                line: 0,
                character: 5
            },
            0,
            5
        )]
    );
}
