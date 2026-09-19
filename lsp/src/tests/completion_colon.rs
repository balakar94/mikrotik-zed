// Colon trigger completion contexts.
// Copied (not moved) from `lsp/src/completion.rs` (`mod extra_coverage` L1694-1747, L2299-2306,
// L2431-2543); the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

fn synthetic() -> MenuData {
    MenuData::from_toml_str(
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
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/route"
type = "Directory"
[[menus.arguments]]
name = "gateway"
type = "ipAddr"
[[menus]]
path = "/ip/route/check"
type = "Command"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "src-address"
type = "ipAddr"
[[menus]]
path = "/system/clock"
type = "Directory"
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "time-zone-name"
type = "string"
"#,
    )
}
const SNIPPET_LABELS: [&str; 4] = [":if", ":foreach", ":for", ":do"];

fn snippet_items(items: &[CompletionItem]) -> Vec<&CompletionItem> {
    items
        .iter()
        .filter(|i| SNIPPET_LABELS.contains(&i.label.as_str()))
        .collect()
}
// ── ':' trigger character ────────────────────────────────────────────────

#[test]
fn test_colon_bare_at_statement_start_returns_only_colon_items() {
    let data = synthetic();
    // ':' alone at a fresh statement fires mid-token: the four statement
    // snippets plus the shared SCRIPT_GLOBALS entries are the colon-prefixed
    // candidates, and NOTHING else (no root menus, no verbs) may leak in.
    let items = compute_completions(&data, ":");
    let globals = crate::script_globals::SCRIPT_GLOBALS.len();
    assert_eq!(
        items.len(),
        globals,
        "got {:?}",
        items.iter().map(|i| &i.label).collect::<Vec<_>>()
    );
    for i in &items {
        assert!(
            i.label.starts_with(':'),
            "only ':'-prefixed labels allowed, got {}",
            i.label
        );
    }
    // The four structural snippets survive the dedupe (richer insert text).
    assert_eq!(snippet_items(&items).len(), 4);
}

#[test]
fn test_colon_prefix_filters_to_matching_script_items() {
    let data = synthetic();
    // ':i' narrows to :if …
    let items = compute_completions(&data, ":i");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec![":if"]);
    // …':fo' keeps :foreach and :for in offer-table order…
    let items = compute_completions(&data, ":fo");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec![":foreach", ":for"]);
    // …and a fully typed script global completes to itself only — no
    // fallback to menu noise after a colon.
    let items = compute_completions(&data, ":put");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec![":put"]);
}

#[test]
fn test_colon_global_items_rank_under_menu_tiers() {
    let data = synthetic();
    let items = compute_completions(&data, ":put");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.label, ":put");
    assert_eq!(item.kind, Some(kind::FUNCTION));
    assert!(
        item.sort_text.as_deref().unwrap_or("").starts_with('2'),
        "script global uses the Verb tier (2), got {:?}",
        item.sort_text
    );
    assert!(
        item.documentation.is_some(),
        "script global carries the shared docs"
    );
    assert_eq!(item.filter_text.as_deref(), Some(":put"));
}

#[test]
fn test_colon_after_block_opener_still_offers_snippets() {
    let data = synthetic();
    // '{ :' — the brace opened the block; the colon word begins the
    // next statement inside it.
    let items = compute_completions(&data, "{ :");
    assert_eq!(snippet_items(&items).len(), 4);
    for i in &items {
        assert!(i.label.starts_with(':'), "colon context leaked {}", i.label);
    }
}

#[test]
fn test_colon_mid_statement_returns_only_script_globals() {
    let data = synthetic();
    // A colon word after a verb is NOT a statement start: no structural
    // snippets, but the script globals (`:put`, …) still apply — and no
    // property or menu noise may leak through the colon filter.
    let items = compute_completions(&data, "/ip/address print :");
    assert!(!items.is_empty());
    assert!(
        items.iter().all(|i| i.label.starts_with(':')),
        "colon context leaked {}",
        items
            .iter()
            .find(|i| !i.label.starts_with(':'))
            .map(|i| &i.label)
            .unwrap()
    );
    assert!(items.iter().any(|i| i.label == ":put"));
    assert!(
        !items.iter().any(|i| i.kind == Some(kind::SNIPPET)),
        "mid-statement colon must not offer structural snippets"
    );
}

#[test]
fn test_trailing_space_after_colon_not_filtered() {
    let data = synthetic();
    // ':' finished with a space starts a NEW (empty) token — that
    // request is not a script-word completion and must behave exactly
    // as before the ':' trigger existed: plain root completions, no
    // snippets (a lone ':' is not a `{`/`;` opener).
    let items = compute_completions(&data, ": ");
    assert!(
        items.iter().any(|i| i.label == "/ip"),
        "finished ':' token keeps ordinary root completions"
    );
    assert_eq!(snippet_items(&items).len(), 0);
}

#[test]
fn test_quoted_colon_is_not_script_word_context() {
    let data = synthetic();
    // A quote opens this token, so it never enters the ':' branch:
    // root menus flow through unfiltered.
    let items = compute_completions(&data, "\"a:b");
    assert!(
        items.iter().any(|i| i.label == "/ip"),
        "quoted colon must not trigger script-word filtering"
    );
}

#[test]
fn test_non_colon_contexts_unchanged_by_colon_trigger() {
    let data = synthetic();
    // Roots + snippets at a plain statement start…
    let empty = compute_completions(&data, "");
    assert!(empty.iter().any(|i| i.label == "/ip"));
    assert_eq!(snippet_items(&empty).len(), 4);
    // …arguments after a verb…
    let args = compute_completions(&data, "/ip/address add ");
    assert!(args.iter().any(|i| i.label == "address"));
    assert!(!args.is_empty());
    // …values after '='…
    let vals = compute_completions(&data, "/ip/firewall/filter add chain=in");
    assert_eq!(vals.len(), 1);
    // …and root navigation via '/'.
    let slash = compute_completions(&data, "/");
    assert!(slash.iter().any(|i| i.label == "/ip"));
}
