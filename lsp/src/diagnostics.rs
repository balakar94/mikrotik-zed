// ── Diagnostics for RSC language server ───────────────────────────
//
// Provides pull and push diagnostics for MikroTik RouterOS Script files.
// Rules:
//  1. Unknown menu path: Warning if path not in menu_by_path nor child_names_by_parent (including implicit parents)
//  2. Unknown property: Warning if property key not in menu.arguments/flags/read_only
//     (except `value-name` on `unset` lines: verb-level pseudo-key owned by Rule 10)
//  3. Missing required property: Warning for Directory menus when `add`/`set` missing required args
//  4. Duplicate property: Warning if same key appears twice
//  5. Invalid enum value: Warning for enum properties with invalid value
//  8. Unknown command verb: Warning for verbs outside the standard set
//  9. Invalid typed value: Hint (never Error) for bool/num/time/macAddr/
//     ipAddr-ipPrefix-family/ubit properties whose value fails a syntactic
//     shape check (`invalid-bool-value`, `invalid-num-value`,
//     `invalid-time-value`, `invalid-mac-value`, `invalid-ip-value`,
//     `invalid-ubit-value`). Silent on empty/truncated types, empty values,
//     dynamic values (`$var`, `[find ...]`, `(expr)`), and a lone trailing
//     comma left by whitespace tokenization (`rates=1Mbps, 2Mbps`).
// 10. Non-unsettable property: Hint when `unset` targets a property whose
//     dataset entry carries `unset=false` (`non-unsettable-property`)
// 11. Read-only write: Information when a read_only column appears as `key=`
//     on `add`/`set` (`read-only-write`)
//
// Syntactic rules (share the quote/comment-aware walk with folding via
// crate::parser::walk_structure; braces and quotes inside comments or
// strings are inert, and a `\` continuation keeps a string alive across
// physical lines):
//  6. Unclosed brace: Error for every `{` never closed before EOF (`unclosed-brace`)
//     — companion `unmatched-brace`: Error for a stray `}` with no open `{`
//  7. Unclosed quote: Error at the opening quote of a string never terminated
//     before EOF (`unclosed-quote`)
//
// RouterOS line continuation is honored: physical lines ending with a trailing
// unescaped backslash are joined into a single logical line before parsing, so
// commands split across lines (e.g. long quoted URLs after `/tool/fetch add`)
// do not produce false positives such as "Unknown menu". Diagnostic ranges are
// mapped from logical-line offsets back to original physical-line coordinates.
//
// Capped for large docs to prevent OOM / CPU blow-up.

use crate::StructureEvent;
use crate::menus::MenuData;
use crate::{MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DIAGNOSTICS};
use std::collections::{HashMap, HashSet};

/// Source tag stamped on every diagnostic this server emits. Crate-visible
/// so consumers (codeAction quick-fixes) can filter client-echoed
/// diagnostics back to exactly the ones we produced.
pub(crate) const DIAGNOSTIC_SOURCE: &str = "rsc-ls";
/// Cap on syntactic diagnostics (the unclosed/unmatched brace and quote
/// family) emitted per publish. The FIRST ten in document order win; when
/// more exist, an explicit `truncated` Information footer names the dropped
/// remainder ("(+N more - see full list)") so the 11th error is acknowledged
/// instead of silently swallowed — mirroring the semantic truncation path.
/// The response payload stays bounded (at most ten findings plus one hint).
pub(crate) const MAX_SYNTAX_DIAGNOSTICS: usize = 10;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<u8>, // 1 Error, 2 Warning, 3 Information, 4 Hint
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
}

// LSP severity constants
pub mod severity {
    pub const ERROR: u8 = 1;
    pub const WARNING: u8 = 2;
    pub const INFORMATION: u8 = 3;
    /// Hint for low-confidence, never-blocking findings (typed value shapes,
    /// non-unsettable `unset` targets). Advisory only by design.
    pub const HINT: u8 = 4;
}

/// Append a `Did you mean 'x'?` suffix to a diagnostic message when a
/// suggestion survived the length-aware threshold, else return `base`
/// unchanged. Message-only UX: the quick-fix CodeAction edit is untouched.
fn with_suggestion(base: String, suggestion: Option<String>) -> String {
    match suggestion {
        Some(s) => format!("{base}. Did you mean '{s}'?"),
        None => base,
    }
}

// ── Typed value shape checks (Rule 9, Hint-only) ────────────────
//
// Syntactic plausibility only: each predicate accepts a deliberate SUPERSET
// of documented RouterOS spellings (case-insensitive bools, lenient numeric
// units, `never`/`infinite` times) so gaps in the upstream type table cost a
// missed Hint, never a false positive. Anything dynamic (`$var`,
// `[find ...]`, `(expr)`), empty, or attached to an empty/truncated type
// string stays silent.

/// Canonical RouterOS booleans plus the spellings scripts commonly use.
/// RouterOS itself is case-insensitive; matching is too.
const BOOL_WORDS: &[&str] = &["yes", "no", "true", "false", "on", "off"];

/// One Rule 9 finding: wire `code`, rendered message, and an optional
/// `Did you mean` candidate (only for closed vocabularies: bool, ubit).
struct TypedHint {
    code: &'static str,
    /// Human-readable expectation fragment, e.g. `bool: yes | no`.
    expected: String,
    suggestion: Option<String>,
}

impl TypedHint {
    fn message(&self, key: &str, raw_value: &str) -> String {
        let shown = raw_value.trim().trim_matches('"').trim_matches('\'').trim();
        format!(
            "Invalid value '{shown}' for '{key}' (expected {})",
            self.expected
        )
    }
}

