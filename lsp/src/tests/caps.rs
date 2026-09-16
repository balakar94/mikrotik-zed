// Caps registry pins.
use crate::caps::*;
use crate::menus::MenuData;
use crate::server::Server;
use crate::text_util;
use std::sync::Arc;
fn severity_data() -> Arc<MenuData> {
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
[[menus.arguments]]
name = "comment"
type = "string"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
"#,
    ))
}

fn code_of(
    diags: &[crate::diagnostics::Diagnostic],
    code: &str,
) -> Vec<crate::diagnostics::Diagnostic>
where
    crate::diagnostics::Diagnostic: Clone,
{
    diags
        .iter()
        .filter(|d| d.code.as_deref() == Some(code))
        .cloned()
        .collect()
}

// ── Folding cap (safe additive pin) ──────────────────────────────────────

#[test]
fn severity_folding_ranges_capped_at_5000() {
    // 6000 independent two-line brace blocks → over the cap.
    let mut doc = String::new();
    for _ in 0..6000 {
        doc.push_str(":do {\n:put x\n}\n");
    }
    let ranges = crate::folding::compute_folding_ranges(&doc);
    assert_eq!(ranges.len(), 5000, "folding cap must hold");
}

#[test]
fn severity_folding_continuation_carries_no_kind() {
    let doc = "/tool/fetch url=\"https://example.com/a/b\\\nc\"\n";
    let ranges = crate::folding::compute_folding_ranges(doc);
    let cont = ranges
        .iter()
        .find(|r| r.start_line == 0 && r.end_line == 1)
        .expect("continuation fold 0..1");
    assert!(cont.kind.is_none(), "continuation folds carry no kind");
    let wire = serde_json::to_value(&ranges).unwrap();
    assert!(wire[0].get("kind").is_none());
}

// ── didChange batch complement (distinct URIs, no overlap) ───────────────

#[test]
fn severity_didchange_batch_applies_edits_after_malformed_element_and_publishes_once() {
    let mut server = Server::new(severity_data());
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///severity-batch.rsc", "text": "hello world"}}}),
    );
    server.published.clear();
    let resp = server.handle_message(
        "textDocument/didChange",
        &serde_json::json!({"params": {
            "textDocument": {"uri": "file:///severity-batch.rsc"},
            "contentChanges": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}, "text": "hi"},
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}},
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "text": "Rust"}
            ]
        }}),
    );
    assert!(resp.is_none(), "didChange is a notification");
    assert_eq!(
        server.docs.get("file:///severity-batch.rsc").unwrap(),
        "hi Rust"
    );
    assert_eq!(
        server.published.len(),
        1,
        "exactly one publish per didChange batch"
    );
    assert_eq!(server.published[0].0, "file:///severity-batch.rsc");
}

// ── Shared caps pins ─────────────────────────────────────────────────────

#[test]
fn severity_caps_max_docs_is_100_and_enforced() {
    assert_eq!(crate::MAX_DOCS, 100);
    let mut server = Server::new(severity_data());
    for i in 0..100 {
        let uri = format!("file:///severity-cap-{i}.rsc");
        server.handle_message(
            "textDocument/didOpen",
            &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": "hi"}}}),
        );
    }
    assert_eq!(server.docs.len(), 100);
    server.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": "file:///severity-cap-overflow.rsc", "text": "hi"}}}),
    );
    assert!(
        !server
            .docs
            .contains_key("file:///severity-cap-overflow.rsc"),
        "101st document must be rejected at MAX_DOCS"
    );
}

#[test]
fn severity_caps_max_diagnostics_is_2000() {
    assert_eq!(crate::MAX_DIAGNOSTICS, 2000);
}

