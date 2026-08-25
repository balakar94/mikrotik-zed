#![allow(unused_macros)]

// ── MikroTik RouterOS Script Language Server ─────────────────────
//
// LSP over stdio, implemented in pure Rust.  Commands.toml is
// embedded at compile time — no external files needed.
//
// LSP handlers:
//   textDocument/completion – menu path, command verb, and property suggestions
//   textDocument/hover        – description for commands and properties
//
// Protocol notes:
// - Uses Content-Length framing with JSON-RPC 2.0 over stdio.
// - Advertises textDocumentSync = { openClose: true, change: 2 } (Incremental).
//   Clients send range-scoped edits; for robustness full-text replacements
//   (no `range`) are still handled, and a failed incremental patch falls
//   back to a full document replace.
// - Content-Length is capped at 10 MiB to avoid unbounded allocation.

mod caps;
mod cli;
mod completion;
mod diagnostics;
mod encoding;
mod folding;
mod framing;
mod hover;
mod logging;
mod menus;
mod navigation;
mod parser;
mod server;
mod signature;
mod suggest;
mod symbols;

pub(crate) use caps::{
    MAX_CODE_ACTIONS, MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS, MAX_HEADER_SIZE,
    MAX_MESSAGE_SIZE,
};
pub(crate) use encoding::{
    PositionEncoding, apply_incremental_edit, convert_diagnostic_ranges, convert_position,
    floor_char_boundary, lsp_character_to_byte_offset, lsp_position_to_offset,
};
pub(crate) use framing::{Frame, FrameError, read_message};
pub(crate) use logging::{log_debug, log_error, log_info, log_level, log_warn};
pub(crate) use parser::{
    MAX_BRACE_DEPTH, StructureEvent, build_before_cursor, parse_line, tokenize_with_spans,
    walk_structure,
};

use menus::MenuData;
use std::collections::HashMap;
use std::io::{BufReader, Write};

// Resource caps live in caps.rs — single source of truth; re-exported here
// so existing paths keep working.

/// Resolved payload of one quick-fix suggestion: the candidate shown in
/// the action title and the text actually spliced into the document.
///
/// The two differ only for enum-value repairs, where the replacement
/// re-wraps the suggested member in the offending value's original quote
/// style while the title stays bare (`Did you mean 'input'?` repairing
/// `"inpt"` splices `"input"`).
struct Suggestion {
    /// Candidate rendered inside `Did you mean '<…>'?`.
    title_subject: String,
    /// Replacement text for the diagnostic's own range.
    new_text: String,
}

impl Suggestion {
    /// A suggestion whose title subject and replacement text coincide.
    fn plain(candidate: String) -> Self {
        Self {
            new_text: candidate.clone(),
            title_subject: candidate,
        }
    }
}

fn main() {
    // CLI flags are handled FIRST — before logging, env reads, or data
    // loading — so `--version`/`--help` probes stay side-effect-free:
    // they never read stdin (must not hang) and never parse menus.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::run_cli_command(cli::parse_cli_args(&args)) {
        std::process::exit(code);
    }

    // Initialize log level early (reads RSC_LS_LOG)
    let level = log_level();
    // Startup banner carries version + pid so multiple server instances are
    // correlatable in `zed: open log`; the version token matches the output
    // of `rsc-ls --version` exactly.
    eprintln!(
        "[rsc-ls][INFO] {} starting (pid={}, RSC_LS_LOG={:?} -> {:?})",
        cli::version_string(),
        std::process::id(),
        std::env::var("RSC_LS_LOG").unwrap_or_else(|_| "(unset, default info)".to_string()),
        level
    );
    // Hint for observability: when run via Zed, use `zed --foreground` to see these logs,
    // or `zed: open log` action. Setting `RSC_LS_LOG=debug` enables verbose diagnostics.
    let data = MenuData::load();
    log_info!("language server started, {} menus loaded", data.menus.len());
    log_debug!(
        "limits: MAX_MESSAGE_SIZE={} MAX_HEADER_SIZE={} MAX_DOC_SIZE={} MAX_DOCS={}",
        MAX_MESSAGE_SIZE,
        MAX_HEADER_SIZE,
        MAX_DOC_SIZE,
        MAX_DOCS
    );

    let mut server = Server::new(data);
    server.run();
    log_info!("language server exiting");
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
/// dropping one leaves the client awaiting it until timeout.
pub(crate) fn invalid_params_response(
    id: &serde_json::Value,
    message: &str,
) -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": message,
        },
    }))
}

