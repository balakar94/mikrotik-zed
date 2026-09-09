// ── Tokenizer / RouterOS line parser ─────────────────────────────────────
//
// Quote-aware tokenization and structural parsing of RouterOS
// command lines. Pure functions over strings —
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

// ── Unified quote / escape / comment contract ────────────────────────────
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

// ── Whole-document structural walk ───────────────────────────────────────
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

/// Split `token` into `(key, value)` at the first `=` outside quotes.
///
/// Quote-aware via [`QuoteState`]: an `=` inside `"..."` or `'...'` (with
/// `\` escaping the next byte inside quotes) is literal content, so
/// `/log info ("a=b" . $x)` yields no key and `comment="a=b c=d"` yields
/// key `comment`. Returns `None` when there is no outside-quote `=`, when
/// `=` sits at index 0, when the key fails `^[A-Za-z][A-Za-z0-9_-]*$`
/// (covers `pool-name`, `tcp-flags`, `start-date`, `place-before`), or when
/// the token leads with `([$"\'` (expression debris like `("a=b"`).
pub(crate) fn split_key_value(token: &str) -> Option<(&str, &str)> {
    if token.is_empty() {
        return None;
    }
    // Expression leaders can never open a property assignment.
    if matches!(token.as_bytes()[0], b'(' | b'[' | b'$' | b'"' | b'\'') {
        return None;
    }
    let bytes = token.as_bytes();
    let mut q = QuoteState::new();
    let mut eq_idx: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        let adv = q.advance_byte(b);
        match adv {
            QuoteAdvance::Escaped | QuoteAdvance::EscapeStart => continue,
            QuoteAdvance::DoubleOpen
            | QuoteAdvance::DoubleClose
            | QuoteAdvance::SingleOpen
            | QuoteAdvance::SingleClose => continue,
            // An unquoted `#` ends meaningful content; nothing after it
            // can be a structural `=`.
            QuoteAdvance::CommentStart => break,
            QuoteAdvance::Other(byte) => {
                if byte == b'=' && !q.is_in_quote() {
                    eq_idx = Some(i);
                    break;
                }
            }
        }
    }
    let eq = eq_idx?;
    if eq == 0 {
        return None;
    }
    let key = &token[..eq];
    let value = &token[eq + 1..];
    // `=` is ASCII so both slices sit on char boundaries.
    if !is_valid_property_key(key) {
        return None;
    }
    Some((key, value))
}

/// True when `key` matches `^[A-Za-z][A-Za-z0-9_-]*$`.
fn is_valid_property_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    for &b in &bytes[1..] {
        if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return false;
        }
    }
    true
}

/// True when `token` may introduce a command verb: the first byte is ASCII
/// alphabetic. RouterOS verbs are plain words (`add`, `print`, `set`,
/// `force-update`); this generalizes the [`split_key_value`] leader rule
/// (`([$"\'` can never lead) to also reject expression debris the key
/// regex never contemplated: the `.` concatenation operator, `+`, and
/// value fragments like `2h)`. Without this, such debris becomes the
/// command (blocking the trailing-verb split) on lines like
/// `/ipv6/nd/prefix/add ... comment=("X" . $y)`.
fn is_command_leader(token: &str) -> bool {
    matches!(token.as_bytes().first(), Some(b) if b.is_ascii_alphabetic())
}

/// Count unquoted `[` / `]` bytes in `token` via [`QuoteState`].
///
/// Quoted brackets are literal content; an unquoted `#` ends the scan.
/// Returns `(opens, closes)`.
pub(crate) fn bracket_counts(token: &str) -> (u32, u32) {
    let mut q = QuoteState::new();
    let mut opens: u32 = 0;
    let mut closes: u32 = 0;
    for &b in token.as_bytes() {
        let adv = q.advance_byte(b);
        match adv {
            QuoteAdvance::Escaped
            | QuoteAdvance::EscapeStart
            | QuoteAdvance::DoubleOpen
            | QuoteAdvance::DoubleClose
            | QuoteAdvance::SingleOpen
            | QuoteAdvance::SingleClose => continue,
            QuoteAdvance::CommentStart => break,
            QuoteAdvance::Other(byte) => {
                if q.is_in_quote() {
                    continue;
                }
                if byte == b'[' {
                    opens += 1;
                } else if byte == b']' {
                    closes += 1;
                }
            }
        }
    }
    (opens, closes)
}

/// Split a trailing verb off a slash path (`/ipv6/nd/prefix/add`).
///
/// Returns `(parent, verb-as-written)` only when `path` itself is unknown
/// (neither in `menu_by_path` nor `ancestor_prefixes`), the parent before
/// the final `/` is known, and the final segment case-insensitively names
/// a [`MenuData::STANDARD_VERBS`] entry. Single level only; a bare `/add`
/// (empty parent) never splits.
pub(crate) fn split_trailing_verb(path: &str, data: &MenuData) -> Option<(String, String)> {
    if path.is_empty() {
        return None;
    }
    if data.menu_by_path.contains_key(path) || data.ancestor_prefixes.contains(path) {
        return None;
    }
    let (parent, last) = path.rsplit_once('/')?;
    if parent.is_empty() || last.is_empty() {
        return None;
    }
    if !(data.menu_by_path.contains_key(parent) || data.ancestor_prefixes.contains(parent)) {
        return None;
    }
    if !MenuData::STANDARD_VERBS
        .iter()
        .any(|v| v.eq_ignore_ascii_case(last))
    {
        return None;
    }
    Some((parent.to_string(), last.to_string()))
}

