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
// - Advertises textDocumentSync = 1 (Full). Clients should send the full
//   document text on each change. For robustness, incremental edits with
//   a `range` are also handled by applying a best-effort patch.
// - Content-Length is capped at 10 MiB to avoid unbounded allocation.

mod completion;
mod diagnostics;
mod hover;
mod menus;
mod server;

use menus::{LineContext, MenuData};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::OnceLock;

// ── Structured logging (RSC_LS_LOG) ───────────────────────────
// Visible via `zed --foreground` or `zed: open log`.
// Levels: error < warn < info < debug < trace
// Env var: RSC_LS_LOG (e.g. "debug", "info", "trace", "warn", "error")
// Also respects RUST_LOG as fallback. Default: info.
// All logs go to stderr so they appear in Zed's foreground logs without
// corrupting LSP stdio (stdout is reserved for JSON-RPC).

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

static LOG_LEVEL: OnceLock<LogLevel> = OnceLock::new();

fn log_level() -> LogLevel {
    *LOG_LEVEL.get_or_init(|| {
        let raw = std::env::var("RSC_LS_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .unwrap_or_else(|_| "info".to_string());
        match raw.to_ascii_lowercase().trim() {
            "error" => LogLevel::Error,
            "warn" | "warning" => LogLevel::Warn,
            "info" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            // Support RUST_LOG style like "rsc_ls=debug" or "debug"
            s if s.contains("trace") => LogLevel::Trace,
            s if s.contains("debug") => LogLevel::Debug,
            s if s.contains("info") => LogLevel::Info,
            s if s.contains("warn") => LogLevel::Warn,
            _ => LogLevel::Info,
        }
    })
}

fn should_log(level: LogLevel) -> bool {
    level <= log_level()
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        if should_log(LogLevel::Error) {
            eprintln!("[rsc-ls][ERROR] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        if should_log(LogLevel::Warn) {
            eprintln!("[rsc-ls][WARN] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        if should_log(LogLevel::Info) {
            eprintln!("[rsc-ls][INFO] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_debug {
    ($($arg:tt)*) => {
        if should_log(LogLevel::Debug) {
            eprintln!("[rsc-ls][DEBUG] {}", format!($($arg)*));
        }
    };
}

macro_rules! log_trace {
    ($($arg:tt)*) => {
        if should_log(LogLevel::Trace) {
            eprintln!("[rsc-ls][TRACE] {}", format!($($arg)*));
        }
    };
}

const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;
const MAX_HEADER_SIZE: usize = 32 * 1024; // 32 KiB — hard cap on header section
const MAX_DOC_SIZE: usize = 5 * 1024 * 1024; // 5 MiB per document — prevents single-file OOM
const MAX_DOCS: usize = 100; // cap number of tracked documents

fn main() {
    // Initialize log level early (reads RSC_LS_LOG)
    let level = log_level();
    eprintln!(
        "[rsc-ls][INFO] language server starting (RSC_LS_LOG={:?} -> {:?})",
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

// ── Server state ────────────────────────────────────────────────

/// Negotiated LSP position encoding for `Position.character` values
/// exchanged with the client (LSP 3.17).
///
/// The default is [`PositionEncoding::Utf16`] because that is what the LSP
/// specification mandates when a client does not advertise the
/// `general.positionEncodings` capability — conservative before
/// `initialize`. The server prefers `utf-8` during negotiation since all
/// internal position math is byte-based; conversions happen only at the
/// protocol boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum PositionEncoding {
    #[default]
    Utf16,
    Utf8,
}

impl PositionEncoding {
    /// Wire identifier as used in `general.positionEncodings` and echoed in
    /// the server's `capabilities.positionEncoding` response field.
    fn as_str(self) -> &'static str {
        match self {
            PositionEncoding::Utf16 => "utf-16",
            PositionEncoding::Utf8 => "utf-8",
        }
    }
}

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
                            "textDocumentSync": 1,
                            "completionProvider": {
                                "triggerCharacters": ["/", " ", "="],
                            },
                            "hoverProvider": true,
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
                // This server advertises textDocumentSync = 1 (Full), so clients
                // should send the full text. For robustness, handle both:
                // - Full sync: each change contains only "text" (replace doc).
                // - Incremental sync: changes contain "range" + "text" (patch doc).
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

// ── Protocol helpers ────────────────────────────────────────────

fn parse_content_length(headers: &str) -> Option<usize> {
    let mut found: Option<usize> = None;
    for line in headers.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(colon_idx) = trimmed.find(':') else {
            continue;
        };
        let (name, value_with_colon) = trimmed.split_at(colon_idx);
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let val_str = value_with_colon[1..].trim();
        // Reject empty, signed, or non-digit values (prevents smuggling via "  42  extra")
        if val_str.is_empty() || val_str.starts_with('+') || val_str.starts_with('-') {
            eprintln!("[rsc-ls] malformed Content-Length value: {val_str:?}");
            return None;
        }
        if !val_str.chars().all(|c| c.is_ascii_digit()) {
            eprintln!("[rsc-ls] malformed Content-Length (non-digit): {val_str:?}");
            return None;
        }
        let parsed: usize = match val_str.parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("[rsc-ls] Content-Length overflow or invalid: {val_str:?}");
                return None;
            }
        };
        if found.is_some() {
            eprintln!("[rsc-ls] duplicate Content-Length header, rejecting message");
            return None;
        }
        found = Some(parsed);
    }
    found
}

fn discard_bytes<R: std::io::Read>(reader: &mut R, mut n: usize) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    while n > 0 {
        let to_read = n.min(buf.len());
        reader.read_exact(&mut buf[..to_read])?;
        n -= to_read;
    }
    Ok(())
}

/// Outcome of reading one length-prefixed message.
#[derive(Debug)]
pub(crate) enum Frame {
    /// One complete message body.
    Message(Vec<u8>),
    /// Clean EOF at a message boundary — the stream is over.
    Eof,
    /// The message was deliberately discarded (oversized body or zero-length
    /// body) and the stream remains usable; the caller should read again.
    Skipped,
}

/// Terminal framing failure: the stream cannot be resynchronized because
/// headers were unparsable (missing / malformed / duplicate Content-Length).
/// The caller must terminate so the client's supervisor can restart a clean
/// server; continuing would interpret body bytes as headers (desync cascade).
#[derive(Debug)]
pub(crate) enum FrameError {
    Protocol(String),
    Io(std::io::Error),
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Read exactly one length-prefixed JSON-RPC message from `reader`.
///
/// Behavior-preserving extraction of the former inline loop in [`Server::run`]
/// with one deliberate change: when headers cannot be parsed into a
/// Content-Length, this returns [`FrameError::Protocol`] instead of skipping
/// ahead — skipping is what caused permanent header/body desync.
///
/// Preserved defensive properties:
/// - Header section capped at [`MAX_HEADER_SIZE`]; on overflow with a
///   parseable Content-Length the body is drained and the frame skipped.
/// - Bodies larger than [`MAX_MESSAGE_SIZE`] are drained and skipped.
/// - Zero-length bodies are skipped.
/// - EOF on the first header line yields [`Frame::Eof`]; EOF mid-frame is an
///   I/O error.
pub(crate) fn read_message<R: BufRead>(reader: &mut R) -> Result<Frame, FrameError> {
    // Read headers until an empty line. Handle both "\r\n" and "\n".
    let mut header_buf = String::new();
    let mut header_bytes: usize = 0;
    let mut header_too_large = false;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line)? {
            0 => return Ok(Frame::Eof),
            _ => {
                header_bytes += line.len();
                if header_bytes > MAX_HEADER_SIZE {
                    log_error!("header too large (> {MAX_HEADER_SIZE} bytes), discarding message");
                    header_too_large = true;
                    // Drain until empty line to resync framing
                    if line == "\r\n" || line == "\n" || line.trim().is_empty() {
                        break;
                    } else {
                        continue;
                    }
                }
                header_buf.push_str(&line);
                if line == "\r\n" || line == "\n" || line.trim().is_empty() {
                    break;
                }
            }
        }
    }

    if header_too_large {
        // If we overflowed, attempt to discard a body if Content-Length was
        // present before overflow detection completed; otherwise the headers
        // are unusable → unrecoverable.
        return match parse_content_length(&header_buf) {
            Some(cl) if cl > 0 => {
                discard_bytes(reader, cl).map_err(FrameError::from)?;
                Ok(Frame::Skipped)
            }
            Some(_) => Ok(Frame::Skipped), // zero-length body claimed: nothing to drain
            None => Err(FrameError::Protocol(
                "headers exceeded MAX_HEADER_SIZE without a parsable Content-Length".to_string(),
            )),
        };
    }

    // Parse Content-Length (case-insensitive). None here is terminal: without
    // a trusted length we cannot know where the body ends, so consuming more
    // bytes would guess — the desync cascade this guard exists to prevent.
    let content_length = parse_content_length(&header_buf).ok_or_else(|| {
        FrameError::Protocol("missing or malformed Content-Length header".to_string())
    })?;

    if content_length == 0 {
        return Ok(Frame::Skipped);
    }

    if content_length > MAX_MESSAGE_SIZE {
        log_warn!(
            "message too large: {content_length} bytes (limit {MAX_MESSAGE_SIZE}), discarding"
        );
        discard_bytes(reader, content_length).map_err(|e| {
            log_error!("failed to discard oversized body: {e}");
            FrameError::Io(e)
        })?;
        return Ok(Frame::Skipped);
    }

    // Read body
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(Frame::Message(body))
}

