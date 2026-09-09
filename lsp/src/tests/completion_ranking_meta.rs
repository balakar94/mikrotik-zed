// Ranking tiers, live key, token span and truncation.
// Copied (not moved) from `lsp/src/completion.rs` (`mod completion_ranking_goldens` L2954-3096);
// the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

// ── Pure rank() unit pins ────────────────────────────────────────────────

#[test]
fn test_rank_tier_shapes_and_match_quality() {
    // Tier shapes: live uses the frozen `0!live_<label>` major key,
    // properties keep legacy `0name`/`1name` when untyped, snippet
    // shares one `9` key (stable-sort invariant, see module header).
    let order = [
        rank(RankTier::Live, "ether1", ""),
        rank(RankTier::RequiredProp, "chain", ""),
        rank(RankTier::OptionalProp, "action", ""),
        rank(RankTier::Verb, "add", ""),
        rank(RankTier::Submenu, "filter", ""),
        rank(RankTier::EnumValue, "accept", ""),
        rank(RankTier::CommonHint, "input", ""),
        rank(RankTier::Placeholder, "0.0.0.0/0", ""),
        rank(RankTier::Flag, "X", ""),
        rank(RankTier::Demoted, "accept", ""),
        rank(RankTier::Snippet, ":if", ""),
    ];
    assert_eq!(
        order,
        [
            "0!live_ether1",
            "0chain",
            "1action",
            "21_add",
            "31_filter",
            "4",
            "5",
            "6",
            "71_x",
            "8_accept",
            "9",
        ]
    );
    for window in order.windows(2) {
        assert!(window[0] < window[1], "{window:?} must be ordered");
    }
    // Within one value context the documented order holds:
    // enum < common < placeholder, demoted fallback last.
    let mut value_tiers = [
        rank(RankTier::EnumValue, "accept", ""),
        rank(RankTier::CommonHint, "input", ""),
        rank(RankTier::Placeholder, "0.0.0.0/0", ""),
        rank(RankTier::Demoted, "accept", ""),
    ];
    value_tiers.sort_unstable();
    assert_eq!(value_tiers, ["4", "5", "6", "8_accept"]);
    // Match quality within one tier: exact < prefix < substring.
    assert!(rank(RankTier::EnumValue, "input", "input") < rank(RankTier::EnumValue, "input", "in"));
    assert!(rank(RankTier::EnumValue, "input", "in") < rank(RankTier::EnumValue, "input", "n"));
    // Case-insensitive: `IN` still prefix-matches `input`.
    assert_eq!(
        rank(RankTier::EnumValue, "input", "IN"),
        rank(RankTier::EnumValue, "input", "in")
    );
    // Deterministic: same inputs, same output.
    assert_eq!(
        rank(RankTier::Verb, "Print", "pr"),
        rank(RankTier::Verb, "Print", "pr")
    );
}

#[test]
fn test_live_major_key_sorts_first_regardless_of_label() {
    // Golden pin for the `0!live_` major key: device truth must sort
    // before required properties for EVERY label pair — including labels
    // that would interleave under the old `0live_` prefix (`"0chain" <
    // "0live_…"`). Clients order `sortText` lexicographically
    // (byte-wise); `!` (0x21) is below every alphanumeric, so the second
    // byte alone decides the tier before any label text is compared.
    for live_label in ["aaa-first", "ether1", "zzz-last"] {
        for req_label in ["aaa-first", "chain", "zzz-last"] {
            let live = rank(RankTier::Live, live_label, "");
            let req = rank(RankTier::RequiredProp, req_label, "");
            assert!(
                live < req,
                "live {live:?} must sort before required {req:?}"
            );
        }
    }
    // Frozen shapes: live ignores the typed prefix (cache/recency order
    // survives filtering), while required/optional embed the match rank
    // once a prefix is typed.
    assert_eq!(
        rank(RankTier::Live, "ether1", ""),
        rank(RankTier::Live, "ether1", "eth")
    );
    assert_eq!(
        rank(RankTier::RequiredProp, "chain", ""),
        "0chain".to_string()
    );
    assert_eq!(
        rank(RankTier::RequiredProp, "chain", "ch"),
        "01_chain".to_string()
    );
    // Snippet invariant: every statement snippet shares the single `9`
    // key, so the stable sort preserves `STATEMENT_SNIPPETS` table order.
    let snippets = statement_snippet_items();
    assert!(
        snippets.iter().all(|s| s.sort_text.as_deref() == Some("9")),
        "all snippets share one tier key"
    );
    let labels: Vec<&str> = snippets.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, vec![":if", ":foreach", ":for", ":do"]);
}

#[test]
fn test_partial_name_token_span_basics() {
    // Plain partial word: span covers the token on the current line.
    let (text, s, e) = partial_name_token("/ip/firewall/filter add act").unwrap();
    assert_eq!((text.as_str(), s, e), ("act", 24, 27));
    // Finished token (trailing space) → no partial.
    assert!(partial_name_token("/ip/firewall/filter add ").is_none());
    // Property assignments are value territory, not name territory.
    assert!(partial_name_token("/ip/firewall/filter add chain=in").is_none());
    // Path, colon, and quote tokens are excluded.
    assert!(partial_name_token("/ip/fire").is_none());
    assert!(partial_name_token(":i").is_none());
    assert!(partial_name_token("\"a:b").is_none());
}

// ── Truncation is relevance-ordered ──────────────────────────────────────

#[test]
fn test_truncation_keeps_most_relevant_200() {
    // 210 optional properties: every item is relevant, so the cap must
    // keep the first 200 IN RANK ORDER (all tier 1, alphabetical).
    let mut toml = String::from("[[menus]]\npath = \"/big\"\ntype = \"Directory\"\n");
    for i in 0..210 {
        toml.push_str(&format!(
            "[[menus.arguments]]\nname = \"prop{i:03}\"\ntype = \"string\"\n"
        ));
    }
    let data = MenuData::from_toml_str(&toml);
    let items = compute_completions(&data, "/big add ");
    assert_eq!(items.len(), crate::caps::MAX_COMPLETION_ITEMS);
    assert_eq!(items.first().unwrap().label, "prop000");
    assert_eq!(items.last().unwrap().label, "prop199");
}
