//! Server — code actions (kinds).
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

// ── Server handle_message integration ─────────────────────────

fn make_server() -> Server {
    Server::new(synthetic_data())
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
    // A REAL Rule 5 diagnostic whose value is hopeless: nothing within the
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
