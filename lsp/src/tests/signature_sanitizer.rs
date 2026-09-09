//! Signature — labels and budgets.
use crate::menus::MenuData;
use crate::menus::MenuEntry;
use crate::parser::tokenize_with_spans;
use crate::server::Server;
use crate::signature::*;
use crate::suggest::MAX_SUGGEST_INPUT_BYTES;
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

// ── Label construction ────────────────────────────────────────

#[test]
fn test_label_offsets_slice_exactly_sanitized_name_type() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "ok"
type = "string"
"#,
    );
    let help = help_for(&data, "/m", "/m add ", 7).expect("has properties");
    let sig = &help.signatures[0];
    for p in &sig.parameters {
        let seg = &sig.label[p.label[0]..p.label[1]];
        let (name, typ) = seg.split_once('=').expect("name=type shape");
        assert_eq!(seg, sanitize_label_segment(name, typ));
    }
}

#[test]
fn test_param_documentation_markdown_sanitized() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "opt"
type = "num"
description = "see [docs](https://example.com/x) <b>y</b>"
"#,
    );
    let help = help_for(&data, "/m", "/m add ", 7).expect("has properties");
    let doc = &help.signatures[0].parameters[0].documentation;
    assert!(doc.contains("see docs"), "got {doc}");
    assert!(!doc.contains("https://example.com"));
    assert!(!doc.contains("<b>"));
}

#[test]
fn test_signature_label_stays_within_budget() {
    let mut toml = String::from("[[menus]]\npath = \"/big\"\ntype = \"Directory\"\n");
    for i in 0..50 {
        toml.push_str(&format!(
            "[[menus.arguments]]\nname = \"prop{i:02}\"\ntype = \"string\"\n"
        ));
    }
    let data = MenuData::from_toml_str(&toml);
    let help = help_for(&data, "/big", "/big add ", 9).expect("capped list");
    assert!(help.signatures[0].label.len() <= MAX_SIGNATURE_LABEL_BYTES);
    for p in &help.signatures[0].parameters {
        let seg = &help.signatures[0].label[p.label[0]..p.label[1]];
        assert!(seg.contains('='));
    }
}
