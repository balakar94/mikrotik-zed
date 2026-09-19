// ── Live enrichment e2e: real binary against a loopback REST mock ────────
//
// Companion to `e2e.rs` (happy-path wire truths) and `perf_smoke.rs` (caps):
// this file exercises the REAL opt-in live-fetch path end to end. Each test
// starts a std-only HTTP server on `127.0.0.1:0` and spawns `rsc-ls` with
// `RSC_LS_LIVE=1` / `MIKROTIK_HOST=127.0.0.1:<port>` / `MIKROTIK_HTTP=1` /
// `RSC_LS_LIVE_ALLOW_LOOPBACK=1`, opens an `interface=` document, and drives
// real `textDocument/completion` requests.
//
// Covered:
// - 200 JSON → parsed, sanitized (bad values dropped), deduplicated, and
//   merged as live-first (`0!live_`) completion items
// - 404 and 500 fail closed and land in the negative cooldown (no retry
//   storm: repeated completions do not hit the device again)
// - malformed JSON fails closed the same way
// - a >512 KiB body yields a bounded PARTIAL set (the documented streaming
//   contract: `MAX_LIVE_ITEMS`/`MAX_COMPLETION_ITEMS`, never unbounded)
// - a short `MIKROTIK_TIMEOUT` surfaces as an honest empty completion and
//   the server stays alive
//
// No external network: the mock binds loopback ephemeral ports only. Every
// wait is bounded so a wedged server or mock fails fast. Runs on Linux,
// macOS and Windows via plain `cargo test -p rsc-ls`. Set
// `RSC_LS_E2E_STDERR=1` to inherit the server's stderr while debugging.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_rsc-ls");

/// Upper bound on ANY single wait for server output.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on waiting for a live fetch to hydrate the cache.
const LIVE_WAIT: Duration = Duration::from_secs(4);

/// Poll cadence while waiting for live items.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Mirror of `caps::MAX_COMPLETION_ITEMS` (contract, not the constant).
const MAX_COMPLETION_ITEMS: usize = 200;

/// Defensive cap on one response body before allocation.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Test credential; must never appear in plaintext on the wire or in a head.
const TEST_PASS: &str = "live-e2e-s3cret";

const DOC: &str = "/ip/address add interface=";

// ── Loopback HTTP mock ───────────────────────────────────────────────────

#[derive(Clone)]
enum MockMode {
    /// 200 with the given JSON body.
    Json(String),
    /// Non-2xx status with a tiny JSON body.
    Status(u16),
    /// 200 with a body larger than the 512 KiB live response cap.
    Oversized,
    /// 200 with a body that is not a JSON array.
    BadJson,
    /// Accept, count the hit, then answer only after the given delay.
    Delay(Duration),
}

