//! End-to-end wire tests for the `rsc-ls` language server.
//!
//! Unlike the unit suites (which call `compute_*` directly), these tests
//! spawn the REAL binary via `CARGO_BIN_EXE_rsc-ls` and speak actual LSP:
//! Content-Length framed JSON-RPC over stdio, exactly what Zed exchanges.
//! They lock down the wire truths no in-process test can see:
//!
//! - framing/encoding negotiation and the full advertised capability surface
//! - push diagnostics over `textDocument/publishDiagnostics` (incl. the
//!   RouterOS split-URL continuation pattern producing NO false positives)
//! - incremental (`change = 2`) sync applied through a range-based didChange
//! - UTF-16 as the default position encoding when the client offers nothing
//! - JSON-RPC error contract (-32601 with echoed id) and the LSP 3.17
//!   shutdown→exit lifecycle (process exit code 0)
//!
//! The client is deliberately tiny and built on std only (threads +
//! `mpsc` + `serde_json`, already a crate dependency). A reader thread
//! parses frames from the child's stdout; every wait is bounded by
//! [`RECV_TIMEOUT`] so a wedged server fails a test instead of hanging CI.
//!
//! Runs on Linux, macOS and Windows via plain `cargo test -p rsc-ls` —
//! same pattern as `cli.rs`, no workflow wiring. Docs use `\n` only;
//! paths come exclusively from the exe env var. Set
//! `RSC_LS_E2E_STDERR=1` to inherit the server's stderr while debugging.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_rsc-ls");

/// Upper bound on ANY single wait for server output. Generous enough for a
/// cold debug build on loaded CI; small enough that a wedged server turns
/// into a fast, named failure rather than a hung job.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Mirror of the server's own header cap (`framing::MAX_HEADER_SIZE`): a
/// peer that never terminates its header section must fail the frame, not
/// spin the reader thread forever.
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Defensive cap on one response body before allocation. Far above anything
/// this server emits (its own hard cap is 10 MiB); exists only so a corrupt
/// Content-Length cannot trigger an unbounded allocation in the test.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

// ── Fixtures ────────────────────────────────────────────────────────

/// Real-world hagezi-style fetch: the quoted URL continues onto the next
/// physical line behind a trailing backslash, followed by a line carrying
/// an unknown-property typo. Without continuation-aware joining the split
/// string fabricates syntax errors and fragment noise; with it, only the
/// genuine typo may surface.
const SPLIT_URL_DOC: &str = concat!(
    "/tool/fetch url=\"https://raw.githubusercontent.com/hagezi/dns-blocklists/main/wildcard/mikrotik.txt\\\n",
    "\"\n",
    "/ip/address add adress=10.0.0.1\n",
);

/// Third line of [`SPLIT_URL_DOC`]: `/ip/address add adress=…`.
const TYPO_LINE: &str = "/ip/address add adress=10.0.0.1";
/// Expected post-fix text once the quick-fix / incremental edit applies.
const FIXED_LINE: &str = "/ip/address add address=10.0.0.1";

/// Multi-command script for documentSymbol: a `:local` variable plus two
/// menu commands.
const SYMBOL_DOC: &str = concat!(
    ":local wanif \"ether1\"\n",
    "/ip/address add address=10.0.0.1/24 interface=ether1\n",
    "/ip/route add gateway=10.0.0.254\n",
);

/// Variable-navigation script: one `:local wanif` declaration (name spans
/// UTF-16 units 7..12 of line 0), one bare usage (`:put $wanif`, units
/// 6..11 of line 1) and one glued to a property value
/// (`interface=$wanif`, units 27..32 of line 2).
const NAV_DOC: &str = concat!(
    ":local wanif \"ether1\"\n",
    ":put $wanif\n",
    "/ip/address add interface=$wanif\n",
);

/// Split-URL fetch whose continuation spans physical lines 0–1; the logical
/// line must fold onto its first physical line.
const FOLD_DOC: &str = concat!("/tool/fetch url=\"https://example.com/a/b\\\n", "c\"\n",);

