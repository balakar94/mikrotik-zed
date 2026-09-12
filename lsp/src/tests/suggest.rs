// Suggestion engine.
use crate::menus::MenuData;
use crate::suggest::*;
use crate::suggest::{MAX_SUGGEST_INPUT_BYTES, MAX_SUGGESTIONS_PER_PUBLISH, SuggestBudget};
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
[[menus.arguments]]
name = "mode"
type = "enum (on | off)"
"#,
    ))
}

#[test]
fn test_dl_empty_strings() {
    assert_eq!(damerau_levenshtein("", ""), 0);
    assert_eq!(damerau_levenshtein("", "abc"), 3);
    assert_eq!(damerau_levenshtein("abc", ""), 3);
}

#[test]
fn test_dl_equal_strings_zero() {
    assert_eq!(damerau_levenshtein("address", "address"), 0);
    assert_eq!(damerau_levenshtein("/ip/address", "/ip/address"), 0);
}

#[test]
fn test_dl_classic_kitten_sitten_is_one() {
    assert_eq!(damerau_levenshtein("kitten", "sitten"), 1);
    // The textbook kitten→sitting chain costs three edits.
    assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
}

#[test]
fn test_dl_transposition_costs_one_under_osa() {
    assert_eq!(damerau_levenshtein("ab", "ba"), 1);
    assert_eq!(damerau_levenshtein("adress", "address"), 1); // insert 'd'
    // Documented OSA restriction: editing "ca" into "abc" needs 3
    // operations because the substring "a" would have to participate
    // twice (unrestricted Damerau-Levenshtein would say 2).
    assert_eq!(damerau_levenshtein("ca", "abc"), 3);
}

#[test]
fn test_dl_completely_different_hits_cap() {
    // Four substitutions — far beyond any accepted threshold.
    assert_eq!(damerau_levenshtein("aaaa", "bbbb"), 4);
    assert_eq!(damerau_levenshtein("chain", "gateway"), 7);
}

#[test]
fn test_dl_multibyte_counts_characters_not_bytes() {
    // Each '🚨' is 4 bytes but 1 char: one substitution total.
    assert_eq!(damerau_levenshtein("🚨", "x"), 1);
    assert_eq!(damerau_levenshtein("çç", "cc"), 2);
}

// ── suggestion_threshold ─────────────────────────────────────────────────

#[test]
fn test_threshold_short_inputs_allow_one_edit_only() {
    assert_eq!(suggestion_threshold(0), 1);
    assert_eq!(suggestion_threshold(1), 1);
    assert_eq!(suggestion_threshold(4), 1);
}

#[test]
fn test_threshold_longer_inputs_allow_two_edits() {
    assert_eq!(suggestion_threshold(5), 2);
    assert_eq!(suggestion_threshold(12), 2);
}

// ── best_candidate ───────────────────────────────────────────────────────

#[test]
fn test_best_candidate_empty_input_returns_none() {
    let cands = ["address", "interface"];
    assert_eq!(best_candidate("", cands.into_iter()), None);
    assert_eq!(best_candidate("   ", cands.into_iter()), None);
}

#[test]
fn test_best_candidate_no_candidates_returns_none() {
    let empty: [String; 0] = [];
    assert_eq!(best_candidate("adress", empty.into_iter()), None);
}

#[test]
fn test_best_candidate_four_letter_typo_suggests_within_threshold_one() {
    // 4-char input → threshold 1: the adjacent transposition qualifies.
    let picked = best_candidate("nmae", ["name", "comment"].into_iter());
    assert_eq!(picked.as_deref(), Some("name"));
}

#[test]
fn test_best_candidate_long_garbage_beyond_threshold_returns_none() {
    // 12 chars of nonsense stays outside threshold 2 of everything.
    let picked = best_candidate(
        "zzzqqqxxxwww",
        ["address", "interface", "gateway", "chain"].into_iter(),
    );
    assert_eq!(picked, None);
}

#[test]
fn test_best_candidate_prefers_smallest_distance() {
    // "adress" is 1 away from "address" and 2+ from the others.
    let picked = best_candidate("adress", ["action", "interface", "address"].into_iter());
    assert_eq!(picked.as_deref(), Some("address"));
}

