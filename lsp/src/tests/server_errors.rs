// Parse error shape and property text edits.
// Copied (not moved) from `lsp/src/server.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::Server;` for the new location.
use crate::Server;
use crate::caps::{MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS};
use crate::diagnostics;
use crate::menus::MenuData;
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
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
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
"#,
    ))
}
// ── Parse error + property textEdit ──────────────────────────────────────

#[test]
fn test_parse_error_response_shape() {
    use crate::server::{extract_id_for_parse_error, parse_error_response};
    let id = serde_json::json!(7);
    let resp = parse_error_response(&id);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["error"]["code"], -32700);
    assert_eq!(resp["error"]["message"], "Parse error");
    // Null id fallback.
    let null_resp = parse_error_response(&serde_json::Value::Null);
    assert!(null_resp["id"].is_null());
    assert_eq!(null_resp["error"]["code"], -32700);
    // Best-effort id recovery from a malformed body.
    let with_id = extract_id_for_parse_error(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":");
    assert_eq!(with_id, serde_json::json!(42));
    let with_str_id =
        extract_id_for_parse_error(b"{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":");
    assert_eq!(with_str_id, serde_json::json!("abc"));
    let missing = extract_id_for_parse_error(b"{not json at all");
    assert!(missing.is_null());
}

#[test]
fn test_completion_property_text_edit_replaces_partial_name() {
    // `/ip/address add inter` with the cursor after `inter` must offer
    // `interface` (kind 5) with a textEdit covering exactly `inter` on
    // the cursor line — never a line-0 guess on another line.
    let mut server = Server::new(synthetic_data());
    let doc = "/ip/address add inter";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///prop.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let comp = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///prop.rsc"},
            "position": {"line": 0, "character": doc.len()}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .expect("completion must be answered");
    let items = resp["result"]["items"].as_array().unwrap();
    let iface = items
        .iter()
        .find(|i| i["label"] == "interface")
        .expect("interface property should be suggested for prefix inter");
    assert_eq!(iface["kind"], 5);
    let edit = iface["textEdit"]
        .as_object()
        .expect("property item must carry a replacing textEdit");
    assert_eq!(edit["range"]["start"]["line"], 0);
    assert_eq!(edit["range"]["end"]["line"], 0);
    let expected_start = doc.rfind("inter").unwrap() as u64;
    assert_eq!(edit["range"]["start"]["character"], expected_start);
    assert_eq!(edit["range"]["end"]["character"], doc.len() as u64);
}
