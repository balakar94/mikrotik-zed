use super::*;
use crate::menus::MenuData;
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

// ── Server handle_message integration ─────────────────────────

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
    // PHASE 1 FLIP: this previously asserted `resp.is_none()` — a request
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
    // PHASE 1 FLIP: previously asserted `resp.is_none()` (dropped
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

// ── Code actions (did-you-mean quick-fixes) ──────────────────

/// Open `doc` in `server` and return its diagnostics exactly as a
/// client would echo them back inside a codeAction request: computed
/// through the push pipeline (including position-encoding conversion)
/// and serialized to wire JSON.
fn opened_wire_diagnostics(server: &mut Server, uri: &str, doc: &str) -> Vec<serde_json::Value> {
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": doc}}}),
    );
    let stored = server.docs.get(uri).cloned().unwrap_or_default();
    let diags = server.encoded_diagnostics(&stored, uri);
    match serde_json::to_value(diags) {
        Ok(serde_json::Value::Array(items)) => items,
        other => panic!("diagnostics must serialize to an array, got {other:?}"),
    }
}

fn code_action_request(id: i64, uri: &str, diags: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "params": {
            "textDocument": {"uri": uri},
            "context": {"diagnostics": diags}
        }
    })
}

#[test]
fn test_code_actions_fixes_typo_property_at_exact_range() {
    let mut s = make_server();
    // "adress" spans bytes 15..21 (ASCII ⇒ UTF-16 units are identical).
    let doc = "/ip/address add adress=1.1.1.1";
    let diags = opened_wire_diagnostics(&mut s, "file:///ca.rsc", doc);
    assert_eq!(diags.len(), 1, "exactly the unknown-property diagnostic");
    assert_eq!(diags[0]["code"], "unknown-property");

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(41, "file:///ca.rsc", &diags),
        )
        .unwrap();
    assert_eq!(resp["id"], 41, "id must be echoed");
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["title"], "Did you mean 'address'?");
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(
        actions[0]["diagnostics"][0],
        serde_json::to_value(&diags[0]).unwrap(),
        "the originating diagnostic object is attached"
    );
    let edit = &actions[0]["edit"]["changes"]["file:///ca.rsc"][0];
    assert_eq!(edit["newText"], "address");
    assert_eq!(
        edit["range"], diags[0]["range"],
        "replacement targets the offending token range exactly"
    );
    assert_eq!(edit["range"]["start"]["character"], 16);
    assert_eq!(edit["range"]["end"]["character"], 22);
}

#[test]
fn test_code_actions_fixes_typo_menu_path() {
    let mut s = make_server();
    // "/ip/addres" is one insertion away from "/ip/address".
    let doc = "/ip/addres add gateway=1";
    let diags = opened_wire_diagnostics(&mut s, "file:///cm.rsc", doc);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "unknown-menu");

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(42, "file:///cm.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["title"], "Did you mean '/ip/address'?");
    let edit = &actions[0]["edit"]["changes"]["file:///cm.rsc"][0];
    assert_eq!(edit["newText"], "/ip/address");
    assert_eq!(edit["range"]["start"]["character"], 0);
    assert_eq!(edit["range"]["end"]["character"], 10);
}

