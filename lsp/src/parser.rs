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

// ── Unified quote / escape / comment contract ───────────────────
//
// Single source of truth for RouterOS string and comment semantics. All
// three scanners (`scan_token`, `effective_content_end`, `walk_structure`)
// funnel their `"`, `'`, `\`, `#` transitions through [`QuoteState`], so
// folding and diagnostics cannot drift apart:
//
// - Inside `"..."` / `'...'` a `\` escapes the next byte (the escaped byte
//   loses all structural meaning — it cannot close a quote, start a comment,
//   or delimit a token). Escape is recognised INSIDE quotes only; a `\`
//   outside quotes is literal (shipped folding behaviour).
// - `"` toggles `in_double` only when not inside `'`, and `'` only when not
//   inside `"`; they never nest.
// - An unquoted `#` starts a comment that runs to end-of-line; inside a
//   string it is literal content.
// - Quote state carries ACROSS physical lines (RouterOS `\` continuations
//   can split a string); `escaped` and comment states reset at each line
//   boundary.
//
// Any change to RouterOS quoting must be made here and the parity tests
// (`test_quote_state_parity`) will catch drift.

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct QuoteState {
    in_double: bool,
    in_single: bool,
    escaped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuoteAdvance {
    Escaped,
    EscapeStart,
    DoubleOpen,
    DoubleClose,
    SingleOpen,
    SingleClose,
    CommentStart,
    Other(u8),
}

impl QuoteState {
    pub(crate) fn new() -> Self {
        Self {
            in_double: false,
            in_single: false,
            escaped: false,
        }
    }

    pub(crate) fn is_in_quote(&self) -> bool {
        self.in_double || self.in_single
    }

    /// Reset per-line transient state (`escaped`) while preserving quote
    /// continuity across physical lines (see module contract).
    pub(crate) fn reset_line(&mut self) {
        self.escaped = false;
    }

    pub(crate) fn advance_byte(&mut self, b: u8) -> QuoteAdvance {
        if self.escaped {
            self.escaped = false;
            return QuoteAdvance::Escaped;
        }
        match b {
            b'\\' if self.in_double || self.in_single => {
                self.escaped = true;
                QuoteAdvance::EscapeStart
            }
            b'"' if !self.in_single => {
                self.in_double = !self.in_double;
                if self.in_double {
                    QuoteAdvance::DoubleOpen
                } else {
                    QuoteAdvance::DoubleClose
                }
            }
            b'\'' if !self.in_double => {
                self.in_single = !self.in_single;
                if self.in_single {
                    QuoteAdvance::SingleOpen
                } else {
                    QuoteAdvance::SingleClose
                }
            }
            b'#' if !self.in_double && !self.in_single => QuoteAdvance::CommentStart,
            other => QuoteAdvance::Other(other),
        }
    }

    pub(crate) fn advance_char(&mut self, c: char) -> QuoteAdvance {
        if self.escaped {
            self.escaped = false;
            return QuoteAdvance::Escaped;
        }
        // All structural chars are ASCII; non-ASCII never toggles state.
        if !c.is_ascii() {
            return QuoteAdvance::Other(0xFF);
        }
        self.advance_byte(c as u8)
    }
}

/// Scan one whitespace-delimited token starting at byte offset `start`.
///
/// Quote/comment-aware via [`QuoteState`]: whitespace inside `"..."` or
/// `'...'` does not split the token, a `\` inside quotes escapes the next
/// byte, and an unquoted `#` terminates the token mid-word (the same rule
/// [`effective_content_end`] centralizes). Returns the exclusive end offset,
/// which is always a char boundary: quote, backslash, hash and whitespace
/// bytes only occur as standalone bytes in valid UTF-8.
fn scan_token(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut q = QuoteState::new();
    while i < bytes.len() {
        let adv = q.advance_byte(bytes[i]);
        match adv {
            QuoteAdvance::Escaped => {
                // Escaped byte inside quotes — inert, consumed.
                i += 1;
                continue;
            }
            QuoteAdvance::EscapeStart => {
                // The '\' itself — the next byte will be reported as Escaped.
                i += 1;
                continue;
            }
            QuoteAdvance::DoubleOpen
            | QuoteAdvance::DoubleClose
            | QuoteAdvance::SingleOpen
            | QuoteAdvance::SingleClose => {
                i += 1;
                continue;
            }
            QuoteAdvance::CommentStart => break,
            QuoteAdvance::Other(b) => {
                if !q.is_in_quote() && b.is_ascii_whitespace() {
                    break;
                }
                i += 1;
            }
        }
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
    let mut q = QuoteState::new();
    let mut in_comment = false;
    // Position of the quote that opened the currently active quoted string,
    // so EOF can report the OPENING quote instead of the end of input.
    let mut quote_open: Option<(usize, usize)> = None;

    for (line_idx, line) in doc.lines().enumerate() {
        for (col, c) in line.char_indices() {
            if in_comment {
                continue; // comments end at end-of-line (reset below)
            }
            match q.advance_char(c) {
                QuoteAdvance::Escaped | QuoteAdvance::EscapeStart => continue,
                QuoteAdvance::DoubleOpen => {
                    quote_open = Some((line_idx, col));
                    continue;
                }
                QuoteAdvance::DoubleClose => {
                    quote_open = None;
                    continue;
                }
                QuoteAdvance::SingleOpen => {
                    quote_open = Some((line_idx, col));
                    continue;
                }
                QuoteAdvance::SingleClose => {
                    quote_open = None;
                    continue;
                }
                QuoteAdvance::CommentStart => {
                    in_comment = true;
                    continue;
                }
                QuoteAdvance::Other(_) => {
                    if q.is_in_quote() {
                        continue;
                    }
                    match c {
                        '{' => on_event(StructureEvent::OpenBrace {
                            line: line_idx,
                            character: col,
                        }),
                        '}' => on_event(StructureEvent::CloseBrace {
                            line: line_idx,
                            character: col,
                        }),
                        _ => {}
                    }
                }
            }
        }
        // Physical line boundary resets per-line states. Quote state does
        // NOT reset: a `\`-continuation can legally split a quoted string.
        in_comment = false;
        q.reset_line();
    }

    // EOF inside a quoted string: point at the opening quote so the user
    // sees where the string started, not where the file happens to end.
    if q.is_in_quote()
        && let Some((line, character)) = quote_open
    {
        on_event(StructureEvent::UnterminatedQuote { line, character });
    }
}

/// Byte offset where the *effective content* of `line` ends: an unquoted
/// `#` starts a comment that runs to end-of-line, so everything from the
/// first unquoted `#` onward is inert. Returns `line.len()` when the line
/// has no such comment.
///
/// Quote-aware: a `#` inside `"..."` or `'...'` (with `\` escaping the next
/// byte inside quotes) is literal content, not a comment start. This is the
/// SAME rule [`walk_structure`] applies per character and the same rule the
/// diagnostics continuation detection uses; centralizing it here means the
/// three consumers cannot drift apart. The returned offset is always a char
/// boundary (`#` is a standalone ASCII byte).
pub(crate) fn effective_content_end(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut q = QuoteState::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let adv = q.advance_byte(bytes[i]);
        match adv {
            QuoteAdvance::Escaped => {
                i += 1;
                continue;
            }
            QuoteAdvance::EscapeStart => {
                // The '\' itself; the next byte will be Escaped.
                i += 1;
                continue;
            }
            QuoteAdvance::DoubleOpen
            | QuoteAdvance::DoubleClose
            | QuoteAdvance::SingleOpen
            | QuoteAdvance::SingleClose => {
                i += 1;
                continue;
            }
            QuoteAdvance::CommentStart => return i,
            QuoteAdvance::Other(_) => i += 1,
        }
    }
    bytes.len()
}

/// Split a line into tokens with spans: quoted strings, /-prefixed paths, or
/// bare words.
///
/// Quote-aware: a bare word that opens a quote keeps consuming across
/// whitespace until the matching close (e.g. `comment="a=b c=d"` stays ONE
/// token), so quoted values can no longer spawn phantom property tokens
/// downstream. Unterminated quotes simply run to end-of-input.
///
/// Comment-aware: an unquoted `#` at any position (token start or mid-word)
/// starts an inert comment for tokenization — the token scan stops at it and
/// tokenization stops as well, so nothing from the first unquoted `#` onward
/// is ever emitted. A `#` inside quotes is literal content.
pub(crate) fn tokenize_with_spans(text: &str) -> Vec<SpanToken> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'#' {
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
/// Preceding lines are contributed as their *effective content*: the comment
/// tail is cut quote-aware ([`effective_content_end`]), an odd trailing
/// backslash run (a continuation marker) is removed, and the remainder is
/// trimmed of surrounding whitespace. Lines whose effective content is empty
/// — full-line comments (including indented ones and comments ending in a
/// backslash) and lone-backslash lines — are INERT: the walk skips them and
/// keeps going, so a comment between a path line and its command line does
/// not lose the path context.
///
/// `cursor_char` is a BYTE offset within the cursor line (already converted
/// from the negotiated wire encoding by callers at the protocol boundary).
///
/// The result is intentionally NOT right-trimmed: trailing whitespace before
/// the cursor is the signal that distinguishes "typing inside the last
/// token" (value-completion mode) from "finished the token, starting a new
/// one" (property-completion mode). The tokenizer ignores surrounding
/// whitespace anyway, so only consumers that care about the cursor boundary
/// can observe the difference. This no-right-trim guarantee applies ONLY to
/// the cursor line itself — preceding lines are normalized as described
/// above. BLANK physical lines still terminate the walk: a blank line
/// separates commands.
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
        // Blank physical lines still separate commands (unchanged rule).
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            break;
        }
        // Effective content: cut the comment tail quote-aware, then remove
        // a trailing backslash run only when it is odd (a continuation
        // marker; an even run is an escaped literal pair).
        let content = &lines[i][..effective_content_end(lines[i])];
        let content = content.trim_end();
        let run = content.bytes().rev().take_while(|&b| b == b'\\').count();
        let body = if run % 2 == 1 {
            &content[..content.len() - run]
        } else {
            content
        };
        let body = body.trim();
        // Empty effective content (full-line comment, lone backslash line,
        // comment ending in a backslash) is inert: skip and keep walking.
        if body.is_empty() {
            continue;
        }
        if body.starts_with('/') || body.starts_with(':') {
            parts.insert(0, body);
            break;
        }
        parts.insert(0, body);
    }

    parts.join(" ")
}