// ── Former SPEC-ONLY proposals (pointers, not asserts) ───────────────────
// The behavior changes below have landed; the contract lives with
// the owning module's tests. This file keeps no duplicate asserts (owner:
// live.rs / server.rs), only pointers so the history stays greppable.
//
// (a) TRANSPORT DOWNGRADE GATE — IMPLEMENTED in `live.rs`:
//     `apply_settings_value` ignores transport-security keys from workspace
//     settings unless `RSC_LS_ALLOW_SETTINGS_TRANSPORT=1` (env always wins).
//     Pinned by the `live.rs` settings-gate tests.
// (b) USER VALIDATION + LOG SANITIZATION — IMPLEMENTED in `live.rs`:
//     `user` is validated against the allowlist (invalid → `admin` + WARN)
//     and `log_status` never emits `pass`. Pinned by `live.rs` tests.
// (c) LIVE CACHE SURVIVES didChange — IMPLEMENTED in `server.rs`:
//     `textDocument/didChange` never calls `live_cache.clear_all()`; TTL,
//     negative TTL, and the fetch-coalescing window govern freshness.
//     Pinned by `server.rs::test_did_change_preserves_live_cache_*`.
//
// (e) TASKS WORKFLOW ORDER WITHOUT SECRETS — spec for validator:
//     `languages/rsc/tasks.json` MUST keep order: validate/dry-run first,
//     deploy REST/SSH after, live-check last, enable-hint informational;
//     NO task `env` may carry `MIKROTIK_PASS` (or `*_PASS`/`*_TOKEN`);
//     live-check MUST take host/user via `${input:...}` prompts only.
//     Suggested check (no edit — read-only validation):
//     `python3 -c "import json; t=json.load(open('languages/rsc/tasks.json')); ..."`.
//     See acceptance checklist in the final report.

#[test]
fn test_caps_table_values_match_consts() {
    assert_eq!(MAX_HEADER_SIZE, 32 * 1024);
    assert_eq!(MAX_MESSAGE_SIZE, 10 * 1024 * 1024);
    assert_eq!(MAX_DOC_SIZE, 5 * 1024 * 1024);
    assert_eq!(MAX_DOCS, 100);
    assert_eq!(MAX_CODE_ACTIONS, 8);
    assert_eq!(MAX_DIAG_LINES, 3000);
    assert_eq!(MAX_DIAG_BYTES, 500_000);
    assert_eq!(MAX_DIAGNOSTICS, 2000);
    assert_eq!(MAX_COMPLETION_ITEMS, 200);
    assert_eq!(MAX_LIVE_ITEMS, 500);
    assert_eq!(MAX_LIVE_VALUE_LEN, 64);
    assert_eq!(MAX_LIVE_RESPONSE_BYTES, 512 * 1024);
    assert_eq!(MAX_CACHE_ENTRIES, 16);
    assert_eq!(LIVE_TTL_SECS, 60);
    assert_eq!(LIVE_TIMEOUT_SECS, 5);
    assert_eq!(LIVE_FETCH_BLOCKING_TIMEOUT_SECS, 2);
    assert_eq!(LIVE_NEGATIVE_TTL_SECS, 15);
    assert_eq!(LIVE_MAX_HOSTS, 4);
    assert_eq!(LIVE_CUSTOM_RESOURCES_MAX, 8);
    // Feature-local caps indexed in the table above (defined beside
    // their feature, values pinned here against drift).
    // `live.rs` (`MAX_CONCURRENT_FETCHES`) and `diagnostics.rs`
    // (`MAX_SYNTAX_DIAGNOSTICS`) stay private to their owner modules
    // (out of scope here) and are pinned by those modules' own tests;
    // every other table row is asserted below.
    assert_eq!(crate::symbols::MAX_SYMBOLS, 5000);
    assert_eq!(crate::folding::MAX_FOLDING_RANGES, 5000);
    assert_eq!(crate::navigation::MAX_REFERENCES, 1000);
    assert_eq!(crate::parser::MAX_BRACE_DEPTH, 4096);
    assert_eq!(crate::signature::MAX_SIGNATURE_PROPERTIES, 40);
    assert_eq!(crate::suggest::MAX_SUGGEST_INPUT_BYTES, 256);
    assert_eq!(crate::diagnostics::MAX_DIAG_TEXT_CHARS, 120);
    // Display-budget micro-caps owned by `text_util`.
    assert_eq!(text_util::MAX_DETAIL_CHARS, 256);
    assert_eq!(text_util::MAX_DETAIL_TYPE_CHARS, 64);
    assert_eq!(text_util::MAX_HOVER_PROPERTIES, 12);
    assert_eq!(text_util::MAX_HOVER_DESC_CHARS, 800);
    assert_eq!(text_util::MAX_LABEL_TYPE_CHARS, 64);
    assert_eq!(text_util::MAX_SIGNATURE_LABEL_BYTES, 4096);
}

