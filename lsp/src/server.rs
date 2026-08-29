// ── LSP server core (protocol boundary) ───────────────────────────
// Legacy note: get_cached_or_fetch_blocking retained for tests (now non-blocking via get_cached_or_fetch_background)
//
// Owns the wire-facing half of rsc-ls: the `Server` state machine
// (stdio read/write loop, `handle_message` method dispatch, tracked-
// document store), the quick-fix payload type, the variable-navigation
// request adapters, and the canonical `is_valid_file_uri` guard.
//
// Split of responsibilities:
// - Feature logic (completion, hover, diagnostics, symbols, folding,
//   signature help, suggestions, navigation math) lives in dedicated
//   sibling modules behind pure functions; this file only marshals
//   JSON-RPC params/results around those calls.
// - Shared resource caps are declared once in `caps.rs` and reach this
//   module through `crate::` paths (re-exported at the crate root).
// - Everything here is `pub(crate)` and re-exported from the crate root
//   (`main.rs`) so the root test modules and these unit tests exercise
//   real wire-visible behavior without reaching into private items.

use crate::caps::{MAX_CODE_ACTIONS, MAX_DOC_SIZE, MAX_DOCS};
use crate::completion;
use crate::diagnostics;
use crate::encoding::{
    PositionEncoding, apply_incremental_edit, byte_offset_to_utf16_units,
    convert_diagnostic_ranges, floor_char_boundary, lsp_character_to_byte_offset,
    lsp_position_to_offset,
};
use crate::folding;
use crate::framing::{Frame, FrameError, read_message};
use crate::hover;
use crate::live::{LiveCache, LiveConfig, get_cached_or_fetch_background};
use crate::logging::{log_debug, log_error, log_info, log_warn};
use crate::menus::MenuData;
use crate::navigation;
use crate::parser::{build_before_cursor, parse_line, tokenize_with_spans};
use crate::signature;
use crate::suggest;
use crate::symbols;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::sync::{Arc, Mutex};

/// Resolved payload of one quick-fix suggestion: the candidate shown in
/// the action title and the text actually spliced into the document.
///
/// The two differ only for enum-value repairs, where the replacement
/// re-wraps the suggested member in the offending value's original quote
/// style while the title stays bare (`Did you mean 'input'?` repairing
/// `"inpt"` splices `"input"`).
pub(crate) struct Suggestion {
    /// Candidate rendered inside `Did you mean '<…>'?`.
    title_subject: String,
    /// Replacement text for the diagnostic's own range.
    new_text: String,
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
/// suspicious file URIs containing path traversal (`..`) or null bytes.
pub(crate) fn is_valid_file_uri(uri: &str) -> bool {
    if !uri.starts_with("file://") {
        return false;
    }
    if uri.contains('\0') {
        return false;
    }
    if uri.contains("..") {
        return false;
    }
    true
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

// ── Variable navigation adapters ────────────────────────────────────
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

/// Everything both navigation requests need after resolving a position.
///
/// `logical_line`/`cursor` locate the request inside the caller's joined
/// logical lines; `name` is the variable identifier under the cursor
/// (usage or declaration — never a mere same-spelling property).
pub(crate) struct CursorOccurrence {
    logical_line: usize,
    cursor: usize,
    name: String,
}

/// Shared resolution step of both navigation requests.
///
/// `logicals` is the ONE continuation-aware join per request (owned by the
/// caller) and `index` was built from that same join, so cursor mapping and
/// occurrence lookup can never disagree about document coordinates.
/// Wire character → byte offset via [`lsp_character_to_byte_offset`];
/// cursor mapped into logical coordinates via
/// [`diagnostics::LogicalLine::logical_offset_from_physical`]; word
/// extracted with hover's helpers ([`navigation::word_at`]); the word must
/// overlap a real indexed occurrence of itself or resolution fails.
///
/// Returns `None` when no variable sits under this position — callers
/// answer with their shape's empty result (definition → null result,
/// references → empty list).
pub(crate) fn resolve_cursor_occurrence(
    doc: &str,
    logicals: &[diagnostics::LogicalLine],
    index: &[navigation::VariableHit],
    enc: PositionEncoding,
    line: usize,
    character: usize,
) -> Option<CursorOccurrence> {
    let ll_idx = diagnostics::covering_logical_line_index(logicals, line)?;
    let ll = &logicals[ll_idx];
    let phys_text = doc.lines().nth(line).unwrap_or("");
    let char_byte = lsp_character_to_byte_offset(phys_text, character, enc);
    let cursor = ll.logical_offset_from_physical(line, char_byte)?;
    let word = navigation::word_at(ll.text(), cursor);
    let hit = navigation::hit_at_cursor(index, word, ll_idx, cursor)?;
    Some(CursorOccurrence {
        logical_line: ll_idx,
        cursor,
        name: hit.name.clone(),
    })
}

/// Compute the `textDocument/definition` RESULT for an already-validated
/// request: `null`, or one `Location` at the exact name-token span of the
/// declaration chosen by `navigation::choose_definition`'s deterministic
/// rule (closest preceding same-name declaration; first one if none
/// precedes).
pub(crate) fn goto_definition_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
) -> serde_json::Value {
    // ONE continuation-aware join per request, feeding both the index and
    // the cursor resolution below.
    let logicals = diagnostics::logical_lines(doc);
    let index = navigation::build_variable_index(&logicals);
    let Some(occ) = resolve_cursor_occurrence(doc, &logicals, &index, enc, line, character) else {
        return serde_json::Value::Null;
    };
    let Some(decl) =
        navigation::choose_definition(&index, &occ.name, (occ.logical_line, occ.cursor))
    else {
        // A usage exists but no declaration shares its name — nothing
        // honest to point at.
        return serde_json::Value::Null;
    };
    let lines: Vec<&str> = doc.lines().collect();
    navigation_location_value(uri, &lines, &logicals, decl, enc)
}

/// Compute the `textDocument/references` RESULT: the chosen declaration
/// first when `include_declaration` is set, then every `$usage` of the
/// name in document order, capped at `navigation::MAX_REFERENCES` total.
pub(crate) fn references_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    include_declaration: bool,
) -> Vec<serde_json::Value> {
    // ONE continuation-aware join per request, feeding both the index and
    // the cursor resolution below.
    let logicals = diagnostics::logical_lines(doc);
    let index = navigation::build_variable_index(&logicals);
    let Some(occ) = resolve_cursor_occurrence(doc, &logicals, &index, enc, line, character) else {
        return Vec::new();
    };
    let declaration = if include_declaration {
        navigation::choose_definition(&index, &occ.name, (occ.logical_line, occ.cursor))
    } else {
        None
    };
    let refs = navigation::collect_references(&index, &occ.name, declaration);
    let lines: Vec<&str> = doc.lines().collect();
    refs.iter()
        .map(|h| navigation_location_value(uri, &lines, &logicals, h, enc))
        .collect()
}

// ── Server state ────────────────────────────────────────────────
pub(crate) struct Server {
    pub(crate) data: MenuData,
    pub(crate) docs: HashMap<String, String>, // URI → document text
    /// Position encoding negotiated during `initialize`; defaults to UTF-16
    /// (the spec default) until then.
    pub(crate) position_encoding: PositionEncoding,
    /// Whether the `shutdown` request was answered before `exit`.
    /// LSP 3.17 requires exit status 0 only when shutdown preceded exit.
    pub(crate) shutdown_received: bool,
    /// Live device config (opt-in, never contains pass in logs).
    pub(crate) live_config: LiveConfig,
    /// Shared live cache (TTL-scoped, in-memory only, capped).
    pub(crate) live_cache: Arc<Mutex<LiveCache>>,
    /// Test-only spy: publishDiagnostics notifications are recorded here
    /// instead of written to stdout (see [`Server::publish_diagnostics`]),
    /// letting tests assert that a publish actually fired. The field does
    /// not exist in production builds.
    #[cfg(test)]
    pub(crate) published: Vec<(String, serde_json::Value)>,
}

impl Server {
    /// Production constructor: live config and cache are provided by `main.rs`
    /// (parsed from env, TTL 60 s, caps from `caps.rs`).
    #[cfg(not(test))]
    pub(crate) fn new(
        data: MenuData,
        live_config: LiveConfig,
        live_cache: Arc<Mutex<LiveCache>>,
    ) -> Self {
        Server {
            data,
            docs: HashMap::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
            live_config,
            live_cache,
        }
    }

    /// Test constructor: live is disabled, cache is empty (honest placeholders).
    #[cfg(test)]
    pub(crate) fn new(data: MenuData) -> Self {
        Server {
            data,
            docs: HashMap::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
            live_config: LiveConfig::from_env_with(|_| None),
            live_cache: Arc::new(Mutex::new(LiveCache::with_default_ttl())),
            published: Vec::new(),
        }
    }

    /// Test helper: create a server with explicit live config/cache.
    #[cfg(test)]
    pub(crate) fn new_with_live(
        data: MenuData,
        live_config: LiveConfig,
        live_cache: Arc<Mutex<LiveCache>>,
    ) -> Self {
        Server {
            data,
            docs: HashMap::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
            live_config,
            live_cache,
            published: Vec::new(),
        }
    }

