// ── JSON-RPC Content-Length framing ─────────────────────────────
//
// Reads one length-prefixed message at a time from a buffered byte
// stream. Defensive properties (all preserved from the original inline
// implementation in main.rs):
// - Header section capped at [`MAX_HEADER_SIZE`]; on overflow with a
//   parseable Content-Length the body is drained and the frame skipped.
// - Bodies larger than [`MAX_MESSAGE_SIZE`] are drained and skipped.
// - Zero-length bodies are skipped.
// - EOF on the first header line yields [`Frame::Eof`]; EOF mid-frame is an
//   I/O error.
// - Unparsable headers are TERMINAL ([`FrameError::Protocol`]): the stream
//   cannot be resynchronized, so continuing would interpret body bytes as
//   headers (permanent desync cascade).

use crate::{MAX_HEADER_SIZE, MAX_MESSAGE_SIZE, log_error, log_warn};
use std::io::BufRead;

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
/// Behavior-preserving extraction of the former inline loop in [`crate::Server::run`]
/// with one deliberate change: when headers cannot be parsed into a
/// Content-Length, this returns [`FrameError::Protocol`] instead of skipping
/// ahead — skipping is what caused permanent header/body desync.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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
}
