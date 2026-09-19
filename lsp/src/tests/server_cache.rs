// Completion cache and parse cache lifecycle.
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
// ── Parse cache lifecycle ────────────────────────────────────────────────

#[test]
fn test_server_completion_identical_cold_and_warm() {
    // No behavior change from caching: the first request (cold cache,
    // parses) and the second (warm cache, reuses) return byte-identical
    // responses.
    let mut server = Server::new(synthetic_data());
    let text = "/ip/address add \\\naddress=1.1.1.1 interface=ether1\n";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///cached.rsc", "text": text}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///cached.rsc"},
            "position": {"line": 1, "character": 5}
        }
    });
    let cold = server
        .handle_message("textDocument/completion", &req)
        .expect("completion must be answered");
    assert!(
        server
            .parse_cache
            .lookup("file:///cached.rsc", text)
            .is_some(),
        "first completion must populate the parse cache"
    );
    let warm = server
        .handle_message("textDocument/completion", &req)
        .expect("completion must be answered");
    assert_eq!(cold, warm, "warm-cache completion must equal cold-cache");
}

#[test]
fn test_server_did_change_reparses_without_explicit_invalidation() {
    // Edits rely on the cache's length+hash guard, not on an explicit
    // invalidation: the stale entry must no longer be served, and the
    // didChange publish path already warms the parse for the NEW text so
    // the next request (completion, symbols, …) hits it.
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///evict.rsc", "text": ":put $a\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///evict.rsc"},
            "position": {"line": 0, "character": 0}
        }
    });
    server.handle_message("textDocument/completion", &req);
    assert!(
        server
            .parse_cache
            .lookup("file:///evict.rsc", ":put $a\n")
            .is_some()
    );
    let change = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///evict.rsc"}, "contentChanges": [{"text": ":put $b\n"}]}
    });
    server.handle_message("textDocument/didChange", &change);
    let stored = server.docs.get("file:///evict.rsc").unwrap().clone();
    assert_eq!(stored, ":put $b\n");
    assert!(
        server
            .parse_cache
            .lookup("file:///evict.rsc", ":put $a\n")
            .is_none(),
        "stale parse must never be served after an edit"
    );
    assert!(
        server
            .parse_cache
            .lookup("file:///evict.rsc", &stored)
            .is_some(),
        "the didChange publish must warm the cache for the new text"
    );
}

#[test]
fn test_cached_and_legacy_diagnostics_are_identical() {
    // The cached path (used by didOpen/didChange/pull) and the explicit-text
    // path (tests/legacy) must emit byte-identical diagnostics for a doc
    // within the byte cap.
    let mut server = Server::new(synthetic_data());
    let text = "/ip/address add \\\naddress=1.1.1.1 interface=ether1\n";
    let uri = "file:///same.rsc";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": uri, "text": text}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let cached = server.encoded_diagnostics_cached(uri);
    let legacy = server.encoded_diagnostics(text, uri);
    assert_eq!(
        serde_json::to_value(&cached).unwrap(),
        serde_json::to_value(&legacy).unwrap(),
        "cached and legacy diagnostics must agree"
    );
}

#[test]
fn test_server_did_close_drops_parse_cache_entry() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///close.rsc", "text": ":put hi\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///close.rsc"},
            "position": {"line": 0, "character": 0}
        }
    });
    server.handle_message("textDocument/completion", &req);
    assert!(
        server
            .parse_cache
            .lookup("file:///close.rsc", ":put hi\n")
            .is_some()
    );
    let close = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///close.rsc"}}
    });
    server.handle_message("textDocument/didClose", &close);
    assert!(
        server
            .parse_cache
            .lookup("file:///close.rsc", ":put hi\n")
            .is_none(),
        "cache entries die with didClose"
    );
}
