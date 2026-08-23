// ── Folding ranges (Stage B) ──────────────────────────────────
//
// textDocument/foldingRange support. Two independent sources, both emitted
// only when they span more than one physical line (`startLine < endLine`),
// merged and sorted by `startLine`:
//
// 1. Brace regions — `{` … `}` pairs on DIFFERENT physical lines are folded
//    with kind "region". Counting is quote-aware: brace characters inside
//    `"…"` / `'…'` strings and inside `#` comments never open or close a
//    region, and quote state carries across physical lines so a string split
//    by a `\` continuation cannot desynchronize the counter. Unterminated
//    braces at EOF simply never produce a region — no crash, no hang.
//
// 2. Multi-line continuations — a logical line joined from several physical
//    lines (trailing `\`) folds into its first line; this collapses the
//    common "split URL across lines" pattern. These ranges carry no kind.
//
// Line numbers are physical document lines, which are encoding-independent —
// unlike symbol/diagnostic positions, folding ranges need NO position-encoding
// conversion at the protocol boundary.

use crate::diagnostics;

/// Defensive cap on emitted folding ranges. Documents are capped at 5 MiB;
/// this bounds the response payload for pathologically nested input.
const MAX_FOLDING_RANGES: usize = 5000;

/// Maximum tracked open-brace depth. Beyond this, further `{` are ignored:
/// memory stays bounded for adversarial input like "{{{{{…", and only
/// regions beyond 4096 nesting levels (not expressible in real scripts)
/// are lost.
const MAX_BRACE_DEPTH: usize = 4096;

/// One folding range in wire shape. `kind` is omitted when absent
/// (LSP FoldingRange.kind is optional).
#[derive(Debug, serde::Serialize)]
pub(crate) struct FoldingRange {
    #[serde(rename = "startLine")]
    pub start_line: u32,
    #[serde(rename = "endLine")]
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
}

/// Compute all folding ranges for a script document.
///
/// Pure function over the document text; deterministic output sorted by
/// `startLine`. An empty document yields an empty list.
pub(crate) fn compute_folding_ranges(doc: &str) -> Vec<FoldingRange> {
    let mut out = brace_regions(doc);
    out.extend(continuation_ranges(doc));

    // Deterministic order: by start line; ties broken by end line so the
    // outer (longer) region of two same-start ranges sorts first, then kind
    // for full deduplication of brace/continuation overlaps.
    out.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then(b.end_line.cmp(&a.end_line))
            .then(a.kind.cmp(&b.kind))
    });
    out.dedup_by(|a, b| {
        a.start_line == b.start_line && a.end_line == b.end_line && a.kind == b.kind
    });
    if out.len() > MAX_FOLDING_RANGES {
        out.truncate(MAX_FOLDING_RANGES);
    }
    out
}

/// Brace regions: scan every physical line with quote/comment state carried
/// across lines; match `{`/`}` pairs and keep those spanning multiple lines.
fn brace_regions(doc: &str) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    // Stack of physical line indices where unclosed `{` sit.
    let mut opens: Vec<u32> = Vec::new();

    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    let mut in_comment = false;

    for (idx, line) in doc.lines().enumerate() {
        let line_no = idx as u32;
        for c in line.chars() {
            if in_comment {
                continue; // comments end at end-of-line (reset below)
            }
            if escaped {
                // Escaped byte inside quotes: never structural.
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_double || in_single => escaped = true,
                '"' if !in_single => in_double = !in_double,
                '\'' if !in_double => in_single = !in_single,
                '#' if !in_double && !in_single => in_comment = true,
                '{' if !in_double && !in_single => {
                    // Depth cap: ignore deeper opens; bounded memory, and
                    // scripts never legitimately nest this deep.
                    if opens.len() < MAX_BRACE_DEPTH {
                        opens.push(line_no);
                    }
                }
                '}' if !in_double && !in_single => {
                    if let Some(start) = opens.pop() {
                        // Only multi-line regions fold; single-line `{ }`
                        // would produce a zero-height range clients render
                        // as noise.
                        if start < line_no {
                            out.push(FoldingRange {
                                start_line: start,
                                end_line: line_no,
                                kind: Some("region"),
                            });
                        }
                    }
                    // Unmatched close: ignored (no panic, no state damage).
                }
                _ => {}
            }
        }
        // Physical line boundary resets per-line states. Quote state does
        // NOT reset: a `\`-continuation can legally split a quoted string.
        in_comment = false;
        escaped = false;
    }

    // Braces still open at EOF emit nothing — unterminated blocks must not
    // fabricate ranges (and cannot crash or hang: the loop is linear).
    out
}

/// Continuation folds: logical lines spanning more than one physical line.
fn continuation_ranges(doc: &str) -> Vec<FoldingRange> {
    diagnostics::logical_lines(doc)
        .iter()
        .filter_map(|ll| {
            let first = ll.first_physical_line() as u32;
            let last = ll.last_physical_line() as u32;
            (first < last).then_some(FoldingRange {
                start_line: first,
                end_line: last,
                kind: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let v = serde_json::to_value(&compute_folding_ranges(doc)).unwrap();
        assert_eq!(v[0]["startLine"], 0);
        assert_eq!(v[0]["endLine"], 1);
        assert!(v[0].get("kind").is_none());

        let v = serde_json::to_value(&compute_folding_regions_fixture()).unwrap();
        assert_eq!(v[0]["kind"], "region");
    }

    /// Helper producing a brace region for serialization-shape assertions.
    fn compute_folding_regions_fixture() -> Vec<FoldingRange> {
        compute_folding_ranges(":do {\n:put x\n}\n")
    }
}
