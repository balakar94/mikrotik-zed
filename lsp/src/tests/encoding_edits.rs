// Position encoding.
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

// ── Negotiation matrix ───────────────────────────────────────────────────

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
