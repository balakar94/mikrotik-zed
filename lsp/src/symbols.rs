// ── Document symbols (Stage B) ────────────────────────────────
//
// textDocument/documentSymbol support. Emits a FLAT list of document
// symbols (no `children`) — flat sidesteps the LSP rule that a parent's
// range must contain every child range, which RouterOS scripts violate
// routinely (blocks are brace-based, not range-nested).
//
// Classification per logical line (`diagnostics::logical_lines`, so `\`
// continuations are joined before inspection):
// - leading `/…` token   → menu command: SymbolKind.Object (19), named by
//   the path + verb substring exactly as written ("/tool fetch add").
// - `:local` / `:global` → SymbolKind.Variable (13), named by the variable
//   identifier token that follows.
// - any other `:verb`    → SymbolKind.Function (12), named by the verb.
// - anything else (bare values, property fragments, `#` comments) is
//   skipped.
//
// All ranges are computed in internal byte coordinates against physical
// document lines; the protocol boundary (main.rs) converts characters to
// the negotiated position encoding, exactly like diagnostics do.

use crate::diagnostics::{self};
use crate::menus::MenuData;
use crate::parser::tokenize_with_spans;

/// LSP DocumentSymbolKind values used here (mirrors the LSP spec).
mod symbol_kind {
    pub const FUNCTION: i32 = 12;
    pub const VARIABLE: i32 = 13;
    pub const OBJECT: i32 = 19;
}

/// Defensive cap on emitted symbols: documents are already capped at 5 MiB,
/// but pathological generated files could still yield hundreds of thousands
/// of one-line commands. Beyond the cap, remaining lines are not classified.
const MAX_SYMBOLS: usize = 5000;

/// One flat document symbol in INTERNAL byte coordinates.
///
/// Serialized shape matches LSP `DocumentSymbol`; optional fields (detail,
/// tags, children) are omitted. Ranges must still be converted to the
/// negotiated wire encoding before serialization — see [`compute_document_symbols`].
#[derive(Debug, serde::Serialize)]
pub(crate) struct DocumentSymbol {
    pub name: String,
    pub kind: i32,
    pub range: diagnostics::Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: diagnostics::Range,
}

/// Compute the flat document-symbol list for a script document.
///
/// Pure function over (menu data, document text); deterministic order —
/// symbols appear in document order. An empty document yields an empty list.
pub(crate) fn compute_document_symbols(data: &MenuData, doc: &str) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();

    for line in diagnostics::logical_lines(doc) {
        if symbols.len() >= MAX_SYMBOLS {
            break;
        }
        let tokens = tokenize_with_spans(line.text());
        let Some(first) = tokens.first() else {
            continue; // blank logical line
        };

        // Whole-line physical span: from the very start of the joined text
        // to its end, mapped onto original physical lines by the segment
        // table. This is the `range` every symbol variant reports.
        let span = line.map_range(0, line.text().len());

        if first.text.starts_with('/') {
            // Root "/" alone is a navigation fragment, not a command — skip.
            if first.text == "/" {
                continue;
            }
            if let Some(sym) = menu_command_symbol(data, &line, &tokens, span) {
                symbols.push(sym);
            }
        } else if first.text.starts_with(':')
            && let Some(sym) = script_command_symbol(&line, &tokens, span)
        {
            symbols.push(sym);
        }
        // Everything else (bare values, lone properties, comments) is not a
        // statement — deliberately skipped.
    }

    symbols
}

/// Build the symbol for a `/path … verb …` menu-command line.
///
/// Mirrors `parser::parse_line`'s submenu walk so symbol naming stays
/// consistent with completion behavior: leading slash-token starts the path,
/// subsequent tokens extend it while they name a known child menu, and the
/// first token that is neither extends the path nor carries `=` is the verb.
///
/// `name` is the original substring covering path + verb ("exactly as
/// written", preserving case and separators); `selectionRange` covers the
/// first path token.
fn menu_command_symbol(
    data: &MenuData,
    line: &diagnostics::LogicalLine,
    tokens: &[crate::parser::SpanToken],
    span: diagnostics::Range,
) -> Option<DocumentSymbol> {
    let first = &tokens[0];
    let mut path_parts: Vec<String> = vec![first.text.trim_start_matches('/').to_string()];
    let mut tail_end = first.end; // end offset of the last path segment

    for tok in &tokens[1..] {
        // Properties (and a second absolute path) end the head of the command.
        if tok.text.contains('=') || tok.text.starts_with('/') {
            break;
        }
        let current_path = format!("/{}", path_parts.join("/"));
        let is_sub_menu = data
            .child_names_by_parent
            .get(&current_path)
            .map(|children| children.iter().any(|c| c.name == tok.text))
            .unwrap_or(false);
        if is_sub_menu {
            path_parts.push(tok.text.clone());
            tail_end = tok.end;
        } else {
            // First non-menu token is the verb; it belongs to the name.
            tail_end = tok.end;
            break;
        }
    }

    // Byte offsets are pre-clamped by construction (tokenizer emits in-range
    // spans over the same text), but floor defensively anyway.
    let start = crate::floor_char_boundary(line.text(), first.start);
    let end = crate::floor_char_boundary(line.text(), tail_end);
    let name = line.text()[start..end].to_string();

    Some(DocumentSymbol {
        name,
        kind: symbol_kind::OBJECT,
        range: span,
        selection_range: line.map_range(first.start, first.end),
    })
}

/// Build the symbol for a `:verb …` script-command line.
///
/// `:local` / `:global` declarations become Variables named by the variable
/// identifier token (text before a possible `=`); all other verbs become
/// Functions named by the verb itself (":put"). Returns `None` for
/// declarations without an identifier token.
///
/// Declaration naming is delegated to [`crate::navigation::declared_variable`]
/// so documentSymbol and go-to-definition/references share ONE notion of
/// what a declaration is and where its identifier spans. Note this narrows
/// `selectionRange` of inline-valued locals (`:local x=1`) from the whole
/// `x=1` token down to exactly `x` — the identifier a rename would target.
fn script_command_symbol(
    line: &diagnostics::LogicalLine,
    tokens: &[crate::parser::SpanToken],
    span: diagnostics::Range,
) -> Option<DocumentSymbol> {
    let first = &tokens[0];
    if first.text == ":local" || first.text == ":global" {
        // Delegation preserves the historical contract: a declaration line
        // without a bare identifier (`:global` alone) yields NO symbol, it
        // does NOT degrade into a Function entry.
        let (_kind, ident, ident_start, ident_end) = crate::navigation::declared_variable(tokens)?;
        return Some(DocumentSymbol {
            name: ident,
            kind: symbol_kind::VARIABLE,
            range: span,
            selection_range: line.map_range(ident_start, ident_end),
        });
    }

    Some(DocumentSymbol {
        name: first.text.clone(),
        kind: symbol_kind::FUNCTION,
        range: span,
        selection_range: line.map_range(first.start, first.end),
    })
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
}
