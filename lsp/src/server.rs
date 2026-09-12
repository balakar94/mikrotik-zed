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

use crate::caps::{MAX_DOC_SIZE, MAX_DOCS};
use crate::completion;
use crate::diagnostics;
use crate::encoding::{
    PositionEncoding, apply_incremental_edit, byte_offset_to_utf16_units, floor_char_boundary,
    lsp_character_to_byte_offset, strip_bom_prefix,
};
use crate::folding;
use crate::framing::{Frame, FrameError, read_message};
use crate::hover;
use crate::live::{
    LiveCache, LiveConfig, get_cached_or_fetch_background, trigger_enrichment_for_completion,
};
use crate::logging::{
    log_debug, log_error, log_info, log_warn, sanitize_for_log, truncate_command_for_log,
    uri_for_log,
};
use crate::menus::MenuData;
use crate::parser::{ParseCache, build_before_cursor, parse_line, tokenize_with_spans};
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
    exit_code, extract_id_for_parse_error, invalid_params_response, is_valid_file_uri,
    live_connection_changed, parse_error_response, resolve_cursor_occurrence,
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
                    let json = match serde_json::to_string(&resp) {
                        Ok(j) => j,
                        Err(e) => {
                            log_error!("failed to serialize parse error response: {e}");
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
        let start = Instant::now();
        let id = params.get("id").cloned().unwrap_or(serde_json::Value::Null);

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
                log_info!(
                    "live status on initialize: enabled={} active={} host={} scheme={} ssl_verify_effective={}",
                    self.live_config.enabled,
                    self.live_config.is_active(),
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
                // The stored text changed: drop any cached parse for this URI.
                // (Hash-based lookup would miss anyway; explicit invalidation
                // keeps the lifecycle obvious and the entry count truthful.)
                self.parse_cache.invalidate(&uri_owned);
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
                    log_debug!(
                        "completion for untracked URI, returning null result: {}",
                        uri_for_log(uri)
                    );
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
                    // Property key of the completion target: the `key` of a
                    // trailing `key=value` token, if the cursor is inside a
                    // property assignment.
                    let tokens = crate::parser::tokenize(&before_cursor);
                    let property = tokens.last().and_then(|tok| {
                        crate::parser::split_key_value(tok)
                            .map(|(key, _)| key.trim_start_matches(':'))
                    });
                    // Menu-declared argument type for the property (empty
                    // string when the property is unknown to the menu).
                    let arg_type = property
                        .and_then(|key| {
                            self.data
                                .menu_by_path
                                .get(&crate::text_util::normalize_path(&context.path))
                                .and_then(|menu| {
                                    menu.arguments
                                        .iter()
                                        .find(|a| a.name.eq_ignore_ascii_case(key))
                                })
                                .map(|arg| arg.arg_type.as_str())
                        })
                        .unwrap_or("");
                    trigger_enrichment_for_completion(
                        &self.live_cache,
                        &self.live_config,
                        property,
                        &context.path,
                        arg_type,
                    );
                }
                let mut items = {
                    // Narrow live-lock: held only for the synchronous cache
                    // read inside `compute_completions_with_live` (fresh-entry
                    // `Arc` clones). A full snapshot clone under lock would
                    // also work but `LiveCache` is not `Clone` and the
                    // completion layer only needs `&LiveCache`, so scoping the
                    // guard to this block is the smallest safe narrowing — the
                    // long textEdit injection below runs lock-free.
                    let live_guard = self.live_cache.lock().unwrap_or_else(|e| {
                        log_warn!("live cache lock poisoned, recovering");
                        e.into_inner()
                    });
                    completion::compute_completions_with_live(
                        &self.data,
                        &before_cursor,
                        Some(&*live_guard as &LiveCache),
                    )
                };

                // ── textEdit injection (C-02 logical vs physical) ────────
                // Populate `textEdit` so accepting a completion replaces the
                // already-typed prefix instead of inserting beside it
                // (`in` + `input` → `input`, not `ininput`). `insertText` is
                // retained as fallback for clients that ignore `textEdit` (Zed
                // supports it). If computing the range fails we leave `textEdit`
                // as `None` and the client falls back to insertion at cursor.
                //
                // Four cases are handled (per spec, at least these):
                // - value completions after `=`: range covers the typed suffix
                //   after `=` (excluding a leading opening quote so `"in` → `input`
                //   preserves the quote as `"input`);
                // - sub-menu / verb completions before a verb: when the cursor
                //   sits inside a partial token that prefixes a child name, range
                //   covers that token so `addr` → `address`;
                // - partial menu-path segment completions (`/ip/addr`): range
                //   covers only the typed final segment so `addr` → `address`
                //   while the already-typed `/ip/` prefix is preserved.
                // - property / flag completions after a verb (kinds 5/14): the
                //   same partial-name span as the sub-menu case, so `inter` →
                //   `interface=…` replaces instead of appending. Span guards
                //   duplicate `completion::partial_name_token` locally (no `=`,
                //   excluded `/ : " ' ( [ $` leaders, mid-token cursor).
                // For all other cases (e.g., already-finished token + space) the
                // edit is zero-length at the cursor (pure insertion).
                //
                // Logical vs physical: RouterOS `\`-continued commands join
                // several physical lines into one logical line. `before_cursor`
                // is a logical join (via `build_before_cursor`), but
                // `line_text`/`char_byte` are physical. We map the cursor into
                // logical coordinates with `diagnostics::logical_lines` +
                // `LogicalLine::logical_offset_from_physical` (as signatureHelp
                // does), compute the prefix range in logical space, then map it
                // back with `LogicalLine::map_range` to physical line/character.
                // This makes a cursor on continuation line 2 (e.g.
                // `gateway=1.1.1.1 \` + `comment="x"`) cover the logical token
                // correctly while staying byte- and UTF-16-correct.
                {
                    let line_text = current_line;
                    // Value vs non-value decision uses the same tolerant trimmed
                    // logic as `completion::match_context` — driven by the
                    // logical `before_cursor` (continuation-aware).
                    let trimmed_bc = before_cursor.trim_end();
                    let has_trailing_ws = trimmed_bc.len() != before_cursor.len();
                    let trimmed_last = crate::parser::tokenize(trimmed_bc)
                        .last()
                        .cloned()
                        .unwrap_or_default();
                    let mut value_range_phys: Option<(usize, usize, usize, usize)> = None;
                    // (phys_start_line, phys_start_char_byte, phys_end_line, phys_end_char_byte) in
                    // byte offsets
                    // Helper: try logical path first, fallback to physical.
                    // Cached join: single-hash lookup-or-insert (cold cache or
                    // changed text reparses once; warm hits reuse the slice).
                    // Identical bytes to a fresh `logical_lines` either way,
                    // so mapping behavior is identical warm or cold.
                    let logicals = self.parse_cache.lookup_or_insert(uri, doc);
                    let covering = diagnostics::covering_logical_line(logicals, line_idx);
                    let cursor_logical_opt = covering
                        .and_then(|ll| ll.logical_offset_from_physical(line_idx, char_byte));

                    if let Some((key_part, value_part)) =
                        crate::parser::split_key_value(&trimmed_last)
                    {
                        let _ = key_part;
                        let raw_suffix = value_part;
                        let trimmed_suffix = raw_suffix.trim_matches(|c| c == '"' || c == '\'');
                        if !has_trailing_ws || trimmed_suffix.is_empty() {
                            // Value context confirmed.
                            // Try logical mapping.
                            let mut logical_success = false;
                            if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt)
                            {
                                let logical_text = ll.text();
                                let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                                let cursor_logical_clamped = crate::encoding::floor_char_boundary(
                                    logical_text,
                                    cursor_logical_clamped,
                                );
                                let logical_prefix = &logical_text[..cursor_logical_clamped];
                                let tokens = crate::parser::tokenize_with_spans(logical_prefix);
                                if let Some(tok) = tokens.last() {
                                    if let Some((key_part, _)) =
                                        crate::parser::split_key_value(&tok.text)
                                    {
                                        let pos = key_part.len();
                                        let suffix_part = &tok.text[pos + 1..];
                                        let leading = if suffix_part.starts_with('"')
                                            || suffix_part.starts_with('\'')
                                        {
                                            1
                                        } else {
                                            0
                                        };
                                        let log_start = tok.start + pos + 1 + leading;
                                        let (log_s, log_e) =
                                            if has_trailing_ws && trimmed_suffix.is_empty() {
                                                (cursor_logical_clamped, cursor_logical_clamped)
                                            } else {
                                                (log_start, cursor_logical_clamped)
                                            };
                                        let log_s = log_s.min(logical_text.len()).min(log_e);
                                        let log_e = log_e.min(logical_text.len());
                                        let log_s = crate::encoding::floor_char_boundary(
                                            logical_text,
                                            log_s,
                                        );
                                        let log_e = crate::encoding::floor_char_boundary(
                                            logical_text,
                                            log_e,
                                        );
                                        let range = ll.map_range(log_s, log_e);
                                        // Convert physical byte offsets per line for utf16 later;
                                        // store as (start_line, start_byte, end_line, end_byte)
                                        value_range_phys = Some((
                                            range.start.line as usize,
                                            range.start.character as usize,
                                            range.end.line as usize,
                                            range.end.character as usize,
                                        ));
                                        logical_success = true;
                                    }
                                } else if has_trailing_ws && trimmed_suffix.is_empty() {
                                    let range = ll
                                        .map_range(cursor_logical_clamped, cursor_logical_clamped);
                                    value_range_phys = Some((
                                        range.start.line as usize,
                                        range.start.character as usize,
                                        range.end.line as usize,
                                        range.end.character as usize,
                                    ));
                                    logical_success = true;
                                }
                            }
                            if !logical_success {
                                // Physical fallback (single-line case or no logical coverage).
                                let prefix_line = &line_text[..char_byte.min(line_text.len())];
                                let tokens = crate::parser::tokenize_with_spans(prefix_line);
                                if let Some(tok) = tokens.last() {
                                    if let Some((key_part, _)) =
                                        crate::parser::split_key_value(&tok.text)
                                    {
                                        let pos = key_part.len();
                                        let suffix_part = &tok.text[pos + 1..];
                                        let leading = if suffix_part.starts_with('"')
                                            || suffix_part.starts_with('\'')
                                        {
                                            1
                                        } else {
                                            0
                                        };
                                        let start = tok.start + pos + 1 + leading;
                                        let (s, e) = if has_trailing_ws && trimmed_suffix.is_empty()
                                        {
                                            (char_byte, char_byte)
                                        } else {
                                            (start, char_byte)
                                        };
                                        let s_clamped = s.min(line_text.len()).min(e);
                                        let e_clamped = e.min(line_text.len());
                                        let s_floored = crate::encoding::floor_char_boundary(
                                            line_text, s_clamped,
                                        );
                                        let e_floored = crate::encoding::floor_char_boundary(
                                            line_text, e_clamped,
                                        );
                                        value_range_phys =
                                            Some((line_idx, s_floored, line_idx, e_floored));
                                    }
                                } else if has_trailing_ws && trimmed_suffix.is_empty() {
                                    value_range_phys =
                                        Some((line_idx, char_byte, line_idx, char_byte));
                                }
                            }
                        }
                    }
                    if let Some((s_line, s_byte, e_line, e_byte)) = value_range_phys {
                        // Convert byte offsets to wire characters per encoding, using
                        // the physical line text for the corresponding line (so
                        // multi-byte chars are counted correctly per line).
                        let lines: Vec<&str> = doc.lines().collect();
                        let s_line_text = lines.get(s_line).copied().unwrap_or("");
                        let e_line_text = lines.get(e_line).copied().unwrap_or("");
                        let start_char = match self.position_encoding {
                            PositionEncoding::Utf8 => s_byte as u32,
                            PositionEncoding::Utf16 => {
                                byte_offset_to_utf16_units(s_line_text, s_byte)
                            }
                        };
                        let end_char = match self.position_encoding {
                            PositionEncoding::Utf8 => e_byte as u32,
                            PositionEncoding::Utf16 => {
                                byte_offset_to_utf16_units(e_line_text, e_byte)
                            }
                        };
                        for item in &mut items {
                            if item.kind == Some(12) {
                                let new_text = item
                                    .insert_text
                                    .clone()
                                    .unwrap_or_else(|| item.label.clone());
                                item.text_edit = Some(completion::TextEdit {
                                    range: completion::CompletionRange {
                                        start: completion::CompletionPosition {
                                            line: s_line as u32,
                                            character: start_char,
                                        },
                                        end: completion::CompletionPosition {
                                            line: e_line as u32,
                                            character: end_char,
                                        },
                                    },
                                    new_text,
                                });
                            }
                        }
                    } else {
                        // Sub-menu / verb (kinds 9/3) and property / flag
                        // (kinds 5/14) prefix case (logical-aware with physical
                        // fallback). Both share the same partial-name token span
                        // (mirrors `completion::partial_name_token` guards); the
                        // kind filter below only selects which items receive the
                        // shared range. Ranges always land on the cursor line
                        // via logical mapping or the physical cursor line —
                        // never a line-0 guess.
                        let mut submenu_range_phys: Option<(usize, usize, usize, usize)> = None;
                        let mut typed_lower: Option<String> = None;
                        // Try logical first when covering exists.
                        if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt) {
                            let logical_text = ll.text();
                            let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                            let cursor_logical_clamped = crate::encoding::floor_char_boundary(
                                logical_text,
                                cursor_logical_clamped,
                            );
                            let logical_prefix = &logical_text[..cursor_logical_clamped];
                            if !logical_prefix.ends_with(char::is_whitespace)
                                && !logical_prefix.is_empty()
                            {
                                let tokens = crate::parser::tokenize_with_spans(logical_prefix);
                                if let Some(tok) = tokens.last()
                                    && crate::parser::split_key_value(&tok.text).is_none()
                                    && !tok.text.contains('=')
                                    && !tok.text.starts_with(':')
                                    && !tok.text.starts_with('/')
                                    && !tok.text.starts_with('"')
                                    && !tok.text.starts_with('\'')
                                    && !tok.text.starts_with('(')
                                    && !tok.text.starts_with('[')
                                    && !tok.text.starts_with('$')
                                {
                                    let typed = tok.text.as_str();
                                    let lower = typed.to_ascii_lowercase();
                                    let needs_edit = items.iter().any(|it| {
                                        (it.kind == Some(9)
                                            || it.kind == Some(3)
                                            || it.kind == Some(5)
                                            || it.kind == Some(14))
                                            && it.label.to_ascii_lowercase().starts_with(&lower)
                                    });
                                    if needs_edit && !typed.is_empty() {
                                        let log_s = tok.start;
                                        let log_e = tok.end.min(cursor_logical_clamped);
                                        let range = ll.map_range(log_s, log_e);
                                        submenu_range_phys = Some((
                                            range.start.line as usize,
                                            range.start.character as usize,
                                            range.end.line as usize,
                                            range.end.character as usize,
                                        ));
                                        typed_lower = Some(lower);
                                    }
                                }
                            }
                        }
                        if submenu_range_phys.is_none() {
                            // Physical fallback.
                            let prefix_line = &line_text[..char_byte.min(line_text.len())];
                            if !prefix_line.ends_with(char::is_whitespace)
                                && !prefix_line.is_empty()
                            {
                                let tokens = crate::parser::tokenize_with_spans(prefix_line);
                                if let Some(tok) = tokens.last()
                                    && crate::parser::split_key_value(&tok.text).is_none()
                                    && !tok.text.contains('=')
                                    && !tok.text.starts_with(':')
                                    && !tok.text.starts_with('/')
                                    && !tok.text.starts_with('"')
                                    && !tok.text.starts_with('\'')
                                    && !tok.text.starts_with('(')
                                    && !tok.text.starts_with('[')
                                    && !tok.text.starts_with('$')
                                {
                                    let typed = tok.text.as_str();
                                    let lower = typed.to_ascii_lowercase();
                                    let needs_edit = items.iter().any(|it| {
                                        (it.kind == Some(9)
                                            || it.kind == Some(3)
                                            || it.kind == Some(5)
                                            || it.kind == Some(14))
                                            && it.label.to_ascii_lowercase().starts_with(&lower)
                                    });
                                    if needs_edit && !typed.is_empty() {
                                        let s_byte = tok.start;
                                        let e_byte = tok.end.min(char_byte);
                                        submenu_range_phys =
                                            Some((line_idx, s_byte, line_idx, e_byte));
                                        typed_lower = Some(lower);
                                    }
                                }
                            }
                        }
                        // Partial menu-path segment (`/ip/addr`): the
                        // completion layer offers the parent's matching child
                        // menu and replaces ONLY the typed final segment. Map
                        // that segment span from the logical join (preferred,
                        // continuation-aware) or the physical cursor line so
                        // the segment-only edit never reaches the wire with a
                        // line-0 guess.
                        if submenu_range_phys.is_none() {
                            if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt)
                            {
                                let logical_text = ll.text();
                                let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                                let cursor_logical_clamped = crate::encoding::floor_char_boundary(
                                    logical_text,
                                    cursor_logical_clamped,
                                );
                                let logical_prefix = &logical_text[..cursor_logical_clamped];
                                if let Some((typed, s, e)) =
                                    completion::partial_path_segment(logical_prefix)
                                {
                                    let range = ll.map_range(s, e);
                                    submenu_range_phys = Some((
                                        range.start.line as usize,
                                        range.start.character as usize,
                                        range.end.line as usize,
                                        range.end.character as usize,
                                    ));
                                    typed_lower = Some(typed.to_ascii_lowercase());
                                }
                            }
                            if submenu_range_phys.is_none() {
                                let prefix_line = &line_text[..char_byte.min(line_text.len())];
                                if let Some((typed, s, e)) =
                                    completion::partial_path_segment(prefix_line)
                                {
                                    submenu_range_phys = Some((line_idx, s, line_idx, e));
                                    typed_lower = Some(typed.to_ascii_lowercase());
                                }
                            }
                        }
                        if let (Some((s_line, s_byte, e_line, e_byte)), Some(lower)) =
                            (submenu_range_phys, typed_lower)
                        {
                            let lines: Vec<&str> = doc.lines().collect();
                            let s_line_text = lines.get(s_line).copied().unwrap_or("");
                            let e_line_text = lines.get(e_line).copied().unwrap_or("");
                            let start_char = match self.position_encoding {
                                PositionEncoding::Utf8 => s_byte as u32,
                                PositionEncoding::Utf16 => {
                                    byte_offset_to_utf16_units(s_line_text, s_byte)
                                }
                            };
                            let end_char = match self.position_encoding {
                                PositionEncoding::Utf8 => e_byte as u32,
                                PositionEncoding::Utf16 => {
                                    byte_offset_to_utf16_units(e_line_text, e_byte)
                                }
                            };
                            for item in &mut items {
                                if (item.kind == Some(9)
                                    || item.kind == Some(3)
                                    || item.kind == Some(5)
                                    || item.kind == Some(14))
                                    && item.label.to_ascii_lowercase().starts_with(&lower)
                                {
                                    let new_text = item
                                        .insert_text
                                        .clone()
                                        .unwrap_or_else(|| item.label.clone());
                                    item.text_edit = Some(completion::TextEdit {
                                        range: completion::CompletionRange {
                                            start: completion::CompletionPosition {
                                                line: s_line as u32,
                                                character: start_char,
                                            },
                                            end: completion::CompletionPosition {
                                                line: e_line as u32,
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
                // must agree on what "the command under the cursor" is.
                let logicals = diagnostics::logical_lines(doc);
                let help = diagnostics::covering_logical_line(&logicals, line_idx).and_then(|ll| {
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

                let result = rename::rename_result(
                    doc,
                    self.position_encoding,
                    uri,
                    line as usize,
                    character as usize,
                    new_name,
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
                    log_debug!(
                        "codeAction for untracked URI, returning empty list: {}",
                        uri_for_log(uri)
                    );
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