/// Document used for the UTF-16 default-encoding scenario. Everything up to
/// `interface` includes a three-emoji run: each 🌍 costs 4 UTF-8 bytes but
/// only 2 UTF-16 units, so byte-interpreted positions diverge from correct
/// ones by 6 units — enough to land in a completely different token.
const EMOJI_BEFORE_INTERFACE: &str = "/ip/address set address=1.2.3.4 comment=\"🌍🌍🌍\" ";
const EMOJI_REST_OF_LINE: &str = "interface=bridge1";
/// Prefix up to (and including) the opening quote of the emoji value; used
/// to aim a probe INSIDE the emoji run's surrogate pairs.
const EMOJI_QUOTE_PREFIX: &str = "/ip/address set address=1.2.3.4 comment=\"";

// ── Small fixture helpers ───────────────────────────────────────────

/// UTF-16 code units of `s` — how LSP clients must count `character`
/// values under the spec-default encoding.
fn utf16_units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// A client already past the initialize/initialized handshake (no client
/// capabilities offered → the server must fall back to the UTF-16 default).
fn initialized_client() -> LspClient {
    let mut client = LspClient::spawn();
    client.initialize(json!({}));
    client
}

/// Send `textDocument/didOpen` for `uri`.
fn open_text_document(client: &mut LspClient, uri: &str, text: &str) {
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

// ── Framed JSON-RPC client ──────────────────────────────────────────

/// Classification of a matched JSON-RPC response.
enum Response {
    /// The `result` member (may be JSON null — LSP uses null results).
    Ok(Value),
    /// The whole `error` object.
    Err(Value),
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    /// Messages received while waiting for something else. Notifications
    /// are never silently dropped: they queue here until a waiter claims
    /// them, keeping publish ordering deterministic.
    pending: VecDeque<Value>,
    next_id: i64,
}

impl LspClient {
    /// Spawn the real binary with piped stdio. Stderr goes to null unless
    /// `RSC_LS_E2E_STDERR` is set (then inherited, visible under
    /// `cargo test -- --nocapture`).
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
            .name("rsc-ls-e2e-reader".into())
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

    /// LSP lifecycle: send the `initialize` request, wait for the matching
    /// response and return its `result`; then discharge the follow-up
    /// `initialized` notification the spec requires from the client.
    fn initialize(&mut self, capabilities: Value) -> Value {
        let result = match self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": null,
                "capabilities": capabilities,
            }),
        ) {
            Response::Ok(result) => result,
            Response::Err(err) => panic!("initialize failed: {err}"),
        };
        assert!(
            result["capabilities"].is_object(),
            "initialize result must carry capabilities, got {result}"
        );
        self.notify("initialized", json!({}));
        result
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn request(&mut self, method: &str, params: Value) -> Response {
        let id = Value::from(self.next_id);
        self.next_id += 1;
        self.request_with_id(id, method, params)
    }

    /// Like [`Self::request`] but with a caller-chosen id (used to prove
    /// non-integer ids are echoed verbatim).
    fn request_with_id(&mut self, id: Value, method: &str, params: Value) -> Response {
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        // Matching by id IS the echo proof: only a response carrying our
        // exact id can satisfy the wait below.
        let msg = self.wait_for_response(&id, method);
        if let Some(err) = msg.get("error") {
            return Response::Err(err.clone());
        }
        Response::Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Next notification with `method`, skipping (and preserving) anything
    /// else that arrives first.
    fn expect_notification(&mut self, method: &str) -> Value {
        if let Some(pos) = self
            .pending
            .iter()
            .position(|m| is_notification_of(m, method))
        {
            return self.pending.remove(pos).expect("position came from len");
        }
        loop {
            match self.recv_next() {
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
            match self.recv_next() {
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

    fn recv_next(&self) -> Result<Value, RecvTimeoutError> {
        self.rx.recv_timeout(RECV_TIMEOUT)
    }

    /// Write one Content-Length framed message. The header length is the
    /// serialized BYTE count (UTF-8 bodies may exceed their char count).
    fn send_raw(&mut self, msg: Value) {
        let body = serde_json::to_string(&msg).expect("JSON-RPC message serializes");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("writing frame header");
        self.stdin
            .write_all(body.as_bytes())
            .expect("writing frame body");
        self.stdin.flush().expect("flushing frame");
    }

    /// LSP 3.17 shutdown sequence: `shutdown` request answered with a null
    /// result, then the `exit` notification; returns the process status for
    /// exit-code assertions.
    fn shutdown_and_exit(&mut self) -> ExitStatus {
        match self.request("shutdown", Value::Null) {
            Response::Ok(result) => {
                assert!(
                    result.is_null(),
                    "shutdown must answer a null result, got {result}"
                );
            }
            Response::Err(err) => panic!("shutdown returned an error: {err}"),
        }
        self.notify("exit", Value::Null);
        self.wait_for_exit()
    }

    /// Bounded wait for process termination so a wedged `exit` fails the
    /// test after [`RECV_TIMEOUT`] instead of hanging CI.
    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + RECV_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().expect("polling child status") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("rsc-ls ignored `exit` — still running after {RECV_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Reap every spawned server even when an assertion panicked above;
        // kill errors are irrelevant once the test outcome is decided.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// True when `msg` is a response (has result/error, carries an id) whose id
/// equals ours.
fn is_response_to(msg: &Value, id: &Value) -> bool {
    msg.get("id") == Some(id) && (msg.get("result").is_some() || msg.get("error").is_some())
}

fn is_notification_of(msg: &Value, method: &str) -> bool {
    msg.get("id").is_none() && msg.get("method").and_then(Value::as_str) == Some(method)
}

/// Reader-thread body: parse frames until EOF, forward values to `tx`. On
/// malformed input or EOF the thread returns, closing the channel — any
/// waiter then fails fast with a clear panic instead of hanging.
fn pump_frames(stdout: ChildStdout, tx: Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader) {
            Ok(Some(msg)) => {
                if tx.send(msg).is_err() {
                    return; // client dropped; stop pumping
                }
            }
            Ok(None) => return, // clean EOF
            Err(err) => {
                eprintln!("rsc-ls e2e reader terminated: {err}");
                return;
            }
        }
    }
}

/// Read one Content-Length framed JSON value; `Ok(None)` on clean EOF at a
/// frame boundary. The spec writes header lines as `\r\n` (what this server
/// emits); a bare `\n` terminator is tolerated so the client never depends
/// on platform newline translation.
fn read_frame<R: BufRead>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            if content_length.is_none() && line.is_empty() {
                return Ok(None); // EOF between frames
            }
            return Err(std::io::Error::other("EOF inside a frame"));
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(std::io::Error::other("header section exceeds 32 KiB"));
        }
        match line.trim_end_matches(['\r', '\n']) {
            "" => break, // blank line ends the header section
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

// ── Scenarios ───────────────────────────────────────────────────────

#[test]
fn initialize_advertises_full_capability_surface_and_version() {
    let mut client = LspClient::spawn();
    let result = client.initialize(json!({}));

    let caps = &result["capabilities"];
    // Client offered nothing → the spec-mandated fallback encoding.
    assert_eq!(
        caps["positionEncoding"], "utf-16",
        "no client offer must negotiate the utf-16 default"
    );
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!(["/", " ", "=", ":"])
    );
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["documentSymbolProvider"], true);
    assert_eq!(caps["foldingRangeProvider"], true);
    assert_eq!(caps["codeActionProvider"], true);
    // Variable navigation must be advertised so Zed wires go-to-definition
    // and find-references for `.rsc` scripts.
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["referencesProvider"], true);
    assert_eq!(
        caps["signatureHelpProvider"]["triggerCharacters"],
        json!([" ", "="])
    );
    assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
    assert_eq!(caps["diagnosticProvider"]["workspaceDiagnostics"], false);
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(
        caps["textDocumentSync"]["change"], 2,
        "incremental sync must be advertised as change = 2"
    );

    let server_info = &result["serverInfo"];
    assert_eq!(server_info["name"], "mikrotik-rsc-ls");
    // Test crate and binary share lsp/Cargo.toml → versions must agree.
    assert_eq!(
        server_info["version"],
        env!("CARGO_PKG_VERSION"),
        "serverInfo.version must equal CARGO_PKG_VERSION"
    );
}

#[test]
fn did_open_split_url_publishes_unknown_property_without_menu_false_positive() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-split-url.rsc", SPLIT_URL_DOC);
    let publish = client.expect_notification("textDocument/publishDiagnostics");
    let uri = publish["params"]["uri"].as_str().expect("publish uri");
    assert_eq!(uri, "file:///e2e-split-url.rsc");
    let diags = publish["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");

    let typos: Vec<_> = diags
        .iter()
        .filter(|d| d["code"] == "unknown-property")
        .collect();
    assert_eq!(
        typos.len(),
        1,
        "exactly the adress typo must fire, got {diags:?}"
    );
    let typo = typos[0];
    assert_eq!(typo["severity"], 2, "unknown-property is a Warning");
    assert_eq!(typo["source"], "rsc-ls");
    assert!(
        typo["message"].as_str().unwrap().contains("adress"),
        "message must name the offending key"
    );
    // Range points at the typo on physical line 2, characters 16..22.
    assert_eq!(typo["range"]["start"]["line"], 2);
    assert_eq!(
        typo["range"]["start"]["character"],
        TYPO_LINE.find("adress").unwrap()
    );

    // The split URL must stay silent: joined continuations mean no unknown
    // menu, no fabricated unterminated-quote/brace errors.
    for banned in ["unknown-menu", "unclosed-quote", "unclosed-brace"] {
        assert!(
            !diags.iter().any(|d| d["code"] == banned),
            "{banned} must not fire on the hagezi split-URL pattern, got {diags:?}"
        );
    }
}

