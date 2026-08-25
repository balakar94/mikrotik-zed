//! Position-encoding negotiation, boundary conversions, and regression
//! coverage for UTF-16 positions against non-ASCII documents.

use super::*;
use crate::menus::MenuData;

fn synth_min() -> MenuData {
    MenuData::from_toml_str(
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
    )
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

// ── Negotiation matrix ────────────────────────────────────────

#[test]
fn test_initialize_without_capability_defaults_to_utf16() {
    let (server, resp) = initialize(None);
    assert_eq!(
        resp["result"]["capabilities"]["positionEncoding"], "utf-16",
        "spec default when client sends no positionEncodings"
    );
    assert_eq!(server.position_encoding, PositionEncoding::Utf16);
}

#[test]
fn test_initialize_prefers_utf8_when_client_advertises_it() {
    let (server, resp) = initialize(Some(serde_json::json!(["utf-16", "utf-8"])));
    assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(server.position_encoding, PositionEncoding::Utf8);
}

#[test]
fn test_initialize_falls_back_to_utf16_when_utf8_absent() {
    let (server, resp) = initialize(Some(serde_json::json!(["utf-32"])));
    assert_eq!(resp["result"]["capabilities"]["positionEncoding"], "utf-16");
    assert_eq!(server.position_encoding, PositionEncoding::Utf16);
}

#[test]
fn test_initialize_keeps_existing_capabilities_intact() {
    let (_, resp) = initialize(Some(serde_json::json!(["utf-8"])));
    let caps = &resp["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(caps["textDocumentSync"]["change"], 2);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        serde_json::json!(["/", " ", "=", ":"])
    );
    assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
}

#[test]
fn test_initialize_advertises_incremental_sync() {
    // textDocumentSync must be the object form (openClose + change = 2),
    // not the legacy scalar Full-sync kind. Incremental patching is
    // implemented and tested (apply_incremental_edit); full-text
    // replacements remain handled as a fallback.
    let (_, resp) = initialize(None);
    let sync = &resp["result"]["capabilities"]["textDocumentSync"];
    assert!(sync.is_object(), "sync capability must be the object form");
    assert_eq!(sync["change"], 2);
    assert_eq!(sync["openClose"], true);
}

#[test]
fn test_initialize_advertises_all_providers() {
    // Stage B: every supported provider must be advertised together.
    let (_, resp) = initialize(None);
    let caps = &resp["result"]["capabilities"];
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        serde_json::json!(["/", " ", "=", ":"])
    );
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(
        caps["documentSymbolProvider"], true,
        "documentSymbol capability must be advertised"
    );
    assert_eq!(
        caps["foldingRangeProvider"], true,
        "foldingRange capability must be advertised"
    );
    assert_eq!(caps["diagnosticProvider"]["interFileDependencies"], false);
}

// ── Regression: incremental edits must not corrupt documents ──

#[test]
fn test_did_change_incremental_utf16_no_corruption_on_non_ascii_line() {
    let mut s = Server::new(synth_min());
    // Client does not advertise utf-8 → positions are UTF-16 code units.
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    let doc = "# comentário ✔\n/ip/address add address=1.1.1.1\n";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///reg.rsc", "text": doc}}}),
    );

    // Delete the trailing '✔' on line 0 expressed in UTF-16 units:
    // "# comentário " is 13 units, '✔' spans units 13..14 (bytes 14..17).
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 13}, "end": {"line": 0, "character": 14}},
                    "text": ""
                }]
            }}),
        );
    // Byte-level treatment would instead delete the SPACE before '✔'
    // (bytes 13..14), leaving the emoji behind — exact equality guards it.
    assert_eq!(
        s.docs.get("file:///reg.rsc").unwrap(),
        "# comentário \n/ip/address add address=1.1.1.1\n"
    );

    // Follow-up ranged edit targeting LINE 1 with non-ASCII above: the
    // line-start scan must stay byte-exact while characters stay UTF-16
    // (replaces exactly "/ip/address", 11 units).
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///reg.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 11}},
                    "text": "/ipv6/address"
                }]
            }}),
        );
    assert_eq!(
        s.docs.get("file:///reg.rsc").unwrap(),
        "# comentário \n/ipv6/address add address=1.1.1.1\n"
    );
}

#[test]
fn test_did_change_incremental_utf8_positions_unchanged_for_ascii() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
    );
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///u8.rsc", "text": "hello world"}}}),
        );
    s.handle_message(
            "textDocument/didChange",
            &serde_json::json!({"params": {
                "textDocument": {"uri": "file:///u8.rsc"},
                "contentChanges": [{
                    "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}},
                    "text": "Rust"
                }]
            }}),
        );
    assert_eq!(s.docs.get("file:///u8.rsc").unwrap(), "hello Rust");
}

