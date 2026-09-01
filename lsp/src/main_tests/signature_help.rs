//! `textDocument/signatureHelp`: capability advertisement, named-
//! parameter signature construction (required-first ordering, offset
//! labels), activeParameter detection (exact/prefix/ambiguous key match,
//! quoted values, continuations), and the response guarantees shared by
//! every request handler (-32602 malformed, null untracked/gated).
//!
//! All requests run against a fresh server WITHOUT `initialize`, so the
//! negotiated encoding is the spec-default UTF-16 — exactly what real
//! clients fall back to.

use super::*;
use crate::menus::MenuData;
use std::sync::Arc;

/// `/tool/fetch`-shaped fixture: two REQUIRED properties, two optional
/// ones sharing the `check-` prefix (for ambiguity coverage), and an
/// enum type whose spaces prove offsets survive multi-word types.
fn sig_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
required = true
[[menus.arguments]]
name = "check-certificate"
type = "bool"
[[menus.arguments]]
name = "check-expired"
type = "bool"
[[menus.arguments]]
name = "http-method"
type = "enum (get | post)"
required = true
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = ""
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
"#,
    ))
}

fn make_server() -> Server {
    Server::new(sig_data())
}

fn open(s: &mut Server, uri: &str, doc: &str) {
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": doc}}}),
    );
}

fn sig_request(id: i64, uri: &str, line: usize, character: usize) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "params": {
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }
    })
}

/// Expected single-line label for `/tool/fetch add …`: REQUIRED FIRST
/// (alphabetical: http-method, url), then the optionals alphabetically.
const FETCH_LABEL: &str = "/tool/fetch add http-method=enum (get | post) url=string \
                               check-certificate=bool check-expired=bool";

// ── Capability advertisement ─────────────────────────────────

#[test]
fn test_initialize_advertises_signature_help_provider_object_form() {
    let mut s = make_server();
    let msg = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    });
    let resp = s.handle_message("initialize", &msg).unwrap();
    let provider = &resp["result"]["capabilities"]["signatureHelpProvider"];
    assert!(
        provider.is_object(),
        "object form (like completionProvider), got {provider}"
    );
    assert_eq!(provider["triggerCharacters"], serde_json::json!([" ", "="]));
}

// ── Signature construction ───────────────────────────────────

#[test]
fn test_signature_after_verb_lists_required_first_with_offset_labels() {
    let mut s = make_server();
    let doc = "/tool/fetch add ";
    open(&mut s, "file:///sig.rsc", doc);
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(60, "file:///sig.rsc", 0, doc.len()),
        )
        .unwrap();
    let result = &resp["result"];
    assert!(!result.is_null(), "menu+verb resolved ⇒ popup");
    assert_eq!(result["activeSignature"], 0);
    let sigs = result["signatures"].as_array().unwrap();
    assert_eq!(sigs.len(), 1, "exactly one signature");
    let label = sigs[0]["label"].as_str().unwrap();
    assert_eq!(label, FETCH_LABEL);

    let params = sigs[0]["parameters"].as_array().unwrap();
    assert_eq!(params.len(), 4);
    // Each ParameterInformation label is [start, end] INTO the label
    // string; slicing must reproduce the intended `name=type` segment.
    let segments: Vec<&str> = params
        .iter()
        .map(|p| {
            let start = p["label"][0].as_u64().unwrap() as usize;
            let end = p["label"][1].as_u64().unwrap() as usize;
            &label[start..end]
        })
        .collect();
    assert_eq!(
        segments,
        [
            "http-method=enum (get | post)",
            "url=string",
            "check-certificate=bool",
            "check-expired=bool"
        ],
        "required properties lead, then alphabetical"
    );
    // "(required)" lives inside the parameter documentation only.
    assert!(
        params[0]["documentation"]
            .as_str()
            .unwrap()
            .starts_with("(required) ")
    );
    assert!(
        params[1]["documentation"]
            .as_str()
            .unwrap()
            .starts_with("(required) ")
    );
    assert!(
        !params[2]["documentation"]
            .as_str()
            .unwrap()
            .starts_with("(required) ")
    );
    // Signature documentation: menu identity + the ordering note.
    let sig_doc = sigs[0]["documentation"].as_str().unwrap();
    assert!(sig_doc.contains("`/tool/fetch`"));
    assert!(sig_doc.contains("Required properties listed first."));
    // Cursor sits after the verb with no property started ⇒ nothing
    // highlighted yet.
    assert!(result.get("activeParameter").is_none());
}

// ── activeParameter detection ────────────────────────────────

#[test]
fn test_signature_prefix_match_highlights_right_param() {
    let mut s = make_server();
    // `check-c` uniquely prefixes check-certificate (param index 2 in
    // the required-first list).
    let doc = "/tool/fetch add check-c";
    open(&mut s, "file:///prefix.rsc", doc);
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(61, "file:///prefix.rsc", 0, doc.len()),
        )
        .unwrap();
    assert_eq!(
        resp["result"]["activeParameter"], 2,
        "unique prefix resolves to check-certificate"
    );
}

