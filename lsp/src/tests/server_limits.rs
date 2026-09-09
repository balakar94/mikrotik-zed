// Document size limits and BOM handling.
// Copied (not moved) from `lsp/src/server.rs` (`mod tests` L2331-2357, L2597-2783); the original block is
// left untouched. `use super::*` is adapted to `use crate::server::{Server, is_valid_file_uri};` for the new location.
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
#[test]
fn test_server_did_open_truncates_large_doc_at_5mib() {
    let mut server = Server::new(synthetic_data());
    let large_text = "a".repeat(MAX_DOC_SIZE + 1000);
    assert!(large_text.len() > MAX_DOC_SIZE);
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///large.rsc", "text": large_text}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let stored = server
        .docs
        .get("file:///large.rsc")
        .expect("should store truncated doc");
    assert_eq!(stored.len(), MAX_DOC_SIZE);
}

#[test]
fn test_server_did_open_exact_max_size_not_truncated() {
    let mut server = Server::new(synthetic_data());
    let exact = "a".repeat(5 * 1024 * 1024);
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///exact.rsc", "text": exact.clone()}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert_eq!(
        server.docs.get("file:///exact.rsc").unwrap().len(),
        exact.len()
    );
}

#[test]
fn test_server_did_change_full_sync_truncation() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///a.rsc", "text": "small"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let large = "b".repeat(5 * 1024 * 1024 + 500);
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///a.rsc"}, "contentChanges": [{"text": large}]}
    });
    server.handle_message("textDocument/didChange", &change);
    // Full sync with an oversize payload takes the early truncation
    // branch (text.len() > MAX_DOC_SIZE → truncate at a char boundary
    // and store): the stored text is capped at EXACTLY MAX_DOC_SIZE.
    // 'b' is ASCII, so the char boundary is byte-exact here.
    let stored = server.docs.get("file:///a.rsc").unwrap();
    assert_eq!(
        stored.len(),
        MAX_DOC_SIZE,
        "full sync must truncate to exactly MAX_DOC_SIZE"
    );
}

// ── MAX_DOCS enforcement ──────────────────────────────────────────

#[test]
fn test_server_max_docs_enforced_at_100() {
    let mut server = Server::new(synthetic_data());
    for i in 0..MAX_DOCS {
        let uri = format!("file:///test{i}.rsc");
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
    }
    assert_eq!(server.docs.len(), MAX_DOCS);
    // 101st should be rejected
    let open101 = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///test101.rsc", "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open101);
    assert_eq!(server.docs.len(), MAX_DOCS);
    assert!(!server.docs.contains_key("file:///test101.rsc"));
}

#[test]
fn test_server_max_docs_allows_update_existing_when_full() {
    let mut server = Server::new(synthetic_data());
    for i in 0..MAX_DOCS {
        let uri = format!("file:///test{i}.rsc");
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open);
    }
    // Update existing doc should succeed even at cap
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///test0.rsc", "text": "updated"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert_eq!(server.docs.len(), MAX_DOCS);
    assert_eq!(server.docs.get("file:///test0.rsc").unwrap(), "updated");
}

#[test]
fn test_server_did_change_max_docs_enforced() {
    let mut server = Server::new(synthetic_data());
    for i in 0..MAX_DOCS {
        let uri = format!("file:///doc{i}.rsc");
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": "hi"}}
        });
        server.handle_message("textDocument/didOpen", &open);
    }
    // didChange to a new URI should be rejected when at cap
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///new.rsc"}, "contentChanges": [{"text": "hello"}]}
    });
    server.handle_message("textDocument/didChange", &change);
    assert!(!server.docs.contains_key("file:///new.rsc"));
    assert_eq!(server.docs.len(), MAX_DOCS);
}

#[test]
fn test_server_did_open_oversized_at_cap_is_rejected_not_inserted() {
    // didOpen MAX_DOCS ordering: the oversized branch must enforce the
    // count cap exactly like the normal branch (previously it inserted
    // unconditionally, growing past MAX_DOCS).
    let mut server = Server::new(synthetic_data());
    for i in 0..MAX_DOCS {
        let uri = format!("file:///doc{i}.rsc");
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": uri, "text": "hi"}}
        });
        server.handle_message("textDocument/didOpen", &open);
    }
    assert_eq!(server.docs.len(), MAX_DOCS);
    let oversized = "a".repeat(MAX_DOC_SIZE + 1000);
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///oversized.rsc", "text": oversized}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert!(
        !server.docs.contains_key("file:///oversized.rsc"),
        "oversized doc at cap must be rejected, not truncated+inserted"
    );
    assert_eq!(server.docs.len(), MAX_DOCS);
}

// ── BOM handling ────────────────────────────────────────────────

#[test]
fn test_server_did_open_strips_leading_bom_before_store() {
    let mut server = Server::new(synthetic_data());
    let bom_text = format!("{}/ip/address add address=1.1.1.1", '\u{FEFF}');
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///bom.rsc", "text": bom_text}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert_eq!(
        server.docs.get("file:///bom.rsc").unwrap(),
        "/ip/address add address=1.1.1.1",
        "leading U+FEFF must be stripped before parse/store"
    );
}

#[test]
fn test_server_bom_doc_diagnoses_identical_to_plain_doc() {
    // Positions must be identical with and without the BOM: the stripped
    // document is what every downstream consumer sees.
    let mut server = Server::new(synthetic_data());
    let plain = "/ip/address add address=1.1.1.1 interface=ether1\n";
    let bom = format!("{}{plain}", '\u{FEFF}');
    let open_plain = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///plain.rsc", "text": plain}}
    });
    let open_bom = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///bom2.rsc", "text": bom}}
    });
    server.handle_message("textDocument/didOpen", &open_plain);
    server.handle_message("textDocument/didOpen", &open_bom);
    let diags_plain = server.encoded_diagnostics(
        server.docs.get("file:///plain.rsc").unwrap(),
        "file:///plain.rsc",
    );
    let diags_bom = server.encoded_diagnostics(
        server.docs.get("file:///bom2.rsc").unwrap(),
        "file:///bom2.rsc",
    );
    assert_eq!(
        serde_json::to_value(&diags_plain).unwrap(),
        serde_json::to_value(&diags_bom).unwrap(),
        "BOM must not shift diagnostics"
    );
}