#[test]
fn test_did_change_incremental_utf16_crlf_insert_before_cr() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///crlf.rsc", "text": "héllo\r\nworld"}}}),
        );
    // Insert at the EOL position (unit 6 == end of "héllo"): must land
    // BEFORE the '\r', never inside or after the CRLF pair.
    s.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": "file:///crlf.rsc"},
            "contentChanges": [{
                "range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 6}},
                "text": "X"
            }]
        }}),
    );
    assert_eq!(s.docs.get("file:///crlf.rsc").unwrap(), "hélloX\r\nworld");
}

// ── Hover / completion context under Utf16 ────────────────────

#[test]
fn test_hover_utf16_with_multibyte_prefix_on_same_line() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // Non-ASCII comment ABOVE and multibyte prefix BEFORE the target
    // token on the same line: 'ççççç' adds 5 extra bytes over units.
    let doc = concat!(
        "# comentário ✔\n",
        "/ip/address add comment=\"ççççç\" address=1.1.1.1",
    );
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///hv.rsc", "text": doc}}}),
    );
    // "address" starts at unit 32 (byte 37); unit 35 is mid-word.
    let hover = serde_json::json!({
        "id": 9,
        "params": {
            "textDocument": {"uri": "file:///hv.rsc"},
            "position": {"line": 1, "character": 35}
        }
    });
    let resp = s.handle_message("textDocument/hover", &hover).unwrap();
    assert!(
        resp["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("**address**"),
        "word extraction must land on 'address', got {}",
        resp["result"]
    );
}

#[test]
fn test_completion_utf16_value_completions_after_multibyte_prefix() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // Trailing target token sits AFTER multibyte content on the line:
    // 'chain=' ends at unit 42 / byte 43 ('ç' costs one extra byte).
    let doc = "/ip/firewall/filter add comment=\"ç\" chain=";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///cp.rsc", "text": doc}}}),
    );
    let comp = serde_json::json!({
        "id": 10,
        "params": {
            "textDocument": {"uri": "file:///cp.rsc"},
            "position": {"line": 0, "character": 42}
        }
    });
    let resp = s.handle_message("textDocument/completion", &comp).unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    let labels: Vec<&str> = items.iter().map(|i| i["label"].as_str().unwrap()).collect();
    assert!(
        labels.contains(&"input"),
        "value completions for 'chain=' expected, got {labels:?}"
    );
}

// ── documentSymbol / foldingRange (Stage B) ───────────────────

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

// ── Diagnostics ranges honor the negotiated encoding ──────────

#[test]
fn test_pull_diagnostics_utf16_character_units_with_emoji_prefix() {
    let mut s = Server::new(synth_min());
    // Default negotiation (no capability) → UTF-16 emission.
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize", "params": {}}),
    );
    // "bogusprop" starts at byte 25 ("…add " = 16 bytes + two 🚨 = 8)
    // but at unit 21 (each 🚨 counts 2 units).
    let doc = "/ip/address add 🚨🚨 bogusprop=1";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///dg.rsc", "text": doc}}}),
    );
    let pull = s
        .handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 11, "params": {"textDocument": {"uri": "file:///dg.rsc"}}}),
        )
        .unwrap();
    let items = pull["result"]["items"].as_array().unwrap();
    let up = items
        .iter()
        .find(|d| d["code"] == "unknown-property")
        .expect("unknown-property diagnostic expected");
    assert_eq!(up["range"]["start"]["character"], 21);
    assert_eq!(up["range"]["end"]["character"], 30);
}

#[test]
fn test_pull_diagnostics_utf8_character_equals_bytes() {
    let mut s = Server::new(synth_min());
    s.handle_message(
        "initialize",
        &serde_json::json!({"id": 0, "method": "initialize",
                "params": {"capabilities": {"general": {"positionEncodings": ["utf-8"]}}}}),
    );
    let doc = "/ip/address add 🚨🚨 bogusprop=1";
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///d8.rsc", "text": doc}}}),
    );
    let pull = s
        .handle_message(
            "textDocument/diagnostic",
            &serde_json::json!({"id": 12, "params": {"textDocument": {"uri": "file:///d8.rsc"}}}),
        )
        .unwrap();
    let items = pull["result"]["items"].as_array().unwrap();
    let up = items
        .iter()
        .find(|d| d["code"] == "unknown-property")
        .expect("unknown-property diagnostic expected");
    // Byte semantics preserved exactly when utf-8 is negotiated.
    assert_eq!(up["range"]["start"]["character"], 25);
    assert_eq!(up["range"]["end"]["character"], 34);
}
