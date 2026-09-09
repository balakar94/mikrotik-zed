// ── Document symbols (run collapsing + detail) ──────────────────
//
// textDocument/documentSymbol support. Emits a FLAT list of document
// symbols (no `children`) — flat sidesteps the LSP rule that a parent's
// range must contain every child range, which RouterOS scripts violate
// routinely (blocks are brace-based, not range-nested).
//
// Provider precedence: when the client supports LSP documentSymbol, the
// server list below WINS over the Tree-sitter fallback
// (`languages/rsc/outline.scm`). The fallback exists for editors without a
// running `rsc-ls`; both providers agree on one row per command plus
// keyword/variable rows, and on kinds (Variable 13 vs Function 12, Object
// 19 for menu commands). The query file documents the same contract.
//
// Classification per logical line (`diagnostics::logical_lines`, so `\`
// continuations are joined before inspection):
// - leading `/…` token   → menu command: SymbolKind.Object (19), named by
//   the path + verb substring exactly as written ("/tool fetch add").
//   CONSECUTIVE logical lines with the IDENTICAL path + verb collapse into
//   ONE symbol named "<path verb> (×N)" spanning the whole run (first line
//   start to last line end). This keeps firewall rule dumps readable:
//   fourteen `/ip/firewall/filter add …` lines surface as one
//   "/ip/firewall/filter add (×14)" landmark instead of fourteen rows.
//   Only strictly adjacent command lines merge: a blank line, a comment,
//   or any other statement between them breaks the run. `:local`/`:global`
//   and `:verb` rows never merge and always break a run.
// - `:local` / `:global` → SymbolKind.Variable (13), named by the variable
//   identifier token that follows. Each declaration stays an INDIVIDUAL
//   landmark — never collapsed — so rename targets remain addressable.
// - any other `:verb`    → SymbolKind.Function (12), named by the verb.
// - anything else (bare values, property fragments, `#` comments) is
//   skipped.
//
// `detail` disambiguates sibling rows: `comment=<value>` when the line
// carries a comment property, else the first distinguishing property in
// token order among interface/chain/address/name/action, else the first
// property overall. Collapsed runs reuse the FIRST line's detail. `detail`
// is `None` (omitted on the wire) when the line carries no property.
// Serialized shape matches LSP `DocumentSymbol`; `children`/`tags` are
// omitted.
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
pub(crate) const MAX_SYMBOLS: usize = 5000;

/// One flat document symbol in INTERNAL byte coordinates.
///
/// Serialized shape matches LSP `DocumentSymbol`; optional fields (tags,
/// children) are omitted. `detail` is omitted when `None`. Ranges must
/// still be converted to the negotiated wire encoding before
/// serialization — see [`compute_document_symbols`].
#[derive(Debug, serde::Serialize)]
pub(crate) struct DocumentSymbol {
    pub name: String,
    pub kind: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub range: diagnostics::Range,
    #[serde(rename = "selectionRange")]
    pub selection_range: diagnostics::Range,
}

/// Property keys preferred for `detail`, in priority order. `comment` is
/// handled first unconditionally; the rest disambiguate sibling rows
/// (`interface=`, `chain=`, `address=` cover the common firewall/address
/// dumps).
const DETAIL_PREFERRED_KEYS: &[&str] = &["interface", "chain", "address", "name", "action"];

/// Max chars for a symbol `detail` string (mirrors completion's
/// `MAX_DETAIL_CHARS` semantics; kept local to avoid cross-module coupling).
const MAX_SYMBOL_DETAIL_CHARS: usize = 256;

/// Single-line detail scrub for symbol `detail` strings (F9).
///
/// Each run of ASCII controls (including `\r`, `\n`, `\t`) becomes one
/// space, then the result is capped at [`MAX_SYMBOL_DETAIL_CHARS`] chars
/// with a trailing `…`. Applied to every `menu_detail` return so a
/// `comment="line1\nline2..."` value can never break the outline layout or
/// smuggle multiline content onto the wire. Normal inputs pass through
/// unchanged.
pub(crate) fn sanitize_symbol_detail(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_gap = false;
    for ch in s.chars() {
        if ch.is_ascii_control() {
            if !in_gap {
                out.push(' ');
                in_gap = true;
            }
        } else {
            in_gap = false;
            out.push(ch);
        }
    }
    let trimmed = out.trim().to_string();
    if trimmed.chars().count() <= MAX_SYMBOL_DETAIL_CHARS {
        trimmed
    } else {
        let kept: String = trimmed.chars().take(MAX_SYMBOL_DETAIL_CHARS).collect();
        format!("{kept}…")
    }
}

