//! White-box: sanitizer.

use crate::menus::MenuData;
use std::sync::Arc;

fn validator_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/validator/typed"
type = "Directory"
[[menus.arguments]]
name = "flag"
type = "bool"
required = true
[[menus.arguments]]
name = "count"
type = "num"
[[menus.arguments]]
name = "uptime"
type = "time"
[[menus.arguments]]
name = "hw"
type = "macAddr"
[[menus.arguments]]
name = "peer"
type = "ipAddr"
[[menus.arguments]]
name = "bits"
type = "ubit (1Mbps, 2Mbps)"
[[menus.arguments]]
name = "freeform"
type = ""
[[menus]]
path = "/validator/props"
type = "Directory"
[[menus.arguments]]
name = "name"
type = "string"
required = true
[[menus.arguments]]
name = "temp"
type = "string"
unset = true
[[menus.arguments]]
name = "fixed"
type = "string"
unset = false
[[menus.read_only]]
name = "serial"
type = "string"
description = "factory serial"
[[menus]]
path = "/validator/chain"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum"
[[menus]]
path = "/validator/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop)"
"#,
    ))
}

// ── Hover markdown fixtures ────────────────────────────────────────

fn validator_markdown_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/validator/md"
type = "Directory"
[[menus.arguments]]
name = "linky"
type = "string"
description = "See [docs](https://example.com/x) for details"
[[menus.arguments]]
name = "picky"
type = "string"
description = "Logo ![alt](https://example.com/i.png) inline"
[[menus.arguments]]
name = "tricky"
type = "string"
description = "Run <script>alert(1)</script> now"
[[menus.arguments]]
name = "ctrl"
type = "string"
description = "Bell here \u0007 done"
"#,
    )
}

#[test]
fn validator_hover_markdown_link_and_image_shape() {
    let data = validator_markdown_data();
    for (prop, text, raw) in [("linky", "docs", "[docs]"), ("picky", "Logo", "![alt]")] {
        let line = format!("/validator/md add {prop}=v");
        let pos = line.find(prop).unwrap() + 1;
        let hover = crate::hover::compute_hover(&data, &line, pos, &line, 0)
            .unwrap_or_else(|| panic!("{prop} must hover"));
        assert_eq!(hover.contents.kind, "markdown");
        assert!(
            hover.contents.value.contains(text),
            "{prop} hover must carry fixture text, got {}",
            hover.contents.value,
        );
        assert!(
            !hover.contents.value.contains(raw) && !hover.contents.value.contains("https://"),
            "{prop} hover must sanitize raw markup/URLs, got {}",
            hover.contents.value,
        );
    }
    // IMPLEMENTED (`hover.rs` via `text_util::sanitize_markdown_for_hover`,
    // truncate-then-strip): raw links rewrite to text, images drop, URLs
    // never survive into the popup.
}

#[test]
fn validator_hover_markdown_script_and_control_never_panics() {
    let data = validator_markdown_data();
    for prop in ["tricky", "ctrl"] {
        let line = format!("/validator/md add {prop}=v");
        let pos = line.find(prop).unwrap() + 1;
        // Must not panic on hostile description bytes; result shape stays.
        let hover = crate::hover::compute_hover(&data, &line, pos, &line, 0);
        assert!(hover.is_some(), "{prop} must hover without panicking");
        assert_eq!(hover.unwrap().contents.kind, "markdown");
    }
    // IMPLEMENTED (`hover.rs` via `text_util::sanitize_markdown_for_hover`):
    // `<script>`/raw-HTML spans are stripped and control characters removed;
    // hover keeps returning markdown with the property name intact.
}

// ── Signature offset integrity ─────────────────────────────────────

#[test]
fn validator_signature_label_offsets_slice_exactly() {
    let data = validator_data();
    let menu = data.menu_by_path.get("/validator/typed").expect("menu");
    let line = "/validator/typed add ";
    let tokens = crate::tokenize_with_spans(line);
    let verb_idx = crate::signature::resolve_verb_token(&data, &tokens).expect("verb");
    let help = crate::signature::compute_signature_help(menu, &tokens, verb_idx, line.len())
        .expect("signature with properties");
    assert_eq!(help.signatures.len(), 1);
    assert_eq!(help.active_signature, 0);
    let sig = &help.signatures[0];
    assert!(sig.label.starts_with("/validator/typed add "));
    for p in &sig.parameters {
        assert!(p.label[0] < p.label[1] && p.label[1] <= sig.label.len());
        let seg = &sig.label[p.label[0]..p.label[1]];
        assert!(seg.contains('='), "segment must be name=type, got {seg:?}");
    }
    if let Some(active) = help.active_parameter {
        assert!((active as usize) < sig.parameters.len());
    }
    // Wire shape: camelCase activeSignature; activeParameter optional.
    let wire = serde_json::to_value(&help).expect("serializes");
    assert!(wire.get("activeSignature").is_some());
}

// ── Error shape: -32602 pin, -32700 implemented (see pointer below) ──

#[test]
fn validator_invalid_params_shape_pins_32602_with_id_echo() {
    // The existing helper is the only error constructor reachable
    // without editing `server.rs`; pin its wire shape for both id kinds.
    for id in [serde_json::json!(7), serde_json::json!("validator-str-id")] {
        let resp = crate::server::invalid_params_response(&id, "missing position.line");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["error"]["code"], -32602);
        assert_eq!(resp["error"]["message"], "missing position.line");
        assert_eq!(resp["id"], id, "id must be echoed verbatim");
    }
}

// IMPLEMENTED: JSON-RPC parse error (-32700) lives in `server.rs` as
// `parse_error_response` / `extract_id_for_parse_error` (well-framed but
// malformed bodies answer `{"jsonrpc": "2.0", "id": <best-effort>,
// "error": {"code": -32700, …}}`), pinned by
// `server.rs::test_parse_error_response_shape`. No duplicate assert here
// (stream ownership: server.rs).

// ── Completion common-hint tier ────────────────────────────────────

#[test]
fn validator_chain_bare_enum_yields_common_hints_with_exact_detail() {
    let data = validator_data();
    // Bare `enum` (no members): the curated built-in chains fill the gap.
    let items = crate::completion::compute_completions(&data, "/validator/chain add chain=");
    assert_eq!(
        items.len(),
        3,
        "exactly the three built-in chains, got {} items",
        items.len()
    );
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["input", "forward", "output"]);
    for item in &items {
        assert_eq!(
            item.detail.as_deref(),
            Some("common value — verify on device"),
            "common-hint detail text is exact",
        );
        let sort = item
            .sort_text
            .as_deref()
            .expect("value items carry sortText");
        assert!(
            sort.starts_with('5'),
            "common hints live one tier below true enum members (4), got {sort:?}",
        );
    }
}

#[test]
fn validator_action_real_members_yield_zero_common_hints() {
    let data = validator_data();
    // Real members documented: no curated hint may leak in.
    let items = crate::completion::compute_completions(&data, "/validator/filter add action=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["accept", "drop"]);
    assert!(
        items
            .iter()
            .all(|i| i.detail.as_deref() != Some("common value — verify on device")),
        "documented members must not carry the common-hint detail",
    );
    for item in &items {
        let sort = item
            .sort_text
            .as_deref()
            .expect("value items carry sortText");
        assert!(
            sort.starts_with('4'),
            "true enum members rank tier 4, got {sort:?}",
        );
    }
}
