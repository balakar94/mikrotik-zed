//! Variable navigation.
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

// ── Server handle_message integration ─────────────────────────

fn make_server() -> Server {
    Server::new(synthetic_data())
}

// ── Variable navigation (textDocument/definition + references) ──
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

// ── Declaration extraction ────────────────────────────────────

#[test]
fn test_server_initialize_advertises_navigation_providers() {
    let mut s = make_server();
    let resp = s
        .handle_message("initialize", &serde_json::json!({"id": 1, "params": {}}))
        .unwrap();
    assert_eq!(resp["result"]["capabilities"]["definitionProvider"], true);
    assert_eq!(resp["result"]["capabilities"]["referencesProvider"], true);
}

#[test]
fn test_server_definition_untracked_uri_returns_null_result() {
    let mut s = make_server();
    let req = nav_request(61, "file:///never-opened.rsc", serde_json::json!({}));
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert_eq!(resp["id"], 61, "id must be echoed");
    assert!(resp["result"].is_null(), "untracked URI → null result");
}

#[test]
fn test_server_definition_malformed_params_return_32602() {
    let mut s = make_server();
    // Missing URI entirely…
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 62, "params": {"position": {"line": 0, "character": 0}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 62, "id must be echoed on error responses");
    // …missing position entirely…
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 63, "params": {"textDocument": {"uri": "file:///a.rsc"}}}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
    // …and a mistyped position component.
    let resp = s
        .handle_message(
            "textDocument/definition",
            &serde_json::json!({"id": 64, "params": {
                "textDocument": {"uri": "file:///a.rsc"},
                "position": {"line": 0, "character": "eight"}
            }}),
        )
        .unwrap();
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_server_definition_jumps_to_exact_declaration_span() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nav.rsc", "text": NAV_DOC}}}),
        );
    let req = nav_request(65, "file:///nav.rsc", serde_json::json!({}));
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    let loc = &resp["result"];
    assert!(loc.is_object(), "usage must resolve, got {loc}");
    assert_eq!(loc["uri"], "file:///nav.rsc");
    // Exact name-token span of `counter` in `:local counter 0` — not
    // the command token, not the initializer.
    assert_eq!(loc["range"]["start"]["line"], 0);
    assert_eq!(loc["range"]["start"]["character"], 7);
    assert_eq!(loc["range"]["end"]["line"], 0);
    assert_eq!(loc["range"]["end"]["character"], 14);

    // Same answer when invoked ON the declaration itself.
    let req = serde_json::json!({
        "id": 66,
        "params": {
            "textDocument": {"uri": "file:///nav.rsc"},
            "position": {"line": 0, "character": 8},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert_eq!(
        resp["result"]["range"]["start"]["character"], 7,
        "requesting from the declaration returns its own span"
    );
}

#[test]
fn test_server_definition_non_variable_word_returns_null() {
    let mut s = make_server();
    s.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": "file:///nv.rsc", "text": NAV_DOC}}}),
        );
    // Cursor over the property `interface` — a real word that merely
    // shares the document with variables must NOT resolve.
    let req = serde_json::json!({
        "id": 67,
        "params": {
            "textDocument": {"uri": "file:///nv.rsc"},
            "position": {"line": 2, "character": 20},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert!(resp["result"].is_null(), "property word → null, got {resp}");
    // …and so does a cursor on the `:local` keyword itself.
    let req = serde_json::json!({
        "id": 68,
        "params": {
            "textDocument": {"uri": "file:///nv.rsc"},
            "position": {"line": 0, "character": 3},
        }
    });
    let resp = s.handle_message("textDocument/definition", &req).unwrap();
    assert!(resp["result"].is_null());
}

#[test]
fn test_declared_variable_local_and_global_bare() {
    let local = tokenize_with_spans(":local counter");
    let (kind, name, s, e) = declared_variable(&local).unwrap();
    assert_eq!(kind, DeclKind::Local);
    assert_eq!(name, "counter");
    assert_eq!(&":local counter"[s..e], "counter");

    let global = tokenize_with_spans(":global g");
    let (kind, name, _, _) = declared_variable(&global).unwrap();
    assert_eq!(kind, DeclKind::Global);
    assert_eq!(name, "g");
}

#[test]
fn test_declared_variable_inline_value_stays_outside_span() {
    let tokens = tokenize_with_spans(":local x=1");
    let (_, name, s, e) = declared_variable(&tokens).unwrap();
    assert_eq!(name, "x", "`:local x=1` declares only `x`");
    assert_eq!(&":local x=1"[s..e], "x");
}

#[test]
fn test_declared_variable_requires_leading_command_and_identifier() {
    // Not a declaration opener at all…
    assert!(declared_variable(&tokenize_with_spans(":put $x")).is_none());
    // …command without identifier…
    assert!(declared_variable(&tokenize_with_spans(":global")).is_none());
    // …quoted identifier unsupported in v1…
    assert!(declared_variable(&tokenize_with_spans(r#":local "my var""#)).is_none());
    // …and a :local buried after a real command is not a declaration.
    assert!(declared_variable(&tokenize_with_spans(":put $x :local y")).is_none());
}

#[test]
fn test_declared_variable_accepts_leading_separators() {
    // Block opener before the command still declares (shared primitive
    // with documentSymbol, so the outline agrees).
    let (kind, name, s, e) = declared_variable(&tokenize_with_spans("{ :local x }")).unwrap();
    assert_eq!(kind, DeclKind::Local);
    assert_eq!(name, "x");
    assert_eq!(&"{ :local x }"[s..e], "x");

    // `;`-separated tail: a finished statement ends in `;`, so the
    // following :global declares.
    let (kind, name, _, _) = declared_variable(&tokenize_with_spans(":put 1; :global y")).unwrap();
    assert_eq!(kind, DeclKind::Global);
    assert_eq!(name, "y");
}

#[test]
fn test_declared_variable_rejects_slash_and_dotdot_prefix() {
    // `/` opens a menu path and `..` navigates to the parent menu —
    // neither is a statement separator, so neither may introduce a
    // declaration.
    assert!(
        declared_variable(&tokenize_with_spans("/ :local x")).is_none(),
        "`/` must not separate a declaration"
    );
    assert!(
        declared_variable(&tokenize_with_spans(".. :local x")).is_none(),
        "`..` must not separate a declaration"
    );
}

// ── Index building ────────────────────────────────────────────