/// Parse a line of RouterOS script into structural components.
pub fn parse_line(data: &MenuData, before_cursor: &str) -> LineContext {
    let tokens = tokenize_with_spans(before_cursor);
    let mut path_parts: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    let mut properties: HashMap<String, String> = HashMap::new();
    // Depth of unquoted `[...]` nesting: tokens inside a bracket region or
    // opening one are skipped entirely so inner `key=value` pairs (e.g.
    // `[find pool-name=x]`) never leak into the outer context.
    let mut depth: u32 = 0;

    for tok in &tokens {
        let token = tok.text.as_str();
        let (opens, closes) = bracket_counts(token);
        if depth > 0 || opens > 0 {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }

        if token.starts_with('/') {
            path_parts.push(token.trim_start_matches('/').to_string());
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }

        if let Some((key, value)) = split_key_value(token) {
            properties.insert(key.to_string(), value.to_string());
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }

        // A token carrying an outside-quote `=` that is not a valid
        // property (e.g. `=debris`) is neither property nor verb.
        if token.contains('=') {
            let mut q = QuoteState::new();
            let mut outside_eq = false;
            for &b in token.as_bytes() {
                match q.advance_byte(b) {
                    QuoteAdvance::Escaped
                    | QuoteAdvance::EscapeStart
                    | QuoteAdvance::DoubleOpen
                    | QuoteAdvance::DoubleClose
                    | QuoteAdvance::SingleOpen
                    | QuoteAdvance::SingleClose => continue,
                    QuoteAdvance::CommentStart => break,
                    QuoteAdvance::Other(byte) => {
                        if byte == b'=' && !q.is_in_quote() {
                            outside_eq = true;
                            break;
                        }
                    }
                }
            }
            if outside_eq {
                depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
                continue;
            }
        }

        if !path_parts.is_empty() {
            let current_path = format!("/{}", path_parts.join("/"));
            // Use child_names_by_parent (not menu_by_path) so implicit
            // intermediate menus like /ip/firewall are recognized as valid
            // path segments even though they have no direct TOML entry.
            let is_sub_menu = data
                .child_names_by_parent
                .get(&current_path)
                .map(|children| children.iter().any(|c| c.name == token))
                .unwrap_or(false);
            if is_sub_menu {
                path_parts.push(token.to_string());
            } else if command.is_none() && is_command_leader(token) {
                command = Some(token.to_string());
            }
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }

        if command.is_none() && is_command_leader(token) {
            command = Some(token.to_string());
        }
        depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
    }

    let mut path = if path_parts.is_empty() {
        String::new()
    } else {
        format!("/{}", path_parts.join("/"))
    };
    // Valid RouterOS shorthand: menu+verb in one slash token. Only when no
    // explicit space-separated verb was typed; explicit verbs are never
    // overwritten.
    if command.is_none()
        && !path.is_empty()
        && let Some((parent, verb)) = split_trailing_verb(&path, data)
    {
        path = parent;
        command = Some(verb);
    }

    LineContext {
        path,
        command,
        properties,
    }
}

// ── Per-document parse cache ─────────────────────────────────────────────
//
// Request handlers used to re-run the continuation-aware logical-line join
// (`diagnostics::logical_lines`) on every request. This cache memoizes that
// join per open document so repeated requests (completion, definition,
// references, rename) only reparse when the text actually changed.
//
// Keying: document URI + byte length + hash of the full text. The server
// tracks no per-document version counter (it stores plain `uri -> text`),
// so the text hash is the change detector: any edit yields a different
// hash and therefore a miss followed by a reparse. The stored length is a
// fast reject: a length mismatch misses without hashing the full document.
// Staleness needs no explicit dirty flag; `invalidate` exists for lifecycle
// events (didOpen re-insert, didClose) where the entry must die regardless
// of content.
//
// Bound: entries are keyed by tracked-document URI and the server caps
// tracked documents at `MAX_DOCS`; as belt-and-braces insertion evicts the
// oldest-inserted URI (FIFO via `order`) before exceeding that cap, so the
// cache can never outgrow the document store it shadows. FIFO (not LRU)
// is deliberate: no new dependency, O(1) bookkeeping, and request locality
// comes from document-count bounds rather than recency.
//
// Hot-path discipline: call [`ParseCache::lookup_or_insert`] (single full
// document hash per request). Separate probe-then-insert sequences would
// hash the same bytes twice per request; the combined entry point hashes
// at most once and returns the slice directly. Join
// output semantics are untouched: warm results are byte-identical to a
// fresh `diagnostics::logical_lines` call (pinned by tests below).

