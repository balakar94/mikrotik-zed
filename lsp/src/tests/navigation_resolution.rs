// Variable navigation — references.
use crate::caps::MAX_REFERENCES;
use crate::menus::MenuData;
use crate::navigation::*;
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

fn index_of(doc: &str) -> VariableIndex {
    build_variable_index(&crate::diagnostics::logical_lines(doc))
}

#[test]
fn test_cursor_resolves_only_indexed_variable_spans() {
    let doc = ":local counter\n:put $counter\n/ip/address add counter=1\n";
    let index = index_of(doc);
    assert!(index.hit_at(0, 8).is_some());
    assert!(index.hit_at(1, 8).is_some());
    assert!(index.hit_at(2, 17).is_none());
}

#[test]
fn test_scope_resolution_and_references_include_writes() {
    let doc = concat!(
        ":local port \"wan\"\n",
        "{ :local port \"lan\"; :put $port }\n",
        ":set port \"up\"\n",
        ":put \"port=$port\"\n",
    );
    let index = index_of(doc);
    let outer = index.hit_at(3, 13).unwrap();
    let inner = index.hit_at(1, 28).unwrap();
    assert_eq!(index.definition(outer).unwrap().logical_line, 0);
    assert_eq!(index.definition(inner).unwrap().logical_line, 1);
    let lines: Vec<_> = index
        .references(outer, true)
        .map(|hit| hit.logical_line)
        .collect();
    assert_eq!(lines, [0, 2, 3]);
    assert_eq!(index.references(inner, false).count(), 1);
}

#[test]
fn test_definition_requires_declaration_in_scope_at_use() {
    let index = index_of(":put $x\n{ :local x; :put $x }\n:put $x\n");
    for line in [0, 2] {
        let use_ = index.hit_at(line, 6).unwrap();
        assert!(index.definition(use_).is_none());
        assert_eq!(index.references(use_, true).count(), 0);
    }
}

#[test]
fn test_collect_references_capped_at_max() {
    let mut doc = String::from(":local v\n");
    for _ in 0..=MAX_REFERENCES {
        doc.push_str(":put $v\n");
    }
    let index = index_of(&doc);
    let hit = index.hit_at(0, 7).unwrap();
    assert_eq!(index.references(hit, true).count(), MAX_REFERENCES);
    assert!(index.rename_hits(hit, None).is_none());
}
