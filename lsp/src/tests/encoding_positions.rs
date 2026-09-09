//! Position encoding.
use crate::encoding::PositionEncoding;
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

// ── Negotiation matrix ────────────────────────────────────────

#[test]
fn test_initialize_without_capability_defaults_to_utf16() {
    let (server, resp) = initialize(None);
    assert_eq!(
        resp["result"]["capabilities"]["positionEncoding"], "utf-16",
        "spec default when client sends no positionEncodings"
    );
    assert_eq!(server.position_encoding, PositionEncoding::Utf16);
}

#[test]
fn test_initialize_prefers_utf8_when_client_advertises_it() {
    let (server, resp) = initialize(Some(serde_json::json!(["utf-16", "utf-8"])));
    assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(server.position_encoding, PositionEncoding::Utf8);
}

#[test]
fn test_initialize_falls_back_to_utf16_when_utf8_absent() {
    let (server, resp) = initialize(Some(serde_json::json!(["utf-32"])));
    assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-16");
    assert_eq!(server.position_encoding, PositionEncoding::Utf16);
}

#[test]
fn test_initialize_keeps_existing_capabilities_intact() {
    let (_, resp) = initialize(Some(serde_json::json!(["utf-8"])));
    let caps = &resp["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["textDocumentSync"]["change"], 2);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        serde_json::json!(["/", " ", "=", ":"])
    );
    assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
}

#[test]
fn test_initialize_advertises_incremental_sync() {
    // textDocumentSync must be the object form (openClose + change = 2),
    // not the legacy scalar Full-sync kind. Incremental patching is
    // implemented and tested (apply_incremental_edit); full-text
    // replacements remain handled as a fallback.
    let (_, resp) = initialize(None);
    let sync = &resp["result"]["capabilities"]["textDocumentSync"];
    assert!(sync.is_object(), "sync capability must be the object form");
    assert_eq!(sync["change"], 2);
    assert_eq!(sync["openClose"], true);
}

#[test]
fn test_initialize_advertises_all_providers() {
    // Stage B: every supported provider must be advertised together.
    let (_, resp) = initialize(None);
    let caps = &resp["result"]["capabilities"];
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        serde_json::json!(["/", " ", "=", ":"])
    );
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(
        caps["documentSymbolProvider"], true,
        "documentSymbol capability must be advertised"
    );
    assert_eq!(
        caps["foldingRangeProvider"], true,
        "foldingRange capability must be advertised"
    );
    assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
}

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