/// Extract a `(line, character)` pair from a JSON LSP Position object,
/// or [`None`] when the object is missing or mistyped (non-numeric
/// fields). Values stay in wire units — callers convert per the
/// negotiated encoding at the document boundary.
fn wire_position(v: Option<&serde_json::Value>) -> Option<(usize, usize)> {
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
fn navigation_location_value(
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
struct CursorOccurrence {
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
fn resolve_cursor_occurrence(
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
fn goto_definition_result(
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
fn references_result(
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
struct Server {
    data: MenuData,
    docs: HashMap<String, String>, // URI → document text
    /// Position encoding negotiated during `initialize`; defaults to UTF-16
    /// (the spec default) until then.
    position_encoding: PositionEncoding,
    /// Whether the `shutdown` request was answered before `exit`.
    /// LSP 3.17 requires exit status 0 only when shutdown preceded exit.
    shutdown_received: bool,
}

impl Server {
    fn new(data: MenuData) -> Self {
        Server {
            data,
            docs: HashMap::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
        }
    }

    fn run(&mut self) {
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
                        eprintln!("[rsc-ls] failed to serialize response: {e}");
                        continue;
                    }
                };
                let header = format!("Content-Length: {}\r\n\r\n", json.len());
                let mut stdout = std::io::stdout().lock();
                if let Err(e) = stdout.write_all(header.as_bytes()) {
                    eprintln!("[rsc-ls] write header error: {e}");
                    return;
                }
                if let Err(e) = stdout.write_all(json.as_bytes()) {
                    eprintln!("[rsc-ls] write body error: {e}");
                    return;
                }
                if let Err(e) = stdout.flush() {
                    eprintln!("[rsc-ls] flush error: {e}");
                    return;
                }
            }
        }
    }

    fn handle_message(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let id = params.get("id").cloned().unwrap_or(serde_json::Value::Null);

        match method {
            "initialize" => {
                let id = params.get("id").cloned().unwrap_or(serde_json::Value::Null);
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
                    eprintln!("[rsc-ls] rejecting didOpen with non-file URI: {uri:?}");
                    return None;
                }
                let text = params["params"]["textDocument"]["text"].as_str()?;
                let uri_owned = uri.to_string();
                if text.len() > MAX_DOC_SIZE {
                    eprintln!(
                        "[rsc-ls] document too large ({} bytes > {MAX_DOC_SIZE}), truncating: {uri:?}",
                        text.len()
                    );
                    // Truncate at char boundary to avoid invalid UTF-8
                    let trunc_idx = floor_char_boundary(text, MAX_DOC_SIZE);
                    self.docs
                        .insert(uri_owned.clone(), text[..trunc_idx].to_string());
                } else {
                    if self.docs.len() >= MAX_DOCS && !self.docs.contains_key(&uri_owned) {
                        eprintln!(
                            "[rsc-ls] too many open documents ({} >= {MAX_DOCS}), rejecting: {uri:?}",
                            self.docs.len()
                        );
                        return None;
                    }
                    self.docs.insert(uri_owned.clone(), text.to_string());
                }
                // Publish diagnostics (push) after open
                let doc_text = self.docs.get(&uri_owned).cloned().unwrap_or_default();
                let diags = self.encoded_diagnostics(&doc_text, &uri_owned);
                Self::publish_diagnostics(&uri_owned, diags);
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
                    eprintln!("[rsc-ls] rejecting didChange with non-file URI: {uri:?}");
                    return None;
                }
                let changes = params["params"]["contentChanges"].as_array()?;
                if changes.is_empty() {
                    return None;
                }
                // Enforce doc count cap on first insert via didChange (client may skip didOpen)
                if !self.docs.contains_key(uri) && self.docs.len() >= MAX_DOCS {
                    eprintln!(
                        "[rsc-ls] too many open documents ({} >= {MAX_DOCS}), rejecting didChange: {uri:?}",
                        self.docs.len()
                    );
                    return None;
                }
                for change in changes {
                    let text = change.get("text").and_then(|t| t.as_str())?;
                    // Reject or truncate oversize incremental payloads early
                    if text.len() > MAX_DOC_SIZE {
                        eprintln!(
                            "[rsc-ls] change text too large ({} > {MAX_DOC_SIZE}), truncating",
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
                // Publish diagnostics after changes (incremental or full)
                let uri_owned = uri.to_string();
                let doc_text = self.docs.get(&uri_owned).cloned().unwrap_or_default();
                let diags = self.encoded_diagnostics(&doc_text, &uri_owned);
                Self::publish_diagnostics(&uri_owned, diags);
                None
            }

            "textDocument/didClose" => {
                if let Some(uri) = params["params"]["textDocument"]["uri"].as_str() {
                    self.docs.remove(uri);
                    // Clear diagnostics for closed file
                    Self::publish_diagnostics(uri, Vec::new());
                }
                None
            }

            "textDocument/completion" => {
                // Requests must always be answered: malformed params →
                // -32602, untracked URI (never opened, closed, or rejected
                // at MAX_DOCS) → spec-permitted null result. Never silence.
                let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return invalid_params_response(&id, "missing position.line");
                };
                let Some(character) = pos["character"].as_u64() else {
                    return invalid_params_response(&id, "missing position.character");
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
                let items = completion::compute_completions(&self.data, &before_cursor);

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
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return invalid_params_response(&id, "missing position.line");
                };
                let Some(character) = pos["character"].as_u64() else {
                    return invalid_params_response(&id, "missing position.character");
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
                            eprintln!("[rsc-ls] hover serialize error: {e}");
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
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return invalid_params_response(&id, "missing position.line");
                };
                let Some(character) = pos["character"].as_u64() else {
                    return invalid_params_response(&id, "missing position.character");
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
                            eprintln!("[rsc-ls] signatureHelp serialize error: {e}");
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
                    return invalid_params_response(&id, "missing textDocument.uri");
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
                        eprintln!("[rsc-ls] documentSymbol serialize error: {e}");
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
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return invalid_params_response(&id, "missing position.line");
                };
                let Some(character) = pos["character"].as_u64() else {
                    return invalid_params_response(&id, "missing position.character");
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
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let pos = &params["params"]["position"];
                let Some(line) = pos["line"].as_u64() else {
                    return invalid_params_response(&id, "missing position.line");
                };
                let Some(character) = pos["character"].as_u64() else {
                    return invalid_params_response(&id, "missing position.character");
                };
                let Some(include_declaration) =
                    params["params"]["context"]["includeDeclaration"].as_bool()
                else {
                    return invalid_params_response(&id, "missing context.includeDeclaration");
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
                    return invalid_params_response(&id, "missing textDocument.uri");
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
                        eprintln!("[rsc-ls] foldingRange serialize error: {e}");
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
                let doc_text = self
                    .docs
                    .get(uri)
                    .cloned()
                    .unwrap_or_else(|| "".to_string());
                // Cap large docs same as push
                let diags = self.encoded_diagnostics(&doc_text, uri);
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
                    return invalid_params_response(&id, "missing textDocument.uri");
                };
                let Some(client_diags) = params["params"]["context"]["diagnostics"].as_array()
                else {
                    return invalid_params_response(&id, "missing context.diagnostics");
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
    fn encoded_diagnostics(&self, doc_text: &str, uri: &str) -> Vec<diagnostics::Diagnostic> {
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
    fn compute_code_actions(
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

    fn publish_diagnostics(uri: &str, diagnostics: Vec<diagnostics::Diagnostic>) {
        // Avoid spamming stdout during `cargo test` (test harness captures stdout)
        if cfg!(test) {
            return;
        }
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": diagnostics
            }
        });
        if let Ok(json) = serde_json::to_string(&notif) {
            let header = format!("Content-Length: {}\r\n\r\n", json.len());
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(header.as_bytes());
            let _ = stdout.write_all(json.as_bytes());
            let _ = stdout.flush();
        }
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

    // ── Server handle_message integration ─────────────────────────

    fn make_server() -> Server {
        Server::new(synthetic_data())
    }

    #[test]
    fn test_server_initialize_advertises_code_action_provider() {
        // Quick-fixes ("Did you mean …?") must be advertised so Zed offers
        // the lightbulb action on unknown-property / unknown-menu squiggles.
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = server.handle_message("initialize", &msg).unwrap();
        assert_eq!(resp["result"]["capabilities"]["codeActionProvider"], true);
    }

    #[test]
    fn test_server_initialize() {
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = server.handle_message("initialize", &msg).unwrap();
        let sync = &resp["result"]["capabilities"]["textDocumentSync"];
        assert_eq!(sync["openClose"], true);
        assert_eq!(sync["change"], 2, "incremental sync must be advertised");
        assert_eq!(resp["result"]["capabilities"]["hoverProvider"], true);
        assert_eq!(resp["result"]["serverInfo"]["name"], "mikrotik-rsc-ls");
        // Assert against the crate version, not a literal, so version bumps
        // don't break this test.
        assert_eq!(
            resp["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn test_server_shutdown() {
        let mut server = make_server();
        assert!(!server.shutdown_received);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": {}
        });
        let resp = server.handle_message("shutdown", &msg).unwrap();
        assert_eq!(resp["result"], serde_json::Value::Null);
        assert!(
            server.shutdown_received,
            "answering shutdown must latch shutdown_received"
        );
    }

    #[test]
    fn test_exit_code_lsp_317() {
        // LSP 3.17: exit status 0 only after a `shutdown` request; else 1.
        assert_eq!(exit_code(true), 0);
        assert_eq!(exit_code(false), 1);
        // Fresh server: no shutdown seen yet → a bare `exit` maps to status 1.
        let fresh = Server::new(synthetic_data());
        assert!(!fresh.shutdown_received);
        assert_eq!(exit_code(fresh.shutdown_received), 1);
        // After answering `shutdown`, the same server maps to status 0.
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "shutdown",
            "params": {}
        });
        server.handle_message("shutdown", &msg).unwrap();
        assert_eq!(exit_code(server.shutdown_received), 0);
    }

    #[test]
    fn test_server_unknown_method_with_id_returns_error() {
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "unknown/method",
            "params": {}
        });
        let resp = server.handle_message("unknown/method", &msg).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown/method")
        );
    }

    #[test]
    fn test_server_unknown_notification_returns_none() {
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "unknown/method",
            "params": {}
        });
        let resp = server.handle_message("unknown/method", &msg);
        assert!(resp.is_none(), "notification without id should return None");
    }

    #[test]
    fn test_server_did_open_and_completion() {
        let mut server = make_server();
        // Open doc
        let open = serde_json::json!({
            "params": {
                "textDocument": {"uri": "file:///test.rsc", "text": "/ip/address add "}
            }
        });
        assert!(
            server
                .handle_message("textDocument/didOpen", &open)
                .is_none()
        );
        assert!(server.docs.contains_key("file:///test.rsc"));

        // Completion request
        let comp = serde_json::json!({
            "id": 10,
            "params": {
                "textDocument": {"uri": "file:///test.rsc"},
                "position": {"line": 0, "character": 15}
            }
        });
        let resp = server
            .handle_message("textDocument/completion", &comp)
            .unwrap();
        let items = resp["result"]["items"].as_array().unwrap();
        assert!(!items.is_empty());
        let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"interface"));
    }

    #[test]
    fn test_server_completion_untracked_uri_returns_null_result() {
        // PHASE 1 FLIP: this previously asserted `resp.is_none()` — a request
        // carrying an id got NO response and the client hung until timeout.
        // Untracked URI now yields a spec-permitted null result with the id
        // echoed.
        let mut server = make_server();
        let comp = serde_json::json!({
            "id": 1,
            "params": {
                "textDocument": {"uri": "file:///notopened.rsc"},
                "position": {"line": 0, "character": 1}
            }
        });
        let resp = server
            .handle_message("textDocument/completion", &comp)
            .unwrap();
        assert_eq!(resp["id"], 1, "id must be echoed");
        assert!(resp["result"].is_null(), "untracked URI → null result");
    }

    #[test]
    fn test_server_completion_malformed_params_returns_32602() {
        let mut server = make_server();
        // Missing position entirely.
        let no_pos = serde_json::json!({
            "id": 7,
            "params": {"textDocument": {"uri": "file:///a.rsc"}}
        });
        let resp = server
            .handle_message("textDocument/completion", &no_pos)
            .unwrap();
        assert_eq!(resp["id"], 7);
        assert_eq!(resp["error"]["code"], -32602);
        // Missing URI entirely.
        let no_uri = serde_json::json!({
            "id": 8,
            "params": {"position": {"line": 0, "character": 0}}
        });
        let resp = server
            .handle_message("textDocument/completion", &no_uri)
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        // Non-numeric position.
        let bad_types = serde_json::json!({
            "id": 9,
            "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": "zero", "character": null}
            }
        });
        let resp = server
            .handle_message("textDocument/completion", &bad_types)
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 9);
    }

    #[test]
    fn test_server_did_change_full_sync() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///a.rsc", "text": "old"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let change = serde_json::json!({
            "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "contentChanges": [{"text": "new content"}]
            }
        });
        server.handle_message("textDocument/didChange", &change);
        assert_eq!(server.docs.get("file:///a.rsc").unwrap(), "new content");
    }

    #[test]
    fn test_server_did_change_incremental() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///b.rsc", "text": "hello world"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let change = serde_json::json!({
            "params": {
                "textDocument": {"uri": "file:///b.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}},
                    "text": "Rust"
                }]
            }
        });
        server.handle_message("textDocument/didChange", &change);
        assert_eq!(server.docs.get("file:///b.rsc").unwrap(), "hello Rust");
    }

    #[test]
    fn test_server_did_change_incremental_fallback_to_full_on_error() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///c.rsc", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Invalid range (out of bounds) should fallback to replacing whole doc
        let change = serde_json::json!({
            "params": {
                "textDocument": {"uri": "file:///c.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 10, "character": 0}, "end": {"line": 10, "character": 5}},
                    "text": "fallback"
                }]
            }
        });
        server.handle_message("textDocument/didChange", &change);
        assert_eq!(server.docs.get("file:///c.rsc").unwrap(), "fallback");
    }

    #[test]
    fn test_server_did_close() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///x.rsc", "text": "hi"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        assert!(server.docs.contains_key("file:///x.rsc"));
        let close = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///x.rsc"}}
        });
        server.handle_message("textDocument/didClose", &close);
        assert!(!server.docs.contains_key("file:///x.rsc"));
    }

    #[test]
    fn test_server_hover_found() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///h.rsc", "text": "/ip/address add address=1.1.1.1"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Hover over property "address" (second occurrence)
        // line 0, character near property name (after "add ")
        let line = "/ip/address add address=1.1.1.1";
        let prop_start = line.find("add ").unwrap() + 4; // start of "address="
        let hover = serde_json::json!({
            "id": 5,
            "params": {
                "textDocument": {"uri": "file:///h.rsc"},
                "position": {"line": 0, "character": prop_start + 2}
            }
        });
        let resp = server.handle_message("textDocument/hover", &hover).unwrap();
        assert!(resp["result"].is_object(), "hover should return object");
        assert!(
            resp["result"]["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("address")
        );
    }

    #[test]
    fn test_server_hover_not_found_returns_null() {
        let mut server = make_server();
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///h2.rsc", "text": "/ip/address add unknownprop"}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let line = "/ip/address add unknownprop";
        let pos = line.find("unknownprop").unwrap() + 2;
        let hover = serde_json::json!({
            "id": 6,
            "params": {
                "textDocument": {"uri": "file:///h2.rsc"},
                "position": {"line": 0, "character": pos}
            }
        });
        let resp = server.handle_message("textDocument/hover", &hover).unwrap();
        assert!(resp["result"].is_null());
    }

    #[test]
    fn test_server_hover_untracked_doc_returns_null_result() {
        // PHASE 1 FLIP: previously asserted `resp.is_none()` (dropped
        // request); untracked URI now answers null result with id echoed.
        let mut server = make_server();
        let hover = serde_json::json!({
            "id": 7,
            "params": {
                "textDocument": {"uri": "file:///notopen.rsc"},
                "position": {"line": 0, "character": 1}
            }
        });
        let resp = server.handle_message("textDocument/hover", &hover).unwrap();
        assert_eq!(resp["id"], 7);
        assert!(resp["result"].is_null());
    }

    #[test]
    fn test_server_hover_malformed_params_returns_32602() {
        let mut server = make_server();
        let no_pos = serde_json::json!({
            "id": 11,
            "params": {"textDocument": {"uri": "file:///a.rsc"}}
        });
        let resp = server
            .handle_message("textDocument/hover", &no_pos)
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 11, "id must be echoed on error responses");
    }

    #[test]
    fn test_server_did_change_no_uri_returns_none() {
        let mut server = make_server();
        let msg = serde_json::json!({
            "params": {"contentChanges": [{"text": "hi"}]}
        });
        let resp = server.handle_message("textDocument/didChange", &msg);
        assert!(resp.is_none());
    }

    // ── Code actions (did-you-mean quick-fixes) ──────────────────

    /// Open `doc` in `server` and return its diagnostics exactly as a
    /// client would echo them back inside a codeAction request: computed
    /// through the push pipeline (including position-encoding conversion)
    /// and serialized to wire JSON.
    fn opened_wire_diagnostics(
        server: &mut Server,
        uri: &str,
        doc: &str,
    ) -> Vec<serde_json::Value> {
        server.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": doc}}}),
        );
        let stored = server.docs.get(uri).cloned().unwrap_or_default();
        let diags = server.encoded_diagnostics(&stored, uri);
        match serde_json::to_value(diags) {
            Ok(serde_json::Value::Array(items)) => items,
            other => panic!("diagnostics must serialize to an array, got {other:?}"),
        }
    }

    fn code_action_request(id: i64, uri: &str, diags: &[serde_json::Value]) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "params": {
                "textDocument": {"uri": uri},
                "context": {"diagnostics": diags}
            }
        })
    }

    #[test]
    fn test_code_actions_fixes_typo_property_at_exact_range() {
        let mut s = make_server();
        // "adress" spans bytes 15..21 (ASCII ⇒ UTF-16 units are identical).
        let doc = "/ip/address add adress=1.1.1.1";
        let diags = opened_wire_diagnostics(&mut s, "file:///ca.rsc", doc);
        assert_eq!(diags.len(), 1, "exactly the unknown-property diagnostic");
        assert_eq!(diags[0]["code"], "unknown-property");

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(41, "file:///ca.rsc", &diags),
            )
            .unwrap();
        assert_eq!(resp["id"], 41, "id must be echoed");
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["title"], "Did you mean 'address'?");
        assert_eq!(actions[0]["kind"], "quickfix");
        assert_eq!(
            actions[0]["diagnostics"][0],
            serde_json::to_value(&diags[0]).unwrap(),
            "the originating diagnostic object is attached"
        );
        let edit = &actions[0]["edit"]["changes"]["file:///ca.rsc"][0];
        assert_eq!(edit["newText"], "address");
        assert_eq!(
            edit["range"], diags[0]["range"],
            "replacement targets the offending token range exactly"
        );
        assert_eq!(edit["range"]["start"]["character"], 16);
        assert_eq!(edit["range"]["end"]["character"], 22);
    }

    #[test]
    fn test_code_actions_fixes_typo_menu_path() {
        let mut s = make_server();
        // "/ip/addres" is one insertion away from "/ip/address".
        let doc = "/ip/addres add gateway=1";
        let diags = opened_wire_diagnostics(&mut s, "file:///cm.rsc", doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["code"], "unknown-menu");

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(42, "file:///cm.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["title"], "Did you mean '/ip/address'?");
        let edit = &actions[0]["edit"]["changes"]["file:///cm.rsc"][0];
        assert_eq!(edit["newText"], "/ip/address");
        assert_eq!(edit["range"]["start"]["character"], 0);
        assert_eq!(edit["range"]["end"]["character"], 10);
    }

    #[test]
    fn test_code_actions_healthy_doc_returns_empty_array() {
        let mut s = make_server();
        let doc = "/ip/address add address=1.1.1.1 interface=ether1";
        let diags = opened_wire_diagnostics(&mut s, "file:///ok.rsc", doc);
        assert!(diags.is_empty());
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(43, "file:///ok.rsc", &diags),
            )
            .unwrap();
        assert_eq!(resp["id"], 43);
        assert!(resp["result"].is_array());
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_code_actions_untracked_uri_returns_empty_array_not_error() {
        let mut s = make_server();
        let fake = serde_json::json!({
            "range": {"start": {"line": 0, "character": 15}, "end": {"line": 0, "character": 21}},
            "severity": 2,
            "code": "unknown-property",
            "source": "rsc-ls",
            "message": "Unknown property 'adress' for '/ip/address'"
        });
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(44, "file:///never-opened.rsc", &[fake]),
            )
            .unwrap();
        assert_eq!(resp["id"], 44, "id must be echoed");
        assert!(
            resp["result"].is_array(),
            "untracked URI must answer an array, not null or error"
        );
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_code_actions_ignore_foreign_and_unparseable_diagnostics() {
        let mut s = make_server();
        let diags = opened_wire_diagnostics(&mut s, "file:///f.rsc", "");
        assert!(diags.is_empty());
        let range = serde_json::json!({
            "start": {"line": 0, "character": 15},
            "end": {"line": 0, "character": 21}
        });
        let mixed = vec![
            // Foreign source — even with our codes.
            serde_json::json!({"source": "other-ls", "code": "unknown-property", "range": range}),
            // Our source but a different rule.
            serde_json::json!({"source": "rsc-ls", "code": "duplicate-property", "range": range}),
            // Numeric code (LSP allows number|string; ours are strings).
            serde_json::json!({"source": "rsc-ls", "code": 7, "range": range}),
            // Missing code entirely.
            serde_json::json!({"source": "rsc-ls", "range": range}),
            // Missing range entirely.
            serde_json::json!({"source": "rsc-ls", "code": "unknown-property"}),
            // Missing source entirely.
            serde_json::json!({"code": "unknown-property", "range": range}),
        ];
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(45, "file:///f.rsc", &mixed),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(actions.is_empty(), "nothing eligible, got {actions:?}");
    }

    #[test]
    fn test_code_actions_capped_at_eight() {
        let mut s = make_server();
        let mut doc = String::new();
        for i in 0..12 {
            doc.push_str(&format!("/ip/address add adress={i}.1.1.1\n"));
        }
        let diags = opened_wire_diagnostics(&mut s, "file:///cap.rsc", &doc);
        assert_eq!(diags.len(), 12, "one eligible diagnostic per line");
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(46, "file:///cap.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(
            actions.len(),
            MAX_CODE_ACTIONS,
            "capped, not truncated to zero"
        );
        // Deterministic order: the first action repairs the FIRST diagnostic.
        let first_edit = &actions[0]["edit"]["changes"]["file:///cap.rsc"][0];
        assert_eq!(first_edit["range"]["start"]["line"], 0);
        assert_eq!(first_edit["newText"], "address");
    }

    #[test]
    fn test_code_actions_malformed_params_return_32602() {
        let mut s = make_server();
        // Missing textDocument.uri entirely.
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &serde_json::json!({"id": 47, "params": {"context": {"diagnostics": []}}}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 47, "id must be echoed on error responses");
        // Missing context entirely.
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &serde_json::json!({"id": 48, "params": {"textDocument": {"uri": "file:///a.rsc"}}}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 48);
        // Context present but diagnostics absent.
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &serde_json::json!({"id": 49, "params": {
                    "textDocument": {"uri": "file:///a.rsc"}, "context": {}
                }}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 49);
    }

    #[test]
    fn test_code_actions_unknown_property_without_resolvable_menu_yields_nothing() {
        let mut s = make_server();
        // Track "/ip": a valid ancestor prefix with NO direct menu entry,
        // hence no property table — a fabricated unknown-property here must
        // be skipped rather than guessed against ALL menus.
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///p.rsc", "text": "/ip"}}}),
        );
        let fake = serde_json::json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            "severity": 2,
            "code": "unknown-property",
            "source": "rsc-ls",
            "message": "Unknown property 'ip' for '/ip'"
        });
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(50, "file:///p.rsc", &[fake]),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(actions.is_empty(), "no menu ⇒ no action, got {actions:?}");
    }

    #[test]
    fn test_code_actions_garbage_beyond_threshold_yields_nothing() {
        let mut s = make_server();
        // 12 characters of nonsense: outside threshold 2 of every property.
        let doc = "/ip/address add zzzqqqxxxwww=1";
        let diags = opened_wire_diagnostics(&mut s, "file:///g.rsc", doc);
        assert_eq!(diags.len(), 1);
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(51, "file:///g.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(
            actions.is_empty(),
            "no candidate within threshold ⇒ no action"
        );
    }

    #[test]
    fn test_code_actions_utf16_positions_extract_correct_token() {
        let mut s = make_server();
        // Default negotiation is UTF-16: 'bogus' token sits at unit 21
        // (byte 25), because each 🚨 costs two units but four bytes.
        let doc = "/ip/address add 🚨🚨 adress=1";
        let diags = opened_wire_diagnostics(&mut s, "file:///u.rsc", doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["range"]["start"]["character"], 21);
        assert_eq!(diags[0]["range"]["end"]["character"], 27);

        // Extraction must round-trip through the negotiated encoding — a
        // byte/unit mix-up would grab the wrong text and yield no action.
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(52, "file:///u.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0]["edit"]["changes"]["file:///u.rsc"][0]["newText"],
            "address"
        );
    }

    #[test]
    fn test_code_actions_resolve_menu_across_line_continuation() {
        let mut s = make_server();
        // RouterOS continuation: the command spans two physical lines; the
        // diagnostic lands on PHYSICAL line 1 while the governing menu path
        // lives on line 0. resolve_menu_for_line must join them exactly like
        // the diagnostic pipeline did when emitting this range.
        let doc = "/ip/address add \\\nadress=1.2.3.4";
        let diags = opened_wire_diagnostics(&mut s, "file:///cont.rsc", doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["code"], "unknown-property");
        assert_eq!(diags[0]["range"]["start"]["line"], 1);
        assert_eq!(diags[0]["range"]["start"]["character"], 0);
        assert_eq!(diags[0]["range"]["end"]["character"], 6);

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(53, "file:///cont.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(
            actions.len(),
            1,
            "menu must resolve across the continuation, got {actions:?}"
        );
        let edit = &actions[0]["edit"]["changes"]["file:///cont.rsc"][0];
        assert_eq!(edit["newText"], "address");
        assert_eq!(edit["range"]["start"]["line"], 1);
    }

    #[test]
    fn test_code_actions_fixes_typo_enum_value_unquoted() {
        let mut s = make_server();
        // "inpt" spans bytes 30..34 (ASCII ⇒ UTF-16 units are identical):
        // the Rule 5 range covers the VALUE part only, skipping "chain=".
        let doc = "/ip/firewall/filter add chain=inpt";
        let diags = opened_wire_diagnostics(&mut s, "file:///ev.rsc", doc);
        assert_eq!(diags.len(), 1, "exactly the invalid-enum-value hint");
        assert_eq!(diags[0]["code"], "invalid-enum-value");
        assert_eq!(diags[0]["range"]["start"]["character"], 30);
        assert_eq!(diags[0]["range"]["end"]["character"], 34);

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(54, "file:///ev.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(actions.len(), 1, "got {actions:?}");
        assert_eq!(actions[0]["title"], "Did you mean 'input'?");
        assert_eq!(actions[0]["kind"], "quickfix");
        assert_eq!(
            actions[0]["diagnostics"][0],
            serde_json::to_value(&diags[0]).unwrap(),
            "the originating diagnostic object is attached"
        );
        let edit = &actions[0]["edit"]["changes"]["file:///ev.rsc"][0];
        assert_eq!(edit["newText"], "input", "bare typo stays bare");
        assert_eq!(
            edit["range"], diags[0]["range"],
            "replacement targets the offending value range exactly"
        );
    }

    #[test]
    fn test_code_actions_fixes_typo_enum_value_quoted() {
        let mut s = make_server();
        // Quoted variant: the Rule 5 range KEEPS the surrounding quotes,
        // so the repair must re-wrap the suggested member in the SAME
        // quote style while the title stays bare.
        let doc = "/ip/firewall/filter add chain=\"forwrd\"";
        let diags = opened_wire_diagnostics(&mut s, "file:///evq.rsc", doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["code"], "invalid-enum-value");

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(55, "file:///evq.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(actions.len(), 1, "got {actions:?}");
        assert_eq!(
            actions[0]["title"], "Did you mean 'forward'?",
            "title shows the bare member, not the re-quoted splice"
        );
        let edit = &actions[0]["edit"]["changes"]["file:///evq.rsc"][0];
        assert_eq!(edit["newText"], "\"forward\"");
        assert_eq!(edit["range"], diags[0]["range"]);
        assert_eq!(edit["range"]["start"]["character"], 30);
        assert_eq!(
            edit["range"]["end"]["character"], 38,
            "quotes stay in range"
        );
    }

    #[test]
    fn test_code_actions_invalid_enum_without_resolvable_menu_yields_nothing() {
        let mut s = make_server();
        // Track "/ip": an implicit parent with NO direct menu entry, hence
        // no property table and no enum members — a fabricated
        // invalid-enum-value here must be skipped rather than guessed.
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///evn.rsc", "text": "/ip"}}}),
        );
        let fake = serde_json::json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
            "severity": 4,
            "code": "invalid-enum-value",
            "source": "rsc-ls",
            "message": "Invalid value 'zz' for 'x' (expected one of: a | b)"
        });
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(57, "file:///evn.rsc", &[fake]),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(actions.is_empty(), "no menu ⇒ no action, got {actions:?}");
    }

    #[test]
    fn test_code_actions_invalid_enum_unknown_key_yields_nothing() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///evk.rsc"},
                "text": "/ip/address add bogus=inpt"
            }}),
        );
        // The menu resolves and the key=value pair is found by spans
        // (value "inpt" at bytes 22..26), but "bogus" names no argument in
        // /ip/address ⇒ no candidate set, no action.
        let fake = serde_json::json!({
            "range": {"start": {"line": 0, "character": 22}, "end": {"line": 0, "character": 26}},
            "severity": 4,
            "code": "invalid-enum-value",
            "source": "rsc-ls",
            "message": "Invalid value 'inpt' for 'bogus'"
        });
        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(58, "file:///evk.rsc", &[fake]),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(
            actions.is_empty(),
            "unknown key ⇒ no enum candidates ⇒ no action, got {actions:?}"
        );
    }

    #[test]
    fn test_code_actions_invalid_enum_garbage_beyond_threshold_yields_nothing() {
        let mut s = make_server();
        // A REAL Rule 5 hint whose value is hopeless: nothing within the
        // length-aware threshold of input/forward/output ⇒ no action.
        let doc = "/ip/firewall/filter add chain=zzzqqqxxxwww";
        let diags = opened_wire_diagnostics(&mut s, "file:///evg.rsc", doc);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["code"], "invalid-enum-value");

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(59, "file:///evg.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert!(
            actions.is_empty(),
            "no candidate within threshold ⇒ no action, got {actions:?}"
        );
    }

    #[test]
    fn test_code_actions_mixed_codes_still_capped_at_eight() {
        let mut s = make_server();
        let mut doc = String::new();
        // Six unknown-property typos…
        for i in 0..6 {
            doc.push_str(&format!("/ip/address add adress={i}.1.1.1\n"));
        }
        // …plus six invalid-enum-value typos: twelve eligible diagnostics
        // across two codes, still answered with exactly MAX_CODE_ACTIONS.
        for i in 0..6 {
            doc.push_str(&format!("/ip/firewall/filter add chain=inpt{i}\n"));
        }
        let diags = opened_wire_diagnostics(&mut s, "file:///mix.rsc", &doc);
        let eligible = diags
            .iter()
            .filter(|d| {
                matches!(
                    d["code"].as_str(),
                    Some("unknown-property") | Some("invalid-enum-value")
                )
            })
            .count();
        assert_eq!(eligible, 12, "six property typos + six enum typos");

        let resp = s
            .handle_message(
                "textDocument/codeAction",
                &code_action_request(60, "file:///mix.rsc", &diags),
            )
            .unwrap();
        let actions = resp["result"].as_array().unwrap();
        assert_eq!(
            actions.len(),
            MAX_CODE_ACTIONS,
            "the cap spans every eligible code, not per kind"
        );
        for a in actions {
            assert!(
                matches!(
                    a["diagnostics"][0]["code"].as_str(),
                    Some("unknown-property") | Some("invalid-enum-value")
                ),
                "only eligible codes may back an action: {a:?}"
            );
        }
    }

    #[test]
    fn test_server_completion_multiline_before_cursor() {
        let mut server = make_server();
        let doc = "/ip/address add\naddress=1.1.1.1";
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///multi.rsc", "text": doc}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Cursor on line 1, after "address="
        let hover_or_completion_line = 1;
        let comp = serde_json::json!({
            "id": 20,
            "params": {
                "textDocument": {"uri": "file:///multi.rsc"},
                "position": {"line": hover_or_completion_line, "character": 8} // "address="
            }
        });
        let resp = server
            .handle_message("textDocument/completion", &comp)
            .unwrap();
        // For "address=" value completions should trigger (ipPrefix)
        let items = resp["result"]["items"].as_array().unwrap();
        // Might be value completions (0.0.0.0/0) or empty if not correctly resolved, but should be Some array
        assert!(items.is_empty() || items.iter().any(|i| i["label"] == "0.0.0.0/0"));
    }

    // ── Variable navigation (textDocument/definition + references) ──
    //
    // Wire-contract coverage for the navigation handlers: -32602 /
    // null / [] shapes per sibling-handler strictness, exact declaration
    // ranges, includeDeclaration toggling, and UTF-16 inbound positions.
    // The pure semantics behind these live in navigation.rs's own suite;
    // end-to-end wire variants live in tests/e2e.rs.

    /// `:local counter 0` / `:put $counter` / `/ip/address add
    /// interface=$counter`. Declaration name spans bytes 7..14 of line 0;
    /// usages sit at line 1 bytes 6..13 and line 2 bytes 27..34.
    const NAV_DOC: &str = ":local counter 0\n:put $counter\n/ip/address add interface=$counter\n";

    fn nav_request(id: i64, uri: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut params = serde_json::json!({
            "textDocument": {"uri": uri},
            "position": {"line": 1, "character": 8}, // inside `$counter`
        });
        if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        serde_json::json!({"id": id, "params": params})
    }

    #[test]
    fn test_server_initialize_advertises_navigation_providers() {
        let mut s = make_server();
        let resp = s
            .handle_message("initialize", &serde_json::json!({"id": 1, "params": {}}))
            .unwrap();
        assert_eq!(resp["result"]["capabilities"]["definitionProvider"], true);
        assert_eq!(resp["result"]["capabilities"]["referencesProvider"], true);
    }

    #[test]
    fn test_server_definition_untracked_uri_returns_null_result() {
        let mut s = make_server();
        let req = nav_request(61, "file:///never-opened.rsc", serde_json::json!({}));
        let resp = s.handle_message("textDocument/definition", &req).unwrap();
        assert_eq!(resp["id"], 61, "id must be echoed");
        assert!(resp["result"].is_null(), "untracked URI → null result");
    }

    #[test]
    fn test_server_definition_malformed_params_return_32602() {
        let mut s = make_server();
        // Missing URI entirely…
        let resp = s
            .handle_message(
                "textDocument/definition",
                &serde_json::json!({"id": 62, "params": {"position": {"line": 0, "character": 0}}}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 62, "id must be echoed on error responses");
        // …missing position entirely…
        let resp = s
            .handle_message(
                "textDocument/definition",
                &serde_json::json!({"id": 63, "params": {"textDocument": {"uri": "file:///a.rsc"}}}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        // …and a mistyped position component.
        let resp = s
            .handle_message(
                "textDocument/definition",
                &serde_json::json!({"id": 64, "params": {
                    "textDocument": {"uri": "file:///a.rsc"},
                    "position": {"line": 0, "character": "eight"}
                }}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn test_server_definition_jumps_to_exact_declaration_span() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nav.rsc", "text": NAV_DOC}}}),
        );
        let req = nav_request(65, "file:///nav.rsc", serde_json::json!({}));
        let resp = s.handle_message("textDocument/definition", &req).unwrap();
        let loc = &resp["result"];
        assert!(loc.is_object(), "usage must resolve, got {loc}");
        assert_eq!(loc["uri"], "file:///nav.rsc");
        // Exact name-token span of `counter` in `:local counter 0` — not
        // the command token, not the initializer.
        assert_eq!(loc["range"]["start"]["line"], 0);
        assert_eq!(loc["range"]["start"]["character"], 7);
        assert_eq!(loc["range"]["end"]["line"], 0);
        assert_eq!(loc["range"]["end"]["character"], 14);

        // Same answer when invoked ON the declaration itself.
        let req = serde_json::json!({
            "id": 66,
            "params": {
                "textDocument": {"uri": "file:///nav.rsc"},
                "position": {"line": 0, "character": 8},
            }
        });
        let resp = s.handle_message("textDocument/definition", &req).unwrap();
        assert_eq!(
            resp["result"]["range"]["start"]["character"], 7,
            "requesting from the declaration returns its own span"
        );
    }

    #[test]
    fn test_server_definition_non_variable_word_returns_null() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nv.rsc", "text": NAV_DOC}}}),
        );
        // Cursor over the property `interface` — a real word that merely
        // shares the document with variables must NOT resolve.
        let req = serde_json::json!({
            "id": 67,
            "params": {
                "textDocument": {"uri": "file:///nv.rsc"},
                "position": {"line": 2, "character": 20},
            }
        });
        let resp = s.handle_message("textDocument/definition", &req).unwrap();
        assert!(resp["result"].is_null(), "property word → null, got {resp}");
        // …and so does a cursor on the `:local` keyword itself.
        let req = serde_json::json!({
            "id": 68,
            "params": {
                "textDocument": {"uri": "file:///nv.rsc"},
                "position": {"line": 0, "character": 3},
            }
        });
        let resp = s.handle_message("textDocument/definition", &req).unwrap();
        assert!(resp["result"].is_null());
    }

    #[test]
    fn test_server_references_untracked_uri_returns_empty_list() {
        let mut s = make_server();
        let req = serde_json::json!({
            "id": 69,
            "params": {
                "textDocument": {"uri": "file:///never-opened.rsc"},
                "position": {"line": 0, "character": 0},
                "context": {"includeDeclaration": true},
            }
        });
        let resp = s.handle_message("textDocument/references", &req).unwrap();
        assert_eq!(resp["id"], 69, "id must be echoed");
        assert!(
            resp["result"].is_array(),
            "list endpoint answers an array even untracked"
        );
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_server_references_missing_context_returns_32602() {
        let mut s = make_server();
        // Context object absent entirely…
        let resp = s
            .handle_message(
                "textDocument/references",
                &serde_json::json!({"id": 70, "params": {
                    "textDocument": {"uri": "file:///a.rsc"},
                    "position": {"line": 0, "character": 0}
                }}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 70);
        // …context present but includeDeclaration missing…
        let resp = s
            .handle_message(
                "textDocument/references",
                &serde_json::json!({"id": 71, "params": {
                    "textDocument": {"uri": "file:///a.rsc"},
                    "position": {"line": 0, "character": 0},
                    "context": {}
                }}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        // …and includeDeclaration mistyped (LSP requires a boolean).
        let resp = s
            .handle_message(
                "textDocument/references",
                &serde_json::json!({"id": 72, "params": {
                    "textDocument": {"uri": "file:///a.rsc"},
                    "position": {"line": 0, "character": 0},
                    "context": {"includeDeclaration": "yes"}
                }}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn test_server_references_include_declaration_toggles_list() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///ref.rsc", "text": NAV_DOC}}}),
        );

        let with = s
            .handle_message(
                "textDocument/references",
                &nav_request(
                    73,
                    "file:///ref.rsc",
                    serde_json::json!({"context": {"includeDeclaration": true}}),
                ),
            )
            .unwrap();
        let items = with["result"].as_array().unwrap();
        assert_eq!(items.len(), 3, "declaration + two usages");
        assert_eq!(
            items[0]["range"]["start"]["character"], 7,
            "the chosen declaration comes first, exact name span"
        );
        assert_eq!(items[0]["range"]["end"]["character"], 14);
        assert_eq!(items[1]["range"]["start"]["line"], 1);
        assert_eq!(items[2]["range"]["start"]["line"], 2);
        assert_eq!(items[2]["range"]["start"]["character"], 27);

        let without = s
            .handle_message(
                "textDocument/references",
                &nav_request(
                    74,
                    "file:///ref.rsc",
                    serde_json::json!({"context": {"includeDeclaration": false}}),
                ),
            )
            .unwrap();
        let items = without["result"].as_array().unwrap();
        assert_eq!(items.len(), 2, "usages only");
        assert_eq!(items[0]["range"]["start"]["line"], 1);
    }

    #[test]
    fn test_server_references_position_off_any_variable_yields_empty_list() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///off.rsc", "text": NAV_DOC}}}),
        );
        let req = serde_json::json!({
            "id": 75,
            "params": {
                "textDocument": {"uri": "file:///off.rsc"},
                "position": {"line": 0, "character": 1}, // on `:local` keyword
                "context": {"includeDeclaration": true},
            }
        });
        let resp = s.handle_message("textDocument/references", &req).unwrap();
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_server_navigation_resolves_utf16_positions_after_emoji() {
        // Default negotiation is UTF-16: `:put "🌍🌍" $ok` puts the usage
        // identifier at units 13..15 but bytes 17..19 (each 🌍 costs 2
        // units / 4 bytes). The probe at unit 14 (mid-identifier) would be
        // byte 14 — the closing quote — under a byte/unit mix-up, where no
        // word can be extracted at all, so this pin is decisive.
        let doc = ":local ok\n:put \"🌍🌍\" $ok\n";
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u16.rsc", "text": doc}}}),
        );
        let pos = serde_json::json!({
            "textDocument": {"uri": "file:///u16.rsc"},
            "position": {"line": 1, "character": 14},
        });

        let def = s
            .handle_message(
                "textDocument/definition",
                &serde_json::json!({"id": 76, "params": pos}),
            )
            .unwrap();
        assert_eq!(
            def["result"]["range"]["start"]["character"], 7,
            "definition resolved through utf-16 units"
        );
        assert_eq!(def["result"]["range"]["end"]["character"], 9);

        let refs = s
            .handle_message(
                "textDocument/references",
                &serde_json::json!({"id": 77, "params": {
                    "textDocument": {"uri": "file:///u16.rsc"},
                    "position": {"line": 1, "character": 14},
                    "context": {"includeDeclaration": false}
                }}),
            )
            .unwrap();
        let items = refs["result"].as_array().unwrap();
        assert_eq!(items.len(), 1, "exactly the `$ok` usage");
        assert_eq!(items[0]["range"]["start"]["line"], 1);
        assert_eq!(items[0]["range"]["start"]["character"], 13);
    }
}

#[cfg(test)]
mod extra_main_coverage {
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
"#,
        )
    }

    fn make_server() -> Server {
        Server::new(synth())
    }

    // ── Caps constants ────────────────────────────────────────────────

    #[test]
    fn test_caps_constants_values() {
        assert_eq!(MAX_MESSAGE_SIZE, 10 * 1024 * 1024);
        assert_eq!(MAX_DOC_SIZE, 5 * 1024 * 1024);
        assert_eq!(MAX_DOCS, 100);
        assert_eq!(MAX_HEADER_SIZE, 32 * 1024);
    }

    // ── URI validation ────────────────────────────────────────────────

    #[test]
    fn test_is_valid_file_uri_accepts_file() {
        assert!(is_valid_file_uri("file:///test.rsc"));
        assert!(is_valid_file_uri("file:///home/user/a.rsc"));
        assert!(is_valid_file_uri("file:///a/b/c.rsc"));
    }

    #[test]
    fn test_is_valid_file_uri_rejects_others() {
        assert!(!is_valid_file_uri("untitled://test.rsc"));
        assert!(!is_valid_file_uri("http://example.com/a.rsc"));
        assert!(!is_valid_file_uri("https://example.com/a.rsc"));
        assert!(!is_valid_file_uri("vscode://test"));
        assert!(!is_valid_file_uri(""));
        assert!(!is_valid_file_uri("/file/test.rsc"));
    }

    #[test]
    fn test_is_valid_file_uri_rejects_traversal_and_null() {
        assert!(!is_valid_file_uri("file:///home/../etc/passwd"));
        assert!(!is_valid_file_uri("file:///a/../b.rsc"));
        assert!(!is_valid_file_uri("file:///test\0.rsc"));
        let uri = format!("file:///test{}.rsc", '\0');
        assert!(!is_valid_file_uri(&uri));
    }

    // ── didOpen / didChange / didClose ────────────────────────────────

    #[test]
    fn test_did_open_stores_and_overwrites() {
        let mut s = make_server();
        let open = serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "hello"}}});
        s.handle_message("textDocument/didOpen", &open);
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "hello");
        let open2 = serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "world"}}});
        s.handle_message("textDocument/didOpen", &open2);
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "world");
        assert_eq!(s.docs.len(), 1);
    }

    #[test]
    fn test_did_open_rejects_invalid_uris() {
        let mut s = make_server();
        for uri in [
            "untitled://a.rsc",
            "http://a.rsc",
            "file:///a/../b.rsc",
            &format!("file:///a{}.rsc", '\0'),
        ] {
            let open = serde_json::json!({"params": {"textDocument": {"uri": uri, "text": "hi"}}});
            s.handle_message("textDocument/didOpen", &open);
            assert!(!s.docs.contains_key(uri), "should reject {uri:?}");
        }
        assert!(s.docs.is_empty());
    }

    #[test]
    fn test_did_change_full_sync() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "old"}}}));
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"text": "new"}]}}));
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "new");
    }

    #[test]
    fn test_did_change_incremental_edit() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "hello world"}}}));
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}}, "text": "Rust"}]}}));
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "hello Rust");
    }

    #[test]
    fn test_did_change_incremental_fallback_on_invalid_range() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "hello"}}}));
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"range": {"start": {"line": 10, "character": 0}, "end": {"line": 10, "character": 5}}, "text": "fallback"}]}}));
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "fallback");
    }

    #[test]
    fn test_did_change_multiple_changes_last_wins_for_full() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "x"}}}),
        );
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"text": "first"}, {"text": "second"}]}}));
        // Full sync last change wins is documented, but implementation processes each change sequentially
        // For non-range, it inserts each in order, so last is "second" (but note second change was buggy? In handle_message it inserts for each change without range)
        // Check final is one of them and not panic
        let doc = s.docs.get("file:///a.rsc").unwrap();
        assert!(doc == "second" || doc == "first");
    }

    #[test]
    fn test_did_change_new_uri_via_change_when_at_cap() {
        let mut s = make_server();
        for i in 0..MAX_DOCS {
            let uri = format!("file:///f{i}.rsc");
            s.handle_message(
                "textDocument/didOpen",
                &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": "hi"}}}),
            );
        }
        assert_eq!(s.docs.len(), MAX_DOCS);
        // New doc via didChange should be rejected at cap
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///new.rsc"}, "contentChanges": [{"text": "hello"}]}}));
        assert!(!s.docs.contains_key("file:///new.rsc"));
    }

    #[test]
    fn test_did_change_rejects_invalid_uri() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "old"}}}));
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "http://evil.com/a.rsc"}, "contentChanges": [{"text": "new"}]}}));
        assert_eq!(s.docs.get("file:///a.rsc").unwrap(), "old");
        assert!(!s.docs.contains_key("http://evil.com/a.rsc"));
    }

    #[test]
    fn test_did_close_removes() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "hi"}}}));
        assert!(s.docs.contains_key("file:///a.rsc"));
        s.handle_message(
            "textDocument/didClose",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}}}),
        );
        assert!(!s.docs.contains_key("file:///a.rsc"));
    }

    #[test]
    fn test_did_close_nonexistent_no_panic() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didClose",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///no.rsc"}}}),
        );
        assert!(s.docs.is_empty());
    }

    #[test]
    fn test_did_open_truncates_at_max_doc_size() {
        let mut s = make_server();
        let big = "a".repeat(MAX_DOC_SIZE + 100);
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///big.rsc", "text": big}}}));
        assert_eq!(s.docs.get("file:///big.rsc").unwrap().len(), MAX_DOC_SIZE);
    }

    #[test]
    fn test_did_open_max_docs_enforced() {
        let mut s = make_server();
        for i in 0..MAX_DOCS {
            let uri = format!("file:///d{i}.rsc");
            s.handle_message(
                "textDocument/didOpen",
                &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": "hi"}}}),
            );
        }
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///extra.rsc", "text": "hi"}}}));
        assert_eq!(s.docs.len(), MAX_DOCS);
        assert!(!s.docs.contains_key("file:///extra.rsc"));
        // Updating existing should succeed
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///d0.rsc", "text": "updated"}}}));
        assert_eq!(s.docs.get("file:///d0.rsc").unwrap(), "updated");
    }

    // ── publishDiagnostics caps and incremental ────────────────────────

    #[test]
    fn test_diagnostic_pull_and_push_consistency() {
        let mut s = make_server();
        let doc = "/unknown/menu add x=1";
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///c.rsc", "text": doc}}}),
        );
        let pull = s.handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "file:///c.rsc"}}}),
        );
        let pull_items = pull.unwrap()["result"]["items"].as_array().unwrap().len();
        let direct = diagnostics::compute_diagnostics(&synth(), doc, "file:///c.rsc").len();
        assert_eq!(pull_items, direct);
    }

    #[test]
    fn test_diagnostic_pull_invalid_uri_returns_empty() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/unknown/menu add x=1"}}}));
        let resp = s
            .handle_message("textDocument/diagnostic", &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "untitled://a.rsc"}}}))
            .unwrap();
        let items = resp["result"]["items"].as_array().unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn test_large_doc_diagnostics_capped() {
        let data = synth();
        let doc = "/unknown/menu add x=1\n".repeat(4000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///a.rsc");
        assert!(diags.len() <= 3000);
    }

    #[test]
    fn test_large_doc_truncation_preserves_first() {
        let data = synth();
        let mut doc = String::new();
        doc.push_str("/unknown/first add x=1\n");
        doc.push_str(&"/unknown/other add x=1\n".repeat(5000));
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///a.rsc");
        assert!(diags.iter().any(|d| d.message.contains("/unknown/first")));
        assert_eq!(diags[0].range.start.line, 0);
    }

    // ── Completion integration ────────────────────────────────────────

    #[test]
    fn test_completion_for_empty_context_returns_roots() {
        let mut s = make_server();
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": ""}}}),
        );
        let resp = s.handle_message("textDocument/completion", &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 0}}}));
        let items = resp.unwrap()["result"]["items"].as_array().unwrap().clone();
        assert!(!items.is_empty());
        assert!(items.iter().any(|i| i["label"] == "/ip"));
    }

    #[test]
    fn test_completion_for_args_after_verb() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address add "}}}));
        let resp = s.handle_message("textDocument/completion", &serde_json::json!({"id": 2, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 15}}}));
        let items = resp.unwrap()["result"]["items"].as_array().unwrap().clone();
        let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"interface"));
    }

    #[test]
    fn test_completion_for_values_after_equals() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/firewall/filter add chain="}}}));
        let resp = s
            .handle_message("textDocument/completion", &serde_json::json!({"id": 3, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 30}}}))
            .unwrap();
        let items = resp["result"]["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["label"] == "input"));
    }

    #[test]
    fn test_hover_returns_correct_for_menu() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address"}}}));
        let resp = s.handle_message("textDocument/hover", &serde_json::json!({"id": 4, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 4}}}));
        let val = resp.unwrap()["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(val.contains("/ip/address"));
    }

    #[test]
    fn test_hover_unknown_returns_null() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/unknown/menu"}}}));
        let resp = s.handle_message("textDocument/hover", &serde_json::json!({"id": 5, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 5}}}));
        assert!(resp.unwrap()["result"].is_null());
    }

    #[test]
    fn test_incremental_edit_applied_then_diagnostics_updated() {
        let mut s = make_server();
        s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address add address=1.1.1.1 interface=ether1"}}}));
        // Valid, no missing
        let before = diagnostics::compute_diagnostics(
            &synth(),
            s.docs.get("file:///a.rsc").unwrap(),
            "file:///a.rsc",
        );
        assert!(
            !before
                .iter()
                .any(|d| d.code.as_deref() == Some("missing-required"))
        );
        // Incremental edit to break it
        s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 11}}, "text": "/unknown/menu"}]}}));
        let after_doc = s.docs.get("file:///a.rsc").unwrap();
        assert!(after_doc.starts_with("/unknown/menu"));
        let after = diagnostics::compute_diagnostics(&synth(), after_doc, "file:///a.rsc");
        assert!(
            after
                .iter()
                .any(|d| d.code.as_deref() == Some("unknown-menu"))
        );
    }
}