/// Dispatch a property value to its type-shape predicate. Returns `None`
/// when the value looks plausible OR when validation is impossible (empty
/// or truncated type, empty/dynamic value, unowned type family).
fn check_typed_value(
    arg: &crate::menus::ArgEntry,
    raw_value: &str,
    budget: &mut crate::suggest::SuggestBudget,
) -> Option<TypedHint> {
    let arg_type = arg.arg_type.trim();
    // Silent on missing types and on generator-truncated display strings.
    if arg_type.is_empty() || arg_type.contains("...") {
        return None;
    }
    let value = raw_value.trim().trim_matches('"').trim_matches('\'').trim();
    if value.is_empty() {
        return None;
    }
    // Dynamic values resolve at runtime; their type is unknowable statically.
    if value.contains(['$', '[', ']', '(', ')']) {
        return None;
    }
    // `enum` stays owned by Rule 5 (Warning with embedded member lists).
    if arg_type == "enum" || arg_type.starts_with("enum ") || arg_type.starts_with("enum(") {
        return None;
    }
    if arg_type == "bool" {
        if is_valid_bool(value) {
            return None;
        }
        return Some(TypedHint {
            code: "invalid-bool-value",
            expected: format!("bool: {}", BOOL_WORDS.join(" | ")),
            suggestion: budget.candidate(value, BOOL_WORDS.iter()),
        });
    }
    if arg_type == "num" {
        if is_valid_num(value) {
            return None;
        }
        return Some(TypedHint {
            code: "invalid-num-value",
            expected: "number with optional unit (e.g. 10, 1500, 10M)".to_string(),
            suggestion: None,
        });
    }
    if arg_type == "time" {
        if is_valid_time(value) {
            return None;
        }
        return Some(TypedHint {
            code: "invalid-time-value",
            expected: "time interval (e.g. 00:10:00, 1h30m, 30s)".to_string(),
            suggestion: None,
        });
    }
    if arg_type == "macAddr" {
        if is_valid_mac(value) {
            return None;
        }
        return Some(TypedHint {
            code: "invalid-mac-value",
            expected: "MAC address (AA:BB:CC:DD:EE:FF)".to_string(),
            suggestion: None,
        });
    }
    if matches!(arg_type, "ipAddr" | "ipPrefix" | "ip6Addr" | "ip6Prefix") {
        if is_valid_ip_field(value) {
            return None;
        }
        return Some(TypedHint {
            code: "invalid-ip-value",
            expected: "IP address or prefix (e.g. 192.168.1.1, 10.0.0.0/24, ::1)".to_string(),
            suggestion: None,
        });
    }
    if arg_type == "ubit" || arg_type.starts_with("ubit ") || arg_type.starts_with("ubit(") {
        let members = arg.ubit_members();
        if members.is_empty() {
            return None;
        }
        // Multi-select bitmask: comma-separated members, each checked; a
        // single leading `!` is RouterOS exclusion syntax, not a typo.
        //
        // Whitespace-split remainder: `tokenize_with_spans` splits on ASCII
        // whitespace, so `rates=1Mbps, 2Mbps` stores only `1Mbps,` for the
        // key while `2Mbps` becomes a bare token. A lone trailing comma is
        // therefore a split artifact, not an empty member — drop exactly one
        // before the empty-member check. Genuine typos (`rates=,`, `a,,b`,
        // `a,,`) still flag because an empty segment survives the strip.
        let effective = value.strip_suffix(',').unwrap_or(value);
        let mut bad: Option<String> = None;
        for member in effective.split(',') {
            let clean = member
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .trim_start_matches('!')
                .trim();
            if clean.is_empty() || members.iter().any(|m| m == clean) {
                continue;
            }
            bad = Some(clean.to_string());
            break;
        }
        // An empty member (`rates=,`, `a,,b`) is a typo worth hinting, but
        // only when nothing else already failed: keep the single-worst-
        // finding shape. Checked against the trailing-comma-normalized form
        // above so a whitespace-split remainder never flags.
        if bad.is_none()
            && effective.split(',').any(|m| {
                m.trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim()
                    .is_empty()
            })
        {
            bad = Some(String::new());
        }
        if let Some(offender) = bad {
            return Some(TypedHint {
                code: "invalid-ubit-value",
                expected: format!("one of: {}", members.join(", ")),
                suggestion: if offender.is_empty() {
                    None
                } else {
                    budget.candidate(&offender, members.iter())
                },
            });
        }
        return None;
    }
    // Every other family (string, iface_enum, date, file, switch, range,
    // multi, object, `address (flags=...)`, timezone, ...) is unowned.
    None
}

fn is_valid_bool(value: &str) -> bool {
    BOOL_WORDS
        .iter()
        .any(|w| w.eq_ignore_ascii_case(value.trim()))
}

fn is_valid_num(value: &str) -> bool {
    let s = value.trim();
    let s = s.strip_prefix('+').unwrap_or(s);
    let s = s.strip_prefix('-').unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    // Hex literals (`0x10`).
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    // Decimal head plus an optional alphabetic unit tail (`10`, `1.5`, `10M`,
    // `100%`). The tail is deliberately permissive: unknown units cost a
    // missed Hint, never a false positive.
    let cut = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (head, tail) = s.split_at(cut);
    if head.is_empty()
        || !head.chars().any(|c| c.is_ascii_digit())
        || head.chars().filter(|&c| c == '.').count() > 1
        || (head.starts_with('.') || head.ends_with('.'))
    {
        return false;
    }
    tail.chars().all(|c| c.is_ascii_alphabetic() || c == '%')
}

fn is_valid_time(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower == "never" || lower == "infinite" {
        return true;
    }
    // Ignore whitespace so quoted multi-part durations never false-positive.
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return false;
    }
    // Clock form (`HH:MM:SS`, `MM:SS`).
    if compact.contains(':') {
        let parts: Vec<&str> = compact.split(':').collect();
        if parts.len() != 2 && parts.len() != 3 {
            return false;
        }
        return parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()));
    }
    // Bare number reads as seconds.
    if compact.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // Duration runs: `<digits><unit>` repeated (`1h30m`, `30s`, `10ms`).
    let mut rest = compact.as_str();
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return false;
        }
        rest = &rest[digits..];
        let units = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        if units == 0 {
            return false;
        }
        rest = &rest[units..];
        if !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

fn is_valid_mac(value: &str) -> bool {
    let s = value.trim();
    // RouterOS prints `:` separators; `-` is accepted as a common variant.
    // Mixed separators are rejected.
    let sep = if s.contains(':') && !s.contains('-') {
        ':'
    } else if s.contains('-') && !s.contains(':') {
        '-'
    } else {
        return false;
    };
    let parts: Vec<&str> = s.split(sep).collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.chars().all(|c| c.is_ascii_digit())
            && p.parse::<u32>().is_ok_and(|n| n <= 255)
    })
}

fn is_valid_ipv6(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Zone id (`fe80::1%ether1`): validate the address part only.
    let addr = match s.split_once('%') {
        Some((head, _)) => head,
        None => s,
    };
    if addr.is_empty()
        || !addr
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        || addr.contains(":::")
        || addr.matches("::").count() > 1
    {
        return false;
    }
    // One side of a `::` split: `8`-minus-compression groups, where a
    // trailing embedded IPv4 (`::ffff:1.2.3.4`) counts as two groups.
    fn side_groups(side: &str, allow_v4_tail: bool) -> Option<usize> {
        if side.is_empty() {
            return Some(0);
        }
        let parts: Vec<&str> = side.split(':').collect();
        let mut count = 0;
        for (i, part) in parts.iter().enumerate() {
            let is_last = i + 1 == parts.len();
            if is_last && allow_v4_tail && part.contains('.') {
                if !is_valid_ipv4(part) {
                    return None;
                }
                count += 2;
            } else {
                if part.is_empty() || part.len() > 4 || !part.chars().all(|c| c.is_ascii_hexdigit())
                {
                    return None;
                }
                count += 1;
            }
        }
        Some(count)
    }
    match addr.split_once("::") {
        Some((head, tail)) => {
            let (Some(h), Some(t)) = (side_groups(head, false), side_groups(tail, true)) else {
                return false;
            };
            h + t <= 7
        }
        None => {
            if addr.starts_with(':') || addr.ends_with(':') {
                return false;
            }
            side_groups(addr, true) == Some(8)
        }
    }
}