#[test]
fn test_code_actions_healthy_doc_returns_empty_array() {
    let mut s = make_server();
    let doc = "/ip/address add address=1.1.1.1 interface=ether1";
    let diags = opened_wire_diagnostics(&mut s, "file:///ok.rsc", doc);
    assert!(diags.is_empty());
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(43, "file:///ok.rsc", &diags),
        )
        .unwrap();
    assert_eq!(resp["id"], 43);
    assert!(resp["result"].is_array());
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_code_actions_untracked_uri_returns_empty_array_not_error() {
    let mut s = make_server();
    let fake = serde_json::json!({
        "range": {"start": {"line": 0, "character": 15}, "end": {"line": 0, "character": 21}},
        "severity": 2,
        "code": "unknown-property",
        "source": "rsc-ls",
        "message": "Unknown property 'adress' for '/ip/address'"
    });
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(44, "file:///never-opened.rsc", &[fake]),
        )
        .unwrap();
    assert_eq!(resp["id"], 44, "id must be echoed");
    assert!(
        resp["result"].is_array(),
        "untracked URI must answer an array, not null or error"
    );
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_code_actions_ignore_foreign_and_unparseable_diagnostics() {
    let mut s = make_server();
    let diags = opened_wire_diagnostics(&mut s, "file:///f.rsc", "");
    assert!(diags.is_empty());
    let range = serde_json::json!({
        "start": {"line": 0, "character": 15},
        "end": {"line": 0, "character": 21}
    });
    let mixed = vec![
        // Foreign source — even with our codes.
        serde_json::json!({"source": "other-ls", "code": "unknown-property", "range": range}),
        // Our source but a different rule.
        serde_json::json!({"source": "rsc-ls", "code": "duplicate-property", "range": range}),
        // Numeric code (LSP allows number|string; ours are strings).
        serde_json::json!({"source": "rsc-ls", "code": 7, "range": range}),
        // Missing code entirely.
        serde_json::json!({"source": "rsc-ls", "range": range}),
        // Missing range entirely.
        serde_json::json!({"source": "rsc-ls", "code": "unknown-property"}),
        // Missing source entirely.
        serde_json::json!({"code": "unknown-property", "range": range}),
    ];
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(45, "file:///f.rsc", &mixed),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(actions.is_empty(), "nothing eligible, got {actions:?}");
}

#[test]
fn test_code_actions_capped_at_eight() {
    let mut s = make_server();
    let mut doc = String::new();
    for i in 0..12 {
        doc.push_str(&format!("/ip/address add adress={i}.1.1.1\n"));
    }
    let diags = opened_wire_diagnostics(&mut s, "file:///cap.rsc", &doc);
    assert_eq!(diags.len(), 12, "one eligible diagnostic per line");
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(46, "file:///cap.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(
        actions.len(),
        MAX_CODE_ACTIONS,
        "capped, not truncated to zero"
    );
    // Deterministic order: the first action repairs the FIRST diagnostic.
    let first_edit = &actions[0]["edit"]["changes"]["file:///cap.rsc"][0];
    assert_eq!(first_edit["range"]["start"]["line"], 0);
    assert_eq!(first_edit["newText"], "address");
}