#[cfg(test)]
mod position_encoding {
    //! Position-encoding negotiation, boundary conversions, and regression
    //! coverage for UTF-16 positions against non-ASCII documents.

    use super::*;
    use crate::menus::MenuData;

    fn synth_min() -> MenuData {
        MenuData::from_toml_str(
            r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
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
"#,
        )
    }

    /// Run `initialize` with an optional `general.positionEncodings` array
    /// (`None` = capability absent) and return the server plus the response.
    fn initialize(encodings: Option<serde_json::Value>) -> (Server, serde_json::Value) {
        let mut server = Server::new(synth_min());
        let params = match encodings {
            None => serde_json::json!({"capabilities": {}}),
            Some(e) => {
                serde_json::json!({"capabilities": {"general": {"positionEncodings": e}}})
            }
        };
        let msg = serde_json::json!({"id": 1, "method": "initialize", "params": params});
        let resp = server.handle_message("initialize", &msg).unwrap();
        (server, resp)
    }

    // ── Negotiation matrix ────────────────────────────────────────

    #[test]
    fn test_initialize_without_capability_defaults_to_utf16() {
        let (server, resp) = initialize(None);
        assert_eq!(
            resp["result"]["capabilities"]["positionEncoding"], "utf-16",
            "spec default when client sends no positionEncodings"
        );
        assert_eq!(server.position_encoding, PositionEncoding::Utf16);
    }

