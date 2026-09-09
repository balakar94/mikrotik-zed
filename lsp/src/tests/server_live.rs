// Publish consistency and live cache preservation.
// Copied (not moved) from `lsp/src/server.rs` (`mod tests` L2331-2357, L3177-3414); the original block is
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

#[test]
#[allow(non_snake_case)]
fn completion_textEdit_with_continuation() {
    // Logical vs physical: gateway + comment split across a RouterOS
    // `\` continuation. Cursor on continuation line 2's value suffix
    // must map back to physical line 1, not logical offset 0.
    let mut server = Server::new(synthetic_data());
    // Line 0 ends with a continuation backslash; line 1 holds the
    // dependent property. The value for `action` is on line 1.
    let doc = "/ip/firewall/filter add chain=input \\\naction=acc";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///cont.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // line 1 is "action=acc" (10 chars: "action=" 7 + "acc" 3)
    // cursor after "acc" (character 10)
    let comp = serde_json::json!({
        "id": 99,
        "params": {"textDocument": {"uri": "file:///cont.rsc"}, "position": {"line": 1, "character": 10}}
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .unwrap();
    let items = resp["result"]["items"].as_array().unwrap();
    // `action` is enum (accept|drop|reject); typed prefix "acc" should
    // filter to "accept" with a textEdit covering exactly "acc" on line 1
    let accept = items
        .iter()
        .find(|i| i["label"] == "accept")
        .expect("accept should be suggested for prefix acc");
    let edit = accept["textEdit"]
        .as_object()
        .expect("textEdit must be set via logical mapping");
    assert_eq!(
        edit["range"]["start"]["line"], 1,
        "logical start maps to physical line 1"
    );
    assert_eq!(edit["range"]["end"]["line"], 1);
    // "action=" is 7 bytes, so "acc" starts at character 7
    assert_eq!(edit["range"]["start"]["character"], 7);
    assert_eq!(edit["range"]["end"]["character"], 10);
    assert_eq!(edit["newText"], "accept");

    // Also verify single-line fallback still works: no continuation
    let mut server2 = Server::new(synthetic_data());
    let doc2 = "/ip/firewall/filter add action=acc";
    let open2 = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///single.rsc", "text": doc2}}
    });
    server2.handle_message("textDocument/didOpen", &open2);
    // line 0 "action=acc" starts at offset of chain value? Actually doc2:
    // "/ip/firewall/filter add " is 24? Let's just request after "acc"
    let line0_len = doc2.len();
    let comp2 = serde_json::json!({
        "id": 100,
        "params": {"textDocument": {"uri": "file:///single.rsc"}, "position": {"line": 0, "character": line0_len}}
    });
    let resp2 = server2
        .handle_message("textDocument/completion", &comp2)
        .unwrap();
    let items2 = resp2["result"]["items"].as_array().unwrap();
    let accept2 = items2.iter().find(|i| i["label"] == "accept").unwrap();
    let edit2 = accept2["textEdit"].as_object().unwrap();
    assert_eq!(edit2["range"]["start"]["line"], 0);
    // suffix "acc" after "action=" (7 chars) at end
    let expected_start = doc2.rfind("acc").unwrap() as u64;
    assert_eq!(edit2["range"]["start"]["character"], expected_start);
    assert_eq!(edit2["range"]["end"]["character"], line0_len as u64);
}

// ── didChange preserves the live cache ───────────────────────────