#[test]
fn test_best_candidate_tie_breaks_lexicographically_regardless_of_order() {
    // Both candidates sit at distance 1 ("aab": substitute last char;
    // "aac": same) — the smaller name must win in EITHER order.
    let a = best_candidate("aaa", ["aac", "aab"].into_iter());
    let b = best_candidate("aaa", ["aab", "aac"].into_iter());
    assert_eq!(a.as_deref(), Some("aab"));
    assert_eq!(b.as_deref(), Some("aab"));
}

#[test]
fn test_best_candidate_rejects_identity_match() {
    // Distance 0 means the diagnostic is stale; no phantom fix.
    let picked = best_candidate("address", ["address", "comment"].into_iter());
    assert_eq!(picked, None);
}

#[test]
fn test_best_candidate_menu_path_suggestion() {
    let paths = ["/ip/address", "/ip/route", "/system/clock"];
    // "/ip/addres" is one insertion away from "/ip/address".
    let picked = best_candidate("/ip/addres", paths.into_iter());
    assert_eq!(picked.as_deref(), Some("/ip/address"));
}

#[test]
fn budget_allows_first_suggestions_then_cuts_off() {
    let mut budget = SuggestBudget::new();
    // First evaluations behave exactly like best_candidate.
    assert_eq!(
        budget.candidate("adress", ["address"].iter()),
        Some("address".to_string())
    );
    // Exhaust the budget: further calls return None without scanning.
    for _ in 0..MAX_SUGGESTIONS_PER_PUBLISH + 5 {
        let _ = budget.candidate("adress", ["address"].iter());
    }
    assert_eq!(budget.candidate("adress", ["address"].iter()), None);
}

// ── input-length cap (DoS guard) ─────────────────────────────────────────

#[test]
fn budget_rejects_over_long_input_without_spending_budget() {
    // Over-long inputs must be rejected before the budget is debited:
    // calling far past the per-publish allowance with garbage cannot use
    // it up, so the next legitimate typo still gets its suggestion.
    let mut budget = SuggestBudget::new();
    let over_long = "a".repeat(MAX_SUGGEST_INPUT_BYTES + 1);
    for _ in 0..(MAX_SUGGESTIONS_PER_PUBLISH + 5) {
        assert_eq!(budget.candidate(&over_long, ["address"].iter()), None);
    }
    assert_eq!(
        budget.candidate("adress", ["address"].iter()),
        Some("address".to_string()),
        "rejected over-long calls must not consume the publish budget"
    );
}

#[test]
fn best_candidate_rejects_very_long_input_quickly() {
    // Defense in depth: direct callers are protected too, so the
    // O(n × m) scan never runs for absurd input.
    let long = "a".repeat(100_000);
    let started = std::time::Instant::now();
    let picked = best_candidate(&long, ["address", "interface"].into_iter());
    let elapsed = started.elapsed();
    assert_eq!(picked, None);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "over-long input must short-circuit, took {elapsed:?}"
    );
}

#[test]
fn input_exactly_at_cap_is_still_evaluated() {
    // The guard is `>`, so exactly MAX bytes proceeds to normal
    // evaluation; `best_candidate`'s trim recovers the identifier.
    let padded = format!(
        "{}{}",
        " ".repeat(MAX_SUGGEST_INPUT_BYTES - "adress".len()),
        "adress"
    );
    assert_eq!(padded.len(), MAX_SUGGEST_INPUT_BYTES);
    assert_eq!(
        best_candidate(&padded, ["address"].into_iter()),
        Some("address".to_string())
    );

    let mut budget = SuggestBudget::new();
    assert_eq!(
        budget.candidate(&padded, ["address"].iter()),
        Some("address".to_string())
    );
}

#[test]
fn short_input_behavior_unchanged_by_input_cap() {
    assert_eq!(
        best_candidate("adress", ["address", "interface"].into_iter()),
        Some("address".to_string())
    );
}

#[test]
fn large_unknown_doc_stays_bounded_and_omits_suffix_only() {
    let data = validator_data();
    // 500 unknown-menu lines × unknown property: every diagnostic must
    // still publish with code+severity; only the "Did you mean" suffix
    // may be omitted after the budget is spent.
    let doc = "/nope/unknown add badprop=1\n".repeat(500);
    let diags = crate::diagnostics::compute_diagnostics(&data, &doc, "file:///budget.rsc");
    assert!(!diags.is_empty());
    assert!(
        diags
            .iter()
            .all(|d| d.code.is_some() && d.severity.is_some())
    );
    assert!(diags.iter().all(|d| d.source.as_deref() == Some("rsc-ls")));
}