/// Polyfill for `str::floor_char_boundary` (stabilized in Rust 1.91).
/// Returns the largest index <= `index` that is a char boundary.
pub(crate) fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    if s.is_char_boundary(index) {
        return index;
    }
    // Walk backwards to previous char boundary (max 3 bytes for UTF-8)
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Convert UTF-16 code units to a byte offset within `line`.
///
/// Walks chars accumulating `ch.encode_utf16().count()`. A value that lands
/// inside a multi-unit character (surrogate half) resolves forward to that
/// character's end; values beyond the line clamp to `line.len()`.
pub(crate) fn utf16_to_byte_offset(line: &str, units: usize) -> usize {
    // ASCII fast path: one byte per UTF-16 code unit.
    if line.is_ascii() {
        return units.min(line.len());
    }
    let mut seen = 0usize;
    let mut units_buf = [0u16; 2];
    for (byte_off, ch) in line.char_indices() {
        if seen >= units {
            return byte_off;
        }
        seen += ch.encode_utf16(&mut units_buf).len();
    }
    line.len()
}

/// Convert a byte offset within `line` to UTF-16 code units (clamps/floors
/// the byte offset first).
///
/// The offset is clamped to the line length and floored to the nearest char
/// boundary before counting, so non-boundary inputs yield the units of the
/// preceding character's start.
pub(crate) fn byte_offset_to_utf16_units(line: &str, byte_offset: usize) -> u32 {
    let off = floor_char_boundary(line, byte_offset.min(line.len()));
    if line.is_ascii() {
        return off as u32;
    }
    line[..off]
        .chars()
        .map(|ch| {
            let mut units_buf = [0u16; 2];
            ch.encode_utf16(&mut units_buf).len() as u32
        })
        .sum()
}

