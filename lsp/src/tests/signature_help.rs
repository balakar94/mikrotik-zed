//! Signature help.
use crate::menus::MenuData;
use crate::menus::MenuEntry;
use crate::parser::tokenize_with_spans;
use crate::server::Server;
use crate::signature::*;
use std::sync::Arc;
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

fn fetch_data() -> MenuData {
    MenuData::from_toml_str(
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
path = "/empty/menu"
type = "Directory"
"#,
    )
}

fn help_for(data: &MenuData, path: &str, line_text: &str, cursor: usize) -> Option<SignatureHelp> {
    let m = menu(data, path);
    let tokens = tokenize_with_spans(line_text);
    let verb_idx = resolve_verb_token(data, &tokens)?;
    compute_signature_help(m, &tokens, verb_idx, cursor)
}

fn menu<'a>(data: &'a MenuData, path: &str) -> &'a MenuEntry {
    data.menu_by_path.get(path).expect("fixture menu")
}

/// Compute with the cursor placed at byte `cursor` of `line_text`,
/// resolving the verb exactly like the handler does.
fn help_at(data: &MenuData, line_text: &str, cursor: usize) -> SignatureHelp {
    help_opt(data, line_text, cursor).expect("fixture menu has properties and a verb")
}

fn help_opt(data: &MenuData, line_text: &str, cursor: usize) -> Option<SignatureHelp> {
    let m = menu(data, "/tool/fetch");
    let tokens = tokenize_with_spans(line_text);
    let verb_idx = resolve_verb_token(data, &tokens)?;
    compute_signature_help(m, &tokens, verb_idx, cursor)
}

fn active(help: &SignatureHelp) -> Option<usize> {
    help.active_parameter.map(|v| v as usize)
}

// ── Label construction ────────────────────────────────────────

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
    // Signature documentation: menu identity + verb role + ordering note.
    let sig_doc = sigs[0]["documentation"].as_str().unwrap();
    assert!(sig_doc.contains("`/tool/fetch add`"));
    assert!(sig_doc.contains("add creates a new entry"));
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
fn test_label_required_first_alphabetical_and_offsets_slice_exactly() {
    let line = "/tool/fetch add ";
    let help = help_at(&fetch_data(), line, line.len());
    assert_eq!(help.signatures.len(), 1, "exactly one signature");
    let sig = &help.signatures[0];
    // Required first (alphabetical: http-method, url), then the rest.
    assert_eq!(
        sig.label,
        "/tool/fetch add http-method=enum (get | post) url=string check-certificate=bool check-expired=bool"
    );
    let names: Vec<&str> = sig
        .parameters
        .iter()
        .map(|p| &sig.label[p.label[0]..p.label[1]])
        .map(|seg| seg.split('=').next().unwrap())
        .collect();
    assert_eq!(
        names,
        ["http-method", "url", "check-certificate", "check-expired"]
    );
    // Every offset pair slices the label cleanly (start <= end, in bounds).
    for p in &sig.parameters {
        assert!(p.label[0] < p.label[1] && p.label[1] <= sig.label.len());
    }
}
