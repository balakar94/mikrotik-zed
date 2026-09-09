// Statement-start snippet gating.
// Copied (not moved) from `lsp/src/completion.rs` (`mod extra_coverage` L1694-1747, L2297-2430);
// the original block is
// left untouched. `use super::*` is adapted to `use crate::completion::*;` for the new location.
use crate::completion::*;
use crate::menus::MenuData;

fn synthetic() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
[[menus.arguments]]
name = "interface"
type = "iface_enum"
[[menus.arguments]]
name = "comment"
type = "string"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus]]
path = "/ip/route"
type = "Directory"
[[menus.arguments]]
name = "gateway"
type = "ipAddr"
[[menus]]
path = "/ip/route/check"
type = "Command"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "src-address"
type = "ipAddr"
[[menus]]
path = "/system/clock"
type = "Directory"
[[menus.arguments]]
name = "enabled"
type = "bool"
[[menus.arguments]]
name = "time-zone-name"
type = "string"
"#,
    )
}
// ── Statement-start snippets (B3) ────────────────────────────────────────

const SNIPPET_LABELS: [&str; 4] = [":if", ":foreach", ":for", ":do"];

fn snippet_items(items: &[CompletionItem]) -> Vec<&CompletionItem> {
    items
        .iter()
        .filter(|i| SNIPPET_LABELS.contains(&i.label.as_str()))
        .collect()
}

#[test]
fn test_at_statement_start_gating() {
    // Statement starts…
    assert!(at_statement_start(""), "nothing typed yet");
    assert!(at_statement_start("   "), "whitespace only");
    assert!(at_statement_start("{ "), "right after block opener");
    assert!(
        at_statement_start("{"),
        "block opener without trailing space"
    );
    assert!(at_statement_start("; "));
    // …and non-starts.
    assert!(!at_statement_start(":if "), "previous token is the verb");
    assert!(!at_statement_start("add address=1.1.1.1 "), "mid-command");
    assert!(
        !at_statement_start("do={ "),
        "a 'do=' plus open-brace token is one token, not a bare block opener"
    );
    assert!(
        !at_statement_start("x=1; "),
        "a property token ending in a separator is one token, not a bare separator"
    );
}

#[test]
fn test_snippets_shape_and_order() {
    let data = synthetic();
    let items = compute_completions(&data, "");
    let snips = snippet_items(&items);
    assert_eq!(snips.len(), 4, "exactly four snippets appended");
    // Offer order matches the constant table.
    let labels: Vec<&str> = snips.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, SNIPPET_LABELS);
    for s in snips {
        assert_eq!(s.insert_text_format, Some(2), "insertTextFormat Snippet");
        assert_eq!(s.kind, Some(kind::SNIPPET));
        assert!(
            s.sort_text.as_deref().unwrap_or("").starts_with('9'),
            "snippets rank below other candidates: {:?}",
            s.sort_text
        );
        // One-line markdown documentation.
        let doc = s.documentation.as_ref().expect("docs required");
        assert_eq!(doc.kind, "markdown");
        assert!(!doc.value.contains('\n'), "documentation stays one line");
        assert!(!s.insert_text.as_ref().unwrap().is_empty());
        assert!(
            s.insert_text.as_ref().unwrap().contains("$0")
                || s.insert_text.as_ref().unwrap().contains("${")
        );
    }
}

#[test]
fn test_snippet_bodies_match_spec() {
    let data = synthetic();
    let items = compute_completions(&data, "");
    let by_label = |l: &str| {
        items
            .iter()
            .find(|i| i.label == l)
            .unwrap_or_else(|| panic!("snippet {l} missing"))
            .insert_text
            .clone()
            .unwrap()
    };
    assert_eq!(
        by_label(":if"),
        ":if (${1:condition}) do={\n\t${2}\n} else={\n\t${3}\n}$0"
    );
    assert_eq!(
        by_label(":foreach"),
        ":foreach ${1:i} in=[${2:find expression}] do={\n\t${3}\n}$0"
    );
    assert_eq!(
        by_label(":for"),
        ":for ${1:i} from=${2:1} to=${3:10} do={\n\t${4}\n}$0"
    );
    assert_eq!(by_label(":do"), ":do {\n\t${1}\n} while=(${2:condition})$0");
}

#[test]
fn test_snippets_absent_mid_command() {
    let data = synthetic();
    // After a verb with properties — the classic mid-command position.
    let items = compute_completions(&data, "/ip/address add ");
    assert!(
        snippet_items(&items).is_empty(),
        "no snippets after a path+verb"
    );
    // Inside a value token.
    let items = compute_completions(&data, "/ip/address add address=");
    assert!(
        snippet_items(&items).is_empty(),
        "no snippets inside values"
    );
}

#[test]
fn test_snippets_absent_after_slash_and_in_path_contexts() {
    let data = synthetic();
    // Typing a path — resolved menu path non-empty → gated off.
    let items = compute_completions(&data, "/ip ");
    assert!(
        snippet_items(&items).is_empty(),
        "no snippets in menu context"
    );
    // Trailing '/' (root navigation) → gated off.
    let items = compute_completions(&data, "/");
    assert!(
        snippet_items(&items).is_empty(),
        "no snippets while typing a path"
    );
}

#[test]
fn test_snippets_present_after_block_opener() {
    let data = synthetic();
    // Statement start inside a script block.
    let items = compute_completions(&data, "{ ");
    assert_eq!(snippet_items(&items).len(), 4);
}