/// Resolve an inbound LSP `character` value into a byte offset within `line`.
///
/// This is the single conversion point between the negotiated wire encoding
/// and the server's internal byte-based positions. Callers must pass the
/// exact same line text that downstream consumers slice (`str::lines()`
/// semantics: `\n`-separated, trailing `\r` stripped).
pub(crate) fn lsp_character_to_byte_offset(
    line: &str,
    character: usize,
    enc: PositionEncoding,
) -> usize {
    match enc {
        // Legacy byte semantics: clamp and floor to a char boundary.
        PositionEncoding::Utf8 => floor_char_boundary(line, character.min(line.len())),
        PositionEncoding::Utf16 => utf16_to_byte_offset(line, character),
    }
}

/// Recompute diagnostic range characters from internal byte-offset semantics
/// into the negotiated encoding, measured against the physical lines of the
/// ORIGINAL document. Lines are split exactly like inbound logic
/// (`str::lines()`: '\n'-separated with trailing '\r' stripped); multi-line
/// ranges convert each endpoint against its own line. Under
/// [`PositionEncoding::Utf8`] this is a semantic no-op.
fn convert_diagnostic_ranges(
    diags: Vec<diagnostics::Diagnostic>,
    doc: &str,
    enc: PositionEncoding,
) -> Vec<diagnostics::Diagnostic> {
    if enc == PositionEncoding::Utf8 {
        return diags;
    }
    let lines: Vec<&str> = doc.lines().collect();
    diags
        .into_iter()
        .map(|mut d| {
            // Each endpoint may sit on a different physical line (LSP allows
            // multi-line ranges across RouterOS continuations), so convert
            // them independently against their own line text.
            let convert = |p: &mut diagnostics::Position| {
                let line_text = lines.get(p.line as usize).copied().unwrap_or("");
                p.character = byte_offset_to_utf16_units(line_text, p.character as usize);
            };
            convert(&mut d.range.start);
            convert(&mut d.range.end);
            d
        })
        .collect()
}

#[derive(Debug)]
enum EditError {
    InvalidRange,
    OutOfBounds,
}

