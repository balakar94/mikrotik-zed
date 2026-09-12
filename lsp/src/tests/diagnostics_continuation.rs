// Continuation edge cases and logical line maps.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod extra_coverage` L2359-2410, L2908-3026);
// the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::diagnostics::*;
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
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/list"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus]]
path = "/tool/ping"
type = "Command"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
[[menus.arguments]]
name = "ssl-verify"
type = "bool"
"#,
    )
}
#[test]
fn test_comment_not_continued() {
    let data = synth();
    // A '#' comment never continues, even with a trailing backslash.
    let doc = "# note \\\n/foo/bar add x=1";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    let menus: Vec<_> = diags
        .iter()
        .filter(|d| d.code.as_deref() == Some("unknown-menu"))
        .collect();
    assert_eq!(menus.len(), 1, "only /foo/bar should be flagged");
    assert!(menus[0].message.contains("/foo/bar"));
    assert_eq!(menus[0].range.start.line, 1);
}

#[test]
fn test_dangling_continuation_at_eof_no_panic() {
    let data = synth();
    // EOF right after the backslash: must not panic; the logical line is
    // flushed and missing-required is still reported sensibly.
    let doc = "/ip/address add address=1.1.1.1\\";
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    let missing = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("missing-required") && d.message.contains("interface"))
        .expect("interface should still be reported as missing");
    assert_eq!(missing.range.start.line, 0);
}

#[test]
fn test_crlf_continuation() {
    let data = synth();
    // Same reproduction as the quoted-url case but with CRLF endings.
    let doc = concat!(
        "/tool/fetch add ssl-verify=no url=\"https://raw.githubusercontent.com",
        "/hagezi/dns-blocklists\\\r\n/main/hosts/pro.txt\"",
    );
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu")),
        "CRLF continuation must not produce unknown-menu, got {diags:?}"
    );
}

#[test]
fn test_has_line_continuation_cases() {
    // Single trailing backslash: normal continuation.
    assert!(has_line_continuation("/ip/address add address=1.1.1.1\\"));
    // Trailing backslash followed by whitespace: still continues.
    assert!(has_line_continuation("add x=1\\   "));
    // Escaped pair = literal backslash: NOT a continuation.
    assert!(!has_line_continuation("add comment=x\\\\"));
    // Triple run = one escaped + one continuation.
    assert!(has_line_continuation("add comment=x\\\\\\"));
    // Inside double quotes (unterminated string): continues.
    assert!(has_line_continuation("url=\"https://example.com/foo\\"));
    // Escaped quote inside double quotes, then trailing backslash.
    assert!(has_line_continuation("url=\"a\\\" b\\"));
    // Single quotes behave like double quotes.
    assert!(has_line_continuation("set x='abc\\"));
    // Unquoted '#' cuts effective content: comments never continue.
    assert!(!has_line_continuation("# note \\"));
    assert!(!has_line_continuation("add x=1 # trailing \\"));
    // Plain lines are not continuations.
    assert!(!has_line_continuation(""));
    assert!(!has_line_continuation("print"));
}

#[test]
fn test_logical_line_map_spans_join() {
    // A range whose start/end land on different physical lines maps to a
    // multi-line LSP range (allowed by the spec).
    let ll = build_logical_lines(&["/tool/fetch add url=\"abc\\", "def\""]);
    assert_eq!(ll.len(), 1);
    // Joined text: /tool/fetch add url="abcdef" (len 28).
    let joined = ll[0].text.as_str();
    assert_eq!(joined, "/tool/fetch add url=\"abcdef\"");
    assert_eq!(ll[0].segments.len(), 2);
    // Segment 0 covers bytes 0..24 ("...\"abc"), segment 1 bytes 24..28
    // ("def\""). A range from 'c' (byte 23, physical line 0) through 'd'
    // (byte 24, physical line 1) spans the join point.
    let r = ll[0].map_range(23, 25);
    assert_eq!(r.start.line, 0);
    assert_eq!(r.start.character, 23);
    assert_eq!(r.end.line, 1);
    assert_eq!(r.end.character, 1);
    // Out-of-bounds offsets clamp defensively to the end of the text:
    // byte 28 lands at line 1, character 4.
    let clamped = ll[0].map_pos(joined.len() + 100);
    assert_eq!(clamped.line, 1);
    assert_eq!(clamped.character, 4);
}

// ── Continuation-boundary mapping ────────────────────────────────────────

/// One logical line from two physical segments: line 0 covers bytes 0..24
/// (`/tool/fetch add url="abc`), line 1 covers bytes 24..28 (`def"`).
/// Byte 24 is the `\` continuation boundary.
fn two_segment_line() -> Vec<LogicalLine> {
    let ll = build_logical_lines(&["/tool/fetch add url=\"abc\\", "def\""]);
    assert_eq!(ll.len(), 1);
    ll
}

#[test]
fn test_map_range_ending_at_boundary_stays_on_previous_line() {
    let ll = two_segment_line();
    // 23..24 covers only 'c' (byte 23). It must END on line 0 rather than
    // jump across the backslash to the start of line 1.
    let r = ll[0].map_range(23, 24);
    assert_eq!((r.start.line, r.start.character), (0, 23));
    assert_eq!((r.end.line, r.end.character), (0, 24));
}

#[test]
fn test_map_range_both_endpoints_at_boundaries() {
    let ll = two_segment_line();
    // Start at the boundary (byte 24) OPENS line 1; end at the text end
    // (byte 28) closes it. The result is a coherent single-line line-1
    // range, never a dangling start at the end of line 0.
    let r = ll[0].map_range(24, 28);
    assert_eq!((r.start.line, r.start.character), (1, 0));
    assert_eq!((r.end.line, r.end.character), (1, 4));
}

#[test]
fn test_map_range_zero_length_at_boundary_keeps_boundary_end() {
    let ll = two_segment_line();
    // A zero-length point at the boundary resolves to the END of the
    // preceding segment, matching `map_pos`, and stays zero-length rather
    // than inverting across the join.
    let r = ll[0].map_range(24, 24);
    assert_eq!((r.start.line, r.start.character), (0, 24));
    assert_eq!((r.end.line, r.end.character), (0, 24));
    let p = ll[0].map_pos(24);
    assert_eq!((p.line, p.character), (0, 24));
}

#[test]
fn test_map_range_mid_segment_unchanged() {
    let ll = two_segment_line();
    // A normal range wholly inside segment 1 is untouched by the boundary
    // rules.
    let r = ll[0].map_range(25, 27);
    assert_eq!((r.start.line, r.start.character), (1, 1));
    assert_eq!((r.end.line, r.end.character), (1, 3));
}

// ── Token-position ranges ────────────────────────────────────────────────

fn demo_menu_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo/alpha"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"

[[menus]]
path = "/demo/enum"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (on | off)"
"#,
    )
}
