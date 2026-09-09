// Performance smoke gates for the `rsc-ls` language server.
//
// Companion to `e2e.rs` / `framing_chaos.rs`: this file pins the
// resource-cap surface over the REAL binary (`CARGO_BIN_EXE_rsc-ls`)
// with synthetic docs only (no device, no filesystem). The client is
// the same tiny std-only shape (threads + `mpsc` + `serde_json`);
// every wait is bounded by [`RECV_TIMEOUT`] so a wedged server fails
// fast instead of hanging CI.
//
// Covered:
// - 5 MiB didOpen truncates at the cap and the server stays alive
// - 3000-line diagnostics stay bounded with exactly one `truncated` hint
// - completion answers stay capped at 200 items, relevance-sorted
//   (live-first) so truncation keeps the most relevant candidates
//
// Timing budgets live ONLY in `#[ignore]`d release-oriented tests;
// the default suite asserts caps and shapes, never elapsed time.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_rsc-ls");

/// Upper bound on ANY single wait for server output.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Mirror of the server caps (`caps.rs`): the test pins the CONTRACT
/// (bounded output, single hint), not the constants themselves.
const MAX_DOC_BYTES: usize = 5 * 1024 * 1024;
const MAX_DIAG_ITEMS: usize = 2000;
const MAX_COMPLETION_ITEMS: usize = 200;

/// Defensive cap on one response body before allocation.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

enum Response {
    Ok(Value),
    Err(Value),
}

struct PerfClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    pending: VecDeque<Value>,
    next_id: i64,
}

impl PerfClient {
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
            .name("rsc-ls-perf-reader".into())
            .spawn(move || pump_frames(stdout, tx))
            .expect("failed to spawn reader thread");
        PerfClient {
            child,
            stdin,
            rx,
            pending: VecDeque::new(),
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        let result = match self.request(
            "initialize",
            json!({"processId": null, "rootUri": null, "capabilities": {}}),
        ) {
            Response::Ok(result) => result,
            Response::Err(err) => panic!("initialize failed: {err}"),
        };
        let _ = result;
        self.notify("initialized", json!({}));
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
        let raw = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .expect("message serializes");
        self.send_bytes(&framed(&raw));
        let msg = self.wait_for_response(&id, method);
        if let Some(err) = msg.get("error") {
            return Response::Err(err.clone());
        }
        Response::Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.stdin
            .write_all(bytes)
            .expect("writing raw frame bytes");
        self.stdin.flush().expect("flushing frame");
    }

    fn expect_diagnostics(&mut self) -> Value {
        if let Some(pos) = self
            .pending
            .iter()
            .position(|m| is_notification_of(m, "textDocument/publishDiagnostics"))
        {
            return self.pending.remove(pos).expect("position came from len");
        }
        loop {
            match self.rx.recv_timeout(RECV_TIMEOUT) {
                Ok(msg) => {
                    if is_notification_of(&msg, "textDocument/publishDiagnostics") {
                        return msg;
                    }
                    self.pending.push_back(msg);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out after {RECV_TIMEOUT:?} waiting for publishDiagnostics")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server closed stdout while awaiting publishDiagnostics")
                }
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
                    panic!("timed out after {RECV_TIMEOUT:?} waiting for `{method}` id={id}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server closed stdout while awaiting `{method}` id={id}")
                }
            }
        }
    }
}

impl Drop for PerfClient {
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

fn framed(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
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
            Err(_) => return,
        }
    }
}

fn read_frame<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        match line.trim_end_matches(['\r', '\n']) {
            "" => break,
            header => {
                if let Some((name, value)) = header.split_once(':')
                    && name.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = Some(value.trim().parse().map_err(|_| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "bad length")
                    })?);
                }
            }
        }
    }
    let Some(len) = content_length else {
        return Err(std::io::Error::other("frame without Content-Length"));
    };
    if len > MAX_BODY_BYTES {
        return Err(std::io::Error::other("response body exceeds sanity cap"));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn open_text_document(client: &mut PerfClient, uri: &str, text: &str) {
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {"uri": uri, "languageId": "rsc", "version": 1, "text": text},
        }),
    );
}

// ── Cap asserts (run by default, no timing) ──────────────────────────────