struct MockServer {
    port: u16,
    hits: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(mode: MockMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock");
        let port = listener.local_addr().expect("mock local addr").port();
        listener
            .set_nonblocking(true)
            .expect("mock listener nonblocking");
        let hits = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let hits = Arc::clone(&hits);
            let heads = Arc::clone(&heads);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("live-e2e-mock".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let mode = mode.clone();
                                let hits = Arc::clone(&hits);
                                let heads = Arc::clone(&heads);
                                thread::spawn(move || serve(stream, &mode, &hits, &heads));
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                })
                .expect("spawn mock accept thread")
        };
        MockServer {
            port,
            hits,
            heads,
            stop,
            accept: Some(accept),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn wait_for_hits(&self, min: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let hits = self.hits();
            if hits >= min || Instant::now() >= deadline {
                return hits;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn request_heads(&self) -> Vec<String> {
        self.heads.lock().expect("mock heads lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the polling accept loop immediately; ignore refusal.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// Read one request head (bounded), count the hit, and answer per mode.
/// Write errors are expected when the client stops reading at a cap.
fn serve(stream: TcpStream, mode: &MockMode, hits: &AtomicUsize, heads: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut head_lines = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(n) => {
                header_bytes += n;
                if header_bytes > 16 * 1024 {
                    return;
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
                head_lines.push(line.trim_end().to_string());
            }
            Err(_) => return,
        }
    }
    hits.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut guard) = heads.lock() {
        guard.extend(head_lines);
    }

    let mut stream = stream;
    match mode {
        MockMode::Json(body) => write_response(&mut stream, 200, "OK", body.as_bytes()),
        MockMode::Status(code) => {
            let reason = if *code >= 500 {
                "Internal Server Error"
            } else {
                "Not Found"
            };
            write_response(&mut stream, *code, reason, b"{}");
        }
        MockMode::BadJson => write_response(&mut stream, 200, "OK", b"{not a json array"),
        MockMode::Oversized => {
            let body = oversized_body();
            write_response(&mut stream, 200, "OK", &body);
        }
        MockMode::Delay(delay) => {
            thread::sleep(*delay);
            write_response(&mut stream, 200, "OK", b"[]");
        }
    }
}

fn write_response(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Body well past the 512 KiB live response cap, made of many small records.
/// The streaming extractor must stop at its item cap, not buffer it all.
fn oversized_body() -> Vec<u8> {
    let mut body = Vec::with_capacity(600 * 1024 + 32);
    body.push(b'[');
    let mut i = 0usize;
    while body.len() < 600 * 1024 {
        if i > 0 {
            body.push(b',');
        }
        body.extend_from_slice(format!(r#"{{"name":"iface{i:05}"}}"#).as_bytes());
        i += 1;
    }
    body.extend_from_slice(b"]");
    body
}

// ── Framed JSON-RPC client over the real binary ──────────────────────────

enum Response {
    Ok(Value),
    Err(Value),
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    pending: VecDeque<Value>,
    next_id: i64,
}

impl LspClient {
    /// Spawn the real binary with the live opt-in pointed at `port`. Every
    /// live-related variable is set or removed explicitly so the developer's
    /// real-device environment cannot leak into the test.
    fn spawn_live(port: u16, timeout_secs: u16) -> Self {
        let stderr = if std::env::var_os("RSC_LS_E2E_STDERR").is_some() {
            Stdio::inherit()
        } else {
            Stdio::null()
        };
        let mut child = Command::new(BIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .env("RSC_LS_LIVE", "1")
            .env("MIKROTIK_HOST", "127.0.0.1")
            .env("MIKROTIK_PORT", port.to_string())
            .env("MIKROTIK_USER", "admin")
            .env("MIKROTIK_PASS", TEST_PASS)
            .env("MIKROTIK_HTTP", "1")
            .env("MIKROTIK_TIMEOUT", timeout_secs.to_string())
            .env("RSC_LS_LIVE_ALLOW_LOOPBACK", "1")
            .env_remove("MIKROTIK_LIVE")
            .env_remove("MIKROTIK_SSL")
            .env_remove("MIKROTIK_FINGERPRINT")
            .env_remove("MIKROTIK_CA_FILE")
            .env_remove("RSC_LS_LIVE_RESOURCES")
            .env_remove("MIKROTIK_LIVE_RESOURCES")
            .env_remove("RSC_LS_LIVE_DENY_PREFIXES")
            .env_remove("RSC_LS_LEGACY_HTTP_SHIM")
            .spawn()
            .expect("failed to spawn rsc-ls binary");
        let stdin = child.stdin.take().expect("child stdin was piped");
        let stdout = child.stdout.take().expect("child stdout was piped");
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("rsc-ls-live-e2e-reader".into())
            .spawn(move || pump_frames(stdout, tx))
            .expect("failed to spawn reader thread");
        LspClient {
            child,
            stdin,
            rx,
            pending: VecDeque::new(),
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        match self.request(
            "initialize",
            json!({"processId": null, "rootUri": null, "capabilities": {}}),
        ) {
            Response::Ok(_) => {}
            Response::Err(err) => panic!("initialize failed: {err}"),
        }
        self.notify("initialized", json!({}));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send_raw(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Response {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let msg = self.wait_for_response(&id, method);
        if let Some(err) = msg.get("error") {
            return Response::Err(err.clone());
        }
        Response::Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    fn send_raw(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).expect("message serializes");
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body.as_bytes());
        self.stdin.write_all(&bytes).expect("write framed request");
        self.stdin.flush().expect("flush framed request");
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
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "server closed stdout while awaiting `{method}` (crashed or exited early)"
                ),
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_response_to(msg: &Value, id: &Value) -> bool {
    msg.get("id") == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some())
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
            Ok(None) | Err(_) => return,
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
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "bad content-length")
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
    std::io::Read::read_exact(reader, &mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

// ── Document + completion helpers ────────────────────────────────────────

fn open_document(client: &mut LspClient, uri: &str) {
    client.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rsc",
                "version": 1,
                "text": DOC,
            },
        }),
    );
}

fn completion_items(client: &mut LspClient, uri: &str) -> Vec<Value> {
    let result = match client.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": DOC.len()},
        }),
    ) {
        Response::Ok(result) => result,
        Response::Err(err) => panic!("completion errored: {err}"),
    };
    if let Some(items) = result.get("items").and_then(Value::as_array) {
        return items.clone();
    }
    result
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("unexpected completion shape: {result}"))
}

fn labels(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| item["label"].as_str().map(str::to_string))
        .collect()
}

/// Poll completion until it yields items (live cache hydrated) or timeout.
fn wait_for_items(client: &mut LspClient, uri: &str, timeout: Duration) -> Vec<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let items = completion_items(client, uri);
        if !items.is_empty() || Instant::now() >= deadline {
            return items;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Trigger one completion (which spawns the background fetch), wait for the
/// mock to be hit, give the fetch time to land, then assert completions stay
/// empty and the negative cooldown suppresses retries.
fn assert_negative_cached(client: &mut LspClient, uri: &str, server: &MockServer) {
    let trigger = completion_items(client, uri);
    assert!(
        trigger.is_empty(),
        "first completion must be honestly empty before the fetch lands, got {:?}",
        labels(&trigger)
    );
    let hits = server.wait_for_hits(1, Duration::from_secs(2));
    assert!(
        hits >= 1,
        "mock REST server was never contacted by the live fetch"
    );
    thread::sleep(Duration::from_millis(300));
    let items = completion_items(client, uri);
    assert!(
        items.is_empty(),
        "failed live fetch must complete empty, got {:?}",
        labels(&items)
    );
    for _ in 0..4 {
        let items = completion_items(client, uri);
        assert!(items.is_empty(), "got {:?}", labels(&items));
    }
    assert_eq!(
        server.hits(),
        hits,
        "negative cooldown must suppress refetch after a failed fetch"
    );
}

// ── 200: parse, sanitize, dedupe, live-first ─────────────────────────────

#[test]
fn live_e2e_local_http_200_parses_sanitizes_and_dedupes() {
    let server = MockServer::start(MockMode::Json(
        r#"[{"name":"ether1"},{"name":"ether2"},{"name":"ether1"},{"name":"bad val"},{"nope":"x"}]"#
            .to_string(),
    ));
    let mut client = LspClient::spawn_live(server.port, 5);
    client.initialize();
    let uri = "file:///live-e2e-200.rsc";
    open_document(&mut client, uri);

    let items = wait_for_items(&mut client, uri, LIVE_WAIT);
    let mut got = labels(&items);
    got.sort();
    assert_eq!(
        got,
        vec!["ether1".to_string(), "ether2".to_string()],
        "live values must be filtered (bad val), deduplicated (ether1), and sorted"
    );
    for item in &items {
        let sort_text = item["sortText"]
            .as_str()
            .expect("live completion items carry sortText");
        assert!(
            sort_text.starts_with("0!live_"),
            "live items must sort first, got {sort_text:?}"
        );
    }

    // The request really hit the REST contract with Basic auth and no
    // plaintext password anywhere in the head.
    let heads = server.request_heads().join("\n");
    assert!(
        heads.contains("GET /rest/interface"),
        "REST path/proplist contract, got:\n{heads}"
    );
    assert!(
        heads.to_ascii_lowercase().contains("authorization: basic "),
        "live fetch must send Basic auth, got:\n{heads}"
    );
    assert!(
        !heads.contains(TEST_PASS),
        "raw password must never appear on the wire"
    );
}

// ── 404 / 500: fail closed + negative cooldown ───────────────────────────

#[test]
fn live_e2e_404_fails_closed_and_cooldowns() {
    let server = MockServer::start(MockMode::Status(404));
    let mut client = LspClient::spawn_live(server.port, 5);
    client.initialize();
    let uri = "file:///live-e2e-404.rsc";
    open_document(&mut client, uri);
    assert_negative_cached(&mut client, uri, &server);
}

#[test]
fn live_e2e_500_fails_closed_and_cooldowns() {
    let server = MockServer::start(MockMode::Status(500));
    let mut client = LspClient::spawn_live(server.port, 5);
    client.initialize();
    let uri = "file:///live-e2e-500.rsc";
    open_document(&mut client, uri);
    assert_negative_cached(&mut client, uri, &server);
}

// ── Malformed JSON: fail closed ──────────────────────────────────────────

#[test]
fn live_e2e_malformed_json_fails_closed() {
    let server = MockServer::start(MockMode::BadJson);
    let mut client = LspClient::spawn_live(server.port, 5);
    client.initialize();
    let uri = "file:///live-e2e-badjson.rsc";
    open_document(&mut client, uri);
    assert_negative_cached(&mut client, uri, &server);
}

// ── Oversized body: bounded partial set, never unbounded ─────────────────

#[test]
fn live_e2e_oversized_body_stays_bounded() {
    let server = MockServer::start(MockMode::Oversized);
    let mut client = LspClient::spawn_live(server.port, 5);
    client.initialize();
    let uri = "file:///live-e2e-oversized.rsc";
    open_document(&mut client, uri);

    let items = wait_for_items(&mut client, uri, LIVE_WAIT);
    assert!(
        !items.is_empty(),
        "streaming extraction must keep the partial values it collected"
    );
    assert!(
        items.len() <= MAX_COMPLETION_ITEMS,
        "completion must stay capped at {MAX_COMPLETION_ITEMS}, got {}",
        items.len()
    );
    for label in labels(&items) {
        assert!(
            label.starts_with("iface"),
            "unexpected partial value {label:?}"
        );
    }

    // Liveness probe: the oversized response must not wedge the server.
    match client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 0},
        }),
    ) {
        Response::Ok(_) => {}
        Response::Err(err) => panic!("server must stay alive after oversized body: {err}"),
    }
}

