// Server — code actions (recovery).
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
    assert_eq!(diags.len(), 1, "exactly the invalid-enum-value warning");
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