/// Validate one IP-typed value: a single address or prefix, or a
/// comma-separated list of them. Mirrors the charset discipline of the live
/// enrichment filters (`live.rs`): no control characters, no surrounding
/// garbage — then structural IPv4/IPv6 checks with no new dependencies.
fn is_valid_ip_field(value: &str) -> bool {
    if value.trim().is_empty() || value.chars().any(|c| c.is_control()) {
        return false;
    }
    let members: Vec<&str> = value.split(',').map(|m| m.trim()).collect();
    if members.iter().any(|m| m.is_empty()) {
        return false;
    }
    members.iter().all(|m| is_valid_ip_or_prefix(m))
}

fn is_valid_ip_or_prefix(value: &str) -> bool {
    let (host, prefix) = match value.split_once('/') {
        Some((h, p)) => (h.trim(), Some(p.trim())),
        None => (value.trim(), None),
    };
    if host.is_empty() {
        return false;
    }
    let is_v6 = host.contains(':');
    if !(if is_v6 {
        is_valid_ipv6(host)
    } else {
        is_valid_ipv4(host)
    }) {
        return false;
    }
    match prefix {
        None => true,
        Some(p) => {
            if p.is_empty() {
                return false;
            }
            // Netmask form (`192.168.1.0/255.255.255.0`) is tolerated.
            if p.contains('.') {
                return is_valid_ipv4(p);
            }
            let max: u32 = if is_v6 { 128 } else { 32 };
            p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u32>().is_ok_and(|n| n <= max)
        }
    }
}

