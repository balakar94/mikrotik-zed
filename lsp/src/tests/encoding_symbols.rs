// Encoding — symbols.
use crate::menus::MenuData;
use crate::server::Server;
use std::sync::Arc;
fn synth_min() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
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
"#,
    ))
}

/// Run `initialize` with an optional `general.positionEncodings` array
/// (`None` = capability absent) and return the server plus the response.
fn initialize(encodings: Option<serde_json::Value>) -> (Server, serde_json::Value) {
    let mut server = Server::new(synth_min());
    let params = match encodings {
        None => serde_json::json!({"capabilities": {}}),
        Some(e) => {
            serde_json::json!({"capabilities": {"general": {"positionEncodings": e}}})
        }
    };
    let msg = serde_json::json!({"id": 1, "method": "initialize", "params": params});
    let resp = server.handle_message("initialize", &msg).unwrap();
    (server, resp)
}

// ── documentSymbol / foldingRange (Stage B) ──────────────────────────────

/// Open `doc` in a fresh utf-16-negotiated server and return the raw
/// response for `method` (documentSymbol / foldingRange).
fn stage_b_request(method: &str, doc: &str, id: i64) -> serde_json::Value {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///b.rsc", "text": doc}}}),
    );
    s.handle_message(
        method,
        &serde_json::json!({"id": id, "params": {"textDocument": {"uri": "file:///b.rsc"}}}),
    )
    .expect("requests must be answered")
}

#[test]
fn test_document_symbols_menu_global_local_mix() {
    let doc = concat!(
        "/ip/address add address=1.2.3.4\n",
        ":global gw1 1.1.1.1\n",
        ":local i 0\n",
        ":put done\n",
        "print\n", // bare fragment — skipped
    );
    let resp = stage_b_request("textDocument/documentSymbol", doc, 21);
    assert_eq!(resp["id"], 21);
    let syms = resp["result"].as_array().expect("flat symbol array");
    let names: Vec<&str> = syms.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["/ip/address add", "gw1", "i", ":put"]);
    let kinds: Vec<i64> = syms.iter().map(|s| s["kind"].as_i64().unwrap()).collect();
    assert_eq!(kinds, vec![19, 13, 13, 12]);
    // First symbol: range covers the whole line; selection the path token.
    assert_eq!(syms[0]["range"]["start"]["line"], 0);
    assert_eq!(syms[0]["range"]["end"]["character"], 31);
    assert_eq!(syms[0]["selectionRange"]["start"]["character"], 0);
    assert_eq!(syms[0]["selectionRange"]["end"]["character"], 11);
}

#[test]
fn test_document_symbol_continuation_spans_physical_lines() {
    let doc = "/ip/address add \\\naddress=1.2.3.4\n";
    let resp = stage_b_request("textDocument/documentSymbol", doc, 22);
    let syms = resp["result"].as_array().unwrap();
    assert_eq!(syms.len(), 1, "continuation joins into one logical command");
    assert_eq!(syms[0]["range"]["start"]["line"], 0);
    assert_eq!(syms[0]["range"]["end"]["line"], 1);
    assert_eq!(syms[0]["range"]["end"]["character"], 15);
}

#[test]
fn test_document_symbols_empty_doc_is_empty_array() {
    let resp = stage_b_request("textDocument/documentSymbol", "", 23);
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_document_symbols_untracked_uri_returns_null_result() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    let resp = s
            .handle_message(
                "textDocument/documentSymbol",
                &serde_json::json!({"id": 24, "params": {"textDocument": {"uri": "file:///never.rsc"}}}),
            )
            .unwrap();
    assert_eq!(resp["id"], 24, "id must be echoed");
    assert!(resp["result"].is_null(), "untracked URI → null result");
}

#[test]
fn test_document_symbols_malformed_params_return_32602() {
    let mut s = Server::new(synth_min());
    // Missing textDocument object entirely.
    let resp = s
        .handle_message(
            "textDocument/documentSymbol",
            &serde_json::json!({"id": 25}),
        )
        .unwrap();
    assert_eq!(resp["id"], 25);
    assert_eq!(resp["error"]["code"], -32602);
    // Missing uri inside textDocument.
    let resp = s
        .handle_message(
            "textDocument/documentSymbol",
            &serde_json::json!({"id": 26, "params": {"textDocument": {}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_folding_ranges_block_and_continuation_sorted() {
    let doc = concat!(
        ":do {\n",              // 0 opens region
        "\t:put x\n",           // 1
        "}\n",                  // 2 closes region → (0,2,"region")
        "/ip/address add \\\n", // 3 continues
        "address=1.2.3.4\n",    // 4 → continuation fold (3,4)
    );
    let resp = stage_b_request("textDocument/foldingRange", doc, 27);
    let ranges = resp["result"].as_array().unwrap();
    let rows: Vec<(i64, i64, Option<&str>)> = ranges
        .iter()
        .map(|r| {
            (
                r["startLine"].as_i64().unwrap(),
                r["endLine"].as_i64().unwrap(),
                r["kind"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![(0, 2, Some("region")), (3, 4, None)],
        "sorted by startLine; region carries kind, continuations do not"
    );
}

#[test]
fn test_folding_ranges_single_line_braces_not_emitted() {
    let doc = ":if (a) do={ :put x } else={ :put y }\n";
    let resp = stage_b_request("textDocument/foldingRange", doc, 28);
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_folding_ranges_unterminated_brace_safe_and_null_untracked() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // Unterminated brace: answered with an empty list, never a hang.
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u.rsc", "text": ":do {\n:put x\n"}}}),
        );
    let resp = s
        .handle_message(
            "textDocument/foldingRange",
            &serde_json::json!({"id": 29, "params": {"textDocument": {"uri": "file:///u.rsc"}}}),
        )
        .unwrap();
    assert!(resp["result"].as_array().unwrap().is_empty());

    // Untracked URI → null result with echoed id.
    let resp = s
        .handle_message(
            "textDocument/foldingRange",
            &serde_json::json!({"id": 30, "params": {"textDocument": {"uri": "file:///nope.rsc"}}}),
        )
        .unwrap();
    assert_eq!(resp["id"], 30);
    assert!(resp["result"].is_null());
}

#[test]
fn test_folding_range_malformed_params_return_32602() {
    let mut s = Server::new(synth_min());
    let resp = s
        .handle_message(
            "textDocument/foldingRange",
            &serde_json::json!({"id": 31, "params": {"textDocument": {"nope": true}}}),
        )
        .unwrap();
    assert_eq!(resp["id"], 31);
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_document_symbol_characters_honor_utf16_negotiation() {
    // Default negotiation is UTF-16. The logical command spans two
    // physical lines; its end lands on line 1 whose content holds a
    // multibyte char BEFORE the end position:
    //   comment="ç"  → 11 UTF-16 units but 12 bytes.
    let doc = "/ip/address add \\\ncomment=\"ç\"\n";
    let resp = stage_b_request("textDocument/documentSymbol", doc, 32);
    let sym = &resp["result"].as_array().unwrap()[0];
    assert_eq!(sym["range"]["end"]["line"], 1);
    assert_eq!(
        sym["range"]["end"]["character"], 11,
        "utf-16 units, not bytes (raw byte offset would be 12)"
    );
    // Selection sits on the ASCII first line — identical either way.
    assert_eq!(sym["selectionRange"]["end"]["character"], 11);
}
