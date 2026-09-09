// ── Signature help for the RSC language server ───────────────────────────
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
// Shared text helpers live in `crate::text_util` (single owner —
// `verb_role`, `type_gloss`, `sanitize_markdown_for_hover`, and the
// label-budget caps); the `#[cfg(test)]` re-export keeps the historical
// `signature::MAX_LABEL_TYPE_CHARS` path resolving for tests.
#[cfg(test)]
pub(crate) use crate::text_util::MAX_LABEL_TYPE_CHARS;
pub(crate) use crate::text_util::{MAX_SIGNATURE_LABEL_BYTES, sanitize_label_segment};
use crate::text_util::{sanitize_markdown_for_hover, type_gloss, verb_role};
use std::collections::HashSet;

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

/// Type as rendered inside the `name=type` label segment.
///
/// Long enum member lists collapse to a bare `enum`; the full members stay
/// in the parameter documentation. Short enums (few members, short display
/// string) keep their members inline so common cases stay scannable.
fn label_type(arg: &ArgEntry) -> String {
    if arg.arg_type.starts_with("enum") {
        let members = arg.enum_members();
        if members.len() > 4 || display_type(arg).len() > 32 {
            return "enum".to_string();
        }
    }
    display_type(arg).to_string()
}

/// Full type for the parameter documentation: the complete member list for
/// collapsed enums, otherwise the raw display type.
fn doc_type(arg: &ArgEntry) -> String {
    if arg.arg_type.starts_with("enum")
        && label_type(arg) == "enum"
        && !arg.enum_members().is_empty()
    {
        format!("enum ({})", arg.enum_members().join(" | "))
    } else {
        display_type(arg).to_string()
    }
}

/// Outer (non-bracket-region) `key` names in COMPLETED pairs only, lowercased
/// for case-insensitive comparison. A pair counts as completed only when its
/// token ends strictly before the cursor (`end < cursor_byte`): the pair
/// under the cursor (`url=`, `url="…`, partial keys) stays listed so
/// `activeParameter` can still highlight it. Mirrors the bracket walk in
/// [`resolve_verb_token`] so `[find key=value]` never filters an outer
/// property.
fn typed_keys(tokens: &[SpanToken], cursor_byte: usize) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut depth: u32 = 0;
    for t in tokens {
        let (opens, closes) = crate::parser::bracket_counts(&t.text);
        if depth > 0 || opens > 0 {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        if t.end < cursor_byte
            && let Some((k, _)) = crate::parser::split_key_value(&t.text)
        {
            out.insert(k.to_ascii_lowercase());
        }
        depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
    }
    out
}

