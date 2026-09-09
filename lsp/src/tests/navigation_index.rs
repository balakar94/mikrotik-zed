// Variable navigation.
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
fn test_index_declarations_then_usages_in_document_order() {
    let doc = ":local wan \"e\"\n:put $wan\n/ip/address add interface=$wan\n";
    let hits = index_of(doc);
    assert_eq!(
        summary(&hits),
        vec![
            "wan::local@0".to_string(),
            "wan:$@1".to_string(),
            "wan:$@2".to_string(),
        ],
        "declaration first, then usages, all in doc order"
    );
    // Usage spans exclude the `$` sigil.
    assert_eq!(hits[1].start, 6);
    assert_eq!(hits[1].end, 9);
}

#[test]
fn test_continuation_split_declaration_is_indexed_once() {
    // `:local counter \` joined with `=1` is ONE logical command; the
    // declaration must be found despite the physical split.
    let doc = ":local counter \\\n=1\n:put $counter\n";
    let hits = index_of(doc);
    assert_eq!(
        summary(&hits),
        vec!["counter::local@0".to_string(), "counter:$@1".to_string()]
    );
}

#[test]
fn test_empty_document_yields_empty_index() {
    assert!(index_of("").is_empty());
    assert!(index_of("# only a comment\n").is_empty());
}

// ── Usage scanning: quotes, $$, comments ─────────────────────────────────

#[test]
fn test_usage_scan_double_quoted_interpolates_single_quoted_literal() {
    // RouterOS interpolates `"…"` but not `'…'`: usages inside double
    // quotes surface, single-quoted dollars stay literal.
    let doc = concat!(
        ":put \"see $live now\"\n", // double-quoted: INDEXED
        ":put 'literal $hidden'\n", // single-quoted: inert
        ":put $plain\n",            // control: still counted
    );
    let hits = index_of(doc);
    assert_eq!(
        summary(&hits),
        vec!["live:$@0".to_string(), "plain:$@2".to_string()],
        "double-quoted $ interpolates, single-quoted stays literal, got {hits:?}"
    );
}

#[test]
fn test_usage_scan_doubled_dollar_literal_even_in_double_quotes() {
    let hits = index_of(":put \"cost $$live\"\n:put $ok\n");
    assert_eq!(summary(&hits), vec!["ok:$@1".to_string()]);
}

#[test]
fn test_usage_scan_ignores_doubled_dollar() {
    let hits = index_of("$$x\n$$$y\n:put $ok\n");
    assert_eq!(summary(&hits), vec!["ok:$@2".to_string()]);
}

#[test]
fn test_usage_scan_stops_at_unquoted_comment() {
    let doc = ":put $before # trailing $hidden note\n";
    let hits = index_of(doc);
    assert_eq!(summary(&hits), vec!["before:$@0".to_string()]);
}
