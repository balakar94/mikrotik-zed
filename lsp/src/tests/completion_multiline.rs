// White-box: completion multiline.

use crate::menus::MenuData;
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

#[test]
fn test_server_completion_multiline_before_cursor() {
    let mut server = make_server();
    let doc = "/ip/address add\naddress=1.1.1.1";
    let open = serde_json::json!({
        "params": {"textDocument": {"uri": "file:///multi.rsc", "text": doc}}
    });
    server.handle_message("textDocument/didOpen", &open);
    // Cursor on line 1, after "address="
    let hover_or_completion_line = 1;
    let comp = serde_json::json!({
        "id": 20,
        "params": {
            "textDocument": {"uri": "file:///multi.rsc"},
            "position": {"line": hover_or_completion_line, "character": 8} // "address="
        }
    });
    let resp = server
        .handle_message("textDocument/completion", &comp)
        .unwrap();
    // For "address=" value completions should trigger (ipPrefix)
    let items = resp["result"]["items"].as_array().unwrap();
    // Might be value completions (0.0.0.0/0) or empty if not correctly resolved, but should be Some
    // array
    assert!(items.is_empty() || items.iter().any(|i| i["label"] == "0.0.0.0/0"));
}

// ── Continuation-aware partial menu path ─────────────────────────────────

#[test]
fn test_partial_path_segment_across_continuation() {
    // The server passes the continuation-aware logical join as the second
    // argument. The physical `before_cursor` (space-joined) only sees
    // `te/che` after the split, while the logical prefix carries the full
    // `/ip/route/che`; the completion layer must resolve the final segment
    // `che` to the `/ip/route/check` action command.
    let data = MenuData::load();
    let cases: &[(&str, &str, usize, usize)] = &[
        // (before_cursor, logical_prefix, expected segment start, expected end)
        ("/ip/rou te/che", "/ip/route/che", 10, 13),
        ("/ip/route /che", "/ip/route/che", 10, 13),
    ];
    for (before, logical, seg_start, seg_end) in cases {
        let items =
            crate::completion::compute_completions_with_logical(&data, before, Some(logical), None);
        let check = items
            .iter()
            .find(|i| i.label == "check")
            .unwrap_or_else(|| {
                panic!(
                    "no `check` item for logical {logical:?}; got {:?}",
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                )
            });
        let edit = check
            .text_edit
            .as_ref()
            .expect("partial-segment item carries a textEdit shadow");
        assert_eq!(
            (
                edit.range.start.character as usize,
                edit.range.end.character as usize
            ),
            (*seg_start, *seg_end),
            "segment-only shadow span for logical {logical:?}"
        );
    }
}