/// One cached parse: the length/hash the entry was built from plus the
/// derived logical lines it memoizes.
pub(crate) struct CachedDoc {
    text_len: usize,
    text_hash: u64,
    logicals: Vec<crate::diagnostics::LogicalLine>,
}

/// Memoized logical-line joins keyed by document URI (FIFO-bounded).
pub(crate) struct ParseCache {
    pub(crate) entries: HashMap<String, CachedDoc>,
    /// Insertion order of `entries` keys (oldest front). Kept in sync on
    /// insert/evict/invalidate; drives FIFO eviction at the `MAX_DOCS` cap.
    order: std::collections::VecDeque<String>,
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
    /// Empty cache; entries accrue lazily via [`ParseCache::lookup_or_insert`].
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    /// Insert `logicals` for `uri` built from `doc` (`text_hash` precomputed
    /// by the caller so the hash is computed exactly once per miss).
    /// New URIs at the `MAX_DOCS` cap evict the oldest-inserted entry first.
    fn insert_with_hash(
        &mut self,
        uri: &str,
        doc: &str,
        hash: u64,
        logicals: Vec<crate::diagnostics::LogicalLine>,
    ) {
        let is_new = !self.entries.contains_key(uri);
        if is_new {
            while self.entries.len() >= crate::MAX_DOCS {
                let Some(victim) = self.order.pop_front() else {
                    break;
                };
                if self.entries.remove(&victim).is_some() {
                    break;
                }
                // Stale order entry (invalidated earlier): keep draining.
            }
            self.order.push_back(uri.to_string());
        }
        self.entries.insert(
            uri.to_string(),
            CachedDoc {
                text_len: doc.len(),
                text_hash: hash,
                logicals,
            },
        );
    }

    /// Look up the cached logical lines for the CURRENT text of `uri`.
    ///
    /// Returns `Some` only when an entry exists AND its length and hash both
    /// match `doc` (warm cache); any edit since the entry was stored yields
    /// `None`. Length is checked first so edits that change the byte count
    /// miss without hashing the full document.
    ///
    /// Read-only probe: unlike [`ParseCache::lookup_or_insert`] it never
    /// parses or inserts, so tests can assert cache state (cold miss,
    /// edit invalidation, didClose drop) without mutating it. The production
    /// hot path uses `lookup_or_insert`; the current in-crate users of this
    /// probe are the test suite, hence the scoped allow for non-test builds.
    #[allow(dead_code)]
    pub(crate) fn lookup(
        &self,
        uri: &str,
        doc: &str,
    ) -> Option<&[crate::diagnostics::LogicalLine]> {
        let entry = self.entries.get(uri)?;
        if entry.text_len != doc.len() {
            return None;
        }
        if entry.text_hash == text_hash(doc) {
            Some(entry.logicals.as_slice())
        } else {
            None
        }
    }

    /// Return the cached logical lines for the CURRENT text of `uri`,
    /// parsing and storing them on a miss (cold cache or changed text).
    ///
    /// Single-hash entry point: the full-document hash is computed at most
    /// once per call, whether the outcome is a hit or a miss. Request
    /// handlers must call this directly instead of a separate `lookup`
    /// followed by an inserting call (which would hash the same bytes twice).
    /// Output is identical to a fresh `diagnostics::logical_lines` join.
    pub(crate) fn lookup_or_insert(
        &mut self,
        uri: &str,
        doc: &str,
    ) -> &[crate::diagnostics::LogicalLine] {
        // Fast path without hashing: no entry, or length mismatch.
        let len_miss = match self.entries.get(uri) {
            None => true,
            Some(entry) => entry.text_len != doc.len(),
        };
        if len_miss {
            let hash = text_hash(doc);
            let logicals = crate::diagnostics::logical_lines(doc);
            self.insert_with_hash(uri, doc, hash, logicals);
            return &self
                .entries
                .get(uri)
                .expect("parse cache entry was just inserted")
                .logicals;
        }
        // Same length: exactly one hash decides hit vs. miss.
        let hash = text_hash(doc);
        let hit = self
            .entries
            .get(uri)
            .is_some_and(|entry| entry.text_hash == hash);
        if !hit {
            let logicals = crate::diagnostics::logical_lines(doc);
            self.insert_with_hash(uri, doc, hash, logicals);
        }
        &self
            .entries
            .get(uri)
            .expect("parse cache entry was just inserted")
            .logicals
    }

    /// Drop the entry for `uri`, if any. Called on didOpen (re-insert) and
    /// didClose (entries die with the document). Edits need no explicit
    /// call: the length/hash check in [`ParseCache::lookup`] already misses.
    /// Also drops the FIFO slot eagerly so `order` stays bounded by the
    /// live entry count (no stale-slot accumulation across open/close cycles).
    pub(crate) fn invalidate(&mut self, uri: &str) {
        self.entries.remove(uri);
        self.order.retain(|u| u != uri);
    }
}
