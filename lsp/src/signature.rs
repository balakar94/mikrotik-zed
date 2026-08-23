// ── Signature help for the RSC language server ──────────────────
//
// textDocument/signatureHelp adapted to RouterOS's named-parameter CLI:
// commands carry no positional parentheses (`/tool fetch url="…"`
// check-certificate=yes), so ONE SignatureInformation describes the resolved
// `/menu verb` command and every ParameterInformation is a `name=type`
// segment inside that single-line label. `activeParameter` points at the
// property the cursor is currently typing, detected from quote-aware tokens
// of the LOGICAL line (the caller joins continuations and maps the cursor).
//
// Anti-noise contract: a popup only appears when the line resolves to a real
// menu AND carries a command verb AND that menu declares at least one settable
// property. Everything here is pure — no I/O, deterministic output.

use crate::menus::{ArgEntry, MenuData, MenuEntry};
use crate::parser::SpanToken;
use crate::suggest::MAX_SUGGEST_INPUT_BYTES;

/// Cap on how many properties the signature may list, counted AFTER the
/// required-first/alphabetical sort. Bounds both the constructed label string
/// and the response payload for menus with enormous property tables (the
/// largest embedded menus declare ~60 arguments); beyond forty entries the
/// tail of an alphabetical list is the least likely thing a user is typing.
pub(crate) const MAX_SIGNATURE_PROPERTIES: usize = 40;

/// One `name=type` entry inside the signature label.
///
/// `label` holds BYTE offsets `[start, end]` of the segment inside the
/// constructed [`SignatureInformation::label`] string (LSP allows the
/// offset form precisely so labels need no per-parameter escaping).
#[derive(Debug, serde::Serialize)]
pub(crate) struct ParameterInformation {
    pub(crate) label: [usize; 2],
    pub(crate) documentation: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SignatureInformation {
    pub(crate) label: String,
    pub(crate) documentation: String,
    pub(crate) parameters: Vec<ParameterInformation>,
}

/// LSP 3.17 SignatureHelp for exactly one signature.
///
/// Field names serialize camelCase per the wire format; `activeParameter` is
/// OMITTED when no current property could be identified (the popup still
/// renders, nothing is highlighted).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignatureHelp {
    pub(crate) signatures: Vec<SignatureInformation>,
    pub(crate) active_signature: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) active_parameter: Option<u32>,
}

/// Type shown for a property inside the label segment and its documentation.
///
/// Empty upstream types render as `any` (same convention as hover's
/// "(any)", minus the parens which read poorly glued to `name=`).
fn display_type(arg: &ArgEntry) -> &str {
    if arg.arg_type.is_empty() {
        "any"
    } else {
        &arg.arg_type
    }
}

/// Settable named properties of `menu`, REQUIRED FIRST then alphabetically
/// within each group, capped at [`MAX_SIGNATURE_PROPERTIES`].
///
/// Only `arguments` participate: flags (`X`, `D`) are print-output markers,
/// and `read_only` properties are outputs — neither is ever typed as a
/// `name=value` pair by a user, so listing them would be misleading.
/// The returned slice backs BOTH the parameters array and the
/// `activeParameter` match so the two can never disagree on indices.
fn sorted_properties(menu: &MenuEntry) -> Vec<&ArgEntry> {
    let mut props: Vec<&ArgEntry> = menu.arguments.iter().collect();
    // Descending on `required` (true sorts first), ties broken by name —
    // a total order, hence independent of the embedded table's order.
    props.sort_by(|a, b| {
        b.required
            .cmp(&a.required)
            .then_with(|| a.name.cmp(&b.name))
    });
    props.truncate(MAX_SIGNATURE_PROPERTIES);
    props
}

