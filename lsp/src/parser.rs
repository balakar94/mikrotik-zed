// ── Tokenizer / RouterOS line parser ────────────────────────────
//
// Quote-aware tokenization and structural parsing of RouterOS
// command lines (ported from ls.mjs). Pure functions over strings —
// consumers: completion, hover, diagnostics, and the LSP handlers.

use crate::menus::{LineContext, MenuData};
use std::collections::HashMap;

/// One token plus its byte span within the tokenized text.
///
/// Spans let consumers (diagnostics) point at the exact occurrence of a
/// token instead of re-finding it with substring search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpanToken {
    pub text: String,
    /// Inclusive start byte offset within the scanned text.
    pub start: usize,
    /// Exclusive end byte offset within the scanned text.
    pub end: usize,
}

/// Scan one whitespace-delimited token starting at byte offset `start`.
///
/// Tracks quote state so whitespace inside `"..."` or `'...'` does not split
/// the token and a backslash inside quotes escapes the next byte. This
/// mirrors the state machine of `continuation_body_end` in diagnostics.rs
/// (RouterOS treats both quote styles symmetrically). Returns the exclusive
/// end offset, which is always a char boundary: quote, backslash and
/// whitespace bytes only occur as standalone bytes in valid UTF-8.
fn scan_token(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut in_double = false;
    let mut in_single = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_double || in_single => {
                // Escaped byte inside quotes: skip it entirely. Clamp so a
                // trailing backslash cannot push `i` past `bytes.len()`.
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' if !in_single => in_double = !in_double,
            b'\'' if !in_double => in_single = !in_single,
            _ if !in_double && !in_single && bytes[i].is_ascii_whitespace() => break,
            _ => {}
        }
        i += 1;
    }
    i
}

// ── Whole-document structural walk ──────────────────────────────
//
// Shared quote/comment-aware scan over a full document, used by every
// consumer that must agree on what counts as a *structural* `{` / `}`
// / quote: folding ranges and the syntax diagnostics rules. Centralizing
// the state machine here means the two features cannot drift apart —
// a brace inside a comment or string is inert for both, and a `\`
// line-continuation keeps a quoted string alive across physical lines
// for both.

/// Maximum open-brace depth tracked by [`walk_structure`] consumers.
///
/// Bounds consumer-side stacks (a `Vec` of positions) for adversarial input
/// like `"{" repeated 5 million times`: memory stays capped, and only
/// structures nested beyond 4096 levels — not expressible in real RouterOS
/// scripts — lose tracking.
pub(crate) const MAX_BRACE_DEPTH: usize = 4096;

/// A structural character observed outside comments and quoted strings.
///
/// `character` is the BYTE offset within the physical line (the crate-internal
/// position convention; conversion to the negotiated wire encoding happens at
/// the protocol boundary). `line` is the zero-based physical line index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructureEvent {
    /// `{` outside any comment or quoted string.
    OpenBrace { line: usize, character: usize },
    /// `}` outside any comment or quoted string.
    CloseBrace { line: usize, character: usize },
    /// End of input reached while still inside the quoted string opened here.
    /// Reported once per walk, pointing at the OPENING quote.
    UnterminatedQuote { line: usize, character: usize },
}

/// Walk `doc` once, emitting [`StructureEvent`]s for structural characters.
///
/// State-machine semantics (identical to the scanner this was extracted from,
/// formerly private to `folding::brace_regions`):
/// - Inside `"…"` / `'…'`, a `\` escapes the next byte; both quote styles
///   toggle symmetrically and cannot nest inside each other.
/// - An unquoted `#` starts a comment that runs to end-of-line.
/// - Quote state carries ACROSS physical lines: RouterOS strings may be split
///   by a trailing `\` continuation, so resetting per line would let split
///   URLs desynchronize brace matching. Comment and escape states reset at
///   line boundaries.
/// - A backslash outside quotes is not special (matches long-shipped folding
///   behavior); escape sequences are recognized inside quotes only.
///
/// Single linear pass, no allocation; events arrive in document order.
pub(crate) fn walk_structure<F>(doc: &str, mut on_event: F)
where
    F: FnMut(StructureEvent),
{
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    let mut in_comment = false;
    // Position of the quote that opened the currently active quoted string,
    // so EOF can report the OPENING quote instead of the end of input.
    // Cleared when the string closes; replaced if another quote opens after
    // a legitimate close.
    let mut quote_open: Option<(usize, usize)> = None;

    for (line_idx, line) in doc.lines().enumerate() {
        for (col, c) in line.char_indices() {
            if in_comment {
                continue; // comments end at end-of-line (reset below)
            }
            if escaped {
                // Escaped byte inside quotes: never structural.
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_double || in_single => escaped = true,
                '"' if !in_single => {
                    in_double = !in_double;
                    quote_open = if in_double {
                        Some((line_idx, col))
                    } else {
                        None
                    };
                }
                '\'' if !in_double => {
                    in_single = !in_single;
                    quote_open = if in_single {
                        Some((line_idx, col))
                    } else {
                        None
                    };
                }
                '#' if !in_double && !in_single => in_comment = true,
                '{' if !in_double && !in_single => {
                    on_event(StructureEvent::OpenBrace {
                        line: line_idx,
                        character: col,
                    });
                }
                '}' if !in_double && !in_single => {
                    on_event(StructureEvent::CloseBrace {
                        line: line_idx,
                        character: col,
                    });
                }
                _ => {}
            }
        }
        // Physical line boundary resets per-line states. Quote state does
        // NOT reset: a `\`-continuation can legally split a quoted string.
        in_comment = false;
        escaped = false;
    }

    // EOF inside a quoted string: point at the opening quote so the user
    // sees where the string started, not where the file happens to end.
    if (in_double || in_single)
        && let Some((line, character)) = quote_open
    {
        on_event(StructureEvent::UnterminatedQuote { line, character });
    }
}

