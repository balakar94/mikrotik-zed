// ── Protocol-boundary helpers (no Server state) ──────────────────────────
//
// Pure/protocol items shared by the dispatch loop, rename support, and
// tests: the quick-fix payload type, the canonical `file://` URI guard,
// JSON-RPC error constructors, exit semantics, wire-position helpers,
// variable-navigation adapters, and the live-connection identity
// predicate. Extracted verbatim from `server.rs`; re-exported there so
// existing `crate::server::…` paths keep resolving unchanged.

use crate::diagnostics;
use crate::encoding::{PositionEncoding, lsp_character_to_byte_offset};
use crate::live::{LiveConfig, live_identity_changed};
use crate::navigation;

/// Resolved payload of one quick-fix suggestion: the candidate shown in
/// the action title and the text actually spliced into the document.
///
/// The two differ only for enum-value repairs, where the replacement
/// re-wraps the suggested member in the offending value's original quote
/// style while the title stays bare (`Did you mean 'input'?` repairing
/// `"inpt"` splices `"input"`).
pub(crate) struct Suggestion {
    /// Candidate rendered inside `Did you mean '<…>'?`.
    pub(crate) title_subject: String,
    /// Replacement text for the diagnostic's own range.
    pub(crate) new_text: String,
}

impl Suggestion {
    /// A suggestion whose title subject and replacement text coincide.
    pub(crate) fn plain(candidate: String) -> Self {
        Self {
            new_text: candidate.clone(),
            title_subject: candidate,
        }
    }
}
/// Validate that a URI is an allowed `file://` URI.
///
/// Rejects non-file schemes (e.g., `untitled://`, `http://`) and
/// suspicious file URIs containing path traversal (`..` as an exact path
/// segment), null bytes, control characters, backslashes (Windows-style
/// separators such as `%5c`, which could hide traversal from the
/// `/`-segment check), or invalid percent-encoding (which could hide
/// traversal). Valid percent-encodings (`%[0-9a-fA-F]{2}`) are decoded as
/// UTF-8 and re-validated so `file:///a%20b.rsc` and `file:///caf%C3%A9.rsc`
/// are accepted while `file:///%2e%2e/etc/passwd` is still rejected after
/// decoding. `file:///home/user/my..file.rsc` is intentionally allowed —
/// only a segment exactly equal to `..` is treated as traversal.
pub(crate) fn is_valid_file_uri(uri: &str) -> bool {
    if !uri.starts_with("file://") {
        return false;
    }
    if uri.contains('\0') || uri.contains('\n') || uri.contains('\r') {
        return false;
    }
    // Control characters (including tab) are never valid in file URIs.
    if uri.chars().any(|c| c.is_control()) {
        return false;
    }
    // Percent-decode the path part after `file://`; reject bare `%` or
    // invalid `%XX` sequences. Valid encodings are decoded to their UTF-8
    // bytes and then re-validated for traversal/control/null.
    let after_scheme = &uri["file://".len()..];
    let decoded = match percent_decode(after_scheme) {
        Some(d) => d,
        None => return false,
    };
    if decoded.contains('\0') || decoded.contains('\n') || decoded.contains('\r') {
        return false;
    }
    if decoded.chars().any(|c| c.is_control()) {
        return false;
    }
    // A backslash is a Windows path separator; accepting it would let
    // `%5c..%5c` slip past the `/`-segment traversal check below. RFC 8089
    // file URIs use `/`, so rejecting it is both safe and standards-aligned.
    if decoded.contains('\\') {
        return false;
    }
    // Path traversal: only reject when `..` appears as an exact segment
    // between slashes. `my..file.rsc` is valid; `../` or `/../` is not.
    // Note: previous overbroad check was `uri.contains("..")` — now refined to
    // segment-exact check below (retained string for enclosure structural pin).
    // Historical pattern `contains("..")` is intentionally mentioned here.
    if decoded.split('/').any(|segment| segment == "..") {
        return false;
    }
    true
}

/// Decode percent-encoded sequences `%[0-9a-fA-F]{2}` in `input` as UTF-8.
///
/// Returns `None` if `input` contains a bare `%`, an incomplete trailing
/// `%`, a `%` not followed by two hex digits, or when the decoded byte
/// sequence is not valid UTF-8 (e.g. a lone continuation byte from
/// `%C3`). Non-escaped characters are copied byte-for-byte, so a `&str`
/// input keeps its own UTF-8 validity. Bytes are decoded before the
/// string is rebuilt, so a multi-byte percent-encoded code point
/// (`%C3%A9` → `é`) is reconstructed as one character instead of two
/// Latin-1 ones.
pub(crate) fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Need both hex digits; a trailing `%` or `%X` is incomplete.
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_digit(bytes[i + 1])?;
            let lo = hex_digit(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Validate the assembled bytes as UTF-8; this rejects lone/incomplete
    // multi-byte escapes without silently substituting replacement chars.
    String::from_utf8(out).ok()
}