#[test]
fn incremental_range_edit_republishes_diagnostics_without_fixed_typo() {
    let mut client = initialized_client();
    let uri = "file:///e2e-incremental.rsc";
    open_text_document(&mut client, uri, SPLIT_URL_DOC);
    let first = client.expect_notification("textDocument/publishDiagnostics");
    assert!(
        first["params"]["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|d| d["code"] == "unknown-property"),
        "fixture doc must start dirty"
    );

    // Range-based edit replacing `adress` with `address`: exercises
    // apply_incremental_edit across the wire (a server that ignored or
    // mis-applied the patch would republish the same typo).
    let start = TYPO_LINE.find("adress").unwrap();
    let end = start + "adress".len();
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{
                "range": {
                    "start": {"line": 2, "character": start},
                    "end": {"line": 2, "character": end},
                },
                "text": "address",
            }],
        }),
    );
    let second = client.expect_notification("textDocument/publishDiagnostics");
    let diags = second["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert!(
        !diags.iter().any(|d| d["code"] == "unknown-property"),
        "republished diagnostics must drop the fixed typo, got {diags:?}"
    );
    assert!(
        !diags.iter().any(|d| d["code"] == "unknown-menu"),
        "fix must not introduce new false positives"
    );
}

#[test]
fn completion_after_menu_path_returns_nonempty_items() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-completion.rsc", "/ip/address ");
    let result = match client.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": "file:///e2e-completion.rsc"},
            "position": {"line": 0, "character": 12}, // after the trailing space
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("completion errored: {err}"),
    };
    let items = result["items"].as_array().expect("CompletionList.items");
    assert!(
        !items.is_empty(),
        "menu path completion must suggest candidates"
    );
    assert!(
        items.iter().any(|i| i["label"] == "add"),
        "verb suggestions expected after `/ip/address `, got labels {:?}",
        items
            .iter()
            .filter_map(|i| i["label"].as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn hover_on_known_command_returns_markdown_contents() {
    let mut client = initialized_client();
    open_text_document(
        &mut client,
        "file:///e2e-hover.rsc",
        "/ip/address add address=1.2.3.4",
    );
    let result = match client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": "file:///e2e-hover.rsc"},
            "position": {"line": 0, "character": 5}, // inside `/ip/address`
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("hover errored: {err}"),
    };
    assert!(
        result.is_object(),
        "hover must be non-null here, got {result}"
    );
    let contents = &result["contents"];
    assert_eq!(
        contents["kind"], "markdown",
        "hover contents must be markdown"
    );
    assert!(
        contents["value"].as_str().unwrap().contains("/ip/address"),
        "menu hover must name the hovered menu, got {contents}"
    );
}

#[test]
fn document_symbol_reports_local_variable_and_menu_commands() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-symbols.rsc", SYMBOL_DOC);
    let symbols = match client.request(
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": "file:///e2e-symbols.rsc"}}),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("documentSymbol errored: {err}"),
    };
    let symbols = symbols.as_array().expect("flat symbol list");

    let local = symbols
        .iter()
        .find(|s| s["name"] == "wanif")
        .expect(":local declaration must become a symbol");
    assert_eq!(local["kind"], 13, ":local maps to SymbolKind.Variable");

    let commands: Vec<_> = symbols
        .iter()
        .filter(|s| s["kind"] == 19)
        .map(|s| s["name"].as_str().unwrap_or_default())
        .collect();
    assert!(
        commands.len() >= 2 && commands.contains(&"/ip/address add"),
        "both menu commands must appear as SymbolKind.Object, got {commands:?}"
    );
}

