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

// ── discard_bytes ────────────────────────────────────────────────────────

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

// ── read_message (golden streams) ────────────────────────────────────────

/// Build one length-prefixed frame around `body`.

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
    // Missing Content-Length is now terminal. Previously the
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
    let mut stream = Cursor::new(b"Content-Length: 5\r\nContent-Length: 6\r\n\r\nhello!".to_vec());
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

// ── Oversized header blocks (bounded-read regression) ────────────────────
//
// The header walk must cap per-line buffering BEFORE a newline is ever
// found, so these streams exercise the exact branch the former
// `read_line`-based loop could not bound.

#[test]
fn test_read_message_oversized_headers_with_content_length_skips_and_resyncs() {
    // Headers blow past MAX_HEADER_SIZE AFTER a valid Content-Length was
    // seen: the message is skipped (its body drained), and the stream
    // stays frame-aligned so the NEXT frame parses cleanly.
    let junk_line = format!("X-Junk: {}\r\n", "A".repeat(MAX_HEADER_SIZE * 2));
    let mut bytes = b"Content-Length: 5\r\n".to_vec();
    bytes.extend_from_slice(junk_line.as_bytes());
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(b"hello");
    bytes.extend_from_slice(&framed(br#"{"id":7}"#));
    let mut stream = Cursor::new(bytes);

    assert!(
        matches!(read_message(&mut stream).unwrap(), Frame::Skipped),
        "oversized-but-parsable headers must skip, not abort"
    );
    assert!(
        matches!(
            read_message(&mut stream).unwrap(),
            Frame::Message(ref b) if b == br#"{"id":7}"#
        ),
        "stream must stay frame-aligned after a skipped oversized block"
    );
    assert!(matches!(read_message(&mut stream).unwrap(), Frame::Eof));
}

#[test]
fn test_read_message_oversized_headers_without_content_length_is_terminal() {
    // Oversized header block WITHOUT any parsable Content-Length:
    // terminal Protocol error naming the cap — the stream cannot be
    // resynchronized, so skipping would cause header/body desync.
    let junk_line = format!("X-Junk: {}\r\n", "B".repeat(MAX_HEADER_SIZE * 2));
    let mut bytes = junk_line.into_bytes();
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(&framed(br#"{"id":8}"#));
    let mut stream = Cursor::new(bytes);
    match read_message(&mut stream).unwrap_err() {
        FrameError::Protocol(why) => assert!(
            why.contains("MAX_HEADER_SIZE"),
            "error should name the exceeded cap, got: {why}"
        ),
        other => panic!("expected Protocol error, got {other:?}"),
    }
}