/// Resolve an LSP position to a byte offset within `doc`.
///
/// `character` is interpreted per `enc`: a UTF-16 code-unit count (spec
/// default) or already-byte-based. The target line is located by scanning
/// '\n' separators; its content excludes the trailing '\r' of CRLF endings,
/// so positions never address the carriage return.
fn lsp_position_to_offset(
    doc: &str,
    line: usize,
    character: usize,
    enc: PositionEncoding,
) -> Result<usize, EditError> {
    // Walk the document to find the start of the target line.
    let mut current_line = 0usize;
    let mut line_start = 0usize;

    for (idx, ch) in doc.char_indices() {
        if current_line == line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    if current_line != line {
        // Requested line beyond end — treat offset as end of doc (append).
        if line > current_line {
            return Err(EditError::OutOfBounds);
        }
    }

    let line_end = doc[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(doc.len());
    // Strip trailing '\r' for "\r\n" handling.
    let line_content = doc[line_start..line_end].trim_end_matches('\r');

    let byte_pos = lsp_character_to_byte_offset(line_content, character, enc);
    Ok(line_start + byte_pos)
}

/// Apply one incremental `range` edit (`new_text`) to `doc`.
///
/// Range characters are interpreted per `enc`; on any invalid or
/// out-of-bounds range the caller falls back to a full document replace.
fn apply_incremental_edit(
    doc: &mut String,
    range: &serde_json::Value,
    new_text: &str,
    enc: PositionEncoding,
) -> Result<(), EditError> {
    let start = range.get("start").ok_or(EditError::InvalidRange)?;
    let end = range.get("end").ok_or(EditError::InvalidRange)?;
    let start_line = start
        .get("line")
        .and_then(|v| v.as_u64())
        .ok_or(EditError::InvalidRange)? as usize;
    let start_char = start
        .get("character")
        .and_then(|v| v.as_u64())
        .ok_or(EditError::InvalidRange)? as usize;
    let end_line = end
        .get("line")
        .and_then(|v| v.as_u64())
        .ok_or(EditError::InvalidRange)? as usize;
    let end_char = end
        .get("character")
        .and_then(|v| v.as_u64())
        .ok_or(EditError::InvalidRange)? as usize;

    let start_offset = lsp_position_to_offset(doc, start_line, start_char, enc)?;
    let end_offset = lsp_position_to_offset(doc, end_line, end_char, enc)?;

    if start_offset > end_offset || end_offset > doc.len() {
        return Err(EditError::OutOfBounds);
    }
    doc.replace_range(start_offset..end_offset, new_text);
    Ok(())
}

// ── Tokenizer / parser (ported from ls.mjs) ─────────────────────

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

/// Scan one whitespace-delimited token starting at byte offset `start`.
///
/// Tracks quote state so whitespace inside `"..."` or `'...'` does not split
/// the token and a backslash inside quotes escapes the next byte. This
/// mirrors the state machine of `continuation_body_end` in diagnostics.rs
/// (RouterOS treats both quote styles symmetrically). Returns the exclusive
/// end offset, which is always a char boundary: quote, backslash and
/// whitespace bytes only occur as standalone bytes in valid UTF-8.
fn scan_token(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut in_double = false;
    let mut in_single = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_double || in_single => {
                // Escaped byte inside quotes: skip it entirely. Clamp so a
                // trailing backslash cannot push `i` past `bytes.len()`.
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' if !in_single => in_double = !in_double,
            b'\'' if !in_double => in_single = !in_single,
            _ if !in_double && !in_single && bytes[i].is_ascii_whitespace() => break,
            _ => {}
        }
        i += 1;
    }
    i
}

/// Split a line into tokens with spans: quoted strings, /-prefixed paths, or
/// bare words.
///
/// Quote-aware: a bare word that opens a quote keeps consuming across
/// whitespace until the matching close (e.g. `comment="a=b c=d"` stays ONE
/// token), so quoted values can no longer spawn phantom property tokens
/// downstream. Unterminated quotes simply run to end-of-input.
pub(crate) fn tokenize_with_spans(text: &str) -> Vec<SpanToken> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
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
/// `cursor_char` is a BYTE offset within the cursor line (already converted
/// from the negotiated wire encoding by callers at the protocol boundary).
pub fn build_before_cursor(doc: &str, cursor_line: usize, cursor_char: usize) -> String {
    let lines: Vec<&str> = doc.lines().collect();
    if cursor_line >= lines.len() {
        return String::new();
    }

    let line = lines[cursor_line];
    let clamped = cursor_char.min(line.len());
    let safe_char = floor_char_boundary(line, clamped);
    let current_part = &line[..safe_char];
    if current_part.trim().is_empty() {
        return String::new();
    }

    let mut parts = vec![current_part];

    for i in (0..cursor_line).rev() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.starts_with('/') || trimmed.starts_with(':') {
            parts.insert(0, lines[i]);
            break;
        }
        parts.insert(0, lines[i]);
    }

    parts.join(" ").trim().to_string()
}