/// Go-to-definition from a `$wanif` usage must land on the exact name span
/// of the `:local` declaration — over the real wire, through framing and
/// UTF-16 default positions.
#[test]
fn definition_from_usage_jumps_to_local_declaration_over_the_wire() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-nav-def.rsc", NAV_DOC);
    // Cursor inside the usage on line 1 (the `a` of $wanif).
    let location = match client.request(
        "textDocument/definition",
        json!({
            "textDocument": {"uri": "file:///e2e-nav-def.rsc"},
            "position": {"line": 1, "character": 8},
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("definition errored: {err}"),
    };
    assert!(
        location.is_object(),
        "usage must resolve to a Location, got {location}"
    );
    assert_eq!(location["uri"], "file:///e2e-nav-def.rsc");
    assert_eq!(location["range"]["start"]["line"], 0);
    assert_eq!(location["range"]["start"]["character"], 7);
    assert_eq!(location["range"]["end"]["line"], 0);
    assert_eq!(
        location["range"]["end"]["character"], 12,
        "range must cover exactly `wanif`, not the command token or value"
    );

    // Splicing sanity: the returned range really names the variable.
    let line0 = ":local wanif \"ether1\"";
    let s = location["range"]["start"]["character"].as_u64().unwrap() as usize;
    let e = location["range"]["end"]["character"].as_u64().unwrap() as usize;
    assert_eq!(&line0[s..e], "wanif");
}

/// Find-references over the wire: flat Location list, declaration gated by
/// `includeDeclaration`, usages in document order including one glued to a
/// property (`interface=$wanif`).
#[test]
fn references_return_expected_counts_and_document_order() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-nav-refs.rsc", NAV_DOC);
    let params = |include: bool| {
        json!({
            "textDocument": {"uri": "file:///e2e-nav-refs.rsc"},
            "position": {"line": 1, "character": 8},
            "context": {"includeDeclaration": include},
        })
    };

    let with = match client.request("textDocument/references", params(true)) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("references errored: {err}"),
    };
    let items = with.as_array().expect("Location[]");
    assert_eq!(items.len(), 3, "declaration + two usages");
    assert_eq!(
        items[0]["range"]["start"],
        json!({"line": 0, "character": 7})
    );
    assert_eq!(
        items[0]["range"]["end"],
        json!({"line": 0, "character": 12})
    );
    assert_eq!(
        items[1]["range"]["start"],
        json!({"line": 1, "character": 6})
    );
    // The glued property usage is found despite sharing a token with
    // `interface=`.
    assert_eq!(
        items[2]["range"]["start"],
        json!({"line": 2, "character": 27})
    );

    let without = match client.request("textDocument/references", params(false)) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("references errored: {err}"),
    };
    let items = without.as_array().expect("Location[]");
    assert_eq!(
        items.len(),
        2,
        "usages only when includeDeclaration is false"
    );
    assert_eq!(items[0]["range"]["start"]["line"], 1);
}

