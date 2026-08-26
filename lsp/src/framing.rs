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
use std::io::{BufRead, Read};

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
            log_warn!("malformed Content-Length value: {val_str:?}");
            return None;
        }
        if !val_str.chars().all(|c| c.is_ascii_digit()) {
            log_warn!("malformed Content-Length (non-digit): {val_str:?}");
            return None;
        }
        let parsed: usize = match val_str.parse() {
            Ok(n) => n,
            Err(_) => {
                log_warn!("Content-Length overflow or invalid: {val_str:?}");
                return None;
            }
        };
        if found.is_some() {
            log_warn!("duplicate Content-Length header, rejecting message");
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

/// Read one header line into `buf`, appending at most `budget` bytes
/// (trailing `\n` included when it falls within the budget).
///
/// Returns the number of bytes appended; the line is complete iff the
/// appended slice ends with `b'\n'`. Capping the read HERE — instead of
/// letting [`std::io::BufRead::read_line`] buffer a whole newline-free run
/// before any check — is what keeps the header walk memory-bounded: no
/// single allocation can exceed `budget`, no matter how many bytes a peer
/// streams between newlines.
///
/// Uses a borrowed [`std::io::Take`]: the temporary wrapper delegates
/// `fill_buf` to the underlying reader's existing buffer and drops without
/// consuming anything extra, so buffered bytes are never lost between calls
/// (verified by the stream-resynchronization golden tests below).
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    budget: usize,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    debug_assert!(budget >= 1, "budget must allow at least one byte");
    let before = buf.len();
    (&mut *reader).take(budget as u64).read_until(b'\n', buf)?;
    Ok(buf.len() - before)
}

/// Read exactly one length-prefixed JSON-RPC message from `reader`.
///
/// Behavior-preserving extraction of the former inline loop in [`crate::Server::run`]
/// with one deliberate change: when headers cannot be parsed into a
/// Content-Length, this returns [`FrameError::Protocol`] instead of skipping
/// ahead — skipping is what caused permanent header/body desync.
///
/// Header lines are read through a byte-capped window ([`read_bounded_line`])
/// so a peer streaming unbounded bytes between newlines can never grow heap
/// memory past [`MAX_HEADER_SIZE`]: once the running total crosses the cap
/// the loop flips to discard mode and merely hunts for the terminating blank
/// line. Line-level semantics match the former uncapped `read_line` walk:
/// - EOF before any byte of a line → [`Frame::Eof`];
/// - EOF mid-line processes the partial line exactly like a complete one
///   (the next read then reports EOF, as before);
/// - oversized block WITH a parsable Content-Length → drain that many body
///   bytes, return [`Frame::Skipped`];
/// - oversized block WITHOUT one → terminal [`FrameError::Protocol`].
pub(crate) fn read_message<R: BufRead>(reader: &mut R) -> Result<Frame, FrameError> {
    // Read headers until an empty line. Handle both "\r\n" and "\n".
    let mut header_buf = String::new();
    let mut header_bytes: usize = 0;
    let mut header_too_large = false;
    // Tracks whether the current physical line (which may be split across
    // multiple bounded chunks) has seen any non-whitespace byte. A chunk that
    // ends with `\n` is a blank terminator only when the whole physical line
    // is whitespace-only. Without this accumulation a huge `X-Junk: AAA…\r\n`
    // split into `budget`-sized pieces would misclassify its final single-byte
    // `"\n"` chunk as a blank line and terminate headers prematurely.
    let mut line_has_content = false;
    loop {
        // One byte past the remaining headroom so an overflowing line is
        // DETECTED (the cap is crossed deterministically) instead of
        // truncating silently at exactly the limit, which could leave
        // half-trusted header text in `header_buf`.
        let headroom = MAX_HEADER_SIZE - header_bytes.min(MAX_HEADER_SIZE);
        let mut line = Vec::new();
        let read = read_bounded_line(reader, headroom + 1, &mut line)?;
        if read == 0 {
            if header_bytes == 0 && !header_too_large && !line_has_content {
                return Ok(Frame::Eof);
            } else {
                // EOF mid-header (no blank terminator seen). The former
                // `read_line` walk would have returned the partial line then
                // reported EOF on the *next* read; we mimic that by
                // terminating the header scan here and letting the post-loop
                // `header_too_large` / `parse_content_length` branches decide
                // between Skipped and Protocol. This is what makes the
                // newline-free flood test deterministic instead of returning
                // a spurious `Eof` after an oversized, unparsable block.
                break;
            }
        }
        // A chunk terminates the header section only when it is COMPLETE
        // (ends with the newline). While the budget forces sub-line chunks,
        // a physical "\r\n" arrives as two pieces; breaking on a lone "\r"
        // would leave its "\n" unconsumed and permanently desync the
        // stream. An incomplete final chunk therefore always continues —
        // the next read reports EOF, matching how the former `read_line`
        // walk handled unterminated garbage tails.
        let ends_line = line.ends_with(b"\n");
        let line = match String::from_utf8(line) {
            Ok(s) => s,
            Err(_) => {
                // Mirror BufRead::read_line, which rejected non-UTF-8
                // header bytes with this exact error kind.
                return Err(FrameError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )));
            }
        };
        if !line.trim().is_empty() {
            line_has_content = true;
        }
        let blank = ends_line && !line_has_content;
        if ends_line {
            line_has_content = false;
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_SIZE {
            log_error!("header too large (> {MAX_HEADER_SIZE} bytes), discarding message");
            header_too_large = true;
            // Drain until empty line to resync framing
            if blank {
                break;
            } else {
                continue;
            }
        }
        header_buf.push_str(&line);
        if blank {
            break;
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

    // ── Oversized header blocks (bounded-read regression) ────────────
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
}