/// Compute diagnostics for a document.
/// `uri` is unused for logic but kept for API compatibility (publish needs it).
pub fn compute_diagnostics(data: &MenuData, doc: &str, _uri: &str) -> Vec<Diagnostic> {
    // Cap large docs
    let bytes_to_process = if doc.len() > MAX_DIAG_BYTES {
        // Truncate at char boundary
        let idx = crate::floor_char_boundary(doc, MAX_DIAG_BYTES);
        &doc[..idx]
    } else {
        doc
    };

    let raw_lines: Vec<&str> = bytes_to_process.lines().collect();

    // Join backslash continuations FIRST, then cap: MAX_DIAG_LINES therefore
    // applies to the LOGICAL line count (one diagnostic unit per command), not
    // the physical line count. The pre-existing cap tests feed one-line
    // logicals, so their expectations remain valid.
    let logical_lines = build_logical_lines(&raw_lines);
    let iter_lines: &[LogicalLine] = if logical_lines.len() > MAX_DIAG_LINES {
        &logical_lines[..MAX_DIAG_LINES]
    } else {
        &logical_lines[..]
    };

    let mut diagnostics = Vec::new();
    let mut budget = crate::suggest::SuggestBudget::new();

    for ll in iter_lines {
        let line = ll.text.as_str();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip comments and global/inline script constructs.
        // RSC comments start with '#', global commands start with ':'.
        // Also skip lines like `}` or `{` or ".." parent navigation.
        if trimmed.starts_with('#') {
            continue;
        }
        // If line is purely a global command without menu path, skip menu-property checks?
        // We still want to skip diagnostics for lines that have no leading '/' and no menu path.
        // But we need to parse to determine path.
        // For lines starting with ":" we treat as script, not menu command → no menu diagnostics.
        if trimmed.starts_with(':') {
            continue;
        }
        if trimmed == "}" || trimmed == "{" || trimmed == ".." {
            continue;
        }

        // Quick check: does line contain '/'? If not, likely not a menu command, skip unknown-menu check.
        // But we still parse to detect path.
        let ctx = crate::parse_line(data, line);

        // If path is empty and command is None and no properties, skip
        if ctx.path.is_empty() && ctx.command.is_none() && ctx.properties.is_empty() {
            // Might be a bare command like `print` without path – skip diagnostics for now.
            continue;
        }

        // ---- Rule 1: Unknown menu path ----
        if !ctx.path.is_empty() {
            // O(1) membership: exact menu OR a proper ancestor prefix of a
            // known menu (precomputed at load time). This replaces the former
            // two linear scans over all menus per logical line — each with a
            // format! allocation per element. The children index remains the
            // authoritative structure for context RESOLUTION in parse_line;
            // this set only answers "is this prefix known?".
            let is_known = data.menu_by_path.contains_key(&ctx.path)
                || data.ancestor_prefixes.contains(&ctx.path);
            if !is_known && let Some((start_char, end_char)) = find_substring_range(line, &ctx.path)
            {
                let suggestion = budget.candidate(&ctx.path, data.menu_by_path.keys());
                diagnostics.push(Diagnostic {
                    range: ll.map_range(start_char, end_char),
                    severity: Some(severity::WARNING),
                    code: Some("unknown-menu".to_string()),
                    source: Some(DIAGNOSTIC_SOURCE.to_string()),
                    message: with_suggestion(format!("Unknown menu '{}'", ctx.path), suggestion),
                });
                // If menu unknown, don't emit further property diagnostics for this line
                // to avoid cascading false positives.
                continue;
            }
        }

        // Need menu entry for remaining rules; if path unknown or not a known menu, skip remaining unless path is known implicitly
        // For implicit parents (no direct menu entry but valid as parent), we skip property checks because they have no arguments.
        let menu = if !ctx.path.is_empty() {
            data.menu_by_path.get(&ctx.path)
        } else {
            None
        };

        // If menu is None but path is implicit parent, we will have is_known true but no menu entry; then property checks should be skipped (no args expected).
        // For unknown property / missing required, we require a known Directory menu with arguments.

        // ---- Tokenize with spans for duplicate and precise range detection ----
        // Property occurrences are recorded DURING tokenization, so diagnostic
        // ranges point at the exact occurrence instead of the first textual
        // match (which could sit inside the menu path or an earlier value).
        let tokens = crate::tokenize_with_spans(line);
        let mut key_counts: HashMap<String, usize> = HashMap::new();
        // key → ordered byte spans (start, end) of each KEY occurrence.
        let mut key_spans: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        // key → (value text, full-token span) of the LAST occurrence.
        let mut key_values: HashMap<String, (String, (usize, usize))> = HashMap::new();

        // Bracket regions (`[find ...]`, `[/sys/clock/get ...]`) are inert:
        // inner `key=value` pairs must not leak into outer Rule 2/4 state,
        // mirroring `parse_line` over the same single token stream.
        let mut depth: u32 = 0;
        for token in &tokens {
            let (opens, closes) = crate::parser::bracket_counts(&token.text);
            if depth > 0 || opens > 0 {
                depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
                continue;
            }
            if let Some((key, value)) = crate::parser::split_key_value(&token.text) {
                let eq_idx = key.len();
                *key_counts.entry(key.to_string()).or_insert(0) += 1;
                key_spans
                    .entry(key.to_string())
                    .or_default()
                    .push((token.start, token.start + eq_idx));
                key_values.insert(
                    key.to_string(),
                    (value.to_string(), (token.start, token.end)),
                );
            }
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
        }

        // Whole-token span of the command verb (first token whose text equals
        // the parsed command). Property values always contain '=' so they can
        // never collide with a bare verb; path tokens start with '/'.
        let command_span = ctx.command.as_deref().and_then(|cmd| {
            tokens
                .iter()
                .find(|t| t.text == cmd)
                .map(|t| (t.start, t.end))
        });

        // ---- Rule 8: Unknown command verb ----
        // Fires only when the path itself is known (exact menu or ancestor
        // prefix); unknown paths already produced Rule 1 and return early
        // above, so no cascade. Verbs are case-insensitive on RouterOS.
        // Menu-specific verbs outside STANDARD_VERBS (e.g. `run` on
        // /system/script, `info`/`warning`/`error`/`debug` on /log) are
        // allowlisted so valid device commands stay silent. `unset` clears
        // an optional property (`/ip/address unset 0 comment`) and is a
        // real RouterOS verb on Directory menus.
        const MENU_SPECIFIC_VERBS: &[&str] = &[
            "run", "info", "warning", "error", "debug", "monitor", "unset",
        ];
        if let Some(cmd) = ctx.command.as_deref()
            && !ctx.path.is_empty()
            && (data.menu_by_path.contains_key(&ctx.path)
                || data.ancestor_prefixes.contains(&ctx.path))
            && !MenuData::STANDARD_VERBS
                .iter()
                .any(|v| v.eq_ignore_ascii_case(cmd))
            && !MENU_SPECIFIC_VERBS
                .iter()
                .any(|v| v.eq_ignore_ascii_case(cmd))
            && let Some((s, e)) = command_span
        {
            let suggestion = budget.candidate(
                cmd,
                MenuData::STANDARD_VERBS
                    .iter()
                    .copied()
                    .chain(MENU_SPECIFIC_VERBS.iter().copied()),
            );
            diagnostics.push(Diagnostic {
                range: ll.map_range(s, e),
                severity: Some(severity::WARNING),
                code: Some("unknown-command".to_string()),
                source: Some(DIAGNOSTIC_SOURCE.to_string()),
                message: with_suggestion(
                    format!("Unknown command '{}' for '{}'", cmd, ctx.path),
                    suggestion,
                ),
            });
        }

        // ---- Rule 4: Duplicate property ----
        // Highlight the SECOND occurrence precisely: the first may be the
        // legitimate definition; repeats are the anomaly. Spans come from
        // tokenization, so a key that also appears inside the menu path (e.g.
        // "address" in "/ip/address") never gets squiggled by accident.
        for (key, count) in &key_counts {
            if *count > 1
                && let Some(&(s, e)) = key_spans
                    .get(key)
                    .and_then(|spans| spans.get(1).or_else(|| spans.first()))
            {
                diagnostics.push(Diagnostic {
                    range: ll.map_range(s, e),
                    severity: Some(severity::WARNING),
                    code: Some("duplicate-property".to_string()),
                    source: Some(DIAGNOSTIC_SOURCE.to_string()),
                    message: format!("Duplicate property '{}'", key),
                });
            }
        }

        // If we have a known menu with arguments/flags, continue with property checks
        if let Some(menu) = menu {
            // Build allowed property set
            let mut allowed: HashSet<String> = HashSet::new();
            for arg in &menu.arguments {
                allowed.insert(arg.name.clone());
            }
            for flag in &menu.flags {
                allowed.insert(flag.name.clone());
            }
            for ro in &menu.read_only {
                allowed.insert(ro.name.clone());
            }

            // ---- Rule 2: Unknown property ----
            // The named `unset` form (`... unset 0 value-name=<prop>`) is
            // owned by Rule 10 below: `value-name` is a verb-level pseudo-key,
            // not a menu property, so it never flags here on `unset` lines.
            let is_unset = ctx
                .command
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("unset"));
            for (key, spans) in &key_spans {
                if is_unset && key == "value-name" {
                    continue;
                }
                if !allowed.contains(key)
                    && let Some(&(s, e)) = spans.first()
                {
                    let suggestion = budget.candidate(key, allowed.iter());
                    diagnostics.push(Diagnostic {
                        range: ll.map_range(s, e),
                        severity: Some(severity::WARNING),
                        code: Some("unknown-property".to_string()),
                        source: Some(DIAGNOSTIC_SOURCE.to_string()),
                        message: with_suggestion(
                            format!("Unknown property '{}' for '{}'", key, ctx.path),
                            suggestion,
                        ),
                    });
                }
            }

            // ---- Rule 3: Missing required property for Directory menus with add/set ----
            // `set` targets an existing entry via a selector (`set 0 ...`,
            // `set *AB12 ...`, `set [find ...] ...`): required-creation args
            // do not apply there. `add` always creates, so it is unchanged.
            if (menu.menu_type == "Directory" || menu.menu_type == "Settings Directory")
                && ctx
                    .command
                    .as_deref()
                    .is_some_and(|c| c == "add" || c == "set")
                && !(ctx.command.as_deref() == Some("set") && line_has_set_selector(&tokens, "set"))
            {
                for arg in &menu.arguments {
                    if arg.required && !key_counts.contains_key(&arg.name) {
                        // Range: point at the command verb token, falling back
                        // to the start of the line if no whole-token match.
                        let (s, e) = command_span.unwrap_or((0, line.len().min(8)));
                        diagnostics.push(Diagnostic {
                            range: ll.map_range(s, e),
                            severity: Some(severity::WARNING),
                            code: Some("missing-required".to_string()),
                            source: Some(DIAGNOSTIC_SOURCE.to_string()),
                            message: format!(
                                "Missing required property '{}' for '{} {}'",
                                arg.name,
                                ctx.path,
                                ctx.command.as_deref().unwrap_or("")
                            ),
                        });
                    }
                }
            }

            // ---- Rule 5: Invalid enum value for enum properties ----
            // Members come from the embedded enum_values list when present
            // (complete even for display-truncated types); the type-string
            // parser is only a fallback. When neither yields members, the
            // check stays silent rather than guessing.
            for (key, (value, span)) in &key_values {
                // Find argument definition
                if let Some(arg) = menu.arguments.iter().find(|a| a.name == *key)
                    && arg.arg_type.starts_with("enum")
                {
                    let allowed_vals = arg.enum_members();
                    if !allowed_vals.is_empty() {
                        // Strip outer quotes and whitespace; empty remains non-error (completion).
                        let raw = value.trim().trim_matches('"').trim_matches('\'');
                        let val = raw.trim();
                        if val.is_empty() {
                            continue;
                        }
                        // Comma-separated enum lists (e.g. address-list=foo,bar):
                        // split on ',' and trim each member; single-value path keeps
                        // strict equality, list path is lenient — only emit when NO
                        // member matches (reduces false positives for mixed lists).
                        let is_valid = if val.contains(',') {
                            let members: Vec<&str> = val
                                .split(',')
                                .map(|s| s.trim().trim_matches('"').trim_matches('\'').trim())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if members.is_empty() {
                                false
                            } else {
                                members
                                    .iter()
                                    .any(|m| allowed_vals.iter().any(|v| v == *m || v.trim() == *m))
                            }
                        } else {
                            allowed_vals.iter().any(|v| v == val || v.trim() == val)
                        };
                        if !is_valid {
                            // Narrow the recorded token span to the value part
                            // only (skip "key="), keeping any quotes in range.
                            let s = span.0 + key.len() + 1;
                            let e = span.1.max(s);
                            let suggestion = budget.candidate(val, allowed_vals.iter());
                            diagnostics.push(Diagnostic {
                                range: ll.map_range(s, e),
                                severity: Some(severity::WARNING),
                                code: Some("invalid-enum-value".to_string()),
                                source: Some(DIAGNOSTIC_SOURCE.to_string()),
                                message: with_suggestion(
                                    format!(
                                        "Invalid value '{}' for '{}' (expected one of: {})",
                                        val,
                                        key,
                                        allowed_vals.join(" | ")
                                    ),
                                    suggestion,
                                ),
                            });
                        }
                    }
                }
            }

            // ---- Rule 9: Typed value shape checks (Hint-only) ----
            // Syntactic plausibility only — never Error, never Warning — so
            // an incomplete upstream type table degrades to silence instead
            // of false positives. `enum` types stay owned by Rule 5 above.
            for (key, (value, span)) in &key_values {
                // The named `unset` form is owned by Rule 10 below.
                if key == "value-name" {
                    continue;
                }
                let Some(arg) = menu.arguments.iter().find(|a| a.name == *key) else {
                    continue;
                };
                if let Some(hint) = check_typed_value(arg, value, &mut budget) {
                    let s = span.0 + key.len() + 1;
                    let e = span.1.max(s);
                    diagnostics.push(Diagnostic {
                        range: ll.map_range(s, e),
                        severity: Some(severity::HINT),
                        code: Some(hint.code.to_string()),
                        source: Some(DIAGNOSTIC_SOURCE.to_string()),
                        message: with_suggestion(hint.message(key, value), hint.suggestion),
                    });
                }
            }

            // ---- Rule 10: `unset` of a non-unsettable property (Hint) ----
            // RouterOS clears optional values positionally
            // (`/ip/address unset 0 comment`) or via the named form
            // (`... unset 0 value-name=comment`). Warn only when the dataset
            // entry explicitly carries `unset=false`; unknown names (numbers,
            // interface names, selectors) stay silent.
            if ctx
                .command
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case("unset"))
            {
                // Positional targets: bare tokens after the verb that are not
                // path segments, selectors (`0`, `*AB12`, `[find ...]`), or
                // `key=value` pairs.
                let verb_idx = tokens.iter().position(|t| {
                    ctx.command
                        .as_deref()
                        .is_some_and(|c| t.text.eq_ignore_ascii_case(c))
                });
                if let Some(vi) = verb_idx {
                    for token in tokens.iter().skip(vi + 1) {
                        let text = token.text.as_str();
                        if text.starts_with('/') {
                            continue;
                        }
                        if crate::parser::split_key_value(text).is_some() || text.contains('=') {
                            continue;
                        }
                        let (opens, _) = crate::parser::bracket_counts(text);
                        if opens > 0 || text.contains(['[', ']']) {
                            continue;
                        }
                        if is_index_selector(text) {
                            continue;
                        }
                        // Space-separated submenu spelling (`/ip firewall ...`)
                        // leaks path segments as bare tokens; never flag those.
                        if ctx
                            .path
                            .split('/')
                            .any(|seg| seg.eq_ignore_ascii_case(text))
                        {
                            continue;
                        }
                        if let Some(arg) = menu.arguments.iter().find(|a| a.name == text)
                            && !arg.unset
                        {
                            diagnostics.push(Diagnostic {
                                range: ll.map_range(token.start, token.end),
                                severity: Some(severity::HINT),
                                code: Some("non-unsettable-property".to_string()),
                                source: Some(DIAGNOSTIC_SOURCE.to_string()),
                                message: format!(
                                    "Property '{}' cannot be unset (unsettable: no) for '{}'",
                                    text, ctx.path
                                ),
                            });
                        }
                    }
                }
                // Named form: `value-name=<property>`.
                if let Some((named_value, named_span)) = key_values.get("value-name") {
                    let target = named_value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .trim()
                        .to_string();
                    if !target.is_empty()
                        && !target.contains(['$', '[', ']', '(', ')'])
                        && let Some(arg) = menu.arguments.iter().find(|a| a.name == target)
                        && !arg.unset
                    {
                        let s = named_span.0 + "value-name".len() + 1;
                        let e = named_span.1.max(s);
                        diagnostics.push(Diagnostic {
                            range: ll.map_range(s, e),
                            severity: Some(severity::HINT),
                            code: Some("non-unsettable-property".to_string()),
                            source: Some(DIAGNOSTIC_SOURCE.to_string()),
                            message: format!(
                                "Property '{target}' cannot be unset (unsettable: no) for '{}'",
                                ctx.path
                            ),
                        });
                    }
                }
            }

            // ---- Rule 11: read-only column written via add/set (Info) ----
            // `read_only` entries are output columns (e.g. `/log` stats);
            // assigning them on `add`/`set` cannot take effect.
            if ctx
                .command
                .as_deref()
                .is_some_and(|c| c == "add" || c == "set")
            {
                for (key, spans) in &key_spans {
                    if menu.read_only.iter().any(|r| r.name == *key)
                        && let Some(&(s, e)) = spans.first()
                    {
                        diagnostics.push(Diagnostic {
                            range: ll.map_range(s, e),
                            severity: Some(severity::INFORMATION),
                            code: Some("read-only-write".to_string()),
                            source: Some(DIAGNOSTIC_SOURCE.to_string()),
                            message: format!(
                                "Property '{}' is read-only and cannot be set with '{}' (output column only)",
                                key,
                                ctx.command.as_deref().unwrap_or("")
                            ),
                        });
                    }
                }
            }
        }
    }

    // Bound the otherwise uncapped semantic loop: a single logical line
    // with thousands of distinct unknown keys would otherwise yield one
    // heap `Diagnostic` per key. Truncate BEFORE the syntax extend + hint
    // push below so the truncation hint still fires and the syntax family
    // still appends within its own cap.
    let count_truncated = diagnostics.len() > MAX_DIAGNOSTICS;
    diagnostics.truncate(MAX_DIAGNOSTICS);

    // ---- Truncation hint (O-01) -----------------------------------------
    // When the document was capped by MAX_DIAG_BYTES, MAX_DIAG_LINES, or
    // MAX_DIAGNOSTICS, emit
    // a single Information diagnostic so the user understands some issues
    // beyond the limit are not shown. The capped slicing above stays intact;
    // this adds at most one extra diagnostic (bounded).
    let bytes_truncated = doc.len() > MAX_DIAG_BYTES;
    let lines_truncated = logical_lines.len() > MAX_DIAG_LINES;
    let truncation_hint = if bytes_truncated || lines_truncated || count_truncated {
        let message = match (lines_truncated, bytes_truncated, count_truncated) {
            (true, true, _) => format!(
                "Diagnostic truncated: showing first {} of {} lines (and {} of {} bytes) — some issues beyond limit not shown",
                MAX_DIAG_LINES,
                logical_lines.len(),
                MAX_DIAG_BYTES,
                doc.len()
            ),
            (true, false, _) => format!(
                "Diagnostic truncated: showing first {} of {} lines — some issues beyond limit not shown",
                MAX_DIAG_LINES,
                logical_lines.len()
            ),
            (false, true, _) => format!(
                "Diagnostic truncated: showing first {} of {} bytes — some issues beyond limit not shown",
                MAX_DIAG_BYTES,
                doc.len()
            ),
            (false, false, true) => format!(
                "Diagnostic truncated: showing first {} diagnostics — some issues beyond limit not shown",
                MAX_DIAGNOSTICS
            ),
            (false, false, false) => unreachable!(),
        };
        Some(Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
            severity: Some(severity::INFORMATION),
            code: Some("truncated".to_string()),
            source: Some(DIAGNOSTIC_SOURCE.to_string()),
            message,
        })
    } else {
        None
    };

    // ---- Syntactic structure rules (unclosed/unmatched braces, quotes) ----
    // Computed over the FULL document, deliberately NOT the byte-capped slice
    // used by the menu rules above: amputating the tail could hide the `}` or
    // closing quote that balances earlier content and fabricate errors for a
    // large well-formed document. The walk is one linear scan (same cost
    // profile as folding ranges) and the server already caps tracked
    // documents at 5 MiB.
    diagnostics.extend(syntax_diagnostics(doc));

    if let Some(hint) = truncation_hint {
        diagnostics.push(hint);
    }

    diagnostics
}