/// Parse a line of RouterOS script into structural components.
pub fn parse_line(data: &MenuData, before_cursor: &str) -> LineContext {
    let tokens = tokenize(before_cursor);
    let mut path_parts: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    let mut properties: HashMap<String, String> = HashMap::new();

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
    }
}

// ── Per-document parse cache ──────────────────────────────────────
//
// Request handlers used to re-run the continuation-aware logical-line join
// (`diagnostics::logical_lines`) on every request. This cache memoizes that
// join per open document so repeated requests (completion, definition,
// references, rename) only reparse when the text actually changed.
//
// Keying: document URI + hash of the full text. The server tracks no
// per-document version counter (it stores plain `uri -> text`), so the
// text hash is the change detector: any edit yields a different hash and
// therefore a miss followed by a reparse. Staleness needs no explicit
// dirty flag; `invalidate` exists for lifecycle events (didOpen re-insert,
// didClose) where the entry must die regardless of content.
//
// Bound: entries are keyed by tracked-document URI and the server caps
// tracked documents at `MAX_DOCS`; as belt-and-braces `get_or_insert`
// evicts one arbitrary entry before exceeding that cap, so the cache can
// never outgrow the document store it shadows.

/// One cached parse: the hash the entry was built from plus the derived
/// logical lines it memoizes.
struct CachedDoc {
    text_hash: u64,
    logicals: Vec<crate::diagnostics::LogicalLine>,
}

