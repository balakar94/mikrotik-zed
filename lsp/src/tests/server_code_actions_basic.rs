// Server — code actions (basic).
use crate::caps::MAX_CODE_ACTIONS;
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

// ── Code actions (did-you-mean quick-fixes) ──────────────────────────────

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
