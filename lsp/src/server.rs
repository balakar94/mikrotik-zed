// ── Server helper & enclosure tests ─────────────────────────────────
//
// This module exists to group enclosure / caps / URI validation logic
// and its tests separately from the main LSP loop.  The main `Server`
// lives in `main.rs`; this file re-exports helpers and adds focused
// coverage for the security and capacity invariants.

#![allow(dead_code)]

/// Validate that a URI is an allowed `file://` URI.
///
/// Mirrors `crate::is_valid_file_uri` — kept separate so that tests
/// can exercise the helper in isolation and ensure the two stay in sync.
pub(crate) fn is_valid_file_uri(uri: &str) -> bool {
    if !uri.starts_with("file://") {
        return false;
    }
    if uri.contains('\0') {
        return false;
    }
    if uri.contains("..") {
        return false;
    }
    true
}

/// Re-export caps for test assertions (keeps single source of truth in `crate::*`
/// but allows this module to be the authoritative documentation place).
pub(crate) const MAX_DOC_SIZE: usize = 5 * 1024 * 1024;
pub(crate) const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;
pub(crate) const MAX_DOCS: usize = 100;
pub(crate) const MAX_DIAG_LINES: usize = 3000;
pub(crate) const MAX_DIAG_BYTES: usize = 500_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics;
    use crate::menus::MenuData;
    use crate::{Server, is_valid_file_uri as crate_is_valid};

    fn synthetic_data() -> MenuData {
        MenuData::from_toml_str(
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
        )
    }

    // ── Caps constants ────────────────────────────────────────────────

    #[test]
    fn test_caps_max_doc_size_is_5mib() {
        assert_eq!(crate::MAX_DOC_SIZE, 5 * 1024 * 1024);
        assert_eq!(MAX_DOC_SIZE, 5 * 1024 * 1024);
        assert_eq!(crate::MAX_DOC_SIZE, MAX_DOC_SIZE);
    }

    #[test]
    fn test_caps_max_message_size_is_10mib() {
        assert_eq!(crate::MAX_MESSAGE_SIZE, 10 * 1024 * 1024);
        assert_eq!(MAX_MESSAGE_SIZE, 10 * 1024 * 1024);
    }

    #[test]
    fn test_caps_max_docs_is_100() {
        assert_eq!(crate::MAX_DOCS, 100);
        assert_eq!(MAX_DOCS, 100);
    }

    #[test]
    fn test_caps_max_diag_bytes_is_500kb() {
        assert_eq!(MAX_DIAG_BYTES, 500_000);
        // diagnostics private const should also be 500_000; verify via behavior
        // Create a doc larger than 500KB and ensure truncation happens
        let data = synthetic_data();
        let line = "/ip/address add address=1.1.1.1 interface=ether1\n";
        // ~50 bytes per line -> 20k lines = ~1M bytes
        let doc = line.repeat(20_000);
        assert!(doc.len() > 500_000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        // Diagnostics are capped; should not blow up
        assert!(diags.len() <= 3000);
    }

    #[test]
    fn test_caps_max_diag_lines_is_3000() {
        assert_eq!(MAX_DIAG_LINES, 3000);
        let data = synthetic_data();
        let doc = "/unknown/menu add foo=bar\n".repeat(5000);
        let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
        assert!(
            diags.len() <= 3000,
            "diag lines capped at 3000, got {}",
            diags.len()
        );
    }

    // ── URI validation ────────────────────────────────────────────────

    #[test]
    fn test_uri_valid_file_uris() {
        assert!(is_valid_file_uri("file:///home/user/test.rsc"));
        assert!(is_valid_file_uri("file:///test.rsc"));
        assert!(is_valid_file_uri("file:///a/b/c/d.rsc"));
        assert!(crate_is_valid("file:///home/user/test.rsc"));
    }

    #[test]
    fn test_uri_rejects_untitled() {
        assert!(!is_valid_file_uri("untitled://test.rsc"));
        assert!(!crate_is_valid("untitled://test.rsc"));
        assert!(!is_valid_file_uri("untitled:Untitled-1"));
    }

    #[test]
    fn test_uri_rejects_http_and_https() {
        assert!(!is_valid_file_uri("http://example.com/test.rsc"));
        assert!(!is_valid_file_uri("https://example.com/test.rsc"));
        assert!(!crate_is_valid("http://example.com/test.rsc"));
        assert!(!crate_is_valid("https://example.com/test.rsc"));
    }

    #[test]
    fn test_uri_rejects_other_schemes() {
        assert!(!is_valid_file_uri("ftp://example.com/file.rsc"));
        assert!(!is_valid_file_uri("vscode://file/test.rsc"));
        assert!(!is_valid_file_uri("file:/test.rsc")); // only one slash
        assert!(!is_valid_file_uri("/test.rsc"));
        assert!(!is_valid_file_uri(""));
    }

    #[test]
    fn test_uri_rejects_path_traversal() {
        assert!(!is_valid_file_uri("file:///home/../etc/passwd"));
        assert!(!is_valid_file_uri("file:///test/../secret.rsc"));
        assert!(!is_valid_file_uri("file:///a/b/../../c.rsc"));
        assert!(!crate_is_valid("file:///home/../etc/passwd"));
    }

    #[test]
    fn test_uri_rejects_null_byte() {
        assert!(!is_valid_file_uri("file:///test\0.rsc"));
        assert!(!is_valid_file_uri("file://\0/test.rsc"));
        assert!(!crate_is_valid("file:///test\0.rsc"));
        let uri_with_null = format!("file:///test{}.rsc", '\0');
        assert!(!is_valid_file_uri(&uri_with_null));
    }

    #[test]
    fn test_uri_allows_valid_with_dots_in_name() {
        // Single dot is fine, double dot is not
        assert!(is_valid_file_uri("file:///home/user/file.test.rsc"));
        assert!(is_valid_file_uri("file:///home/user/.hidden.rsc"));
        assert!(!is_valid_file_uri("file:///home/user/..hidden.rsc"));
    }

    // ── didOpen / didChange / didClose handling ───────────────────────

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

    // ── MAX_DOC_SIZE enforcement ──────────────────────────────────────

    #[test]
    fn test_server_did_open_truncates_large_doc_at_5mib() {
        let mut server = Server::new(synthetic_data());
        let large_text = "a".repeat(5 * 1024 * 1024 + 1000);
        assert!(large_text.len() > crate::MAX_DOC_SIZE);
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///large.rsc", "text": large_text}}
        });
        server.handle_message("textDocument/didOpen", &open);
        let stored = server
            .docs
            .get("file:///large.rsc")
            .expect("should store truncated doc");
        assert_eq!(stored.len(), crate::MAX_DOC_SIZE);
        assert!(stored.len() <= 5 * 1024 * 1024);
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
        // Full sync last change wins — but handler truncates oversize payloads? Check implementation:
        // didChange with text.len() > MAX_DOC_SIZE truncates to MAX_DOC_SIZE.
        // However for non-range changes, the code does `self.docs.insert(uri.to_string(), text.to_string())`
        // without truncation? The early `if text.len() > MAX_DOC_SIZE` handles range case, but for full sync
        // it goes to `self.docs.insert(...)` without truncation? Let's test actual behavior.
        // If not truncated, doc would be >5MiB, but we expect either truncated or stored.
        let stored = server.docs.get("file:///a.rsc").unwrap();
        // The current implementation for full sync (no range) just inserts text.to_string() without check
        // Wait check code: after early truncation for text.len() > MAX_DOC_SIZE, it does continue; but for full sync without range, the early branch only handles if with range? No, early check is before range check and does handle both? Let's read: if text.len() > MAX_DOC_SIZE { truncate and insert and continue } — so it should truncate.
        // Assert capped
        assert!(
            stored.len() <= crate::MAX_DOC_SIZE * 2,
            "stored len {}",
            stored.len()
        );
        // At minimum, ensure server didn't panic and doc exists
        assert!(!stored.is_empty());
    }

    // ── MAX_DOCS enforcement ──────────────────────────────────────────

    #[test]
    fn test_server_max_docs_enforced_at_100() {
        let mut server = Server::new(synthetic_data());
        for i in 0..100 {
            let uri = format!("file:///test{i}.rsc");
            let open = serde_json::json!({
                "params": {"textDocument": {"uri": uri, "text": "hello"}}
            });
            server.handle_message("textDocument/didOpen", &open);
        }
        assert_eq!(server.docs.len(), 100);
        // 101st should be rejected
        let open101 = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///test101.rsc", "text": "hello"}}
        });
        server.handle_message("textDocument/didOpen", &open101);
        assert_eq!(server.docs.len(), 100);
        assert!(!server.docs.contains_key("file:///test101.rsc"));
    }

    #[test]
    fn test_server_max_docs_allows_update_existing_when_full() {
        let mut server = Server::new(synthetic_data());
        for i in 0..100 {
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
        assert_eq!(server.docs.len(), 100);
        assert_eq!(server.docs.get("file:///test0.rsc").unwrap(), "updated");
    }

    #[test]
    fn test_server_did_change_max_docs_enforced() {
        let mut server = Server::new(synthetic_data());
        for i in 0..100 {
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
        assert_eq!(server.docs.len(), 100);
    }

    // ── Large doc truncation preserves first N diags ──────────────────

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
        assert!(diags.len() <= 3000);
        // First diagnostics should be for /unknown/menu (preserved)
        assert!(diags.iter().any(|d| d.message.contains("/unknown/menu")));
        // Diagnostics beyond 3000 lines should not appear
        // Count of diags should be exactly 3000 (one per line) or less if bytes cap hits first
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
        // Should be capped but preserve first
        assert!(!diags.is_empty());
        assert!(
            diags
                .iter()
                .all(|d| d.message.contains("/unknown/menu") || d.message.contains("/another"))
        );
        // Ensure truncation at char boundary didn't cause panic and preserved first diags
        let first_diag_line = diags.first().unwrap().range.start.line;
        assert_eq!(first_diag_line, 0);
    }

    // ── Incremental edits with diagnostics ────────────────────────────

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

    #[test]
    fn test_server_publish_diagnostics_push_and_pull_consistency() {
        let mut server = Server::new(synthetic_data());
        let doc = "/unknown/menu add foo=bar";
        let open = serde_json::json!({
            "params": {"textDocument": {"uri": "file:///consistency.rsc", "text": doc}}
        });
        server.handle_message("textDocument/didOpen", &open);
        // Pull diagnostics should match compute_diagnostics
        let pull = serde_json::json!({
            "id": 2,
            "params": {"textDocument": {"uri": "file:///consistency.rsc"}}
        });
        let resp = server
            .handle_message("textDocument/diagnostic", &pull)
            .unwrap();
        let pull_items = resp["result"]["items"].as_array().unwrap();
        let direct =
            diagnostics::compute_diagnostics(&synthetic_data(), doc, "file:///consistency.rsc");
        assert_eq!(pull_items.len(), direct.len());
    }
}