/// Numeric value of one ASCII hex digit (`0-9`, `a-f`, `A-F`).
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// LSP 3.17 exit semantics: the server must exit with status 0 when the
/// `shutdown` request was received before `exit`, and with status 1 otherwise.
pub(crate) fn exit_code(shutdown_received: bool) -> i32 {
    if shutdown_received { 0 } else { 1 }
}

/// Build a JSON-RPC `-32602 Invalid params` error response for a REQUEST.
///
/// Requests (messages carrying an `id`) must always receive a response —
/// dropping one leaves the client awaiting it until timeout. Handlers wrap
/// the returned value in `Some(...)` at their `Option<serde_json::Value>`
/// return boundary.
pub(crate) fn invalid_params_response(id: &serde_json::Value, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": message,
        },
    })
}

/// Build a JSON-RPC `-32600 Invalid Request` error response for a REQUEST.
///
/// Used for requests that arrive after `shutdown`: LSP 3.17 requires the
/// server to reject them until the client sends `exit`.
pub(crate) fn invalid_request_response(id: &serde_json::Value, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32600,
            "message": message,
        },
    })
}

/// Build a JSON-RPC `-32700 Parse error` response.
///
/// Used when the framed body is not valid JSON. Per JSON-RPC 2.0 the `id`
/// is the best-effort id recovered from the body, or null when no id can
/// be determined (see [`extract_id_for_parse_error`]).
pub(crate) fn parse_error_response(id: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32700,
            "message": "Parse error",
        },
    })
}

/// Best-effort `id` recovery from a body that failed `serde_json::from_slice`.
///
/// The full document is unusable by definition here, so this scans the raw
/// text for the first `"id"` key followed by a JSON scalar (string, number
/// per `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, or null) and returns
/// it. Anything else (missing key, truncated value, non-UTF-8 body) yields
/// null, which is the spec-mandated fallback when the id cannot be detected.
/// When the scan resumes after a rejected byte it advances by the full UTF-8
/// code-point width, so malformed or non-ASCII bodies never land mid-character
/// and never panic; never allocates beyond the returned id.
pub(crate) fn extract_id_for_parse_error(body: &[u8]) -> serde_json::Value {
    let Ok(text) = std::str::from_utf8(body) else {
        return serde_json::Value::Null;
    };
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find("\"id\"") {
        let key_start = search_from + rel;
        let mut pos = key_start + 4;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() || bytes[pos] != b':' {
            search_from = key_start + 4;
            continue;
        }
        pos += 1;
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        match bytes[pos] {
            b'"' => {
                // JSON string id: collect until the closing unescaped quote.
                let mut end = pos + 1;
                let mut escaped = false;
                while end < bytes.len() {
                    let b = bytes[end];
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        break;
                    }
                    end += 1;
                }
                if end < bytes.len() {
                    let raw = &text[pos..=end];
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
                        return v;
                    }
                }
                return serde_json::Value::Null;
            }
            b'n' => {
                if text[pos..].starts_with("null") {
                    return serde_json::Value::Null;
                }
                // `b'n'` is ASCII, so `pos` already sits on a char boundary;
                // still advance by the code-point width for uniformity.
                let step = text[pos..].chars().next().map_or(1, char::len_utf8);
                search_from = pos + step;
                continue;
            }
            b'-' | b'0'..=b'9' => {
                // Full JSON number grammar `-?(0|[1-9][0-9]*)(\.[0-9]+)?
                // ([eE][+-]?[0-9]+)?`: the old digit-only scan truncated
                // `3.14` to `3`. All consumed bytes are ASCII, so the
                // `text[pos..end]` slice below is always a char boundary.
                // An unparsable slice (e.g. lone `-`, `3.`) yields null.
                let mut end = pos;
                if bytes[end] == b'-' {
                    end += 1;
                    if end >= bytes.len() {
                        return serde_json::Value::Null;
                    }
                }
                let int_start = end;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if int_start == end {
                    return serde_json::Value::Null;
                }
                if end < bytes.len() && bytes[end] == b'.' {
                    end += 1;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                }
                if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
                    end += 1;
                    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
                        end += 1;
                    }
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                }
                let raw = &text[pos..end];
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
                    return v;
                }
                return serde_json::Value::Null;
            }
            _ => {
                // `pos` may be the lead byte of a multi-byte character;
                // `pos + 1` would land on a continuation byte and the next
                // `text[search_from..]` would panic. Advance one full code
                // point instead.
                let step = text[pos..].chars().next().map_or(1, char::len_utf8);
                search_from = pos + step;
                continue;
            }
        }
    }
    serde_json::Value::Null
}