/// Extract the `detail` string for a menu-command line from its tokens.
///
/// - `comment=<value>` when any token carries a `comment` property (value
///   kept verbatim, quotes included: `comment="allow dns"`).
/// - else the first property in TOKEN order whose key is in
///   [`DETAIL_PREFERRED_KEYS`] (`chain=input`, `interface=ether1`, …).
/// - else the first property overall (`key=value` verbatim).
/// - else `None` (no property on the line).
pub(crate) fn menu_detail(tokens: &[crate::parser::SpanToken]) -> Option<String> {
    let mut first_overall: Option<String> = None;
    let mut first_preferred: Option<String> = None;
    for tok in tokens {
        let Some((key, _)) = crate::parser::split_key_value(&tok.text) else {
            continue;
        };
        if key == "comment" {
            // F9: comment values are attacker-influenced device text — scrub
            // newlines/controls and cap length before emitting on the wire.
            return Some(sanitize_symbol_detail(&tok.text));
        }
        if first_overall.is_none() {
            first_overall = Some(tok.text.clone());
        }
        if first_preferred.is_none() && DETAIL_PREFERRED_KEYS.contains(&key) {
            first_preferred = Some(tok.text.clone());
        }
    }
    first_preferred
        .or(first_overall)
        .map(|d| sanitize_symbol_detail(&d))
}

/// One classified menu-command line, before run collapsing.
pub(crate) struct MenuEntry {
    /// Verbatim path + verb substring ("exactly as written").
    name: String,
    detail: Option<String>,
    range: diagnostics::Range,
    selection_range: diagnostics::Range,
}

/// Compute the flat document-symbol list for a script document.
///
/// Pure function over (menu data, document text); deterministic order —
/// symbols appear in document order. An empty document yields an empty list.
/// Consecutive identical menu path + verb lines collapse into one
/// `(×N)` symbol; `:local`/`:global` declarations stay individual.
pub(crate) fn compute_document_symbols(data: &MenuData, doc: &str) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    // Pending run of identical menu names: (entry of first line, count,
    // end range of last line). Flushed when a different name, a
    // non-menu symbol, or a skipped line breaks adjacency.
    let mut pending: Option<(MenuEntry, usize, diagnostics::Range)> = None;

    // Flush helper: materialize the pending run as one symbol if capacity
    // remains. Collapsed names append " (×N)"; single lines keep the bare
    // name. Range spans first-line start to last-line end.
    let flush = |pending: &mut Option<(MenuEntry, usize, diagnostics::Range)>,
                 symbols: &mut Vec<DocumentSymbol>| {
        let Some((first, count, last_range)) = pending.take() else {
            return;
        };
        if symbols.len() >= MAX_SYMBOLS {
            return;
        }
        if count > 1 {
            symbols.push(DocumentSymbol {
                name: format!("{} (×{})", first.name, count),
                kind: symbol_kind::OBJECT,
                detail: first.detail,
                range: diagnostics::Range {
                    start: first.range.start,
                    end: last_range.end,
                },
                selection_range: first.selection_range,
            });
        } else {
            symbols.push(DocumentSymbol {
                name: first.name,
                kind: symbol_kind::OBJECT,
                detail: first.detail,
                range: first.range,
                selection_range: first.selection_range,
            });
        }
    };

    for line in diagnostics::logical_lines(doc) {
        if symbols.len() >= MAX_SYMBOLS {
            break;
        }
        let tokens = tokenize_with_spans(line.text());
        let Some(first) = tokens.first() else {
            // Blank logical line breaks a run (strict adjacency).
            flush(&mut pending, &mut symbols);
            continue; // blank logical line
        };

        // Whole-line physical span: from the very start of the joined text
        // to its end, mapped onto original physical lines by the segment
        // table. This is the `range` every symbol variant reports.
        let span = line.map_range(0, line.text().len());

        if first.text.starts_with('/') {
            // Root "/" alone is a navigation fragment, not a command — skip.
            if first.text == "/" {
                flush(&mut pending, &mut symbols);
                continue;
            }
            if let Some(entry) = menu_command_entry(data, &line, &tokens, span) {
                match pending.take() {
                    Some((prev, count, last_range)) if prev.name == entry.name => {
                        // Extend the run: keep first entry, bump count, move
                        // the end edge to this line's end.
                        pending = Some((prev, count + 1, entry.range));
                        let _ = last_range;
                    }
                    Some((prev, count, last_range)) => {
                        // Different name: flush previous, start a new run.
                        let mut slot = Some((prev, count, last_range));
                        flush(&mut slot, &mut symbols);
                        debug_assert!(slot.is_none());
                        if symbols.len() < MAX_SYMBOLS {
                            let last = entry.range.clone();
                            pending = Some((entry, 1, last));
                        }
                    }
                    None => {
                        // No open run: start one only when capacity remains
                        // (mirrors the `< MAX_SYMBOLS` guard on the
                        // different-name arm above — starting a run that the
                        // final `flush` would drop is wasted work).
                        if symbols.len() < MAX_SYMBOLS {
                            let last = entry.range.clone();
                            pending = Some((entry, 1, last));
                        }
                    }
                }
                continue;
            }
            // Unclassifiable menu line breaks the run.
            flush(&mut pending, &mut symbols);
        } else if let Some(sym) = script_command_symbol(&line, &tokens, span) {
            // Script rows (Variable landmarks and :verb Functions) never
            // merge: flush any open run first. The shared
            // `declared_variable` primitive also catches brace-prefixed
            // declarations (`{ :local x }`), which land here even though
            // the logical line does not open with `:`.
            flush(&mut pending, &mut symbols);
            // Same cap as the loop-top break: the flush above may have just
            // filled the last slot, so re-check before pushing.
            if symbols.len() < MAX_SYMBOLS {
                symbols.push(sym);
            }
        } else {
            // Everything else (bare values, lone properties, comments) is
            // not a statement — deliberately skipped, and breaks a run.
            flush(&mut pending, &mut symbols);
        }
    }
    flush(&mut pending, &mut symbols);

    symbols
}

