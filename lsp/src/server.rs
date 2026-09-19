// ── LSP server core (protocol boundary) ──────────────────────────────────
//
// Owns the wire-facing half of rsc-ls: the `Server` state machine
// (stdio read/write loop, `handle_message` method dispatch, tracked-
// document store, publish/diagnostics/code-action methods).
//
// Split of responsibilities:
// - Feature logic (completion, hover, diagnostics, symbols, folding,
//   signature help, suggestions, navigation math) lives in dedicated
//   sibling modules behind pure functions; this file only marshals
//   JSON-RPC params/results around those calls.
// - Protocol helpers (quick-fix payload, URI guard, error constructors,
//   navigation adapters, live-identity predicate) live in `server_proto`
//   and are re-exported below.
// - Shared resource caps are declared once in `caps.rs` and reach this
//   module through `crate::` paths (re-exported at the crate root).
// - Everything here is `pub(crate)` and re-exported from the crate root
//   (`main.rs`) so the root test modules and these unit tests exercise
//   real wire-visible behavior without reaching into private items.

use crate::caps::{MAX_CHANGES_PER_NOTIFICATION, MAX_DOC_SIZE, MAX_DOCS};
use crate::diagnostics;
use crate::encoding::{
    PositionEncoding, apply_incremental_edit, floor_char_boundary, lsp_character_to_byte_offset,
    strip_bom_prefix,
};
use crate::folding;
use crate::framing::{Frame, FrameError, read_message};
use crate::hover;
use crate::live::{LiveCache, LiveConfig, get_cached_or_fetch_background};
use crate::logging::{
    log_debug, log_error, log_info, log_warn, sanitize_for_log, truncate_command_for_log,
    uri_for_log,
};
use crate::menus::MenuData;
use crate::parser::{ParseCache, parse_line, tokenize_with_spans};
use crate::rename;
use crate::signature;
use crate::symbols;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// Protocol helpers (quick-fix payload, URI guard, error constructors,
// navigation adapters, live-identity predicate) live in `server_proto`.
// Re-exported here so existing `crate::server::…` paths (tests,
// `rename.rs`) keep resolving unchanged.
pub(crate) use crate::server_proto::{
    exit_code, extract_id_for_parse_error, invalid_params_response, invalid_request_response,
    is_valid_file_uri, live_connection_changed, parse_error_response, resolve_cursor_occurrence,
};
use crate::server_proto::{goto_definition_result, references_result};

pub(crate) struct Server {
    pub(crate) data: Arc<MenuData>,
    pub(crate) docs: HashMap<String, String>, // URI → document text
    /// Memoized logical-line joins per open document (see `parser::ParseCache`).
    pub(crate) parse_cache: ParseCache,
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

/// Write one JSON-RPC `value` to stdout with `Content-Length` framing.
///
/// Serializes `value`, then writes the header, the body, and a flush in
/// that order (no buffering layer). A serialization failure is logged with
/// `serialize_kind` and reported as non-fatal (caller keeps serving); a
/// header, body, or flush failure is logged and reported as fatal (caller
/// terminates the loop). Returns `Err` only when the caller must return.
fn write_response(value: &serde_json::Value, serialize_kind: &str) -> Result<(), ()> {
    let json = match serde_json::to_string(value) {
        Ok(j) => j,
        Err(e) => {
            log_error!("failed to serialize {serialize_kind}: {e}");
            return Ok(());
        }
    };
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout.write_all(header.as_bytes()) {
        log_error!("write header error: {e}");
        return Err(());
    }
    if let Err(e) = stdout.write_all(json.as_bytes()) {
        log_error!("write body error: {e}");
        return Err(());
    }
    if let Err(e) = stdout.flush() {
        log_error!("flush error: {e}");
        return Err(());
    }
    Ok(())
}

impl Server {
    /// Production constructor: live config and cache are provided by `main.rs`
    /// (parsed from env, TTL 60 s, caps from `caps.rs`).
    #[cfg(not(test))]
    pub(crate) fn new(
        data: Arc<MenuData>,
        live_config: LiveConfig,
        live_cache: Arc<Mutex<LiveCache>>,
    ) -> Self {
        Server {
            data: Arc::clone(&data),
            docs: HashMap::new(),
            parse_cache: ParseCache::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
            live_config,
            live_cache,
        }
    }