/// Memoized logical-line joins keyed by document URI.
pub(crate) struct ParseCache {
    entries: HashMap<String, CachedDoc>,
}

/// Hash the full document text for change detection (SipHash via std).
fn text_hash(text: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl ParseCache {
    /// Empty cache; entries accrue lazily via [`ParseCache::get_or_insert`].
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Look up the cached logical lines for the CURRENT text of `uri`.
    ///
    /// Returns `Some` only when an entry exists AND its hash matches `doc`
    /// (warm cache); any edit since the entry was stored yields `None`.
    pub(crate) fn lookup(
        &self,
        uri: &str,
        doc: &str,
    ) -> Option<&[crate::diagnostics::LogicalLine]> {
        self.entries.get(uri).and_then(|entry| {
            if entry.text_hash == text_hash(doc) {
                Some(entry.logicals.as_slice())
            } else {
                None
            }
        })
    }

    /// Return the cached logical lines for the CURRENT text of `uri`,
    /// parsing and storing them on a miss (cold cache or changed text).
    ///
    /// Bounded by the document-store discipline (`MAX_DOCS`): inserting a
    /// new URI while at the cap evicts one arbitrary entry first.
    pub(crate) fn get_or_insert(
        &mut self,
        uri: &str,
        doc: &str,
    ) -> &[crate::diagnostics::LogicalLine] {
        let hash = text_hash(doc);
        let fresh = self
            .entries
            .get(uri)
            .is_none_or(|entry| entry.text_hash != hash);
        if fresh {
            let logicals = crate::diagnostics::logical_lines(doc);
            if !self.entries.contains_key(uri)
                && self.entries.len() >= crate::MAX_DOCS
                && let Some(victim) = self.entries.keys().next().cloned()
            {
                self.entries.remove(&victim);
            }
            self.entries.insert(
                uri.to_string(),
                CachedDoc {
                    text_hash: hash,
                    logicals,
                },
            );
        }
        &self
            .entries
            .get(uri)
            .expect("parse cache entry was just inserted")
            .logicals
    }

    /// Drop the entry for `uri`, if any. Called on didOpen (re-insert) and
    /// didClose (entries die with the document). Edits need no explicit
    /// call: the hash check in [`ParseCache::lookup`] already misses.
    pub(crate) fn invalidate(&mut self, uri: &str) {
        self.entries.remove(uri);
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

    #[test]
    fn test_build_before_cursor_comment_between_path_and_command_is_inert() {
        // A full-line comment between the path line and the command line
        // must not break the walk — the path context survives.
        let doc = "/ip/address\n# some note\nadd address=1.1.1.1/24";
        let line = doc.lines().nth(2).unwrap();
        let s = build_before_cursor(doc, 2, line.len());
        assert!(
            s.contains("/ip/address"),
            "path line must survive the comment: {s}"
        );
        assert!(
            s.contains("add address=1.1.1.1/24"),
            "command line must be present: {s}"
        );
        let data = synthetic_data();
        let ctx = parse_line(&data, &s);
        assert_eq!(ctx.path, "/ip/address");
        assert_eq!(ctx.command.as_deref(), Some("add"));
        assert_eq!(
            ctx.properties.get("address").map(|v| v.as_str()),
            Some("1.1.1.1/24")
        );
    }

    #[test]
    fn test_build_before_cursor_comment_inert_equivalence() {
        // Inserting a full-line comment changes nothing: the joined context
        // is identical with or without it.
        let plain = "/ip/address\nadd address=1.1.1.1/24";
        let with_comment = "/ip/address\n# some note\nadd address=1.1.1.1/24";
        let a = build_before_cursor(plain, 1, plain.lines().nth(1).unwrap().len());
        let b = build_before_cursor(with_comment, 2, with_comment.lines().nth(2).unwrap().len());
        assert_eq!(a, b);
    }

    #[test]
    fn test_build_before_cursor_strips_continuation_backslash() {
        // A trailing continuation backslash on the preceding line must not
        // survive into the joined text as a bare '\' token.
        let doc = "/ip/address add \\\naddress=1.1.1.1/24";
        let line = doc.lines().nth(1).unwrap();
        let s = build_before_cursor(doc, 1, line.len());
        assert_eq!(s, "/ip/address add address=1.1.1.1/24");
        for token in tokenize(&s) {
            assert!(!token.contains('\\'), "no backslash token allowed: {s}");
        }
        let data = synthetic_data();
        let ctx = parse_line(&data, &s);
        assert_eq!(ctx.command.as_deref(), Some("add"));
        assert_eq!(
            ctx.properties.get("address").map(|v| v.as_str()),
            Some("1.1.1.1/24")
        );
    }

    #[test]
    fn test_build_before_cursor_keeps_escaped_backslash_pair() {
        // Two trailing backslashes are an escaped literal pair, NOT a
        // continuation marker: both must be kept in the joined text.
        let doc = "/ip/address add comment=x\\\\\naddress=1.1.1.1/24";
        let line = doc.lines().nth(1).unwrap();
        let s = build_before_cursor(doc, 1, line.len());
        assert_eq!(s, "/ip/address add comment=x\\\\ address=1.1.1.1/24");
    }

    #[test]
    fn test_build_before_cursor_comment_ending_in_backslash_is_inert() {
        // A comment line ending in a backslash does NOT continue: the
        // comment tail (and its backslash) is cut before continuation
        // counting, so the line is inert and the walk keeps going.
        let doc = "/ip/address\n# note \\\nadd address=1.1.1.1/24";
        let line = doc.lines().nth(2).unwrap();
        let s = build_before_cursor(doc, 2, line.len());
        assert!(!s.contains("note"), "comment content must be inert: {s}");
        assert!(s.contains("/ip/address"), "path must survive: {s}");
        assert!(
            s.contains("add address=1.1.1.1/24"),
            "command must be present: {s}"
        );
    }

    #[test]
    fn test_build_before_cursor_strips_inline_comment_tail() {
        // An inline comment tail on a preceding line is cut (quote-aware)
        // before the line is contributed to the joined context.
        let doc = "/ip/address add # starting\naddress=1.1.1.1/24";
        let line = doc.lines().nth(1).unwrap();
        let s = build_before_cursor(doc, 1, line.len());
        assert_eq!(s, "/ip/address add address=1.1.1.1/24");
    }

    #[test]
    fn test_build_before_cursor_lone_backslash_line_is_inert() {
        // A lone-backslash line has empty effective content (the odd run is
        // a continuation marker): inert, skipped, walk keeps going.
        let doc = "/ip/address\n\\\nadd address=1.1.1.1/24";
        let line = doc.lines().nth(2).unwrap();
        let s = build_before_cursor(doc, 2, line.len());
        assert_eq!(s, "/ip/address add address=1.1.1.1/24");
    }

    // ── effective_content_end ─────────────────────────────────────

    #[test]
    fn test_effective_content_end_units() {
        // '#' at byte offset 8 of "add x=1 # c".
        assert_eq!(effective_content_end("add x=1 # c"), 8);
        // '#' inside double quotes is literal content: whole line.
        assert_eq!(effective_content_end(r#"a="b#c""#), r#"a="b#c""#.len());
        // Leading comment: no effective content at all.
        assert_eq!(effective_content_end("#x"), 0);
        // No comment: whole line.
        assert_eq!(effective_content_end("print"), 5);
    }

    // ── parse_line ────────────────────────────────────────────────

    #[test]
    fn test_parse_line_path_only() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address");
        assert_eq!(ctx.path, "/ip/address");
        assert!(ctx.command.is_none());
        assert!(ctx.properties.is_empty());
    }

    #[test]
    fn test_parse_line_path_with_verb() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address add");
        assert_eq!(ctx.path, "/ip/address");
        assert_eq!(ctx.command.as_deref(), Some("add"));
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
    }

    #[test]
    fn test_parse_line_property_with_empty_value() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/firewall/filter add chain=");
        assert_eq!(ctx.properties.get("chain").map(|s| s.as_str()), Some(""));
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
        assert!(ctx.properties.is_empty());
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
    }

    #[test]
    fn test_tokenize_inline_comment() {
        let tokens =
            tokenize_with_spans(r#"/ip/address add address=1.1.1.1/24 # comment with foo=bar"#);
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].text, "/ip/address");
        assert_eq!(tokens[1].text, "add");
        assert_eq!(tokens[2].text, "address=1.1.1.1/24");
    }

    #[test]
    fn test_tokenize_hash_inside_quotes_is_not_comment() {
        let tokens =
            tokenize_with_spans(r##"/ip/address add comment="#1 interface" address=1.1.1.1/24"##);
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[0].text, "/ip/address");
        assert_eq!(tokens[1].text, "add");
        assert_eq!(tokens[2].text, r##"comment="#1 interface""##);
        assert_eq!(tokens[3].text, "address=1.1.1.1/24");
    }

    #[test]
    fn test_tokenize_unquoted_hash_stops_token_mid_word() {
        // An unquoted '#' starts a comment at ANY position, even mid-word:
        // the comment tail must never leak into the token.
        assert_eq!(tokenize("add foo=bar#baz"), vec!["add", "foo=bar"]);
        assert_eq!(
            tokenize("add url=https://x#frag"),
            vec!["add", "url=https://x"]
        );
        // A '#' inside single quotes is literal content, not a comment:
        // the whole word stays ONE token.
        let tokens = tokenize(r#"comment='a # b'"#);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0], r#"comment='a # b'"#);
    }

    #[test]
    fn test_parse_line_with_inline_comment() {
        let data = synthetic_data();
        let ctx = parse_line(
            &data,
            "/ip/address add address=1.1.1.1/24 # comment with extra=prop",
        );
        assert_eq!(ctx.path, "/ip/address");
        assert_eq!(ctx.command.as_deref(), Some("add"));
        assert_eq!(
            ctx.properties.get("address").map(|s| s.as_str()),
            Some("1.1.1.1/24")
        );
        assert_eq!(ctx.properties.get("extra"), None);
    }

    // ── QuoteState parity ───────────────────────────────────────────

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
            .get_or_insert(uri, doc)
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
    fn test_parse_cache_edit_invalidates_via_hash_mismatch() {
        let mut cache = ParseCache::new();
        let uri = "file:///cache-edit.rsc";
        let before = ":local x\n:put $x\n";
        let after = ":local x\n:put $x\n:put $x\n";
        cache.get_or_insert(uri, before);
        assert!(cache.lookup(uri, before).is_some());
        // Same URI, changed text: the stored hash no longer matches, so the
        // lookup misses (stale entries can never be served)…
        assert!(
            cache.lookup(uri, after).is_none(),
            "edited text must miss the cache"
        );
        // …and the next access reparses the new content.
        let texts: Vec<String> = cache
            .get_or_insert(uri, after)
            .iter()
            .map(|ll| ll.text().to_string())
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(cache.lookup(uri, after).is_some());
        assert!(cache.lookup(uri, before).is_none());
    }

    #[test]
    fn test_parse_cache_invalidate_drops_entry_regardless_of_content() {
        let mut cache = ParseCache::new();
        let uri = "file:///cache-close.rsc";
        let doc = ":put hi\n";
        cache.get_or_insert(uri, doc);
        assert!(cache.lookup(uri, doc).is_some());
        cache.invalidate(uri);
        assert!(
            cache.lookup(uri, doc).is_none(),
            "didClose must kill the entry even for unchanged text"
        );
        // Invalidating an unknown URI is a no-op, never a panic.
        cache.invalidate("file:///never-opened.rsc");
    }

    #[test]
    fn test_parse_cache_bounded_by_max_docs_discipline() {
        let mut cache = ParseCache::new();
        for i in 0..(crate::MAX_DOCS + 25) {
            let uri = format!("file:///cache-cap-{i}.rsc");
            cache.get_or_insert(&uri, ":put hi\n");
        }
        assert!(
            cache.entries.len() <= crate::MAX_DOCS,
            "cache must never outgrow the document store, got {}",
            cache.entries.len()
        );
    }
}
