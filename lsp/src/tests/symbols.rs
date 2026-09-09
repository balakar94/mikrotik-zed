// White-box: symbols (symbols).
use crate::symbols::*;

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
[[menus]]
path = "/tool"
type = "Directory"
[[menus]]
path = "/tool/fetch"
type = "Command"
"#,
    )
}

fn kinds(syms: &[DocumentSymbol]) -> Vec<i32> {
    syms.iter().map(|s| s.kind).collect()
}

fn names(syms: &[DocumentSymbol]) -> Vec<&str> {
    syms.iter().map(|s| s.name.as_str()).collect()
}

#[test]
fn test_menu_global_local_mix() {
    let doc = concat!(
        "/ip/address add address=1.2.3.4\n",
        ":global backupName \"b\"\n",
        ":local i\n",
        ":put done\n",
        "print\n",            // bare fragment — skipped
        "# just a comment\n", // comment — skipped
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(
        names(&syms),
        vec!["/ip/address add", "backupName", "i", ":put"]
    );
    assert_eq!(kinds(&syms), vec![19, 13, 13, 12]);
}

#[test]
fn test_menu_name_is_verbatim_substring_including_submenu_segments() {
    // "/tool fetch add": "/tool" is a path, "fetch" resolves as a known
    // child of /tool, "add" is the verb → name covers all three, exactly
    // as written (spaces preserved).
    let doc = "/tool fetch add url=http://x\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec!["/tool fetch add"]);
    assert_eq!(kinds(&syms), vec![19]);
    // selectionRange covers the FIRST path token "/tool".
    assert_eq!(syms[0].selection_range.start.line, 0);
    assert_eq!(syms[0].selection_range.start.character, 0);
    assert_eq!(syms[0].selection_range.end.character, 5);
}

#[test]
fn test_selection_range_covers_first_path_token() {
    let doc = "/ip/address add";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].selection_range.start.character, 0);
    assert_eq!(
        syms[0].selection_range.end.character,
        "/ip/address".len() as u32
    );
    assert_eq!(syms[0].selection_range.end.line, 0);
}

#[test]
fn test_continuation_line_yields_single_symbol_spanning_physical_lines() {
    let doc = "/ip/address add \\\naddress=1.2.3.4\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(syms.len(), 1, "continuation joins into ONE logical command");
    assert_eq!(syms[0].name, "/ip/address add");
    // Physical span crosses the continuation: starts line 0, ends line 1
    // at the end of "address=1.2.3.4".
    assert_eq!(syms[0].range.start.line, 0);
    assert_eq!(syms[0].range.end.line, 1);
    assert_eq!(syms[0].range.end.character, "address=1.2.3.4".len() as u32);
}

#[test]
fn test_local_with_inline_value_names_identifier_only() {
    let doc = ":local x=1\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec!["x"]);
    assert_eq!(kinds(&syms), vec![13]);
}

#[test]
fn test_bare_declaration_without_identifier_is_skipped() {
    let doc = ":global\n:put ok\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec![":put"]);
}

#[test]
fn test_braces_inside_quotes_do_not_confuse_classification() {
    // The quoted value contains '{'; classification must still be driven
    // by the first token only.
    let doc = ":put \"}{\"\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec![":put"]);
}

#[test]
fn test_empty_document_yields_empty_list() {
    assert!(compute_document_symbols(&synthetic_data(), "").is_empty());
    assert!(compute_document_symbols(&synthetic_data(), "\n\n  \n").is_empty());
}

#[test]
fn test_root_slash_and_property_fragments_are_skipped() {
    let doc = "/\nchain=input\naddress=\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert!(syms.is_empty(), "got {:?}", names(&syms));
}

#[test]
fn test_symbol_cap_bounds_output() {
    let doc = ":put x\n".repeat(MAX_SYMBOLS + 100);
    let syms = compute_document_symbols(&synthetic_data(), &doc);
    assert_eq!(syms.len(), MAX_SYMBOLS);
}

#[test]
fn test_symbols_serialize_to_lsp_wire_shape() {
    let syms = compute_document_symbols(&synthetic_data(), "/ip/address add");
    let v = serde_json::to_value(&syms).unwrap();
    let s = &v[0];
    assert_eq!(s["name"], "/ip/address add");
    assert_eq!(s["kind"], 19);
    assert!(s["range"]["start"]["line"].is_u64());
    assert!(s["selectionRange"]["start"]["character"].is_u64());
    assert!(s.get("children").is_none(), "flat symbols have no children");
    assert!(s.get("detail").is_none());
}

// ── Run collapsing + detail ──────────────────────────────────────────────

