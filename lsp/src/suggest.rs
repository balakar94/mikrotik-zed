// ── "Did you mean" suggestion engine ──────────────────────────────
//
// Pure string-distance helpers backing textDocument/codeAction
// quick-fixes for typo'd properties and menu paths. No I/O, no server
// state — fully deterministic by construction:
//
// - Distance is the Optimal String Alignment variant of Damerau-
//   Levenshtein (see [`damerau_levenshtein`] for the exact semantics).
// - Acceptance uses a length-aware threshold so short identifiers do
//   not pick up noisy neighbors.
// - Ties are broken lexicographically, so the chosen candidate never
//   depends on candidate iteration order (menu tables are HashMaps).

/// Upper bound on the token length worth attempting to repair.
///
/// Menu paths are validated at load time to at most 256 bytes and real
/// property names are far shorter; anything longer than this under a
/// diagnostic range is not an identifier-like token (e.g. a stale or
/// fabricated range), so no suggestion attempt is made.
pub(crate) const MAX_SUGGEST_INPUT_BYTES: usize = 256;

/// Edit distance between `a` and `b` under the **Optimal String
/// Alignment** restriction of Damerau-Levenshtein (a.k.a. restricted
/// edit distance): substitutions, insertions, deletions and
/// transpositions of **adjacent** characters each cost 1, but no
/// substring may be edited twice.
///
/// Consequence of the restriction: `dl("ca", "abc")` is 3 here whereas
/// unrestricted Damerau-Levenshtein would report 2 (`ca` → transposition →
/// `ac` → insertion → `abc`). OSA is used because it admits the simple,
/// well-understood DP below; the difference only matters for exotic
/// inputs and never changes which near-miss identifier wins.
///
/// Character-based (not byte-based), so multibyte tokens measure
/// correctly. Runs in O(len_a × len_b) time, O(len_b) memory.
pub(crate) fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }

    // Three rolling rows: i-2 (for the transposition case), i-1 and the
    // current row being filled. Buffers are rotated (not reallocated)
    // so every row write lands in a full-length scratch vector.
    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];

    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let substitution = usize::from(a[i - 1] != b[j - 1]);
            let mut val = prev[j - 1] + substitution;
            val = val.min(prev[j] + 1); // deletion
            val = val.min(cur[j - 1] + 1); // insertion
            // Adjacent transposition: a[..i] ends with b[j-2..j] swapped.
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                val = val.min(prev2[j - 2] + 1);
            }
            cur[j] = val;
        }
        // Rotate buffers: prev2 ← row i-1, prev ← row i, cur becomes the
        // recycled old row i-2 buffer (fully rewritten next iteration).
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Maximum accepted edit distance for an input of `input_len` characters.
///
/// Rationale: identifiers of ≤ 4 characters are so short that two edits
/// already destroy most of the signal — allowing distance 2 there makes
/// almost any short garbage ("x", "=", "ab12") match some property name.
/// From 5 characters up, distance 2 still captures the realistic typo
/// space (transposed pair + missing letter, doubled keystroke + stray
/// character, …) while remaining far too tight for unrelated names to
/// sneak through.
pub(crate) fn suggestion_threshold(input_len: usize) -> usize {
    if input_len <= 4 { 1 } else { 2 }
}

/// Pick the best replacement candidate for a mistyped `input`.
///
/// Selection rules, applied in order:
/// 1. Candidates whose OSA distance exceeds
///    [`suggestion_threshold`] are rejected — `None` when nothing
///    survives, so garbage input yields no action instead of a wild
///    guess.
/// 2. A candidate identical to the input (distance 0) is rejected too:
///    that means the diagnostic is stale (the text is already valid),
///    and a quick-fix replacing text with itself would be a phantom.
/// 3. Among equals, the **lexicographically smallest** candidate wins,
///    making the result independent of candidate iteration order.
///
/// The input is trimmed defensively; empty input returns `None`.
pub(crate) fn best_candidate(
    input: &str,
    candidates: impl Iterator<Item = impl AsRef<str>>,
) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let threshold = suggestion_threshold(input.chars().count());
    // Owned storage: iterator items are transient, so a borrowed best
    // pick could not outlive a single loop iteration.
    let mut best: Option<(usize, String)> = None;
    for candidate in candidates {
        let candidate = candidate.as_ref();
        let dist = damerau_levenshtein(input, candidate);
        if dist == 0 || dist > threshold {
            continue;
        }
        let take_over = match &best {
            None => true,
            Some((best_dist, best_name)) => {
                dist < *best_dist || (dist == *best_dist && candidate < best_name.as_str())
            }
        };
        if take_over {
            best = Some((dist, candidate.to_string()));
        }
    }
    best.map(|(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── damerau_levenshtein ───────────────────────────────────────

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

    // ── suggestion_threshold ──────────────────────────────────────

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

    // ── best_candidate ────────────────────────────────────────────

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
}