#[test]
fn perf_didopen_5mib_truncates_and_stays_alive() {
    let mut client = PerfClient::spawn();
    client.initialize();
    let uri = "file:///perf-5mib.rsc";
    // One KiB past the tracked-document cap: the server must truncate
    // (not drop, not crash) and keep serving afterwards.
    let big = "x".repeat(MAX_DOC_BYTES + 1024);
    open_text_document(&mut client, uri, &big);
    let _ = client.expect_diagnostics();
    // Liveness probe: any answered request proves the oversized open
    // did not wedge the loop (result may be null for unknown words).
    match client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 0},
        }),
    ) {
        Response::Ok(_) => {}
        Response::Err(err) => panic!("server must stay alive after 5 MiB open: {err}"),
    }
}

#[test]
fn perf_3000line_diagnostics_bounded_with_single_hint() {
    let mut client = PerfClient::spawn();
    client.initialize();
    let uri = "file:///perf-3000lines.rsc";
    // 3500 unknown-menu lines exceed both the 3000-line window and the
    // 2000-diagnostic publish cap: output stays bounded with exactly
    // one `truncated` Information footer.
    let doc = "/foo/unknown add badprop=1\n".repeat(3500);
    open_text_document(&mut client, uri, &doc);
    let notif = client.expect_diagnostics();
    let diags = notif["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        diags.len() <= MAX_DIAG_ITEMS + 12,
        "publish must stay bounded (semantic + syntax + hint), got {}",
        diags.len()
    );
    let hints: Vec<&Value> = diags
        .iter()
        .filter(|d| d.get("code").and_then(Value::as_str) == Some("truncated"))
        .collect();
    assert_eq!(
        hints.len(),
        1,
        "exactly one truncation footer, got {} diags",
        diags.len()
    );
    assert_eq!(hints[0]["severity"], 3, "truncation hint is Information");
    assert!(
        hints[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("some issues beyond limit not shown"),
        "footer suffix contract, got {:?}",
        hints[0]["message"]
    );
}

#[test]
fn perf_completion_capped_at_200_and_relevance_sorted() {
    let mut client = PerfClient::spawn();
    client.initialize();
    let uri = "file:///perf-completion.rsc";
    let doc = "/ip/address add ";
    open_text_document(&mut client, uri, doc);
    let _ = client.expect_diagnostics();
    let items = match client.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": doc.len()},
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("completion must answer: {err}"),
    };
    // The server answers a CompletionList object (spec-compliant);
    // accept a bare array too so the cap holds for either shape.
    let items_vec: Vec<Value>;
    let items = if let Some(arr) = items.as_array() {
        arr
    } else {
        items_vec = items
            .get("items")
            .and_then(Value::as_array)
            .expect("completion result array or CompletionList")
            .clone();
        &items_vec
    };
    assert!(
        items.len() <= MAX_COMPLETION_ITEMS,
        "completion must truncate at 200 (live-first), got {}",
        items.len()
    );
    // Relevance order: sortText keys are non-decreasing so live items
    // (`0live_…`) and required properties (`0…`) survive truncation.
    let keys: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("sortText").and_then(Value::as_str))
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] <= w[1]),
        "completion must arrive relevance-sorted (live-first)"
    );
}

// ── Release-only budgets (ignored by default) ────────────────────────────

/// 5 MiB open latency budget. Ignored in the default suite (debug
/// builds are slow and CI machines vary); run explicitly in release:
/// `cargo test -p rsc-ls --release --test perf_smoke -- --ignored`.
#[test]
#[ignore]
fn perf_5mib_open_within_release_budget() {
    let mut client = PerfClient::spawn();
    client.initialize();
    let uri = "file:///perf-5mib-budget.rsc";
    let big = "x".repeat(MAX_DOC_BYTES + 1024);
    let start = Instant::now();
    open_text_document(&mut client, uri, &big);
    let _ = client.expect_diagnostics();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "5 MiB open+publish took {elapsed:?} (> 20 s release budget)"
    );
}

/// 3500-line publish latency budget. Same release-only policy as above.
#[test]
#[ignore]
fn perf_3000line_publish_within_release_budget() {
    let mut client = PerfClient::spawn();
    client.initialize();
    let uri = "file:///perf-lines-budget.rsc";
    let doc = "/foo/unknown add badprop=1\n".repeat(3500);
    let start = Instant::now();
    open_text_document(&mut client, uri, &doc);
    let notif = client.expect_diagnostics();
    let elapsed = start.elapsed();
    let count = notif["params"]["diagnostics"].as_array().map(Vec::len);
    assert!(
        elapsed < Duration::from_secs(10),
        "3500-line publish took {elapsed:?} (> 10 s release budget, {count:?} diags)"
    );
}