/// Split a line into tokens with spans: quoted strings, /-prefixed paths, or
/// bare words.
///
/// Quote-aware: a bare word that opens a quote keeps consuming across
/// whitespace until the matching close (e.g. `comment="a=b c=d"` stays ONE
/// token), so quoted values can no longer spawn phantom property tokens
/// downstream. Unterminated quotes simply run to end-of-input.
pub(crate) fn tokenize_with_spans(text: &str) -> Vec<SpanToken> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Quoted string, /-prefixed path, or bare word — all share the same
        // quote-aware scanner; the distinction lives in the token text, not
        // in how far it scans.
        let start = i;
        let end = scan_token(bytes, i);
        tokens.push(SpanToken {
            text: std::str::from_utf8(&bytes[start..end])
                .unwrap_or("")
                .to_string(),
            start,
            end,
        });
        i = end;
    }

    tokens
}

/// [`tokenize_with_spans`] without the span bookkeeping (kept for callers
/// that only need token text).
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    tokenize_with_spans(text)
        .into_iter()
        .map(|t| t.text)
        .collect()
}

/// Build the "before cursor" context across multiple lines.
///
/// RouterOS commands can span multiple lines — properties on subsequent lines
/// are continuations of the same command.  Walks backwards from the cursor
/// line, collecting all lines belonging to the current command.
///
/// `cursor_char` is a BYTE offset within the cursor line (already converted
/// from the negotiated wire encoding by callers at the protocol boundary).
///
/// The result is intentionally NOT right-trimmed: trailing whitespace before
/// the cursor is the signal that distinguishes "typing inside the last
/// token" (value-completion mode) from "finished the token, starting a new
/// one" (property-completion mode). The tokenizer ignores surrounding
/// whitespace anyway, so only consumers that care about the cursor boundary
/// can observe the difference.
pub fn build_before_cursor(doc: &str, cursor_line: usize, cursor_char: usize) -> String {
    let lines: Vec<&str> = doc.lines().collect();
    if cursor_line >= lines.len() {
        return String::new();
    }

    let line = lines[cursor_line];
    let clamped = cursor_char.min(line.len());
    let safe_char = crate::floor_char_boundary(line, clamped);
    let current_part = &line[..safe_char];
    if current_part.trim().is_empty() {
        return String::new();
    }

    let mut parts = vec![current_part];

    for i in (0..cursor_line).rev() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with('/') || trimmed.starts_with(':') {
            parts.insert(0, lines[i]);
            break;
        }
        parts.insert(0, lines[i]);
    }

    parts.join(" ")
}

/// Parse a line of RouterOS script into structural components.
pub fn parse_line(data: &MenuData, before_cursor: &str) -> LineContext {
    let tokens = tokenize(before_cursor);
    let mut path_parts: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    let mut properties: HashMap<String, String> = HashMap::new();
    let last_token = tokens.last().cloned().unwrap_or_default();

    for token in &tokens {
        if token.starts_with('/') {
            path_parts.push(token.trim_start_matches('/').to_string());
            continue;
        }

        if let Some(eq_idx) = token.find('=') {
            let key = token[..eq_idx].to_string();
            let value = token[eq_idx + 1..].to_string();
            properties.insert(key, value);
            continue;
        }

        if !path_parts.is_empty() {
            let current_path = format!("/{}", path_parts.join("/"));
            // Use child_names_by_parent (not menu_by_path) so implicit
            // intermediate menus like /ip/firewall are recognized as valid
            // path segments even though they have no direct TOML entry.
            let is_sub_menu = data
                .child_names_by_parent
                .get(&current_path)
                .map(|children| children.iter().any(|c| c.name == *token))
                .unwrap_or(false);
            if is_sub_menu {
                path_parts.push(token.clone());
            } else {
                command = Some(token.clone());
            }
            continue;
        }

        command = Some(token.clone());
    }

    LineContext {
        path: if path_parts.is_empty() {
            String::new()
        } else {
            format!("/{}", path_parts.join("/"))
        },
        command,
        properties,
        last_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            StructureEvent::UnterminatedQuote { line, character } => {
                out.push((ev, line, character))
            }
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

    // ── parse_line ────────────────────────────────────────────────

    #[test]
    fn test_parse_line_path_only() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address");
        assert_eq!(ctx.path, "/ip/address");
        assert!(ctx.command.is_none());
        assert!(ctx.properties.is_empty());
        assert_eq!(ctx.last_token, "/ip/address");
    }

    #[test]
    fn test_parse_line_path_with_verb() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address add");
        assert_eq!(ctx.path, "/ip/address");
        assert_eq!(ctx.command.as_deref(), Some("add"));
        assert_eq!(ctx.last_token, "add");
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
        assert_eq!(ctx.last_token, "interface=ether1");
    }

    #[test]
    fn test_parse_line_property_with_empty_value() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/firewall/filter add chain=");
        assert_eq!(ctx.properties.get("chain").map(|s| s.as_str()), Some(""));
        assert_eq!(ctx.last_token, "chain=");
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
        assert_eq!(ctx.last_token, "");
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
        // PHASE 1 FLIP: this previously asserted the BROKEN behavior where
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
        // last_token remains the RAW token text (key included), per LineContext.
        assert_eq!(ctx2.last_token, r#"comment="hello world""#);
    }
}
