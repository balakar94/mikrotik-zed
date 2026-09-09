// Brace, comment and string syntax rules.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod syntax_rules` L3130-3135, L3141-3170, L3172-3356); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::diagnostics::*;
use crate::menus::MenuData;

// ── Syntactic structure rules (unclosed braces / quotes) ───────────
//
// Coverage for rules 6–8. Docs deliberately favor `:`-prefixed script lines
// (skipped by the menu rules) so total-count assertions isolate the syntax
// pipeline; one interaction test proves both rule families coexist in the
// same publish.
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

fn codes(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().filter_map(|d| d.code.as_deref()).collect()
}
#[test]
fn test_balanced_doc_no_syntax_diagnostics() {
    // Realistic script shape: nested blocks, braces inside strings and a
    // trailing comment — all inert or matched. Also proves a stray-close
    // report is NOT raised for legitimate closers of real blocks.
    let doc = concat!(
        ":do {\n",
        "\t:foreach i in=[find] do={\n",
        "\t\t:put (\"item { \" . $i)\n",
        "\t}\n",
        "}\n",
        ":put 'all done}'\n",
        "# trailing { comment\n",
    );
    assert!(
        compute_diagnostics(&synth(), doc, "file:///a.rsc").is_empty(),
        "balanced doc must stay clean"
    );
}

#[test]
fn test_single_unclosed_brace_exact_range() {
    // ':foreach i in=[find] do={' → '{' sits at byte 24 of line 0.
    let doc = ":foreach i in=[find] do={\n\t:put $i\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    assert_eq!(diags.len(), 1, "exactly one syntax error, got {diags:?}");
    let d = &diags[0];
    assert_eq!(d.code.as_deref(), Some("unclosed-brace"));
    assert_eq!(d.severity, Some(severity::ERROR));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    assert_eq!(d.message, "Brace '{' opened here is never closed");
    // Range covers EXACTLY the brace character.
    assert_eq!(d.range.start.line, 0);
    assert_eq!(d.range.start.character, 24);
    assert_eq!(d.range.end.line, 0);
    assert_eq!(d.range.end.character, 25);
}

#[test]
fn test_nested_unclosed_braces_each_reported() {
    // Outer opens at (0,4), inner at (1,18); neither ever closes.
    let doc = ":do {\n\t:if ($a > $b) do={\n\t\t:put x\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    assert_eq!(
        diags.len(),
        2,
        "both unclosed opens reported, got {diags:?}"
    );
    assert_eq!(
        diags[0].range.start,
        Position {
            line: 0,
            character: 4
        }
    );
    assert_eq!(
        diags[1].range.start,
        Position {
            line: 1,
            character: 18
        }
    );
    assert!(codes(&diags).iter().all(|&c| c == "unclosed-brace"));
}

#[test]
fn test_brace_inside_comment_is_inert() {
    // '}', '{' and even a quote inside a comment must never fire.
    let doc = "# } { \" unterminated-looking\n:put x\n";
    assert!(compute_diagnostics(&synth(), doc, "file:///a.rsc").is_empty());
}

#[test]
fn test_brace_inside_closed_string_is_inert() {
    let doc = ":put \"}{\"\n:put '}'\n# c {\n:put x\n";
    assert!(compute_diagnostics(&synth(), doc, "file:///a.rsc").is_empty());
}

#[test]
fn test_split_url_continuation_not_flagged() {
    // Real-world hagezi repro (mirror of the continuation tests above):
    // a quoted URL split across lines by a trailing backslash inside the
    // string must not read as an unclosed quote.
    let data = synth();
    let doc = concat!(
        "/tool/fetch add ssl-verify=no url=\"https://raw.githubusercontent.com",
        "/hagezi/dns-blocklists\\\n/main/hosts/pro.txt\"",
    );
    let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
    assert!(
        diags.is_empty(),
        "split-URL continuation must stay clean, got {diags:?}"
    );
}

#[test]
fn test_unterminated_quote_at_eof_reports_opening_quote_once() {
    // The open string swallows the rest of the document (including a
    // stray '}') — exactly ONE error, pointing at the OPENING quote.
    // ':log info "' → quote at byte 10 of line 0.
    let doc = ":log info \"oops\n:put x\n}\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    assert_eq!(
        diags.len(),
        1,
        "no cascade past the root cause, got {diags:?}"
    );
    let d = &diags[0];
    assert_eq!(d.code.as_deref(), Some("unclosed-quote"));
    assert_eq!(d.severity, Some(severity::ERROR));
    assert_eq!(d.message, "Quoted string opened here is never closed");
    assert_eq!(d.range.start.line, 0);
    assert_eq!(d.range.start.character, 10);
    assert_eq!(d.range.end.character, 11);
}

#[test]
fn test_quote_spanning_lines_via_continuation_not_flagged() {
    // String legitimately continues across the physical line via a
    // trailing backslash INSIDE the quotes and closes on line 1.
    let doc = ":put \"abc\\\ndef\"\n:put done\n";
    assert!(compute_diagnostics(&synth(), doc, "file:///a.rsc").is_empty());
}

#[test]
fn test_crlf_variant_matches_lf() {
    // LF baseline: unclosed brace at (0,4) plus unclosed quote at (1,6).
    let lf = ":do {\n\t:put \"unterminated\n";
    let diags = compute_diagnostics(&synth(), lf, "file:///a.rsc");
    let starts: Vec<_> = diags
        .iter()
        .map(|d| (d.range.start.line, d.range.start.character))
        .collect();
    assert_eq!(
        starts,
        vec![(0, 4), (1, 6)],
        "LF: brace then quote, oldest first, got {diags:?}"
    );

    // CRLF produces identical results (str::lines strips '\r'; columns
    // are byte offsets within the stripped line).
    let crlf = lf.replace('\n', "\r\n");
    let diags_crlf = compute_diagnostics(&synth(), &crlf, "file:///a.rsc");
    let starts_crlf: Vec<_> = diags_crlf
        .iter()
        .map(|d| (d.range.start.line, d.range.start.character))
        .collect();
    assert_eq!(starts_crlf, starts, "CRLF must match LF results");
}

#[test]
fn test_syntax_diagnostics_capped_at_10_oldest_first() {
    // 15 unclosed opens on their own lines ('{' lines are skipped by the
    // menu rules): only the FIRST ten survive, in document order, plus
    // one explicit `truncated` footer naming the dropped remainder.
    let doc = "{\n".repeat(15);
    let diags = compute_diagnostics(&synth(), &doc, "file:///a.rsc");
    assert_eq!(
        diags.len(),
        11,
        "cap keeps exactly 10 plus one truncated footer, got {}",
        diags.len()
    );
    for (i, d) in diags.iter().take(10).enumerate() {
        assert_eq!(d.code.as_deref(), Some("unclosed-brace"));
        assert_eq!(
            d.range.start,
            Position {
                line: i as u32,
                character: 0
            },
            "oldest-first: line {i} expected"
        );
    }
    let footer = &diags[10];
    assert_eq!(footer.code.as_deref(), Some("truncated"));
    assert_eq!(footer.severity, Some(severity::INFORMATION));
    assert_eq!(footer.source.as_deref(), Some("rsc-ls"));
    assert!(
        footer.message.contains("(+5 more - see full list)"),
        "footer must name the dropped remainder, got {:?}",
        footer.message
    );
}
