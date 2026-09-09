//! Wire/chaos gates for the `rsc-ls` language server.
//!
//! Companion to `e2e.rs` (which owns the happy-path wire truths): this
//! file is Stream D territory — hostile transports and rename wire
//! shapes. It spawns the REAL binary via `CARGO_BIN_EXE_rsc-ls` and
//! speaks raw Content-Length framed JSON-RPC over stdio with a
//! deliberately tiny std-only client (threads + `mpsc` +
//! `serde_json`). Every wait is bounded by [`RECV_TIMEOUT`] so a wedged
//! server fails fast instead of hanging CI.
//!
//! Covered (synthetic docs only, no device, no filesystem):
//! - chunked writes (1-byte and 7-byte) still frame correctly
//! - LF-only (`\n`) frame terminators are accepted
//! - 32 KiB header boundary: exactly 32 KiB parses, 32 KiB+1 with a
//!   Content-Length drains-and-skips while staying aligned, 32 KiB+1
//!   without one terminates the server (exit code 1)
//! - pipelined [oversized body -> valid frame] stays aligned
//! - garbage-JSON body then a valid frame recovers (JSON errors are
//!   non-terminal)
//! - rename over the wire: valid declaration rename returns a
//!   single-document `changes` map with sigil preservation; an invalid
//!   new name returns null; malformed params return -32602 echoing id
//!
//! Runs on Linux, macOS and Windows via plain `cargo test -p rsc-ls`.
//! Set `RSC_LS_E2E_STDERR=1` to inherit the server's stderr while
//! debugging.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_rsc-ls");

/// Upper bound on ANY single wait for server output.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Mirror of the server's header cap (`caps::MAX_HEADER_SIZE`).
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Mirror of the server's body cap (`caps::MAX_MESSAGE_SIZE`).
const MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

/// Defensive cap on one response body before allocation.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

enum Response {
    Ok(Value),
    Err(Value),
}

struct ChaosClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    pending: VecDeque<Value>,
    next_id: i64,
}

impl ChaosClient {
    fn spawn() -> Self {
        let stderr = if std::env::var_os("RSC_LS_E2E_STDERR").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(BIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("failed to spawn rsc-ls binary");
        let stdin = child.stdin.take().expect("child stdin was piped");
        let stdout = child.stdout.take().expect("child stdout was piped");
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("rsc-ls-chaos-reader".into())
            .spawn(move || pump_frames(stdout, tx))
            .expect("failed to spawn reader thread");
        ChaosClient {
            child,
            stdin,
            rx,
            pending: VecDeque::new(),
            next_id: 1,
        }
    }

    fn initialize(&mut self) -> Value {
        let result = match self.request(
            "initialize",
            json!({"processId": null, "rootUri": null, "capabilities": {}}),
        ) {
            Response::Ok(result) => result,
            Response::Err(err) => panic!("initialize failed: {err}"),
        };
        self.notify("initialized", json!({}));
        result
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send_bytes(&framed(
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }))
            .expect("message serializes"),
        ));
    }

    fn request(&mut self, method: &str, params: Value) -> Response {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        self.request_with_id(id, method, params)
    }

    fn request_with_id(&mut self, id: Value, method: &str, params: Value) -> Response {
        self.send_bytes(&framed(
            &serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .expect("message serializes"),
        ));
        let msg = self.wait_for_response(&id, method);
        if let Some(err) = msg.get("error") {
            return Response::Err(err.clone());
        }
        Response::Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Write raw bytes; used for chunked/chaos sends.
    fn send_bytes(&mut self, bytes: &[u8]) {
        self.stdin
            .write_all(bytes)
            .expect("writing raw frame bytes");
        self.stdin.flush().expect("flushing frame");
    }

    /// Write raw bytes in `chunk`-sized pieces, flushing each piece, to
    /// prove the server reassembles arbitrarily split TCP-style streams.
    fn send_bytes_chunked(&mut self, bytes: &[u8], chunk: usize) {
        for piece in bytes.chunks(chunk) {
            self.stdin.write_all(piece).expect("writing frame chunk");
            self.stdin.flush().expect("flushing frame chunk");
        }
    }

    /// Next notification with `method`, preserving anything else.
    fn expect_notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self
            .pending
            .iter()
            .position(|m| is_notification_of(m, method))
        {
            return self.pending.remove(pos).expect("position came from len");
        }
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(msg) => {
                    if is_notification_of(&msg, method) {
                        return msg;
                    }
                    self.pending.push_back(msg);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out after {RECV_TIMEOUT:?} waiting for `{method}` notification")
                }
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "server closed stdout while awaiting `{method}` notification \
                     (crashed or exited early)"
                ),
            }
        }
    }

    fn wait_for_response(&mut self, id: &Value, method: &str) -> Value {
        if let Some(pos) = self.pending.iter().position(|m| is_response_to(m, id)) {
            return self.pending.remove(pos).expect("position came from len");
        }
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(msg) => {
                    if is_response_to(&msg, id) {
                        return msg;
                    }
                    self.pending.push_back(msg);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!(
                        "timed out after {RECV_TIMEOUT:?} waiting for `{method}` response id={id}"
                    )
                }
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "server closed stdout while awaiting `{method}` response id={id} \
                     (crashed or exited early)"
                ),
            }
        }
    }

    /// Bounded wait for process termination.
    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + RECV_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("polling child status") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("rsc-ls still running after {RECV_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChaosClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_response_to(msg: &Value, id: &Value) -> bool {
    msg.get("id") == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some())
}

