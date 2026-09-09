// Completion documentation, sort text and root trigger.
// Copied (not moved) from `lsp/src/completion.rs` (`mod extra_coverage` L1694-1747, L2156-2300);
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
// ── documentation on completion items ────────────────────────────────────

#[test]
fn test_arg_item_documentation_markdown_from_description() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/docs/menu"
type = "Directory"
[[menus.arguments]]
name = "with-desc"
type = "ipPrefix"
description = "The IP address"
[[menus.arguments]]
name = "no-desc"
type = "string"
"#,
    );
    let items = compute_completions(&data, "/docs/menu add ");
    let with = items.iter().find(|i| i.label == "with-desc").unwrap();
    let doc = with.documentation.as_ref().expect("documentation present");
    assert_eq!(doc.kind, "markdown");
    assert_eq!(doc.value, "The IP address");

    // No description → no documentation field at all.
    let without = items.iter().find(|i| i.label == "no-desc").unwrap();
    assert!(without.documentation.is_none());
}

#[test]
fn test_flag_item_documentation_from_description() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/flags/menu"
type = "Directory"
[[menus.flags]]
name = "X"
description = "disabled"
[[menus.flags]]
name = "D"
description = ""
"#,
    );
    let items = compute_completions(&data, "/flags/menu add ");
    let x = items.iter().find(|i| i.label == "X").unwrap();
    let doc = x.documentation.as_ref().expect("flag documentation");
    assert_eq!(doc.kind, "markdown");
    assert_eq!(doc.value, "disabled");

    // Flags WITHOUT description carry no documentation field at all.
    let d = items.iter().find(|i| i.label == "D").unwrap();
    assert!(d.documentation.is_none());
}

#[test]
fn test_items_without_description_have_no_documentation_field() {
    // /system/clock time-zone-name has no description in this fixture.
    let data = synthetic();
    let items = compute_completions(&data, "/system/clock set ");
    let tzn = items.iter().find(|i| i.label == "time-zone-name").unwrap();
    assert!(tzn.documentation.is_none());
}

// ── sortText: required before optional ───────────────────────────────────

#[test]
fn test_sorttext_required_before_optional() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/demo"
type = "Directory"
[[menus.arguments]]
name = "required-prop"
type = "string"
required = true
[[menus.arguments]]
name = "aaa-optional"
type = "string"
"#,
    );
    let items = compute_completions(&data, "/demo add ");
    let req = items.iter().find(|i| i.label == "required-prop").unwrap();
    let opt = items.iter().find(|i| i.label == "aaa-optional").unwrap();
    assert_eq!(req.sort_text.as_deref(), Some("0required-prop"));
    assert_eq!(opt.sort_text.as_deref(), Some("1aaa-optional"));
    // Lexicographic sortText puts the required property first even
    // though it sorts later alphabetically.
    assert!(req.sort_text < opt.sort_text);
}

#[test]
fn test_sorttext_tiers_for_verbs_submenus_flags() {
    let data = synthetic();
    // Verbs rank at tier 2, sub-menus at tier 3, flags at tier 7 — every
    // non-root item carries a deterministic sortText so truncation is
    // relevance-ordered instead of construction-ordered.
    let verbs = compute_completions(&data, "/ip/address ");
    let add = verbs.iter().find(|i| i.label == "add").unwrap();
    assert!(
        add.sort_text.as_deref().unwrap_or("").starts_with('2'),
        "verb tier 2, got {:?}",
        add.sort_text
    );
    let menus = compute_completions(&data, "/ip ");
    let addr = menus.iter().find(|i| i.label == "address").unwrap();
    assert!(
        addr.sort_text.as_deref().unwrap_or("").starts_with('3'),
        "sub-menu tier 3, got {:?}",
        addr.sort_text
    );
    // Flags are CONSTANT kind at tier 7.
    let args = compute_completions(&data, "/ip/address add ");
    let flag = args.iter().find(|i| i.label == "X").unwrap();
    assert!(
        flag.sort_text.as_deref().unwrap_or("").starts_with('7'),
        "flag tier 7, got {:?}",
        flag.sort_text
    );
}

// ── Root trigger variants ────────────────────────────────────────────────

#[test]
fn test_slash_alone_returns_root_menus_not_verbs() {
    let data = synthetic();
    let items = compute_completions(&data, "/");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    // Roots of this fixture: /ip and /system.
    assert!(labels.contains(&"/ip"));
    assert!(labels.contains(&"/system"));
    assert!(
        !labels.contains(&"add"),
        "verbs must not leak into root trigger"
    );
    for i in &items {
        assert_eq!(i.kind, Some(kind::CLASS));
    }
}

// ── Statement-start snippets (B3) ────────────────────────────────────────

const SNIPPET_LABELS: [&str; 4] = [":if", ":foreach", ":for", ":do"];
