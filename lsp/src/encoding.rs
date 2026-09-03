// ── Position-encoding conversion & document patching ────────────
//
// Single boundary between the negotiated LSP wire encoding and the
// server's internal byte-based position math. Everything here is pure
// string/offset logic — no I/O, no server state.

use crate::diagnostics;

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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PositionEncoding::Utf16 => "utf-16",
            PositionEncoding::Utf8 => "utf-8",
        }
    }
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

/// Convert one internal byte-based position into the negotiated wire
/// encoding, measured against the physical lines of the ORIGINAL document.
///
/// Shared boundary helper: used by the diagnostic range converter below and
/// by handlers that emit fresh positions (documentSymbol). Lines are split
/// exactly like inbound logic (`str::lines()`: '\n'-separated, trailing
/// '\r' stripped). Under [`PositionEncoding::Utf8`] this is a no-op.
pub(crate) fn convert_position(
    p: &mut diagnostics::Position,
    lines: &[&str],
    enc: PositionEncoding,
) {
    if enc == PositionEncoding::Utf8 {
        return;
    }
    let line_text = lines.get(p.line as usize).copied().unwrap_or("");
    p.character = byte_offset_to_utf16_units(line_text, p.character as usize);
}

/// Recompute diagnostic range characters from internal byte-offset semantics
/// into the negotiated encoding, measured against the physical lines of the
/// ORIGINAL document. Lines are split exactly like inbound logic
/// (`str::lines()`: '\n'-separated with trailing '\r' stripped); multi-line
/// ranges convert each endpoint against its own line. Under
/// [`PositionEncoding::Utf8`] this is a semantic no-op.
pub(crate) fn convert_diagnostic_ranges(
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
            convert_position(&mut d.range.start, &lines, enc);
            convert_position(&mut d.range.end, &lines, enc);
            d
        })
        .collect()
}

#[derive(Debug)]
pub(crate) enum EditError {
    InvalidRange,
    OutOfBounds,
}

/// Build the byte offset of every physical line's first character.
///
/// One linear `memchr` scan (`b'\n'` is a single ASCII byte, so the byte
/// index is always a char boundary). The result is the standard LSP
/// `str::lines()`-equivalent line start table: `starts[0] == 0`,
/// `starts[i]` points after the `i-1`th `'\n'`. A trailing `'\n'` leaves an
/// empty final entry at `doc.len()`, matching the protocol's vacant last
/// line.
pub(crate) fn line_starts(doc: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    starts.push(0);
    for (i, &b) in doc.as_bytes().iter().enumerate() {
        if b == b'\n' {
            // `i+1` is always a char boundary (after ASCII '\n')
            starts.push(i + 1);
        }
    }
    starts
}

/// Resolve an LSP position to a byte offset within `doc`.
///
/// `character` is interpreted per `enc`: a UTF-16 code-unit count (spec
/// default) or already-byte-based. The target line is located via the
/// precomputed [`line_starts`] table (`'\n'`-separated, mirroring
/// `str::lines()`); its content excludes the trailing `'\r'` of CRLF
/// endings, so positions never address the carriage return. UTF-16 handling
/// stays byte-correct via [`lsp_character_to_byte_offset`] +
/// [`floor_char_boundary`].
pub(crate) fn lsp_position_to_offset(
    doc: &str,
    line: usize,
    character: usize,
    enc: PositionEncoding,
) -> Result<usize, EditError> {
    let starts = line_starts(doc);
    if line >= starts.len() {
        return Err(EditError::OutOfBounds);
    }
    let line_start = starts[line];
    // Byte index of the next '\n' (the `'\n'` itself) or end of document.
    // `starts` encodes exactly this: the next line starts one byte past
    // its preceding '\n', so the `\n` lives at `starts[line+1]-1`.
    let line_end = if line + 1 < starts.len() {
        starts[line + 1] - 1
    } else {
        doc.len()
    };
    // Strip trailing '\r' for "\r\n" handling.
    let line_content = doc[line_start..line_end].trim_end_matches('\r');

    let byte_pos = lsp_character_to_byte_offset(line_content, character, enc);
    Ok(line_start + byte_pos)
}

/// Apply one incremental `range` edit (`new_text`) to `doc`.
///
/// Range characters are interpreted per `enc`; on any invalid or
/// out-of-bounds range the caller falls back to a full document replace.
pub(crate) fn apply_incremental_edit(
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

/// Strip a leading Unicode byte-order mark (U+FEFF) from document text.
///
/// Returns the string unchanged when no BOM is present. Only a single
/// leading BOM is removed; U+FEFF occurrences elsewhere are preserved.
///
/// Applied to document text on `textDocument/didOpen` (before parse/store)
/// so a BOM can never shift positions or surface as a phantom token.
pub(crate) fn strip_bom_prefix(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_floor_char_boundary_clamps() {
        assert_eq!(floor_char_boundary("héllo", 2), 1);
        assert_eq!(floor_char_boundary("hello", 10), 5);
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

    // ── strip_bom_prefix ────────────────────────────────────────

    #[test]
    fn test_strip_bom_prefix_present() {
        assert_eq!(strip_bom_prefix("\u{FEFF}/ip/route"), "/ip/route");
    }

    #[test]
    fn test_strip_bom_prefix_absent() {
        assert_eq!(strip_bom_prefix("/ip/route"), "/ip/route");
    }

    #[test]
    fn test_strip_bom_prefix_empty() {
        assert_eq!(strip_bom_prefix(""), "");
    }

    // ── convert_diagnostic_ranges ─────────────────────────────────

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