fn is_notification_of(msg: &Value, method: &str) -> bool {
    msg.get("id").is_none() && msg.get("method").and_then(Value::as_str) == Some(method)
}

/// One CRLF-framed message for `body`.
fn framed(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// The standard `initialize` request body carrying `id`.
fn initialize_body(id: &Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {"processId": null, "rootUri": null, "capabilities": {}},
    }))
    .expect("initialize serializes")
}

fn pump_frames(stdout: ChildStdout, tx: Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader) {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(err) => {
                eprintln!("rsc-ls chaos reader terminated: {err}");
                return;
            }
        }
    }
}

fn read_frame<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            if content_length.is_none() && line.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::other("EOF inside a frame"));
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(std::io::Error::other("header section exceeds 32 KiB"));
        }
        match line.trim_end_matches(['\r', '\n']) {
            "" => break,
            header => {
                if let Some((name, value)) = header.split_once(':')
                    && name.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse().map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("malformed Content-Length: {value:?}"),
                        )
                    })?);
                }
            }
        }
    }
    let Some(len) = content_length else {
        return Err(std::io::Error::other("frame without Content-Length"));
    };
    if len > MAX_BODY_BYTES {
        return Err(std::io::Error::other(format!(
            "Content-Length {len} exceeds sanity cap {MAX_BODY_BYTES}"
        )));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn open_text_document(client: &mut ChaosClient, uri: &str, text: &str) {
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rsc",
                "version": 1,
                "text": text,
            },
        }),
    );
}

// ── Chunked transport ────────────────────────────────────────────

#[test]
fn framing_chunked_1byte_writes_still_frame_initialize() {
    let mut client = ChaosClient::spawn();
    let body = initialize_body(&json!(1));
    client.send_bytes_chunked(&framed(&body), 1);
    let msg = client.wait_for_response(&json!(1), "initialize");
    assert!(
        msg.get("result").is_some(),
        "1-byte-chunked initialize must be answered, got {msg}"
    );
    assert_eq!(msg["id"], 1, "id must be echoed verbatim");
}

#[test]
fn framing_chunked_7byte_writes_still_frame_initialize() {
    let mut client = ChaosClient::spawn();
    let body = initialize_body(&json!(2));
    client.send_bytes_chunked(&framed(&body), 7);
    let msg = client.wait_for_response(&json!(2), "initialize");
    assert!(
        msg.get("result").is_some(),
        "7-byte-chunked initialize must be answered, got {msg}"
    );
    assert_eq!(msg["id"], 2, "id must be echoed verbatim");
}

#[test]
fn framing_lf_only_frame_is_accepted() {
    let mut client = ChaosClient::spawn();
    let body = initialize_body(&json!(3));
    let mut raw = format!("Content-Length: {}\n\n", body.len()).into_bytes();
    raw.extend_from_slice(body.as_bytes());
    client.send_bytes(&raw);
    let msg = client.wait_for_response(&json!(3), "initialize");
    assert!(
        msg.get("result").is_some(),
        "LF-only frame must be accepted, got {msg}"
    );
}

// ── 32 KiB header boundary ───────────────────────────────────────

/// Header section of EXACTLY `total` bytes: a valid Content-Length line,
/// one `X-Pad` line absorbing the remainder, and the blank terminator.
fn padded_header_frame(body: &str, total: usize) -> Vec<u8> {
    let first = format!("Content-Length: {}\r\n", body.len());
    // `X-Pad: <A*K>\r\n` is 9 + K bytes; blank line is 2 bytes.
    let pad_len = total
        .checked_sub(first.len() + 9 + 2)
        .expect("total too small for a padded header");
    let mut out = first.into_bytes();
    out.extend_from_slice(format!("X-Pad: {}\r\n", "A".repeat(pad_len)).as_bytes());
    out.extend_from_slice(b"\r\n");
    assert_eq!(
        out.len(),
        total,
        "padded header must be exactly {total} bytes"
    );
    out.extend_from_slice(body.as_bytes());
    out
}

#[test]
fn framing_header_exactly_32kib_is_accepted() {
    let mut client = ChaosClient::spawn();
    let body = initialize_body(&json!(11));
    client.send_bytes(&padded_header_frame(&body, MAX_HEADER_BYTES));
    let msg = client.wait_for_response(&json!(11), "initialize");
    assert!(
        msg.get("result").is_some(),
        "a header section of exactly 32 KiB must parse, got {msg}"
    );
}