/// Build the classified entry for a `/path … verb …` menu-command line.
///
/// Mirrors `parser::parse_line`'s submenu walk so symbol naming stays
/// consistent with completion behavior: leading slash-token starts the path,
/// subsequent tokens extend it while they name a known child menu, and the
/// first token that is neither extends the path nor carries `=` is the verb.
///
/// `name` is the original substring covering path + verb ("exactly as
/// written", preserving case and separators); `selectionRange` covers the
/// first path token; `detail` comes from [`menu_detail`].
fn menu_command_entry(
    data: &MenuData,
    line: &diagnostics::LogicalLine,
    tokens: &[crate::parser::SpanToken],
    span: diagnostics::Range,
) -> Option<MenuEntry> {
    let first = &tokens[0];
    let mut path_parts: Vec<String> = vec![first.text.trim_start_matches('/').to_string()];
    let mut tail_end = first.end; // end offset of the last path segment

    let mut depth: u32 = 0;
    {
        let (opens, _) = crate::parser::bracket_counts(&tokens[0].text);
        depth = depth.saturating_add(opens).min(32);
    }
    for tok in &tokens[1..] {
        let (opens, closes) = crate::parser::bracket_counts(&tok.text);
        // Bracket regions are inert: inner words never extend the head, not
        // even the token that closes the region.
        let was_inside = depth > 0 || opens > 0;
        depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
        if was_inside {
            continue;
        }
        // Properties (and a second absolute path) end the head of the command.
        if crate::parser::split_key_value(&tok.text).is_some() || tok.text.starts_with('/') {
            break;
        }
        // Expression debris with an outside-quote `=` is not a verb either.
        if tok.text.contains('=') {
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
    let detail = menu_detail(tokens);

    Some(MenuEntry {
        name,
        detail,
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
/// what a declaration is and where its identifier spans. The delegation
/// also inherits the leading-separator tolerance (`{ :local x }`,
/// `;`-separated tails): a declaration after `{`/`}`/`;` separators still
/// yields a Variable landmark. Note this narrows `selectionRange` of
/// inline-valued locals (`:local x=1`) from the whole `x=1` token down to
/// exactly `x` — the identifier a rename would target.
fn script_command_symbol(
    line: &diagnostics::LogicalLine,
    tokens: &[crate::parser::SpanToken],
    span: diagnostics::Range,
) -> Option<DocumentSymbol> {
    // Declarations first (position-independent via the shared primitive):
    // a brace- or semicolon-prefixed `:local x` still landmarks `x`.
    if let Some((_kind, ident, ident_start, ident_end)) =
        crate::navigation::declared_variable(tokens)
    {
        // Delegation preserves the historical contract: a declaration line
        // without a bare identifier (`:global` alone) yields NO symbol, it
        // does NOT degrade into a Function entry.
        return Some(DocumentSymbol {
            name: ident,
            kind: symbol_kind::VARIABLE,
            detail: None,
            range: span,
            selection_range: line.map_range(ident_start, ident_end),
        });
    }
    let first = &tokens[0];
    // Historical contract: a bare `:local` / `:global` without an
    // identifier yields NO symbol — it must NOT degrade into a Function
    // entry for the command word itself. (Declaration success returned
    // above; reaching here means no identifier followed.)
    if first.text == ":local" || first.text == ":global" {
        return None;
    }
    if !first.text.starts_with(':') {
        return None;
    }

    Some(DocumentSymbol {
        name: first.text.clone(),
        kind: symbol_kind::FUNCTION,
        detail: None,
        range: span,
        selection_range: line.map_range(first.start, first.end),
    })
}
