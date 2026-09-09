//! Encoding — boundaries and ranges.
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

// ── Regression: incremental edits must not corrupt documents ──

#[test]
fn test_did_change_incremental_utf16_no_corruption_on_non_ascii_line() {
    let mut s = Server::new(synth_min());
    // Client does not advertise utf-8 → positions are UTF-16 code units.
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    let doc = "# comentário ✔\n/ip/address add address=1.1.1.1\n";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///reg.rsc", "text": doc}}}),
    );

    // Delete the trailing '✔' on line 0 expressed in UTF-16 units:
    // "# comentário " is 13 units, '✔' spans units 13..14 (bytes 14..17).
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 14}},
                    "text": ""
                }]
            }}),
        );
    // Byte-level treatment would instead delete the SPACE before '✔'
    // (bytes 13..14), leaving the emoji behind — exact equality guards it.
    assert_eq!(
        s.docs.get("file:///reg.rsc").unwrap(),
        "# comentário \n/ip/address add address=1.1.1.1\n"
    );

    // Follow-up ranged edit targeting LINE 1 with non-ASCII above: the
    // line-start scan must stay byte-exact while characters stay UTF-16
    // (replaces exactly "/ip/address", 11 units).
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 11}},
                    "text": "/ipv6/address"
                }]
            }}),
        );
    assert_eq!(
        s.docs.get("file:///reg.rsc").unwrap(),
        "# comentário \n/ipv6/address add address=1.1.1.1\n"
    );
}

#[test]
fn test_did_change_incremental_utf8_positions_unchanged_for_ascii() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
    );
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u8.rsc", "text": "hello world"}}}),
        );
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///u8.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}},
                    "text": "Rust"
                }]
            }}),
        );
    assert_eq!(s.docs.get("file:///u8.rsc").unwrap(), "hello Rust");
}

#[test]
fn test_did_change_incremental_utf16_crlf_insert_before_cr() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///crlf.rsc", "text": "héllo\r\nworld"}}}),
        );
    // Insert at the EOL position (unit 6 == end of "héllo"): must land
    // BEFORE the '\r', never inside or after the CRLF pair.
    s.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": "file:///crlf.rsc"},
            "contentChanges": [{
                "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 6}},
                "text": "X"
            }]
        }}),
    );
    assert_eq!(s.docs.get("file:///crlf.rsc").unwrap(), "hélloX\r\nworld");
}

// ── Hover / completion context under Utf16 ────────────────────

#[test]
fn test_hover_utf16_with_multibyte_prefix_on_same_line() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // Non-ASCII comment ABOVE and multibyte prefix BEFORE the target
    // token on the same line: 'ççççç' adds 5 extra bytes over units.
    let doc = concat!(
        "# comentário ✔\n",
        "/ip/address add comment=\"ççççç\" address=1.1.1.1",
    );
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///hv.rsc", "text": doc}}}),
    );
    // "address" starts at unit 32 (byte 37); unit 35 is mid-word.
    let hover = serde_json::json!({
        "id": 9,
        "params": {
            "textDocument": {"uri": "file:///hv.rsc"},
            "position": {"line": 1, "character": 35}
        }
    });
    let resp = s.handle_message("textDocument/hover", &hover).unwrap();
    assert!(
        resp["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("**address**"),
        "word extraction must land on 'address', got {}",
        resp["result"]
    );
}

#[test]
fn test_completion_utf16_value_completions_after_multibyte_prefix() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // Trailing target token sits AFTER multibyte content on the line:
    // 'chain=' ends at unit 42 / byte 43 ('ç' costs one extra byte).
    let doc = "/ip/firewall/filter add comment=\"ç\" chain=";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///cp.rsc", "text": doc}}}),
    );
    let comp = serde_json::json!({
        "id": 10,
        "params": {
            "textDocument": {"uri": "file:///cp.rsc"},
            "position": {"line": 0, "character": 42}
        }
    });
    let resp = s.handle_message("textDocument/completion", &comp).unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(
        labels.contains(&"input"),
        "value completions for 'chain=' expected, got {labels:?}"
    );
}

// ── Diagnostics ranges honor the negotiated encoding ──────────

#[test]
fn test_pull_diagnostics_utf16_character_units_with_emoji_prefix() {
    let mut s = Server::new(synth_min());
    // Default negotiation (no capability) → UTF-16 emission.
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // "bogusprop" starts at byte 25 ("…add " = 16 bytes + two 🚨 = 8)
    // but at unit 21 (each 🚨 counts 2 units).
    let doc = "/ip/address add 🚨🚨 bogusprop=1";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///dg.rsc", "text": doc}}}),
    );
    let pull = s
        .handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 11, "params": {"textDocument": {"uri": "file:///dg.rsc"}}}),
        )
        .unwrap();
    let items = pull["result"]["items"].as_array().unwrap();
    let up = items
        .iter()
        .find(|d| d["code"] == "unknown-property")
        .expect("unknown-property diagnostic expected");
    assert_eq!(up["range"]["start"]["character"], 21);
    assert_eq!(up["range"]["end"]["character"], 30);
}

#[test]
fn test_pull_diagnostics_utf8_character_equals_bytes() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
    );
    let doc = "/ip/address add 🚨🚨 bogusprop=1";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///d8.rsc", "text": doc}}}),
    );
    let pull = s
        .handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 12, "params": {"textDocument": {"uri": "file:///d8.rsc"}}}),
        )
        .unwrap();
    let items = pull["result"]["items"].as_array().unwrap();
    let up = items
        .iter()
        .find(|d| d["code"] == "unknown-property")
        .expect("unknown-property diagnostic expected");
    // Byte semantics preserved exactly when utf-8 is negotiated.
    assert_eq!(up["range"]["start"]["character"], 25);
    assert_eq!(up["range"]["end"]["character"], 34);
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