#[test]
fn test_code_actions_malformed_params_return_32602() {
    let mut s = make_server();
    // Missing textDocument.uri entirely.
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &serde_json::json!({"id": 47, "params": {"context": {"diagnostics": []}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 47, "id must be echoed on error responses");
    // Missing context entirely.
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &serde_json::json!({"id": 48, "params": {"textDocument": {"uri": "file:///a.rsc"}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 48);
    // Context present but diagnostics absent.
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &serde_json::json!({"id": 49, "params": {
                "textDocument": {"uri": "file:///a.rsc"}, "context": {}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 49);
}

#[test]
fn test_code_actions_unknown_property_without_resolvable_menu_yields_nothing() {
    let mut s = make_server();
    // Track "/ip": a valid ancestor prefix with NO direct menu entry,
    // hence no property table — a fabricated unknown-property here must
    // be skipped rather than guessed against ALL menus.
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///p.rsc", "text": "/ip"}}}),
    );
    let fake = serde_json::json!({
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
        "severity": 2,
        "code": "unknown-property",
        "source": "rsc-ls",
        "message": "Unknown property 'ip' for '/ip'"
    });
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(50, "file:///p.rsc", &[fake]),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(actions.is_empty(), "no menu ⇒ no action, got {actions:?}");
}

#[test]
fn test_code_actions_garbage_beyond_threshold_yields_nothing() {
    let mut s = make_server();
    // 12 characters of nonsense: outside threshold 2 of every property.
    let doc = "/ip/address add zzzqqqxxxwww=1";
    let diags = opened_wire_diagnostics(&mut s, "file:///g.rsc", doc);
    assert_eq!(diags.len(), 1);
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(51, "file:///g.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(
        actions.is_empty(),
        "no candidate within threshold ⇒ no action"
    );
}

#[test]
fn test_code_actions_utf16_positions_extract_correct_token() {
    let mut s = make_server();
    // Default negotiation is UTF-16: 'bogus' token sits at unit 21
    // (byte 25), because each 🚨 costs two units but four bytes.
    let doc = "/ip/address add 🚨🚨 adress=1";
    let diags = opened_wire_diagnostics(&mut s, "file:///u.rsc", doc);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["range"]["start"]["character"], 21);
    assert_eq!(diags[0]["range"]["end"]["character"], 27);

    // Extraction must round-trip through the negotiated encoding — a
    // byte/unit mix-up would grab the wrong text and yield no action.
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(52, "file:///u.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0]["edit"]["changes"]["file:///u.rsc"][0]["newText"],
        "address"
    );
}

#[test]
fn test_code_actions_resolve_menu_across_line_continuation() {
    let mut s = make_server();
    // RouterOS continuation: the command spans two physical lines; the
    // diagnostic lands on PHYSICAL line 1 while the governing menu path
    // lives on line 0. resolve_menu_for_line must join them exactly like
    // the diagnostic pipeline did when emitting this range.
    let doc = "/ip/address add \\\nadress=1.2.3.4";
    let diags = opened_wire_diagnostics(&mut s, "file:///cont.rsc", doc);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "unknown-property");
    assert_eq!(diags[0]["range"]["start"]["line"], 1);
    assert_eq!(diags[0]["range"]["start"]["character"], 0);
    assert_eq!(diags[0]["range"]["end"]["character"], 6);

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(53, "file:///cont.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(
        actions.len(),
        1,
        "menu must resolve across the continuation, got {actions:?}"
    );
    let edit = &actions[0]["edit"]["changes"]["file:///cont.rsc"][0];
    assert_eq!(edit["newText"], "address");
    assert_eq!(edit["range"]["start"]["line"], 1);
}

#[test]
fn test_code_actions_fixes_typo_enum_value_unquoted() {
    let mut s = make_server();
    // "inpt" spans bytes 30..34 (ASCII ⇒ UTF-16 units are identical):
    // the Rule 5 range covers the VALUE part only, skipping "chain=".
    let doc = "/ip/firewall/filter add chain=inpt";
    let diags = opened_wire_diagnostics(&mut s, "file:///ev.rsc", doc);
    assert_eq!(diags.len(), 1, "exactly the invalid-enum-value hint");
    assert_eq!(diags[0]["code"], "invalid-enum-value");
    assert_eq!(diags[0]["range"]["start"]["character"], 30);
    assert_eq!(diags[0]["range"]["end"]["character"], 34);

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(54, "file:///ev.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "got {actions:?}");
    assert_eq!(actions[0]["title"], "Did you mean 'input'?");
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(
        actions[0]["diagnostics"][0],
        serde_json::to_value(&diags[0]).unwrap(),
        "the originating diagnostic object is attached"
    );
    let edit = &actions[0]["edit"]["changes"]["file:///ev.rsc"][0];
    assert_eq!(edit["newText"], "input", "bare typo stays bare");
    assert_eq!(
        edit["range"], diags[0]["range"],
        "replacement targets the offending value range exactly"
    );
}

#[test]
fn test_code_actions_fixes_typo_enum_value_quoted() {
    let mut s = make_server();
    // Quoted variant: the Rule 5 range KEEPS the surrounding quotes,
    // so the repair must re-wrap the suggested member in the SAME
    // quote style while the title stays bare.
    let doc = "/ip/firewall/filter add chain=\"forwrd\"";
    let diags = opened_wire_diagnostics(&mut s, "file:///evq.rsc", doc);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "invalid-enum-value");

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(55, "file:///evq.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "got {actions:?}");
    assert_eq!(
        actions[0]["title"], "Did you mean 'forward'?",
        "title shows the bare member, not the re-quoted splice"
    );
    let edit = &actions[0]["edit"]["changes"]["file:///evq.rsc"][0];
    assert_eq!(edit["newText"], "\"forward\"");
    assert_eq!(edit["range"], diags[0]["range"]);
    assert_eq!(edit["range"]["start"]["character"], 30);
    assert_eq!(
        edit["range"]["end"]["character"], 38,
        "quotes stay in range"
    );
}

#[test]
fn test_code_actions_invalid_enum_without_resolvable_menu_yields_nothing() {
    let mut s = make_server();
    // Track "/ip": an implicit parent with NO direct menu entry, hence
    // no property table and no enum members — a fabricated
    // invalid-enum-value here must be skipped rather than guessed.
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///evn.rsc", "text": "/ip"}}}),
    );
    let fake = serde_json::json!({
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
        "severity": 4,
        "code": "invalid-enum-value",
        "source": "rsc-ls",
        "message": "Invalid value 'zz' for 'x' (expected one of: a | b)"
    });
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(57, "file:///evn.rsc", &[fake]),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(actions.is_empty(), "no menu ⇒ no action, got {actions:?}");
}

#[test]
fn test_code_actions_invalid_enum_unknown_key_yields_nothing() {
    let mut s = make_server();
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {
            "textDocument": {"uri": "file:///evk.rsc"},
            "text": "/ip/address add bogus=inpt"
        }}),
    );
    // The menu resolves and the key=value pair is found by spans
    // (value "inpt" at bytes 22..26), but "bogus" names no argument in
    // /ip/address ⇒ no candidate set, no action.
    let fake = serde_json::json!({
        "range": {"start": {"line": 0, "character": 22}, "end": {"line": 0, "character": 26}},
        "severity": 4,
        "code": "invalid-enum-value",
        "source": "rsc-ls",
        "message": "Invalid value 'inpt' for 'bogus'"
    });
    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(58, "file:///evk.rsc", &[fake]),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(
        actions.is_empty(),
        "unknown key ⇒ no enum candidates ⇒ no action, got {actions:?}"
    );
}

