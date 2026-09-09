//! White-box: diag completion.

use crate::diagnostics;
use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;

fn synth() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
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
"#,
    ))
}

fn make_server() -> Server {
    Server::new(synth())
}

// ── publishDiagnostics caps and incremental ────────────────────────

#[test]
fn test_diagnostic_pull_and_push_consistency() {
    let mut s = make_server();
    let doc = "/unknown/menu add x=1";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///c.rsc", "text": doc}}}),
    );
    let pull = s.handle_message(
        "textDocument/diagnostic",
        &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "file:///c.rsc"}}}),
    );
    let pull_items = pull.unwrap()["result"]["items"].as_array().unwrap().len();
    let direct = diagnostics::compute_diagnostics(&synth(), doc, "file:///c.rsc").len();
    assert_eq!(pull_items, direct);
}

#[test]
fn test_diagnostic_pull_invalid_uri_returns_empty() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/unknown/menu add x=1"}}}));
    let resp = s
        .handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "untitled://a.rsc"}}}),
        )
        .unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    assert!(items.is_empty());
}

#[test]
fn test_large_doc_diagnostics_capped() {
    let data = synth();
    let doc = "/unknown/menu add x=1\n".repeat(4000);
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///a.rsc");
    assert!(diags.len() <= 3001);
}

#[test]
fn test_large_doc_truncation_preserves_first() {
    let data = synth();
    let mut doc = String::new();
    doc.push_str("/unknown/first add x=1\n");
    doc.push_str(&"/unknown/other add x=1\n".repeat(5000));
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///a.rsc");
    assert!(diags.iter().any(|d| d.message.contains("/unknown/first")));
    assert_eq!(diags[0].range.start.line, 0);
}

// ── Completion integration ────────────────────────────────────────

#[test]
fn test_completion_for_empty_context_returns_roots() {
    let mut s = make_server();
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": ""}}}),
    );
    let resp = s.handle_message("textDocument/completion", &serde_json::json!({"id": 1, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 0}}}));
    let items = resp.unwrap()["result"]["items"].as_array().unwrap().clone();
    assert!(!items.is_empty());
    assert!(items.iter().any(|i| i["label"] == "/ip"));
}

#[test]
fn test_completion_for_args_after_verb() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address add "}}}));
    let resp = s.handle_message("textDocument/completion", &serde_json::json!({"id": 2, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 15}}}));
    let items = resp.unwrap()["result"]["items"].as_array().unwrap().clone();
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(labels.contains(&"address"));
    assert!(labels.contains(&"interface"));
}

#[test]
fn test_completion_for_values_after_equals() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/firewall/filter add chain="}}}));
    let resp = s
            .handle_message("textDocument/completion", &serde_json::json!({"id": 3, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 30}}}))
            .unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    assert!(items.iter().any(|i| i["label"] == "input"));
}

#[test]
fn test_hover_returns_correct_for_menu() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address"}}}));
    let resp = s.handle_message("textDocument/hover", &serde_json::json!({"id": 4, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 4}}}));
    let val = resp.unwrap()["result"]["contents"]["value"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(val.contains("/ip/address"));
}

#[test]
fn test_hover_unknown_returns_null() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/unknown/menu"}}}));
    let resp = s.handle_message("textDocument/hover", &serde_json::json!({"id": 5, "params": {"textDocument": {"uri": "file:///a.rsc"}, "position": {"line": 0, "character": 5}}}));
    assert!(resp.unwrap()["result"].is_null());
}

#[test]
fn test_incremental_edit_applied_then_diagnostics_updated() {
    let mut s = make_server();
    s.handle_message("textDocument/didOpen", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc", "text": "/ip/address add address=1.1.1.1 interface=ether1"}}}));
    // Valid, no missing
    let before = diagnostics::compute_diagnostics(
        &synth(),
        s.docs.get("file:///a.rsc").unwrap(),
        "file:///a.rsc",
    );
    assert!(
        !before
            .iter()
            .any(|d| d.code.as_deref() == Some("missing-required"))
    );
    // Incremental edit to break it
    s.handle_message("textDocument/didChange", &serde_json::json!({"params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 11}}, "text": "/unknown/menu"}]}}));
    let after_doc = s.docs.get("file:///a.rsc").unwrap();
    assert!(after_doc.starts_with("/unknown/menu"));
    let after = diagnostics::compute_diagnostics(&synth(), after_doc, "file:///a.rsc");
    assert!(
        after
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
}