#[test]
fn framing_header_32kib_plus_one_with_length_skips_and_stays_aligned() {
    let mut client = ChaosClient::spawn();
    // Oversized-but-parsable headers: the message is drained and
    // skipped, and the NEXT pipelined frame parses cleanly.
    let mut bytes = padded_header_frame("{}", MAX_HEADER_BYTES + 1);
    bytes.extend_from_slice(&framed(&initialize_body(&json!(12))));
    client.send_bytes(&bytes);
    let msg = client.wait_for_response(&json!(12), "initialize");
    assert!(
        msg.get("result").is_some(),
        "stream must stay aligned after a skipped oversized block, got {msg}"
    );
}

#[test]
fn framing_header_32kib_plus_one_without_length_terminates() {
    let mut client = ChaosClient::spawn();
    // Oversized headers WITHOUT any Content-Length are terminal: the
    // stream cannot be resynchronized, so the server exits code 1.
    let pad_len = (MAX_HEADER_BYTES + 1) - ("X-Junk: \r\n".len() + 2);
    let mut bytes = format!("X-Junk: {}\r\n", "B".repeat(pad_len)).into_bytes();
    bytes.extend_from_slice(b"\r\n");
    assert_eq!(bytes.len(), MAX_HEADER_BYTES + 1);
    client.send_bytes(&bytes);
    let status = client.wait_for_exit();
    assert_eq!(
        status.code(),
        Some(1),
        "unparsable oversized headers must terminate with code 1, got {status:?}"
    );
}

// ── Oversized body + garbage recovery ────────────────────────────

#[test]
fn framing_oversized_then_valid_pipelined_stays_aligned() {
    let mut client = ChaosClient::spawn();
    let big_len = MAX_MESSAGE_BYTES + 1;
    let mut bytes = format!("Content-Length: {big_len}\r\n\r\n").into_bytes();
    bytes.extend_from_slice(&vec![b'x'; big_len]);
    bytes.extend_from_slice(&framed(&initialize_body(&json!(21))));
    client.send_bytes(&bytes);
    let msg = client.wait_for_response(&json!(21), "initialize");
    assert!(
        msg.get("result").is_some(),
        "valid frame after an oversized body must parse, got keys {:?}",
        msg.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
}

#[test]
fn framing_garbage_body_then_valid_recovers() {
    let mut client = ChaosClient::spawn();
    // A well-framed but non-JSON body is a non-terminal JSON error: the
    // server warns and keeps serving.
    let mut bytes = framed("{this is not json");
    bytes.extend_from_slice(&framed(&initialize_body(&json!(22))));
    client.send_bytes(&bytes);
    let msg = client.wait_for_response(&json!(22), "initialize");
    assert!(
        msg.get("result").is_some(),
        "server must recover after a garbage body, got {msg}"
    );
}

// ── Rename over the wire ─────────────────────────────────────────

const WIRE_RENAME_DOC: &str = ":local wan \"ether1\"\n:put $wan\n";

#[test]
fn wire_rename_valid_declaration_returns_single_doc_changes_with_sigil() {
    let mut client = ChaosClient::spawn();
    client.initialize();
    let uri = "file:///wire-rename-e2e.rsc";
    open_text_document(&mut client, uri, WIRE_RENAME_DOC);
    let result = match client.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 8},
            "newName": "uplink",
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("rename errored: {err}"),
    };
    let changes = result["changes"].as_object().expect("changes map");
    assert_eq!(changes.len(), 1, "v1 rename stays in one document");
    let edits = changes[uri].as_array().expect("edits array");
    assert_eq!(edits.len(), 2, "declaration + usage, got {result}");
    assert!(edits.iter().all(|e| e["newText"] == "uplink"));
    // Declaration covers exactly `wan`; usage excludes the `$` sigil.
    assert_eq!(
        edits[0]["range"]["start"],
        json!({"line": 0, "character": 7})
    );
    assert_eq!(
        edits[0]["range"]["end"],
        json!({"line": 0, "character": 10})
    );
    assert_eq!(
        edits[1]["range"]["start"],
        json!({"line": 1, "character": 6})
    );
    assert_eq!(edits[1]["range"]["end"], json!({"line": 1, "character": 9}));
}

#[test]
fn wire_rename_invalid_name_returns_null_and_malformed_returns_32602() {
    let mut client = ChaosClient::spawn();
    client.initialize();
    let uri = "file:///wire-rename-e2e-invalid.rsc";
    open_text_document(&mut client, uri, WIRE_RENAME_DOC);
    // Unusable new name → null result (nothing honest to apply).
    let result = match client.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 8},
            "newName": "bad name",
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("invalid new name must yield null, not an error: {err}"),
    };
    assert!(
        result.is_null(),
        "invalid new name must return null, got {result}"
    );
    // Missing newName → -32602 with the (string) id echoed verbatim.
    match client.request_with_id(
        json!("wire-rename-str-id"),
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 8},
        }),
    ) {
        Response::Err(err) => assert_eq!(err["code"], -32602),
        Response::Ok(v) => panic!("malformed rename must error, got {v}"),
    }
    // Off-target cursor (menu text, no variable) → null result.
    let _ = client.expect_notification("textDocument/publishDiagnostics");
    let plain = match client.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 1, "character": 1},
            "newName": "other",
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("off-target rename must yield null: {err}"),
    };
    assert!(
        plain.is_null(),
        "off-target cursor must return null, got {plain}"
    );
}
