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
fn test_completed_pair_before_cursor_advances_to_next_required() {
    // Just finished `url=x `: the cursor sits past a completed KNOWN
    // pair, so the highlight advances to the next missing required
    // property (`http-method`, index 0) instead of the finished one.
    // `url` itself is filtered out of the label.
    let line = "/tool/fetch add url=x ";
    let help = help_at(&fetch_data(), line, line.len());
    assert_eq!(active(&help), Some(0));
    let sig = &help.signatures[0];
    assert!(
        !sig.label.contains("url="),
        "typed pair must be filtered, got {}",
        sig.label
    );
    assert!(sig.label.contains("http-method="));
}

#[test]
fn test_typed_pair_filtered_keeps_required_missing_first() {
    // A completed optional pair disappears; required-missing stays first.
    let line = "/tool/fetch add check-certificate=yes ";
    let help = help_at(&fetch_data(), line, line.len());
    let sig = &help.signatures[0];
    assert!(
        !sig.label.contains("check-certificate="),
        "got {}",
        sig.label
    );
    let first = &sig.label[sig.parameters[0].label[0]..sig.parameters[0].label[1]];
    assert!(first.starts_with("http-method="), "got {first}");
    assert_eq!(active(&help), Some(0));
}

#[test]
fn test_long_enum_collapses_in_label_keeps_members_in_docs() {
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/m"
type = "Directory"
[[menus.arguments]]
name = "mode"
type = "enum (a | b | c | d | e | f)"
enum_values = ["a", "b", "c", "d", "e", "f"]
"#,
    );
    let help = help_for(&data, "/m", "/m add ", 7).expect("has properties");
    let sig = &help.signatures[0];
    assert_eq!(sig.label, "/m add mode=enum");
    assert!(
        sig.parameters[0]
            .documentation
            .contains("enum (a | b | c | d | e | f)"),
        "full members live in documentation, got {}",
        sig.parameters[0].documentation
    );
}

#[test]
fn test_short_enum_stays_expanded_in_label() {
    let line = "/tool/fetch add ";
    let sig = &help_at(&fetch_data(), line, line.len()).signatures[0];
    assert!(
        sig.label.contains("http-method=enum (get | post)"),
        "short enum keeps members inline, got {}",
        sig.label
    );
}

#[test]
fn test_unknown_key_after_verb_yields_no_active_parameter() {
    let line = "/tool/fetch add zzz=";
    let help = help_at(&fetch_data(), line, line.len());
    assert!(help.active_parameter.is_none());
}

#[test]
fn test_absurdly_long_key_yields_no_active_parameter() {
    let long = "k".repeat(MAX_SUGGEST_INPUT_BYTES + 1);
    let line = format!("/tool/fetch add {long}=");
    let help = help_at(&fetch_data(), &line, line.len());
    assert!(help.active_parameter.is_none());
}

#[test]
fn test_verb_found_after_submenu_words_not_property_collision() {
    // Space-separated sub-menu segments precede the verb; the detector
    // must anchor on the VERB token, not the first bare word.
    let data = MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward)"
required = true
[[menus]]
path = "/ip"
type = "Directory"
[[menus]]
path = "/ip/firewall"
type = "Directory"
"#,
    );
    let line = "/ip firewall filter add chain=";
    let help = help_for(&data, "/ip/firewall/filter", line, line.len()).unwrap();
    assert_eq!(active(&help), Some(0));
}

#[test]
fn test_real_data_ip_address_signature() {
    // Real embedded table: /ip/address has `interface` required, `address`
    // untyped. Required-first ordering must hold on live data too.
    let data = MenuData::load();
    let line = "/ip/address add ";
    let help = help_for(&data, "/ip/address", line, line.len()).expect("real menu");
    let sig = &help.signatures[0];
    let first = &sig.label[sig.parameters[0].label[0]..sig.parameters[0].label[1]];
    assert_eq!(first, "interface=iface_enum", "required property leads");
    assert!(sig.label.starts_with("/ip/address add "));
    assert!(
        sig.parameters[0]
            .documentation
            .starts_with("(required) iface_enum")
    );
}

// ── Stream B: label hardening ───────────────────────────────────

#[test]
fn test_sanitize_label_segment_controls_and_type_cap() {
    assert_eq!(sanitize_label_segment("name", "string"), "name=string");
    assert_eq!(sanitize_label_segment("a\nb", "x\ry"), "a b=x y");
    assert_eq!(sanitize_label_segment("a\tb", "t"), "a b=t");
    let long_type = "t".repeat(100);
    let seg = sanitize_label_segment("n", &long_type);
    let typ = seg.split('=').nth(1).unwrap();
    assert_eq!(typ.chars().count(), MAX_LABEL_TYPE_CHARS);
}