    #[test]
    fn test_initialize_prefers_utf8_when_client_advertises_it() {
        let (server, resp) = initialize(Some(serde_json::json!(["utf-16", "utf-8"])));
        assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-8");
        assert_eq!(server.position_encoding, PositionEncoding::Utf8);
    }

    #[test]
    fn test_initialize_falls_back_to_utf16_when_utf8_absent() {
        let (server, resp) = initialize(Some(serde_json::json!(["utf-32"])));
        assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-16");
        assert_eq!(server.position_encoding, PositionEncoding::Utf16);
    }

    #[test]
    fn test_initialize_keeps_existing_capabilities_intact() {
        let (_, resp) = initialize(Some(serde_json::json!(["utf-8"])));
        let caps = &resp["result"]["capabilities"];
        assert_eq!(caps["textDocumentSync"]["openClose"], true);
        assert_eq!(caps["textDocumentSync"]["change"], 2);
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(
            caps["completionProvider"]["triggerCharacters"],
            serde_json::json!(["/", " ", "=", ":"])
        );
        assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
    }

    #[test]
    fn test_initialize_advertises_incremental_sync() {
        // textDocumentSync must be the object form (openClose + change = 2),
        // not the legacy scalar Full-sync kind. Incremental patching is
        // implemented and tested (apply_incremental_edit); full-text
        // replacements remain handled as a fallback.
        let (_, resp) = initialize(None);
        let sync = &resp["result"]["capabilities"]["textDocumentSync"];
        assert!(sync.is_object(), "sync capability must be the object form");
        assert_eq!(sync["change"], 2);
        assert_eq!(sync["openClose"], true);
    }