// ── Syntactic structure rules ──────────────────────────────────────
//
// Detect plain syntax breakage the menu-semantics rules cannot see. All
// three diagnostics below derive from ONE stack pass over the shared
// [`crate::parser::walk_structure`] events — the exact same walk folding
// uses — so "what is inside a string/comment" has a single source of truth.
//
// Conservative by design: false positives here are worse than silence, so
// anything ambiguous (depth beyond MAX_BRACE_DEPTH, an unterminated string
// that swallows the rest of the document) reports at most the root cause and
// never cascades.

/// Kind of one syntactic finding, resolved to its wire `code` and fixed
/// message only when the surviving findings are materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntaxFindingKind {
    UnclosedBrace,
    UnmatchedBrace,
    UnclosedQuote,
}

impl SyntaxFindingKind {
    fn code(self) -> &'static str {
        match self {
            Self::UnclosedBrace => "unclosed-brace",
            Self::UnmatchedBrace => "unmatched-brace",
            Self::UnclosedQuote => "unclosed-quote",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::UnclosedBrace => "Brace '{' opened here is never closed",
            Self::UnmatchedBrace => "Unmatched '}': no '{' is open at this point",
            Self::UnclosedQuote => "Quoted string opened here is never closed",
        }
    }
}

/// One deferred syntactic finding: position plus kind, WITHOUT the built
/// Diagnostic. The `message` String (plus code/source Strings) is the heap
/// cost that made the former build-everything-then-truncate walk allocate
/// one full Diagnostic per event on pathological input.
struct SyntaxFinding {
    line: usize,
    character: usize,
    kind: SyntaxFindingKind,
}