/// Token index of the command verb on the tokenized LOGICAL line.
///
/// Mirrors `parse_line`'s walk — `/`-prefixed tokens extend the menu path,
/// `key=value` tokens are properties, a bare word extends the path only when
/// `child_names_by_parent` knows it as a sub-menu child — but anchors on the
/// **FIRST** bare non-sub-menu word instead of the last. That difference is
/// the whole point: while a property is being typed (`/tool fetch add che`),
/// `parse_line.command` already points at the trailing fragment `che`, which
/// would anchor activeParameter detection on the wrong token and disable
/// highlighting exactly when it matters most. Returns `None` when no verb
/// exists (the caller's anti-noise gate).
pub(crate) fn resolve_verb_token(data: &MenuData, tokens: &[SpanToken]) -> Option<usize> {
    let mut path_parts: Vec<String> = Vec::new();
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.text.starts_with('/') {
            path_parts.push(tok.text.trim_start_matches('/').to_string());
            continue;
        }
        if tok.text.contains('=') {
            continue; // key=value property, wherever it appears
        }
        let current_path = format!("/{}", path_parts.join("/"));
        let is_sub_menu = data
            .child_names_by_parent
            .get(&current_path)
            .is_some_and(|children| children.iter().any(|c| c.name == tok.text));
        if is_sub_menu {
            path_parts.push(tok.text.clone());
            continue;
        }
        return Some(idx);
    }
    None
}

/// Identify which parameter index the cursor is currently on, if any.
///
/// `tokens` are quote-aware spans of the LOGICAL line, `verb_token_idx` the
/// index returned by [`resolve_verb_token`], and `cursor_byte` the insertion
/// point within that same text, so a quoted VALUE stays part of its `key=…`
/// token and cannot confuse the match.
///
/// Candidates are only tokens AFTER the verb that the cursor has reached
/// (`start < cursor`): this excludes the menu path, the verb itself (typing
/// `add` must never highlight `address`), and any text after the insertion
/// point. From the newest such token the KEY before `=` is matched — exact
/// name first, else a UNIQUE prefix (an ambiguous prefix highlights nothing
/// rather than guessing). No candidate ⇒ `None`; the popup still shows.
fn detect_active_parameter(
    tokens: &[SpanToken],
    verb_token_idx: usize,
    cursor_byte: usize,
    properties: &[&ArgEntry],
) -> Option<u32> {
    let token = tokens[verb_token_idx + 1..]
        .iter()
        .rev()
        .find(|t| t.start < cursor_byte)?;

    // `key=value` → key; bare (possibly partial) word → the word itself.
    let eq_idx = token.text.find('=');
    let key = match eq_idx {
        Some(eq) => &token.text[..eq],
        None => token.text.as_str(),
    };
    // Empty key (`=value` debris) or identifier-absurd length ⇒ no highlight.
    if key.is_empty() || key.len() > MAX_SUGGEST_INPUT_BYTES {
        return None;
    }

    if let Some(idx) = properties.iter().position(|p| p.name == key) {
        return Some(idx as u32);
    }
    // Unique-prefix match; two or more hits stay silent (ambiguous).
    let mut candidates = properties
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name.starts_with(key));
    let (idx, _) = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(idx as u32)
}

