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
fn test_word_at_matches_hover_extraction_on_usage() {
    let text = ":put $counter";
    // Cursor on the `o` of counter (byte 8): `$` excluded, word is bare.
    assert_eq!(word_at(text, 8), "counter");
    // Cursor just past the identifier still extracts it backwards,
    // exactly like hover after a finished word.
    assert_eq!(word_at(text, text.len()), "counter");
    // On the space, hover's backward extraction grabs the preceding
    // word ("put") — a documented quirk of the shared helpers that
    // navigation tolerates because only an exact occurrence overlap
    // ever resolves.
    assert_eq!(word_at(text, 4), "put");
}

// ── Cursor → occurrence resolution ───────────────────────────────────────

#[test]
fn test_hit_at_cursor_resolves_usages_and_declarations_only_in_place() {
    let doc = ":local ip 1\n/ip/address add address=1.2.3.4\n:put $ip\n";
    let index = index_of(doc);
    // On the declaration identifier (byte 7 of logical line 0).
    let decl = hit_at_cursor(&index, "ip", 0, 7);
    assert!(matches!(
        decl.map(|d| d.kind),
        Some(HitKind::Declaration(_))
    ));
    // On the usage (logical line 2, byte 6).
    let usage = hit_at_cursor(&index, "ip", 2, 6);
    assert!(matches!(usage.map(|u| u.kind), Some(HitKind::Usage)));
    // A same-spelling PROPERTY elsewhere must not resolve even though
    // the name exists in the index.
    assert!(hit_at_cursor(&index, "address", 1, 17).is_none());
}

#[test]
fn test_hit_at_cursor_strips_defensive_sigil_and_rejects_empty() {
    let index = index_of(":local x\n$y\n");
    assert!(
        hit_at_cursor(&index, "$x", 0, 7).is_some(),
        "sigil tolerated"
    );
    assert!(hit_at_cursor(&index, "", 0, 0).is_none());
    assert!(hit_at_cursor(&index, "$", 0, 0).is_none());
    assert!(hit_at_cursor(&index, "zz", 0, 0).is_none());
}

// ── Definition-choice rule ───────────────────────────────────────────────

#[test]
fn test_choose_definition_prefers_closest_preceding_local() {
    let doc = ":global x\n:local x\n:put $x\n";
    let index = index_of(doc);
    let def = choose_definition(&index, "x", (2, 5)).expect("definition exists");
    assert_eq!(
        def.kind,
        HitKind::Declaration(DeclKind::Local),
        "the LAST preceding declaration (:local) wins over the earlier :global"
    );
    assert_eq!(def.logical_line, 1);
}

#[test]
fn test_choose_definition_falls_back_to_first_when_none_precedes() {
    let doc = ":put $x\n:global x\n:local x\n";
    let index = index_of(doc);
    let def = choose_definition(&index, "x", (0, 6)).expect("definition exists");
    assert_eq!(
        def.kind,
        HitKind::Declaration(DeclKind::Global),
        "nothing precedes ⇒ FIRST declaration by document position, kind irrelevant"
    );
}

#[test]
fn test_choose_definition_from_declaration_returns_itself() {
    let doc = ":local x\n:local y\n:put $x\n";
    let index = index_of(doc);
    let def = choose_definition(&index, "x", (0, 7)).expect("definition exists");
    assert_eq!(
        def.logical_line, 0,
        "requesting from a declaration returns it"
    );
}

#[test]
fn test_choose_definition_unknown_name_is_none() {
    let index = index_of(":local x\n");
    assert!(choose_definition(&index, "zzz", (0, 0)).is_none());
}

// ── References collection ────────────────────────────────────────────────

#[test]
fn test_collect_references_counts_toggled_by_include_declaration() {
    let doc = ":local n 0\n:put $n\n:set $n ($n + 1)\n";
    let index = index_of(doc);
    let decl = choose_definition(&index, "n", (2, 6));

    let with = collect_references(&index, "n", decl);
    assert_eq!(with.len(), 4, "declaration + three usages");
    assert!(
        matches!(with[0].kind, HitKind::Declaration(_)),
        "declaration comes first"
    );

    let without = collect_references(&index, "n", None);
    assert_eq!(without.len(), 3, "usages only");
    assert!(without.iter().all(|h| h.kind == HitKind::Usage));
}

#[test]
fn test_collect_references_capped_at_max() {
    // MAX_REFERENCES + 1 usages ⇒ exactly the cap survives.
    let mut doc = String::from(":local v\n");
    for _ in 0..=MAX_REFERENCES {
        doc.push_str(":put $v\n");
    }
    let index = index_of(&doc);
    assert_eq!(index.len(), MAX_REFERENCES + 2);
    let refs = collect_references(&index, "v", None);
    assert_eq!(refs.len(), MAX_REFERENCES, "flat list capped");
}

#[test]
fn test_collect_references_unknown_name_is_empty() {
    let index = index_of(":put $v\n");
    assert!(collect_references(&index, "zzz", None).is_empty());
}
