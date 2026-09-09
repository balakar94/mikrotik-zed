// White-box: folding (folding).
use crate::folding::*;

fn tuples(ranges: &[FoldingRange]) -> Vec<(u32, u32, Option<&'static str>)> {
    ranges
        .iter()
        .map(|r| (r.start_line, r.end_line, r.kind))
        .collect()
}

#[test]
fn test_multiline_brace_block_is_region() {
    let doc = "/ip/firewall/filter add chain=input do={\n\tprint\n}\n";
    // Brace opens on line 0 and closes on line 2 → one region.
    assert_eq!(
        tuples(&compute_folding_ranges(doc)),
        vec![(0, 2, Some("region"))]
    );
}

#[test]
fn test_single_line_braces_are_not_emitted() {
    let doc = ":if (x) do={ print } else={ put }\n";
    assert!(compute_folding_ranges(doc).is_empty());
}

#[test]
fn test_continuation_fold_has_no_kind() {
    let doc = concat!(
        "/tool fetch url=\"https://example.com/very/long\\\n", // continues
        "/path/continues/here\" \\\n",                         // ALSO continues (trailing \)
        "address=1.2.3.4\n",                                   // final physical line
        "/print done\n",
    );
    // The joined logical command spans physical 0..=2 → one kindless fold.
    assert_eq!(tuples(&compute_folding_ranges(doc)), vec![(0, 2, None)]);
}

#[test]
fn test_single_trailing_backslash_continuation_folds_one_line() {
    let doc = "/ip/address add \\\naddress=1.2.3.4\n/print done\n";
    assert_eq!(tuples(&compute_folding_ranges(doc)), vec![(0, 1, None)]);
}

#[test]
fn test_crlf_variant_matches_lf() {
    let braces = "/ip/firewall/filter add do={\n\tprint\n}\n";
    let crlf_doc = braces.replace('\n', "\r\n");
    assert_eq!(
        tuples(&compute_folding_ranges(&crlf_doc)),
        vec![(0, 2, Some("region"))]
    );

    let cont = "/ip/address add \\\naddress=1.2.3.4\n";
    let crlf_cont = cont.replace('\n', "\r\n");
    assert_eq!(
        tuples(&compute_folding_ranges(&crlf_cont)),
        vec![(0, 1, None)]
    );
}

#[test]
fn test_unterminated_brace_at_eof_is_safe() {
    // Open brace, no close anywhere: empty result, no crash/hang.
    let doc = ":foreach i in=[find] do={\n\t:put $i\n".repeat(1000);
    assert!(compute_folding_ranges(&doc).is_empty());

    // Deeply unbalanced opens stay bounded and safe too.
    let doc = "{".repeat(10_000);
    assert!(compute_folding_ranges(&doc).is_empty());
}

#[test]
fn test_nested_independent_regions_both_emitted() {
    let doc = concat!(
        ":do {\n",          // 0 opens outer
        "\t:if (1) do={\n", // 1 opens inner
        "\t\t:put a\n",     // 2
        "\t}\n",            // 3 closes inner → (1,3)
        "}\n",              // 4 closes outer → (0,4)
    );
    assert_eq!(
        tuples(&compute_folding_ranges(doc)),
        vec![(0, 4, Some("region")), (1, 3, Some("region"))]
    );
}

#[test]
fn test_braces_inside_strings_and_comments_are_ignored() {
    let doc = concat!(
        ":put \"}{\"\n",       // braces inside double quotes — none
        ":put '}'\n",          // brace inside single quotes — none
        "# comment { brace\n", // comment — none
        ":put x\n",
    );
    assert!(compute_folding_ranges(doc).is_empty());
}

#[test]
fn test_string_split_across_continuation_keeps_quote_state() {
    // The quoted string contains '{' on the second physical line after a
    // continuation backslash; carrying quote state prevents a phantom
    // brace region. The multi-line logical line itself still folds as a
    // continuation (kindless).
    let doc = ":put \"abc{\\\ndef}ghi\"\n";
    assert_eq!(
        tuples(&compute_folding_ranges(doc)),
        vec![(0, 1, None)],
        "quote-split braces must not create a region"
    );
}

