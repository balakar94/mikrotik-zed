// ── Folding ranges (Stage B) ─────────────────────────────────────────────
//
// textDocument/foldingRange support. Two independent sources, both emitted
// only when they span more than one physical line (`startLine < endLine`),
// merged and sorted by `startLine`:
//
// 1. Brace regions — `{` … `}` pairs on DIFFERENT physical lines are folded
//    with kind "region". Counting is quote-aware (shared scanner in
//    `parser::walk_structure`): brace characters inside `"…"` / `'…'` strings
//    and inside `#` comments never open or close a region, and quote state
//    carries across physical lines so a string split by a `\` continuation
//    cannot desynchronize the counter. Unterminated braces at EOF simply
//    never produce a region — no crash, no hang.
//
// 2. Multi-line continuations — a logical line joined from several physical
//    lines (trailing `\`) folds into its first line; this collapses the
//    common "split URL across lines" pattern. These ranges carry no kind.
//
// Line numbers are physical document lines, which are encoding-independent —
// unlike symbol/diagnostic positions, folding ranges need NO position-encoding
// conversion at the protocol boundary.

use crate::diagnostics;
use crate::{MAX_BRACE_DEPTH, StructureEvent, walk_structure};

/// Defensive cap on emitted folding ranges. Documents are capped at 5 MiB;
/// this bounds the response payload for pathologically nested input. The
/// FIRST ranges in sorted order win; the tail is dropped.
pub(crate) const MAX_FOLDING_RANGES: usize = 5000;

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
///
/// The quote/comment state machine itself lives in
/// [`crate::parser::walk_structure`] — shared with the syntax diagnostics so
/// both features agree on what is structural. This function only owns the
/// stack matching and the multi-line filter.
fn brace_regions(doc: &str) -> Vec<FoldingRange> {
    let mut out = Vec::new();
    // Stack of physical line indices where unclosed `{` sit.
    let mut opens: Vec<u32> = Vec::new();

    walk_structure(doc, |ev| match ev {
        StructureEvent::OpenBrace { line, .. } => {
            // Depth cap (MAX_BRACE_DEPTH, defined beside the shared walker):
            // ignore deeper opens; bounded memory, and scripts never
            // legitimately nest this deep.
            if opens.len() < MAX_BRACE_DEPTH {
                opens.push(line as u32);
            }
        }
        StructureEvent::CloseBrace { line, .. } => {
            if let Some(start) = opens.pop() {
                // Only multi-line regions fold; single-line `{ }`
                // would produce a zero-height range clients render
                // as noise.
                if start < line as u32 {
                    out.push(FoldingRange {
                        start_line: start,
                        end_line: line as u32,
                        kind: Some("region"),
                    });
                }
            }
            // Unmatched close: ignored (no panic, no state damage).
        }
        // Unterminated quotes are diagnostics' concern; folding ignores them.
        StructureEvent::UnterminatedQuote { .. } => {}
    });

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
