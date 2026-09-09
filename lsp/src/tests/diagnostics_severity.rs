//! Diagnostics — severity pins.
use crate::diagnostics;
use crate::diagnostics::severity;
use crate::diagnostics::*;
use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;
fn severity_data() -> Arc<MenuData> {
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
[[menus.arguments]]
name = "comment"
type = "string"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
"#,
    ))
}

fn code_of(
    diags: &[crate::diagnostics::Diagnostic],
    code: &str,
) -> Vec<crate::diagnostics::Diagnostic>
where
    crate::diagnostics::Diagnostic: Clone,
{
    diags
        .iter()
        .filter(|d| d.code.as_deref() == Some(code))
        .cloned()
        .collect()
}

// ── (d) diagnostics severity matrix ──────────────────────────────────

fn synth() -> MenuData {
    MenuData::from_toml_str(
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
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus]]
path = "/interface/list"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus]]
path = "/tool/ping"
type = "Command"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
[[menus.arguments]]
name = "ssl-verify"
type = "bool"
"#,
    )
}
// ── Explicit 5 rules with severity ─────────────────────────────────

#[test]
fn severity_severity_unknown_menu_is_warning() {
    let data = severity_data();
    let embedded = crate::menus::MenuData::load();
    let _ = &embedded;
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/foo/bar add x=1",
        "file:///severity-sev.rsc",
    );
    let found = code_of(&diags, "unknown-menu");
    assert_eq!(found.len(), 1, "one unknown-menu, got {diags:?}");
    assert_eq!(found[0].severity, Some(severity::WARNING));
    assert_eq!(found[0].source.as_deref(), Some("rsc-ls"));
}

#[test]
fn severity_severity_unknown_property_is_warning() {
    let data = severity_data();
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/ip/address add address=1.1.1.1 interface=ether1 bogus=1",
        "file:///severity-sev.rsc",
    );
    let found = code_of(&diags, "unknown-property");
    assert_eq!(found.len(), 1, "got {diags:?}");
    assert_eq!(found[0].severity, Some(severity::WARNING));
}

#[test]
fn severity_severity_missing_required_is_warning() {
    let data = severity_data();
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/ip/address add comment=hi",
        "file:///severity-sev.rsc",
    );
    let found = code_of(&diags, "missing-required");
    assert!(!found.is_empty(), "got {diags:?}");
    assert!(found.iter().all(|d| d.severity == Some(severity::WARNING)));
}

#[test]
fn severity_severity_invalid_enum_is_warning() {
    let data = severity_data();
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        "/ip/firewall/filter add chain=bogus action=accept",
        "file:///severity-sev.rsc",
    );
    let found = code_of(&diags, "invalid-enum-value");
    assert_eq!(found.len(), 1, "got {diags:?}");
    assert_eq!(found[0].severity, Some(severity::WARNING));
}

#[test]
fn severity_severity_syntax_errors_are_errors() {
    let data = severity_data();
    // Unclosed brace + unclosed quote each surface as severity Error (1).
    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        ":do {\n:put x\n",
        "file:///severity-sev.rsc",
    );
    let braces = code_of(&diags, "unclosed-brace");
    assert_eq!(braces.len(), 1, "got {diags:?}");
    assert_eq!(braces[0].severity, Some(severity::ERROR));

    let diags = crate::diagnostics::compute_diagnostics(
        &data,
        ":put \"never closed\n",
        "file:///severity-sev.rsc",
    );
    let quotes = code_of(&diags, "unclosed-quote");
    assert_eq!(quotes.len(), 1, "got {diags:?}");
    assert_eq!(quotes[0].severity, Some(severity::ERROR));
}

#[test]
fn severity_truncation_hint_is_information_with_truncated_code() {
    let data = severity_data();
    let doc = "/foo/unknown add badprop=1\n".repeat(4000);
    let diags = crate::diagnostics::compute_diagnostics(&data, &doc, "file:///severity-sev.rsc");
    let hints = code_of(&diags, "truncated");
    assert_eq!(
        hints.len(),
        1,
        "exactly one truncation footer, got {} diags",
        diags.len()
    );
    assert_eq!(hints[0].severity, Some(severity::INFORMATION));
    assert!(
        hints[0]
            .message
            .contains("some issues beyond limit not shown"),
        "footer suffix contract, got {:?}",
        hints[0].message
    );
}

// ── (d) quick-fix title suffix contract ──────────────────────────────

#[test]
fn severity_quickfix_title_ends_with_question_mark_suffix() {
    let mut server = Server::new(severity_data());
    let uri = "file:///severity-title.rsc";
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": "/ip/address add adress=1.1.1.1"}}}),
    );
    let stored = server.docs.get(uri).cloned().unwrap();
    let diags = server.encoded_diagnostics(&stored, uri);
    let wire = serde_json::to_value(&diags).unwrap();
    let wire_items = wire.as_array().unwrap().clone();
    let resp = server
        .handle_message(
            "textDocument/codeAction",
            &serde_json::json!({"id": 9001, "params": {
                "textDocument": {"uri": uri},
                "context": {"diagnostics": wire_items},
            }}),
        )
        .unwrap();
    let actions = resp["result"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    let title = actions[0]["title"].as_str().unwrap();
    assert!(
        title.starts_with("Did you mean '") && title.ends_with("'?"),
        "title suffix contract `Did you mean '<c>'?`, got {title:?}"
    );
    assert_eq!(actions[0]["kind"], "quickfix");
}

#[test]
fn test_rule1_unknown_menu_warning_severity() {
    let data = synth();
    let diags = compute_diagnostics(&data, "/foo/bar add x=1", "file:///a.rsc");
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-menu"))
        .expect("unknown-menu");
    assert_eq!(d.severity, Some(severity::WARNING));
    assert_eq!(d.source.as_deref(), Some("rsc-ls"));
    assert!(d.message.contains("/foo/bar"));
    assert_eq!(d.range.start.line, 0);
}

#[test]
fn test_rule2_unknown_property_warning_severity() {
    let data = synth();
    let diags = compute_diagnostics(
        &data,
        "/ip/address add address=1.1.1.1 interface=ether1 bogus=1",
        "file:///a.rsc",
    );
    let d = diags
        .iter()
        .find(|d| d.code.as_deref() == Some("unknown-property"))
        .expect("unknown-property");
    assert_eq!(d.severity, Some(severity::WARNING));
    assert!(d.message.contains("bogus"));
    assert!(d.message.contains("/ip/address"));
}
