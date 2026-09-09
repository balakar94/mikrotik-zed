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
fn test_quote_state_parity_tokenize_vs_effective_content() {
    // Every line's effective_content_end must equal the prefix that
    // tokenize_with_spans would keep: tokens joined must be prefix of
    // the effective content.
    let cases = [
        r#"add comment="a # b" x=1 # tail"#,
        r#"add comment='a # b' url="https://x#frag""#,
        r#"comment="say \"hi\" # not comment" y=1"#,
        "plain # comment",
        r#""open quote without close"#,
        r#"a=\"escaped\""#,
    ];
    for line in cases {
        let end = effective_content_end(line);
        let content = &line[..end];
        let tokens = tokenize(line);
        // All token texts concatenated with single spaces should be
        // within the effective content (no token from comment tail).
        for tok in tokens {
            assert!(
                content.contains(&tok) || content == tok,
                "token {tok:?} must be within effective content {content:?} for line {line:?}"
            );
            assert!(
                !tok.contains('#') || tok.contains('"') || tok.contains('\''),
                "unquoted '#' must not appear inside token {tok:?}"
            );
        }
    }
}

#[test]
fn test_quote_state_walk_vs_tokenize_agree_on_string_boundaries() {
    // walk_structure must not emit braces inside quoted strings that
    // tokenize also treats as inside a single token.
    // Use a CLOSED single-quoted string on line 1 so the brace on line 2
    // is outside any string; this verifies that inert braces (line 0) and
    // real braces (line 2) are distinguished correctly.
    let doc = ":put \"{\" # comment { still\n'closed { inert'\n:do { real brace }\n";
    let mut braces = Vec::new();
    walk_structure(doc, |ev| match ev {
        StructureEvent::OpenBrace { line, character } => braces.push((line, character, '{')),
        StructureEvent::CloseBrace { line, character } => braces.push((line, character, '}')),
        _ => {}
    });
    // The '{' inside ":put \"{\"" on line 0 is inert, so first brace is
    // the real ":do {".
    assert!(
        braces.iter().any(|&(l, _, c)| l == 2 && c == '{'),
        "real brace on line 2 must be reported, got {braces:?}"
    );
    assert!(
        !braces.iter().any(|&(l, _, _)| l == 0),
        "brace inside quoted string on line 0 must be inert, got {braces:?}"
    );
    // Continuation: an unterminated single-quoted string DOES carry across
    // lines — the brace on the following line stays inert.
    let doc2 = ":put \"{\" # comment { still\n'open single { inert\n:do { still inert }\n";
    let mut braces2 = Vec::new();
    walk_structure(doc2, |ev| match ev {
        StructureEvent::OpenBrace { line, character } => braces2.push((line, character, '{')),
        StructureEvent::CloseBrace { line, character } => braces2.push((line, character, '}')),
        _ => {}
    });
    assert!(
        braces2.is_empty(),
        "unterminated single quote must keep following brace inert, got {braces2:?}"
    );
    // Ensure QuoteState reset_line preserves string across lines.
    let line0 = r#"comment="a \"b\" c""#;
    let end = effective_content_end(line0);
    assert_eq!(end, line0.len());
    let tok = tokenize(line0);
    assert_eq!(tok.len(), 1);
}

#[test]
fn test_quote_state_escape_inside_quotes_inert() {
    // Backslash inside quotes escapes next byte — it must not close the string.
    let doc = r#":put "a\"b{"
:put "c\\d"
"#;
    let mut events = Vec::new();
    walk_structure(doc, |ev| events.push(ev));
    // No brace inside the first quoted string should be reported
    // because the escaped quote keeps the string open and the `{` stays inert.
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, StructureEvent::OpenBrace { line: 0, .. })),
        "escaped quote must keep string open, events: {events:?}"
    );
    // Second line's braces? none
    assert!(
        events.is_empty()
            || !events
                .iter()
                .any(|ev| matches!(ev, StructureEvent::UnterminatedQuote { .. }))
    );
}

// ── ParseCache ────────────────────────────────────────────────

fn cache_texts(cache: &ParseCache, uri: &str, doc: &str) -> Option<Vec<String>> {
    cache
        .lookup(uri, doc)
        .map(|logicals| logicals.iter().map(|ll| ll.text().to_string()).collect())
}

#[test]
fn test_parse_cache_cold_miss_then_warm_hit_matches_fresh_join() {
    let mut cache = ParseCache::new();
    let uri = "file:///cache.rsc";
    let doc = "/ip/address add \\\naddress=1.2.3.4\n:local x\n";
    // Cold: no entry yet.
    assert!(cache.lookup(uri, doc).is_none());
    // First access parses and stores…
    let warm: Vec<String> = cache
        .lookup_or_insert(uri, doc)
        .iter()
        .map(|ll| ll.text().to_string())
        .collect();
    // …and the warm result is byte-identical to a fresh join, so every
    // consumer (diagnostics, completions) observes identical input.
    let fresh: Vec<String> = crate::diagnostics::logical_lines(doc)
        .iter()
        .map(|ll| ll.text().to_string())
        .collect();
    assert_eq!(warm, fresh);
    assert_eq!(cache_texts(&cache, uri, doc), Some(fresh));
}

#[test]
fn test_parse_cache_no_reparse_on_repeat_lookup_or_insert() {
    // Repeat access with unchanged text must reuse the stored vector
    // (no reparse): both slices point at the same allocation, and the
    // content stays identical to a fresh join (join semantics unchanged).
    let mut cache = ParseCache::new();
    let uri = "file:///cache-repeat.rsc";
    let doc = "/ip/address add \\\naddress=1.2.3.4\n";
    let first_ptr = {
        let slice = cache.lookup_or_insert(uri, doc);
        assert!(!slice.is_empty());
        slice.as_ptr()
    };
    let second_ptr = cache.lookup_or_insert(uri, doc).as_ptr();
    assert_eq!(
        first_ptr, second_ptr,
        "repeat lookup_or_insert must not reparse"
    );
    assert!(cache.lookup(uri, doc).is_some());
    let fresh: Vec<String> = crate::diagnostics::logical_lines(doc)
        .iter()
        .map(|ll| ll.text().to_string())
        .collect();
    assert_eq!(cache_texts(&cache, uri, doc), Some(fresh));
    // Same-length edit still misses (hash differs) and reparses.
    let edited = "/ip/address add \\\naddress=9.9.9.9\n";
    assert_eq!(edited.len(), doc.len());
    assert!(cache.lookup(uri, edited).is_none());
    let third_ptr = cache.lookup_or_insert(uri, edited).as_ptr();
    assert_ne!(
        first_ptr, third_ptr,
        "edited text must reparse into a new allocation"
    );
}
