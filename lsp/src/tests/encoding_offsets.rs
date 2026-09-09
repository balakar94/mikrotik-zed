// Encoding — boundaries and ranges.
use crate::diagnostics;
use crate::encoding::*;
use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;
fn synth_min() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
"#,
    ))
}

/// Run `initialize` with an optional `general.positionEncodings` array
/// (`None` = capability absent) and return the server plus the response.
fn initialize(encodings: Option<serde_json::Value>) -> (Server, serde_json::Value) {
    let mut server = Server::new(synth_min());
    let params = match encodings {
        None => serde_json::json!({"capabilities": {}}),
        Some(e) => {
            serde_json::json!({"capabilities": {"general": {"positionEncodings": e}}})
        }
    };
    let msg = serde_json::json!({"id": 1, "method": "initialize", "params": params});
    let resp = server.handle_message("initialize", &msg).unwrap();
    (server, resp)
}

// ── Regression: incremental edits must not corrupt documents ─────────────

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

// ── utf16_to_byte_offset ─────────────────────────────────────────────────

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

// ── byte_offset_to_utf16_units ───────────────────────────────────────────

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

// ── strip_bom_prefix ─────────────────────────────────────────────────────

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

// ── convert_diagnostic_ranges ────────────────────────────────────────────

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
    let out = convert_diagnostic_ranges(vec![make_diag()], "a🚨bc\r\n/de", PositionEncoding::Utf8);
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