/// Settable named properties of `menu` over an explicit typed-key exclusion
/// set, REQUIRED FIRST then alphabetically within each group, capped at
/// [`MAX_SIGNATURE_PROPERTIES`].
///
/// Only `arguments` participate: flags (`X`, `D`) are print-output markers,
/// and `read_only` properties are outputs — neither is ever typed as a
/// `name=value` pair by a user, so listing them would be misleading.
/// The returned slice backs BOTH the parameters array and the
/// `activeParameter` match so the two can never disagree on indices.
///
/// Already-typed `key=` pairs (outer tokens only) are filtered out, mirroring
/// the completion `ctx.properties` exclusion: re-offering a typed property
/// is noise, and required-missing entries stay pinned first among the rest.
fn sorted_filtered_properties<'a>(
    menu: &'a MenuEntry,
    typed: &HashSet<String>,
) -> Vec<&'a ArgEntry> {
    let mut props: Vec<&ArgEntry> = menu
        .arguments
        .iter()
        .filter(|a| !typed.contains(&a.name.to_ascii_lowercase()))
        .collect();
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
    let mut depth: u32 = 0;
    for (idx, tok) in tokens.iter().enumerate() {
        let (opens, closes) = crate::parser::bracket_counts(&tok.text);
        // Bracket regions are inert: inner words are never path or verb.
        if depth > 0 || opens > 0 {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        if tok.text.starts_with('/') {
            // Valid shorthand menu+verb in one slash token carries the verb.
            if crate::parser::split_trailing_verb(&tok.text, data).is_some() {
                return Some(idx);
            }
            path_parts.push(tok.text.trim_start_matches('/').to_string());
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        if crate::parser::split_key_value(&tok.text).is_some() {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue; // key=value property, wherever it appears
        }
        // Verbs are alphabetic words (mirrors `parser::is_command_leader`):
        // expression debris (`(...)`, `[...]`, `$var`, `.`, `+`, `2h)`,
        // quoted words) is never a verb; anything with an outside-quote
        // `=` that failed validation is debris as well.
        let is_debris = !matches!(
            tok.text.as_bytes().first(),
            Some(b) if b.is_ascii_alphabetic()
        ) || tok.text.contains('=');
        if is_debris {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        let current_path = format!("/{}", path_parts.join("/"));
        let is_sub_menu = data
            .child_names_by_parent
            .get(&current_path)
            .is_some_and(|children| children.iter().any(|c| c.name == tok.text));
        if is_sub_menu {
            path_parts.push(tok.text.clone());
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
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
    // Newest token after the verb that the cursor has reached, skipping
    // inert bracket-region tokens so `[find pool-name=x]` never highlights
    // an outer property.
    let mut depth_by_index: Vec<u32> = Vec::with_capacity(tokens.len());
    let mut depth: u32 = 0;
    for t in tokens.iter() {
        let (opens, closes) = crate::parser::bracket_counts(&t.text);
        depth_by_index.push(depth);
        if depth > 0 || opens > 0 {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
    }
    let token = tokens[verb_token_idx + 1..]
        .iter()
        .enumerate()
        .rev()
        .filter(|(i, _)| {
            depth_by_index
                .get(verb_token_idx + 1 + i)
                .is_some_and(|&d| d == 0)
        })
        .map(|(_, t)| t)
        .find(|t| t.start < cursor_byte)?;

    // `key=value` → key; bare (possibly partial) word → the word itself.
    let key = match crate::parser::split_key_value(&token.text) {
        Some((k, _)) => k,
        None => token.text.as_str(),
    };
    // Empty key (`=value` debris) or identifier-absurd length ⇒ no highlight.
    if key.is_empty() || key.len() > MAX_SUGGEST_INPUT_BYTES {
        return None;
    }

    if let Some(idx) = properties
        .iter()
        .position(|p| p.name.eq_ignore_ascii_case(key))
    {
        return Some(idx as u32);
    }
    // Unique-prefix match (case-insensitive); two or more hits stay silent (ambiguous).
    let lower_key = key.to_ascii_lowercase();
    let mut candidates = properties
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name.to_ascii_lowercase().starts_with(&lower_key));
    let (idx, _) = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(idx as u32)
}

/// The completed-pair key when the cursor sits strictly past a KNOWN outer
/// `key=value` pair after the verb (newest reached token is a `key=value`
/// whose token ends before the cursor and whose key names a menu argument).
/// The caller then advances `activeParameter` to the next missing required
/// property instead of re-highlighting the finished pair. Unknown keys,
/// bare partial words, and the pair under the cursor yield `None` (the
/// normal detection path applies).
fn completed_known_key<'a>(
    menu: &MenuEntry,
    tokens: &'a [SpanToken],
    verb_token_idx: usize,
    cursor_byte: usize,
) -> Option<&'a str> {
    let mut depth_by_index: Vec<u32> = Vec::with_capacity(tokens.len());
    let mut depth: u32 = 0;
    for t in tokens.iter() {
        let (opens, closes) = crate::parser::bracket_counts(&t.text);
        depth_by_index.push(depth);
        if depth > 0 || opens > 0 {
            depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
            continue;
        }
        depth = depth.saturating_add(opens).saturating_sub(closes).min(32);
    }
    let token = tokens[verb_token_idx + 1..]
        .iter()
        .enumerate()
        .rev()
        .filter(|(i, _)| {
            depth_by_index
                .get(verb_token_idx + 1 + i)
                .is_some_and(|&d| d == 0)
        })
        .map(|(_, t)| t)
        .find(|t| t.start < cursor_byte)?;
    let (key, _) = crate::parser::split_key_value(&token.text)?;
    if token.end < cursor_byte
        && menu
            .arguments
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(key))
    {
        Some(key)
    } else {
        None
    }
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
    // Exclude COMPLETED `key=` pairs (same exclusion completion uses
    // via `ctx.properties`); the pair under the cursor stays so
    // `activeParameter` can still highlight it. What remains keeps
    // required-missing first.
    let typed = typed_keys(tokens, cursor_byte);
    let properties = sorted_filtered_properties(menu, &typed);
    if properties.is_empty() {
        return None;
    }

    // Single-line label: `/menu verb name=type name=type …`. The verb text
    // comes from the anchored token, so the label shows the verb AS WRITTEN;
    // a slash shorthand token (`/ipv6/nd/prefix/add`) contributes only its
    // trailing verb segment.
    let raw_verb = &tokens.get(verb_token_idx)?.text;
    let verb_owned;
    let verb: &str = if raw_verb.starts_with('/') {
        verb_owned = raw_verb
            .rsplit('/')
            .next()
            .unwrap_or(raw_verb.as_str())
            .to_string();
        &verb_owned
    } else {
        raw_verb
    };
    let mut label = format!("{} {}", menu.path, verb);
    let mut parameters = Vec::with_capacity(properties.len());
    for arg in &properties {
        let typ = label_type(arg);
        let segment = sanitize_label_segment(&arg.name, &typ);
        // Total-label budget: stop before exceeding ~4KiB so offsets stay exact.
        if label.len() + ' '.len_utf8() + segment.len() > MAX_SIGNATURE_LABEL_BYTES {
            break;
        }
        let start = label.len() + ' '.len_utf8();
        label.push(' ');
        label.push_str(&segment);

        let mut documentation = String::new();
        if arg.required {
            documentation.push_str("(required) ");
        }
        documentation.push_str(&doc_type(arg));
        if let Some(gloss) = type_gloss(&arg.arg_type) {
            documentation.push_str(" — ");
            documentation.push_str(gloss);
        }
        if !arg.description.is_empty() {
            documentation.push_str(" — ");
            documentation.push_str(&sanitize_markdown_for_hover(&arg.description));
        }

        parameters.push(ParameterInformation {
            label: [start, start + segment.len()],
            documentation,
        });
    }

    // Short markdown header: backticked resolved path + verb, menu type,
    // and the verb role (what this command does), plus the ordering note
    // once at least one required property exists.
    let menu_kind = if menu.menu_type.is_empty() {
        "Directory"
    } else {
        &menu.menu_type
    };
    let mut documentation = format!(
        "`{} {verb}` ({menu_kind}) — {verb} {}",
        menu.path,
        verb_role(verb)
    );
    if properties.iter().any(|p| p.required) {
        documentation.push_str("\n\nRequired properties listed first.");
    }
    // Truncation note: the property list stays capped at
    // MAX_SIGNATURE_PROPERTIES, but the header says how many were hidden.
    // `total` counts only untyped properties so the note stays consistent
    // with the filtered list actually shown.
    let total_untyped = menu
        .arguments
        .iter()
        .filter(|a| !typed.contains(&a.name.to_ascii_lowercase()))
        .count();
    if total_untyped > properties.len() {
        documentation.push_str(&format!(
            "\n\n… (+{} more)",
            total_untyped - properties.len()
        ));
    }

    let active_parameter =
        if completed_known_key(menu, tokens, verb_token_idx, cursor_byte).is_some() {
            // The cursor sits past a completed KNOWN `key=value` pair: advance
            // to the next missing required property instead of re-highlighting
            // the finished one. No required missing ⇒ no highlight (the popup
            // still shows).
            properties
                .iter()
                .position(|p| p.required)
                .map(|idx| idx as u32)
        } else {
            detect_active_parameter(tokens, verb_token_idx, cursor_byte, &properties)
        };

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