impl SyntaxFinding {
    /// Materialize the wire diagnostic for one finding. Called only for the
    /// ≤ [`MAX_SYNTAX_DIAGNOSTICS`] survivors, so heap traffic per publish
    /// stays bounded regardless of document size.
    fn into_diagnostic(self) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: self.line as u32,
                    character: self.character as u32,
                },
                end: Position {
                    line: self.line as u32,
                    character: self.character as u32 + 1,
                },
            },
            severity: Some(severity::ERROR),
            code: Some(self.kind.code().to_string()),
            source: Some(DIAGNOSTIC_SOURCE.to_string()),
            message: self.kind.message().to_string(),
        }
    }
}

/// Syntax rules 6–7 (see module header): unclosed `{` (plus its
/// `unmatched-brace` companion), and unterminated quoted strings.
///
/// Positions come from the shared walker in byte coordinates and are mapped
/// to wire encoding by the usual boundary conversion (`convert_diagnostic_ranges`),
/// exactly like every other rule's output. Output is deterministic: sorted by
/// document position ("oldest first"), capped at [`MAX_SYNTAX_DIAGNOSTICS`]
/// plus one explicit `truncated` footer when the cap drops findings.
///
/// Memory-bounded by construction: the walk records lightweight
/// (position, kind) pairs only; sorting, truncation, and Diagnostic
/// materialization happen afterwards over those records. A pathological
/// document (megabytes of stray `}`) therefore costs a small struct per
/// finding instead of a full Diagnostic allocation per event. Sorting
/// `(line, character)` usize pairs is order-identical to sorting
/// `(range.start.line, range.start.character)` u32 pairs (monotone casts),
/// and `sort_by_key` is stable, so emitted output matches the former
/// implementation byte-for-byte — including tie order among findings that
/// share a position. No ordering invariant between unclosed opens and
/// unmatched closes is assumed: findings from all three sources are sorted
/// globally by the same key the old code used.
fn syntax_diagnostics(doc: &str) -> Vec<Diagnostic> {
    let mut findings: Vec<SyntaxFinding> = Vec::new();
    // Stack of (line, character) positions of `{` still considered open.
    let mut opens: Vec<(usize, usize)> = Vec::new();
    // Latched once the stack overflows MAX_BRACE_DEPTH (pathological input).
    // Dropped opens would make later legitimate closers look unmatched, so
    // stray-close reporting stops for the rest of the document: silence
    // beats false positives on absurd input.
    let mut saturated = false;

    crate::walk_structure(doc, |ev| match ev {
        StructureEvent::OpenBrace { line, character } => {
            if opens.len() < crate::MAX_BRACE_DEPTH {
                opens.push((line, character));
            } else {
                saturated = true;
            }
        }
        StructureEvent::CloseBrace { line, character } => {
            if opens.pop().is_none() && !saturated {
                findings.push(SyntaxFinding {
                    line,
                    character,
                    kind: SyntaxFindingKind::UnmatchedBrace,
                });
            }
        }
        StructureEvent::UnterminatedQuote { line, character } => {
            // One error at the OPENING quote; everything after it is treated
            // as string content by the shared walker, so this never cascades.
            findings.push(SyntaxFinding {
                line,
                character,
                kind: SyntaxFindingKind::UnclosedQuote,
            });
        }
    });

    // Remaining opens are unclosed braces. The stack pops innermost-first,
    // so drain it reversed to recover document order.
    for (line, character) in opens.into_iter().rev() {
        findings.push(SyntaxFinding {
            line,
            character,
            kind: SyntaxFindingKind::UnclosedBrace,
        });
    }

    // Deterministic ordering across the three sources, then an explicit
    // cap keeping the OLDEST ten (document order) plus a `truncated`
    // Information footer naming the dropped remainder — mirroring the
    // semantic truncation path so the 11th error is acknowledged, not
    // silent. Full Diagnostics — with their heap messages — are built
    // only for the survivors.
    findings.sort_by_key(|f| (f.line, f.character));
    let total = findings.len();
    let dropped = total.saturating_sub(MAX_SYNTAX_DIAGNOSTICS);
    findings.truncate(MAX_SYNTAX_DIAGNOSTICS);
    let mut out: Vec<Diagnostic> = findings
        .into_iter()
        .map(SyntaxFinding::into_diagnostic)
        .collect();
    if dropped > 0 {
        out.push(Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 0,
                },
            },
            severity: Some(severity::INFORMATION),
            code: Some("truncated".to_string()),
            source: Some(DIAGNOSTIC_SOURCE.to_string()),
            message: format!(
                "Diagnostic truncated: showing first {MAX_SYNTAX_DIAGNOSTICS} of {total} syntax diagnostics (+{dropped} more - see full list) — some issues beyond limit not shown"
            ),
        });
    }
    out
}