#[test]
fn test_signature_ambiguous_prefix_omits_active_parameter() {
    let mut s = make_server();
    // `check-` matches check-certificate AND check-expired ⇒ omit rather
    // than guess; the popup itself must still render.
    let doc = "/tool/fetch add check-";
    open(&mut s, "file:///ambig.rsc", doc);
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(62, "file:///ambig.rsc", 0, doc.len()),
        )
        .unwrap();
    assert!(
        !resp["result"]["signatures"].as_array().unwrap().is_empty(),
        "popup still shows"
    );
    assert!(
        resp["result"].get("activeParameter").is_none(),
        "ambiguous prefix ⇒ no activeParameter field at all"
    );
}

#[test]
fn test_signature_quoted_value_keeps_key_active() {
    let mut s = make_server();
    let doc = "/tool/fetch add url=\"http://x y\" check-certificate=";
    open(&mut s, "file:///quote.rsc", doc);

    // Inside the quoted VALUE: quote-aware tokens keep the whole
    // `url="http://x y"` as ONE token, so its key stays active (url is
    // param index 1, required-first).
    let inside_quote = doc.find("//").unwrap() + 1;
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(63, "file:///quote.rsc", 0, inside_quote),
        )
        .unwrap();
    assert_eq!(resp["result"]["activeParameter"], 1);

    // Right after the second `=`: that key becomes active instead.
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(64, "file:///quote.rsc", 0, doc.len()),
        )
        .unwrap();
    assert_eq!(resp["result"]["activeParameter"], 2);
}

// ── Gating: anti-noise contract ──────────────────────────────

#[test]
fn test_signature_no_verb_returns_null() {
    let mut s = make_server();
    open(&mut s, "file:///noverb.rsc", "/tool/fetch ");
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(65, "file:///noverb.rsc", 0, 12),
        )
        .unwrap();
    assert!(resp["result"].is_null(), "no verb ⇒ no popup");
}

#[test]
fn test_signature_unknown_menu_returns_null() {
    let mut s = make_server();
    open(&mut s, "file:///unknown.rsc", "/foo/bar add url=x");
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(66, "file:///unknown.rsc", 0, 18),
        )
        .unwrap();
    assert!(resp["result"].is_null(), "unresolvable menu ⇒ no popup");
}

#[test]
fn test_signature_untracked_uri_returns_null_result() {
    let mut s = make_server();
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(67, "file:///never-opened.rsc", 0, 0),
        )
        .unwrap();
    assert_eq!(resp["id"], 67, "id must be echoed");
    assert!(resp["result"].is_null());
}

#[test]
fn test_signature_malformed_params_return_32602() {
    let mut s = make_server();
    // Variant A: position missing entirely.
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &serde_json::json!({
                "id": 68,
                "params": {"textDocument": {"uri": "file:///a.rsc"}}
            }),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 68, "id echoed on error responses");

    // Variant B: uri missing entirely.
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &serde_json::json!({
                "id": 69,
                "params": {"position": {"line": 0, "character": 0}}
            }),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 69);
}

// ── Encoding & continuation integration ──────────────────────

#[test]
fn test_signature_utf16_multibyte_before_cursor() {
    let mut s = make_server();
    // Two 'ç' sit BEFORE the target position inside url's quoted value:
    // each costs 1 UTF-16 unit but 2 bytes. Byte layout:
    //   `/tool/fetch add url="https://` = 29 bytes/units,
    //   `çç` = +4 bytes/+2 units, `"` closes at byte 34 / unit 32.
    // Requesting unit 32 must resolve to BYTE 34 (the closing quote),
    // i.e. inside url's token — a bytes-as-units mix-up would land two
    // bytes later and wrongly highlight check-certificate.
    let doc = "/tool/fetch add url=\"https://çç\" check-certificate=";
    open(&mut s, "file:///utf16.rsc", doc);
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(70, "file:///utf16.rsc", 0, 32),
        )
        .unwrap();
    assert_eq!(
        resp["result"]["activeParameter"], 1,
        "unit→byte conversion must keep url active despite multibyte prefix"
    );
}

#[test]
fn test_signature_continuation_joined_line_resolves_context_and_offsets() {
    let mut s = make_server();
    // RouterOS `\` continuation: menu path lives on PHYSICAL line 0, the
    // property being typed on PHYSICAL line 1. The joined logical text is
    // "/tool/fetch add check-certificate=".
    let doc = "/tool/fetch add \\\ncheck-certificate=";
    open(&mut s, "file:///cont.rsc", doc);
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(71, "file:///cont.rsc", 1, 18),
        )
        .unwrap();
    let result = &resp["result"];
    assert!(
        !result.is_null(),
        "menu must resolve across the continuation"
    );
    let label = result["signatures"][0]["label"].as_str().unwrap();
    assert_eq!(label, FETCH_LABEL, "label built from the JOINED line");
    // Offsets still slice the label exactly (context correctness).
    let p0 = &result["signatures"][0]["parameters"][0];
    let seg = &label
        [p0["label"][0].as_u64().unwrap() as usize..p0["label"][1].as_u64().unwrap() as usize];
    assert_eq!(seg, "http-method=enum (get | post)");
    // Cursor maps into the joined text right after the continued key.
    assert_eq!(result["activeParameter"], 2);
}
