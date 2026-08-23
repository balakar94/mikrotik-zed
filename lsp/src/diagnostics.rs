// ── Diagnostics for RSC language server ───────────────────────────
//
// Provides pull and push diagnostics for MikroTik RouterOS Script files.
// Rules:
//  1. Unknown menu path: Warning if path not in menu_by_path nor child_names_by_parent (including implicit parents)
//  2. Unknown property: Warning if property key not in menu.arguments/flags/read_only
//  3. Missing required property: Info for Directory menus when `add`/`set` missing required args
//  4. Duplicate property: Warning if same key appears twice
//  5. Type hint: Hint for enum properties with invalid value
//
// RouterOS line continuation is honored: physical lines ending with a trailing
// unescaped backslash are joined into a single logical line before parsing, so
// commands split across lines (e.g. long quoted URLs after `/tool/fetch add`)
// do not produce false positives such as "Unknown menu". Diagnostic ranges are
// mapped from logical-line offsets back to original physical-line coordinates.
//
// Capped for large docs to prevent OOM / CPU blow-up.

use crate::menus::MenuData;
use std::collections::{HashMap, HashSet};

const DIAGNOSTIC_SOURCE: &str = "rsc-ls";
const MAX_DIAG_LINES: usize = 3000;
const MAX_DIAG_BYTES: usize = 500_000; // cap per-doc bytes considered for diagnostics

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
    #[allow(dead_code)]
    pub const ERROR: u8 = 1;
    pub const WARNING: u8 = 2;
    pub const INFORMATION: u8 = 3;
    pub const HINT: u8 = 4;
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
                diagnostics.push(Diagnostic {
                    range: ll.map_range(start_char, end_char),
                    severity: Some(severity::WARNING),
                    code: Some("unknown-menu".to_string()),
                    source: Some(DIAGNOSTIC_SOURCE.to_string()),
                    message: format!("Unknown menu '{}'", ctx.path),
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

        for token in &tokens {
            if let Some(eq_idx) = token.text.find('=') {
                let key = &token.text[..eq_idx];
                let value = &token.text[eq_idx + 1..];
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
            for (key, spans) in &key_spans {
                if !allowed.contains(key)
                    && let Some(&(s, e)) = spans.first()
                {
                    diagnostics.push(Diagnostic {
                        range: ll.map_range(s, e),
                        severity: Some(severity::WARNING),
                        code: Some("unknown-property".to_string()),
                        source: Some(DIAGNOSTIC_SOURCE.to_string()),
                        message: format!("Unknown property '{}' for '{}'", key, ctx.path),
                    });
                }
            }

            // ---- Rule 3: Missing required property for Directory menus with add/set ----
            if (menu.menu_type == "Directory" || menu.menu_type == "Settings Directory")
                && ctx
                    .command
                    .as_deref()
                    .is_some_and(|c| c == "add" || c == "set")
            {
                for arg in &menu.arguments {
                    if arg.required && !key_counts.contains_key(&arg.name) {
                        // Range: point at the command verb token, falling back
                        // to the start of the line if no whole-token match.
                        let (s, e) = command_span.unwrap_or((0, line.len().min(8)));
                        diagnostics.push(Diagnostic {
                            range: ll.map_range(s, e),
                            severity: Some(severity::INFORMATION),
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

            // ---- Rule 5: Type hint for enum properties with invalid value ----
            for (key, (value, span)) in &key_values {
                // Find argument definition
                if let Some(arg) = menu.arguments.iter().find(|a| a.name == *key)
                    && arg.arg_type.starts_with("enum")
                {
                    let allowed_vals = parse_enum_values(&arg.arg_type);
                    if !allowed_vals.is_empty() {
                        // Strip quotes from value
                        let val = value.trim().trim_matches('"').trim_matches('\'');
                        // Empty value is not an error here (user may be completing)
                        if val.is_empty() {
                            continue;
                        }
                        // Handle comma-separated values? RouterOS may allow comma lists, but we check single
                        // For simplicity, check if val is in allowed list
                        // Also allow values with trailing comma?
                        let is_valid = allowed_vals.iter().any(|v| {
                            v == val || v.trim() == val
                            // Handle values that may be prefix? No, strict equality
                        });
                        if !is_valid {
                            // Narrow the recorded token span to the value part
                            // only (skip "key="), keeping any quotes in range.
                            let s = span.0 + key.len() + 1;
                            let e = span.1.max(s);
                            diagnostics.push(Diagnostic {
                                range: ll.map_range(s, e),
                                severity: Some(severity::HINT),
                                code: Some("invalid-enum-value".to_string()),
                                source: Some(DIAGNOSTIC_SOURCE.to_string()),
                                message: format!(
                                    "Invalid value '{}' for '{}' (expected one of: {})",
                                    val,
                                    key,
                                    allowed_vals.join(" | ")
                                ),
                            });
                        }
                    }
                }
            }
        }
    }

    // If we capped lines, add a hint diagnostic about truncation? Not necessary.
    diagnostics
}

// ── RouterOS backslash line continuation ──────────────────────────
//
// RouterOS joins a physical line ending in an unescaped trailing `\` with the
// next physical line (the newline is removed, no separator is inserted).
// Diagnostics must therefore parse *logical* lines while still reporting
// positions in original physical coordinates.

/// Byte-scan state shared by continuation detection.
struct ScanState {
    in_double: bool,
    in_single: bool,
}

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
fn continuation_body_end(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut state = ScanState {
        in_double: false,
        in_single: false,
    };
    let mut i = 0usize;
    // Content end defaults to EOL; an unquoted '#' cuts it earlier.
    let mut content_end = line.len();

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if state.in_double || state.in_single => {
                // Escaped byte inside quotes; skip it entirely.
                i += 2;
                continue;
            }
            b'"' if !state.in_single => state.in_double = !state.in_double,
            b'\'' if !state.in_double => state.in_single = !state.in_single,
            b'#' if !state.in_double && !state.in_single => {
                content_end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }

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
fn has_line_continuation(line: &str) -> bool {
    continuation_body_end(line).is_some()
}

/// A slice of one physical line contributed to a [`LogicalLine`].
#[derive(Debug)]
struct Segment {
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
#[derive(Debug, Default)]
struct LogicalLine {
    text: String,
    segments: Vec<Segment>,
}

impl LogicalLine {
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
    fn map_pos(&self, offset: usize) -> Position {
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
    fn map_range(&self, start: usize, end: usize) -> Range {
        Range {
            start: self.map_pos(start),
            end: self.map_pos(end.max(start)),
        }
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
fn build_logical_lines(raw_lines: &[&str]) -> Vec<LogicalLine> {
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

fn find_substring_range(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    haystack
        .find(needle)
        .map(|start| (start, start + needle.len()))
}

fn parse_enum_values(type_str: &str) -> Vec<String> {
    let inner = type_str
        .strip_prefix("enum")
        .and_then(|s| s.trim().strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    match inner {
        Some(body) => body.split('|').map(|s| s.trim().to_string()).collect(),
        None => Vec::new(),
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
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
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
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus]]
path = "/system/clock"
type = "Directory"
[[menus.arguments]]
name = "time-zone-name"
type = "string"
[[menus.arguments]]
name = "enabled"
type = "bool"
"#,
        )
    }

    #[test]
    fn test_unknown_menu_warning() {
        let data = synthetic_data();
        let doc = "/foo/bar add something=1";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu")),
            "should have unknown-menu diagnostic, got {:?}",
            diags
        );
        assert!(diags.iter().any(|d| d.message.contains("/foo/bar")));
        assert!(diags.iter().any(|d| d.severity == Some(severity::WARNING)));
    }

    #[test]
    fn test_known_menu_no_unknown_diag() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1/24 interface=ether1";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        // Should not have unknown-menu
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu")),
            "should not have unknown-menu for known path"
        );
    }

    #[test]
    fn test_unknown_property_warning() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1/24 unknownprop=foo";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property"))
        );
        assert!(diags.iter().any(|d| d.message.contains("unknownprop")));
    }

    #[test]
    fn test_known_property_no_unknown() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1/24 interface=ether1 comment=\"hi\"";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property")),
            "should not have unknown-property for valid props, got {:?}",
            diags
        );
    }

    #[test]
    fn test_missing_required_info() {
        let data = synthetic_data();
        // /ip/address add requires address and interface
        let doc = "/ip/address add comment=hi";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        let missing: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("missing-required"))
            .collect();
        assert!(
            !missing.is_empty(),
            "should have missing-required, got {:?}",
            diags
        );
        assert!(missing.iter().any(|d| d.message.contains("address")));
        assert!(missing.iter().any(|d| d.message.contains("interface")));
        assert!(
            missing
                .iter()
                .all(|d| d.severity == Some(severity::INFORMATION))
        );
    }

    #[test]
    fn test_no_missing_when_required_present() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1/24 interface=ether1";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required")),
            "should not have missing-required when all present"
        );
    }

    #[test]
    fn test_missing_not_emitted_for_print_verb() {
        let data = synthetic_data();
        // print does not require address/interface
        let doc = "/ip/address print";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required")),
            "print should not trigger missing-required"
        );
    }

    #[test]
    fn test_duplicate_property_warning() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property")),
            "should have duplicate-property, got {:?}",
            diags
        );
        assert!(diags.iter().any(|d| d.message.contains("address")));
    }

    #[test]
    fn test_no_duplicate_when_unique() {
        let data = synthetic_data();
        let doc = "/ip/address add address=1.1.1.1 interface=ether1";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property")),
            "should not have duplicate when unique"
        );
    }

    #[test]
    fn test_invalid_enum_hint() {
        let data = synthetic_data();
        let doc = "/ip/firewall/filter add chain=invalid action=accept";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        let hints: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("invalid-enum-value"))
            .collect();
        assert!(
            !hints.is_empty(),
            "should have invalid-enum-value, got {:?}",
            diags
        );
        assert!(hints.iter().any(|d| d.message.contains("invalid")));
        assert!(hints.iter().all(|d| d.severity == Some(severity::HINT)));
    }

    #[test]
    fn test_valid_enum_no_hint() {
        let data = synthetic_data();
        let doc = "/ip/firewall/filter add chain=input action=accept";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value")),
            "should not have hint for valid enum values"
        );
    }

    #[test]
    fn test_multiple_rules_together() {
        let data = synthetic_data();
        let doc = "/foo/bar add unknown=foo chain=bad\n/ip/address add address=1.1.1.1 interface=ether1 address=1.1.1.1\n/ip/firewall/filter add chain=bad";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
        );
    }

    #[test]
    fn test_empty_and_comment_lines_no_diags() {
        let data = synthetic_data();
        let doc = "# comment\n\n   \n:global x 1\n/ip/address add address=1.1.1.1 interface=ether1";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        // Only last line should be checked, and it's valid
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu")),
            "comments and empty should not produce diagnostics"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property")),
            "valid line should not have unknown-property"
        );
    }

    #[test]
    fn test_large_doc_capped() {
        let data = synthetic_data();
        // Generate large doc beyond cap
        let mut doc = String::new();
        for i in 0..4000 {
            doc.push_str(&format!("/foo/unknown{} add badprop=1\n", i));
        }
        let diags = compute_diagnostics(&data, &doc, "file:///test.rsc");
        // Should be capped at MAX_DIAG_LINES (3000) -> at most 3000 diagnostics (one per line)
        assert!(
            diags.len() <= MAX_DIAG_LINES,
            "diagnostics should be capped, got {}",
            diags.len()
        );
        // Should still have some diagnostics
        assert!(!diags.is_empty());
    }

    #[test]
    fn test_incremental_edit_simulation() {
        let data = synthetic_data();
        // Simulate incremental edits: initial doc has error, then fix
        let doc1 = "/ip/address add comment=hi"; // missing required
        let diags1 = compute_diagnostics(&data, doc1, "file:///test.rsc");
        assert!(
            diags1
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );

        let doc2 = "/ip/address add address=1.1.1.1/24 interface=ether1"; // fixed
        let diags2 = compute_diagnostics(&data, doc2, "file:///test.rsc");
        assert!(
            !diags2
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
    }

    #[test]
    fn test_implicit_parent_not_unknown() {
        let data = synthetic_data();
        // /ip/firewall is implicit parent (no direct entry but has children), should not be unknown
        // synthetic data has /ip/firewall/filter, so /ip/firewall should be considered known via child_names
        let doc = "/ip/firewall print";
        let diags = compute_diagnostics(&data, doc, "file:///test.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu")),
            "implicit parent /ip/firewall should not be unknown, got {:?}",
            diags
        );
    }
}