// ── Short timeout: honest empty completion, server alive ─────────────────

#[test]
fn live_e2e_short_timeout_fails_closed() {
    let server = MockServer::start(MockMode::Delay(Duration::from_secs(3)));
    // MIKROTIK_TIMEOUT=1: the fetch must give up long before the mock answers.
    let mut client = LspClient::spawn_live(server.port, 1);
    client.initialize();
    let uri = "file:///live-e2e-timeout.rsc";
    open_document(&mut client, uri);

    // First completion spawns the background fetch; it must answer empty.
    let trigger = completion_items(&mut client, uri);
    assert!(
        trigger.is_empty(),
        "first completion must be honestly empty, got {:?}",
        labels(&trigger)
    );
    let hits = server.wait_for_hits(1, Duration::from_secs(2));
    assert!(hits >= 1, "mock REST server was never contacted");
    // 1 s client timeout + margin for the negative-cache insert.
    thread::sleep(Duration::from_millis(1_500));

    let items = completion_items(&mut client, uri);
    assert!(
        items.is_empty(),
        "timed-out fetch must complete empty, got {:?}",
        labels(&items)
    );
    for _ in 0..4 {
        let items = completion_items(&mut client, uri);
        assert!(items.is_empty(), "got {:?}", labels(&items));
    }
    assert_eq!(
        server.hits(),
        hits,
        "negative cooldown must suppress refetch after a timeout"
    );
}
