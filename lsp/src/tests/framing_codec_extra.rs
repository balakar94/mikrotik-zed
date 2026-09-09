// Framing — codec and limits.
use crate::caps::MAX_DOC_SIZE;
use crate::caps::{MAX_HEADER_SIZE, MAX_MESSAGE_SIZE};
use crate::folding;
use crate::framing::*;
use crate::menus::MenuData;
use crate::server::Server;
use std::io::Cursor;
use std::sync::Arc;
fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

fn wire_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
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
"#,
    ))
}

fn wire_open(server: &mut Server, uri: &str, text: &str) {
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": text}}}),
    );
    server.published.clear();
}

#[test]
fn test_read_message_newline_free_flood_terminates_deterministically() {
    // The unbounded-allocation shape itself: one gigantic line with NO
    // newline anywhere. The capped reader must terminate with bounded
    // memory; with no parsable Content-Length in sight the outcome is
    // still the terminal Protocol error.
    let mut bytes = vec![b'A'; MAX_HEADER_SIZE * 4];
    bytes.extend_from_slice(b"\r\n");
    let mut stream = Cursor::new(bytes);
    match read_message(&mut stream).unwrap_err() {
        FrameError::Protocol(why) => assert!(
            why.contains("MAX_HEADER_SIZE"),
            "error should name the exceeded cap, got: {why}"
        ),
        other => panic!("expected Protocol error, got {other:?}"),
    }
}

#[test]
fn test_read_message_non_utf8_header_is_io_error_like_read_line() {
    // Parity with the former BufRead::read_line behavior: invalid UTF-8
    // in the header section is an I/O-class error, not a Protocol one.
    let mut bytes = b"Content-Length: 5\r\nX-Junk: ".to_vec();
    bytes.push(0xFF);
    bytes.extend_from_slice(b"\r\n\r\nhello");
    let mut stream = Cursor::new(bytes);
    assert!(matches!(
        read_message(&mut stream).unwrap_err(),
        FrameError::Io(_)
    ));
}

#[test]
fn wire_didchange_full_sync_last_wins_deterministic() {
    let mut server = Server::new(wire_data());
    let uri = "file:///wire-lastwins.rsc";
    wire_open(&mut server, uri, "v0");
    let resp = server.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": uri},
            "contentChanges": [{"text": "first"}, {"text": "second"}],
        }}),
    );
    assert!(resp.is_none(), "didChange is a notification");
    assert_eq!(server.docs.get(uri).unwrap(), "second");
    assert_eq!(
        server.published.len(),
        1,
        "exactly one publish per didChange batch"
    );
}

#[test]
fn wire_didchange_incremental_full_incremental_sequence_is_deterministic() {
    let mut server = Server::new(wire_data());
    let uri = "file:///wire-seq.rsc";
    wire_open(&mut server, uri, "hello world");
    server.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": uri},
            "contentChanges": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"},
                {"text": "RESET"},
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "text": "AB"}
            ],
        }}),
    );
    // "hello world" -> "hi world" -> "RESET" -> "ABSET".
    assert_eq!(server.docs.get(uri).unwrap(), "ABSET");
    assert_eq!(
        server.published.len(),
        1,
        "exactly one publish per didChange batch"
    );
    assert_eq!(server.published[0].0, uri);
}

#[test]
fn wire_didchange_good_malformed_no_text_good_still_publishes() {
    let mut server = Server::new(wire_data());
    let uri = "file:///wire-batch.rsc";
    wire_open(&mut server, uri, "seed");
    let resp = server.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": uri},
            "contentChanges": [
                {"text": "AAA"},
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
                {"text": "BBB"}
            ],
        }}),
    );
    assert!(resp.is_none(), "didChange is a notification");
    assert_eq!(
        server.docs.get(uri).unwrap(),
        "BBB",
        "malformed element (no `text`) must be skipped, batch continues"
    );
    assert_eq!(
        server.published.len(),
        1,
        "trailing publish must fire even with a malformed element"
    );
}

// ── Folding / perf smoke ─────────────────────────────────────────────────

#[test]
fn wire_folding_6000_continuations_capped_and_sorted() {
    // 6000 independent two-line continuations → over the 5000 cap.
    let mut doc = String::new();
    for _ in 0..6000 {
        doc.push_str("/tool/fetch url=\"https://example.com/a/b\\\nc\"\n");
    }
    let ranges = crate::folding::compute_folding_ranges(&doc);
    assert_eq!(
        ranges.len(),
        5000,
        "folding cap must hold for continuations"
    );
    assert!(
        ranges
            .windows(2)
            .all(|w| (w[0].start_line, w[0].end_line) <= (w[1].start_line, w[1].end_line)),
        "folding output must stay sorted by (start, end)"
    );
    assert!(
        ranges.iter().all(|r| r.start_line < r.end_line),
        "every range must span more than one line"
    );
}

#[test]
fn wire_folding_nested_do_depth_smoke() {
    // 20 nested `:do {` blocks: one region per level, outer first.
    const DEPTH: u32 = 20;
    let mut doc = String::new();
    for _ in 0..DEPTH {
        doc.push_str(":do {\n");
    }
    doc.push_str(":put x\n");
    for _ in 0..DEPTH {
        doc.push_str("}\n");
    }
    let ranges = crate::folding::compute_folding_ranges(&doc);
    assert_eq!(ranges.len(), DEPTH as usize, "one region per level");
    assert_eq!(ranges[0].start_line, 0);
    assert_eq!(ranges[0].end_line, 2 * DEPTH);
    assert_eq!(ranges[0].kind, Some("region"));
    assert!(
        ranges
            .windows(2)
            .all(|w| (w[0].start_line, w[0].end_line) <= (w[1].start_line, w[1].end_line)),
        "nested output must stay sorted by (start, end)"
    );
}

#[test]
fn wire_didopen_5mib_truncation_with_warn_not_fail_time_budget() {
    // Correctness asserts (must hold); elapsed time only warns.
    let mut server = Server::new(wire_data());
    let uri = "file:///wire-5mib.rsc";
    let big = "x".repeat(crate::MAX_DOC_SIZE + 1024);
    let start = std::time::Instant::now();
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": big}}}),
    );
    let elapsed = start.elapsed();
    let stored = server
        .docs
        .get(uri)
        .expect("oversized doc is truncated, not dropped");
    assert_eq!(
        stored.len(),
        crate::MAX_DOC_SIZE,
        "didOpen must truncate at MAX_DOC_SIZE on a char boundary"
    );
    // Warn-not-fail budget: 10 s on debug CI; exceeding it is an
    // observation for the perf owner, never a gate failure.
    const BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
    if elapsed > BUDGET {
        eprintln!(
            "[wire] WARN: 5 MiB didOpen took {elapsed:?} (> {BUDGET:?}); perf follow-up, not a failure"
        );
    }
}