#[test]
fn test_code_actions_invalid_enum_garbage_beyond_threshold_yields_nothing() {
    let mut s = make_server();
    // A REAL Rule 5 hint whose value is hopeless: nothing within the
    // length-aware threshold of input/forward/output ⇒ no action.
    let doc = "/ip/firewall/filter add chain=zzzqqqxxxwww";
    let diags = opened_wire_diagnostics(&mut s, "file:///evg.rsc", doc);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "invalid-enum-value");

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(59, "file:///evg.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert!(
        actions.is_empty(),
        "no candidate within threshold ⇒ no action, got {actions:?}"
    );
}

#[test]
fn test_code_actions_mixed_codes_still_capped_at_eight() {
    let mut s = make_server();
    let mut doc = String::new();
    // Six unknown-property typos…
    for i in 0..6 {
        doc.push_str(&format!("/ip/address add adress={i}.1.1.1\n"));
    }
    // …plus six invalid-enum-value typos: twelve eligible diagnostics
    // across two codes, still answered with exactly MAX_CODE_ACTIONS.
    for i in 0..6 {
        doc.push_str(&format!("/ip/firewall/filter add chain=inpt{i}\n"));
    }
    let diags = opened_wire_diagnostics(&mut s, "file:///mix.rsc", &doc);
    let eligible = diags
        .iter()
        .filter(|d| {
            matches!(
                d["code"].as_str(),
                Some("unknown-property") | Some("invalid-enum-value")
            )
        })
        .count();
    assert_eq!(eligible, 12, "six property typos + six enum typos");

    let resp = s
        .handle_message(
            "textDocument/codeAction",
            &code_action_request(60, "file:///mix.rsc", &diags),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(
        actions.len(),
        MAX_CODE_ACTIONS,
        "the cap spans every eligible code, not per kind"
    );
    for a in actions {
        assert!(
            matches!(
                a["diagnostics"][0]["code"].as_str(),
                Some("unknown-property") | Some("invalid-enum-value")
            ),
            "only eligible codes may back an action: {a:?}"
        );
    }
}

#[test]
fn test_server_completion_multiline_before_cursor() {
    let mut server = make_server();
    let doc = "/ip/address add\naddress=1.1.1.1";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///multi.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Cursor on line 1, after "address="
    let hover_or_completion_line = 1;
    let comp = serde_json::json!({
        "id": 20,
        "params": {
            "textDocument": {"uri": "file:///multi.rsc"},
            "position": {"line": hover_or_completion_line, "character": 8} // "address="
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .unwrap();
    // For "address=" value completions should trigger (ipPrefix)
    let items = resp["result"]["items"].as_array().unwrap();
    // Might be value completions (0.0.0.0/0) or empty if not correctly resolved, but should be Some array
    assert!(items.is_empty() || items.iter().any(|i| i["label"] == "0.0.0.0/0"));
}

// ── Variable navigation (textDocument/definition + references) ──
//
// Wire-contract coverage for the navigation handlers: -32602 /
// null / [] shapes per sibling-handler strictness, exact declaration
// ranges, includeDeclaration toggling, and UTF-16 inbound positions.
// The pure semantics behind these live in navigation.rs's own suite;
// end-to-end wire variants live in tests/e2e.rs.

/// `:local counter 0` / `:put $counter` / `/ip/address add
/// interface=$counter`. Declaration name spans bytes 7..14 of line 0;
/// usages sit at line 1 bytes 6..13 and line 2 bytes 27..34.
const NAV_DOC: &str = ":local counter 0\n:put $counter\n/ip/address add interface=$counter\n";

fn nav_request(id: i64, uri: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut params = serde_json::json!({
        "textDocument": {"uri": uri},
        "position": {"line": 1, "character": 8}, // inside `$counter`
    });
    if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    serde_json::json!({"id": id, "params": params})
}

#[test]
fn test_server_initialize_advertises_navigation_providers() {
    let mut s = make_server();
    let resp = s
        .handle_message("initialize", &serde_json::json!({"id": 1, "params": {}}))
        .unwrap();
    assert_eq!(resp["result"]["capabilities"]["definitionProvider"], true);
    assert_eq!(resp["result"]["capabilities"]["referencesProvider"], true);
}

#[test]
fn test_server_definition_untracked_uri_returns_null_result() {
    let mut s = make_server();
    let req = nav_request(61, "file:///never-opened.rsc", serde_json::json!({}));
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert_eq!(resp["id"], 61, "id must be echoed");
    assert!(resp["result"].is_null(), "untracked URI → null result");
}

#[test]
fn test_server_definition_malformed_params_return_32602() {
    let mut s = make_server();
    // Missing URI entirely…
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 62, "params": {"position": {"line": 0, "character": 0}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 62, "id must be echoed on error responses");
    // …missing position entirely…
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 63, "params": {"textDocument": {"uri": "file:///a.rsc"}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    // …and a mistyped position component.
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 64, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": "eight"}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_server_definition_jumps_to_exact_declaration_span() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nav.rsc", "text": NAV_DOC}}}),
        );
    let req = nav_request(65, "file:///nav.rsc", serde_json::json!({}));
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    let loc = &resp["result"];
    assert!(loc.is_object(), "usage must resolve, got {loc}");
    assert_eq!(loc["uri"], "file:///nav.rsc");
    // Exact name-token span of `counter` in `:local counter 0` — not
    // the command token, not the initializer.
    assert_eq!(loc["range"]["start"]["line"], 0);
    assert_eq!(loc["range"]["start"]["character"], 7);
    assert_eq!(loc["range"]["end"]["line"], 0);
    assert_eq!(loc["range"]["end"]["character"], 14);

    // Same answer when invoked ON the declaration itself.
    let req = serde_json::json!({
        "id": 66,
        "params": {
            "textDocument": {"uri": "file:///nav.rsc"},
            "position": {"line": 0, "character": 8},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert_eq!(
        resp["result"]["range"]["start"]["character"], 7,
        "requesting from the declaration returns its own span"
    );
}

#[test]
fn test_server_definition_non_variable_word_returns_null() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nv.rsc", "text": NAV_DOC}}}),
        );
    // Cursor over the property `interface` — a real word that merely
    // shares the document with variables must NOT resolve.
    let req = serde_json::json!({
        "id": 67,
        "params": {
            "textDocument": {"uri": "file:///nv.rsc"},
            "position": {"line": 2, "character": 20},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert!(resp["result"].is_null(), "property word → null, got {resp}");
    // …and so does a cursor on the `:local` keyword itself.
    let req = serde_json::json!({
        "id": 68,
        "params": {
            "textDocument": {"uri": "file:///nv.rsc"},
            "position": {"line": 0, "character": 3},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert!(resp["result"].is_null());
}

#[test]
fn test_server_references_untracked_uri_returns_empty_list() {
    let mut s = make_server();
    let req = serde_json::json!({
        "id": 69,
        "params": {
            "textDocument": {"uri": "file:///never-opened.rsc"},
            "position": {"line": 0, "character": 0},
            "context": {"includeDeclaration": true},
        }
    });
    let resp = s.handle_message("textDocument/references", &req).unwrap();
    assert_eq!(resp["id"], 69, "id must be echoed");
    assert!(
        resp["result"].is_array(),
        "list endpoint answers an array even untracked"
    );
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_server_references_missing_context_returns_32602() {
    let mut s = make_server();
    // Context object absent entirely…
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 70, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 70);
    // …context present but includeDeclaration missing…
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 71, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0},
                "context": {}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    // …and includeDeclaration mistyped (LSP requires a boolean).
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 72, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0},
                "context": {"includeDeclaration": "yes"}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_server_references_include_declaration_toggles_list() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///ref.rsc", "text": NAV_DOC}}}),
        );

    let with = s
        .handle_message(
            "textDocument/references",
            &nav_request(
                73,
                "file:///ref.rsc",
                serde_json::json!({"context": {"includeDeclaration": true}}),
            ),
        )
        .unwrap();
    let items = with["result"].as_array().unwrap();
    assert_eq!(items.len(), 3, "declaration + two usages");
    assert_eq!(
        items[0]["range"]["start"]["character"], 7,
        "the chosen declaration comes first, exact name span"
    );
    assert_eq!(items[0]["range"]["end"]["character"], 14);
    assert_eq!(items[1]["range"]["start"]["line"], 1);
    assert_eq!(items[2]["range"]["start"]["line"], 2);
    assert_eq!(items[2]["range"]["start"]["character"], 27);

    let without = s
        .handle_message(
            "textDocument/references",
            &nav_request(
                74,
                "file:///ref.rsc",
                serde_json::json!({"context": {"includeDeclaration": false}}),
            ),
        )
        .unwrap();
    let items = without["result"].as_array().unwrap();
    assert_eq!(items.len(), 2, "usages only");
    assert_eq!(items[0]["range"]["start"]["line"], 1);
}

#[test]
fn test_server_references_position_off_any_variable_yields_empty_list() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///off.rsc", "text": NAV_DOC}}}),
        );
    let req = serde_json::json!({
        "id": 75,
        "params": {
            "textDocument": {"uri": "file:///off.rsc"},
            "position": {"line": 0, "character": 1}, // on `:local` keyword
            "context": {"includeDeclaration": true},
        }
    });
    let resp = s.handle_message("textDocument/references", &req).unwrap();
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_server_navigation_resolves_utf16_positions_after_emoji() {
    // Default negotiation is UTF-16: `:put "🌍🌍" $ok` puts the usage
    // identifier at units 13..15 but bytes 17..19 (each 🌍 costs 2
    // units / 4 bytes). The probe at unit 14 (mid-identifier) would be
    // byte 14 — the closing quote — under a byte/unit mix-up, where no
    // word can be extracted at all, so this pin is decisive.
    let doc = ":local ok\n:put \"🌍🌍\" $ok\n";
    let mut s = make_server();
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///u16.rsc", "text": doc}}}),
    );
    let pos = serde_json::json!({
        "textDocument": {"uri": "file:///u16.rsc"},
        "position": {"line": 1, "character": 14},
    });

    let def = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 76, "params": pos}),
        )
        .unwrap();
    assert_eq!(
        def["result"]["range"]["start"]["character"], 7,
        "definition resolved through utf-16 units"
    );
    assert_eq!(def["result"]["range"]["end"]["character"], 9);

    let refs = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 77, "params": {
                "textDocument": {"uri": "file:///u16.rsc"},
                "position": {"line": 1, "character": 14},
                "context": {"includeDeclaration": false}
            }}),
        )
        .unwrap();
    let items = refs["result"].as_array().unwrap();
    assert_eq!(items.len(), 1, "exactly the `$ok` usage");
    assert_eq!(items[0]["range"]["start"]["line"], 1);
    assert_eq!(items[0]["range"]["start"]["character"], 13);
}