#[test]
fn test_consecutive_identical_menu_lines_collapse_with_count() {
    let doc = concat!(
        "/ip/address add address=1.1.1.1\n",
        "/ip/address add address=2.2.2.2\n",
        "/ip/address add address=3.3.3.3\n",
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(syms.len(), 1, "three identical runs become ONE symbol");
    assert_eq!(syms[0].name, "/ip/address add (×3)");
    assert_eq!(syms[0].kind, 19);
    // Range spans the run: starts line 0, ends line 2.
    assert_eq!(syms[0].range.start.line, 0);
    assert_eq!(syms[0].range.end.line, 2);
    // selectionRange still covers the first path token of line 0.
    assert_eq!(syms[0].selection_range.start.line, 0);
}

#[test]
fn test_different_verbs_break_the_run() {
    let doc = concat!(
        "/ip/address add address=1.1.1.1\n",
        "/ip/address add address=2.2.2.2\n",
        "/ip/address print\n",
        "/ip/address add address=3.3.3.3\n",
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(
        names(&syms),
        vec![
            "/ip/address add (×2)",
            "/ip/address print",
            "/ip/address add",
        ]
    );
}

#[test]
fn test_comment_or_blank_breaks_the_run() {
    // A comment line between two identical commands breaks adjacency.
    let doc = concat!(
        "/ip/address add address=1.1.1.1\n",
        "# separator note\n",
        "/ip/address add address=2.2.2.2\n",
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec!["/ip/address add", "/ip/address add"]);

    // A blank line breaks it too.
    let doc = "/ip/address add address=1.1.1.1\n\n/ip/address add address=2.2.2.2\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec!["/ip/address add", "/ip/address add"]);
}

#[test]
fn test_variables_never_collapse() {
    let doc = ":local i\n:local i\n:local i\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(names(&syms), vec!["i", "i", "i"]);
    assert_eq!(kinds(&syms), vec![13, 13, 13]);
}

#[test]
fn test_detail_prefers_comment_then_distinguishing_prop() {
    // comment= wins over everything.
    let doc = "/ip/address add address=1.1.1.1 comment=\"wan link\"\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(syms[0].detail.as_deref(), Some("comment=\"wan link\""));

    // No comment: first distinguishing prop (interface before address).
    let doc = "/ip/address add address=1.1.1.1 interface=ether1\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(
        syms[0].detail.as_deref(),
        Some("address=1.1.1.1"),
        "first distinguishing prop in token order wins"
    );

    // No property at all: no detail on the wire.
    let doc = "/ip/address print\n";
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert!(syms[0].detail.is_none());
    let v = serde_json::to_value(&syms).unwrap();
    assert!(v[0].get("detail").is_none());
}

#[test]
fn test_collapsed_run_reuses_first_line_detail() {
    let doc = concat!(
        "/ip/address add address=1.1.1.1 comment=\"first\"\n",
        "/ip/address add address=2.2.2.2 comment=\"second\"\n",
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "/ip/address add (×2)");
    assert_eq!(syms[0].detail.as_deref(), Some("comment=\"first\""));
}

#[test]
fn test_collapsed_run_detail_serializes_on_wire() {
    let doc = concat!(
        "/ip/address add address=1.1.1.1\n",
        "/ip/address add address=2.2.2.2\n",
    );
    let syms = compute_document_symbols(&synthetic_data(), doc);
    let v = serde_json::to_value(&syms).unwrap();
    assert_eq!(v[0]["name"], "/ip/address add (×2)");
    assert_eq!(v[0]["detail"], "address=1.1.1.1");
}

// ── F9: detail sanitization ──────────────────────────────────────────────

#[test]
fn test_detail_strips_newlines_and_caps_length() {
    // Embedded CR/LF in a comment value collapses to spaces (single-line).
    assert_eq!(
        sanitize_symbol_detail("comment=\"a\nb\rc\""),
        "comment=\"a b c\""
    );
    assert!(!sanitize_symbol_detail("comment=\"a\rb\"").contains('\r'));
    // 256-char cap with trailing ellipsis.
    let long = "comment=".to_string() + &"x".repeat(400);
    let out = sanitize_symbol_detail(&long);
    assert_eq!(out.chars().count(), 257);
    assert!(out.ends_with('…'));
    // Normal inputs pass through unchanged.
    assert_eq!(
        sanitize_symbol_detail("comment=\"wan link\""),
        "comment=\"wan link\""
    );
}

#[test]
fn test_menu_detail_comment_with_newline_is_single_line() {
    // A comment property carrying a newline yields single-line detail.
    let tokens = vec![crate::parser::SpanToken {
        text: "comment=\"line1\nline2\"".to_string(),
        start: 0,
        end: 20,
    }];
    let detail = menu_detail(&tokens).expect("comment detail");
    assert!(!detail.contains('\n'));
    assert!(detail.contains("comment="));
}
