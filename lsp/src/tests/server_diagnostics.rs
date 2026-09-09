// Truncation, incremental edits and pull diagnostics.
// Copied (not moved) from `lsp/src/server.rs`; the original block is
// left untouched. `use super::*` is adapted to `use crate::server::{Server, is_valid_file_uri};`
// for the new location.
use crate::caps::{MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS};
use crate::diagnostics;
use crate::menus::MenuData;
use crate::server::{Server, is_valid_file_uri};
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
// ── Large doc truncation preserves first N diags ─────────────────────────

#[test]
fn test_large_doc_truncation_preserves_first_diags() {
    let data = synthetic_data();
    // First 10 lines are errors, next 5000 lines are also errors but beyond cap
    let mut doc = String::new();
    for _ in 0..10 {
        doc.push_str("/unknown/menu add foo=bar\n");
    }
    for _ in 0..5000 {
        doc.push_str("/another/unknown add x=1\n");
    }
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
    assert!(diags.len() <= 3001);
    // First diagnostics should be for /unknown/menu (preserved)
    assert!(diags.iter().any(|d| d.message.contains("/unknown/menu")));
    // Diagnostics beyond 3000 lines should not appear
    // Count of diags should be exactly 3000 (one per line) plus truncation hint, or less if bytes
    // cap hits first
    assert!(!diags.is_empty());
}

#[test]
fn test_large_doc_bytes_truncation_preserves_first_diags() {
    let data = synthetic_data();
    // Create a doc >500KB where first lines have errors and truncated tail is beyond bytes cap
    let error_line = "/unknown/menu add foo=bar\n"; // ~25 bytes
    // Need >500KB: 25 * 25000 = 625K
    let doc = error_line.repeat(25_000);
    assert!(doc.len() > 500_000);
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
    // Should be capped but preserve first (ignore truncation hint)
    assert!(!diags.is_empty());
    assert!(
        diags
            .iter()
            .filter(|d| d.code.as_deref() != Some("truncated"))
            .all(|d| d.message.contains("/unknown/menu") || d.message.contains("/another"))
    );
    // Ensure truncation at char boundary didn't cause panic and preserved first diags
    let first_diag_line = diags.first().unwrap().range.start.line;
    assert_eq!(first_diag_line, 0);
}

// ── Incremental edits with diagnostics ───────────────────────────────────

#[test]
fn test_incremental_edit_then_diagnostics_updated() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///inc.rsc", "text": "/ip/address add address=1.1.1.1 interface=ether1"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Valid doc should have no unknown-menu diags
    let diags_before = diagnostics::compute_diagnostics(
        &synthetic_data(),
        server.docs.get("file:///inc.rsc").unwrap(),
        "file:///inc.rsc",
    );
    assert!(
        !diags_before
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );

    // Incremental edit: change to unknown menu
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///inc.rsc"}, "contentChanges": [{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 11}},
            "text": "/unknown/menu"
        }]}
    });
    server.handle_message("textDocument/didChange", &change);
    let doc_after = server.docs.get("file:///inc.rsc").unwrap();
    assert!(doc_after.starts_with("/unknown/menu"));
    let diags_after =
        diagnostics::compute_diagnostics(&synthetic_data(), doc_after, "file:///inc.rsc");
    assert!(
        diags_after
            .iter()
            .any(|d| d.code.as_deref() == Some("unknown-menu"))
    );
}

#[test]
fn test_incremental_edit_multiple_changes() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///multi.rsc", "text": "hello world"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///multi.rsc"}, "contentChanges": [
            {"range": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 11}}, "text": "Rust"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"}
        ]}
    });
    server.handle_message("textDocument/didChange", &change);
    assert_eq!(server.docs.get("file:///multi.rsc").unwrap(), "hi Rust");
}

#[test]
fn test_diagnostic_pull_rejects_invalid_uri() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///valid.rsc", "text": "/ip/address add address=1.1.1.1"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let pull = serde_json::json!({
        "id": 1,
        "params": {"textDocument": {"uri": "untitled://valid.rsc"}}
    });
    let resp = server
        .handle_message("textDocument/diagnostic", &pull)
        .unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    assert!(
        items.is_empty(),
        "invalid URI should return empty diagnostics"
    );
}