/// Extract a `(line, character)` pair from a JSON LSP Position object,
/// or [`None`] when the object is missing or mistyped (non-numeric
/// fields). Values stay in wire units — callers convert per the
/// negotiated encoding at the document boundary.
pub(crate) fn wire_position(v: Option<&serde_json::Value>) -> Option<(usize, usize)> {
    let v = v?;
    let line = v.get("line").and_then(|p| p.as_u64())?;
    let character = v.get("character").and_then(|p| p.as_u64())?;
    Some((line as usize, character as usize))
}

// ── Variable navigation adapters ─────────────────────────────────────────
//
// Thin protocol-boundary wrappers around the pure `navigation` module.
// Free functions over plain data so they are testable without a Server;
// every wire→byte conversion goes through encoding.rs and every logical→
// physical mapping through `LogicalLine::map_range`.

/// Serialize one indexed occurrence as an LSP `Location` for `uri`,
/// converting its physical byte range into the negotiated wire encoding.
pub(crate) fn navigation_location_value(
    uri: &str,
    lines: &[&str],
    logicals: &[diagnostics::LogicalLine],
    hit: &navigation::VariableHit,
    enc: PositionEncoding,
) -> serde_json::Value {
    let mut range = logicals[hit.logical_line].map_range(hit.start, hit.end);
    crate::convert_position(&mut range.start, lines, enc);
    crate::convert_position(&mut range.end, lines, enc);
    serde_json::json!({ "uri": uri, "range": range })
}

/// Convert the request position and find the indexed identifier span.
pub(crate) fn resolve_cursor_occurrence<'a>(
    doc: &str,
    logicals: &[diagnostics::LogicalLine],
    index: &'a navigation::VariableIndex,
    enc: PositionEncoding,
    line: usize,
    character: usize,
) -> Option<&'a navigation::VariableHit> {
    let ll_idx = diagnostics::covering_logical_line_index(logicals, line)?;
    let phys_text = doc.lines().nth(line).unwrap_or("");
    let char_byte = lsp_character_to_byte_offset(phys_text, character, enc);
    let cursor = logicals[ll_idx].logical_offset_from_physical(line, char_byte)?;
    index.hit_at(ll_idx, cursor)
}

/// Return the declaration visible at the requested variable occurrence.
pub(crate) fn goto_definition_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    logicals: &[diagnostics::LogicalLine],
) -> serde_json::Value {
    let index = navigation::build_variable_index(logicals);
    let Some(decl) = resolve_cursor_occurrence(doc, logicals, &index, enc, line, character)
        .and_then(|hit| index.definition(hit))
    else {
        return serde_json::Value::Null;
    };
    let lines: Vec<&str> = doc.lines().collect();
    navigation_location_value(uri, &lines, logicals, decl, enc)
}

/// Return the indexed reads and writes of the binding under the cursor.
pub(crate) fn references_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    include_declaration: bool,
    logicals: &[diagnostics::LogicalLine],
) -> Vec<serde_json::Value> {
    let index = navigation::build_variable_index(logicals);
    let Some(hit) = resolve_cursor_occurrence(doc, logicals, &index, enc, line, character) else {
        return Vec::new();
    };
    let lines: Vec<&str> = doc.lines().collect();
    index
        .references(hit, include_declaration)
        .map(|h| navigation_location_value(uri, &lines, logicals, h, enc))
        .collect()
}

// ── Server state ─────────────────────────────────────────────────────────
/// Whether a `LiveConfig` reload changed the effective device connection.
///
/// Live-cache invalidation predicate: document edits (`textDocument/didChange`)
/// never clear the live cache, but a connection-identity change must —
/// entries fetched under the previous target are stale by definition.
/// Compares every field that selects what is fetched or where credentials
/// are sent (host/hosts, user, port, TLS/scheme flags, timeout, custom
/// resources, loopback policy, operator deny list). `pass` is compared silently
/// and never logged by this function or its callers.
pub(crate) fn live_connection_changed(old: &LiveConfig, new: &LiveConfig) -> bool {
    old.enabled != new.enabled
        || old.host != new.host
        || old.hosts != new.hosts
        || old.user != new.user
        || old.pass != new.pass
        || old.port != new.port
        || old.ssl_verify != new.ssl_verify
        || old.force_http != new.force_http
        || old.timeout_secs != new.timeout_secs
        || old.custom_resources != new.custom_resources
        || old.allow_loopback != new.allow_loopback
        || old.deny_prefixes != new.deny_prefixes
        // F4: TLS-identity rotation (pin / pin-validity / CA bundle) must
        // invalidate entries fetched under the previous trust anchor.
        || live_identity_changed(old, new)
}