/// Build the signature-help response for a resolved `menu`.
///
/// `tokens` are quote-aware spans of the joined LOGICAL line with
/// `verb_token_idx` pointing at its command verb ([`resolve_verb_token`]);
/// `cursor_byte` is a byte offset within that same text (both produced by
/// the caller via `diagnostics::logical_lines` /
/// `LogicalLine::logical_offset_from_physical`). Returns `None` when the
/// menu declares no settable properties — a bare `/menu verb` popup without
/// any `name=type` segment carries no information worth interrupting for.
pub(crate) fn compute_signature_help(
    menu: &MenuEntry,
    tokens: &[SpanToken],
    verb_token_idx: usize,
    cursor_byte: usize,
) -> Option<SignatureHelp> {
    let properties = sorted_properties(menu);
    if properties.is_empty() {
        return None;
    }

    // Single-line label: `/menu verb name=type name=type …`. The verb text
    // comes from the anchored token, so the label shows the verb AS WRITTEN.
    // Each segment's byte offsets are recorded while assembling so
    // ParameterInformation labels point EXACTLY at their slice of the
    // finished string.
    let verb = &tokens.get(verb_token_idx)?.text;
    let mut label = format!("{} {}", menu.path, verb);
    let mut parameters = Vec::with_capacity(properties.len());
    for arg in &properties {
        let typ = display_type(arg);
        let start = label.len() + ' '.len_utf8();
        let segment = format!("{}={}", arg.name, typ);
        label.push(' ');
        label.push_str(&segment);

        let mut documentation = String::new();
        if arg.required {
            documentation.push_str("(required) ");
        }
        documentation.push_str(typ);
        if !arg.description.is_empty() {
            documentation.push_str(" — ");
            documentation.push_str(&arg.description);
        }

        parameters.push(ParameterInformation {
            label: [start, start + segment.len()],
            documentation,
        });
    }

    // Short markdown header: backticked resolved path + menu type (the
    // dataset carries no menu-level description), plus the ordering note
    // once at least one required property exists.
    let mut documentation = format!(
        "`{}` ({})",
        menu.path,
        if menu.menu_type.is_empty() {
            "Directory"
        } else {
            &menu.menu_type
        }
    );
    if properties.iter().any(|p| p.required) {
        documentation.push_str("\n\nRequired properties listed first.");
    }

    let active_parameter =
        detect_active_parameter(tokens, verb_token_idx, cursor_byte, &properties);

    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation,
            parameters,
        }],
        active_signature: 0,
        active_parameter,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::MenuData;
    use crate::tokenize_with_spans;

    /// /tool/fetch-shaped fixture: two required, two optional sharing a
    /// prefix (for ambiguity checks), one enum type with spaces.
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

    fn help_for(
        data: &MenuData,
        path: &str,
        line_text: &str,
        cursor: usize,
    ) -> Option<SignatureHelp> {
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
    fn test_label_required_first_alphabetical_and_offsets_slice_exactly() {
        let line = "/tool/fetch add ";
        let help = help_at(&fetch_data(), line, line.len());
        assert_eq!(help.signatures.len(), 1, "exactly one signature");
        let sig = &help.signatures[0];
        // Required first (alphabetical: http-method, url), then the rest.
        assert_eq!(
            sig.label,
            "/tool/fetch add http-method=enum (get | post) url=string check-certificate=bool check-expired=bool"
        );
        let names: Vec<&str> = sig
            .parameters
            .iter()
            .map(|p| &sig.label[p.label[0]..p.label[1]])
            .map(|seg| seg.split('=').next().unwrap())
            .collect();
        assert_eq!(
            names,
            ["http-method", "url", "check-certificate", "check-expired"]
        );
        // Every offset pair slices the label cleanly (start <= end, in bounds).
        for p in &sig.parameters {
            assert!(p.label[0] < p.label[1] && p.label[1] <= sig.label.len());
        }
    }

    #[test]
    fn test_documentation_marks_required_and_mentions_ordering() {
        let line = "/tool/fetch add ";
        let sig = &help_at(&fetch_data(), line, line.len()).signatures[0];
        assert!(
            sig.documentation.contains("`/tool/fetch` (Command)")
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
        // Optional docs have no marker; enum type survives verbatim.
        assert!(sig.parameters[2].documentation == "bool");
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

    // ── Gating / caps ─────────────────────────────────────────────

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

    // ── activeParameter detection ─────────────────────────────────

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

    #[test]
    fn test_completed_pair_before_cursor_stays_active() {
        // Just finished `url=x `: the newest reached token is url's pair.
        let line = "/tool/fetch add url=x ";
        let help = help_at(&fetch_data(), line, line.len());
        assert_eq!(active(&help), Some(1));
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
}