    /// Production helper: construct with explicit live values (used by
    /// non-test code that needs to build a server without env).
    #[cfg(not(test))]
    #[allow(dead_code)]
    pub(crate) fn new_with_live(
        data: MenuData,
        live_config: LiveConfig,
        live_cache: Arc<Mutex<LiveCache>>,
    ) -> Self {
        Self::new(data, live_config, live_cache)
    }

    pub(crate) fn run(&mut self) {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());

        loop {
            // Read one framed message. A Protocol framing failure is terminal
            // (exit code 1 — no shutdown was received): the stream cannot be
            // resynchronized, so terminate and let the client's supervisor
            // restart a clean server. I/O failures exit cleanly as before.
            let body = match read_message(&mut reader) {
                Ok(Frame::Message(body)) => body,
                Ok(Frame::Eof) => return,
                Ok(Frame::Skipped) => continue,
                Err(FrameError::Io(e)) => {
                    log_error!("read error: {e}");
                    return;
                }
                Err(FrameError::Protocol(why)) => {
                    log_error!(
                        "unrecoverable framing error ({why}) — terminating with code {} \
                         so the client supervisor restarts a clean server",
                        exit_code(false)
                    );
                    std::process::exit(exit_code(false));
                }
            };

            let msg: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    log_warn!("JSON parse error: {e}");
                    continue;
                }
            };

            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

            let response = self.handle_message(method, &msg);

            if let Some(resp) = response {
                let json = match serde_json::to_string(&resp) {
                    Ok(j) => j,
                    Err(e) => {
                        log_error!("failed to serialize response: {e}");
                        continue;
                    }
                };
                let header = format!("Content-Length: {}\r\n\r\n", json.len());
                let mut stdout = std::io::stdout().lock();
                if let Err(e) = stdout.write_all(header.as_bytes()) {
                    log_error!("write header error: {e}");
                    return;
                }
                if let Err(e) = stdout.write_all(json.as_bytes()) {
                    log_error!("write body error: {e}");
                    return;
                }
                if let Err(e) = stdout.flush() {
                    log_error!("flush error: {e}");
                    return;
                }
            }
        }
    }

    pub(crate) fn handle_message(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let id = params.get("id").cloned().unwrap_or(serde_json::Value::Null);

        match method {
            "initialize" => {
                // Reuses the request `id` extracted once above the dispatch.
                // Negotiate the position encoding (LSP 3.17): prefer utf-8
                // (internal positions are byte offsets); otherwise fall back
                // to utf-16, which is also the mandated default when the
                // client sends no capability. NOTE: per LSP 3.17 the array
                // lives at InitializeParams.capabilities.general.positionEncodings.
                let client_offers_utf8 =
                    params["params"]["capabilities"]["general"]["positionEncodings"]
                        .as_array()
                        .map(|encodings| encodings.iter().any(|v| v.as_str() == Some("utf-8")))
                        .unwrap_or(false);
                self.position_encoding = if client_offers_utf8 {
                    PositionEncoding::Utf8
                } else {
                    PositionEncoding::Utf16
                };
                log_debug!(
                    "negotiated position encoding: {}",
                    self.position_encoding.as_str()
                );
                // Visible feedback for live connection system.
                self.live_config.log_status();
                log_info!(
                    "live status on initialize: enabled={} active={} host={} scheme={} ssl_verify_effective={}",
                    self.live_config.enabled,
                    self.live_config.is_active(),
                    self.live_config.host,
                    self.live_config.scheme(),
                    self.live_config.ssl_verify_effective()
                );
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "positionEncoding": self.position_encoding.as_str(),
                            // Incremental sync (change = 2): the patching
                            // path (apply_incremental_edit) is authoritative;
                            // full-text changes remain supported as fallback.
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 2
                            },
                            // ':' opens script-word completions (statement
                            // snippets / script globals);
                            // compute_completions filters that context to
                            // ':'-prefixed labels only.
                            "completionProvider": {
                                "triggerCharacters": ["/", " ", "=", ":"],
                            },
                            "hoverProvider": true,
                            "documentSymbolProvider": true,
                            "foldingRangeProvider": true,
                            // Variable navigation: go-to-definition and
                            // find-references for `:local`/`:global`
                            // declarations vs `$name` usages — pure logic
                            // lives in navigation.rs.
                            "definitionProvider": true,
                            "referencesProvider": true,
                            // Quick-fixes ("Did you mean …?") for
                            // unknown-property / unknown-menu /
                            // invalid-enum-value diagnostics.
                            "codeActionProvider": true,
                            // Named-parameter signature popup; same space/=
                            // triggers completion uses (typing a new property
                            // or its `=` re-issues the request).
                            "signatureHelpProvider": {
                                "triggerCharacters": [" ", "="]
                            },
                            "diagnosticProvider": {
                                "interFileDependencies": false,
                                "workspaceDiagnostics": false
                            },
                            "executeCommandProvider": {
                                "commands": ["rsc.live.refresh", "rsc.live.status"]
                            }
                        },
                        "serverInfo": {
                            "name": "mikrotik-rsc-ls",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    },
                }))
            }

            "shutdown" => {
                // Latch that shutdown was answered: the subsequent `exit` must
                // then terminate with status 0 (LSP 3.17 exit semantics).
                self.shutdown_received = true;
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null,
                }))
            }

            "exit" => {
                let code = exit_code(self.shutdown_received);
                if code != 0 {
                    log_warn!("exit without prior shutdown (LSP 3.17) — exiting with code {code}");
                }
                std::process::exit(code);
            }

            "textDocument/didOpen" => {
                let uri = params["params"]["textDocument"]["uri"].as_str()?;
                // Validate URI scheme — only file:// URIs are expected; reject others to avoid
                // leaking path handling or storing attacker-controlled arbitrary schemes.
                if !is_valid_file_uri(uri) {
                    log_warn!("rejecting didOpen with non-file URI: {uri:?}");
                    return None;
                }
                let text = params["params"]["textDocument"]["text"].as_str()?;
                let uri_owned = uri.to_string();
                if text.len() > MAX_DOC_SIZE {
                    log_warn!(
                        "document too large ({} bytes > {MAX_DOC_SIZE}), truncating: {uri:?}",
                        text.len()
                    );
                    // Truncate at char boundary to avoid invalid UTF-8
                    let trunc_idx = floor_char_boundary(text, MAX_DOC_SIZE);
                    self.docs
                        .insert(uri_owned.clone(), text[..trunc_idx].to_string());
                } else {
                    if self.docs.len() >= MAX_DOCS && !self.docs.contains_key(&uri_owned) {
                        log_warn!(
                            "too many open documents ({} >= {MAX_DOCS}), rejecting: {uri:?}",
                            self.docs.len()
                        );
                        return None;
                    }
                    self.docs.insert(uri_owned.clone(), text.to_string());
                }
                // Publish diagnostics (push) after open. Borrow the stored
                // text instead of cloning it — a full copy costs up to
                // MAX_DOC_SIZE per keystroke-path open.
                let diags = match self.docs.get(&uri_owned) {
                    Some(doc_text) => self.encoded_diagnostics(doc_text, &uri_owned),
                    None => Vec::new(),
                };
                self.publish_diagnostics(&uri_owned, diags);
                None
            }

            "textDocument/didChange" => {
                // This server advertises textDocumentSync change = 2
                // (Incremental), so clients normally send range-scoped edits.
                // For robustness, handle both:
                // - Incremental sync: changes contain "range" + "text" (patch doc).
                // - Full sync: each change contains only "text" (replace doc).
                let uri = params["params"]["textDocument"]["uri"].as_str()?;
                if !is_valid_file_uri(uri) {
                    log_warn!("rejecting didChange with non-file URI: {uri:?}");
                    return None;
                }
                let changes = params["params"]["contentChanges"].as_array()?;
                if changes.is_empty() {
                    return None;
                }
                // Enforce doc count cap on first insert via didChange (client may skip didOpen)
                if !self.docs.contains_key(uri) && self.docs.len() >= MAX_DOCS {
                    log_warn!(
                        "too many open documents ({} >= {MAX_DOCS}), rejecting didChange: {uri:?}",
                        self.docs.len()
                    );
                    return None;
                }
                for change in changes {
                    // A malformed element must neither abandon the batch nor
                    // skip the trailing publish: earlier edits were already
                    // applied to the document, and later elements still
                    // deserve processing. (Formerly `?` returned None out of
                    // handle_message here — silently dropping the rest of the
                    // batch AND the publish below, desynchronizing client and
                    // server state.) Log the bad element and keep going.
                    let Some(text) = change.get("text").and_then(|t| t.as_str()) else {
                        log_warn!(
                            "didChange: skipping contentChanges element without a string 'text' \
                             for {uri:?}"
                        );
                        continue;
                    };
                    // Reject or truncate oversize incremental payloads early
                    if text.len() > MAX_DOC_SIZE {
                        log_warn!(
                            "change text too large ({} > {MAX_DOC_SIZE}), truncating",
                            text.len()
                        );
                        let trunc_idx = floor_char_boundary(text, MAX_DOC_SIZE);
                        let truncated = &text[..trunc_idx];
                        if let Some(range) = change.get("range") {
                            let needs_insert: bool;
                            let mut truncate_needed = false;
                            {
                                if let Some(doc) = self.docs.get_mut(uri) {
                                    if apply_incremental_edit(
                                        doc,
                                        range,
                                        truncated,
                                        self.position_encoding,
                                    )
                                    .is_err()
                                    {
                                        needs_insert = true;
                                    } else {
                                        needs_insert = false;
                                        if doc.len() > MAX_DOC_SIZE {
                                            truncate_needed = true;
                                        }
                                    }
                                    if truncate_needed {
                                        let ti = floor_char_boundary(doc, MAX_DOC_SIZE);
                                        doc.truncate(ti);
                                    }
                                } else {
                                    needs_insert = true;
                                }
                            }
                            if needs_insert {
                                self.docs.insert(uri.to_string(), truncated.to_string());
                            }
                        } else {
                            self.docs.insert(uri.to_string(), truncated.to_string());
                        }
                        continue;
                    }
                    if let Some(range) = change.get("range") {
                        let mut fallback_insert: Option<String> = None;
                        let mut truncate_doc = false;
                        {
                            if let Some(doc) = self.docs.get_mut(uri) {
                                if apply_incremental_edit(doc, range, text, self.position_encoding)
                                    .is_err()
                                {
                                    // Fallback: replace whole document if incremental patch fails.
                                    if text.len() > MAX_DOC_SIZE {
                                        let ti = floor_char_boundary(text, MAX_DOC_SIZE);
                                        fallback_insert = Some(text[..ti].to_string());
                                    } else {
                                        fallback_insert = Some(text.to_string());
                                    }
                                } else if doc.len() > MAX_DOC_SIZE {
                                    truncate_doc = true;
                                }
                            } else {
                                // No existing doc — treat as full insert.
                                fallback_insert = Some(text.to_string());
                            }
                        }
                        if let Some(s) = fallback_insert {
                            self.docs.insert(uri.to_string(), s);
                            // Check resulting doc size after fallback insert
                            if let Some(d) = self.docs.get_mut(uri)
                                && d.len() > MAX_DOC_SIZE
                            {
                                let ti = floor_char_boundary(d, MAX_DOC_SIZE);
                                d.truncate(ti);
                            }
                        } else if truncate_doc && let Some(doc) = self.docs.get_mut(uri) {
                            let ti = floor_char_boundary(doc, MAX_DOC_SIZE);
                            doc.truncate(ti);
                        }
                    } else {
                        // Full sync — last change wins.
                        self.docs.insert(uri.to_string(), text.to_string());
                    }
                }
                // Publish diagnostics after changes (incremental or full) —
                // reached even when individual elements above were skipped.
                // Borrow the stored text instead of cloning it (up to
                // MAX_DOC_SIZE per keystroke).
                let uri_owned = uri.to_string();
                let diags = match self.docs.get(&uri_owned) {
                    Some(doc_text) => self.encoded_diagnostics(doc_text, &uri_owned),
                    None => Vec::new(),
                };
                self.publish_diagnostics(&uri_owned, diags);
                None
            }

            "textDocument/didClose" => {
                if let Some(uri) = params["params"]["textDocument"]["uri"].as_str() {
                    self.docs.remove(uri);
                    // Clear diagnostics for closed file
                    self.publish_diagnostics(uri, Vec::new());
                }
                None
            }

            "textDocument/completion" => {
                // Requests must always be answered: malformed params →
                // -32602, untracked URI (never opened, closed, or rejected
                // at MAX_DOCS) → spec-permitted null result. Never silence.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.line"));
                };
                let Some(character) = pos["character"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.character"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("completion for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };

                // Convert the wire `character` ONCE into a byte offset within
                // the cursor line, extracted with the same `str::lines()` split
                // that `build_before_cursor` uses internally, so encoding math
                // cannot diverge from the string being sliced.
                let line_idx = line as usize;
                let current_line = doc.lines().nth(line_idx).unwrap_or("");
                let char_byte = lsp_character_to_byte_offset(
                    current_line,
                    character as usize,
                    self.position_encoding,
                );
                let before_cursor = build_before_cursor(doc, line_idx, char_byte);
                // Live enrichment: stale-while-revalidate (non-blocking).
                // Completion only reads fresh cache; misses trigger a background
                // thread that hydrates for the next keystroke. Coalescing and
                // negative TTL prevent retry spam. Never logs pass.
                if self.live_config.is_active() {
                    let context = parse_line(&self.data, &before_cursor);
                    let target_resource = if let Some(last_tok) =
                        crate::parser::tokenize(&before_cursor).last()
                        && last_tok.contains('=')
                    {
                        let key = last_tok
                            .split('=')
                            .next()
                            .unwrap_or("")
                            .trim_start_matches(':');
                        if let Some(menu) = self.data.menu_by_path.get(&context.path)
                            && let Some(arg) = menu.arguments.iter().find(|a| a.name == key)
                        {
                            self.live_config
                                .resolve_resource_with_custom(&context.path, key, &arg.arg_type)
                                .or_else(|| {
                                    crate::live::live_resource_for_menu_property(
                                        &context.path,
                                        key,
                                        &arg.arg_type,
                                    )
                                })
                        } else {
                            self.live_config
                                .resolve_resource_with_custom(&context.path, key, "")
                                .or_else(|| {
                                    crate::live::live_resource_for_menu_property(
                                        &context.path,
                                        key,
                                        "",
                                    )
                                })
                        }
                    } else {
                        Some(crate::live::ResourceKind::Interfaces)
                    };

                    if let Some(res) = target_resource {
                        let _ = get_cached_or_fetch_background(
                            &self.live_cache,
                            &self.live_config,
                            res,
                        );
                        log_debug!("live background fetch triggered for {:?}", res);
                    } else {
                        let _ = get_cached_or_fetch_background(
                            &self.live_cache,
                            &self.live_config,
                            crate::live::ResourceKind::Interfaces,
                        );
                    }
                    // Custom resource specific background fetch (separate cache key `custom:<property>`).
                    // Coalesced via `LiveCache::can_spawn_fetch` / `is_negative_cooldown`
                    // and `record_fetch_attempt` to avoid 16 threads/min.
                    if let Some(last_tok) = crate::parser::tokenize(&before_cursor).last()
                        && last_tok.contains('=')
                    {
                        let key = last_tok
                            .split('=')
                            .next()
                            .unwrap_or("")
                            .trim_start_matches(':');
                        if let Some(custom) =
                            self.live_config.custom_resource_for_property(key).cloned()
                        {
                            let cache_key = format!("custom:{}", custom.property);
                            // Check coalescing / negative TTL / fresh hit before spawning.
                            let should_spawn = {
                                let mut guard =
                                    self.live_cache.lock().expect("live cache lock poisoned");
                                if guard.try_get_cached(&cache_key).is_some() {
                                    log_debug!(
                                        "live custom cache hit (fresh) for {cache_key}, skipping fetch"
                                    );
                                    false
                                } else if guard.is_negative_cooldown(&cache_key) {
                                    log_debug!(
                                        "live custom negative cooldown for {cache_key}, skipping fetch"
                                    );
                                    false
                                } else if !guard.can_spawn_fetch(&cache_key) {
                                    log_debug!(
                                        "live custom fetch coalesced for {cache_key}, skipping"
                                    );
                                    false
                                } else {
                                    guard.record_fetch_attempt(cache_key.clone());
                                    true
                                }
                            };
                            if should_spawn {
                                let cache_clone = Arc::clone(&self.live_cache);
                                let config_clone = self.live_config.clone();
                                std::thread::spawn(move || {
                                    let start = std::time::Instant::now();
                                    match crate::live::fetch_custom_resource(&config_clone, &custom)
                                    {
                                        Ok(vals) => {
                                            log_info!(
                                                "live fetch ok custom property={} path={} latency_ms={} items={}",
                                                custom.property,
                                                custom.path,
                                                start.elapsed().as_millis(),
                                                vals.len()
                                            );
                                            let mut guard = cache_clone
                                                .lock()
                                                .expect("live cache lock poisoned");
                                            let cache_key = format!("custom:{}", custom.property);
                                            if vals.is_empty() {
                                                // Cache empty vec briefly (negative TTL) to avoid churn;
                                                // still considered fresh for coalescing.
                                                log_debug!(
                                                    "live custom fetch empty for {}, inserting empty vec",
                                                    custom.property
                                                );
                                                guard.insert(cache_key.clone(), vals);
                                                // Also ensure negative not set (insert clears it).
                                            } else {
                                                guard.insert(cache_key, vals);
                                            }
                                        }
                                        Err(e) => {
                                            log_warn!(
                                                "live fetch custom failed property={} path={} err={} latency_ms={}",
                                                custom.property,
                                                custom.path,
                                                e,
                                                start.elapsed().as_millis()
                                            );
                                            let mut guard = cache_clone
                                                .lock()
                                                .expect("live cache lock poisoned");
                                            let cache_key = format!("custom:{}", custom.property);
                                            guard.insert_negative(cache_key);
                                        }
                                    }
                                });
                            }
                        }
                    }
                }
                let live_guard = self.live_cache.lock().ok();
                let mut items = if let Some(ref cache) = live_guard {
                    completion::compute_completions_with_live(
                        &self.data,
                        &before_cursor,
                        Some(cache as &LiveCache),
                    )
                } else {
                    completion::compute_completions(&self.data, &before_cursor)
                };

                // ── textEdit injection (C2) ─────────────────────────────────
                // Populate `textEdit` so accepting a completion replaces the
                // already-typed prefix instead of inserting beside it
                // (`in` + `input` → `input`, not `ininput`). `insertText` is
                // retained as fallback for clients that ignore `textEdit` (Zed
                // supports it). If computing the range fails we leave `textEdit`
                // as `None` and the client falls back to insertion at cursor.
                //
                // Two cases are handled (per spec, at least these):
                // - value completions after `=`: range covers the typed suffix
                //   after `=` (excluding a leading opening quote so `"in` → `input`
                //   preserves the quote as `"input`);
                // - sub-menu / verb completions before a verb: when the cursor
                //   sits inside a partial token that prefixes a child name, range
                //   covers that token so `addr` → `address`.
                // For all other cases (e.g., already-finished token + space) the
                // edit is zero-length at the cursor (pure insertion).
                {
                    // Use the physical current line for byte offsets; `before_cursor`
                    // may be a joined multi-line string whose offsets do not map
                    // to the LSP line/character requested.
                    let line_text = current_line;
                    // Decide whether we are in a value-completion context using
                    // the same tolerant trimmed logic as `completion::match_context`.
                    let trimmed_bc = before_cursor.trim_end();
                    let has_trailing_ws = trimmed_bc.len() != before_cursor.len();
                    let trimmed_last = crate::parser::tokenize(trimmed_bc)
                        .last()
                        .cloned()
                        .unwrap_or_default();
                    let mut value_range: Option<(usize, usize)> = None;
                    if let Some(eq_pos) = trimmed_last.rfind('=') {
                        let raw_suffix = &trimmed_last[eq_pos + 1..];
                        let trimmed_suffix = raw_suffix.trim_matches(|c| c == '"' || c == '\'');
                        if !has_trailing_ws || trimmed_suffix.is_empty() {
                            // Value context confirmed — compute byte range in the
                            // physical line.
                            let prefix_line = &line_text[..char_byte.min(line_text.len())];
                            // Clamp to char boundary already ensured by char_byte.
                            let tokens = crate::parser::tokenize_with_spans(prefix_line);
                            if let Some(tok) = tokens.last() {
                                if let Some(pos) = tok.text.rfind('=') {
                                    let suffix_part = &tok.text[pos + 1..];
                                    let leading = if suffix_part.starts_with('"')
                                        || suffix_part.starts_with('\'')
                                    {
                                        1
                                    } else {
                                        0
                                    };
                                    let start = tok.start + pos + 1 + leading;
                                    // Trailing-space with empty value → zero-length at cursor
                                    let (s, e) = if has_trailing_ws && trimmed_suffix.is_empty() {
                                        (char_byte, char_byte)
                                    } else {
                                        (start, char_byte)
                                    };
                                    // Defensive clamp
                                    let s_clamped = s.min(line_text.len()).min(e);
                                    let e_clamped = e.min(line_text.len());
                                    let s_floored =
                                        crate::encoding::floor_char_boundary(line_text, s_clamped);
                                    let e_floored =
                                        crate::encoding::floor_char_boundary(line_text, e_clamped);
                                    value_range = Some((s_floored, e_floored));
                                }
                            } else if has_trailing_ws && trimmed_suffix.is_empty() {
                                // No token in prefix (e.g., cursor after "chain= " where
                                // tokenization of prefix_line yields ["chain="] but we already
                                // handled; this branch is for safety).
                                value_range = Some((char_byte, char_byte));
                            }
                        }
                    }
                    if let Some((s_byte, e_byte)) = value_range {
                        let start_char = match self.position_encoding {
                            PositionEncoding::Utf8 => s_byte as u32,
                            PositionEncoding::Utf16 => {
                                byte_offset_to_utf16_units(line_text, s_byte)
                            }
                        };
                        let end_char = match self.position_encoding {
                            PositionEncoding::Utf8 => e_byte as u32,
                            PositionEncoding::Utf16 => {
                                byte_offset_to_utf16_units(line_text, e_byte)
                            }
                        };
                        for item in &mut items {
                            // Value items are ENUM_MEMBER (12). Live + static values share it.
                            if item.kind == Some(12) {
                                let new_text = item
                                    .insert_text
                                    .clone()
                                    .unwrap_or_else(|| item.label.clone());
                                item.text_edit = Some(completion::TextEdit {
                                    range: completion::CompletionRange {
                                        start: completion::CompletionPosition {
                                            line: line_idx as u32,
                                            character: start_char,
                                        },
                                        end: completion::CompletionPosition {
                                            line: line_idx as u32,
                                            character: end_char,
                                        },
                                    },
                                    new_text,
                                });
                            }
                        }
                    } else {
                        // Sub-menu / verb prefix case: when not a value context and
                        // cursor inside a token that prefixes a child/verb, replace it.
                        // Heuristic: last token in prefix_line is non-empty, does not
                        // contain `=`, `:`, `/` at token level? Actually path tokens
                        // contain `/`, so we skip those. We only handle plain word
                        // prefixes for sub-menus/verbs.
                        let prefix_line = &line_text[..char_byte.min(line_text.len())];
                        if !prefix_line.ends_with(char::is_whitespace) && !prefix_line.is_empty() {
                            let tokens = crate::parser::tokenize_with_spans(prefix_line);
                            if let Some(tok) = tokens.last() {
                                // Skip tokens that are path, value, or script word
                                if !tok.text.contains('=')
                                    && !tok.text.starts_with(':')
                                    && !tok.text.starts_with('/')
                                    && !tok.text.starts_with('"')
                                    && !tok.text.starts_with('\'')
                                {
                                    let typed = tok.text.as_str();
                                    // Check if any CLASS or FUNCTION item is a case-insensitive prefix match
                                    let lower_typed = typed.to_ascii_lowercase();
                                    let needs_edit = items.iter().any(|it| {
                                        (it.kind == Some(9) || it.kind == Some(3))
                                            && it
                                                .label
                                                .to_ascii_lowercase()
                                                .starts_with(&lower_typed)
                                    });
                                    if needs_edit && !typed.is_empty() {
                                        let s_byte = tok.start;
                                        let e_byte = tok.end.min(char_byte);
                                        let start_char = match self.position_encoding {
                                            PositionEncoding::Utf8 => s_byte as u32,
                                            PositionEncoding::Utf16 => {
                                                byte_offset_to_utf16_units(line_text, s_byte)
                                            }
                                        };
                                        let end_char = match self.position_encoding {
                                            PositionEncoding::Utf8 => e_byte as u32,
                                            PositionEncoding::Utf16 => {
                                                byte_offset_to_utf16_units(line_text, e_byte)
                                            }
                                        };
                                        for item in &mut items {
                                            if item.kind == Some(9) || item.kind == Some(3) {
                                                // Only add edit if label actually starts with typed (avoid replacing unrelated verbs)
                                                if item
                                                    .label
                                                    .to_ascii_lowercase()
                                                    .starts_with(&lower_typed)
                                                {
                                                    let new_text = item
                                                        .insert_text
                                                        .clone()
                                                        .unwrap_or_else(|| item.label.clone());
                                                    item.text_edit = Some(completion::TextEdit {
                                                        range: completion::CompletionRange {
                                                            start: completion::CompletionPosition {
                                                                line: line_idx as u32,
                                                                character: start_char,
                                                            },
                                                            end: completion::CompletionPosition {
                                                                line: line_idx as u32,
                                                                character: end_char,
                                                            },
                                                        },
                                                        new_text,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isIncomplete": false,
                        "items": items,
                    },
                }))
            }

            "textDocument/hover" => {
                // Same response guarantees as completion: -32602 for
                // malformed params, null result for untracked URIs.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.line"));
                };
                let Some(character) = pos["character"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.character"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("hover for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };
                let line = line as usize;
                let current_line = doc.lines().nth(line).unwrap_or("");

                // Same single boundary conversion as completion: the wire
                // `character` becomes a byte offset within `current_line`,
                // which is exactly the slice `compute_hover` inspects and the
                // line `build_before_cursor` re-slices internally.
                let char_byte = lsp_character_to_byte_offset(
                    current_line,
                    character as usize,
                    self.position_encoding,
                );

                let hover = hover::compute_hover(&self.data, current_line, char_byte, doc, line);

                let result = match hover {
                    Some(h) => match serde_json::to_value(h) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            log_error!("hover serialize error: {e}");
                            None
                        }
                    },
                    None => None,
                };

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/signatureHelp" => {
                // Same response guarantees as completion/hover: -32602 for
                // malformed params (echoed id), null result for untracked
                // URIs. Null is ALSO the anti-noise contract's answer when
                // the line resolves to no menu+verb pair — no verb, no popup.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.line"));
                };
                let Some(character) = pos["character"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.character"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("signatureHelp for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };

                // Wire `character` → byte offset within the cursor's
                // PHYSICAL line, using the exact same lines() split the
                // logical-line join consumes below.
                let line_idx = line as usize;
                let current_line = doc.lines().nth(line_idx).unwrap_or("");
                let char_byte = lsp_character_to_byte_offset(
                    current_line,
                    character as usize,
                    self.position_encoding,
                );

                // ONE continuation-aware join per REQUEST (same cost profile
                // as codeAction), shared by the covering lookup, menu
                // resolution, tokenization, and cursor mapping — all of which
                // must agree on what "the command under the cursor" is.
                let logicals = diagnostics::logical_lines(doc);
                let help = diagnostics::covering_logical_line(&logicals, line_idx).and_then(|ll| {
                    let ctx = parse_line(&self.data, ll.text());
                    let menu = self.data.menu_by_path.get(&ctx.path)?;
                    let tokens = tokenize_with_spans(ll.text());
                    let verb_idx = signature::resolve_verb_token(&self.data, &tokens)?;
                    let cursor_logical = ll.logical_offset_from_physical(line_idx, char_byte)?;
                    signature::compute_signature_help(menu, &tokens, verb_idx, cursor_logical)
                });

                let result = match help {
                    Some(h) => match serde_json::to_value(h) {
                        Ok(v) => v,
                        Err(e) => {
                            log_error!("signatureHelp serialize error: {e}");
                            serde_json::Value::Null
                        }
                    },
                    None => serde_json::Value::Null,
                };

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/documentSymbol" => {
                // Same response guarantees as completion: -32602 for
                // malformed params, null result for untracked URIs.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("documentSymbol for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };
                let symbols = symbols::compute_document_symbols(&self.data, doc);

                // Symbol ranges are computed in byte coordinates; convert
                // every endpoint into the negotiated wire encoding, exactly
                // like the diagnostic pipeline does before emission.
                let lines: Vec<&str> = doc.lines().collect();
                let mut wire_symbols = symbols;
                for sym in &mut wire_symbols {
                    crate::convert_position(&mut sym.range.start, &lines, self.position_encoding);
                    crate::convert_position(&mut sym.range.end, &lines, self.position_encoding);
                    crate::convert_position(
                        &mut sym.selection_range.start,
                        &lines,
                        self.position_encoding,
                    );
                    crate::convert_position(
                        &mut sym.selection_range.end,
                        &lines,
                        self.position_encoding,
                    );
                }

                let result = match serde_json::to_value(&wire_symbols) {
                    Ok(v) => v,
                    Err(e) => {
                        // Unserializable output is a bug, not a client error:
                        // degrade to an empty list rather than dropping the
                        // request (requests must be answered).
                        log_error!("documentSymbol serialize error: {e}");
                        serde_json::Value::Array(Vec::new())
                    }
                };

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/definition" => {
                // Same response guarantees as hover: -32602 for malformed
                // params (echoed id), null result for untracked URIs. Null
                // is ALSO the answer when the cursor does not sit on a
                // RouterOS script variable — a definition must never be
                // invented that cannot be grounded in an indexed
                // `:local`/`:global` declaration.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.line"));
                };
                let Some(character) = pos["character"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.character"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("definition for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };

                let result = goto_definition_result(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                );

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/references" => {
                // List-shaped like codeAction: -32602 for malformed params,
                // empty array (a valid Location[] result) for untracked
                // URIs and variable-less positions. `context.includeDeclaration`
                // is REQUIRED by LSP ReferenceParams, so an absent or
                // non-bool context mirrors the sibling handlers' -32602
                // strictness instead of silently guessing false.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.line"));
                };
                let Some(character) = pos["character"].as_u64() else {
                    return Some(invalid_params_response(&id, "missing position.character"));
                };
                let Some(include_declaration) =
                    params["params"]["context"]["includeDeclaration"].as_bool()
                else {
                    return Some(invalid_params_response(
                        &id,
                        "missing context.includeDeclaration",
                    ));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("references for untracked URI, returning empty list: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [],
                    }));
                };

                let result = references_result(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                    include_declaration,
                );

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/foldingRange" => {
                // Same response guarantees as documentSymbol. Folding
                // ranges are line-only, so no position-encoding conversion
                // applies.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("foldingRange for untracked URI, returning null result: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };
                let ranges = folding::compute_folding_ranges(doc);
                let result = match serde_json::to_value(&ranges) {
                    Ok(v) => v,
                    Err(e) => {
                        log_error!("foldingRange serialize error: {e}");
                        serde_json::Value::Array(Vec::new())
                    }
                };
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/diagnostic" => {
                // Pull diagnostics (LSP 3.17+)
                let uri = params["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or("");
                if !uri.is_empty() && !is_valid_file_uri(uri) {
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "kind": "full",
                            "items": []
                        }
                    }));
                }
                // Borrow the stored text (an empty result for an untracked
                // URI is identical to diagnosing an empty document, minus
                // the pointless work).
                let diags = match self.docs.get(uri) {
                    Some(doc_text) => self.encoded_diagnostics(doc_text, uri),
                    None => Vec::new(),
                };
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "kind": "full",
                        "items": diags
                    }
                }))
            }

            "workspace/diagnostic" => {
                // Workspace diagnostics not supported (interFileDependencies false)
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "items": []
                    }
                }))
            }

            "textDocument/codeAction" => {
                // Quick-fixes ("Did you mean …?") for our own
                // unknown-property / unknown-menu / invalid-enum-value
                // diagnostics. Same response
                // guarantees as the other request handlers: -32602 for
                // malformed params; an untracked URI answers with an EMPTY
                // action list (a valid CodeAction[] result), never an error.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return Some(invalid_params_response(&id, "missing textDocument.uri"));
                };
                let Some(client_diags) = params["params"]["context"]["diagnostics"].as_array()
                else {
                    return Some(invalid_params_response(&id, "missing context.diagnostics"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!("codeAction for untracked URI, returning empty list: {uri:?}");
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [],
                    }));
                };
                let actions = self.compute_code_actions(uri, doc, client_diags);
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": actions,
                }))
            }

            "workspace/executeCommand" => {
                let command = params["params"]["command"].as_str().unwrap_or("");
                log_info!("workspace/executeCommand received: {}", command);
                match command {
                    "rsc.live.refresh" => {
                        let mut guard = self.live_cache.lock().expect("live cache lock poisoned");
                        let before = guard.entries.len();
                        let args = params["params"]["arguments"].as_array();
                        if let Some(arr) = args {
                            if arr.is_empty() {
                                guard.clear_all();
                            } else {
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        // Support both raw keys and cache keys; clear exact.
                                        guard.clear_key(s);
                                        // Also try to map property-like args to cache keys if needed.
                                        // No extra mapping; caller should pass cache keys like "interfaces".
                                    }
                                }
                            }
                        } else {
                            guard.clear_all();
                        }
                        let after = guard.entries.len();
                        drop(guard);
                        // Optionally trigger background fetch for all ResourceKind if active.
                        if self.live_config.is_active() {
                            for &kind in &[
                                crate::live::ResourceKind::Interfaces,
                                crate::live::ResourceKind::IpAddresses,
                                crate::live::ResourceKind::AddressLists,
                                crate::live::ResourceKind::FirewallFilterChains,
                                crate::live::ResourceKind::IpPools,
                            ] {
                                let _ = get_cached_or_fetch_background(
                                    &self.live_cache,
                                    &self.live_config,
                                    kind,
                                );
                            }
                        }
                        log_info!(
                            "live refresh executed before={} after={} command={}",
                            before,
                            after,
                            command
                        );
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "cleared": true,
                                "before": before,
                                "after": after
                            }
                        }))
                    }
                    "rsc.live.status" => {
                        let guard = self.live_cache.lock().expect("live cache lock poisoned");
                        let entries = guard.entries.len();
                        let failed = guard.failed_at.len();
                        drop(guard);
                        let status = serde_json::json!({
                            "enabled": self.live_config.enabled,
                            "active": self.live_config.is_active(),
                            "host": self.live_config.host,
                            "hosts": self.live_config.hosts,
                            "port": self.live_config.port,
                            "scheme": self.live_config.scheme(),
                            "ssl_verify": self.live_config.ssl_verify,
                            "ssl_verify_effective": self.live_config.ssl_verify_effective(),
                            "timeout_secs": self.live_config.timeout_secs,
                            "cache_entries": entries,
                            "failed_entries": failed,
                            "custom_resources": self.live_config.custom_resources.len()
                        });
                        log_info!("live status queried: {}", status);
                        Some(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": status
                        }))
                    }
                    _ => Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Unknown command: {command}")
                        }
                    })),
                }
            }

            "workspace/didChangeConfiguration" => {
                let settings = &params["params"]["settings"];
                if settings.is_null() || !settings.is_object() {
                    // No settings: re-read from env.
                    self.live_config = crate::live::LiveConfig::from_env();
                } else {
                    self.live_config = crate::live::LiveConfig::from_settings_value(settings);
                }
                self.live_config.log_status();
                log_info!("live config reloaded via didChangeConfiguration");
                // Notifications have no id; if this was unexpectedly sent as a request, answer with null.
                if id.is_null() {
                    None
                } else {
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null
                    }))
                }
            }

            _ => {
                // Unknown method
                if !id.is_null() {
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Method not found: {method}"),
                        },
                    }))
                } else {
                    None
                }
            }
        }
    }

    /// Compute diagnostics for `doc` and convert emitted range characters
    /// from internal byte-offset semantics to the negotiated position
    /// encoding. Shared by both push paths (didOpen / didChange) and the
    /// pull handler so they can never diverge.
    pub(crate) fn encoded_diagnostics(
        &self,
        doc_text: &str,
        uri: &str,
    ) -> Vec<diagnostics::Diagnostic> {
        let diags = diagnostics::compute_diagnostics(&self.data, doc_text, uri);
        convert_diagnostic_ranges(diags, doc_text, self.position_encoding)
    }

    /// Build `quickfix` CodeActions for client-echoed diagnostics.
    ///
    /// Eligibility: `source == "rsc-ls"` AND code `unknown-property`,
    /// `unknown-menu`, or `invalid-enum-value`. Anything else — foreign
    /// sources, other codes, missing or mistyped fields — is ignored by
    /// design: a quick-fix must never be invented that cannot be grounded
    /// in the document.
    ///
    /// For each eligible diagnostic the mistyped token is recovered from
    /// the tracked document at the diagnostic's own range (never by
    /// re-parsing our message text) and [`suggest::best_candidate`] picks
    /// a deterministic replacement under the length-aware threshold. The
    /// candidate SET depends on the code:
    ///
    /// - `unknown-property`: the property names of THE menu the
    ///   diagnostic's line belongs to, resolved with the same
    ///   line-resolution machinery the diagnostic pipeline uses
    ///   ([`diagnostics::resolve_menu_for_line`]).
    /// - `unknown-menu`: every known menu path.
    /// - `invalid-enum-value`: the enum members of the argument named by
    ///   the `key=value` token pair whose value span overlaps the
    ///   diagnostic range. The pair is located WITHOUT touching the
    ///   message: the covering logical line is tokenized with spans
    ///   (exactly like signature help), wire positions are mapped into
    ///   logical coordinates via [`diagnostics::LogicalLine::logical_offset_from_physical`],
    ///   and only the pair's KEY is consumed — the replacement text is
    ///   derived from the recovered range slice itself, so even a stale
    ///   range can never splice mismatched text. A quoted typo is
    ///   repaired to a quoted member in the SAME quote style; every
    ///   unresolved link (no menu, no pair, no argument, no members) or
    ///   candidate beyond threshold yields no action rather than a guess.
    ///
    /// Total actions are capped at [`MAX_CODE_ACTIONS`].
    pub(crate) fn compute_code_actions(
        &self,
        uri: &str,
        doc: &str,
        client_diags: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let mut actions = Vec::new();
        if client_diags.is_empty() {
            return actions;
        }
        // One continuation-aware logical-line join per REQUEST, shared by
        // every diagnostic below — not one join per diagnostic.
        let logicals = diagnostics::logical_lines(doc);

        for diag in client_diags {
            if actions.len() >= MAX_CODE_ACTIONS {
                break;
            }
            if diag.get("source").and_then(|s| s.as_str()) != Some(diagnostics::DIAGNOSTIC_SOURCE) {
                continue;
            }
            // LSP Diagnostic.code is number|string; ours are strings, so a
            // numeric or absent code fails this binding and is skipped.
            let Some(code) = diag.get("code").and_then(|c| c.as_str()) else {
                continue;
            };
            // The offending-token range, echoed verbatim into the edit.
            let Some(range) = diag.get("range") else {
                continue;
            };
            let Some((start_line, start_char)) = wire_position(range.get("start")) else {
                continue;
            };
            let Some((end_line, end_char)) = wire_position(range.get("end")) else {
                continue;
            };

            // Recover the mistyped token text from the document itself.
            // Stale ranges (pointing outside the current text) and absurdly
            // long spans yield no suggestion rather than a wild guess.
            let start_off =
                lsp_position_to_offset(doc, start_line, start_char, self.position_encoding);
            let end_off = lsp_position_to_offset(doc, end_line, end_char, self.position_encoding);
            let (start_off, end_off) = match (start_off, end_off) {
                (Ok(s), Ok(e)) if e > s && e - s <= suggest::MAX_SUGGEST_INPUT_BYTES => (s, e),
                _ => continue,
            };
            let input = &doc[start_off..end_off];

            let fix = match code {
                // Candidate set: the property names of THE menu the
                // diagnostic's line belongs to. If that menu cannot be
                // resolved (implicit parent, stale range), skip — never
                // guess across all menus.
                "unknown-property" => {
                    let Some(menu) =
                        diagnostics::resolve_menu_for_line(&self.data, &logicals, start_line)
                    else {
                        continue;
                    };
                    suggest::best_candidate(
                        input,
                        menu.arguments
                            .iter()
                            .chain(menu.flags.iter())
                            .chain(menu.read_only.iter())
                            .map(|a| &a.name),
                    )
                    .map(Suggestion::plain)
                }
                // Candidate set: every known menu path. best_candidate is
                // deterministic, so HashMap iteration order is irrelevant.
                "unknown-menu" => suggest::best_candidate(input, self.data.menu_by_path.keys())
                    .map(Suggestion::plain),
                // Candidate set: the enum members of the `key=value` pair
                // the diagnostic points at, recovered WITHOUT parsing the
                // message. The Rule 5 range covers exactly the value part
                // (quotes included), so the pair is found by matching that
                // range against token VALUE spans in logical coordinates —
                // the same tokenize-the-logical-line walk signature help
                // performs.
                "invalid-enum-value" => {
                    let Some(menu) =
                        diagnostics::resolve_menu_for_line(&self.data, &logicals, start_line)
                    else {
                        continue;
                    };
                    let Some(ll) = diagnostics::covering_logical_line(&logicals, start_line) else {
                        continue;
                    };
                    // Wire characters → byte offsets within their own
                    // physical lines → logical offsets (the conversion
                    // chain the signature-help handler uses); token spans
                    // live in logical coordinates, so the comparison must
                    // happen there.
                    let lines: Vec<&str> = doc.lines().collect();
                    let to_logical = |line_idx: usize, character: usize| -> Option<usize> {
                        let text = lines.get(line_idx)?;
                        let byte =
                            lsp_character_to_byte_offset(text, character, self.position_encoding);
                        ll.logical_offset_from_physical(line_idx, byte)
                    };
                    let (Some(log_start), Some(log_end)) = (
                        to_logical(start_line, start_char),
                        to_logical(end_line, end_char),
                    ) else {
                        continue;
                    };
                    // First key=value token whose VALUE part overlaps the
                    // diagnostic range; only its KEY is consumed — the
                    // repaired text comes from `input`, the exact slice
                    // this edit replaces.
                    let tokens = tokenize_with_spans(ll.text());
                    let Some(key) = tokens.iter().find_map(|t| {
                        let eq = t.text.find('=')?;
                        let value_start = t.start + eq + 1;
                        (log_start < t.end && log_end > value_start).then(|| &t.text[..eq])
                    }) else {
                        continue;
                    };
                    // Mirror the Rule 5 emitter: only `arguments` carry
                    // enum-typed values, and an argument without resolvable
                    // members never guesses.
                    let Some(arg) = menu.arguments.iter().find(|a| a.name == *key) else {
                        continue;
                    };
                    let members = arg.enum_members();
                    if members.is_empty() {
                        continue;
                    }
                    // Strip quotes exactly like the Rule 5 emitter did
                    // before validating, so distance is measured against
                    // the bare member.
                    let trimmed_input = input.trim();
                    let stripped = trimmed_input.trim_matches('"').trim_matches('\'');
                    let Some(member) = suggest::best_candidate(stripped, members.iter()) else {
                        continue;
                    };
                    // Preserve the value's quote style: a quoted typo is
                    // repaired to a quoted member, a bare one stays bare.
                    // Only a symmetric pair of quotes counts; anything
                    // else (debris, unterminated string) edits as bare.
                    let quote_style = trimmed_input.chars().next().filter(|&q| {
                        (q == '"' || q == '\'')
                            && trimmed_input.len() >= 2
                            && trimmed_input.ends_with(q)
                    });
                    Some(Suggestion {
                        new_text: match quote_style {
                            Some(q) => format!("{q}{member}{q}"),
                            None => member.clone(),
                        },
                        title_subject: member,
                    })
                }
                _ => continue,
            };
            let Some(fix) = fix else {
                continue;
            };

            // serde_json's json! macro cannot take the dynamic URI as a map
            // key, so the per-URI change list is inserted explicitly.
            let mut changes = serde_json::Map::new();
            changes.insert(
                uri.to_string(),
                serde_json::json!([{
                    "range": range,
                    "newText": fix.new_text,
                }]),
            );
            actions.push(serde_json::json!({
                "title": format!("Did you mean '{}'?", fix.title_subject),
                "kind": "quickfix",
                "diagnostics": [diag],
                "edit": { "changes": changes },
            }));
        }
        actions
    }

    /// Emit one `textDocument/publishDiagnostics` notification.
    ///
    /// Write-failure policy (deliberate, documented): a failed write is
    /// logged and SURVIVED — it must NOT kill the server loop. A
    /// notification carries no `id` the client blocks on; transient stdout
    /// backpressure or a momentarily closed client pipe should not take the
    /// session down, and if stdout is truly dead the next request/response
    /// write in [`Server::run`] fails and terminates cleanly anyway.
    /// Request-path failures, by contrast, already surface as JSON-RPC
    /// error responses (or fatal run-loop exits), preserving that split.
    ///
    /// Under `cargo test` the serialized notification is recorded on the
    /// server instead of written (stdout writes would be swallowed by the
    /// test harness capture), giving tests an observable spy.
    pub(crate) fn publish_diagnostics(
        &mut self,
        uri: &str,
        diagnostics: Vec<diagnostics::Diagnostic>,
    ) {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": diagnostics
            }
        });
        #[cfg(test)]
        {
            self.published.push((uri.to_string(), notif));
        }
        #[cfg(not(test))]
        {
            match serde_json::to_string(&notif) {
                Ok(json) => {
                    let header = format!("Content-Length: {}\r\n\r\n", json.len());
                    let mut stdout = std::io::stdout().lock();
                    if let Err(e) = stdout
                        .write_all(header.as_bytes())
                        .and_then(|_| stdout.write_all(json.as_bytes()))
                        .and_then(|_| stdout.flush())
                    {
                        // Non-fatal by policy — see the doc comment above.
                        log_error!(
                            "failed to write publishDiagnostics notification for {uri:?}: {e}"
                        );
                    }
                }
                Err(e) => {
                    // Serialization failure is a bug, not a client
                    // condition; non-fatal by the same policy.
                    log_error!("failed to serialize publishDiagnostics notification: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::caps::{MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS};
    use crate::diagnostics;
    use crate::menus::MenuData;
    use crate::{Server, is_valid_file_uri};

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
"#,
        )
    }

    // ── Caps constants ────────────────────────────────────────────────
    //
    // The shared caps themselves are pinned by exact value in `caps.rs`
    // (and cross-checked by tests/test_enclosure.py). The tests below are
    // behavioral companions: they prove each cap is actually enforced.

    #[test]
    fn test_caps_max_diag_bytes_is_500kb() {
        assert_eq!(MAX_DIAG_BYTES, 500_000);
        // Behavioral companion: a doc larger than 500KB is truncated before
        // diagnosis instead of blowing up.
        let data = synthetic_data();
        let line = "/ip/address add address=1.1.1.1 interface=ether1\n";
        // ~50 bytes per line -> 20k lines = ~1M bytes
        let doc = line.repeat(20_000);
        assert!(doc.len() > 500_000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        // Diagnostics are capped; should not blow up
        assert!(diags.len() <= 3000);
    }

    #[test]
    fn test_caps_max_diag_lines_is_3000() {
        assert_eq!(MAX_DIAG_LINES, 3000);
        let data = synthetic_data();
        let doc = "/unknown/menu add foo=bar\n".repeat(5000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        assert!(
            diags.len() <= 3000,
            "diag lines capped at 3000, got {}",
            diags.len()
        );
    }

    // ── URI validation ────────────────────────────────────────────────

    #[test]
    fn test_uri_valid_file_uris() {
        assert!(is_valid_file_uri("file:///home/user/test.rsc"));
        assert!(is_valid_file_uri("file:///test.rsc"));
        assert!(is_valid_file_uri("file:///a/b/c/d.rsc"));
    }

    #[test]
    fn test_uri_rejects_untitled() {
        assert!(!is_valid_file_uri("untitled://test.rsc"));
        assert!(!is_valid_file_uri("untitled:Untitled-1"));
    }

    #[test]
    fn test_uri_rejects_http_and_https() {
        assert!(!is_valid_file_uri("http://example.com/test.rsc"));
        assert!(!is_valid_file_uri("https://example.com/test.rsc"));
    }

    #[test]
    fn test_uri_rejects_other_schemes() {
        assert!(!is_valid_file_uri("ftp://example.com/file.rsc"));
        assert!(!is_valid_file_uri("vscode://file/test.rsc"));
        assert!(!is_valid_file_uri("file:/test.rsc")); // only one slash
        assert!(!is_valid_file_uri("/test.rsc"));
        assert!(!is_valid_file_uri(""));
    }

    #[test]
    fn test_uri_rejects_path_traversal() {
        assert!(!is_valid_file_uri("file:///home/../etc/passwd"));
        assert!(!is_valid_file_uri("file:///test/../secret.rsc"));
        assert!(!is_valid_file_uri("file:///a/b/../../c.rsc"));
    }

    #[test]
    fn test_uri_rejects_null_byte() {
        assert!(!is_valid_file_uri("file:///test\0.rsc"));
        assert!(!is_valid_file_uri("file://\0/test.rsc"));
        let uri_with_null = format!("file:///test{}.rsc", '\0');
        assert!(!is_valid_file_uri(&uri_with_null));
    }

    #[test]
    fn test_uri_allows_valid_with_dots_in_name() {
        // Single dot is fine, double dot is not
        assert!(is_valid_file_uri("file:///home/user/file.test.rsc"));
        assert!(is_valid_file_uri("file:///home/user/.hidden.rsc"));
        assert!(!is_valid_file_uri("file:///home/user/..hidden.rsc"));
    }

    // ── didOpen / didChange / didClose handling ───────────────────────

    #[test]
    fn test_server_did_open_valid_file_uri_stores_doc() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///test.rsc", "text": "/ip/address add address=1.1.1.1"}}
        });
        let resp = server.handle_message("textDocument/didOpen", &open);
        assert!(resp.is_none());
        assert_eq!(
            server.docs.get("file:///test.rsc").unwrap(),
            "/ip/address add address=1.1.1.1"
        );
    }

    #[test]
    fn test_server_did_open_rejects_untitled_uri() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "untitled://test.rsc", "text": "hello"}}
        });
        let resp = server.handle_message("textDocument/didOpen", &open);
        assert!(resp.is_none());
        assert!(!server.docs.contains_key("untitled://test.rsc"));
    }

    #[test]
    fn test_server_did_open_rejects_http_uri() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "http://example.com/test.rsc", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert!(!server.docs.contains_key("http://example.com/test.rsc"));
    }

    #[test]
    fn test_server_did_open_rejects_traversal_uri() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///home/../etc/passwd", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert!(!server.docs.contains_key("file:///home/../etc/passwd"));
    }

    #[test]
    fn test_server_did_open_rejects_null_byte_uri() {
        let mut server = Server::new(synthetic_data());
        let uri = format!("file:///test{}.rsc", '\0');
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Should not store doc with null byte
        assert!(server.docs.is_empty());
    }

    #[test]
    fn test_server_did_change_rejects_invalid_uri() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///valid.rsc", "text": "old"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "untitled://valid.rsc"}, "contentChanges": [{"text": "new"}]}
        });
        server.handle_message("textDocument/didChange", &change);
        // Original doc should remain unchanged
        assert_eq!(server.docs.get("file:///valid.rsc").unwrap(), "old");
        assert!(!server.docs.contains_key("untitled://valid.rsc"));
    }

    #[test]
    fn test_server_did_change_malformed_element_does_not_abort_batch_or_publish() {
        // Regression: a contentChanges element without a "text" field must
        // be skipped, not abort the whole didChange. The former `?` returned
        // None out of handle_message mid-batch — abandoning already-applied
        // edits and skipping the trailing diagnostics publish.
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///batch.rsc", "text": "hello world"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        server.published.clear(); // drop the didOpen publish

        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///batch.rsc"}, "contentChanges": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"},
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}},
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "text": "Rust"}
            ]}
        });
        let resp = server.handle_message("textDocument/didChange", &change);
        assert!(resp.is_none());

        // Both valid edits applied — including the one AFTER the malformed
        // element ("hello world" → "hi world" → "hi Rust").
        assert_eq!(
            server.docs.get("file:///batch.rsc").unwrap(),
            "hi Rust",
            "the batch must survive a malformed element"
        );

        // The trailing diagnostics publish still fired, exactly once.
        assert_eq!(
            server.published.len(),
            1,
            "publish must run after the batch despite the malformed element"
        );
        let (published_uri, notif) = &server.published[0];
        assert_eq!(published_uri, "file:///batch.rsc");
        assert_eq!(notif["method"], "textDocument/publishDiagnostics");
        assert_eq!(notif["params"]["uri"], "file:///batch.rsc");
    }

    #[test]
    fn test_server_did_close_removes_doc_and_clears() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///close.rsc", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert!(server.docs.contains_key("file:///close.rsc"));
        let close = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///close.rsc"}}
        });
        let resp = server.handle_message("textDocument/didClose", &close);
        assert!(resp.is_none());
        assert!(!server.docs.contains_key("file:///close.rsc"));
    }

    #[test]
    fn test_server_did_close_nonexistent_is_noop() {
        let mut server = Server::new(synthetic_data());
        let close = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///notopen.rsc"}}
        });
        let resp = server.handle_message("textDocument/didClose", &close);
        assert!(resp.is_none());
    }

    // ── MAX_DOC_SIZE enforcement ──────────────────────────────────────

    #[test]
    fn test_server_did_open_truncates_large_doc_at_5mib() {
        let mut server = Server::new(synthetic_data());
        let large_text = "a".repeat(MAX_DOC_SIZE + 1000);
        assert!(large_text.len() > MAX_DOC_SIZE);
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///large.rsc", "text": large_text}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let stored = server
            .docs
            .get("file:///large.rsc")
            .expect("should store truncated doc");
        assert_eq!(stored.len(), MAX_DOC_SIZE);
    }

    #[test]
    fn test_server_did_open_exact_max_size_not_truncated() {
        let mut server = Server::new(synthetic_data());
        let exact = "a".repeat(5 * 1024 * 1024);
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///exact.rsc", "text": exact.clone()}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert_eq!(
            server.docs.get("file:///exact.rsc").unwrap().len(),
            exact.len()
        );
    }

    #[test]
    fn test_server_did_change_full_sync_truncation() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///a.rsc", "text": "small"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let large = "b".repeat(5 * 1024 * 1024 + 500);
        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"text": large}]}
        });
        server.handle_message("textDocument/didChange", &change);
        // Full sync with an oversize payload takes the early truncation
        // branch (text.len() > MAX_DOC_SIZE → truncate at a char boundary
        // and store): the stored text is capped at EXACTLY MAX_DOC_SIZE.
        // 'b' is ASCII, so the char boundary is byte-exact here.
        let stored = server.docs.get("file:///a.rsc").unwrap();
        assert_eq!(
            stored.len(),
            MAX_DOC_SIZE,
            "full sync must truncate to exactly MAX_DOC_SIZE"
        );
    }

    // ── MAX_DOCS enforcement ──────────────────────────────────────────

    #[test]
    fn test_server_max_docs_enforced_at_100() {
        let mut server = Server::new(synthetic_data());
        for i in 0..MAX_DOCS {
            let uri = format!("file:///test{i}.rsc");
            let open = serde_json::json!({
                "params": {"textDocument": {"uri": uri, "text": "hello"}}
            });
            server.handle_message("textDocument/didOpen", &open);
        }
        assert_eq!(server.docs.len(), MAX_DOCS);
        // 101st should be rejected
        let open101 = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///test101.rsc", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open101);
        assert_eq!(server.docs.len(), MAX_DOCS);
        assert!(!server.docs.contains_key("file:///test101.rsc"));
    }

    #[test]
    fn test_server_max_docs_allows_update_existing_when_full() {
        let mut server = Server::new(synthetic_data());
        for i in 0..MAX_DOCS {
            let uri = format!("file:///test{i}.rsc");
            let open = serde_json::json!({
                "params": {"textDocument": {"uri": uri, "text": "hello"}}
            });
            server.handle_message("textDocument/didOpen", &open);
        }
        // Update existing doc should succeed even at cap
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///test0.rsc", "text": "updated"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert_eq!(server.docs.len(), MAX_DOCS);
        assert_eq!(server.docs.get("file:///test0.rsc").unwrap(), "updated");
    }

    #[test]
    fn test_server_did_change_max_docs_enforced() {
        let mut server = Server::new(synthetic_data());
        for i in 0..MAX_DOCS {
            let uri = format!("file:///doc{i}.rsc");
            let open = serde_json::json!({
                "params": {"textDocument": {"uri": uri, "text": "hi"}}
            });
            server.handle_message("textDocument/didOpen", &open);
        }
        // didChange to a new URI should be rejected when at cap
        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///new.rsc"}, "contentChanges": [{"text": "hello"}]}
        });
        server.handle_message("textDocument/didChange", &change);
        assert!(!server.docs.contains_key("file:///new.rsc"));
        assert_eq!(server.docs.len(), MAX_DOCS);
    }

    // ── Large doc truncation preserves first N diags ──────────────────

    #[test]
    fn test_large_doc_truncation_preserves_first_diags() {
        let data = synthetic_data();
        // First 10 lines are errors, next 5000 lines are also errors but beyond cap
        let mut doc = String::new();
        for _ in 0..10 {
            doc.push_str("/unknown/menu add foo=bar\n");
        }
        for _ in 0..5000 {
            doc.push_str("/another/unknown add x=1\n");
        }
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        assert!(diags.len() <= 3000);
        // First diagnostics should be for /unknown/menu (preserved)
        assert!(diags.iter().any(|d| d.message.contains("/unknown/menu")));
        // Diagnostics beyond 3000 lines should not appear
        // Count of diags should be exactly 3000 (one per line) or less if bytes cap hits first
        assert!(!diags.is_empty());
    }

    #[test]
    fn test_large_doc_bytes_truncation_preserves_first_diags() {
        let data = synthetic_data();
        // Create a doc >500KB where first lines have errors and truncated tail is beyond bytes cap
        let error_line = "/unknown/menu add foo=bar\n"; // ~25 bytes
        // Need >500KB: 25 * 25000 = 625K
        let doc = error_line.repeat(25_000);
        assert!(doc.len() > 500_000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        // Should be capped but preserve first
        assert!(!diags.is_empty());
        assert!(
            diags
                .iter()
                .all(|d| d.message.contains("/unknown/menu") || d.message.contains("/another"))
        );
        // Ensure truncation at char boundary didn't cause panic and preserved first diags
        let first_diag_line = diags.first().unwrap().range.start.line;
        assert_eq!(first_diag_line, 0);
    }

    // ── Incremental edits with diagnostics ────────────────────────────

    #[test]
    fn test_incremental_edit_then_diagnostics_updated() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///inc.rsc", "text": "/ip/address add address=1.1.1.1 interface=ether1"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Valid doc should have no unknown-menu diags
        let diags_before = diagnostics::compute_diagnostics(
            &synthetic_data(),
            server.docs.get("file:///inc.rsc").unwrap(),
            "file:///inc.rsc",
        );
        assert!(
            !diags_before
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );

        // Incremental edit: change to unknown menu
        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///inc.rsc"}, "contentChanges": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 11}},
                "text": "/unknown/menu"
            }]}
        });
        server.handle_message("textDocument/didChange", &change);
        let doc_after = server.docs.get("file:///inc.rsc").unwrap();
        assert!(doc_after.starts_with("/unknown/menu"));
        let diags_after =
            diagnostics::compute_diagnostics(&synthetic_data(), doc_after, "file:///inc.rsc");
        assert!(
            diags_after
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
    }

    #[test]
    fn test_incremental_edit_multiple_changes() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///multi.rsc", "text": "hello world"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let change = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///multi.rsc"}, "contentChanges": [
                {"range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}}, "text": "Rust"},
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"}
            ]}
        });
        server.handle_message("textDocument/didChange", &change);
        assert_eq!(server.docs.get("file:///multi.rsc").unwrap(), "hi Rust");
    }

    #[test]
    fn test_diagnostic_pull_rejects_invalid_uri() {
        let mut server = Server::new(synthetic_data());
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///valid.rsc", "text": "/ip/address add address=1.1.1.1"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let pull = serde_json::json!({
            "id": 1,
            "params": {"textDocument": {"uri": "untitled://valid.rsc"}}
        });
        let resp = server
            .handle_message("textDocument/diagnostic", &pull)
            .unwrap();
        let items = resp["result"]["items"].as_array().unwrap();
        assert!(
            items.is_empty(),
            "invalid URI should return empty diagnostics"
        );
    }

    #[test]
    fn test_server_publish_diagnostics_push_and_pull_consistency() {
        let mut server = Server::new(synthetic_data());
        let doc = "/unknown/menu add foo=bar";
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///consistency.rsc", "text": doc}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Pull diagnostics should match compute_diagnostics
        let pull = serde_json::json!({
            "id": 2,
            "params": {"textDocument": {"uri": "file:///consistency.rsc"}}
        });
        let resp = server
            .handle_message("textDocument/diagnostic", &pull)
            .unwrap();
        let pull_items = resp["result"]["items"].as_array().unwrap();
        let direct =
            diagnostics::compute_diagnostics(&synthetic_data(), doc, "file:///consistency.rsc");
        assert_eq!(pull_items.len(), direct.len());
    }
}
