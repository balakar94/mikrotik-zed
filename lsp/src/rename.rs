// ── Rename provider ──────────────────────────────────────────────────────
//
// Rename the declaration and references of a resolved local variable.
// Decline a rename when the index cannot account for every occurrence.

use crate::diagnostics;
use crate::encoding::PositionEncoding;
use crate::navigation;
use crate::server_proto::resolve_cursor_occurrence;

/// Whether `name` is a usable rename replacement.
///
/// Trims surrounding whitespace, tolerates one defensive leading `$` (some
/// clients synthesize the sigil into the new name), then requires a
/// non-empty bare identifier. Returns the cleaned name on success.
fn cleaned_new_name(name: &str) -> Option<&str> {
    let trimmed = name.trim();
    let bare = trimmed.strip_prefix('$').unwrap_or(trimmed);
    if !bare
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        || !bare.bytes().all(navigation::is_ident_char)
    {
        return None;
    }
    Some(bare)
}

/// Compute the `textDocument/rename` RESULT for an already-validated
/// request: a single-document `WorkspaceEdit` (`{"changes": {uri: [...]}}`
/// with one `{range, newText}` per occurrence in document order), or
/// `Null` when no variable sits under the cursor or `new_name` is unusable.
///
/// Test/legacy entry point that builds its own logical-line join; the
/// server calls [`rename_result_with_logicals`] with the shared parse.
#[allow(dead_code)]
pub(crate) fn rename_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    new_name: &str,
) -> serde_json::Value {
    let logicals = diagnostics::logical_lines(doc);
    rename_result_with_logicals(doc, enc, uri, line, character, new_name, &logicals)
}

/// [`rename_result`] over an already-joined logical-line slice.
///
/// The server passes the shared [`crate::parser::ParseCache`] join so a
/// rename request does not reparse the document; the test/legacy wrapper
/// above builds its own.
pub(crate) fn rename_result_with_logicals(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    new_name: &str,
    logicals: &[diagnostics::LogicalLine],
) -> serde_json::Value {
    let Some(replacement) = cleaned_new_name(new_name) else {
        return serde_json::Value::Null;
    };
    // The caller's continuation-aware join feeds both the index and the
    // cursor resolution — the same pipeline definition/references use, so
    // rename can never disagree with navigation about what is where.
    let index = navigation::build_variable_index(logicals);
    let Some(occ) = resolve_cursor_occurrence(doc, logicals, &index, enc, line, character) else {
        return serde_json::Value::Null;
    };
    let Some(hits) = index.rename_hits(occ, Some(replacement)) else {
        return serde_json::Value::Null;
    };
    let lines: Vec<&str> = doc.lines().collect();
    let edits: Vec<serde_json::Value> = hits
        .iter()
        .map(|hit| {
            let mut range = logicals[hit.logical_line].map_range(hit.start, hit.end);
            crate::convert_position(&mut range.start, &lines, enc);
            crate::convert_position(&mut range.end, &lines, enc);
            serde_json::json!({ "range": range, "newText": replacement })
        })
        .collect();
    let mut changes = serde_json::Map::new();
    changes.insert(uri.to_string(), serde_json::Value::Array(edits));
    serde_json::json!({ "changes": changes })
}

/// Report the exact editable span before the client asks for a replacement.
pub(crate) fn prepare_rename(
    doc: &str,
    enc: PositionEncoding,
    line: usize,
    character: usize,
    logicals: &[diagnostics::LogicalLine],
) -> Option<serde_json::Value> {
    let index = navigation::build_variable_index(logicals);
    let hit = resolve_cursor_occurrence(doc, logicals, &index, enc, line, character)?;
    index.rename_hits(hit, None)?;
    let lines: Vec<&str> = doc.lines().collect();
    let mut range = logicals[hit.logical_line].map_range(hit.start, hit.end);
    crate::convert_position(&mut range.start, &lines, enc);
    crate::convert_position(&mut range.end, &lines, enc);
    Some(serde_json::json!({ "range": range, "placeholder": hit.name }))
}
