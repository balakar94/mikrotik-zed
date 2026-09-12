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
fn test_extract_id_multibyte_value_does_not_panic_and_returns_null() {
    // Reproduces the pre-fix panic: the scanner advanced one byte past the
    // lead byte of `é` and then sliced mid-character. Must return Null.
    let body = "{\"id\": é}".as_bytes();
    assert!(crate::server_proto::extract_id_for_parse_error(body).is_null());
}

#[test]
fn test_extract_id_multibyte_resumes_at_char_boundary() {
    // Multi-byte value with no separating space.
    let body = "{\"id\":é}".as_bytes();
    assert!(crate::server_proto::extract_id_for_parse_error(body).is_null());

    // A rejected multi-byte value must not abort the scan: the scanner
    // resumes after the whole code point and still recovers a later id.
    let body = "{\"id\": é, \"id\": 7}".as_bytes();
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(body),
        serde_json::json!(7)
    );

    // A later `"id"` with a multi-byte value must not panic either; the
    // first id is a valid string and wins.
    let body = "{\"id\": \"x\", \"id\": é}".as_bytes();
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(body),
        serde_json::json!("x")
    );
}

#[test]
fn test_extract_id_numeric_and_string_values() {
    // Parse the expected number from text: a `3.14` float literal would trip
    // clippy's `approx_constant` lint.
    let expected_float =
        serde_json::from_str::<serde_json::Value>("3.14").expect("3.14 is valid JSON");
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(b"{\"id\": 3.14}"),
        expected_float
    );
    assert_eq!(
        crate::server_proto::extract_id_for_parse_error(b"{\"id\": \"hello world\"}"),
        serde_json::json!("hello world")
    );
}

#[test]
fn test_extract_id_non_utf8_returns_null() {
    let body = b"{\"id\": \xff}";
    assert!(crate::server_proto::extract_id_for_parse_error(body).is_null());
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

#[test]
fn test_completion_partial_path_segment_text_edit_segment_only() {
    // `/ip/addr` must offer `address` with a textEdit replacing ONLY the
    // typed `addr` (not the `/ip/` prefix), so accepting yields `/ip/address`.
    let mut server = Server::new(synthetic_data());
    let doc = "/ip/addr";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///seg.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let comp = serde_json::json!({
        "id": 2,
        "params": {
            "textDocument": {"uri": "file:///seg.rsc"},
            "position": {"line": 0, "character": doc.len()}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .expect("completion must be answered");
    let items = resp["result"]["items"].as_array().unwrap();
    let addr = items
        .iter()
        .find(|i| i["label"] == "address")
        .expect("partial segment 'addr' must suggest 'address'");
    let edit = addr["textEdit"]
        .as_object()
        .expect("partial-segment item must carry a replacing textEdit");
    assert_eq!(edit["range"]["start"]["line"], 0);
    assert_eq!(edit["range"]["end"]["line"], 0);
    assert_eq!(edit["range"]["start"]["character"], 4); // after "/ip/"
    assert_eq!(edit["range"]["end"]["character"], doc.len() as u64);
    assert_eq!(edit["newText"], "address");
}

#[test]
fn test_completion_partial_path_segment_maps_continuation_to_physical_line() {
    // A `/`-continued logical join places the partial path token on physical
    // line 1. The server must map the segment edit there, never to line 0.
    let mut server = Server::new(synthetic_data());
    let doc = "/ip\\\n/addr";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///segcont.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let comp = serde_json::json!({
        "id": 3,
        "params": {
            "textDocument": {"uri": "file:///segcont.rsc"},
            "position": {"line": 1, "character": 5}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .expect("completion must be answered");
    let items = resp["result"]["items"].as_array().unwrap();
    let addr = items
        .iter()
        .find(|i| i["label"] == "address")
        .expect("partial segment 'addr' must suggest 'address'");
    let edit = addr["textEdit"]
        .as_object()
        .expect("partial-segment item must carry a replacing textEdit");
    assert_eq!(edit["range"]["start"]["line"], 1, "maps to physical line 1");
    assert_eq!(edit["range"]["end"]["line"], 1);
    assert_eq!(edit["range"]["start"]["character"], 1); // after the leading "/"
    assert_eq!(edit["range"]["end"]["character"], 5);
    assert_eq!(edit["newText"], "address");
}

#[test]
fn test_completion_multiline_never_emits_line_zero_shadow() {
    // The completion layer builds value/sub-menu/verb `textEdit`s with a
    // line-0 range; the server must remap them to the cursor's physical line
    // or drop them. A leaked line-0 range carries characters measured
    // against the joined LOGICAL line, so they exceed the physical line.
    let mut server = Server::new(synthetic_data());
    let doc = "/ip/firewall/filter add chain=input \\\naction=acc";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///multi_edit.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Cursor on continuation line 1 after "action=acc".
    let comp = serde_json::json!({
        "id": 4,
        "params": {
            "textDocument": {"uri": "file:///multi_edit.rsc"},
            "position": {"line": 1, "character": "action=acc".len()}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .expect("completion must be answered");
    let items = resp["result"]["items"].as_array().unwrap();
    let lines: Vec<&str> = doc.lines().collect();
    let mut edits = 0;
    for item in items {
        let Some(te) = item.get("textEdit") else {
            continue;
        };
        let sl = te["range"]["start"]["line"].as_u64().unwrap() as usize;
        let sc = te["range"]["start"]["character"].as_u64().unwrap() as usize;
        let el = te["range"]["end"]["line"].as_u64().unwrap() as usize;
        let ec = te["range"]["end"]["character"].as_u64().unwrap() as usize;
        assert!(
            sl < lines.len() && el < lines.len(),
            "range in bounds: {te}"
        );
        assert!(sc <= lines[sl].len(), "start char in line: {te}");
        assert!(ec <= lines[el].len(), "end char in line: {te}");
        assert!(
            sl != 0 || el != 0 || sc <= lines[0].len(),
            "line-0 shadow leaked to the wire: {te}"
        );
        edits += 1;
    }
    assert!(
        edits > 0,
        "fixture must exercise at least one remapped edit"
    );
}

#[test]
fn test_completion_edit_ending_at_continuation_boundary_stays_on_line() {
    // `inter` sits at the end of physical line 0, immediately before the
    // `\`. Its replacing textEdit must END on line 0: mapping the boundary
    // to line 1's start would make the edit swallow the continuation and
    // the first bytes of the next line.
    let mut server = Server::new(synthetic_data());
    let doc = "/ip/address add inter\\\nface";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///edge.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Cursor at the end of `inter`, before the backslash (line 0, char 21).
    let comp = serde_json::json!({
        "id": 5,
        "params": {
            "textDocument": {"uri": "file:///edge.rsc"},
            "position": {"line": 0, "character": 21}
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .expect("completion must be answered");
    let items = resp["result"]["items"].as_array().unwrap();
    let iface = items
        .iter()
        .find(|i| i["label"] == "interface")
        .expect("interface must be suggested for prefix inter");
    let edit = iface["textEdit"]
        .as_object()
        .expect("property item must carry a replacing textEdit");
    assert_eq!(edit["range"]["start"]["line"], 0);
    assert_eq!(edit["range"]["start"]["character"], 16);
    assert_eq!(
        edit["range"]["end"]["line"], 0,
        "edit must not cross the continuation boundary"
    );
    assert_eq!(edit["range"]["end"]["character"], 21);
    assert!(
        edit["newText"].as_str().unwrap().starts_with("interface"),
        "replacement keeps the property text: {edit:?}"
    );
}