// ── RouterOS backslash line continuation ──────────────────────────
//
// RouterOS joins a physical line ending in an unescaped trailing `\` with the
// next physical line (the newline is removed, no separator is inserted).
// Diagnostics must therefore parse *logical* lines while still reporting
// positions in original physical coordinates.

/// Returns the byte index where the "continuation body" of `line` ends — that
/// is, the start of the trailing odd run of backslashes within the effective
/// content — or [`None`] when the line does not continue onto the next one.
///
/// Effective content rules:
/// - Inside `"..."` / `'...'`, a `\` escapes the next byte.
/// - An unquoted `#` starts a comment: nothing after it can continue a line,
///   so scanning stops there.
/// - Trailing whitespace is ignored; then the consecutive trailing backslash
///   run is counted: odd → continuation (`\`), even → escaped literal (`\\`).
///
/// The comment cut is delegated to
/// [`crate::parser::effective_content_end`], the single source of truth for
/// the unquoted-`#` rule, so parser and diagnostics cannot drift apart.
fn continuation_body_end(line: &str) -> Option<usize> {
    // The unquoted-'#' cut is shared with walk_structure/build_before_cursor
    // (see crate::parser::effective_content_end).
    let content_end = crate::parser::effective_content_end(line);
    // Note: ASCII quote/backslash/hash bytes only occur as standalone bytes in
    // valid UTF-8, so `content_end` is always a char boundary here.
    let content = &line[..content_end];
    let trimmed_len = content.trim_end().len();
    // Backslashes are 1 byte each, so counting chars == counting bytes.
    let run_len = content[..trimmed_len]
        .chars()
        .rev()
        .take_while(|&c| c == '\\')
        .count();
    if run_len % 2 == 1 {
        Some(trimmed_len - run_len)
    } else {
        None
    }
}

/// Returns true if this physical line continues onto the next line via a
/// trailing unescaped backslash (RouterOS line continuation).
pub(crate) fn has_line_continuation(line: &str) -> bool {
    continuation_body_end(line).is_some()
}

/// A slice of one physical line contributed to a [`LogicalLine`].
#[derive(Debug)]
pub(crate) struct Segment {
    /// Byte offset of this chunk within [`LogicalLine::text`].
    text_start: usize,
    /// Byte length of this chunk (segments tile `text` contiguously).
    len: usize,
    /// Index of the source physical line in the original document.
    phys_line: usize,
}

/// One RouterOS command: physical lines joined at `\` continuations.
///
/// Segments map offsets in the joined text back to original document
/// coordinates. Each segment starts at character 0 of its physical line:
/// continued bodies are prefixes of their physical line and final lines are
/// appended whole, so a logical offset inside a segment maps 1:1 onto a byte
/// offset within that physical line.
///
/// Crate-visible so sibling modules (symbols, folding) can reuse the
/// continuation-aware join; fields stay private — access goes through
/// [`LogicalLine::text`], [`LogicalLine::first_physical_line`],
/// [`LogicalLine::last_physical_line`] and [`LogicalLine::map_range`].
#[derive(Debug, Default)]
pub(crate) struct LogicalLine {
    pub(crate) text: String,
    pub(crate) segments: Vec<Segment>,
}

impl LogicalLine {
    /// Joined logical text (all physical chunks concatenated).
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Physical line index of the first contributing chunk.
    ///
    /// Note: a continuing line whose body before `\` is empty contributes no
    /// chunk, so this can point past the true visual start of very degenerate
    /// continuations (e.g. a lone `\` line). Consumers only compare it with
    /// [`Self::last_physical_line`], where that bias is harmless.
    pub(crate) fn first_physical_line(&self) -> usize {
        self.segments.first().map(|s| s.phys_line).unwrap_or(0)
    }

    /// Physical line index of the last contributing chunk.
    pub(crate) fn last_physical_line(&self) -> usize {
        self.segments.last().map(|s| s.phys_line).unwrap_or(0)
    }

    fn push_chunk(&mut self, chunk: &str, phys_line: usize) {
        if chunk.is_empty() {
            return;
        }
        self.segments.push(Segment {
            text_start: self.text.len(),
            len: chunk.len(),
            phys_line,
        });
        self.text.push_str(chunk);
    }

    /// Map a byte offset in the joined text to a [`Position`] in original
    /// document coordinates. Out-of-bounds offsets are clamped defensively.
    pub(crate) fn map_pos(&self, offset: usize) -> Position {
        let offset = crate::floor_char_boundary(&self.text, offset.min(self.text.len()));
        let idx = self.segments.partition_point(|s| s.text_start <= offset);
        let Some(seg) = idx.checked_sub(1).and_then(|i| self.segments.get(i)) else {
            return Position {
                line: 0,
                character: 0,
            };
        };
        debug_assert!(
            offset >= seg.text_start && offset <= seg.text_start + seg.len,
            "selected segment must contain the clamped offset"
        );
        Position {
            line: seg.phys_line as u32,
            character: (offset - seg.text_start) as u32,
        }
    }

    /// Map a byte range in the joined text to a [`Range`] in original document
    /// coordinates. Start and end may land on different physical lines (LSP
    /// allows multi-line ranges), which happens when a token spans a join.
    pub(crate) fn map_range(&self, start: usize, end: usize) -> Range {
        Range {
            start: self.map_pos(start),
            end: self.map_pos(end.max(start)),
        }
    }