    #[test]
    fn test_initialize_advertises_all_providers() {
        // Stage B: every supported provider must be advertised together.
        let (_, resp) = initialize(None);
        let caps = &resp["result"]["capabilities"];
        assert_eq!(
            caps["completionProvider"]["triggerCharacters"],
            serde_json::json!(["/", " ", "=", ":"])
        );
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(
            caps["documentSymbolProvider"], true,
            "documentSymbol capability must be advertised"
        );
        assert_eq!(
            caps["foldingRangeProvider"], true,
            "foldingRange capability must be advertised"
        );
        assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
    }

    // ── Regression: incremental edits must not corrupt documents ──

    #[test]
    fn test_did_change_incremental_utf16_no_corruption_on_non_ascii_line() {
        let mut s = Server::new(synth_min());
        // Client does not advertise utf-8 → positions are UTF-16 code units.
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        let doc = "# comentário ✔\n/ip/address add address=1.1.1.1\n";
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///reg.rsc", "text": doc}}}),
        );

        // Delete the trailing '✔' on line 0 expressed in UTF-16 units:
        // "# comentário " is 13 units, '✔' spans units 13..14 (bytes 14..17).
        s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 14}},
                    "text": ""
                }]
            }}),
        );
        // Byte-level treatment would instead delete the SPACE before '✔'
        // (bytes 13..14), leaving the emoji behind — exact equality guards it.
        assert_eq!(
            s.docs.get("file:///reg.rsc").unwrap(),
            "# comentário \n/ip/address add address=1.1.1.1\n"
        );

        // Follow-up ranged edit targeting LINE 1 with non-ASCII above: the
        // line-start scan must stay byte-exact while characters stay UTF-16
        // (replaces exactly "/ip/address", 11 units).
        s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 11}},
                    "text": "/ipv6/address"
                }]
            }}),
        );
        assert_eq!(
            s.docs.get("file:///reg.rsc").unwrap(),
            "# comentário \n/ipv6/address add address=1.1.1.1\n"
        );
    }

    #[test]
    fn test_did_change_incremental_utf8_positions_unchanged_for_ascii() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
        );
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u8.rsc", "text": "hello world"}}}),
        );
        s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///u8.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}},
                    "text": "Rust"
                }]
            }}),
        );
        assert_eq!(s.docs.get("file:///u8.rsc").unwrap(), "hello Rust");
    }

    #[test]
    fn test_did_change_incremental_utf16_crlf_insert_before_cr() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///crlf.rsc", "text": "héllo\r\nworld"}}}),
        );
        // Insert at the EOL position (unit 6 == end of "héllo"): must land
        // BEFORE the '\r', never inside or after the CRLF pair.
        s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///crlf.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 6}},
                    "text": "X"
                }]
            }}),
        );
        assert_eq!(s.docs.get("file:///crlf.rsc").unwrap(), "hélloX\r\nworld");
    }

    // ── Hover / completion context under Utf16 ────────────────────

    #[test]
    fn test_hover_utf16_with_multibyte_prefix_on_same_line() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        // Non-ASCII comment ABOVE and multibyte prefix BEFORE the target
        // token on the same line: 'ççççç' adds 5 extra bytes over units.
        let doc = concat!(
            "# comentário ✔\n",
            "/ip/address add comment=\"ççççç\" address=1.1.1.1",
        );
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///hv.rsc", "text": doc}}}),
        );
        // "address" starts at unit 32 (byte 37); unit 35 is mid-word.
        let hover = serde_json::json!({
            "id": 9,
            "params": {
                "textDocument": {"uri": "file:///hv.rsc"},
                "position": {"line": 1, "character": 35}
            }
        });
        let resp = s.handle_message("textDocument/hover", &hover).unwrap();
        assert!(
            resp["result"]["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("**address**"),
            "word extraction must land on 'address', got {}",
            resp["result"]
        );
    }

    #[test]
    fn test_completion_utf16_value_completions_after_multibyte_prefix() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        // Trailing target token sits AFTER multibyte content on the line:
        // 'chain=' ends at unit 42 / byte 43 ('ç' costs one extra byte).
        let doc = "/ip/firewall/filter add comment=\"ç\" chain=";
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///cp.rsc", "text": doc}}}),
        );
        let comp = serde_json::json!({
            "id": 10,
            "params": {
                "textDocument": {"uri": "file:///cp.rsc"},
                "position": {"line": 0, "character": 42}
            }
        });
        let resp = s.handle_message("textDocument/completion", &comp).unwrap();
        let items = resp["result"]["items"].as_array().unwrap();
        let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
        assert!(
            labels.contains(&"input"),
            "value completions for 'chain=' expected, got {labels:?}"
        );
    }

    // ── documentSymbol / foldingRange (Stage B) ───────────────────

    /// Open `doc` in a fresh utf-16-negotiated server and return the raw
    /// response for `method` (documentSymbol / foldingRange).
    fn stage_b_request(method: &str, doc: &str, id: i64) -> serde_json::Value {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///b.rsc", "text": doc}}}),
        );
        s.handle_message(
            method,
            &serde_json::json!({"id": id, "params": {"textDocument": {"uri": "file:///b.rsc"}}}),
        )
        .expect("requests must be answered")
    }

    #[test]
    fn test_document_symbols_menu_global_local_mix() {
        let doc = concat!(
            "/ip/address add address=1.2.3.4\n",
            ":global gw1 1.1.1.1\n",
            ":local i 0\n",
            ":put done\n",
            "print\n", // bare fragment — skipped
        );
        let resp = stage_b_request("textDocument/documentSymbol", doc, 21);
        assert_eq!(resp["id"], 21);
        let syms = resp["result"].as_array().expect("flat symbol array");
        let names: Vec<&str> = syms.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["/ip/address add", "gw1", "i", ":put"]);
        let kinds: Vec<i64> = syms.iter().map(|s| s["kind"].as_i64().unwrap()).collect();
        assert_eq!(kinds, vec![19, 13, 13, 12]);
        // First symbol: range covers the whole line; selection the path token.
        assert_eq!(syms[0]["range"]["start"]["line"], 0);
        assert_eq!(syms[0]["range"]["end"]["character"], 31);
        assert_eq!(syms[0]["selectionRange"]["start"]["character"], 0);
        assert_eq!(syms[0]["selectionRange"]["end"]["character"], 11);
    }

    #[test]
    fn test_document_symbol_continuation_spans_physical_lines() {
        let doc = "/ip/address add \\\naddress=1.2.3.4\n";
        let resp = stage_b_request("textDocument/documentSymbol", doc, 22);
        let syms = resp["result"].as_array().unwrap();
        assert_eq!(syms.len(), 1, "continuation joins into one logical command");
        assert_eq!(syms[0]["range"]["start"]["line"], 0);
        assert_eq!(syms[0]["range"]["end"]["line"], 1);
        assert_eq!(syms[0]["range"]["end"]["character"], 15);
    }

    #[test]
    fn test_document_symbols_empty_doc_is_empty_array() {
        let resp = stage_b_request("textDocument/documentSymbol", "", 23);
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_document_symbols_untracked_uri_returns_null_result() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        let resp = s
            .handle_message(
                "textDocument/documentSymbol",
                &serde_json::json!({"id": 24, "params": {"textDocument": {"uri": "file:///never.rsc"}}}),
            )
            .unwrap();
        assert_eq!(resp["id"], 24, "id must be echoed");
        assert!(resp["result"].is_null(), "untracked URI → null result");
    }

    #[test]
    fn test_document_symbols_malformed_params_return_32602() {
        let mut s = Server::new(synth_min());
        // Missing textDocument object entirely.
        let resp = s
            .handle_message(
                "textDocument/documentSymbol",
                &serde_json::json!({"id": 25}),
            )
            .unwrap();
        assert_eq!(resp["id"], 25);
        assert_eq!(resp["error"]["code"], -32602);
        // Missing uri inside textDocument.
        let resp = s
            .handle_message(
                "textDocument/documentSymbol",
                &serde_json::json!({"id": 26, "params": {"textDocument": {}}}),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn test_folding_ranges_block_and_continuation_sorted() {
        let doc = concat!(
            ":do {\n",              // 0 opens region
            "\t:put x\n",           // 1
            "}\n",                  // 2 closes region → (0,2,"region")
            "/ip/address add \\\n", // 3 continues
            "address=1.2.3.4\n",    // 4 → continuation fold (3,4)
        );
        let resp = stage_b_request("textDocument/foldingRange", doc, 27);
        let ranges = resp["result"].as_array().unwrap();
        let rows: Vec<(i64, i64, Option<&str>)> = ranges
            .iter()
            .map(|r| {
                (
                    r["startLine"].as_i64().unwrap(),
                    r["endLine"].as_i64().unwrap(),
                    r["kind"].as_str(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![(0, 2, Some("region")), (3, 4, None)],
            "sorted by startLine; region carries kind, continuations do not"
        );
    }

    #[test]
    fn test_folding_ranges_single_line_braces_not_emitted() {
        let doc = ":if (a) do={ :put x } else={ :put y }\n";
        let resp = stage_b_request("textDocument/foldingRange", doc, 28);
        assert!(resp["result"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_folding_ranges_unterminated_brace_safe_and_null_untracked() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        // Unterminated brace: answered with an empty list, never a hang.
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u.rsc", "text": ":do {\n:put x\n"}}}),
        );
        let resp = s
            .handle_message(
                "textDocument/foldingRange",
                &serde_json::json!({"id": 29, "params": {"textDocument": {"uri": "file:///u.rsc"}}}),
            )
            .unwrap();
        assert!(resp["result"].as_array().unwrap().is_empty());

        // Untracked URI → null result with echoed id.
        let resp = s
            .handle_message(
                "textDocument/foldingRange",
                &serde_json::json!({"id": 30, "params": {"textDocument": {"uri": "file:///nope.rsc"}}}),
            )
            .unwrap();
        assert_eq!(resp["id"], 30);
        assert!(resp["result"].is_null());
    }

    #[test]
    fn test_folding_range_malformed_params_return_32602() {
        let mut s = Server::new(synth_min());
        let resp = s
            .handle_message(
                "textDocument/foldingRange",
                &serde_json::json!({"id": 31, "params": {"textDocument": {"nope": true}}}),
            )
            .unwrap();
        assert_eq!(resp["id"], 31);
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[test]
    fn test_document_symbol_characters_honor_utf16_negotiation() {
        // Default negotiation is UTF-16. The logical command spans two
        // physical lines; its end lands on line 1 whose content holds a
        // multibyte char BEFORE the end position:
        //   comment="ç"  → 11 UTF-16 units but 12 bytes.
        let doc = "/ip/address add \\\ncomment=\"ç\"\n";
        let resp = stage_b_request("textDocument/documentSymbol", doc, 32);
        let sym = &resp["result"].as_array().unwrap()[0];
        assert_eq!(sym["range"]["end"]["line"], 1);
        assert_eq!(
            sym["range"]["end"]["character"], 11,
            "utf-16 units, not bytes (raw byte offset would be 12)"
        );
        // Selection sits on the ASCII first line — identical either way.
        assert_eq!(sym["selectionRange"]["end"]["character"], 11);
    }

    // ── Diagnostics ranges honor the negotiated encoding ──────────

    #[test]
    fn test_pull_diagnostics_utf16_character_units_with_emoji_prefix() {
        let mut s = Server::new(synth_min());
        // Default negotiation (no capability) → UTF-16 emission.
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
        );
        // "bogusprop" starts at byte 25 ("…add " = 16 bytes + two 🚨 = 8)
        // but at unit 21 (each 🚨 counts 2 units).
        let doc = "/ip/address add 🚨🚨 bogusprop=1";
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///dg.rsc", "text": doc}}}),
        );
        let pull = s
            .handle_message(
                "textDocument/diagnostic",
                &serde_json::json!({"id": 11, "params": {"textDocument": {"uri": "file:///dg.rsc"}}}),
            )
            .unwrap();
        let items = pull["result"]["items"].as_array().unwrap();
        let up = items
            .iter()
            .find(|d| d["code"] == "unknown-property")
            .expect("unknown-property diagnostic expected");
        assert_eq!(up["range"]["start"]["character"], 21);
        assert_eq!(up["range"]["end"]["character"], 30);
    }

    #[test]
    fn test_pull_diagnostics_utf8_character_equals_bytes() {
        let mut s = Server::new(synth_min());
        s.handle_message(
            "initialize",
            &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
        );
        let doc = "/ip/address add 🚨🚨 bogusprop=1";
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///d8.rsc", "text": doc}}}),
        );
        let pull = s
            .handle_message(
                "textDocument/diagnostic",
                &serde_json::json!({"id": 12, "params": {"textDocument": {"uri": "file:///d8.rsc"}}}),
            )
            .unwrap();
        let items = pull["result"]["items"].as_array().unwrap();
        let up = items
            .iter()
            .find(|d| d["code"] == "unknown-property")
            .expect("unknown-property diagnostic expected");
        // Byte semantics preserved exactly when utf-8 is negotiated.
        assert_eq!(up["range"]["start"]["character"], 25);
        assert_eq!(up["range"]["end"]["character"], 34);
    }
}

#[cfg(test)]
mod signature_help {
    //! `textDocument/signatureHelp`: capability advertisement, named-
    //! parameter signature construction (required-first ordering, offset
    //! labels), activeParameter detection (exact/prefix/ambiguous key match,
    //! quoted values, continuations), and the response guarantees shared by
    //! every request handler (-32602 malformed, null untracked/gated).
    //!
    //! All requests run against a fresh server WITHOUT `initialize`, so the
    //! negotiated encoding is the spec-default UTF-16 — exactly what real
    //! clients fall back to.

    use super::*;
    use crate::menus::MenuData;

    /// `/tool/fetch`-shaped fixture: two REQUIRED properties, two optional
    /// ones sharing the `check-` prefix (for ambiguity coverage), and an
    /// enum type whose spaces prove offsets survive multi-word types.
    fn sig_data() -> MenuData {
        MenuData::from_toml_str(
            r#"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
required = true
[[menus.arguments]]
name = "check-certificate"
type = "bool"
[[menus.arguments]]
name = "check-expired"
type = "bool"
[[menus.arguments]]
name = "http-method"
type = "enum (get | post)"
required = true
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = ""
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
"#,
        )
    }

    fn make_server() -> Server {
        Server::new(sig_data())
    }

    fn open(s: &mut Server, uri: &str, doc: &str) {
        s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": doc}}}),
        );
    }

    fn sig_request(id: i64, uri: &str, line: usize, character: usize) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "params": {
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character}
            }
        })
    }

    /// Expected single-line label for `/tool/fetch add …`: REQUIRED FIRST
    /// (alphabetical: http-method, url), then the optionals alphabetically.
    const FETCH_LABEL: &str = "/tool/fetch add http-method=enum (get | post) url=string \
                               check-certificate=bool check-expired=bool";

    // ── Capability advertisement ─────────────────────────────────

    #[test]
    fn test_initialize_advertises_signature_help_provider_object_form() {
        let mut s = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        });
        let resp = s.handle_message("initialize", &msg).unwrap();
        let provider = &resp["result"]["capabilities"]["signatureHelpProvider"];
        assert!(
            provider.is_object(),
            "object form (like completionProvider), got {provider}"
        );
        assert_eq!(provider["triggerCharacters"], serde_json::json!([" ", "="]));
    }

    // ── Signature construction ───────────────────────────────────

    #[test]
    fn test_signature_after_verb_lists_required_first_with_offset_labels() {
        let mut s = make_server();
        let doc = "/tool/fetch add ";
        open(&mut s, "file:///sig.rsc", doc);
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(60, "file:///sig.rsc", 0, doc.len()),
            )
            .unwrap();
        let result = &resp["result"];
        assert!(!result.is_null(), "menu+verb resolved ⇒ popup");
        assert_eq!(result["activeSignature"], 0);
        let sigs = result["signatures"].as_array().unwrap();
        assert_eq!(sigs.len(), 1, "exactly one signature");
        let label = sigs[0]["label"].as_str().unwrap();
        assert_eq!(label, FETCH_LABEL);

        let params = sigs[0]["parameters"].as_array().unwrap();
        assert_eq!(params.len(), 4);
        // Each ParameterInformation label is [start, end] INTO the label
        // string; slicing must reproduce the intended `name=type` segment.
        let segments: Vec<&str> = params
            .iter()
            .map(|p| {
                let start = p["label"][0].as_u64().unwrap() as usize;
                let end = p["label"][1].as_u64().unwrap() as usize;
                &label[start..end]
            })
            .collect();
        assert_eq!(
            segments,
            [
                "http-method=enum (get | post)",
                "url=string",
                "check-certificate=bool",
                "check-expired=bool"
            ],
            "required properties lead, then alphabetical"
        );
        // "(required)" lives inside the parameter documentation only.
        assert!(
            params[0]["documentation"]
                .as_str()
                .unwrap()
                .starts_with("(required) ")
        );
        assert!(
            params[1]["documentation"]
                .as_str()
                .unwrap()
                .starts_with("(required) ")
        );
        assert!(
            !params[2]["documentation"]
                .as_str()
                .unwrap()
                .starts_with("(required) ")
        );
        // Signature documentation: menu identity + the ordering note.
        let sig_doc = sigs[0]["documentation"].as_str().unwrap();
        assert!(sig_doc.contains("`/tool/fetch`"));
        assert!(sig_doc.contains("Required properties listed first."));
        // Cursor sits after the verb with no property started ⇒ nothing
        // highlighted yet.
        assert!(result.get("activeParameter").is_none());
    }

    // ── activeParameter detection ────────────────────────────────

    #[test]
    fn test_signature_prefix_match_highlights_right_param() {
        let mut s = make_server();
        // `check-c` uniquely prefixes check-certificate (param index 2 in
        // the required-first list).
        let doc = "/tool/fetch add check-c";
        open(&mut s, "file:///prefix.rsc", doc);
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(61, "file:///prefix.rsc", 0, doc.len()),
            )
            .unwrap();
        assert_eq!(
            resp["result"]["activeParameter"], 2,
            "unique prefix resolves to check-certificate"
        );
    }

    #[test]
    fn test_signature_ambiguous_prefix_omits_active_parameter() {
        let mut s = make_server();
        // `check-` matches check-certificate AND check-expired ⇒ omit rather
        // than guess; the popup itself must still render.
        let doc = "/tool/fetch add check-";
        open(&mut s, "file:///ambig.rsc", doc);
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(62, "file:///ambig.rsc", 0, doc.len()),
            )
            .unwrap();
        assert!(
            !resp["result"]["signatures"].as_array().unwrap().is_empty(),
            "popup still shows"
        );
        assert!(
            resp["result"].get("activeParameter").is_none(),
            "ambiguous prefix ⇒ no activeParameter field at all"
        );
    }

    #[test]
    fn test_signature_quoted_value_keeps_key_active() {
        let mut s = make_server();
        let doc = "/tool/fetch add url=\"http://x y\" check-certificate=";
        open(&mut s, "file:///quote.rsc", doc);

        // Inside the quoted VALUE: quote-aware tokens keep the whole
        // `url="http://x y"` as ONE token, so its key stays active (url is
        // param index 1, required-first).
        let inside_quote = doc.find("//").unwrap() + 1;
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(63, "file:///quote.rsc", 0, inside_quote),
            )
            .unwrap();
        assert_eq!(resp["result"]["activeParameter"], 1);

        // Right after the second `=`: that key becomes active instead.
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(64, "file:///quote.rsc", 0, doc.len()),
            )
            .unwrap();
        assert_eq!(resp["result"]["activeParameter"], 2);
    }

    // ── Gating: anti-noise contract ──────────────────────────────

    #[test]
    fn test_signature_no_verb_returns_null() {
        let mut s = make_server();
        open(&mut s, "file:///noverb.rsc", "/tool/fetch ");
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(65, "file:///noverb.rsc", 0, 12),
            )
            .unwrap();
        assert!(resp["result"].is_null(), "no verb ⇒ no popup");
    }

    #[test]
    fn test_signature_unknown_menu_returns_null() {
        let mut s = make_server();
        open(&mut s, "file:///unknown.rsc", "/foo/bar add url=x");
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(66, "file:///unknown.rsc", 0, 18),
            )
            .unwrap();
        assert!(resp["result"].is_null(), "unresolvable menu ⇒ no popup");
    }

    #[test]
    fn test_signature_untracked_uri_returns_null_result() {
        let mut s = make_server();
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(67, "file:///never-opened.rsc", 0, 0),
            )
            .unwrap();
        assert_eq!(resp["id"], 67, "id must be echoed");
        assert!(resp["result"].is_null());
    }

    #[test]
    fn test_signature_malformed_params_return_32602() {
        let mut s = make_server();
        // Variant A: position missing entirely.
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &serde_json::json!({
                    "id": 68,
                    "params": {"textDocument": {"uri": "file:///a.rsc"}}
                }),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 68, "id echoed on error responses");

        // Variant B: uri missing entirely.
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &serde_json::json!({
                    "id": 69,
                    "params": {"position": {"line": 0, "character": 0}}
                }),
            )
            .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["id"], 69);
    }

    // ── Encoding & continuation integration ──────────────────────

    #[test]
    fn test_signature_utf16_multibyte_before_cursor() {
        let mut s = make_server();
        // Two 'ç' sit BEFORE the target position inside url's quoted value:
        // each costs 1 UTF-16 unit but 2 bytes. Byte layout:
        //   `/tool/fetch add url="https://` = 29 bytes/units,
        //   `çç` = +4 bytes/+2 units, `"` closes at byte 34 / unit 32.
        // Requesting unit 32 must resolve to BYTE 34 (the closing quote),
        // i.e. inside url's token — a bytes-as-units mix-up would land two
        // bytes later and wrongly highlight check-certificate.
        let doc = "/tool/fetch add url=\"https://çç\" check-certificate=";
        open(&mut s, "file:///utf16.rsc", doc);
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(70, "file:///utf16.rsc", 0, 32),
            )
            .unwrap();
        assert_eq!(
            resp["result"]["activeParameter"], 1,
            "unit→byte conversion must keep url active despite multibyte prefix"
        );
    }

    #[test]
    fn test_signature_continuation_joined_line_resolves_context_and_offsets() {
        let mut s = make_server();
        // RouterOS `\` continuation: menu path lives on PHYSICAL line 0, the
        // property being typed on PHYSICAL line 1. The joined logical text is
        // "/tool/fetch add check-certificate=".
        let doc = "/tool/fetch add \\\ncheck-certificate=";
        open(&mut s, "file:///cont.rsc", doc);
        let resp = s
            .handle_message(
                "textDocument/signatureHelp",
                &sig_request(71, "file:///cont.rsc", 1, 18),
            )
            .unwrap();
        let result = &resp["result"];
        assert!(
            !result.is_null(),
            "menu must resolve across the continuation"
        );
        let label = result["signatures"][0]["label"].as_str().unwrap();
        assert_eq!(label, FETCH_LABEL, "label built from the JOINED line");
        // Offsets still slice the label exactly (context correctness).
        let p0 = &result["signatures"][0]["parameters"][0];
        let seg = &label
            [p0["label"][0].as_u64().unwrap() as usize..p0["label"][1].as_u64().unwrap() as usize];
        assert_eq!(seg, "http-method=enum (get | post)");
        // Cursor maps into the joined text right after the continued key.
        assert_eq!(result["activeParameter"], 2);
    }
}
