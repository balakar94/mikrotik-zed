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
fn test_requests_after_shutdown_error_invalid_request() {
    // LSP 3.17 lifecycle: after shutdown, requests must be rejected with
    // InvalidRequest (-32600) until `exit`.
    let mut server = make_server();
    let shutdown = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "shutdown",
        "params": {}
    });
    assert!(server.handle_message("shutdown", &shutdown).is_some());

    let hover = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "textDocument/hover",
        "params": {
            "textDocument": {"uri": "file:///a.rsc"},
            "position": {"line": 0, "character": 0}
        }
    });
    let resp = server.handle_message("textDocument/hover", &hover).unwrap();
    assert_eq!(resp["error"]["code"], -32600);
    assert_eq!(resp["id"], 3, "error response must echo the request id");

    // Notifications after shutdown are ignored: no id, no response.
    let did_close = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didClose",
        "params": {"textDocument": {"uri": "file:///a.rsc"}}
    });
    assert!(
        server
            .handle_message("textDocument/didClose", &did_close)
            .is_none()
    );
}

#[test]
fn test_cancel_request_is_notification_noop() {
    // `$/cancelRequest` is a documented no-op on this single-threaded
    // server: the in-flight request has already finished by the time the
    // cancellation is read, so there is nothing to interrupt.
    let mut server = make_server();
    let cancel = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "$/cancelRequest",
        "params": {"id": 99}
    });
    assert!(server.handle_message("$/cancelRequest", &cancel).is_none());

    // Tolerate the non-conforming request form: answer with null so the
    // client never waits for a response.
    let cancel_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "$/cancelRequest",
        "params": {"id": 99}
    });
    let resp = server
        .handle_message("$/cancelRequest", &cancel_request)
        .unwrap();
    assert_eq!(resp["result"], serde_json::Value::Null);
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

#[test]
fn test_completion_is_incomplete_when_item_cap_hit() {
    // A value list larger than MAX_COMPLETION_ITEMS is truncated; the
    // response must say so (`isIncomplete: true`) or clients cache the
    // truncated list as final.
    use crate::caps::MAX_COMPLETION_ITEMS;
    let mut toml = String::from(
        "[[menus]]\npath = \"/demo/many\"\ntype = \"Directory\"\n\
         [[menus.arguments]]\nname = \"mode\"\ntype = \"enum\"\nenum_values = [",
    );
    let values: Vec<String> = (0..(MAX_COMPLETION_ITEMS + 20))
        .map(|i| format!("\"v{i}\""))
        .collect();
    toml.push_str(&values.join(", "));
    toml.push_str("]\n");
    let data = Arc::new(MenuData::from_toml_str(&toml));
    let mut server = Server::new(data);
    let doc = "/demo/many add mode=";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///many.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 5,
        "params": {
            "textDocument": {"uri": "file:///many.rsc"},
            "position": {"line": 0, "character": doc.len()}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &req)
        .unwrap();
    let result = &resp["result"];
    assert_eq!(
        result["items"].as_array().unwrap().len(),
        MAX_COMPLETION_ITEMS,
        "list must be capped"
    );
    assert_eq!(
        result["isIncomplete"], true,
        "a capped list must be marked incomplete: {result}"
    );
}

#[test]
fn test_server_hover_slash_joined_commands_real_data() {
    // Runtime regression: slash-joined paths (menu + verb in one token),
    // commands inside `:do { }`, and properties on a `\`-continued
    // slash-joined command must all hover through the wire handler. Each
    // scenario gets its own document so the backward context walk cannot
    // join unrelated commands (a separate, documented limitation).
    let mut server = Server::new(Arc::new(MenuData::load()));
    let block_doc = ":do { /ipv6/address/remove [find] } on-error={}\n";
    let cont_doc = "/ipv6/address/add advertise=no \\\n  comment=test\n";
    for (uri, text) in [
        ("file:///slash-block.rsc", block_doc),
        ("file:///slash-cont.rsc", cont_doc),
    ] {
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": text}}
        });
        server.handle_message("textDocument/didOpen", &open);
    }
    let req = |id: i64, uri: &str, line: u64, character: u64| {
        serde_json::json!({
            "id": id,
            "params": {
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": character}
            }
        })
    };
    let menu = server
        .handle_message(
            "textDocument/hover",
            &req(1, "file:///slash-block.rsc", 0, 14),
        )
        .unwrap();
    let menu_val = menu["result"]["contents"]["value"].as_str().unwrap_or("");
    assert!(
        menu_val.contains("### /ipv6/address"),
        "block menu hover: {menu_val}"
    );
    let verb = server
        .handle_message(
            "textDocument/hover",
            &req(2, "file:///slash-block.rsc", 0, 22),
        )
        .unwrap();
    let verb_val = verb["result"]["contents"]["value"].as_str().unwrap_or("");
    assert!(
        verb_val.contains("**remove**"),
        "block verb hover: {verb_val}"
    );
    let prop = server
        .handle_message(
            "textDocument/hover",
            &req(3, "file:///slash-cont.rsc", 0, 20),
        )
        .unwrap();
    let prop_val = prop["result"]["contents"]["value"].as_str().unwrap_or("");
    assert!(
        prop_val.contains("**advertise**"),
        "continued property hover: {prop_val}"
    );
}