    /// Map a byte offset within physical line `phys_line` onto the joined
    /// logical text. Inverse of the segment layout: each chunk starts at
    /// character 0 of its physical line, so the logical offset is the
    /// segment's `text_start` plus the offset, clamped to the chunk length
    /// (a cursor sitting past a continuation body's cut point — e.g. on or
    /// after the trailing `\` — lands at the body's end).
    ///
    /// Returns `None` when this physical line contributes no chunk to the
    /// logical line (the empty-body degenerate case).
    pub(crate) fn logical_offset_from_physical(
        &self,
        phys_line: usize,
        byte_in_phys_line: usize,
    ) -> Option<usize> {
        let seg = self.segments.iter().find(|s| s.phys_line == phys_line)?;
        Some(seg.text_start + byte_in_phys_line.min(seg.len))
    }
}

/// Join raw physical lines into logical lines at `\` continuations.
///
/// Join semantics mirror RouterOS exactly:
/// - A continuing line contributes everything before its trailing backslash
///   run (`continuation_body_end`); preceding whitespace is kept so it acts as
///   a normal token separator for the next token.
/// - The next physical line's text is appended **without** inserting any
///   separator (RouterOS removes the newline). Its leading whitespace — when
///   present — provides separation; when absent, tokens genuinely concatenate,
///   just like on a real router. This keeps split quoted strings intact as one
///   token, e.g. `url="https://x\` + `/main/hosts/pro.txt"`.
/// - The final line of a logical line contributes its whole text.
///
/// Runs in O(n) over the input bytes.
///
/// Detection and slicing run as two passes over continuing lines only: the
/// predicate [`has_line_continuation`] decides whether to join, then
/// [`continuation_body_end`] yields the exact body cut point (guaranteed
/// `Some` at that point).
pub(crate) fn build_logical_lines(raw_lines: &[&str]) -> Vec<LogicalLine> {
    let mut logicals = Vec::new();
    let mut current = LogicalLine::default();

    for (idx, raw) in raw_lines.iter().enumerate() {
        let mut continues = false;
        if has_line_continuation(raw)
            && let Some(body_end) = continuation_body_end(raw)
        {
            // Contribute everything before the trailing backslash run;
            // preceding whitespace is kept as the token separator.
            current.push_chunk(&raw[..body_end], idx);
            continues = true;
        }
        if !continues {
            // Final physical line of this logical line: contribute whole text.
            current.push_chunk(raw, idx);
            logicals.push(std::mem::take(&mut current));
        }
    }

    // Flush a dangling continuation at EOF (document ends with '\').
    if !current.segments.is_empty() {
        logicals.push(current);
    }

    logicals
}

/// Crate-visible wrapper so sibling modules (symbols, folding) can join
/// physical lines into logical lines without duplicating the RouterOS
/// continuation semantics documented on the inner function.
pub(crate) fn logical_lines(doc: &str) -> Vec<LogicalLine> {
    let raw: Vec<&str> = doc.lines().collect();
    build_logical_lines(&raw)
}

/// Find the logical line whose inclusive physical-line span
/// `[first_physical_line, last_physical_line]` covers `phys_line`, as its
/// INDEX in `logicals`.
///
/// Logical lines partition physical lines in order, so at most one matches.
/// Callers that need to correlate a hit with an index into `logicals`
/// (navigation) use this; [`covering_logical_line`] wraps it for callers
/// that only want the line itself.
///
/// Returns `None` when no logical line covers the line (out of range, or the
/// empty-body continuation degenerate case where no token can exist).
pub(crate) fn covering_logical_line_index(
    logicals: &[LogicalLine],
    phys_line: usize,
) -> Option<usize> {
    logicals.iter().position(|ll| {
        ll.first_physical_line() <= phys_line && phys_line <= ll.last_physical_line()
    })
}

/// Find the logical line whose inclusive physical-line span
/// `[first_physical_line, last_physical_line]` covers `phys_line`.
///
/// Index-returning twin: [`covering_logical_line_index`].
///
/// Returns `None` when no logical line covers the line (out of range, or the
/// empty-body continuation degenerate case where no token can exist).
pub(crate) fn covering_logical_line(
    logicals: &[LogicalLine],
    phys_line: usize,
) -> Option<&LogicalLine> {
    covering_logical_line_index(logicals, phys_line).map(|idx| &logicals[idx])
}

/// Resolve the [`crate::menus::MenuEntry`] governing the RouterOS command
/// that contains physical line `phys_line`, using **exactly** the pipeline
/// [`compute_diagnostics`] uses: continuation-aware logical-line join,
/// then `parse_line`, then a `menu_by_path` lookup.
///
/// Sharing this resolver matters for consumers that repair our own
/// diagnostics (textDocument/codeAction quick-fixes): they must agree
/// with the diagnostic about which menu a line belongs to, and must never
/// invent a second, diverging notion of line resolution.
///
/// Returns `None` when:
/// - `phys_line` belongs to no logical line (out of range, or inside the
///   empty-body continuation degenerate case where no token can exist),
/// - the command parses to no menu path,
/// - or the path has no direct `menu_by_path` entry (implicit parents such
///   as `/ip/firewall` carry no property table of their own).
pub(crate) fn resolve_menu_for_line<'a>(
    data: &'a MenuData,
    logicals: &[LogicalLine],
    phys_line: usize,
) -> Option<&'a crate::menus::MenuEntry> {
    let line = covering_logical_line(logicals, phys_line)?;
    let ctx = crate::parse_line(data, line.text());
    data.menu_by_path.get(&ctx.path)
}

fn find_substring_range(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .find(needle)
        .map(|start| (start, start + needle.len()))
}

/// True when `tok` looks like a RouterOS entry selector for `set`:
/// a `*ID` reference (`*0`, `*AB12`) or a numeric index list (`0`, `0,1`).
fn is_index_selector(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    if tok.starts_with('*') {
        return tok.len() > 1;
    }
    !tok.is_empty()
        && tok.chars().all(|c| c.is_ascii_digit() || c == ',')
        && tok.chars().any(|c| c.is_ascii_digit())
}

/// True when the logical line targets `set` at an existing entry via a
/// selector: any non-path, non-verb, non-property token carrying an unquoted
/// `[` (e.g. `[find ...]`, detected via [`crate::parser::bracket_counts`]
/// so quoted brackets stay inert) or a numeric/ID selector (`0`, `*AB12`).
fn line_has_set_selector(tokens: &[crate::parser::SpanToken], verb: &str) -> bool {
    for t in tokens {
        if t.text.starts_with('/') {
            continue;
        }
        if t.text == verb {
            continue;
        }
        if crate::parser::split_key_value(&t.text).is_some() {
            continue;
        }
        let (opens, _) = crate::parser::bracket_counts(&t.text);
        if opens > 0 {
            return true;
        }
        if is_index_selector(&t.text) {
            return true;
        }
    }
    false
}

// ── Syntactic structure rules (unclosed braces / quotes) ───────────
//
// Coverage for rules 6–8. Docs deliberately favor `:`-prefixed script lines
// (skipped by the menu rules) so total-count assertions isolate the syntax
// pipeline; one interaction test proves both rule families coexist in the
// same publish.
