// Signature help.
use crate::menus::MenuData;
use crate::menus::MenuEntry;
use crate::parser::tokenize_with_spans;
use crate::server::Server;
use crate::signature::*;
use std::sync::Arc;
fn sig_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
required = true
[[menus.arguments]]
name = "check-certificate"
type = "bool"
[[menus.arguments]]
name = "check-expired"
type = "bool"
[[menus.arguments]]
name = "http-method"
type = "enum (get | post)"
required = true
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = ""
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
"#,
    ))
}

fn make_server() -> Server {
    Server::new(sig_data())
}

fn open(s: &mut Server, uri: &str, doc: &str) {
    s.handle_message(
        "textDocument/didOpen",
        &serde_json::json!({"params": {"textDocument": {"uri": uri, "text": doc}}}),
    );
}

fn sig_request(id: i64, uri: &str, line: usize, character: usize) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "params": {
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": character}
        }
    })
}

/// Expected single-line label for `/tool/fetch add …`: REQUIRED FIRST
/// (alphabetical: http-method, url), then the optionals alphabetically.
const FETCH_LABEL: &str = "/tool/fetch add http-method=enum (get | post) url=string \
                               check-certificate=bool check-expired=bool";

// ── Capability advertisement ─────────────────────────────────────────────

fn fetch_data() -> MenuData {
    MenuData::from_toml_str(
        r#"
[[menus]]
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
required = true
[[menus.arguments]]
name = "check-certificate"
type = "bool"
[[menus.arguments]]
name = "check-expired"
type = "bool"
[[menus.arguments]]
name = "http-method"
type = "enum (get | post)"
required = true
[[menus]]
path = "/empty/menu"
type = "Directory"
"#,
    )
}

fn help_for(data: &MenuData, path: &str, line_text: &str, cursor: usize) -> Option<SignatureHelp> {
    let m = menu(data, path);
    let tokens = tokenize_with_spans(line_text);
    let verb_idx = resolve_verb_token(data, &tokens)?;
    compute_signature_help(m, &tokens, verb_idx, cursor)
}

fn menu<'a>(data: &'a MenuData, path: &str) -> &'a MenuEntry {
    data.menu_by_path.get(path).expect("fixture menu")
}

/// Compute with the cursor placed at byte `cursor` of `line_text`,
/// resolving the verb exactly like the handler does.
fn help_at(data: &MenuData, line_text: &str, cursor: usize) -> SignatureHelp {
    help_opt(data, line_text, cursor).expect("fixture menu has properties and a verb")
}

fn help_opt(data: &MenuData, line_text: &str, cursor: usize) -> Option<SignatureHelp> {
    let m = menu(data, "/tool/fetch");
    let tokens = tokenize_with_spans(line_text);
    let verb_idx = resolve_verb_token(data, &tokens)?;
    compute_signature_help(m, &tokens, verb_idx, cursor)
}

fn active(help: &SignatureHelp) -> Option<usize> {
    help.active_parameter.map(|v| v as usize)
}

// ── Label construction ───────────────────────────────────────────────────

#[test]
fn test_documentation_marks_required_and_mentions_ordering() {
    let line = "/tool/fetch add ";
    let sig = &help_at(&fetch_data(), line, line.len()).signatures[0];
    assert!(
        sig.documentation.contains("`/tool/fetch add` (Command)")
            && sig.documentation.contains("add creates a new entry")
            && sig
                .documentation
                .contains("Required properties listed first."),
        "got {}",
        sig.documentation
    );
    let required_docs: Vec<&str> = sig
        .parameters
        .iter()
        .map(|p| p.documentation.as_str())
        .filter(|d| d.starts_with("(required) "))
        .collect();
    assert_eq!(required_docs.len(), 2, "http-method + url are required");
    // Optional docs have no marker; bool keeps raw type plus gloss.
    assert!(
        sig.parameters[2].documentation.starts_with("bool"),
        "got {}",
        sig.parameters[2].documentation
    );
    assert!(
        sig.parameters[0]
            .documentation
            .starts_with("(required) enum (get | post)")
    );
}

#[test]
fn test_description_attached_to_parameter_documentation() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "opt"
type = "num"
description = "how much"
"#,
    );
    let help = help_for(&data, "/m", "/m add ", 7).expect("has properties");
    assert_eq!(
        help.signatures[0].parameters[0].documentation,
        "num — how much"
    );
}

#[test]
fn test_no_required_properties_omits_ordering_note() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "alpha"
type = "string"
"#,
    );
    let help = help_for(&data, "/m", "/m print ", 9).unwrap();
    assert!(!help.signatures[0].documentation.contains("Required"));
    assert_eq!(help.signatures[0].label, "/m print alpha=string");
}

#[test]
fn test_empty_type_displays_any() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "blank"
type = ""
"#,
    );
    let help = help_for(&data, "/m", "/m add ", 7).unwrap();
    assert_eq!(help.signatures[0].label, "/m add blank=any");
}

// ── Gating / caps ────────────────────────────────────────────────────────

#[test]
fn test_menu_without_arguments_returns_none() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/empty/menu"
type = "Directory"
"#,
    );
    let help = help_for(&data, "/empty/menu", "/empty/menu print ", 17);
    assert!(help.is_none(), "anti-noise: nothing to show");
}

#[test]
fn test_property_list_capped_at_max() {
    let mut toml = String::from("[[menus]]\npath = \"/big\"\ntype = \"Directory\"\n");
    for i in 0..50 {
        toml.push_str(&format!(
            "[[menus.arguments]]\nname = \"prop{i:02}\"\ntype = \"string\"\n"
        ));
    }
    let data = MenuData::from_toml_str(&toml);
    let help = help_for(&data, "/big", "/big add ", 9).expect("capped list still non-empty");
    assert_eq!(
        help.signatures[0].parameters.len(),
        MAX_SIGNATURE_PROPERTIES
    );
    // Alphabetical truncation keeps the FIRST forty (prop00..prop39).
    let last = help.signatures[0].parameters.last().unwrap();
    let seg = &help.signatures[0].label[last.label[0]..last.label[1]];
    assert_eq!(seg.split('=').next().unwrap(), "prop39");
}

#[test]
fn test_truncation_note_reports_hidden_count() {
    let mut toml = String::from("[[menus]]\npath = \"/big60\"\ntype = \"Directory\"\n");
    for i in 0..60 {
        toml.push_str(&format!(
            "[[menus.arguments]]\nname = \"prop{i:02}\"\ntype = \"string\"\n"
        ));
    }
    let data = MenuData::from_toml_str(&toml);
    let help = help_for(&data, "/big60", "/big60 add ", 11).expect("capped list still non-empty");
    assert_eq!(
        help.signatures[0].parameters.len(),
        MAX_SIGNATURE_PROPERTIES
    );
    assert!(
        help.signatures[0].documentation.contains("(+20 more)"),
        "truncation note must report hidden count, got {}",
        help.signatures[0].documentation
    );
}

// ── activeParameter detection ────────────────────────────────────────────

#[test]
fn test_exact_key_match_after_equals() {
    let line = "/tool/fetch add url=";
    let help = help_at(&fetch_data(), line, line.len());
    assert_eq!(
        active(&help),
        Some(1),
        "url is the second (required-first) param"
    );
}
