// Variable navigation — references.
use crate::menus::MenuData;
use crate::navigation::*;
use crate::parser::tokenize_with_spans;
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

// ── Variable navigation (textDocument/definition + references) ───────────
//
// Wire-contract coverage for the navigation handlers: -32602 /
// null / [] shapes per sibling-handler strictness, exact declaration
// ranges, includeDeclaration toggling, and UTF-16 inbound positions.
// The pure semantics behind these live in navigation.rs's own suite;
// end-to-end wire variants live in tests/e2e.rs.

/// `:local counter 0` / `:put $counter` / `/ip/address add
/// interface=$counter`. Declaration name spans bytes 7..14 of line 0;
/// usages sit at line 1 bytes 6..13 and line 2 bytes 27..34.
const NAV_DOC: &str = ":local counter 0\n:put $counter\n/ip/address add interface=$counter\n";

fn nav_request(id: i64, uri: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut params = serde_json::json!({
        "textDocument": {"uri": uri},
        "position": {"line": 1, "character": 8}, // inside `$counter`
    });
    if let (Some(dst), Some(src)) = (params.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    serde_json::json!({"id": id, "params": params})
}

fn index_of(doc: &str) -> Vec<VariableHit> {
    build_variable_index(&crate::diagnostics::logical_lines(doc))
}

fn summary(hits: &[VariableHit]) -> Vec<String> {
    hits.iter()
        .map(|h| {
            let kind = match h.kind {
                HitKind::Declaration(DeclKind::Local) => ":local",
                HitKind::Declaration(DeclKind::Global) => ":global",
                HitKind::Usage => "$",
            };
            format!("{}:{}@{}", h.name, kind, h.logical_line)
        })
        .collect()
}

// ── Declaration extraction ───────────────────────────────────────────────

#[test]
fn test_server_references_untracked_uri_returns_empty_list() {
    let mut s = make_server();
    let req = serde_json::json!({
        "id": 69,
        "params": {
            "textDocument": {"uri": "file:///never-opened.rsc"},
            "position": {"line": 0, "character": 0},
            "context": {"includeDeclaration": true},
        }
    });
    let resp = s.handle_message("textDocument/references", &req).unwrap();
    assert_eq!(resp["id"], 69, "id must be echoed");
    assert!(
        resp["result"].is_array(),
        "list endpoint answers an array even untracked"
    );
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_server_references_missing_context_returns_32602() {
    let mut s = make_server();
    // Context object absent entirely…
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 70, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 70);
    // …context present but includeDeclaration missing…
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 71, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0},
                "context": {}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    // …and includeDeclaration mistyped (LSP requires a boolean).
    let resp = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 72, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": 0},
                "context": {"includeDeclaration": "yes"}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_server_references_include_declaration_toggles_list() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///ref.rsc", "text": NAV_DOC}}}),
        );

    let with = s
        .handle_message(
            "textDocument/references",
            &nav_request(
                73,
                "file:///ref.rsc",
                serde_json::json!({"context": {"includeDeclaration": true}}),
            ),
        )
        .unwrap();
    let items = with["result"].as_array().unwrap();
    assert_eq!(items.len(), 3, "declaration + two usages");
    assert_eq!(
        items[0]["range"]["start"]["character"], 7,
        "the chosen declaration comes first, exact name span"
    );
    assert_eq!(items[0]["range"]["end"]["character"], 14);
    assert_eq!(items[1]["range"]["start"]["line"], 1);
    assert_eq!(items[2]["range"]["start"]["line"], 2);
    assert_eq!(items[2]["range"]["start"]["character"], 27);

    let without = s
        .handle_message(
            "textDocument/references",
            &nav_request(
                74,
                "file:///ref.rsc",
                serde_json::json!({"context": {"includeDeclaration": false}}),
            ),
        )
        .unwrap();
    let items = without["result"].as_array().unwrap();
    assert_eq!(items.len(), 2, "usages only");
    assert_eq!(items[0]["range"]["start"]["line"], 1);
}

#[test]
fn test_server_references_position_off_any_variable_yields_empty_list() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///off.rsc", "text": NAV_DOC}}}),
        );
    let req = serde_json::json!({
        "id": 75,
        "params": {
            "textDocument": {"uri": "file:///off.rsc"},
            "position": {"line": 0, "character": 1}, // on `:local` keyword
            "context": {"includeDeclaration": true},
        }
    });
    let resp = s.handle_message("textDocument/references", &req).unwrap();
    assert!(resp["result"].as_array().unwrap().is_empty());
}

#[test]
fn test_server_navigation_resolves_utf16_positions_after_emoji() {
    // Default negotiation is UTF-16: `:put "🌍🌍" $ok` puts the usage
    // identifier at units 13..15 but bytes 17..19 (each 🌍 costs 2
    // units / 4 bytes). The probe at unit 14 (mid-identifier) would be
    // byte 14 — the closing quote — under a byte/unit mix-up, where no
    // word can be extracted at all, so this pin is decisive.
    let doc = ":local ok\n:put \"🌍🌍\" $ok\n";
    let mut s = make_server();
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///u16.rsc", "text": doc}}}),
    );
    let pos = serde_json::json!({
        "textDocument": {"uri": "file:///u16.rsc"},
        "position": {"line": 1, "character": 14},
    });

    let def = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 76, "params": pos}),
        )
        .unwrap();
    assert_eq!(
        def["result"]["range"]["start"]["character"], 7,
        "definition resolved through utf-16 units"
    );
    assert_eq!(def["result"]["range"]["end"]["character"], 9);

    let refs = s
        .handle_message(
            "textDocument/references",
            &serde_json::json!({"id": 77, "params": {
                "textDocument": {"uri": "file:///u16.rsc"},
                "position": {"line": 1, "character": 14},
                "context": {"includeDeclaration": false}
            }}),
        )
        .unwrap();
    let items = refs["result"].as_array().unwrap();
    assert_eq!(items.len(), 1, "exactly the `$ok` usage");
    assert_eq!(items[0]["range"]["start"]["line"], 1);
    assert_eq!(items[0]["range"]["start"]["character"], 13);
}

#[test]
fn test_usage_scan_handles_parens_and_arithmetic_neighbors() {
    // `($count+1)` glues the sigil to a paren; `($count-1)` must NOT
    // swallow the dash into the name.
    let hits = index_of(":put ($count+1)\n:put ($count-1)\n");
    assert_eq!(
        summary(&hits),
        vec!["count:$@0".to_string(), "count:$@1".to_string()]
    );
    assert_eq!(hits[0].end - hits[0].start, 5);
}

#[test]
fn test_lone_dollar_is_not_a_usage() {
    let hits = index_of(":put $\n:put $ more\n");
    assert!(hits.is_empty(), "got {hits:?}");
}

// ── Word extraction consistency with hover ───────────────────────────────
