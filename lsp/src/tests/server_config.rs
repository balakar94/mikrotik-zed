// Live settings scope and rename handler.
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
// ── Live settings scope tightening ───────────────────────────────────────

#[test]
fn test_server_did_change_configuration_unscoped_host_ignored() {
    let mut server = Server::new(synthetic_data());
    let before_host = server.live_config.host.clone();
    let before_enabled = server.live_config.enabled;
    // A bare object that merely contains host-like keys carries no
    // explicit scope and must be ignored (never hijack the connection).
    let settings = serde_json::json!({
        "params": {"settings": {"host": "10.9.9.9", "MIKROTIK_PASS": "s3cret"}}
    });
    server.handle_message("workspace/didChangeConfiguration", &settings);
    assert_eq!(
        server.live_config.host, before_host,
        "unscoped host key must not change the effective host"
    );
    assert_eq!(
        server.live_config.enabled, before_enabled,
        "env opt-in (enabled) is never settings-overridable"
    );
}

#[test]
fn test_server_did_change_configuration_scoped_host_applies() {
    // F2: scoped host overlay requires RSC_LS_ALLOW_SETTINGS_TRANSPORT=1.
    crate::live::with_settings_transport_env(true, || {
        let mut server = Server::new(synthetic_data());
        let before_enabled = server.live_config.enabled;
        let settings = serde_json::json!({
            "params": {"settings": {"rsc": {"live": {"host": "10.9.9.9"}}}}
        });
        server.handle_message("workspace/didChangeConfiguration", &settings);
        assert_eq!(server.live_config.host, "10.9.9.9");
        assert_eq!(server.live_config.hosts, vec!["10.9.9.9".to_string()]);
        assert_eq!(
            server.live_config.enabled, before_enabled,
            "env opt-in (enabled) is never settings-overridable"
        );
        // The `mikrotik` scope applies the same way.
        let settings = serde_json::json!({
            "params": {"settings": {"mikrotik": {"host": "10.9.9.10"}}}
        });
        server.handle_message("workspace/didChangeConfiguration", &settings);
        assert_eq!(server.live_config.host, "10.9.9.10");
    });
}

#[test]
fn test_server_did_change_configuration_scoped_host_denied_by_default() {
    // F2 default deny: without the opt-in the scoped host is ignored and
    // the cache survives.
    crate::live::with_settings_transport_env(false, || {
        let mut server = Server::new(synthetic_data());
        {
            let mut guard = server.live_cache.lock().unwrap();
            guard.insert("interfaces".to_string(), vec!["ether1".to_string()]);
        }
        let settings = serde_json::json!({
            "params": {"settings": {"rsc": {"live": {"host": "10.9.9.9"}}}}
        });
        server.handle_message("workspace/didChangeConfiguration", &settings);
        assert!(
            server.live_config.host != "10.9.9.9",
            "host overlay must be denied without RSC_LS_ALLOW_SETTINGS_TRANSPORT=1"
        );
        assert!(
            server
                .live_cache
                .lock()
                .unwrap()
                .try_get_cached("interfaces")
                .is_some(),
            "denied overlay must not clear the live cache"
        );
    });
}

// ── Rename handler ───────────────────────────────────────────────────────

#[test]
fn test_server_rename_happy_path_returns_single_document_edit() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///rename.rsc", "text": ":local wan 1\n:put $wan\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 1,
        "params": {
            "textDocument": {"uri": "file:///rename.rsc"},
            "position": {"line": 0, "character": 8},
            "newName": "uplink"
        }
    });
    let resp = server
        .handle_message("textDocument/rename", &req)
        .expect("rename request must be answered");
    assert_eq!(resp["id"], 1);
    let edits = resp["result"]["changes"]["file:///rename.rsc"]
        .as_array()
        .expect("single-document changes map");
    assert_eq!(edits.len(), 2, "declaration + usage, got {resp}");
    assert!(edits.iter().all(|e| e["newText"] == "uplink"));
}

#[test]
fn test_server_rename_untracked_uri_returns_null() {
    let mut server = Server::new(synthetic_data());
    let req = serde_json::json!({
        "id": 2,
        "params": {
            "textDocument": {"uri": "file:///never-opened.rsc"},
            "position": {"line": 0, "character": 0},
            "newName": "x"
        }
    });
    let resp = server
        .handle_message("textDocument/rename", &req)
        .expect("rename request must be answered");
    assert_eq!(resp["result"], serde_json::Value::Null);
}

#[test]
fn test_server_rename_missing_new_name_is_invalid_params() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///rename2.rsc", "text": ":local x\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 3,
        "params": {
            "textDocument": {"uri": "file:///rename2.rsc"},
            "position": {"line": 0, "character": 7}
        }
    });
    let resp = server
        .handle_message("textDocument/rename", &req)
        .expect("rename request must be answered");
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["id"], 3);
}

#[test]
fn test_server_rename_non_variable_cursor_returns_null() {
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///rename3.rsc", "text": "/ip/address add address=1.1.1.1\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);
    let req = serde_json::json!({
        "id": 4,
        "params": {
            "textDocument": {"uri": "file:///rename3.rsc"},
            "position": {"line": 0, "character": 20},
            "newName": "other"
        }
    });
    let resp = server
        .handle_message("textDocument/rename", &req)
        .expect("rename request must be answered");
    assert_eq!(resp["result"], serde_json::Value::Null);
}

#[test]
fn test_server_did_change_configuration_transport_override_applies_with_opt_in() {
    // Positive mirror of the default-deny tests: with
    // RSC_LS_ALLOW_SETTINGS_TRANSPORT=1, one allowlisted transport
    // override (force_http) applies end-to-end through
    // workspace/didChangeConfiguration, while env-only state is untouched.
    crate::live::with_settings_transport_env(true, || {
        let mut server = Server::new(synthetic_data());
        assert!(!server.live_config.force_http);
        let before_enabled = server.live_config.enabled;
        let settings = serde_json::json!({
            "params": {"settings": {"rsc": {"live": {"force_http": true}}}}
        });
        server.handle_message("workspace/didChangeConfiguration", &settings);
        assert!(
            server.live_config.force_http,
            "force_http must apply with RSC_LS_ALLOW_SETTINGS_TRANSPORT=1"
        );
        assert_eq!(
            server.live_config.enabled, before_enabled,
            "env opt-in (enabled) is never settings-overridable"
        );
    });
}
