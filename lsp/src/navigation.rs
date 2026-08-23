// ── Variable navigation (textDocument/definition + references) ──
//
// Pure computation module for RouterOS script variable navigation, in the
// house style of `signature.rs`: no I/O, deterministic output, unit-tested
// in place.
//
// V1 SEMANTICS (deliberately simple, honest about limits):
//
// - Declarations are the identifier token immediately following a `:local`
//   or `:global` COMMAND token on a logical line whose FIRST token is that
//   command (same rule documentSymbol uses, so the outline and navigation
//   can never disagree). Inline values (`:local x=1`) belong to the
//   identifier only up to the `=`.
// - Usages are bare `$name` references anywhere else in the document. The
//   scanner is quote-aware (`$` inside `"…"`/`'…'` is literal text), stops
//   at unquoted `#` comments, and treats a doubled `$$` as a literal dollar
//   rather than a reference start. `$()` expression syntax and quoted
//   identifier names (`$"my var"`) are out of scope for v1.
// - Definition lookup from any occurrence of a name uses ONE total,
//   deterministic rule (see [`choose_definition`]).
// - References are every `$usage` of the name plus — when the client asks
//   with `includeDeclaration` — the SAME declaration go-to-definition would
//   choose from the request position. Results cap at [`MAX_REFERENCES`].
//
// Documented v1 limitations: no cross-file resolution; no block-scope
// precision (a `:local` inside `{ … }` is treated as visible to the whole
// document; the position rule below is what keeps answers stable); hyphenated
// or otherwise non-`[A-Za-z0-9_]` names are not tracked.
//
// All positions handled here are LOGICAL coordinates: byte offsets within a
// joined logical line's text plus that line's index in the joined vector.
// Mapping to physical document coordinates is done by the caller through
// `diagnostics::LogicalLine::map_range`, and wire-encoding conversion stays
// at the protocol boundary (`encoding.rs`) exactly as everywhere else.

use crate::diagnostics::LogicalLine;
use crate::hover;
use crate::parser::{SpanToken, tokenize_with_spans};

/// Cap on one `textDocument/references` result list.
///
/// Bounds the response payload for adversarial documents (thousands of
/// `$x` lines); beyond the cap the tail is silently dropped — same
/// defensive posture as the diagnostics/symbol caps.
pub(crate) const MAX_REFERENCES: usize = 1000;

/// Which script command declared a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclKind {
    Local,
    Global,
}

/// What kind of variable occurrence a [`VariableHit`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HitKind {
    Declaration(DeclKind),
    Usage,
}

/// One variable occurrence located in LOGICAL coordinates.
///
/// `start`/`end` are byte offsets within the joined text of logical line
/// `logical_line`, covering ONLY the identifier — never the leading `$`,
/// never an inline `=value`, never the `:local`/`:global` command token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VariableHit {
    pub name: String,
    pub kind: HitKind,
    pub logical_line: usize,
    pub start: usize,
    pub end: usize,
}

/// Bytes permitted in a bare variable identifier (v1).
///
/// RouterOS identifiers are letters, digits and underscores; `-` is
/// deliberately excluded so arithmetic like `($count-1)` cannot glue a
/// fake name onto a real usage.
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Extract the declaration introduced by a `:local` / `:global` command line.
///
/// Returns `(kind, identifier, ident_start, ident_end)` where the offsets
/// locate ONLY the identifier inside the tokenized text. Shared primitive
/// for documentSymbol naming (`symbols.rs`) and the navigation index, so
/// the outline and go-to-definition can never disagree about what a
/// declaration is or where its name spans. Returns `None` when the line
/// does not open with either command or carries no bare identifier token
/// (`:global` alone, quoted names).
pub(crate) fn declared_variable(tokens: &[SpanToken]) -> Option<(DeclKind, String, usize, usize)> {
    let first = tokens.first()?;
    let kind = match first.text.as_str() {
        ":local" => DeclKind::Local,
        ":global" => DeclKind::Global,
        _ => return None,
    };
    // The identifier is the token immediately following the command token…
    let var = tokens.get(1)?;
    // …restricted to its bare-identifier prefix: `:local x=1` tokenizes as
    // one "x=1" token and the declaration owns only `x`.
    let end = var.text.bytes().take_while(|&b| is_ident_char(b)).count();
    if end == 0 {
        return None; // quoted / decorated names are unsupported in v1
    }
    Some((
        kind,
        var.text[..end].to_string(),
        var.start,
        var.start + end,
    ))
}