// ── Wire-contract mirrors (integration tests) ────────────────────────────
//
// `lsp/tests/perf_smoke.rs` and `lsp/tests/framing_chaos.rs` speak the raw
// wire protocol and keep local `const` mirrors of the server caps (they
// cannot import `caps.rs`: each integration target builds alone). The
// mirrors pin the CONTRACT (bounded output, skip-and-stay-aligned) while
// this test pins the VALUES: changing a cap here without updating the
// mirror fails here, and changing a mirror without the cap fails the wire
// test itself. Change both together, never one side alone.

/// Read an integration-test mirror file from `lsp/tests/`.
fn mirror_source(name: &str) -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cap mirror {name} unreadable at {}: {e}", path.display()))
}

/// Evaluate a mirror `const NAME: usize = <expr>;` where `<expr>` is a
/// product of integer literals (`2000`, `32 * 1024`). Anything else fails
/// loudly so a hand-edited mirror cannot silently stop asserting.
fn mirror_value(src: &str, name: &str) -> usize {
    let needle = format!("const {name}: usize =");
    let start = src.find(&needle).unwrap_or_else(|| {
        panic!("cap mirror const `{name}` missing; update the wire mirror when changing caps.rs")
    });
    let tail = &src[start + needle.len()..];
    let end = tail
        .find(';')
        .expect("mirror const expression must end with `;`");
    tail[..end]
        .split('*')
        .map(|part| {
            part.trim().replace('_', "").parse::<usize>().unwrap_or_else(
                |_| panic!("cap mirror const `{name}` has an unevaluatable expression; keep it a plain integer product"),
            )
        })
        .product()
}

#[test]
fn test_wire_mirrors_match_caps() {
    // `perf_smoke.rs` mirrors use wire-side names; each assert maps the
    // mirror name to its authoritative `caps.rs` const.
    let perf = mirror_source("perf_smoke.rs");
    assert_eq!(
        mirror_value(&perf, "MAX_DOC_BYTES"),
        MAX_DOC_SIZE,
        "perf_smoke.rs MAX_DOC_BYTES drifted from caps.rs MAX_DOC_SIZE; change both together"
    );
    assert_eq!(
        mirror_value(&perf, "MAX_DIAG_ITEMS"),
        MAX_DIAGNOSTICS,
        "perf_smoke.rs MAX_DIAG_ITEMS drifted from caps.rs MAX_DIAGNOSTICS; change both together"
    );
    assert_eq!(
        mirror_value(&perf, "MAX_COMPLETION_ITEMS"),
        MAX_COMPLETION_ITEMS,
        "perf_smoke.rs MAX_COMPLETION_ITEMS drifted from caps.rs MAX_COMPLETION_ITEMS; change both together"
    );
    // `framing_chaos.rs` mirrors the framing caps.
    let framing = mirror_source("framing_chaos.rs");
    assert_eq!(
        mirror_value(&framing, "MAX_HEADER_BYTES"),
        MAX_HEADER_SIZE,
        "framing_chaos.rs MAX_HEADER_BYTES drifted from caps.rs MAX_HEADER_SIZE; change both together"
    );
    assert_eq!(
        mirror_value(&framing, "MAX_MESSAGE_BYTES"),
        MAX_MESSAGE_SIZE,
        "framing_chaos.rs MAX_MESSAGE_BYTES drifted from caps.rs MAX_MESSAGE_SIZE; change both together"
    );
}
