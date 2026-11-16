// White-box: rename (rename).
use crate::encoding::PositionEncoding;
use crate::rename::*;

const URI: &str = "file:///rename.rsc";

fn rename(doc: &str, line: usize, character: usize, new_name: &str) -> serde_json::Value {
    rename_result(doc, PositionEncoding::Utf8, URI, line, character, new_name)
}

#[test]
fn test_rename_variable_covers_declaration_and_usages() {
    let doc = ":local wan \"e\"\n:put $wan\n/ip/address add interface=$wan\n";
    // Cursor on the declaration identifier `wan` (line 0, byte 8).
    let result = rename(doc, 0, 8, "uplink");
    let edits = result["changes"][URI].as_array().expect("edits array");
    assert_eq!(edits.len(), 3, "declaration + two usages, got {result}");
    for edit in edits {
        assert_eq!(edit["newText"], "uplink");
    }
    // Declaration edit covers exactly `wan` (bytes 7..10 of line 0),
    // so `:local ` and the value survive.
    assert_eq!(edits[0]["range"]["start"]["line"], 0);
    assert_eq!(edits[0]["range"]["start"]["character"], 7);
    assert_eq!(edits[0]["range"]["end"]["character"], 10);
    // Usage edits exclude the `$` sigil: `:put $wan` keeps its `$`.
    assert_eq!(edits[1]["range"]["start"]["line"], 1);
    assert_eq!(edits[1]["range"]["start"]["character"], 6);
    assert_eq!(edits[1]["range"]["end"]["character"], 9);
}

#[test]
fn test_rename_from_usage_renames_same_set() {
    let doc = ":local x 1\n:put $x\n";
    // Cursor on the usage (line 1, inside `x`).
    let result = rename(doc, 1, 6, "y");
    let edits = result["changes"][URI].as_array().expect("edits array");
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["range"]["start"]["line"], 0);
    assert_eq!(edits[1]["range"]["start"]["line"], 1);
}

#[test]
fn test_rename_inline_value_keeps_value_outside_edit() {
    // `:local x=1` declares only `x`; the `=1` must survive the rename.
    let doc = ":local x=1\n:put $x\n";
    let result = rename(doc, 0, 7, "count");
    let edits = result["changes"][URI].as_array().expect("edits array");
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["newText"], "count");
    assert_eq!(edits[0]["range"]["end"]["character"], 8);
}

#[test]
fn test_rename_with_no_identifier_returns_null() {
    let doc = "/ip/address add address=1.2.3.4\n";
    // Cursor on a property value: no variable occurrence there.
    assert_eq!(rename(doc, 0, 25, "other"), serde_json::Value::Null);
    // Cursor on whitespace-only text.
    assert_eq!(rename("   \n", 0, 1, "other"), serde_json::Value::Null);
    // Same-spelling property of an existing variable never resolves
    // (mirrors the navigation overlap rule).
    let doc = ":local ip 1\n/ip/address add address=1.2.3.4\n";
    assert_eq!(rename(doc, 1, 20, "other"), serde_json::Value::Null);
}

#[test]
fn test_rename_with_unusable_new_name_returns_null() {
    let doc = ":local x\n:put $x\n";
    for bad in ["", "   ", "has space", "with-dash", "semi;colon", "$"] {
        assert_eq!(
            rename(doc, 0, 7, bad),
            serde_json::Value::Null,
            "new name {bad:?} must yield null, never an edit"
        );
    }
}

#[test]
fn test_rename_tolerates_leading_sigil_in_new_name() {
    let doc = ":local x\n:put $x\n";
    let result = rename(doc, 0, 7, "$y");
    let edits = result["changes"][URI].as_array().expect("edits array");
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0]["newText"], "y");
}

#[test]
fn test_rename_result_is_single_document_changes_map() {
    let doc = ":local a\n:put $a\n";
    let result = rename(doc, 0, 7, "b");
    let changes = result["changes"].as_object().expect("changes map");
    assert_eq!(changes.len(), 1, "v1 rename stays within one document");
    assert!(changes.contains_key(URI));
    assert!(result.get("documentChanges").is_none());
}

#[test]
fn test_rename_declines_unresolved_and_global_variables() {
    assert!(rename(":put $external\n", 0, 8, "renamed").is_null());
    assert!(rename(":global x\n:put $x\n", 0, 8, "renamed").is_null());
    assert!(rename(":local café=1\n:put $café\n", 0, 8, "renamed").is_null());
}

#[test]
fn test_rename_keeps_shadowed_bindings_and_includes_bare_writes() {
    let doc = concat!(
        ":local port \"wan\"\n",
        "{ :local port \"lan\"; :put $port }\n",
        ":set port \"uplink\"\n",
        ":put \"now $port\"\n",
    );
    let result = rename(doc, 3, 12, "wan");
    let edits = result["changes"][URI].as_array().unwrap();
    let lines: Vec<_> = edits
        .iter()
        .map(|edit| edit["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, [0, 2, 3]);
    assert_eq!(edits[1]["range"]["start"]["character"], 5);
}

#[test]
fn test_rename_declines_a_collision_or_ambiguous_use() {
    let collision = ":local port\n{ :local other; :put $port }\n";
    assert!(rename(collision, 0, 8, "other").is_null());
    let filter = ":local type\n/interface print where name=$type\n";
    assert!(rename(filter, 0, 8, "kind").is_null());
    for initializer in ["$port", "[:pick $port 0 1]"] {
        let doc = format!(":local port\n{{ :local port {initializer} }}\n");
        assert!(rename(&doc, 0, 8, "iface").is_null());
    }
}

#[test]
fn test_foreach_body_uses_its_own_loop_bindings() {
    let doc = ":local v\n:foreach k,v in={1;2} do={ :put \"$k=$v\" }\n:put $v\n";
    let result = rename(doc, 0, 7, "outer");
    let edits = result["changes"][URI].as_array().unwrap();
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[1]["range"]["start"]["line"], 2);
}

#[test]
fn prepare_rename_waits_for_a_complete_document_after_partial_sync() {
    let mut server = crate::server::Server::new(std::sync::Arc::new(
        crate::menus::MenuData::from_toml_str(""),
    ));
    let position = serde_json::json!({
        "id": 1, "params": {
            "textDocument": { "uri": URI },
            "position": { "line": 0, "character": 7 },
        },
    });
    let mut change = serde_json::json!({
        "params": {
            "textDocument": { "uri": URI },
            "contentChanges": [{
                "range": {
                    "start": { "line": 2, "character": 0 },
                    "end": { "line": 2, "character": 0 },
                },
                "text": ":local x\n:put \"$x ends with $\"\n",
            }],
        },
    });
    server.handle_message("textDocument/didChange", &change);
    let prepared = server
        .handle_message("textDocument/prepareRename", &position)
        .unwrap();
    assert!(prepared["result"].is_null());

    change["params"]["contentChanges"][0]
        .as_object_mut()
        .unwrap()
        .remove("range");
    server.handle_message("textDocument/didChange", &change);
    let prepared = server
        .handle_message("textDocument/prepareRename", &position)
        .unwrap();
    assert_eq!(prepared["result"]["placeholder"], "x");
}