#[cfg(test)]
mod extra_coverage {
    use super::*;
    use crate::menus::MenuData;

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
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/list"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus]]
path = "/tool/ping"
type = "Command"
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

    // ── Explicit 5 rules with severity ─────────────────────────────────

    #[test]
    fn test_rule1_unknown_menu_warning_severity() {
        let data = synth();
        let diags = compute_diagnostics(&data, "/foo/bar add x=1", "file:///a.rsc");
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-menu"))
            .expect("unknown-menu");
        assert_eq!(d.severity, Some(severity::WARNING));
        assert_eq!(d.source.as_deref(), Some("rsc-ls"));
        assert!(d.message.contains("/foo/bar"));
        assert_eq!(d.range.start.line, 0);
    }

    #[test]
    fn test_rule2_unknown_property_warning_severity() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/address add address=1.1.1.1 interface=ether1 bogus=1",
            "file:///a.rsc",
        );
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-property"))
            .expect("unknown-property");
        assert_eq!(d.severity, Some(severity::WARNING));
        assert!(d.message.contains("bogus"));
        assert!(d.message.contains("/ip/address"));
    }

    #[test]
    fn test_rule3_missing_required_info_for_add_on_directory() {
        let data = synth();
        let diags = compute_diagnostics(&data, "/ip/address add comment=hi", "file:///a.rsc");
        let missing: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("missing-required"))
            .collect();
        assert_eq!(
            missing.len(),
            2,
            "should have 2 missing (address, interface)"
        );
        for m in &missing {
            assert_eq!(m.severity, Some(severity::INFORMATION));
            assert!(m.message.contains("Missing required"));
        }
        assert!(missing.iter().any(|d| d.message.contains("address")));
        assert!(missing.iter().any(|d| d.message.contains("interface")));
    }

    #[test]
    fn test_rule3_missing_required_for_set_on_directory() {
        let data = synth();
        let diags = compute_diagnostics(&data, "/ip/address set comment=hi", "file:///a.rsc");
        // set on Directory should also require
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(severity::INFORMATION))
        );
    }

    #[test]
    fn test_rule3_not_for_command_type() {
        let data = synth();
        // /tool/ping is Command, not Directory, so missing-required should not trigger
        let diags = compute_diagnostics(&data, "/tool/ping address=1.1.1.1", "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
    }

    #[test]
    fn test_rule3_not_for_print_verb() {
        let data = synth();
        let diags = compute_diagnostics(&data, "/ip/address print", "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
    }

    #[test]
    fn test_rule4_duplicate_property_warning_severity() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2",
            "file:///a.rsc",
        );
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("duplicate-property"))
            .expect("duplicate");
        assert_eq!(d.severity, Some(severity::WARNING));
        assert!(d.message.contains("address"));
        // Second occurrence range should be after first
        assert!(d.range.start.character > 0);
    }

    #[test]
    fn test_rule4_duplicate_with_three_occurrences_still_warns() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/address add address=1 interface=ether1 address=2 address=3",
            "file:///a.rsc",
        );
        // Should have at least one duplicate diagnostic
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property"))
        );
    }

    #[test]
    fn test_rule5_invalid_enum_hint_severity() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/firewall/filter add chain=invalid",
            "file:///a.rsc",
        );
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
            .expect("hint");
        assert_eq!(d.severity, Some(severity::HINT));
        assert!(d.message.contains("Invalid value"));
        assert!(d.message.contains("input | forward | output"));
        assert!(d.message.contains("invalid"));
    }

    #[test]
    fn test_rule5_valid_enum_no_hint() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/firewall/filter add chain=input",
            "file:///a.rsc",
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
        );
    }

    #[test]
    fn test_rule5_empty_value_not_hint() {
        let data = synth();
        let diags = compute_diagnostics(&data, "/ip/firewall/filter add chain=", "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
        );
    }

    #[test]
    fn test_rule5_quoted_value_stripped_then_checked() {
        let data = synth();
        let diags = compute_diagnostics(
            &data,
            "/ip/firewall/filter add chain=\"invalid\"",
            "file:///a.rsc",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
        );
        let diags2 = compute_diagnostics(
            &data,
            "/ip/firewall/filter add chain=\"input\"",
            "file:///a.rsc",
        );
        assert!(
            !diags2
                .iter()
                .any(|d| d.code.as_deref() == Some("invalid-enum-value"))
        );
    }

    // ── Empty and comment-only docs ────────────────────────────────────

    #[test]
    fn test_empty_doc_no_diags() {
        let data = synth();
        let diags = compute_diagnostics(&data, "", "file:///a.rsc");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_whitespace_only_no_diags() {
        let data = synth();
        let diags = compute_diagnostics(&data, "   \n\n\t\n  ", "file:///a.rsc");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_comment_only_no_diags() {
        let data = synth();
        let doc = "# comment\n# another\n   # indented\n";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_global_and_brace_lines_no_diags() {
        let data = synth();
        let doc = ":global x 1\n:local y 2\n{\n}\n..\n";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(diags.is_empty());
    }

    #[test]
    fn test_mixed_valid_and_comments() {
        let data = synth();
        let doc = "# comment\n\n/ip/address add address=1.1.1.1 interface=ether1\n# trailing\n";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property"))
        );
    }

    // ── Large doc caps ─────────────────────────────────────────────────

    #[test]
    fn test_large_doc_capped_at_max_diag_lines() {
        let data = synth();
        let doc = "/unknown/menu add x=1\n".repeat(4000);
        let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
        assert!(diags.len() <= MAX_DIAG_LINES);
        assert!(diags.len() <= 3000);
        assert!(!diags.is_empty());
        // All should be unknown-menu
        assert!(
            diags
                .iter()
                .all(|d| d.code.as_deref() == Some("unknown-menu"))
        );
    }

    #[test]
    fn test_large_doc_capped_at_max_diag_bytes() {
        let data = synth();
        // Each line ~30 bytes, need >500KB => ~17000 lines, but MAX_DIAG_LINES is 3000 so lines cap hits first
        // To test bytes cap, use long lines
        let long_line = format!("/unknown/menu add x={}\n", "a".repeat(500));
        let doc = long_line.repeat(2000); // ~1M bytes
        assert!(doc.len() > MAX_DIAG_BYTES);
        let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
        // Should be capped (either lines or bytes)
        assert!(diags.len() <= MAX_DIAG_LINES);
        assert!(!diags.is_empty());
        // Ensure first diags preserved
        assert_eq!(diags[0].range.start.line, 0);
    }

    #[test]
    fn test_large_doc_truncation_preserves_first_n() {
        let data = synth();
        // First 5 lines are errors, then 5000 more errors beyond cap
        let mut doc = String::new();
        for i in 0..5 {
            doc.push_str(&format!("/unknown{}/menu add x=1\n", i));
        }
        doc.push_str(&"/unknown/menu add x=1\n".repeat(5000));
        let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
        assert!(diags.len() <= 3000);
        // First 5 should be present
        for i in 0..5 {
            let needle = format!("/unknown{}/menu", i);
            assert!(
                diags.iter().any(|d| d.message.contains(&needle)),
                "missing {needle}"
            );
        }
    }

    #[test]
    fn test_large_doc_bytes_truncation_preserves_first() {
        let data = synth();
        let first = "/unknown/first add x=1\n";
        let tail = "/unknown/tail add x=1\n".repeat(50_000); // huge
        let doc = format!("{}{}", first, tail);
        assert!(doc.len() > MAX_DIAG_BYTES);
        let diags = compute_diagnostics(&data, &doc, "file:///a.rsc");
        assert!(!diags.is_empty());
        assert!(diags.iter().any(|d| d.message.contains("/unknown/first")));
    }

    // ── Incremental edits simulation ───────────────────────────────────

    #[test]
    fn test_incremental_fix_removes_diag() {
        let data = synth();
        let before = "/ip/address add comment=hi"; // missing required
        let after = "/ip/address add address=1.1.1.1 interface=ether1";
        assert!(
            compute_diagnostics(&data, before, "file:///a.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
        assert!(
            !compute_diagnostics(&data, after, "file:///a.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
    }

    #[test]
    fn test_incremental_introduces_duplicate() {
        let data = synth();
        let before = "/ip/address add address=1.1.1.1 interface=ether1";
        let after = "/ip/address add address=1.1.1.1 interface=ether1 address=2.2.2.2";
        assert!(
            !compute_diagnostics(&data, before, "file:///a.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property"))
        );
        assert!(
            compute_diagnostics(&data, after, "file:///a.rsc")
                .iter()
                .any(|d| d.code.as_deref() == Some("duplicate-property"))
        );
    }

    #[test]
    fn test_unknown_menu_does_not_cascade_property_errors() {
        let data = synth();
        // Unknown menu should not also emit unknown-property for same line
        let doc = "/unknown/menu add bogus=1";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(
            diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property")),
            "should not cascade"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
    }

    #[test]
    fn test_diagnostics_source_always_rsc_ls() {
        let data = synth();
        let doc = "/foo/bar add x=1\n/ip/address add unknown=1\n/ip/address add address=1 interface=ether1 address=2\n/ip/firewall/filter add chain=bad";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        for d in &diags {
            assert_eq!(d.source.as_deref(), Some("rsc-ls"));
        }
    }

    #[test]
    fn test_diagnostics_range_within_line() {
        let data = synth();
        let line = "/foo/bar add x=1";
        let diags = compute_diagnostics(&data, line, "file:///a.rsc");
        let d = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-menu"))
            .unwrap();
        assert_eq!(d.range.start.line, 0);
        // Path "/foo/bar" starts at 0, ends at 8
        assert_eq!(d.range.start.character, 0);
        assert_eq!(d.range.end.character, 8);
    }

    // ── RouterOS backslash line continuation ──────────────────────────

    #[test]
    fn test_continuation_quoted_url_no_unknown_menu() {
        let data = synth();
        // Real-world reproduction: /tool/fetch URL split across lines with a
        // trailing backslash inside a quoted string. The second physical line
        // starts with '/' and must NOT be diagnosed as an unknown menu.
        let doc = concat!(
            "/tool/fetch add ssl-verify=no url=\"https://raw.githubusercontent.com",
            "/hagezi/dns-blocklists\\\n/main/hosts/pro.txt\"",
        );
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(
            diags.is_empty(),
            "joined continuation must not produce diagnostics, got {diags:?}"
        );
    }

    #[test]
    fn test_continuation_property_split_recognized() {
        let data = synth();
        // Property split across lines. Note the space BEFORE the backslash:
        // RouterOS removes the newline without inserting whitespace, so a
        // separating space must be present for the tokens to stay distinct
        // (exactly as on a real router).
        let doc = "/ip/address add address=10.0.0.1/24 \\\ninterface=ether1";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required")),
            "interface must be recognized via continuation, got {diags:?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-property")),
            "no unknown property expected, got {diags:?}"
        );
    }

    #[test]
    fn test_continuation_range_maps_to_physical_lines() {
        let data = synth();
        let doc = "/ip/address add bogusprop=x\\\n other=y";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        let ups: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("unknown-property"))
            .collect();
        assert_eq!(ups.len(), 2, "expected two unknown-property, got {ups:?}");

        // 'bogusprop' lives in the first segment: joined offset == physical
        // offset on line 0 ("/ip/address add " is 16 bytes).
        let bogus = ups
            .iter()
            .find(|d| d.message.contains("'bogusprop'"))
            .expect("bogusprop diag");
        assert_eq!(bogus.range.start.line, 0);
        assert_eq!(bogus.range.start.character, 16);
        assert_eq!(bogus.range.end.character, 25);

        // ' other=y' is appended verbatim from physical line 1, so 'other'
        // starts at character 1 of line 1.
        let other = ups
            .iter()
            .find(|d| d.message.contains("Unknown property 'other'"))
            .expect("other diag");
        assert_eq!(other.range.start.line, 1);
        assert_eq!(other.range.start.character, 1);
        assert_eq!(other.range.end.line, 1);
        assert_eq!(other.range.end.character, 6);
    }

    #[test]
    fn test_escaped_backslash_not_continuation() {
        let data = synth();
        // Line 1 ends with an escaped backslash pair ("...with \\"): even run,
        // so it does NOT swallow the next command line.
        let doc = "/ip/address add comment=\"ends with \\\\\n/foo/bar add x=1";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        let menu = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-menu"))
            .expect("/foo/bar must still be flagged as unknown menu");
        assert!(menu.message.contains("/foo/bar"));
        assert_eq!(menu.range.start.line, 1);
    }

    #[test]
    fn test_comment_not_continued() {
        let data = synth();
        // A '#' comment never continues, even with a trailing backslash.
        let doc = "# note \\\n/foo/bar add x=1";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        let menus: Vec<_> = diags
            .iter()
            .filter(|d| d.code.as_deref() == Some("unknown-menu"))
            .collect();
        assert_eq!(menus.len(), 1, "only /foo/bar should be flagged");
        assert!(menus[0].message.contains("/foo/bar"));
        assert_eq!(menus[0].range.start.line, 1);
    }

    #[test]
    fn test_dangling_continuation_at_eof_no_panic() {
        let data = synth();
        // EOF right after the backslash: must not panic; the logical line is
        // flushed and missing-required is still reported sensibly.
        let doc = "/ip/address add address=1.1.1.1\\";
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        let missing = diags
            .iter()
            .find(|d| {
                d.code.as_deref() == Some("missing-required") && d.message.contains("interface")
            })
            .expect("interface should still be reported as missing");
        assert_eq!(missing.range.start.line, 0);
    }

    #[test]
    fn test_crlf_continuation() {
        let data = synth();
        // Same reproduction as the quoted-url case but with CRLF endings.
        let doc = concat!(
            "/tool/fetch add ssl-verify=no url=\"https://raw.githubusercontent.com",
            "/hagezi/dns-blocklists\\\r\n/main/hosts/pro.txt\"",
        );
        let diags = compute_diagnostics(&data, doc, "file:///a.rsc");
        assert!(
            !diags
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu")),
            "CRLF continuation must not produce unknown-menu, got {diags:?}"
        );
    }

    #[test]
    fn test_has_line_continuation_cases() {
        // Single trailing backslash: normal continuation.
        assert!(has_line_continuation("/ip/address add address=1.1.1.1\\"));
        // Trailing backslash followed by whitespace: still continues.
        assert!(has_line_continuation("add x=1\\   "));
        // Escaped pair = literal backslash: NOT a continuation.
        assert!(!has_line_continuation("add comment=x\\\\"));
        // Triple run = one escaped + one continuation.
        assert!(has_line_continuation("add comment=x\\\\\\"));
        // Inside double quotes (unterminated string): continues.
        assert!(has_line_continuation("url=\"https://example.com/foo\\"));
        // Escaped quote inside double quotes, then trailing backslash.
        assert!(has_line_continuation("url=\"a\\\" b\\"));
        // Single quotes behave like double quotes.
        assert!(has_line_continuation("set x='abc\\"));
        // Unquoted '#' cuts effective content: comments never continue.
        assert!(!has_line_continuation("# note \\"));
        assert!(!has_line_continuation("add x=1 # trailing \\"));
        // Plain lines are not continuations.
        assert!(!has_line_continuation(""));
        assert!(!has_line_continuation("print"));
    }

    #[test]
    fn test_logical_line_map_spans_join() {
        // A range whose start/end land on different physical lines maps to a
        // multi-line LSP range (allowed by the spec).
        let ll = build_logical_lines(&["/tool/fetch add url=\"abc\\", "def\""]);
        assert_eq!(ll.len(), 1);
        // Joined text: /tool/fetch add url="abcdef" (len 28).
        let joined = ll[0].text.as_str();
        assert_eq!(joined, "/tool/fetch add url=\"abcdef\"");
        assert_eq!(ll[0].segments.len(), 2);
        // Segment 0 covers bytes 0..24 ("...\"abc"), segment 1 bytes 24..28
        // ("def\""). A range from 'c' (byte 23, physical line 0) to 'd'
        // (byte 24, physical line 1) spans the join point.
        let r = ll[0].map_range(23, 24);
        assert_eq!(r.start.line, 0);
        assert_eq!(r.start.character, 23);
        assert_eq!(r.end.line, 1);
        assert_eq!(r.end.character, 0);
        // Out-of-bounds offsets clamp defensively to the end of the text:
        // byte 28 lands at line 1, character 4.
        let clamped = ll[0].map_pos(joined.len() + 100);
        assert_eq!(clamped.line, 1);
        assert_eq!(clamped.character, 4);
    }

    // ── Token-position ranges ──────────────────────────────────────

    fn demo_menu_data() -> MenuData {
        MenuData::from_toml_str(
            r#"
[[menus]]
path = "/demo/alpha"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"

[[menus]]
path = "/demo/enum"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (on | off)"
"#,
        )
    }

    #[test]
    fn test_duplicate_property_highlights_second_occurrence_precisely() {
        // PHASE 1: ranges come from tokenization, so the flagged occurrence
        // is the SECOND property occurrence (bytes 33..40), never the "address"
        // substring inside the menu path (bytes 4..11).
        let md = MenuData::from_toml_str(
            r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
required = true
"#,
        );
        let line = "/ip/address add address=1.1.1.1 address=2.2.2.2";
        let diags = compute_diagnostics(&md, line, "file:///a.rsc");
        let dup = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("duplicate-property"))
            .expect("duplicate-property expected");
        assert_eq!(dup.range.start.line, 0);
        assert_eq!(dup.range.start.character, 32, "must flag second occurrence");
        assert_eq!(dup.range.end.character, 39, "range covers the KEY only");
    }

    #[test]
    fn test_unknown_property_key_inside_menu_path_is_not_misranged() {
        // Key text also appears inside the menu path ("alpha" in "/demo/alpha");
        // the diagnostic must point at the PROPERTY occurrence after "add".
        let md = demo_menu_data();
        let line = "/demo/alpha add alpha=1 name=x";
        let diags = compute_diagnostics(&md, line, "file:///a.rsc");
        let up = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-property"))
            .expect("unknown-property expected for 'alpha'");
        assert_eq!(up.range.start.character, 16, "'alpha' after 'add'");
        assert_eq!(up.range.end.character, 21);
    }

    #[test]
    fn test_quoted_value_with_keylike_substring_no_phantom_diagnostics() {
        // Quote-aware tokenization keeps this as ONE value token, so "alpha="
        // inside the quoted string can no longer fabricate properties.
        let md = demo_menu_data();
        let line = r#"/demo/alpha add name="x alpha=9 y""#;
        let diags = compute_diagnostics(&md, line, "file:///a.rsc");
        assert!(
            diags.is_empty(),
            "quoted key-like substrings must not warn, got {diags:?}"
        );
    }

    #[test]
    fn test_enum_value_range_points_at_value_part_only() {
        let md = demo_menu_data();
        let line = "/demo/enum set mode=bogus";
        let diags = compute_diagnostics(&md, line, "file:///a.rsc");
        let hint = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("invalid-enum-value"))
            .expect("invalid-enum-value expected");
        // "/demo/enum set mode=bogus": token "mode=bogus" starts at 15;
        // value part starts after "key=" (15+5=20), ends at token end (25).
        assert_eq!(hint.range.start.character, 20);
        assert_eq!(hint.range.end.character, 25);
    }

    // ── Known-prefix O(1) parity ──────────────────────────────────

    #[test]
    fn test_known_prefix_parity_root_deep_implicit_unknown() {
        let md = demo_menu_data();
        // Root prefix of a known menu ("/demo") → known, no warning.
        assert!(
            !compute_diagnostics(&md, "/demo print", "f")
                .iter()
                .any(|d| { d.code.as_deref() == Some("unknown-menu") })
        );
        // Implicit parent with no direct entry but known children → known.
        assert!(
            !compute_diagnostics(&md, "/demo/enum print", "f")
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
        // Exact deep menu → known.
        assert!(
            !compute_diagnostics(&md, "/demo/alpha print", "f")
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
        // Genuinely unknown → still warned, same message shape as before.
        let diags = compute_diagnostics(&md, "/foo/bar add x=1", "f");
        let unk = diags
            .iter()
            .find(|d| d.code.as_deref() == Some("unknown-menu"))
            .expect("unknown menu must still be flagged");
        assert!(unk.message.contains("/foo/bar"));
    }
}