    /// Test constructor: live is disabled, cache is empty (honest placeholders).
    #[cfg(test)]
    pub(crate) fn new(data: Arc<MenuData>) -> Self {
        Server {
            data: Arc::clone(&data),
            docs: HashMap::new(),
            parse_cache: ParseCache::new(),
            position_encoding: PositionEncoding::default(),
            shutdown_received: false,
            live_config: LiveConfig::from_env_with(|_| None),
            live_cache: Arc::new(Mutex::new(LiveCache::with_default_ttl())),
            published: Vec::new(),
        }
    }

    /// Create a server with explicit live config/cache (used by production and tests).
    pub(crate) fn new_with_live(
        data: Arc<MenuData>,
        live_config: LiveConfig,
        live_cache: Arc<Mutex<LiveCache>>,
    ) -> Self {
        #[cfg(not(test))]
        {
            Self::new(Arc::clone(&data), live_config, live_cache)
        }
        #[cfg(test)]
        {
            Server {
                data: Arc::clone(&data),
                docs: HashMap::new(),
                parse_cache: ParseCache::new(),
                position_encoding: PositionEncoding::default(),
                shutdown_received: false,
                live_config,
                live_cache,
                published: Vec::new(),
            }
        }
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
                    // JSON-RPC -32700: the frame was well-formed but the body
                    // is not valid JSON. Unlike framing Protocol errors (which
                    // are terminal — the stream cannot be resynchronized), the
                    // stream IS still aligned here, so answer with a Parse
                    // error and keep serving. The id is best-effort (body id
                    // or null per spec).
                    log_warn!("JSON parse error: {e}");
                    let id = extract_id_for_parse_error(&body);
                    let resp = parse_error_response(&id);
                    if write_response(&resp, "parse error response").is_err() {
                        return;
                    }
                    continue;
                }
            };

            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

            let response = self.handle_message(method, &msg);

