// ── Rename provider (textDocument/rename) ─────────────────────────
//
// Rename the RouterOS script variable under the cursor: the declaration
// chosen by `navigation::choose_definition`'s deterministic rule plus every
// `$usage` sharing its name, each replaced by the requested new name.
//
// SCOPE SEMANTICS (v1, deliberately narrow and honest):
// - Only variables tracked by the navigation index participate:
//   `:local` / `:global` declarations (including brace- or `;`-separated
//   leading forms via the shared `declared_variable` primitive) and bare
//   `$name` usages — now INCLUDING `$name` inside `"…"` double-quoted
//   strings (RouterOS interpolates them), while `'…'` stays literal with
//   the same `$$`/comment rules as go-to-definition. Interface names, menu
//   paths, properties, and verbs are NOT rename targets — renaming those
//   would rewrite device semantics the server cannot verify.
//   NOTE (scope expansion): because rename reuses the navigation index,
//   double-quoted usages are now renamed too; a future `${name}` /
//   `$"my var"` expansion will widen this set again — audit callers then.
// - No block-scope precision (same documented limitation as navigation): all
//   same-name declarations and usages in the document are renamed together.
// - Document-local only (navigation has no cross-file resolution), so the
//   returned `WorkspaceEdit` carries a single-document `changes` map keyed
//   by the requesting URI. Multi-document rename is future work once the
//   index spans open documents.
// - Identifier spans exclude the `$` sigil (usages) and any inline `=value`
//   (declarations), so `:local x=1` renames to `:local new=1` and `$x` to
//   `$new` — the sigil and value survive untouched.
//
// Contracts: malformed positions resolve to `Null` (same empty-result shape
// as definition); an invalid `newName` is also `Null` (nothing honest to
// apply). The handler maps missing params to `-32602`; this module never
// panics.

use crate::diagnostics;
use crate::encoding::PositionEncoding;
use crate::navigation;

/// Bytes permitted in a rename target (v1 bare identifiers).
///
/// Mirrors the navigation index rule (letters, digits, underscore; `-`
/// excluded so arithmetic like `($count-1)` can never donate a name).
fn is_rename_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `name` is a usable rename replacement.
///
/// Trims surrounding whitespace, tolerates one defensive leading `$` (some
/// clients synthesize the sigil into the new name), then requires a
/// non-empty bare identifier. Returns the cleaned name on success.
fn cleaned_new_name(name: &str) -> Option<&str> {
    let trimmed = name.trim();
    let bare = trimmed.strip_prefix('$').unwrap_or(trimmed);
    if bare.is_empty() || !bare.bytes().all(is_rename_ident_char) {
        return None;
    }
    Some(bare)
}

/// Compute the `textDocument/rename` RESULT for an already-validated
/// request: a single-document `WorkspaceEdit` (`{"changes": {uri: [...]}}`
/// with one `{range, newText}` per occurrence in document order), or
/// `Null` when no variable sits under the cursor or `new_name` is unusable.
pub(crate) fn rename_result(
    doc: &str,
    enc: PositionEncoding,
    uri: &str,
    line: usize,
    character: usize,
    new_name: &str,
) -> serde_json::Value {
    let Some(replacement) = cleaned_new_name(new_name) else {
        return serde_json::Value::Null;
    };
    // ONE continuation-aware join per request, feeding both the index and
    // the cursor resolution — the same pipeline definition/references use,
    // so rename can never disagree with navigation about what is where.
    let logicals = diagnostics::logical_lines(doc);
    let index = navigation::build_variable_index(&logicals);
    let Some(occ) =
        crate::server::resolve_cursor_occurrence(doc, &logicals, &index, enc, line, character)
    else {
        return serde_json::Value::Null;
    };
    // All same-name occurrences (every declaration plus every usage):
    // without block-scope analysis, renaming a subset would leave the
    // document referring to two different bindings under one spelling.
    let hits: Vec<&navigation::VariableHit> = index.iter().filter(|h| h.name == occ.name).collect();
    if hits.is_empty() {
        return serde_json::Value::Null;
    }
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
