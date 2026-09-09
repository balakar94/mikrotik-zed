//! Signature help.
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

// ── Capability advertisement ─────────────────────────────────

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
fn test_unique_prefix_partial_word_matches() {
    // `check-c` prefixes exactly one property.
    let line = "/tool/fetch add check-c";
    let help = help_at(&fetch_data(), line, line.len());
    assert_eq!(active(&help), Some(2));
}

#[test]
fn test_ambiguous_prefix_yields_no_active_parameter() {
    let line = "/tool/fetch add check-";
    let help = help_at(&fetch_data(), line, line.len());
    assert!(
        help.active_parameter.is_none(),
        "check- matches two properties ⇒ omit instead of guessing"
    );
    assert_eq!(help.signatures.len(), 1, "popup still shows");
}

#[test]
fn test_cursor_inside_quoted_value_keeps_key_active() {
    // Unterminated quote: tokenizer keeps `url="http://x y` as ONE token,
    // so the spaces/quotes cannot spawn phantom words.
    let line = "/tool/fetch add url=\"http://x y";
    let help = help_at(&fetch_data(), line, line.find("//").unwrap());
    assert_eq!(active(&help), Some(1));
}

#[test]
fn test_cursor_on_verb_or_before_it_yields_no_active_parameter() {
    let line = "/tool/fetch add url=x";
    let verb_end = line.rfind("add").unwrap() + 3;
    let help = help_at(&fetch_data(), line, verb_end);
    assert!(
        help.active_parameter.is_none(),
        "verb token itself never matches"
    );
    // Cursor inside the menu path: likewise nothing highlighted.
    let help = help_at(&fetch_data(), line, 5);
    assert!(help.active_parameter.is_none());
}
