// ── Rename provider (textDocument/rename) ─────────────────────────
//
// Rename the RouterOS script variable under the cursor: the declaration
// chosen by `navigation::choose_definition`'s deterministic rule plus every
// `$usage` sharing its name, each replaced by the requested new name.
//
// SCOPE SEMANTICS (v1, deliberately narrow and honest):
// - Only variables tracked by the navigation index participate:
//   `:local` / `:global` declarations and bare `$name` usages (same
//   quote/`$$`/comment rules as go-to-definition). Interface names, menu
//   paths, properties, and verbs are NOT rename targets — renaming those
//   would rewrite device semantics the server cannot verify.
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

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "file:///rename.rsc";

    fn rename(doc: &str, line: usize, character: usize, new_name: &str) -> serde_json::Value {
        rename_result(doc, PositionEncoding::Utf8, URI, line, character, new_name)
    }

    #[test]
    fn test_rename_variable_covers_declaration_and_usages() {
        let doc = ":local wan \"e\"\n:put $wan\n/ip/address add interface=$wan\n";
        // Cursor on the declaration identifier `wan` (line 0, byte 8).
        let result = rename(doc, 0, 8, "uplink");
        let edits = result["changes"][URI].as_array().expect("edits array");
        assert_eq!(edits.len(), 3, "declaration + two usages, got {result}");
        for edit in edits {
            assert_eq!(edit["newText"], "uplink");
        }
        // Declaration edit covers exactly `wan` (bytes 7..10 of line 0),
        // so `:local ` and the value survive.
        assert_eq!(edits[0]["range"]["start"]["line"], 0);
        assert_eq!(edits[0]["range"]["start"]["character"], 7);
        assert_eq!(edits[0]["range"]["end"]["character"], 10);
        // Usage edits exclude the `$` sigil: `:put $wan` keeps its `$`.
        assert_eq!(edits[1]["range"]["start"]["line"], 1);
        assert_eq!(edits[1]["range"]["start"]["character"], 6);
        assert_eq!(edits[1]["range"]["end"]["character"], 9);
    }

    #[test]
    fn test_rename_from_usage_renames_same_set() {
        let doc = ":local x 1\n:put $x\n";
        // Cursor on the usage (line 1, inside `x`).
        let result = rename(doc, 1, 6, "y");
        let edits = result["changes"][URI].as_array().expect("edits array");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["range"]["start"]["line"], 0);
        assert_eq!(edits[1]["range"]["start"]["line"], 1);
    }

    #[test]
    fn test_rename_inline_value_keeps_value_outside_edit() {
        // `:local x=1` declares only `x`; the `=1` must survive the rename.
        let doc = ":local x=1\n:put $x\n";
        let result = rename(doc, 0, 7, "count");
        let edits = result["changes"][URI].as_array().expect("edits array");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["newText"], "count");
        assert_eq!(edits[0]["range"]["end"]["character"], 8);
    }

    #[test]
    fn test_rename_with_no_identifier_returns_null() {
        let doc = "/ip/address add address=1.2.3.4\n";
        // Cursor on a property value: no variable occurrence there.
        assert_eq!(rename(doc, 0, 25, "other"), serde_json::Value::Null);
        // Cursor on whitespace-only text.
        assert_eq!(rename("   \n", 0, 1, "other"), serde_json::Value::Null);
        // Same-spelling property of an existing variable never resolves
        // (mirrors the navigation overlap rule).
        let doc = ":local ip 1\n/ip/address add address=1.2.3.4\n";
        assert_eq!(rename(doc, 1, 20, "other"), serde_json::Value::Null);
    }

    #[test]
    fn test_rename_with_unusable_new_name_returns_null() {
        let doc = ":local x\n:put $x\n";
        for bad in ["", "   ", "has space", "with-dash", "semi;colon", "$"] {
            assert_eq!(
                rename(doc, 0, 7, bad),
                serde_json::Value::Null,
                "new name {bad:?} must yield null, never an edit"
            );
        }
    }

    #[test]
    fn test_rename_tolerates_leading_sigil_in_new_name() {
        let doc = ":local x\n:put $x\n";
        let result = rename(doc, 0, 7, "$y");
        let edits = result["changes"][URI].as_array().expect("edits array");
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0]["newText"], "y");
    }

    #[test]
    fn test_rename_result_is_single_document_changes_map() {
        let doc = ":local a\n:put $a\n";
        let result = rename(doc, 0, 7, "b");
        let changes = result["changes"].as_object().expect("changes map");
        assert_eq!(changes.len(), 1, "v1 rename stays within one document");
        assert!(changes.contains_key(URI));
        assert!(result.get("documentChanges").is_none());
    }

    #[test]
    fn test_rename_usage_without_declaration_still_renames_occurrences() {
        // No declaration exists, but the cursor resolves to a real usage
        // occurrence, so the deterministic all-same-name edit set applies.
        let doc = ":put $lonely\n";
        let result = rename(doc, 0, 7, "found");
        let edits = result["changes"][URI].as_array().expect("edits array");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["newText"], "found");
    }
}