/// Scan ONE logical line's text and push every `$name` usage into `hits`.
///
/// Quote state machine mirrors `parser::scan_token` / `walk_structure`:
/// both quote styles toggle symmetrically, `\` escapes the next byte only
/// INSIDE quotes, and an unquoted `#` starts a comment running to the end
/// of the physical line. Because an unquoted `#` always prevents a `\`
/// continuation (the joiner cuts comment tails first), everything after it
/// in joined text is comment, so scanning can simply stop there.
///
/// A usage starts at a `$` outside quotes whose next byte begins an
/// identifier AND whose PREVIOUS byte is not another `$`: a doubled dollar
/// reads as a literal `$`, so `$$x` and `$$$x` produce nothing while
/// `$x ($y)` produces both names.
fn push_usages(text: &str, logical_line: usize, hits: &mut Vec<VariableHit>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut in_double = false;
    let mut in_single = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_double || in_single => {
                // Escaped byte inside quotes: skip it entirely. Clamp so a
                // trailing backslash cannot push past the buffer.
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' if !in_single => in_double = !in_double,
            b'\'' if !in_double => in_single = !in_single,
            b'#' if !in_double && !in_single => break, // comment tail
            b'$' if !in_double && !in_single => {
                let name_start = i + 1;
                let doubled = i > 0 && bytes[i - 1] == b'$';
                if !doubled && name_start < bytes.len() && is_ident_char(bytes[name_start]) {
                    let mut end = name_start;
                    while end < bytes.len() && is_ident_char(bytes[end]) {
                        end += 1;
                    }
                    hits.push(VariableHit {
                        name: text[name_start..end].to_string(),
                        kind: HitKind::Usage,
                        logical_line,
                        start: name_start,
                        end,
                    });
                    i = end;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Index every variable occurrence in the document, in document order.
///
/// One pass over the continuation-aware logical lines; each line is
/// classified once (declaration via its opening tokens, usages via the
/// quote-aware byte scan). Empty documents yield an empty index.
pub(crate) fn build_variable_index(logicals: &[LogicalLine]) -> Vec<VariableHit> {
    let mut hits = Vec::new();
    for (idx, ll) in logicals.iter().enumerate() {
        let tokens = tokenize_with_spans(ll.text());
        if let Some((kind, name, start, end)) = declared_variable(&tokens) {
            hits.push(VariableHit {
                name,
                kind: HitKind::Declaration(kind),
                logical_line: idx,
                start,
                end,
            });
        }
        push_usages(ll.text(), idx, &mut hits);
    }
    hits
}

/// Word under a cursor offset, extracted with hover's own helpers.
///
/// Reusing `hover::find_word_start/find_word_end` keeps go-to-definition,
/// find-references and hover in agreement about what "the word at the
/// cursor" means (same word-character set, same boundary clamping).
/// Callers apply it to the JOINED logical-line text; on non-split lines it
/// is byte-for-byte hover's behavior.
pub(crate) fn word_at(text: &str, offset: usize) -> &str {
    let start = hover::find_word_start(text, offset);
    let end = hover::find_word_end(text, offset);
    &text[start..end]
}

/// The indexed occurrence the cursor actually sits on, if any.
///
/// The hover-style `word` alone is not trusted: it must ALSO overlap a
/// real occurrence span of the same name, so a property that merely shares
/// a spelling with a variable never resolves. Matching tolerates the
/// cursor parked ON the end boundary of the identifier (hover's backward
/// extraction behaves the same way after a finished word); when several
/// occurrences touch at that boundary the first in document order wins.
/// A defensive leading `$` is stripped because some clients synthesize
/// positions with the sigil included in the word.
pub(crate) fn hit_at_cursor<'a>(
    index: &'a [VariableHit],
    word: &str,
    logical_line: usize,
    offset: usize,
) -> Option<&'a VariableHit> {
    let name = word.strip_prefix('$').unwrap_or(word);
    if name.is_empty() {
        return None;
    }
    index.iter().find(|h| {
        h.name == name && h.logical_line == logical_line && h.start <= offset && offset <= h.end
    })
}

/// THE deterministic definition-choice rule (v1, total over any input).
///
/// Among declarations sharing the requested name, ordered by document
/// position `(logical_line, start)`:
///
/// 1. Prefer the LAST declaration whose position is `<=` the requesting
///    occurrence's position — i.e. the closest preceding declaration, or
///    the declaration itself when the request originates from one.
/// 2. If NO declaration precedes the request, take the FIRST declaration
///    of that name regardless of kind — `:local` and `:global` NEVER break
///    ties, only document position does.
///
/// This models RouterOS's read-the-closest-previous-binding intuition
/// without block-scope analysis, and is stable because both the index and
/// the request position derive from the same logical-line join.
pub(crate) fn choose_definition<'a>(
    index: &'a [VariableHit],
    name: &str,
    request: (usize, usize),
) -> Option<&'a VariableHit> {
    let mut last_before: Option<&VariableHit> = None;
    let mut first_of_name: Option<&VariableHit> = None;
    for hit in index {
        if hit.name != name {
            continue;
        }
        let HitKind::Declaration(_) = hit.kind else {
            continue;
        };
        if first_of_name.is_none() {
            first_of_name = Some(hit);
        }
        if (hit.logical_line, hit.start) <= request {
            last_before = Some(hit);
        }
    }
    last_before.or(first_of_name)
}

