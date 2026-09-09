//! Signature — labels and budgets.
use crate::menus::MenuData;
use crate::menus::MenuEntry;
use crate::parser::tokenize_with_spans;
use crate::server::Server;
use crate::signature::*;
use crate::suggest::MAX_SUGGEST_INPUT_BYTES;
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

    // Right after the second `=`: that key becomes active instead. The
    // completed `url=` pair is filtered out (completion exclusion), so
    // check-certificate shifts from index 2 to index 1.
    let resp = s
        .handle_message(
            "textDocument/signatureHelp",
            &sig_request(64, "file:///quote.rsc", 0, doc.len()),
        )
        .unwrap();
    assert_eq!(resp["result"]["activeParameter"], 1);
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
