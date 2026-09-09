// Completion ranking goldens (filter, fetch).
// Copied (not moved) from `lsp/src/completion.rs` (`mod completion_ranking_goldens` L2758-2955); the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

// Ranking golden tests over the REAL embedded dataset.
//
// These pin relevance ranking (`sortText`), `filterText`, and
// `textEdit` replacement for `/ip/firewall/filter` and `/tool/fetch`.
// A `textEdit` is verified by SIMULATING the replacement it encodes:
// splicing `new_text` over `range` on the input line must yield the
// completed line (e.g. `chain=in` + `input` → `chain=input`, never
// `chain=ininput`).

/// Apply a completion `textEdit` to `line` and return the result.
///
/// Test-only ASCII simulation: ranges produced by this layer are byte
/// offsets on a single line, so a byte splice is faithful here.
fn apply_edit(line: &str, edit: &TextEdit) -> String {
    let s = edit.range.start.character as usize;
    let e = edit.range.end.character as usize;
    assert_eq!(edit.range.start.line, 0);
    assert_eq!(edit.range.end.line, 0);
    assert!(s <= e && e <= line.len(), "edit range {s}..{e} in {line:?}");
    format!("{}{}{}", &line[..s], edit.new_text, &line[e..])
}

// ── /ip/firewall/filter chain ─────────────────────────────────

#[test]
fn test_golden_filter_chain_empty_gives_common_hints() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter add chain=";
    let items = compute_completions(&data, line);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["input", "forward", "output"]);
    for item in &items {
        assert_eq!(item.detail.as_deref(), Some(COMMON_HINT_DETAIL));
        assert_eq!(
            item.sort_text.as_deref(),
            Some(match item.label.as_str() {
                "input" => "5",
                "forward" => "5",
                "output" => "5",
                other => panic!("unexpected hint {other}"),
            })
        );
        assert_eq!(item.filter_text.as_deref(), Some(item.label.as_str()));
        // Empty suffix → zero-length insertion edit at the cursor.
        let edit = item.text_edit.as_ref().expect("value textEdit");
        assert_eq!(apply_edit(line, edit), format!("{line}{}", item.label));
    }
}

#[test]
fn test_golden_filter_chain_partial_replaces_suffix() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter add chain=in";
    let items = compute_completions(&data, line);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.label, "input");
    // Exact-tier common hint rank: tier 5, prefix match.
    assert_eq!(item.sort_text.as_deref(), Some("51_input"));
    let edit = item.text_edit.as_ref().expect("value textEdit");
    // The ininput regression guard: accepting `input` over typed `in`
    // must REPLACE, not append.
    assert_eq!(
        apply_edit(line, edit),
        "/ip/firewall/filter add chain=input"
    );
}

#[test]
fn test_golden_filter_chain_partial_open_quote_preserved() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter add chain=\"in";
    let items = compute_completions(&data, line);
    assert_eq!(items.len(), 1);
    let edit = items[0].text_edit.as_ref().expect("value textEdit");
    assert_eq!(
        apply_edit(line, edit),
        "/ip/firewall/filter add chain=\"input"
    );
}

#[test]
fn test_golden_filter_action_prefix_before_substring() {
    let data = MenuData::load();
    let line = "/ip/firewall/filter add action=ac";
    let items = compute_completions(&data, line);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // `accept` is a prefix match; `fasttrack-connection` only a
    // substring match — exact-prefix > substring within the enum tier.
    assert_eq!(labels, vec!["accept", "fasttrack-connection"]);
    assert_eq!(items[0].sort_text.as_deref(), Some("41_accept"));
    assert_eq!(
        items[1].sort_text.as_deref(),
        Some("42_fasttrack-connection")
    );
    for item in &items {
        assert!(
            item.detail
                .as_deref()
                .unwrap_or("")
                .starts_with("enum value"),
            "true enum tier keeps enum detail, got {:?}",
            item.detail
        );
    }
    let edit = items[0].text_edit.as_ref().expect("value textEdit");
    assert_eq!(
        apply_edit(line, edit),
        "/ip/firewall/filter add action=accept"
    );
}

#[test]
fn test_golden_filter_action_typo_falls_back_demoted() {
    let data = MenuData::load();
    let items = compute_completions(&data, "/ip/firewall/filter add action=zzz");
    assert!(!items.is_empty(), "typo keeps the fallback set");
    for item in &items {
        assert!(
            item.sort_text
                .as_deref()
                .unwrap_or("")
                .starts_with(RankTier::Demoted.prefix()),
            "fallback demoted to lowest tier, got {:?}",
            item.sort_text
        );
        assert!(
            item.detail
                .as_deref()
                .unwrap_or("")
                .contains("no prefix match"),
            "fallback carries a detail hint, got {:?}",
            item.detail
        );
    }
}

// ── /tool/fetch ───────────────────────────────────────────────

#[test]
fn test_golden_fetch_http_method_prefix_ranking_and_replace() {
    let data = MenuData::load();
    assert!(
        data.menu_by_path.contains_key("/tool/fetch"),
        "golden menu must exist in the embedded table"
    );
    let line = "/tool/fetch output=file http-method=p";
    let items = compute_completions(&data, line);
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // Prefix matches only (`post`, `put`, `patch`), relevance-sorted.
    assert_eq!(labels, vec!["patch", "post", "put"]);
    assert_eq!(items[0].sort_text.as_deref(), Some("41_patch"));
    let edit = items[0].text_edit.as_ref().expect("value textEdit");
    assert_eq!(
        apply_edit(line, edit),
        "/tool/fetch output=file http-method=patch"
    );
}

#[test]
fn test_golden_fetch_output_enum_members_share_tier_4() {
    let data = MenuData::load();
    let items = compute_completions(&data, "/tool/fetch output=");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"file"));
    assert!(labels.contains(&"user"));
    // True enum members share the tier-4 key and keep document order.
    for item in &items {
        assert_eq!(item.sort_text.as_deref(), Some("4"));
        assert_eq!(item.filter_text.as_deref(), Some(item.label.as_str()));
    }
}

#[test]
fn test_golden_fetch_partial_property_name_boosts_rank() {
    let data = MenuData::load();
    // `get` parses as the verb; `http-m` is the partial property name.
    let items = compute_completions(&data, "/tool/fetch get http-m");
    let method = items
        .iter()
        .find(|i| i.label == "http-method")
        .expect("http-method suggested");
    // Optional property, prefix match: tier 1, rank 1.
    assert_eq!(method.sort_text.as_deref(), Some("11_http-method"));
    assert_eq!(method.filter_text.as_deref(), Some("http-method"));
    // Non-matching properties sort after within their tier, and the set
    // itself is never filtered (client fuzzy is the authority).
    let url = items.iter().find(|i| i.label == "url").expect("url kept");
    assert!(method.sort_text < url.sort_text);
    // Property items carry no positional textEdit from this layer (the
    // server owns positional edits and has no injector for this kind).
    assert!(method.text_edit.is_none());
}

// ── Pure rank() unit pins ─────────────────────────────────────
