// White-box: server basic.

use crate::menus::MenuData;
use crate::server::{Server, exit_code};
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
fn test_server_initialize_advertises_code_action_provider() {
    // Quick-fixes ("Did you mean …?") must be advertised so Zed offers
    // the lightbulb action on unknown-property / unknown-menu squiggles.
    let mut server = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let resp = server.handle_message("initialize", &msg).unwrap();
    assert_eq!(resp["result"]["capabilities"]["codeActionProvider"], true);
}

#[test]
fn test_server_initialize() {
    let mut server = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let resp = server.handle_message("initialize", &msg).unwrap();
    let sync = &resp["result"]["capabilities"]["textDocumentSync"];
    assert_eq!(sync["openClose"], true);
    assert_eq!(sync["change"], 2, "incremental sync must be advertised");
    assert_eq!(resp["result"]["capabilities"]["hoverProvider"], true);
    assert_eq!(resp["result"]["serverInfo"]["name"], "mikrotik-rsc-ls");
    // Assert against the crate version, not a literal, so version bumps
    // don't break this test.
    assert_eq!(
        resp["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn test_server_shutdown() {
    let mut server = make_server();
    assert!(!server.shutdown_received);
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "shutdown",
        "params": {}
    });
    let resp = server.handle_message("shutdown", &msg).unwrap();
    assert_eq!(resp["result"], serde_json::Value::Null);
    assert!(
        server.shutdown_received,
        "answering shutdown must latch shutdown_received"
    );
}

#[test]
fn test_exit_code_lsp_317() {
    // LSP 3.17: exit status 0 only after a `shutdown` request; else 1.
    assert_eq!(exit_code(true), 0);
    assert_eq!(exit_code(false), 1);
    // Fresh server: no shutdown seen yet → a bare `exit` maps to status 1.
    let fresh = Server::new(synthetic_data());
    assert!(!fresh.shutdown_received);
    assert_eq!(exit_code(fresh.shutdown_received), 1);
    // After answering `shutdown`, the same server maps to status 0.
    let mut server = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "shutdown",
        "params": {}
    });
    server.handle_message("shutdown", &msg).unwrap();
    assert_eq!(exit_code(server.shutdown_received), 0);
}

#[test]
fn test_server_unknown_method_with_id_returns_error() {
    let mut server = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "unknown/method",
        "params": {}
    });
    let resp = server.handle_message("unknown/method", &msg).unwrap();
    assert_eq!(resp["error"]["code"], -32601);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown/method")
    );
}

#[test]
fn test_server_unknown_notification_returns_none() {
    let mut server = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "unknown/method",
        "params": {}
    });
    let resp = server.handle_message("unknown/method", &msg);
    assert!(resp.is_none(), "notification without id should return None");
}

#[test]
fn test_server_did_open_and_completion() {
    let mut server = make_server();
    // Open doc
    let open = serde_json::json!({
        "params": {
            "textDocument": {"uri": "file:///test.rsc", "text": "/ip/address add "}
        }
    });
    assert!(
        server
            .handle_message("textDocument/didOpen", &open)
            .is_none()
    );
    assert!(server.docs.contains_key("file:///test.rsc"));

    // Completion request
    let comp = serde_json::json!({
        "id": 10,
        "params": {
            "textDocument": {"uri": "file:///test.rsc"},
            "position": {"line": 0, "character": 15}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    assert!(!items.is_empty());
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"address"));
    assert!(labels.contains(&"interface"));
}

#[test]
fn test_server_completion_untracked_uri_returns_null_result() {
    // This previously asserted `resp.is_none()` — a request
    // carrying an id got NO response and the client hung until timeout.
    // Untracked URI now yields a spec-permitted null result with the id
    // echoed.
    let mut server = make_server();
    let comp = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///notopened.rsc"},
            "position": {"line": 0, "character": 1}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .unwrap();
    assert_eq!(resp["id"], 1, "id must be echoed");
    assert!(resp["result"].is_null(), "untracked URI → null result");
}

#[test]
fn test_server_completion_malformed_params_returns_32602() {
    let mut server = make_server();
    // Missing position entirely.
    let no_pos = serde_json::json!({
        "id": 7,
        "params": {"textDocument": {"uri": "file:///a.rsc"}}
    });
    let resp = server
        .handle_message("textDocument/completion", &no_pos)
        .unwrap();
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["code"], -32602);
    // Missing URI entirely.
    let no_uri = serde_json::json!({
        "id": 8,
        "params": {"position": {"line": 0, "character": 0}}
    });
    let resp = server
        .handle_message("textDocument/completion", &no_uri)
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    // Non-numeric position.
    let bad_types = serde_json::json!({
        "id": 9,
        "params": {
            "textDocument": {"uri": "file:///a.rsc"},
            "position": {"line": "zero", "character": null}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &bad_types)
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 9);
}