#[test]
fn test_did_change_preserves_live_cache_coalescing_and_negative_cooldown() {
    // Regression: didChange used to call `live_cache.clear_all()` on
    // every keystroke, discarding fresh entries, negative cooldowns,
    // and the 2 s fetch-coalescing marker — so N rapid edits produced
    // N background fetches. TTL/negative-TTL must govern instead.
    let mut server = Server::new(synthetic_data());
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///live.rsc", "text": "/ip/address add address=1.1.1.1\n"}}
    });
    server.handle_message("textDocument/didOpen", &open);

    // Seed: one fresh entry, one negative cooldown, one in-flight
    // fetch marker (coalescing window open for "interfaces").
    {
        let mut guard = server.live_cache.lock().unwrap();
        guard.insert(
            "interfaces".to_string(),
            vec!["ether1".to_string(), "ether2".to_string()],
        );
        guard.insert_negative("ip_addresses".to_string());
        guard.record_fetch_attempt("interfaces".to_string());
        assert!(
            !guard.can_spawn_fetch("interfaces"),
            "precondition: interfaces fetch must be coalesced"
        );
    }

    // N rapid unrelated edits (keystrokes on another line).
    for i in 0..8 {
        let change = serde_json::json!({
            "params": {
                "textDocument": {"uri": "file:///live.rsc"},
                "contentChanges": [{"text": format!("/ip/address add address=1.1.1.{i}\n# edit {i}\n")}]
            }
        });
        server.handle_message("textDocument/didChange", &change);
    }

    // Cache survives unrelated edits: at most one background fetch per
    // 2 s window (coalescing marker intact) and negative cooldown kept.
    {
        let guard = server.live_cache.lock().unwrap();
        assert!(
            guard.try_get_cached("interfaces").is_some(),
            "fresh live entry must survive didChange edits (TTL governs)"
        );
        assert!(
            guard.is_negative_cooldown("ip_addresses"),
            "negative cooldown must survive didChange edits"
        );
        assert!(
            !guard.can_spawn_fetch("interfaces"),
            "coalescing window must survive didChange: N rapid edits yield <=1 fetch per 2 s"
        );
    }

    // didClose remains an invalidation point.
    let close = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///live.rsc"}}
    });
    server.handle_message("textDocument/didClose", &close);
    {
        let guard = server.live_cache.lock().unwrap();
        assert!(
            guard.try_get_cached("interfaces").is_none(),
            "didClose must invalidate the live cache"
        );
        assert!(
            !guard.is_negative_cooldown("ip_addresses"),
            "didClose must clear negative cooldown state"
        );
    }
}

#[test]
fn test_did_change_configuration_clears_live_cache_on_connection_change() {
    // LiveConfig change invalidates; an unscoped (ignored) overlay keeps
    // the cache intact. F2: the scoped host change needs the transport
    // opt-in to apply (and then clears the cache).
    let mut server = Server::new(synthetic_data());
    {
        let mut guard = server.live_cache.lock().unwrap();
        guard.insert("interfaces".to_string(), vec!["ether1".to_string()]);
    }
    // Unscoped overlay is ignored: cache must survive.
    let unscoped = serde_json::json!({
        "params": {"settings": {"host": "10.9.9.9"}}
    });
    server.handle_message("workspace/didChangeConfiguration", &unscoped);
    assert!(
        server
            .live_cache
            .lock()
            .unwrap()
            .try_get_cached("interfaces")
            .is_some(),
        "ignored (unscoped) settings overlay must not clear the live cache"
    );
    // Scoped host change: connection identity changed, cache clears.
    crate::live::with_settings_transport_env(true, || {
        let scoped = serde_json::json!({
            "params": {"settings": {"rsc": {"live": {"host": "10.9.9.9"}}}}
        });
        server.handle_message("workspace/didChangeConfiguration", &scoped);
        assert_eq!(server.live_config.host, "10.9.9.9");
        assert!(
            server
                .live_cache
                .lock()
                .unwrap()
                .try_get_cached("interfaces")
                .is_none(),
            "LiveConfig host change must invalidate the live cache"
        );
    });
}

#[test]
fn test_live_connection_changed_predicate() {
    use crate::live::LiveConfig;
    use crate::server::live_connection_changed;
    let base = LiveConfig::from_env_with(|_| None);
    // Identical config: no invalidation.
    assert!(!live_connection_changed(&base, &base.clone()));
    // Host change invalidates.
    let mut changed = base.clone();
    changed.host = "10.9.9.9".to_string();
    assert!(live_connection_changed(&base, &changed));
    // Timeout-only change also selects a different fetch profile.
    let mut timeout_changed = base.clone();
    timeout_changed.timeout_secs = base.timeout_secs + 1;
    assert!(live_connection_changed(&base, &timeout_changed));
    // F4: TLS-identity rotation invalidates.
    let mut pin_changed = base.clone();
    pin_changed.fingerprint = Some([0xabu8; 32]);
    assert!(live_connection_changed(&base, &pin_changed));
    let mut invalid_changed = base.clone();
    invalid_changed.fingerprint_invalid = true;
    assert!(live_connection_changed(&base, &invalid_changed));
    let mut ca_changed = base.clone();
    ca_changed.ca_file = "/tmp/ca.pem".to_string();
    assert!(live_connection_changed(&base, &ca_changed));
}
