// Completion context gating (root, menu, verb, args).
// Copied (not moved) from `lsp/src/completion.rs` (`mod extra_coverage` L1694-1747, L1749-1940); the original block is
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
// ── Root menus only when at root ─────────────────────────────────

#[test]
fn test_root_only_at_empty_context() {
    let data = synthetic();
    let items = compute_completions(&data, "");
    assert!(!items.is_empty());
    // Menus keep CLASS kind (directories) or FUNCTION (root Commands);
    // snippets (kind SNIPPET) are appended at statement start since B3.
    for it in items.iter().filter(|i| i.label.starts_with('/')) {
        assert!(it.label.starts_with('/'), "root label must start with /");
        assert!(
            it.kind == Some(kind::CLASS) || it.kind == Some(kind::FUNCTION),
            "root kind must be CLASS or FUNCTION, got {:?} for {}",
            it.kind,
            it.label
        );
    }
    // Should not contain verbs or properties at root
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(!labels.contains(&"add"));
    assert!(!labels.contains(&"address"));
    // Non-menu items must be exactly the four statement snippets.
    let mut extra: Vec<&str> = labels.into_iter().filter(|l| !l.starts_with('/')).collect();
    extra.sort_unstable();
    assert_eq!(extra, vec![":do", ":for", ":foreach", ":if"]);
}

#[test]
fn test_root_not_returned_when_path_present() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip ");
    // Should contain sub-menus, not roots
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        !labels.contains(&"/ip"),
        "roots should not appear when path present"
    );
    assert!(labels.contains(&"address") || labels.contains(&"route"));
}

#[test]
fn test_empty_context_vs_whitespace_only() {
    let data = synthetic();
    let empty = compute_completions(&data, "");
    let ws = compute_completions(&data, "   ");
    // Both tokenizations yield empty path -> root completions
    assert_eq!(empty.len(), ws.len());
    assert!(ws.iter().any(|i| i.label == "/ip"));
}

// ── Sub-menus after path ──────────────────────────────────────────

#[test]
fn test_submenus_after_ip_path() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"address"));
    assert!(labels.contains(&"route"));
    assert!(labels.contains(&"firewall"));
    // Sub-menu kind should be CLASS
    for it in items
        .iter()
        .filter(|i| ["address", "route", "firewall"].contains(&i.label.as_str()))
    {
        assert_eq!(it.kind, Some(kind::CLASS));
    }
}

#[test]
fn test_submenus_after_ip_with_trailing_space_vs_without() {
    let data = synthetic();
    let with_space = compute_completions(&data, "/ip ");
    let without = compute_completions(&data, "/ip");
    // Both parse to path "/ip", so completions should be equivalent
    let mut a: Vec<String> = with_space.iter().map(|i| i.label.clone()).collect();
    let mut b: Vec<String> = without.iter().map(|i| i.label.clone()).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn test_submenus_include_only_directory_types() {
    let data = synthetic();
    // /ip/route has a child Command /ip/route/check which should appear via verbs, not sub-menu
    let items = compute_completions(&data, "/ip/route ");
    let sub_labels: Vec<&str> = items
        .iter()
        .filter(|i| i.kind == Some(kind::CLASS))
        .map(|i| i.label.as_str())
        .collect();
    // No CLASS item should be "check" because check is Command; it appears as FUNCTION verb
    assert!(!sub_labels.contains(&"check"));
    assert!(
        items
            .iter()
            .any(|i| i.label == "check" && i.kind == Some(kind::FUNCTION))
    );
}

// ── Verbs after menu+space ────────────────────────────────────────

#[test]
fn test_verbs_after_menu_space() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address ");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    for verb in MenuData::STANDARD_VERBS {
        assert!(labels.contains(verb), "missing verb {verb}");
    }
    // Verbs should be FUNCTION kind
    for it in items
        .iter()
        .filter(|i| MenuData::STANDARD_VERBS.contains(&i.label.as_str()))
    {
        assert_eq!(it.kind, Some(kind::FUNCTION));
        assert!(it.detail.as_ref().unwrap().contains("standard"));
    }
}

#[test]
fn test_verbs_after_menu_without_trailing_space() {
    let data = synthetic();
    let with = compute_completions(&data, "/ip/address ");
    let without = compute_completions(&data, "/ip/address");
    // Both should produce same verb+submenu set (no command yet)
    assert_eq!(with.len(), without.len());
}

#[test]
fn test_verbs_include_action_command() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/route ");
    assert!(items.iter().any(|i| i.label == "check"));
    let check = items.iter().find(|i| i.label == "check").unwrap();
    assert_eq!(check.detail.as_deref(), Some("action command"));
}

// ── Args after verb ───────────────────────────────────────────────

#[test]
fn test_args_after_verb_only_args_and_flags() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add ");
    // Should contain args + flags, not verbs or sub-menus
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"address"));
    assert!(labels.contains(&"interface"));
    assert!(labels.contains(&"comment"));
    assert!(labels.contains(&"X"));
    assert!(!labels.contains(&"print"));
    assert!(!labels.contains(&"route"));
    for it in &items {
        assert!(
            it.kind == Some(kind::PROPERTY) || it.kind == Some(kind::CONSTANT),
            "unexpected kind for {}: {:?}",
            it.label,
            it.kind
        );
    }
}

#[test]
fn test_args_after_verb_with_trailing_space_vs_without() {
    let data = synthetic();
    let with = compute_completions(&data, "/ip/address add ");
    let without = compute_completions(&data, "/ip/address add");
    // Both have command "add", so both should be arg completions
    assert_eq!(with.len(), without.len());
    assert!(with.iter().any(|i| i.label == "address"));
    assert!(without.iter().any(|i| i.label == "address"));
}

#[test]
fn test_args_filter_used_properties() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add address=1.1.1.1 ");
    assert!(!items.iter().any(|i| i.label == "address"));
    assert!(items.iter().any(|i| i.label == "interface"));
}

#[test]
fn test_args_string_type_snippet() {
    let data = synthetic();
    let items = compute_completions(&data, "/ip/address add ");
    let comment = items.iter().find(|i| i.label == "comment").unwrap();
    assert_eq!(comment.insert_text.as_deref(), Some("comment=\"$1\"$0"));
    assert_eq!(comment.insert_text_format, Some(2));
}
