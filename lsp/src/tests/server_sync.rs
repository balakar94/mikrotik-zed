// White-box: server sync.

use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;

fn synthetic_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus]]
path = "/ip/route"
type = "Directory"
[[menus.arguments]]
name = "gateway"
type = "ipAddr"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/bridge/port"
type = "Directory"
[[menus]]
path = "/system/clock"
type = "Directory"
"#,
    ))
}

// ── Server handle_message integration ────────────────────────────────────

fn make_server() -> Server {
    Server::new(synthetic_data())
}

#[test]
fn test_server_did_change_full_sync() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///a.rsc", "text": "old"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let change = serde_json::json!({
        "params": {
            "textDocument": {"uri": "file:///a.rsc"},
            "contentChanges": [{"text": "new content"}]
        }
    });
    server.handle_message("textDocument/didChange", &change);
    assert_eq!(server.docs.get("file:///a.rsc").unwrap(), "new content");
}

#[test]
fn test_server_did_change_incremental() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///b.rsc", "text": "hello world"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let change = serde_json::json!({
        "params": {
            "textDocument": {"uri": "file:///b.rsc"},
            "contentChanges": [{
                "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}},
                "text": "Rust"
            }]
        }
    });
    server.handle_message("textDocument/didChange", &change);
    assert_eq!(server.docs.get("file:///b.rsc").unwrap(), "hello Rust");
}

#[test]
fn test_server_did_change_incremental_fallback_to_full_on_error() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///c.rsc", "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Invalid range (out of bounds) should fallback to replacing whole doc
    let change = serde_json::json!({
        "params": {
            "textDocument": {"uri": "file:///c.rsc"},
            "contentChanges": [{
                "range": {"start": {"line": 10, "character": 0}, "end": {"line": 10, "character": 5}},
                "text": "fallback"
            }]
        }
    });
    server.handle_message("textDocument/didChange", &change);
    assert_eq!(server.docs.get("file:///c.rsc").unwrap(), "fallback");
}

#[test]
fn test_server_did_close() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///x.rsc", "text": "hi"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert!(server.docs.contains_key("file:///x.rsc"));
    let close = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///x.rsc"}}
    });
    server.handle_message("textDocument/didClose", &close);
    assert!(!server.docs.contains_key("file:///x.rsc"));
}

#[test]
fn test_server_hover_found() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///h.rsc", "text": "/ip/address add address=1.1.1.1"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Hover over property "address" (second occurrence)
    // line 0, character near property name (after "add ")
    let line = "/ip/address add address=1.1.1.1";
    let prop_start = line.find("add ").unwrap() + 4; // start of "address="
    let hover = serde_json::json!({
        "id": 5,
        "params": {
            "textDocument": {"uri": "file:///h.rsc"},
            "position": {"line": 0, "character": prop_start + 2}
        }
    });
    let resp = server.handle_message("textDocument/hover", &hover).unwrap();
    assert!(resp["result"].is_object(), "hover should return object");
    assert!(
        resp["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("address")
    );
}

#[test]
fn test_server_hover_not_found_returns_null() {
    let mut server = make_server();
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///h2.rsc", "text": "/ip/address add unknownprop"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let line = "/ip/address add unknownprop";
    let pos = line.find("unknownprop").unwrap() + 2;
    let hover = serde_json::json!({
        "id": 6,
        "params": {
            "textDocument": {"uri": "file:///h2.rsc"},
            "position": {"line": 0, "character": pos}
        }
    });
    let resp = server.handle_message("textDocument/hover", &hover).unwrap();
    assert!(resp["result"].is_null());
}

#[test]
fn test_server_hover_untracked_doc_returns_null_result() {
    // Previously asserted `resp.is_none()` (dropped
    // request); untracked URI now answers null result with id echoed.
    let mut server = make_server();
    let hover = serde_json::json!({
        "id": 7,
        "params": {
            "textDocument": {"uri": "file:///notopen.rsc"},
            "position": {"line": 0, "character": 1}
        }
    });
    let resp = server.handle_message("textDocument/hover", &hover).unwrap();
    assert_eq!(resp["id"], 7);
    assert!(resp["result"].is_null());
}

#[test]
fn test_server_hover_malformed_params_returns_32602() {
    let mut server = make_server();
    let no_pos = serde_json::json!({
        "id": 11,
        "params": {"textDocument": {"uri": "file:///a.rsc"}}
    });
    let resp = server
        .handle_message("textDocument/hover", &no_pos)
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 11, "id must be echoed on error responses");
}

#[test]
fn test_server_did_change_no_uri_returns_none() {
    let mut server = make_server();
    let msg = serde_json::json!({
        "params": {"contentChanges": [{"text": "hi"}]}
    });
    let resp = server.handle_message("textDocument/didChange", &msg);
    assert!(resp.is_none());
}