            if let Some(resp) = response
                && write_response(&resp, "response").is_err()
            {
                return;
            }
        }
    }

    pub(crate) fn handle_message(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let start = Instant::now();
        let id = params.get("id").cloned().unwrap_or(serde_json::Value::Null);

        // LSP 3.17 lifecycle: after a `shutdown` request the server must
        // reject every further request with InvalidRequest until `exit`.
        // Notifications (no id) get no response and are silently ignored;
        // `exit` itself is still processed so the process can terminate.
        if self.shutdown_received && method != "exit" {
            if id.is_null() {
                return None;
            }
            return Some(invalid_request_response(&id, "server is shutting down"));
        }

        let result = match method {
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
                // "Enabled but inactive" is the confusing state: surface the
                // exact reason (missing host, fail-closed TLS pin, …) from
                // LiveConfig::inactive_reason instead of a bare active=false.
                log_info!(
                    "live status on initialize: enabled={} active={} inactive_reason={} host={} scheme={} ssl_verify_effective={}",
                    self.live_config.enabled,
                    self.live_config.is_active(),
                    self.live_config.inactive_reason().unwrap_or("none"),
                    sanitize_for_log(&self.live_config.host),
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
                            // Rename for `:local`/`:global` variables vs
                            // `$name` usages — pure logic lives in rename.rs
                            // (document-local, single-document WorkspaceEdit).
                            "renameProvider": true,
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

            "$/cancelRequest" => {
                // Documented no-op. This server reads and handles messages
                // one at a time on the stdio loop, so a cancellation can only
                // ever be observed AFTER the in-flight request has already
                // completed — there is nothing left to interrupt. Cancelling
                // in-flight work would require moving handlers off the read
                // loop (the protocol lifecycle allows it; this server
                // deliberately does not). Log at debug so the limitation is
                // observable, and never answer a notification.
                let cancel_id = params["params"]["id"].as_str().unwrap_or("");
                log_debug!(
                    "$/cancelRequest ignored (single-threaded server): id={}",
                    sanitize_for_log(cancel_id)
                );
                if id.is_null() {
                    None
                } else {
                    // Tolerate a client that sent this as a request: answer
                    // with null so it never waits for a response.
                    Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }))
                }
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
                    log_warn!("rejecting didOpen with non-file URI: {}", uri_for_log(uri));
                    return None;
                }
                let text = params["params"]["textDocument"]["text"].as_str()?;
                // Strip a leading BOM before size checks and storage so it can
                // never shift positions or surface as a phantom token.
                let text = strip_bom_prefix(text);
                let uri_owned = uri.to_string();
                // MAX_DOCS applies to EVERY new URI in didOpen — including
                // oversized documents (which previously skipped this check
                // and were inserted unconditionally), symmetric with didChange.
                if !self.docs.contains_key(&uri_owned) && self.docs.len() >= MAX_DOCS {
                    log_warn!(
                        "too many open documents ({} >= {MAX_DOCS}), rejecting: {}",
                        self.docs.len(),
                        uri_for_log(uri)
                    );
                    return None;
                }
                if text.len() > MAX_DOC_SIZE {
                    log_warn!(
                        "document too large ({} bytes > {MAX_DOC_SIZE}), truncating: {}",
                        text.len(),
                        uri_for_log(uri)
                    );
                    // Truncate at char boundary to avoid invalid UTF-8
                    let trunc_idx = floor_char_boundary(text, MAX_DOC_SIZE);
                    self.docs
                        .insert(uri_owned.clone(), text[..trunc_idx].to_string());
                } else {
                    self.docs.insert(uri_owned.clone(), text.to_string());
                }
                // The stored text changed: drop any cached parse for this URI
                // (a re-open must never serve pre-close logical lines).
                self.parse_cache.invalidate(&uri_owned);
                // Publish diagnostics (push) after open. The cached path
                // borrows the stored text internally, so no up-to-MAX_DOC_SIZE
                // clone happens here.
                let diags = self.encoded_diagnostics_cached(&uri_owned);
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
                    log_warn!(
                        "rejecting didChange with non-file URI: {}",
                        uri_for_log(uri)
                    );
                    return None;
                }
                let changes = params["params"]["contentChanges"].as_array()?;
                if changes.is_empty() {
                    return None;
                }
                // Bound adversarial/pathological batches: each element costs
                // at least one O(doc) line-start scan (encoding.rs), so an
                // unbounded array is a CPU sink on the single-threaded loop.
                // The whole notification is rejected — partially applying it
                // would desync the server's copy from the client's document.
                if changes.len() > MAX_CHANGES_PER_NOTIFICATION {
                    log_warn!(
                        "didChange batch too large ({} > {MAX_CHANGES_PER_NOTIFICATION}), rejecting: {}",
                        changes.len(),
                        uri_for_log(uri)
                    );
                    return None;
                }
                // Enforce doc count cap on first insert via didChange (client may skip didOpen)
                if !self.docs.contains_key(uri) && self.docs.len() >= MAX_DOCS {
                    log_warn!(
                        "too many open documents ({} >= {MAX_DOCS}), rejecting didChange: {}",
                        self.docs.len(),
                        uri_for_log(uri)
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
                             for {}",
                            uri_for_log(uri)
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
                // Live-cache rule: document edits never invalidate the live cache.
                // Live entries are device snapshots keyed by resource kind
                // (not by document), so TTL (`LIVE_TTL_SECS`), negative TTL
                // (`LIVE_NEGATIVE_TTL_SECS`), and the 2 s fetch-coalescing
                // window govern freshness. Clearing here per keystroke
                // discarded fresh entries and defeated coalescing/negative
                // cooldown, causing a fetch storm. Invalidation happens
                // only on didClose, on LiveConfig change
                // (workspace/didChangeConfiguration), or on explicit
                // `rsc.live.refresh`.
                // The stored text changed. No explicit parse-cache
                // invalidation is needed: entries are keyed by text length +
                // hash, so an edited document misses on its own while the
                // preceding publish already warmed the cache for the NEXT
                // request (completion, symbols, …).
                let diags = self.encoded_diagnostics_cached(&uri_owned);
                self.publish_diagnostics(&uri_owned, diags);
                None
            }

            "textDocument/didClose" => {
                if let Some(uri) = params["params"]["textDocument"]["uri"].as_str() {
                    self.docs.remove(uri);
                    // Cache entries die with the document — a later re-open
                    // must reparse, never resurrect pre-close logical lines.
                    self.parse_cache.invalidate(uri);
                    // Live-cache rule: didClose is an invalidation point for the live
                    // cache (device snapshots may be stale for the next
                    // session). Never logs the pass (clear carries no
                    // credentials at all).
                    {
                        let mut guard = self.live_cache.lock().unwrap_or_else(|e| {
                            log_warn!("live cache lock poisoned, recovering");
                            e.into_inner()
                        });
                        guard.clear_all();
                    }
                    // Clear diagnostics for closed file
                    self.publish_diagnostics(uri, Vec::new());
                }
                None
            }

            "textDocument/completion" => Some(self.handle_completion(id, params)),

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
                    log_debug!(
                        "hover for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
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
                    log_debug!(
                        "signatureHelp for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
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
                // must agree on what "the command under the cursor" is. The
                // parse cache shares that join with sibling requests.
                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let help = diagnostics::covering_logical_line(logicals, line_idx).and_then(|ll| {
                    let ctx = parse_line(&self.data, ll.text());
                    let menu = self
                        .data
                        .menu_by_path
                        .get(&crate::text_util::normalize_path(&ctx.path))?;
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
                    log_debug!(
                        "documentSymbol for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };
                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let symbols = symbols::compute_document_symbols_with_logicals(&self.data, logicals);

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
                    log_debug!(
                        "definition for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };

                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let result = goto_definition_result(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                    logicals,
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
                    log_debug!(
                        "references for untracked URI, returning empty list: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [],
                    }));
                };

                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let result = references_result(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                    include_declaration,
                    logicals,
                );

                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }

            "textDocument/rename" => {
                // Variable rename (`:local`/`:global` vs `$name` usages).
                // Same response guarantees as the navigation siblings:
                // -32602 for malformed params, null result for untracked
                // URIs, variable-less cursors, and unusable new names. The
                // pure computation lives in rename.rs; this arm only
                // marshals params/results around it.
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
                let Some(new_name) = params["params"]["newName"].as_str() else {
                    return Some(invalid_params_response(&id, "missing newName"));
                };
                let Some(doc) = self.docs.get(uri) else {
                    log_debug!(
                        "rename for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };

                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let result = rename::rename_result_with_logicals(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                    new_name,
                    logicals,
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
                    log_debug!(
                        "foldingRange for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null,
                    }));
                };
                let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                let ranges = folding::compute_folding_ranges_with_logicals(doc, logicals);
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
                // The cached path borrows the stored text and shares the
                // logical-line join; an untracked URI yields an empty list.
                let diags = self.encoded_diagnostics_cached(uri);
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
                // LSP 3.17: `context.only` is a REQUESTED kind filter. This
                // server produces only `quickfix` actions, so any present
                // `only` list that does not ask for `quickfix` must receive
                // an empty result rather than actions it did not request.
                // An absent (or null) `only` means no filter.
                if let Some(only) = params["params"]["context"]["only"].as_array()
                    && !only.iter().any(|kind| kind.as_str() == Some("quickfix"))
                {
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [],
                    }));
                }
                if !self.docs.contains_key(uri) {
                    log_debug!(
                        "codeAction for untracked URI, returning empty list: {}",
                        uri_for_log(uri)
                    );
                    return Some(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [],
                    }));
                }
                let actions = self.compute_code_actions(uri, client_diags);
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": actions,
                }))
            }

            "workspace/executeCommand" => {
                let command = params["params"]["command"].as_str().unwrap_or("");
                // F5: sanitize + truncate the client-supplied command before logging.
                log_info!(
                    "workspace/executeCommand received: {}",
                    truncate_command_for_log(command)
                );
                match command {
                    "rsc.live.refresh" => {
                        let mut guard = self.live_cache.lock().unwrap_or_else(|e| {
                            log_warn!("live cache lock poisoned, recovering");
                            e.into_inner()
                        });
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
                                        // Also try to map property-like args to cache keys if
                                        // needed.
                                        // No extra mapping; caller should pass cache keys like
                                        // "interfaces".
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
                            truncate_command_for_log(command)
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
                        let guard = self.live_cache.lock().unwrap_or_else(|e| {
                            log_warn!("live cache lock poisoned, recovering");
                            e.into_inner()
                        });
                        let entries = guard.entries.len();
                        let failed = guard.failed_at.len();
                        drop(guard);
                        let status = serde_json::json!({
                            "enabled": self.live_config.enabled,
                            "active": self.live_config.is_active(),
                            // Why live is off even though enabled (None when
                            // active); owned by LiveConfig so callers never
                            // re-derive the policy.
                            "inactive_reason": self.live_config.inactive_reason(),
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
                // Prominent warning when the effective live host changes via
                // settings: the device connection target is security-relevant
                // (SSRF surface), so the change must be visible in logs. The
                // pass is NEVER logged (only old/new hosts, which are safe).
                let old_config = self.live_config.clone();
                if settings.is_null() || !settings.is_object() {
                    // No settings: re-read from env.
                    self.live_config = crate::live::LiveConfig::from_env();
                } else {
                    self.live_config = crate::live::LiveConfig::from_settings_value(settings);
                }
                if self.live_config.host != old_config.host {
                    // F5: hosts are sanitized (no raw newlines into the log).
                    log_warn!(
                        "live host changed via didChangeConfiguration (old={:?} new={:?})",
                        sanitize_for_log(&old_config.host),
                        sanitize_for_log(&self.live_config.host)
                    );
                }
                // Live-cache rule: a LiveConfig change invalidates the live cache —
                // entries fetched under the previous target/credentials are
                // stale by definition. Document edits (didChange) never
                // invalidate; only connection-identity changes do (plus
                // didClose and explicit `rsc.live.refresh`).
                if live_connection_changed(&old_config, &self.live_config) {
                    let mut guard = self.live_cache.lock().unwrap_or_else(|e| {
                        log_warn!("live cache lock poisoned, recovering");
                        e.into_inner()
                    });
                    guard.clear_all();
                    log_info!("live cache cleared after LiveConfig change");
                }
                self.live_config.log_status();
                log_info!("live config reloaded via didChangeConfiguration");
                // Notifications have no id; if this was unexpectedly sent as a request, answer with
                // null.
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
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let uri_opt = params
            .get("params")
            .and_then(|p| p.get("textDocument"))
            .and_then(|t| t.get("uri"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                params
                    .pointer("/params/textDocument/uri")
                    .and_then(|v| v.as_str())
            })
            .or_else(|| params.pointer("/params/uri").and_then(|v| v.as_str()));
        let suffix = crate::logging::request_suffix(
            method,
            uri_opt,
            duration_ms,
            self.position_encoding.as_str(),
        );
        log_debug!("handled {}", suffix); // never logs raw URI or MIKROTIK_PASS
        result
    }
}