#[test]
fn test_merged_output_sorted_and_deduplicated() {
    // A block that is BOTH a brace region (0..2) and a continuation? Not
    // constructible simultaneously, so exercise sorting via two blocks
    // plus a later continuation.
    let doc = concat!(
        ":do {\n",                 // 0
        "\t:put \"a\\\nb\"\n",     // 1..2 continuation inside block
        "}\n",                     // 3
        "/ip/address add \\\nx\n", // 4..5 continuation
    );
    assert_eq!(
        tuples(&compute_folding_ranges(doc)),
        vec![(0, 3, Some("region")), (1, 2, None), (4, 5, None),]
    );
}

#[test]
fn test_empty_document_yields_empty_list() {
    assert!(compute_folding_ranges("").is_empty());
    assert!(compute_folding_ranges("\n\n").is_empty());
}

#[test]
fn test_unmatched_close_brace_is_ignored() {
    let doc = "}\n:put x\n}\n";
    assert!(compute_folding_ranges(doc).is_empty());
}

#[test]
fn test_wire_shape_omits_absent_kind() {
    let doc = "/ip/address add \\\nx\n";
    let v = serde_json::to_value(compute_folding_ranges(doc)).unwrap();
    assert_eq!(v[0]["startLine"], 0);
    assert_eq!(v[0]["endLine"], 1);
    assert!(v[0].get("kind").is_none());

    let v = serde_json::to_value(compute_folding_regions_fixture()).unwrap();
    assert_eq!(v[0]["kind"], "region");
}

// ── Folding caps: cap, order, unterminated safety ────────────────────────

#[test]
fn test_cap_preserves_first_ranges_in_sorted_order() {
    // MAX_FOLDING_RANGES + 50 independent blocks: the FIRST ranges in
    // sorted (document) order survive truncation.
    let mut doc = String::new();
    for _ in 0..(MAX_FOLDING_RANGES + 50) {
        doc.push_str(":do {\n:put x\n}\n");
    }
    let ranges = compute_folding_ranges(&doc);
    assert_eq!(ranges.len(), MAX_FOLDING_RANGES, "cap enforced");
    // First-preserved: the surviving starts are the earliest blocks.
    assert_eq!(ranges[0].start_line, 0);
    for w in ranges.windows(2) {
        assert!(
            w[0].start_line <= w[1].start_line,
            "sorted by startLine: {:?} before {:?}",
            (w[0].start_line, w[0].end_line),
            (w[1].start_line, w[1].end_line)
        );
    }
    let last_start = ranges.last().map(|r| r.start_line).unwrap_or(0);
    assert!(
        last_start < ((MAX_FOLDING_RANGES + 50) * 3) as u32,
        "tail beyond the cap is dropped, last start {last_start}"
    );
}

#[test]
fn test_output_sorted_by_start_line_with_outer_first_ties() {
    // Inner block opens after the outer one; both interleave with a
    // later continuation. Sorted output puts the outer (longer) range
    // first on ties and everything in startLine order.
    let doc = concat!(
        ":do {\n",              // 0 opens outer
        "\t:if (1) do={\n",     // 1 opens inner
        "\t\t:put a\n",         // 2
        "\t}\n",                // 3 closes inner → (1,3)
        "}\n",                  // 4 closes outer → (0,4)
        "/ip/address add \\\n", // 5..6 continuation
        "x\n",
    );
    let ranges = compute_folding_ranges(doc);
    let starts: Vec<u32> = ranges.iter().map(|r| r.start_line).collect();
    let mut sorted = starts.clone();
    sorted.sort_unstable();
    assert_eq!(starts, sorted, "ranges sorted by startLine");
    assert_eq!(
        tuples(&ranges),
        vec![(0, 4, Some("region")), (1, 3, Some("region")), (5, 6, None),]
    );
}

#[test]
fn test_thousands_of_unterminated_braces_never_hang() {
    // 20k opens with no close: linear scan, empty result, returns.
    let doc = ":do {\n".repeat(20_000);
    let ranges = compute_folding_ranges(&doc);
    assert!(ranges.is_empty(), "unterminated braces emit nothing");
}

/// Helper producing a brace region for serialization-shape assertions.
fn compute_folding_regions_fixture() -> Vec<FoldingRange> {
    compute_folding_ranges(":do {\n:put x\n}\n")
}