#[test]
fn folding_range_covers_split_url_continuation() {
    let mut client = initialized_client();
    open_text_document(&mut client, "file:///e2e-fold.rsc", FOLD_DOC);
    let ranges = match client.request(
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": "file:///e2e-fold.rsc"}}),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("foldingRange errored: {err}"),
    };
    let ranges = ranges.as_array().expect("folding range list");
    let continuation = ranges
        .iter()
        .find(|r| r["startLine"] == 0 && r["endLine"] == 1)
        .expect("the split-URL continuation must fold onto its first line");
    assert!(
        continuation.get("kind").is_none(),
        "continuation folds carry no kind (brace regions would say region), got {continuation}"
    );
}

#[test]
fn code_action_quick_fix_edit_applies_cleanly_to_document_text() {
    let mut client = initialized_client();
    let uri = "file:///e2e-codeaction.rsc";
    open_text_document(&mut client, uri, TYPO_LINE);
    let publish = client.expect_notification("textDocument/publishDiagnostics");
    let diag = publish["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|d| d["code"] == "unknown-property")
        .expect("server-published unknown-property diagnostic")
        .clone();

    // Echo the server's OWN diagnostic verbatim, like a real client would.
    let actions = match client.request(
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": uri},
            "range": diag["range"],
            "context": {"diagnostics": [diag]},
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("codeAction errored: {err}"),
    };
    let actions = actions.as_array().expect("CodeAction[]");
    let fix = actions
        .iter()
        .find(|a| a["title"] == "Did you mean 'address'?")
        .expect("quick fix proposing 'address'");
    assert_eq!(fix["kind"], "quickfix");

    let edit = &fix["edit"]["changes"][uri][0];
    assert_eq!(
        edit["range"], diag["range"],
        "edit must target the diagnostic's own range"
    );
    let new_text = edit["newText"].as_str().expect("newText");
    assert_eq!(new_text, "address");

    // String-level sanity: splicing the edit into the original line yields
    // the corrected command.
    let start = diag["range"]["start"]["character"].as_u64().unwrap() as usize;
    let end = diag["range"]["end"]["character"].as_u64().unwrap() as usize;
    let mut fixed = String::with_capacity(TYPO_LINE.len());
    fixed.push_str(&TYPO_LINE[..start]);
    fixed.push_str(new_text);
    fixed.push_str(&TYPO_LINE[end..]);
    assert_eq!(fixed, FIXED_LINE, "quick-fix edit must splice cleanly");
}

#[test]
fn signature_help_after_verb_lists_named_parameters() {
    let mut client = initialized_client();
    let uri = "file:///e2e-signature.rsc";
    open_text_document(&mut client, uri, "/ip/address add ");
    let help = match client.request(
        "textDocument/signatureHelp",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": 16}, // right after `add `
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("signatureHelp errored: {err}"),
    };
    assert!(
        help.is_object(),
        "signature popup expected after a resolved command, got {help}"
    );
    let signatures = help["signatures"].as_array().expect("signatures array");
    assert_eq!(signatures.len(), 1, "exactly one signature per command");
    let sig = &signatures[0];
    let label = sig["label"].as_str().expect("signature label");
    assert!(
        label.starts_with("/ip/address add"),
        "signature label must describe the resolved command, got {label:?}"
    );
    let parameters = sig["parameters"].as_array().expect("parameters array");
    assert!(
        !parameters.is_empty(),
        "`/ip/address add` declares settable properties → parameters expected"
    );
    // Parameter labels are [start, end] byte offsets INSIDE the label string.
    for param in parameters {
        let span = param["label"].as_array().expect("offset-form label");
        let (lo, hi) = (
            span[0].as_u64().expect("label start") as usize,
            span[1].as_u64().expect("label end") as usize,
        );
        assert!(
            lo < hi && hi <= label.len(),
            "parameter label offsets [{lo}, {hi}) must index the signature label"
        );
    }
    // activeParameter is omitted when no property is being typed; when
    // present it must index the parameters array.
    if let Some(active) = sig.get("activeParameter") {
        let idx = active.as_u64().expect("activeParameter number") as usize;
        assert!(idx < parameters.len(), "activeParameter {idx} out of range");
    }
}

#[test]
fn unknown_method_with_id_answers_method_not_found_echoing_id() {
    let mut client = initialized_client();
    const MISSING: &str = "workspace/doesNotExist";
    // Non-integer id doubles as proof that ids are echoed verbatim, not
    // re-numbered: request_with_id matches the response by exact id.
    match client.request_with_id(json!("e2e-string-id"), MISSING, json!({})) {
        Response::Err(err) => {
            assert_eq!(err["code"], -32601, "MethodNotFound family");
            assert!(
                err["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(MISSING),
                "error should name the unknown method, got {err}"
            );
        }
        Response::Ok(result) => {
            panic!("unknown method must error, got result {result}")
        }
    }
}

#[test]
fn shutdown_then_exit_terminates_with_status_zero() {
    let mut client = initialized_client();
    let status = client.shutdown_and_exit();
    assert_eq!(
        status.code(),
        Some(0),
        "LSP 3.17: exit after shutdown must terminate with status 0, got {status}"
    );
}

#[test]
fn positions_default_to_utf16_units_when_client_sends_no_encoding() {
    let mut client = LspClient::spawn();
    let negotiated = client.initialize(json!({}))["capabilities"]["positionEncoding"]
        .as_str()
        .expect("positionEncoding string")
        .to_owned();
    assert_eq!(
        negotiated, "utf-16",
        "client sent no positionEncodings → server must default to utf-16"
    );

    let uri = "file:///e2e-utf16.rsc";
    open_text_document(
        &mut client,
        uri,
        &format!("{EMOJI_BEFORE_INTERFACE}{EMOJI_REST_OF_LINE}\n"),
    );

    // Ground truth in UTF-16 units: 41 ASCII units + 3×2 units per 🌍 +
    // quote and space = 49, so `interface` starts at unit 49. The absolute
    // pin guards against silent fixture drift (the probes below reason
    // about exact byte-vs-unit divergence).
    let interface_start = utf16_units(EMOJI_BEFORE_INTERFACE);
    assert_eq!(interface_start, 49, "fixture layout drifted");
    let inside_interface = interface_start + 3; // unit 52: the `e` of interface

    // DECISIVE probe: read as BYTES, 52 lands mid-emoji (the third 🌍 owns
    // bytes 49..53) where word extraction finds nothing; read correctly as
    // UTF-16 units it sits inside `interface` and must resolve.
    let hover = match client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": inside_interface},
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("hover errored: {err}"),
    };
    assert!(
        hover.is_object(),
        "utf-16 position inside `interface` must hover, got {hover}"
    );
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("interface"),
        "hover must resolve `interface` from a utf-16 offset, got {hover}"
    );

    // Second probe: a position inside an emoji's surrogate pair —
    // EMOJI_QUOTE_PREFIX is 41 units, +3 lands mid-surrogate-pair of the
    // second 🌍. A correct decoder floors it onto a non-word character and
    // answers null; byte passthrough would behave differently.
    let surrogate_half = utf16_units(EMOJI_QUOTE_PREFIX) + 3;
    let near_emoji = match client.request(
        "textDocument/hover",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 0, "character": surrogate_half},
        }),
    ) {
        Response::Ok(v) => v,
        Response::Err(err) => panic!("hover errored: {err}"),
    };
    assert!(
        near_emoji.is_null(),
        "position inside an astral char must not resolve to a word, got {near_emoji}"
    );
}