/// Collect references for `name`: the chosen declaration first (only when
/// the caller resolved one, i.e. `includeDeclaration`), then every `$usage`
/// of the name in document order. Total results capped at
/// [`MAX_REFERENCES`] — the cap bounds the WHOLE flat list, declaration
/// included.
pub(crate) fn collect_references<'a>(
    index: &'a [VariableHit],
    name: &str,
    declaration: Option<&'a VariableHit>,
) -> Vec<&'a VariableHit> {
    let mut refs = Vec::new();
    if let Some(d) = declaration {
        refs.push(d);
    }
    refs.extend(
        index
            .iter()
            .filter(|h| h.name == name && h.kind == HitKind::Usage),
    );
    refs.truncate(MAX_REFERENCES);
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_of(doc: &str) -> Vec<VariableHit> {
        build_variable_index(&crate::diagnostics::logical_lines(doc))
    }

    fn summary(hits: &[VariableHit]) -> Vec<String> {
        hits.iter()
            .map(|h| {
                let kind = match h.kind {
                    HitKind::Declaration(DeclKind::Local) => ":local",
                    HitKind::Declaration(DeclKind::Global) => ":global",
                    HitKind::Usage => "$",
                };
                format!("{}:{}@{}", h.name, kind, h.logical_line)
            })
            .collect()
    }

    // ── Declaration extraction ────────────────────────────────────

    #[test]
    fn test_declared_variable_local_and_global_bare() {
        let local = tokenize_with_spans(":local counter");
        let (kind, name, s, e) = declared_variable(&local).unwrap();
        assert_eq!(kind, DeclKind::Local);
        assert_eq!(name, "counter");
        assert_eq!(&":local counter"[s..e], "counter");

        let global = tokenize_with_spans(":global g");
        let (kind, name, _, _) = declared_variable(&global).unwrap();
        assert_eq!(kind, DeclKind::Global);
        assert_eq!(name, "g");
    }

    #[test]
    fn test_declared_variable_inline_value_stays_outside_span() {
        let tokens = tokenize_with_spans(":local x=1");
        let (_, name, s, e) = declared_variable(&tokens).unwrap();
        assert_eq!(name, "x", "`:local x=1` declares only `x`");
        assert_eq!(&":local x=1"[s..e], "x");
    }

    #[test]
    fn test_declared_variable_requires_leading_command_and_identifier() {
        // Not a declaration opener at all…
        assert!(declared_variable(&tokenize_with_spans(":put $x")).is_none());
        // …command without identifier…
        assert!(declared_variable(&tokenize_with_spans(":global")).is_none());
        // …quoted identifier unsupported in v1…
        assert!(declared_variable(&tokenize_with_spans(r#":local "my var""#)).is_none());
        // …and a mid-line :local after `{` is not classified (same rule as
        // documentSymbol).
        assert!(declared_variable(&tokenize_with_spans("{ :local x }")).is_none());
    }

    // ── Index building ────────────────────────────────────────────

    #[test]
    fn test_index_declarations_then_usages_in_document_order() {
        let doc = ":local wan \"e\"\n:put $wan\n/ip/address add interface=$wan\n";
        let hits = index_of(doc);
        assert_eq!(
            summary(&hits),
            vec![
                "wan::local@0".to_string(),
                "wan:$@1".to_string(),
                "wan:$@2".to_string(),
            ],
            "declaration first, then usages, all in doc order"
        );
        // Usage spans exclude the `$` sigil.
        assert_eq!(hits[1].start, 6);
        assert_eq!(hits[1].end, 9);
    }

    #[test]
    fn test_continuation_split_declaration_is_indexed_once() {
        // `:local counter \` joined with `=1` is ONE logical command; the
        // declaration must be found despite the physical split.
        let doc = ":local counter \\\n=1\n:put $counter\n";
        let hits = index_of(doc);
        assert_eq!(
            summary(&hits),
            vec!["counter::local@0".to_string(), "counter:$@1".to_string()]
        );
    }

    #[test]
    fn test_empty_document_yields_empty_index() {
        assert!(index_of("").is_empty());
        assert!(index_of("# only a comment\n").is_empty());
    }

    // ── Usage scanning: quotes, $$, comments ─────────────────────

    #[test]
    fn test_usage_scan_ignores_dollar_inside_strings() {
        let doc = concat!(
            ":put \"cost is $5 not $var\"\n",    // double-quoted: inert
            ":put 'literal $var too'\n",         // single-quoted: inert
            "set comment=\"see $var now\" ok\n", // mixed token, string part inert
            ":put $live\n",                      // control: still counted
        );
        let hits = index_of(doc);
        assert_eq!(
            summary(&hits),
            vec!["live:$@3".to_string()],
            "no $ inside any quote style may surface, got {hits:?}"
        );
    }

    #[test]
    fn test_usage_scan_ignores_doubled_dollar() {
        let hits = index_of("$$x\n$$$y\n:put $ok\n");
        assert_eq!(summary(&hits), vec!["ok:$@2".to_string()]);
    }

    #[test]
    fn test_usage_scan_stops_at_unquoted_comment() {
        let doc = ":put $before # trailing $hidden note\n";
        let hits = index_of(doc);
        assert_eq!(summary(&hits), vec!["before:$@0".to_string()]);
    }

    #[test]
    fn test_usage_scan_handles_parens_and_arithmetic_neighbors() {
        // `($count+1)` glues the sigil to a paren; `($count-1)` must NOT
        // swallow the dash into the name.
        let hits = index_of(":put ($count+1)\n:put ($count-1)\n");
        assert_eq!(
            summary(&hits),
            vec!["count:$@0".to_string(), "count:$@1".to_string()]
        );
        assert_eq!(hits[0].end - hits[0].start, 5);
    }

    #[test]
    fn test_lone_dollar_is_not_a_usage() {
        let hits = index_of(":put $\n:put $ more\n");
        assert!(hits.is_empty(), "got {hits:?}");
    }

    // ── Word extraction consistency with hover ───────────────────

    #[test]
    fn test_word_at_matches_hover_extraction_on_usage() {
        let text = ":put $counter";
        // Cursor on the `o` of counter (byte 8): `$` excluded, word is bare.
        assert_eq!(word_at(text, 8), "counter");
        // Cursor just past the identifier still extracts it backwards,
        // exactly like hover after a finished word.
        assert_eq!(word_at(text, text.len()), "counter");
        // On the space, hover's backward extraction grabs the preceding
        // word ("put") — a documented quirk of the shared helpers that
        // navigation tolerates because only an exact occurrence overlap
        // ever resolves.
        assert_eq!(word_at(text, 4), "put");
    }

    // ── Cursor → occurrence resolution ────────────────────────────

    #[test]
    fn test_hit_at_cursor_resolves_usages_and_declarations_only_in_place() {
        let doc = ":local ip 1\n/ip/address add address=1.2.3.4\n:put $ip\n";
        let index = index_of(doc);
        // On the declaration identifier (byte 7 of logical line 0).
        let decl = hit_at_cursor(&index, "ip", 0, 7);
        assert!(matches!(
            decl.map(|d| d.kind),
            Some(HitKind::Declaration(_))
        ));
        // On the usage (logical line 2, byte 6).
        let usage = hit_at_cursor(&index, "ip", 2, 6);
        assert!(matches!(usage.map(|u| u.kind), Some(HitKind::Usage)));
        // A same-spelling PROPERTY elsewhere must not resolve even though
        // the name exists in the index.
        assert!(hit_at_cursor(&index, "address", 1, 17).is_none());
    }

    #[test]
    fn test_hit_at_cursor_strips_defensive_sigil_and_rejects_empty() {
        let index = index_of(":local x\n$y\n");
        assert!(
            hit_at_cursor(&index, "$x", 0, 7).is_some(),
            "sigil tolerated"
        );
        assert!(hit_at_cursor(&index, "", 0, 0).is_none());
        assert!(hit_at_cursor(&index, "$", 0, 0).is_none());
        assert!(hit_at_cursor(&index, "zz", 0, 0).is_none());
    }

    // ── Definition-choice rule ────────────────────────────────────

    #[test]
    fn test_choose_definition_prefers_closest_preceding_local() {
        let doc = ":global x\n:local x\n:put $x\n";
        let index = index_of(doc);
        let def = choose_definition(&index, "x", (2, 5)).expect("definition exists");
        assert_eq!(
            def.kind,
            HitKind::Declaration(DeclKind::Local),
            "the LAST preceding declaration (:local) wins over the earlier :global"
        );
        assert_eq!(def.logical_line, 1);
    }

    #[test]
    fn test_choose_definition_falls_back_to_first_when_none_precedes() {
        let doc = ":put $x\n:global x\n:local x\n";
        let index = index_of(doc);
        let def = choose_definition(&index, "x", (0, 6)).expect("definition exists");
        assert_eq!(
            def.kind,
            HitKind::Declaration(DeclKind::Global),
            "nothing precedes ⇒ FIRST declaration by document position, kind irrelevant"
        );
    }

    #[test]
    fn test_choose_definition_from_declaration_returns_itself() {
        let doc = ":local x\n:local y\n:put $x\n";
        let index = index_of(doc);
        let def = choose_definition(&index, "x", (0, 7)).expect("definition exists");
        assert_eq!(
            def.logical_line, 0,
            "requesting from a declaration returns it"
        );
    }

    #[test]
    fn test_choose_definition_unknown_name_is_none() {
        let index = index_of(":local x\n");
        assert!(choose_definition(&index, "zzz", (0, 0)).is_none());
    }

    // ── References collection ─────────────────────────────────────

    #[test]
    fn test_collect_references_counts_toggled_by_include_declaration() {
        let doc = ":local n 0\n:put $n\n:set $n ($n + 1)\n";
        let index = index_of(doc);
        let decl = choose_definition(&index, "n", (2, 6));

        let with = collect_references(&index, "n", decl);
        assert_eq!(with.len(), 4, "declaration + three usages");
        assert!(
            matches!(with[0].kind, HitKind::Declaration(_)),
            "declaration comes first"
        );

        let without = collect_references(&index, "n", None);
        assert_eq!(without.len(), 3, "usages only");
        assert!(without.iter().all(|h| h.kind == HitKind::Usage));
    }

    #[test]
    fn test_collect_references_capped_at_max() {
        // MAX_REFERENCES + 1 usages ⇒ exactly the cap survives.
        let mut doc = String::from(":local v\n");
        for _ in 0..=MAX_REFERENCES {
            doc.push_str(":put $v\n");
        }
        let index = index_of(&doc);
        assert_eq!(index.len(), MAX_REFERENCES + 2);
        let refs = collect_references(&index, "v", None);
        assert_eq!(refs.len(), MAX_REFERENCES, "flat list capped");
    }

    #[test]
    fn test_collect_references_unknown_name_is_empty() {
        let index = index_of(":put $v\n");
        assert!(collect_references(&index, "zzz", None).is_empty());
    }
}
