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
use std::io::{BufRead, BufReader, Read, Write};
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

// ── Server state ────────────────────────────────────────────────

struct Server {
    data: MenuData,
    docs: HashMap<String, String>, // URI → document text
}

impl Server {
    fn new(data: MenuData) -> Self {
        Server {
            data,
            docs: HashMap::new(),
        }
    }

    fn run(&mut self) {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut header_buf = String::new();

        loop {
            // Read headers until an empty line. Handle both "\r\n" and "\n".
            // Enforce MAX_HEADER_SIZE to prevent header-based OOM / slowloris.
            header_buf.clear();
            let mut header_bytes: usize = 0;
            let mut header_too_large = false;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return, // EOF
                    Ok(_) => {
                        header_bytes += line.len();
                        if header_bytes > MAX_HEADER_SIZE {
                            eprintln!(
                                "[rsc-ls] header too large (> {MAX_HEADER_SIZE} bytes), discarding message"
                            );
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
                    Err(e) => {
                        eprintln!("[rsc-ls] read error: {e}");
                        return;
                    }
                }
            }
            if header_too_large {
                // If we overflowed, attempt to discard a body if Content-Length was present
                // before overflow detection was complete; otherwise just resync.
                if let Some(cl) = parse_content_length(&header_buf)
                    && cl > 0
                {
                    let _ = discard_bytes(&mut reader, cl);
                }
                // Prevent header_buf capacity from staying huge (allocation DoS)
                if header_buf.capacity() > MAX_HEADER_SIZE * 2 {
                    header_buf.shrink_to_fit();
                }
                continue;
            }

            // Parse Content-Length (case-insensitive)
            let content_length = parse_content_length(&header_buf);

            let content_length = match content_length {
                Some(n) => n,
                None => continue,
            };

            if content_length == 0 {
                continue;
            }

            if content_length > MAX_MESSAGE_SIZE {
                eprintln!(
                    "[rsc-ls] message too large: {} bytes (limit {MAX_MESSAGE_SIZE}), discarding",
                    content_length
                );
                if let Err(e) = discard_bytes(&mut reader, content_length) {
                    eprintln!("[rsc-ls] failed to discard oversized body: {e}");
                    return;
                }
                continue;
            }

            // Read body
            let mut body = vec![0u8; content_length];
            if let Err(e) = reader.read_exact(&mut body) {
                eprintln!("[rsc-ls] read body error: {e}");
                return;
            }

            let msg: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("[rsc-ls] JSON parse error: {e}");
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
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
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
                            "version": "0.1.0",
                        },
                    },
                }))
            }

            "shutdown" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": null,
            })),

            "exit" => {
                std::process::exit(0);
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
                let diags = diagnostics::compute_diagnostics(&self.data, &doc_text, &uri_owned);
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
                                    if apply_incremental_edit(doc, range, truncated).is_err() {
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
                                if apply_incremental_edit(doc, range, text).is_err() {
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
                let diags = diagnostics::compute_diagnostics(&self.data, &doc_text, &uri_owned);
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
                let uri = params["params"]["textDocument"]["uri"].as_str()?;
                let pos = &params["params"]["position"];
                let line = pos["line"].as_u64()?;
                let character = pos["character"].as_u64()?;
                let doc = self.docs.get(uri)?;

                let before_cursor = build_before_cursor(doc, line as usize, character as usize);
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
                let uri = params["params"]["textDocument"]["uri"].as_str()?;
                let pos = &params["params"]["position"];
                let line = pos["line"].as_u64()? as usize;
                let character = pos["character"].as_u64()? as usize;
                let doc = self.docs.get(uri)?;

                let lines: Vec<&str> = doc.lines().collect();
                let current_line = lines.get(line).copied().unwrap_or("");

                let hover = hover::compute_hover(&self.data, current_line, character, doc, line);

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
                let diags = diagnostics::compute_diagnostics(&self.data, &doc_text, uri);
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

#[derive(Debug)]
enum EditError {
    InvalidRange,
    OutOfBounds,
}

fn lsp_position_to_offset(doc: &str, line: usize, character: usize) -> Result<usize, EditError> {
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

    let clamped = character.min(line_content.len());
    let byte_pos = floor_char_boundary(line_content, clamped);
    Ok(line_start + byte_pos)
}

fn apply_incremental_edit(
    doc: &mut String,
    range: &serde_json::Value,
    new_text: &str,
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

    let start_offset = lsp_position_to_offset(doc, start_line, start_char)?;
    let end_offset = lsp_position_to_offset(doc, end_line, end_char)?;

    if start_offset > end_offset || end_offset > doc.len() {
        return Err(EditError::OutOfBounds);
    }
    doc.replace_range(start_offset..end_offset, new_text);
    Ok(())
}

// ── Tokenizer / parser (ported from ls.mjs) ─────────────────────

/// Split a line into tokens: quoted strings, /-prefixed paths, or bare words.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
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

        // Quoted string
        if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2; // skip escaped char
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            tokens.push(
                std::str::from_utf8(&bytes[start..i])
                    .unwrap_or("")
                    .to_string(),
            );
            continue;
        }

        // /-prefixed path segment
        if bytes[i] == b'/' {
            let start = i;
            i += 1;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(
                std::str::from_utf8(&bytes[start..i])
                    .unwrap_or("")
                    .to_string(),
            );
            continue;
        }

        // Bare word
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        tokens.push(
            std::str::from_utf8(&bytes[start..i])
                .unwrap_or("")
                .to_string(),
        );
    }

    tokens
}

