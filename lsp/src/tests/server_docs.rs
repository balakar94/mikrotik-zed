// Document open, change and close handling.
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
#[test]
fn test_server_did_open_valid_file_uri_stores_doc() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///test.rsc", "text": "/ip/address add address=1.1.1.1"}}
    });
    let resp = server.handle_message("textDocument/didOpen", &open);
    assert!(resp.is_none());
    assert_eq!(
        server.docs.get("file:///test.rsc").unwrap(),
        "/ip/address add address=1.1.1.1"
    );
}

#[test]
fn test_server_did_open_rejects_untitled_uri() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "untitled://test.rsc", "text": "hello"}}
    });
    let resp = server.handle_message("textDocument/didOpen", &open);
    assert!(resp.is_none());
    assert!(!server.docs.contains_key("untitled://test.rsc"));
}

#[test]
fn test_server_did_open_rejects_http_uri() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "http://example.com/test.rsc", "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert!(!server.docs.contains_key("http://example.com/test.rsc"));
}

#[test]
fn test_server_did_open_rejects_traversal_uri() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///home/../etc/passwd", "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert!(!server.docs.contains_key("file:///home/../etc/passwd"));
}

#[test]
fn test_server_did_open_rejects_null_byte_uri() {
    let mut server = Server::new(synthetic_data());
    let uri = format!("file:///test{}.rsc", '\0');
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": uri, "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Should not store doc with null byte
    assert!(server.docs.is_empty());
}

#[test]
fn test_server_did_change_rejects_invalid_uri() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///valid.rsc", "text": "old"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "untitled://valid.rsc"}, "contentChanges": [{"text": "new"}]}
    });
    server.handle_message("textDocument/didChange", &change);
    // Original doc should remain unchanged
    assert_eq!(server.docs.get("file:///valid.rsc").unwrap(), "old");
    assert!(!server.docs.contains_key("untitled://valid.rsc"));
}

#[test]
fn test_server_did_change_malformed_element_does_not_abort_batch_or_publish() {
    // Regression: a contentChanges element without a "text" field must
    // be skipped, not abort the whole didChange. The former `?` returned
    // None out of handle_message mid-batch — abandoning already-applied
    // edits and skipping the trailing diagnostics publish.
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///batch.rsc", "text": "hello world"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    server.published.clear(); // drop the didOpen publish

    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///batch.rsc"}, "contentChanges": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "text": "Rust"}
        ]}
    });
    let resp = server.handle_message("textDocument/didChange", &change);
    assert!(resp.is_none());

    // Both valid edits applied — including the one AFTER the malformed
    // element ("hello world" → "hi world" → "hi Rust").
    assert_eq!(
        server.docs.get("file:///batch.rsc").unwrap(),
        "hi Rust",
        "the batch must survive a malformed element"
    );

    // The trailing diagnostics publish still fired, exactly once.
    assert_eq!(
        server.published.len(),
        1,
        "publish must run after the batch despite the malformed element"
    );
    let (published_uri, notif) = &server.published[0];
    assert_eq!(published_uri, "file:///batch.rsc");
    assert_eq!(notif["method"], "textDocument/publishDiagnostics");
    assert_eq!(notif["params"]["uri"], "file:///batch.rsc");
}

#[test]
fn test_server_did_close_removes_doc_and_clears() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///close.rsc", "text": "hello"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    assert!(server.docs.contains_key("file:///close.rsc"));
    let close = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///close.rsc"}}
    });
    let resp = server.handle_message("textDocument/didClose", &close);
    assert!(resp.is_none());
    assert!(!server.docs.contains_key("file:///close.rsc"));
}

#[test]
fn test_server_did_close_nonexistent_is_noop() {
    let mut server = Server::new(synthetic_data());
    let close = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///notopen.rsc"}}
    });
    let resp = server.handle_message("textDocument/didClose", &close);
    assert!(resp.is_none());
}

// ── MAX_DOC_SIZE enforcement ─────────────────────────────────────────────