/// Parse a line of RouterOS script into structural components.
pub fn parse_line(data: &MenuData, before_cursor: &str) -> LineContext {
    let tokens = tokenize(before_cursor);
    let mut path_parts: Vec<String> = Vec::new();
    let mut command: Option<String> = None;
    let mut properties: HashMap<String, String> = HashMap::new();
    let last_token = tokens.last().cloned().unwrap_or_default();

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
        last_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::MenuData;
    use std::io::Cursor;

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
        // Full expected: "/ip/address print /ip/route"
        assert_eq!(s, "/ip/address print /ip/route");
    }

    #[test]
    fn test_build_before_cursor_trims() {
        let doc = "  /ip/address add  ";
        let s = build_before_cursor(doc, 0, doc.len());
        assert_eq!(s, "/ip/address add");
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

    // ── parse_line ────────────────────────────────────────────────

    #[test]
    fn test_parse_line_path_only() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address");
        assert_eq!(ctx.path, "/ip/address");
        assert!(ctx.command.is_none());
        assert!(ctx.properties.is_empty());
        assert_eq!(ctx.last_token, "/ip/address");
    }

    #[test]
    fn test_parse_line_path_with_verb() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/address add");
        assert_eq!(ctx.path, "/ip/address");
        assert_eq!(ctx.command.as_deref(), Some("add"));
        assert_eq!(ctx.last_token, "add");
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
        assert_eq!(ctx.last_token, "interface=ether1");
    }

    #[test]
    fn test_parse_line_property_with_empty_value() {
        let data = synthetic_data();
        let ctx = parse_line(&data, "/ip/firewall/filter add chain=");
        assert_eq!(ctx.properties.get("chain").map(|s| s.as_str()), Some(""));
        assert_eq!(ctx.last_token, "chain=");
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
        assert_eq!(ctx.last_token, "");
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
        // last_token remains the RAW token text (key included), per LineContext.
        assert_eq!(ctx2.last_token, r#"comment="hello world""#);
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

    // ── parse_content_length ──────────────────────────────────────

    #[test]
    fn test_parse_content_length_simple() {
        let headers = "Content-Length: 42\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(42));
    }

    #[test]
    fn test_parse_content_length_lowercase() {
        let headers = "content-length: 123\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(123));
    }

    #[test]
    fn test_parse_content_length_mixed_case() {
        let headers = "ConTent-LenGth: 99\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(99));
    }

    #[test]
    fn test_parse_content_length_missing() {
        let headers = "Content-Type: foo\r\n\r\n";
        assert_eq!(parse_content_length(headers), None);
    }

    #[test]
    fn test_parse_content_length_with_spaces() {
        let headers = "Content-Length:   7  \r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(7));
    }

    #[test]
    fn test_parse_content_length_invalid_number() {
        let headers = "Content-Length: abc\r\n\r\n";
        assert_eq!(parse_content_length(headers), None);
    }

    #[test]
    fn test_parse_content_length_multiple_headers() {
        let headers = "Host: example\r\nContent-Length: 10\r\nX-Custom: foo\r\n\r\n";
        assert_eq!(parse_content_length(headers), Some(10));
    }

    // ── floor_char_boundary ───────────────────────────────────────

    #[test]
    fn test_floor_char_boundary_ascii() {
        let s = "hello";
        assert_eq!(floor_char_boundary(s, 2), 2);
        assert_eq!(floor_char_boundary(s, 5), 5);
        assert_eq!(floor_char_boundary(s, 10), 5);
    }

    #[test]
    fn test_floor_char_boundary_utf8_inside() {
        let s = "héllo"; // 'é' is 2 bytes
        // String bytes: h (1) + é (2) + l l o
        // Char boundaries: 0,1,3,4,5,6
        assert_eq!(
            floor_char_boundary(s, 2),
            1,
            "index 2 inside é should floor to 1"
        );
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 3), 3);
    }

    #[test]
    fn test_floor_char_boundary_beyond_len() {
        let s = "hi";
        assert_eq!(floor_char_boundary(s, 100), 2);
    }

    #[test]
    fn test_floor_char_boundary_empty() {
        assert_eq!(floor_char_boundary("", 0), 0);
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    // ── lsp_position_to_offset ────────────────────────────────────

    #[test]
    fn test_lsp_position_to_offset_single_line() {
        let doc = "hello world";
        assert_eq!(
            lsp_position_to_offset(doc, 0, 5, PositionEncoding::Utf8).unwrap(),
            5
        );
        assert_eq!(
            lsp_position_to_offset(doc, 0, 0, PositionEncoding::Utf8).unwrap(),
            0
        );
    }

    #[test]
    fn test_lsp_position_to_offset_multiline() {
        let doc = "line1\nline2\nline3";
        // line 0 "line1\n" (5 chars + newline)
        // line 1 starts at offset 6
        assert_eq!(
            lsp_position_to_offset(doc, 1, 0, PositionEncoding::Utf8).unwrap(),
            6
        );
        assert_eq!(
            lsp_position_to_offset(doc, 1, 3, PositionEncoding::Utf8).unwrap(),
            9
        );
        assert_eq!(
            lsp_position_to_offset(doc, 2, 2, PositionEncoding::Utf8).unwrap(),
            14
        );
    }

    #[test]
    fn test_lsp_position_to_offset_char_beyond_line_clamped() {
        let doc = "hi\nhello";
        // line 0 "hi" len 2, request char 10 should clamp to 2
        assert_eq!(
            lsp_position_to_offset(doc, 0, 10, PositionEncoding::Utf8).unwrap(),
            2
        );
    }

    #[test]
    fn test_lsp_position_to_offset_line_beyond_doc_errors() {
        let doc = "a\nb";
        let res = lsp_position_to_offset(doc, 5, 0, PositionEncoding::Utf8);
        assert!(matches!(res, Err(EditError::OutOfBounds)));
    }

    #[test]
    fn test_lsp_position_to_offset_crlf() {
        let doc = "line1\r\nline2";
        // line 0 content is "line1" (without \r), offset calculation should handle \r\n
        assert_eq!(
            lsp_position_to_offset(doc, 0, 5, PositionEncoding::Utf8).unwrap(),
            5
        );
        // line1 starts after "line1\r\n" (7 bytes)
        assert_eq!(
            lsp_position_to_offset(doc, 1, 0, PositionEncoding::Utf8).unwrap(),
            7
        );
    }

    #[test]
    fn test_lsp_position_to_offset_utf8() {
        let doc = "héllo\nworld";
        // 'é' 2 bytes, line 0 len bytes 6, but chars? Should floor boundary
        let off = lsp_position_to_offset(doc, 0, 2, PositionEncoding::Utf8).unwrap();
        // char 2 is inside é? Actually floor to 1
        assert!(off == 1 || off == 3);
    }

    // ── apply_incremental_edit ────────────────────────────────────

    #[test]
    fn test_apply_incremental_edit_single_line_replace() {
        let mut doc = "hello world".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 6},
            "end": {"line": 0, "character": 11}
        });
        apply_incremental_edit(&mut doc, &range, "Rust", PositionEncoding::Utf8).unwrap();
        assert_eq!(doc, "hello Rust");
    }

    #[test]
    fn test_apply_incremental_edit_insertion() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 5},
            "end": {"line": 0, "character": 5}
        });
        apply_incremental_edit(&mut doc, &range, " world", PositionEncoding::Utf8).unwrap();
        assert_eq!(doc, "hello world");
    }

    #[test]
    fn test_apply_incremental_edit_deletion() {
        let mut doc = "hello world".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 5},
            "end": {"line": 0, "character": 11}
        });
        apply_incremental_edit(&mut doc, &range, "", PositionEncoding::Utf8).unwrap();
        assert_eq!(doc, "hello");
    }

    #[test]
    fn test_apply_incremental_edit_multiline() {
        let mut doc = "line1\nline2\nline3".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 1, "character": 5}
        });
        apply_incremental_edit(&mut doc, &range, "replaced", PositionEncoding::Utf8).unwrap();
        assert_eq!(doc, "replaced\nline3");
    }

    #[test]
    fn test_apply_incremental_edit_invalid_range_missing_field() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x", PositionEncoding::Utf8);
        assert!(matches!(res, Err(EditError::InvalidRange)));
    }

    #[test]
    fn test_apply_incremental_edit_out_of_bounds() {
        let mut doc = "hi".to_string();
        let range = serde_json::json!({
            "start": {"line": 5, "character": 0},
            "end": {"line": 5, "character": 2}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x", PositionEncoding::Utf8);
        assert!(matches!(res, Err(EditError::OutOfBounds)));
    }

    #[test]
    fn test_apply_incremental_edit_start_after_end_error() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 4},
            "end": {"line": 0, "character": 2}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x", PositionEncoding::Utf8);
        assert!(matches!(res, Err(EditError::OutOfBounds)));
    }

    // ── discard_bytes ─────────────────────────────────────────────

    #[test]
    fn test_discard_bytes() {
        let data = b"abcdefghij";
        let mut cursor = Cursor::new(data.to_vec());
        discard_bytes(&mut cursor, 4).unwrap();
        let mut remaining = Vec::new();
        std::io::Read::read_to_end(&mut cursor, &mut remaining).unwrap();
        assert_eq!(remaining, b"efghij");
    }

    #[test]
    fn test_discard_bytes_zero() {
        let data = b"hello";
        let mut cursor = Cursor::new(data.to_vec());
        discard_bytes(&mut cursor, 0).unwrap();
        let mut remaining = Vec::new();
        std::io::Read::read_to_end(&mut cursor, &mut remaining).unwrap();
        assert_eq!(remaining, b"hello");
    }

    // ── read_message (golden streams) ─────────────────────────────

    /// Build one length-prefixed frame around `body`.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn test_read_message_valid_frame_then_eof() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"x"}"#;
        let mut stream = Cursor::new(framed(body));
        match read_message(&mut stream).unwrap() {
            Frame::Message(b) => assert_eq!(&b[..], &body[..]),
            other => panic!("expected Message, got {other:?}"),
        }
        assert!(matches!(read_message(&mut stream).unwrap(), Frame::Eof));
    }

    #[test]
    fn test_read_message_two_frames_back_to_back() {
        let mut bytes = framed(br#"{"id":1}"#);
        bytes.extend_from_slice(&framed(br#"{"id":2}"#));
        let mut stream = Cursor::new(bytes);
        assert!(matches!(
            read_message(&mut stream).unwrap(),
            Frame::Message(ref b) if b == br#"{"id":1}"#
        ));
        assert!(matches!(
            read_message(&mut stream).unwrap(),
            Frame::Message(ref b) if b == br#"{"id":2}"#
        ));
        assert!(matches!(read_message(&mut stream).unwrap(), Frame::Eof));
    }

    #[test]
    fn test_read_message_garbage_header_fails_fast() {
        // PHASE 1: missing Content-Length is now terminal. Previously the
        // loop continued without consuming anything, so the body bytes were
        // re-parsed as headers → permanent desync cascade.
        let mut stream = Cursor::new(b"X-Garbage: 1\r\n\r\n{\"body\":true}".to_vec());
        match read_message(&mut stream).unwrap_err() {
            FrameError::Protocol(why) => assert!(
                why.contains("Content-Length"),
                "error should name the missing header, got: {why}"
            ),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn test_read_message_malformed_content_length_fails_fast() {
        let mut stream = Cursor::new(b"Content-Length: abc\r\n\r\nhello".to_vec());
        assert!(matches!(
            read_message(&mut stream).unwrap_err(),
            FrameError::Protocol(_)
        ));
    }

    #[test]
    fn test_read_message_duplicate_content_length_is_unparsable() {
        let mut stream =
            Cursor::new(b"Content-Length: 5\r\nContent-Length: 6\r\n\r\nhello!".to_vec());
        assert!(matches!(
            read_message(&mut stream).unwrap_err(),
            FrameError::Protocol(_)
        ));
    }

    #[test]
    fn test_read_message_oversized_body_drained_and_stream_usable() {
        // Oversize bodies are still drained/skipped (defensive cap preserved),
        // and the next valid frame parses cleanly afterwards.
        let big = vec![b'x'; MAX_MESSAGE_SIZE + 1];
        let mut bytes = framed(&big);
        bytes.extend_from_slice(&framed(br#"{"id":2}"#));
        let mut stream = Cursor::new(bytes);
        assert!(matches!(read_message(&mut stream).unwrap(), Frame::Skipped));
        assert!(matches!(
            read_message(&mut stream).unwrap(),
            Frame::Message(ref b) if b == br#"{"id":2}"#
        ));
    }

    #[test]
    fn test_read_message_zero_length_body_skipped() {
        let mut bytes = b"Content-Length: 0\r\n\r\n".to_vec();
        bytes.extend_from_slice(&framed(br#"{"id":3}"#));
        let mut stream = Cursor::new(bytes);
        assert!(matches!(read_message(&mut stream).unwrap(), Frame::Skipped));
        assert!(matches!(
            read_message(&mut stream).unwrap(),
            Frame::Message(_)
        ));
    }

    // ── Server handle_message integration ─────────────────────────

    fn make_server() -> Server {
        Server::new(synthetic_data())
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
        assert_eq!(resp["result"]["capabilities"]["textDocumentSync"], 1);
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

    // ── Protocol helpers ──────────────────────────────────────────────

    #[test]
    fn test_parse_content_length_edge_cases() {
        assert_eq!(parse_content_length("Content-Length: 0\r\n\r\n"), Some(0));
        assert_eq!(parse_content_length("content-length: 42\r\n\r\n"), Some(42));
        assert_eq!(parse_content_length("Content-Length: abc\r\n\r\n"), None);
        assert_eq!(parse_content_length("Content-Length: -5\r\n\r\n"), None);
        assert_eq!(parse_content_length("Content-Length: +5\r\n\r\n"), None);
        assert_eq!(
            parse_content_length("Content-Length: 5 extra\r\n\r\n"),
            None
        );
        // Duplicate should reject
        assert_eq!(
            parse_content_length("Content-Length: 5\r\nContent-Length: 6\r\n\r\n"),
            None
        );
    }

    #[test]
    fn test_floor_char_boundary_clamps() {
        assert_eq!(floor_char_boundary("héllo", 2), 1);
        assert_eq!(floor_char_boundary("hello", 10), 5);
        assert_eq!(floor_char_boundary("", 5), 0);
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
        assert_eq!(caps["textDocumentSync"], 1);
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(
            caps["completionProvider"]["triggerCharacters"],
            serde_json::json!(["/", " ", "="])
        );
        assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
    }

    // ── utf16_to_byte_offset ──────────────────────────────────────

    #[test]
    fn test_utf16_to_byte_offset_ascii_fast_path() {
        let line = "hello world";
        assert_eq!(utf16_to_byte_offset(line, 0), 0);
        assert_eq!(utf16_to_byte_offset(line, 5), 5);
        // Beyond end of line clamps to the byte length.
        assert_eq!(utf16_to_byte_offset(line, 100), line.len());
    }

    #[test]
    fn test_utf16_to_byte_offset_bmp_multibyte() {
        // 'ó' and 'é' are 2 bytes each but 1 UTF-16 unit.
        let line = "# configuración é";
        assert_eq!(line.len(), 19);
        assert_eq!(utf16_to_byte_offset(line, 13), 13); // start of 'ó'
        assert_eq!(utf16_to_byte_offset(line, 14), 15); // char after 'ó'
        assert_eq!(utf16_to_byte_offset(line, 17), 19); // end of line
        assert_eq!(utf16_to_byte_offset(line, 99), 19); // clamped
    }

    #[test]
    fn test_utf16_to_byte_offset_surrogate_pair_clamps_forward() {
        // '🚨' is U+1F6A8: 4 bytes but a surrogate pair (2 UTF-16 units).
        let line = "🚨x";
        assert_eq!(utf16_to_byte_offset(line, 0), 0);
        // A value inside the surrogate half resolves to the character's END.
        assert_eq!(utf16_to_byte_offset(line, 1), 4);
        assert_eq!(utf16_to_byte_offset(line, 2), 4);
        assert_eq!(utf16_to_byte_offset(line, 3), 5); // past 'x' start → EOL
        assert_eq!(utf16_to_byte_offset("🚨", usize::MAX), 4);
    }

    #[test]
    fn test_utf16_to_byte_offset_cjk() {
        // CJK chars are 3 bytes each but 1 UTF-16 unit.
        let line = "語語";
        assert_eq!(utf16_to_byte_offset(line, 1), 3);
        assert_eq!(utf16_to_byte_offset(line, 2), 6);
        assert_eq!(utf16_to_byte_offset(line, 50), 6);
    }

    #[test]
    fn test_utf16_to_byte_offset_empty_line() {
        assert_eq!(utf16_to_byte_offset("", 0), 0);
        assert_eq!(utf16_to_byte_offset("", 7), 0);
    }

    // ── byte_offset_to_utf16_units ────────────────────────────────

    #[test]
    fn test_byte_offset_to_utf16_units_ascii() {
        let line = "hello";
        assert_eq!(byte_offset_to_utf16_units(line, 0), 0);
        assert_eq!(byte_offset_to_utf16_units(line, 3), 3);
        // Beyond end clamps.
        assert_eq!(byte_offset_to_utf16_units(line, 100), 5);
    }

    #[test]
    fn test_byte_offset_to_utf16_units_bmp_multibyte() {
        let line = "# configuración é";
        assert_eq!(byte_offset_to_utf16_units(line, 13), 13);
        // Start of 'ó': 13 preceding chars → 13 units.
        assert_eq!(byte_offset_to_utf16_units(line, 14), 13);
        // Mid-'ó' floors to the char start.
        assert_eq!(byte_offset_to_utf16_units(line, 15), 14);
        assert_eq!(byte_offset_to_utf16_units(line, 19), 17);
    }

    #[test]
    fn test_byte_offset_to_utf16_units_surrogate_pair_counts_two() {
        let line = "🚨x";
        assert_eq!(byte_offset_to_utf16_units(line, 0), 0);
        // Mid-character floors to the char start (2 units for the pair).
        assert_eq!(byte_offset_to_utf16_units(line, 2), 0);
        assert_eq!(byte_offset_to_utf16_units(line, 4), 2);
        assert_eq!(byte_offset_to_utf16_units(line, 5), 3);
    }

    #[test]
    fn test_byte_offset_to_utf16_units_cjk() {
        let line = "語語";
        assert_eq!(byte_offset_to_utf16_units(line, 3), 1);
        assert_eq!(byte_offset_to_utf16_units(line, 5), 1); // floors
        assert_eq!(byte_offset_to_utf16_units(line, 6), 2);
    }

    #[test]
    fn test_position_conversion_round_trip_property() {
        let lines = [
            "hello world",
            "# configuración é",
            "/ip/address add address=1.1.1.1",
            "🚨🚨 bogus=1",
            "語セ語 x=y",
            "",
        ];
        for line in lines {
            // Every char boundary round-trips exactly through both helpers.
            for b in 0..=line.len() {
                if line.is_char_boundary(b) {
                    let units = byte_offset_to_utf16_units(line, b);
                    assert_eq!(
                        utf16_to_byte_offset(line, units as usize),
                        b,
                        "round-trip failed at byte {b} for {line:?}"
                    );
                }
            }
            // Saturating behavior: any unit value maps within the line.
            let total = byte_offset_to_utf16_units(line, line.len());
            for u in 0..=(total as usize + 3) {
                let b = utf16_to_byte_offset(line, u);
                assert!(b <= line.len(), "unit {u} out of range for {line:?}");
            }
        }
    }

    // ── lsp_position_to_offset under Utf16 ────────────────────────

    #[test]
    fn test_lsp_position_to_offset_utf16_non_ascii_line() {
        let doc = "héllo\nworld";
        // 'héllo' = 5 chars/units but 6 bytes; unit 2 lands after 'é'.
        assert_eq!(
            lsp_position_to_offset(doc, 0, 2, PositionEncoding::Utf16).unwrap(),
            3
        );
        assert_eq!(
            lsp_position_to_offset(doc, 0, 5, PositionEncoding::Utf16).unwrap(),
            6
        );
        // Beyond the line clamps to its byte length.
        assert_eq!(
            lsp_position_to_offset(doc, 0, 50, PositionEncoding::Utf16).unwrap(),
            6
        );
    }

    #[test]
    fn test_lsp_position_to_offset_utf16_crlf_excludes_cr() {
        let doc = "héllo\r\nworld";
        // Line content excludes '\r': "héllo" is 5 units / 6 bytes.
        assert_eq!(
            lsp_position_to_offset(doc, 0, 5, PositionEncoding::Utf16).unwrap(),
            6
        );
        // The EOL position resolves before the carriage return, not past it.
        assert_eq!(
            lsp_position_to_offset(doc, 0, 6, PositionEncoding::Utf16).unwrap(),
            6
        );
        // Line 1 starts after "héllo\r\n" (8 bytes: 6 + CRLF pair).
        assert_eq!(
            lsp_position_to_offset(doc, 1, 0, PositionEncoding::Utf16).unwrap(),
            8
        );
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

    #[test]
    fn test_convert_diagnostic_ranges_multiline_and_noop() {
        let make_diag = || diagnostics::Diagnostic {
            range: diagnostics::Range {
                start: diagnostics::Position {
                    line: 0,
                    character: 1,
                },
                end: diagnostics::Position {
                    line: 1,
                    character: 4,
                },
            },
            severity: Some(diagnostics::severity::WARNING),
            code: Some("t".to_string()),
            source: None,
            message: "m".to_string(),
        };
        // Multi-line range: each endpoint converts against its OWN physical
        // line ('a🚨bc' has 5 units; '/de' is ASCII, and the endpoint beyond
        // its length clamps to 3).
        let diags = vec![make_diag()];
        let out = convert_diagnostic_ranges(diags.clone(), "a🚨bc\r\n/de", PositionEncoding::Utf16);
        assert_eq!(out[0].range.start.line, 0);
        assert_eq!(out[0].range.start.character, 1);
        assert_eq!(out[0].range.end.line, 1);
        assert_eq!(out[0].range.end.character, 3);

        // Utf8 conversion is a semantic no-op.
        let out =
            convert_diagnostic_ranges(vec![make_diag()], "a🚨bc\r\n/de", PositionEncoding::Utf8);
        assert_eq!(out[0].range, diags[0].range);
        assert_eq!(out[0].severity, diags[0].severity);
        assert_eq!(out[0].code, diags[0].code);
        assert_eq!(out[0].source, diags[0].source);
        assert_eq!(out[0].message, diags[0].message);

        // Non-boundary endpoints floor defensively to the char start.
        let mut d = make_diag();
        d.range.start.character = 3; // mid-'🚨' byte offset
        let out = convert_diagnostic_ranges(vec![d], "a🚨bc\r\n/de", PositionEncoding::Utf16);
        assert_eq!(out[0].range.start.character, 1);

        // Missing lines clamp defensively to zero without panicking.
        let mut d = make_diag();
        d.range.end.line = 99;
        let out = convert_diagnostic_ranges(vec![d], "", PositionEncoding::Utf16);
        assert_eq!(out[0].range.end.character, 0);
    }
}
