// ── Variable navigation (textDocument/definition + references) ──
//
// Pure computation module for RouterOS script variable navigation, in the
// house style of `signature.rs`: no I/O, deterministic output, unit-tested
// in place.
//
// V1 SEMANTICS (deliberately simple, honest about limits):
//
// - Declarations are the identifier token immediately following a `:local`
//   or `:global` COMMAND token on a logical line. The command may be
//   preceded only by statement separators (`{`, `}`, `;`, or a token ending
//   in `;` from a `;`-separated tail), so `{ :local x }` and
//   `:put 1; :local y` still declare (same rule documentSymbol uses, so
//   the outline and navigation can never disagree). `/` and `..` are
//   deliberately NOT separators: a `/`-prefixed token opens a menu path,
//   so `/ :local x` must not declare. Inline values
//   (`:local x=1`) belong to the identifier only up to the `=`.
// - Usages are bare `$name` references anywhere else in the document. The
//   scanner is quote-aware: `$` inside `"…"` IS indexed (RouterOS
//   interpolates double-quoted strings) while `$` inside `'…'` stays
//   literal, an unquoted `#` stops the scan (comments), and a doubled `$$`
//   reads as a literal dollar rather than a reference start. `$()`
//   expression syntax, `${name}` braces, and quoted identifier names
//   (`$"my var"`) are out of scope for v1 (deferred to keep this change
//   small — see follow-up note below).
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

/// True when `text` is a statement separator that may precede a `:local` /
/// `:global` command token on the same logical line: an isolated brace or
/// semicolon (`{`, `}`, `;`), or any token ending in `;` (a `;`-separated
/// tail like `:put 1;`). `/` and `..` are NOT separators — a `/`-prefixed
/// token opens a menu path, so `/ :local x` must never declare.
fn is_leading_separator(text: &str) -> bool {
    matches!(text, "{" | "}" | ";") || text.ends_with(';')
}

/// Extract the declaration introduced by a `:local` / `:global` command line.
///
/// Returns `(kind, identifier, ident_start, ident_end)` where the offsets
/// locate ONLY the identifier inside the tokenized text. Shared primitive
/// for documentSymbol naming (`symbols.rs`) and the navigation index, so
/// the outline and go-to-definition can never disagree about what a
/// declaration is or where its name spans.
///
/// The command token is the FIRST `:local` / `:global` token whose
/// predecessors are all statement separators (see [`is_leading_separator`]
/// — covering `{ :local x }` block openers and `;`-separated tails like
/// `:put 1; :local y`), or the token at index 0. A `:local` buried after a
/// real command (`:put $x :local y`) is NOT a declaration. Returns `None`
/// when no such command token exists or it carries no bare identifier
/// token (`:global` alone, quoted names).
pub(crate) fn declared_variable(tokens: &[SpanToken]) -> Option<(DeclKind, String, usize, usize)> {
    let cmd_idx = tokens
        .iter()
        .position(|t| t.text == ":local" || t.text == ":global")?;
    // Enforce the separator rule for non-zero positions: all tokens before
    // the command must be separators.
    if cmd_idx > 0
        && !tokens[..cmd_idx]
            .iter()
            .all(|t| is_leading_separator(&t.text))
    {
        // Fallback: accept when the IMMEDIATELY preceding token terminates
        // a statement (`;`-separated tail) even if earlier tokens are real
        // commands — `:put 1; :local y` still declares.
        let prev = &tokens[cmd_idx - 1].text;
        if !(prev == "{" || prev == "}" || prev == ";" || prev.ends_with(';')) {
            return None;
        }
        // Earlier tokens may be real commands here; the `;` boundary is
        // what matters.
    }
    let kind = match tokens[cmd_idx].text.as_str() {
        ":local" => DeclKind::Local,
        ":global" => DeclKind::Global,
        _ => return None,
    };
    // The identifier is the token immediately following the command token…
    let var = tokens.get(cmd_idx + 1)?;
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
/// Quoting rule (RouterOS interpolation): `$name` inside `"…"` IS a usage
/// (double-quoted strings interpolate), while `$name` inside `'…'` is
/// literal text and never surfaces. A usage starts at a `$` outside
/// single quotes whose next byte begins an identifier AND whose PREVIOUS
/// byte is not another `$`: a doubled dollar reads as a literal `$`, so
/// `$$x` and `$$$x` produce nothing (inside or outside double quotes)
/// while `$x ($y)` produces both names.
///
/// Deferred follow-up (kept out to hold this change small): `${name}`
/// braces and `$"quoted name"` forms never yield usages here. Follow-up:
/// expand the scanner to `${name}` / `$"my var"` (and audit rename scope
/// expansion when that lands — rename reuses this index).
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
            b'$' if !in_single => {
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