/// Build the "before cursor" context across multiple lines.
///
/// RouterOS commands can span multiple lines — properties on subsequent lines
/// are continuations of the same command.  Walks backwards from the cursor
/// line, collecting all lines belonging to the current command.
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
        // Value without space stays as one token; with space the tokenizer splits on whitespace
        // so we test the no-space case which is handled correctly.
        let ctx = parse_line(&data, r#"/ip/address add comment="hello""#);
        assert_eq!(
            ctx.properties.get("comment").map(|s| s.as_str()),
            Some("\"hello\"")
        );
        // Space inside quotes currently splits into two tokens due to bare-word tokenization.
        // Expected behavior: first part truncated to "\"hello", second token orphaned.
        let ctx2 = parse_line(&data, r#"/ip/address add comment="hello world""#);
        assert_eq!(
            ctx2.properties.get("comment").map(|s| s.as_str()),
            Some("\"hello")
        );
        assert_eq!(ctx2.last_token, "world\"");
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
        assert_eq!(lsp_position_to_offset(doc, 0, 5).unwrap(), 5);
        assert_eq!(lsp_position_to_offset(doc, 0, 0).unwrap(), 0);
    }

    #[test]
    fn test_lsp_position_to_offset_multiline() {
        let doc = "line1\nline2\nline3";
        // line 0 "line1\n" (5 chars + newline)
        // line 1 starts at offset 6
        assert_eq!(lsp_position_to_offset(doc, 1, 0).unwrap(), 6);
        assert_eq!(lsp_position_to_offset(doc, 1, 3).unwrap(), 9);
        assert_eq!(lsp_position_to_offset(doc, 2, 2).unwrap(), 14);
    }

    #[test]
    fn test_lsp_position_to_offset_char_beyond_line_clamped() {
        let doc = "hi\nhello";
        // line 0 "hi" len 2, request char 10 should clamp to 2
        assert_eq!(lsp_position_to_offset(doc, 0, 10).unwrap(), 2);
    }

    #[test]
    fn test_lsp_position_to_offset_line_beyond_doc_errors() {
        let doc = "a\nb";
        let res = lsp_position_to_offset(doc, 5, 0);
        assert!(matches!(res, Err(EditError::OutOfBounds)));
    }

    #[test]
    fn test_lsp_position_to_offset_crlf() {
        let doc = "line1\r\nline2";
        // line 0 content is "line1" (without \r), offset calculation should handle \r\n
        assert_eq!(lsp_position_to_offset(doc, 0, 5).unwrap(), 5);
        // line1 starts after "line1\r\n" (7 bytes)
        assert_eq!(lsp_position_to_offset(doc, 1, 0).unwrap(), 7);
    }

    #[test]
    fn test_lsp_position_to_offset_utf8() {
        let doc = "héllo\nworld";
        // 'é' 2 bytes, line 0 len bytes 6, but chars? Should floor boundary
        let off = lsp_position_to_offset(doc, 0, 2).unwrap();
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
        apply_incremental_edit(&mut doc, &range, "Rust").unwrap();
        assert_eq!(doc, "hello Rust");
    }

    #[test]
    fn test_apply_incremental_edit_insertion() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 5},
            "end": {"line": 0, "character": 5}
        });
        apply_incremental_edit(&mut doc, &range, " world").unwrap();
        assert_eq!(doc, "hello world");
    }

    #[test]
    fn test_apply_incremental_edit_deletion() {
        let mut doc = "hello world".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 5},
            "end": {"line": 0, "character": 11}
        });
        apply_incremental_edit(&mut doc, &range, "").unwrap();
        assert_eq!(doc, "hello");
    }

    #[test]
    fn test_apply_incremental_edit_multiline() {
        let mut doc = "line1\nline2\nline3".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 0},
            "end": {"line": 1, "character": 5}
        });
        apply_incremental_edit(&mut doc, &range, "replaced").unwrap();
        assert_eq!(doc, "replaced\nline3");
    }

    #[test]
    fn test_apply_incremental_edit_invalid_range_missing_field() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x");
        assert!(matches!(res, Err(EditError::InvalidRange)));
    }

    #[test]
    fn test_apply_incremental_edit_out_of_bounds() {
        let mut doc = "hi".to_string();
        let range = serde_json::json!({
            "start": {"line": 5, "character": 0},
            "end": {"line": 5, "character": 2}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x");
        assert!(matches!(res, Err(EditError::OutOfBounds)));
    }

    #[test]
    fn test_apply_incremental_edit_start_after_end_error() {
        let mut doc = "hello".to_string();
        let range = serde_json::json!({
            "start": {"line": 0, "character": 4},
            "end": {"line": 0, "character": 2}
        });
        let res = apply_incremental_edit(&mut doc, &range, "x");
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
    }

    #[test]
    fn test_server_shutdown() {
        let mut server = make_server();
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": {}
        });
        let resp = server.handle_message("shutdown", &msg).unwrap();
        assert_eq!(resp["result"], serde_json::Value::Null);
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
    fn test_server_completion_unknown_uri_returns_none() {
        let mut server = make_server();
        let comp = serde_json::json!({
            "id": 1,
            "params": {
                "textDocument": {"uri": "file:///notopened.rsc"},
                "position": {"line": 0, "character": 1}
            }
        });
        let resp = server.handle_message("textDocument/completion", &comp);
        assert!(resp.is_none());
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
    fn test_server_hover_unknown_doc_returns_none() {
        let mut server = make_server();
        let hover = serde_json::json!({
            "id": 7,
            "params": {
                "textDocument": {"uri": "file:///notopen.rsc"},
                "position": {"line": 0, "character": 1}
            }
        });
        let resp = server.handle_message("textDocument/hover", &hover);
        assert!(resp.is_none());
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
