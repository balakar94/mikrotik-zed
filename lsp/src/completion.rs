// ── Completion logic for the RSC language server ─────────────────
//
// Port of the ls.mjs completion engine.  Strategy:
// - Always return ALL possible candidates (sub-menus, verbs, arguments)
//   and let Zed's fuzzy filter narrow them down.
// - Exception: when the cursor sits inside a "property=value" token,
//   switch to value suggestions (enum values, booleans, type hints) with
//   a case-insensitive prefix pre-filter over the typed suffix.
// - Exception: when the cursor sits inside a `:`-prefixed token (`:` is a
//   completion trigger character), keep only candidates whose label starts
//   with that typed token — script globals and statement snippets. Menu
//   paths and property names make no sense after a colon.

use crate::menus::{ArgEntry, LineContext, MenuData};

/// LSP CompletionItemKind values (mirrors the LSP spec)
mod kind {
    pub const FUNCTION: i32 = 3;
    pub const PROPERTY: i32 = 5;
    pub const CLASS: i32 = 9;
    pub const ENUM_MEMBER: i32 = 12;
    pub const CONSTANT: i32 = 14;
    pub const SNIPPET: i32 = 15;
}

/// LSP MarkupContent for `CompletionItem.documentation`.
#[derive(serde::Serialize, Clone)]
pub struct Documentation {
    pub kind: &'static str, // always "markdown"
    pub value: String,
}

/// A completion item ready for JSON serialization.
///
/// Newer optional fields (`documentation`, `sortText`) are omitted from the
/// JSON when unset instead of serialized as null — semantically identical
/// for LSP clients and keeps the payload small. Pre-existing optional fields
/// keep their historical null-emitting shape for wire compatibility.
#[derive(serde::Serialize, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub kind: Option<i32>,
    pub detail: Option<String>,
    pub insert_text: Option<String>,
    #[serde(rename = "insertTextFormat")]
    pub insert_text_format: Option<i32>,
    #[serde(rename = "sortText", skip_serializing_if = "Option::is_none")]
    pub sort_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<Documentation>,
}

impl CompletionItem {
    fn new(label: String, kind: i32) -> Self {
        CompletionItem {
            label,
            kind: Some(kind),
            detail: None,
            insert_text: None,
            insert_text_format: None,
            sort_text: None,
            documentation: None,
        }
    }
}

pub fn compute_completions(data: &MenuData, before_cursor: &str) -> Vec<CompletionItem> {
    let context = crate::parse_line(data, before_cursor);

    let mut items = match_context(data, &context, before_cursor);

    // The partially typed `:`-prefixed token under the cursor, if any.
    // `:` fires completion requests; detecting it here (instead of earlier)
    // keeps every non-colon context byte-for-byte unchanged.
    let colon_token = colon_typed_token(before_cursor);

    // Statement-start snippets: structural `:if` / `:foreach` / `:for` /
    // `:do` templates offered ONLY where a new statement may begin. Two
    // trigger paths reach them:
    // - a SPACE-fired request at a statement start — an empty line or right
    //   after `{` (the historical path);
    // - a `:`-fired request while the script word itself is being typed
    //   (`:`, `:i`, …) — the statement-start question then applies to
    //   whatever precedes the partial token.
    let at_start = if colon_token.is_some() {
        at_statement_start_before_last_token(before_cursor)
    } else {
        at_statement_start(before_cursor)
    };
    if context.path.is_empty() && !before_cursor.ends_with('/') && at_start {
        items.extend(statement_snippet_items());
    }

    // Colon filtering: keep only candidates whose label starts with the
    // typed `:`-token (`:` → every script item; `:i` → just `:if`). Unlike
    // the value-completion prefix filter there is deliberately NO fallback
    // to the unfiltered set — menu paths and property names are noise after
    // a colon, so an unknown script word completes to nothing.
    if let Some(typed) = colon_token {
        items.retain(|item| item.label.starts_with(&typed));
    }

    items
}

/// The pre-snippet dispatch of [`compute_completions`], kept as its own step
/// so snippet appending can wrap whatever base candidate set applies.
fn match_context(
    data: &MenuData,
    context: &LineContext,
    before_cursor: &str,
) -> Vec<CompletionItem> {
    // No path yet (or a bare "/") → suggest root menus. "/" parses to path
    // "/" which has no child index entry of its own, so it must be treated
    // as the root trigger it is.
    if context.path.is_empty() || context.path == "/" {
        return get_root_completion_items(data);
    }

    // Typing a property VALUE inside the current token ("chain=" or the
    // partial "chain=in") → suggest enum/bool/type values filtered by the
    // already-typed suffix. Only when the cursor sits INSIDE the token:
    // trailing whitespace means the token is finished and the user is about
    // to start a new property, which must stay an argument-completion case.
    if !before_cursor.ends_with(char::is_whitespace)
        && let Some(eq_pos) = context.last_token.rfind('=')
    {
        let key = &context.last_token[..eq_pos];
        let typed_suffix = &context.last_token[eq_pos + 1..];
        let items = get_value_completions(data, context, key);
        return filter_by_typed_prefix(items, typed_suffix);
    }

    // If a verb is already typed (e.g., "add", "print"), only suggest
    // arguments — no more sub-menus or verbs.  This matches real RouterOS
    // terminal behavior where Tab after "add" shows property completions.
    if context.command.is_some() {
        return get_arg_completion_items(data, context);
    }

    // Before a verb: suggest sub-menus + standard verbs
    let mut items = Vec::new();
    items.extend(get_sub_menu_completion_items(data, context));
    items.extend(get_verb_completion_items(data, context));
    items
}

/// True when the cursor sits where a NEW statement may begin on the current
/// logical line: either nothing precedes it, or the previous token is exactly
/// `{` or `;` (a block opener / statement separator).
///
/// Token comparison is STRICT equality over quote-aware tokens
/// ([`crate::tokenize_with_spans`]), which is what keeps snippets out of
/// mid-command positions:
/// - `do={` is one token ≠ `{` → no snippets mid-command;
/// - `"…{…"` quoted braces never split into a `{` token;
/// - `x=1;` is one token ≠ `;` → no snippets after an inline separator that
///   still sits inside a larger token.
pub(crate) fn at_statement_start(before_cursor: &str) -> bool {
    match crate::tokenize_with_spans(before_cursor).last() {
        None => true,
        Some(last) => last.text == "{" || last.text == ";",
    }
}

/// [`at_statement_start`] evaluated on everything BEFORE the final partial
/// token.
///
/// Used while a `:`-prefixed word is being typed (`:`, `:i`, `:foreach`):
/// the word itself IS the statement being written, so "may a new statement
/// begin here?" applies to the tokens preceding it. Same strict token
/// equality rule as [`at_statement_start`] — only a bare `{` or `;` opens a
/// statement slot (`x=1; :put` stays mid-command, exactly like `x=1; `
/// does for the space-fired path).
fn at_statement_start_before_last_token(before_cursor: &str) -> bool {
    match crate::tokenize_with_spans(before_cursor).split_last() {
        None => true,
        Some((_, head)) => match head.last() {
            None => true,
            Some(prev) => prev.text == "{" || prev.text == ";",
        },
    }
}

/// The partial token under the cursor when it starts with `':'`.
///
/// "Under the cursor" means the request fired MID-token: trailing
/// whitespace says the previous token finished and a new one is starting,
/// which must stay an unfiltered completion case. Quote-aware tokenization
/// keeps quoted colons (`"a:b`) out of script-word territory.
fn colon_typed_token(before_cursor: &str) -> Option<String> {
    if before_cursor.ends_with(char::is_whitespace) {
        return None;
    }
    crate::tokenize_with_spans(before_cursor)
        .last()
        .filter(|t| t.text.starts_with(':'))
        .map(|t| t.text.clone())
}

/// One statement template: label, snippet body, one-line markdown docs.
struct StatementSnippet {
    label: &'static str,
    snippet: &'static str,
    doc: &'static str,
}

/// The four structural statement snippets, in offer order.
const STATEMENT_SNIPPETS: [StatementSnippet; 4] = [
    StatementSnippet {
        label: ":if",
        snippet: ":if (${1:condition}) do={\n\t${2}\n} else={\n\t${3}\n}$0",
        doc: "`:if` — conditional block with `do=` / `else=` branches.",
    },
    StatementSnippet {
        label: ":foreach",
        snippet: ":foreach ${1:i} in=[${2:find expression}] do={\n\t${3}\n}$0",
        doc: "`:foreach` — iterate over a list or `find` result.",
    },
    StatementSnippet {
        label: ":for",
        snippet: ":for ${1:i} from=${2:1} to=${3:10} do={\n\t${4}\n}$0",
        doc: "`:for` — counted loop from `from=` to `to=`.",
    },
    StatementSnippet {
        label: ":do",
        snippet: ":do {\n\t${1}\n} while=(${2:condition})$0",
        doc: "`:do` — run block once, repeat while `while=` holds.",
    },
];

/// Build the snippet completion items.
///
/// `sortText` "9…" ranks them below menu/argument suggestions ("0…"/"1…")
/// while staying deterministic; kind SNIPPET (15) + insertTextFormat Snippet(2)
/// tell clients to expand placeholders/tab stops instead of inserting literally.
fn statement_snippet_items() -> Vec<CompletionItem> {
    STATEMENT_SNIPPETS
        .iter()
        .map(|s| {
            let mut item = CompletionItem::new(s.label.to_string(), kind::SNIPPET);
            item.detail = Some("statement snippet".to_string());
            item.insert_text = Some(s.snippet.to_string());
            item.insert_text_format = Some(2); // Snippet
            item.sort_text = Some(format!("9{}", s.label));
            item.documentation = Some(Documentation {
                kind: "markdown",
                value: s.doc.to_string(),
            });
            item
        })
        .collect()
}

/// Case-insensitive prefix filter over candidate labels using the value text
/// the user already typed.
///
/// Surrounding quote characters on the typed suffix are ignored so partial
/// input like `chain="in` still filters to `input`. An empty effective
/// prefix returns the candidates unchanged; a non-empty prefix that matches
/// nothing also falls back to the full set (the client's own fuzzy matcher
/// remains the authority).
fn filter_by_typed_prefix(items: Vec<CompletionItem>, typed_suffix: &str) -> Vec<CompletionItem> {
    let trimmed = typed_suffix.trim_matches(|c| c == '"' || c == '\'');
    if trimmed.is_empty() {
        return items;
    }
    let lower = trimmed.to_ascii_lowercase();
    let matched: Vec<CompletionItem> = items
        .iter()
        .filter(|i| i.label.to_ascii_lowercase().starts_with(&lower))
        .cloned()
        .collect();
    if matched.is_empty() {
        // Nothing matches — fall back to the unfiltered set rather than
        // returning zero items for a typo'd prefix.
        items
    } else {
        matched
    }
}

// ── Root menus ──────────────────────────────────────────────────

fn get_root_completion_items(data: &MenuData) -> Vec<CompletionItem> {
    match data.child_names_by_parent.get("") {
        Some(roots) => roots
            .iter()
            .map(|r| {
                let mut item = CompletionItem::new(r.path.clone(), kind::CLASS);
                item.detail = Some(format!("root menu — {}", r.path));
                item.insert_text = Some(r.path.clone());
                item.insert_text_format = Some(1);
                item
            })
            .collect(),
        None => Vec::new(),
    }
}

// ── Sub-menus ───────────────────────────────────────────────────

fn get_sub_menu_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    match data.child_names_by_parent.get(&ctx.path) {
        Some(children) => children
            .iter()
            .filter(|c| c.menu_type == "Directory" || c.menu_type == "Settings Directory")
            .map(|c| {
                let mut item = CompletionItem::new(c.name.clone(), kind::CLASS);
                item.detail = Some(format!("sub-menu — {}", c.path));
                item.insert_text = Some(c.name.clone());
                item.insert_text_format = Some(1);
                item
            })
            .collect(),
        None => Vec::new(),
    }
}

// ── Verbs ───────────────────────────────────────────────────────

fn get_verb_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = MenuData::STANDARD_VERBS
        .iter()
        .map(|verb| {
            let mut item = CompletionItem::new(verb.to_string(), kind::FUNCTION);
            item.detail = Some(format!("{verb} — standard command"));
            item.insert_text = Some(verb.to_string());
            item.insert_text_format = Some(1);
            item
        })
        .collect();

    // Action commands (type = "Command" entries under this path)
    if let Some(children) = data.child_names_by_parent.get(&ctx.path) {
        for child in children {
            if child.menu_type == "Command" {
                let mut item = CompletionItem::new(child.name.clone(), kind::FUNCTION);
                item.detail = Some("action command".to_string());
                item.insert_text = Some(child.name.clone());
                item.insert_text_format = Some(1);
                items.push(item);
            }
        }
    }

    items
}

// ── Arguments ───────────────────────────────────────────────────

fn get_arg_completion_items(data: &MenuData, ctx: &LineContext) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&ctx.path) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    for arg in &menu.arguments {
        if ctx.properties.contains_key(&arg.name) {
            continue; // already used
        }
        let mut item = CompletionItem::new(arg.name.clone(), kind::PROPERTY);
        item.detail = Some(get_detail(arg));
        // Required properties sort before optional ones for the client
        // ("0…" < "1…"); other kinds keep the default (label) order by
        // leaving sortText unset.
        item.sort_text = Some(format!(
            "{}{}",
            if arg.required { "0" } else { "1" },
            arg.name
        ));
        item.documentation = documentation_from(arg.description.clone());
        item.insert_text = Some(get_insert_text(arg));
        item.insert_text_format = Some(2); // snippet
        items.push(item);
    }

    for flag in &menu.flags {
        let mut item = CompletionItem::new(flag.name.clone(), kind::CONSTANT);
        item.detail = Some(format!("{}: {}", flag.name, flag.description));
        item.documentation = documentation_from(flag.description.clone());
        item.insert_text = Some(flag.name.clone());
        item.insert_text_format = Some(1);
        items.push(item);
    }

    items
}

// ── Value completions (inside "property=value" tokens) ───────────

/// Honest placeholder values per argument type.
///
/// Only types with a universally valid representative get an item; anything
/// device-specific (interface names, script variables) returns zero items
/// rather than fabricated suggestions like `ether1`.
fn get_value_completions(
    data: &MenuData,
    ctx: &LineContext,
    property_key: &str,
) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&ctx.path) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let arg = match menu.arguments.iter().find(|a| a.name == property_key) {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    // Enum values — complete embedded list when present, display-string
    // fallback otherwise (synthetic/test data).
    if arg.arg_type.starts_with("enum") {
        for val in arg.enum_members() {
            let mut item = CompletionItem::new(val.clone(), kind::ENUM_MEMBER);
            item.detail = Some(format!("enum value — {}", arg.arg_type));
            // Values insert bare: a preceding opening quote in the token is
            // never doubled.
            item.insert_text = Some(val);
            item.insert_text_format = Some(1);
            items.push(item);
        }
    }

    // Boolean
    if arg.arg_type == "bool" || arg.arg_type == "boolean" {
        for val in ["yes", "no", "true", "false"] {
            let mut item = CompletionItem::new(val.to_string(), kind::ENUM_MEMBER);
            item.detail = Some("bool value".to_string());
            item.insert_text = Some(val.to_string());
            item.insert_text_format = Some(1);
            items.push(item);
        }
    }

    // IP address / prefix — one honest placeholder per actual type.
    if arg.arg_type.starts_with("ipPrefix") {
        items.push(ip_placeholder(arg, "0.0.0.0/0"));
    } else if arg.arg_type.starts_with("ipAddr") || arg.arg_type == "address" {
        items.push(ip_placeholder(arg, "0.0.0.0"));
    }

    items
}

fn ip_placeholder(arg: &ArgEntry, value: &str) -> CompletionItem {
    let mut item = CompletionItem::new(value.to_string(), kind::ENUM_MEMBER);
    item.detail = Some(format!("type: {}", arg.arg_type));
    item.insert_text = Some(value.to_string());
    item.insert_text_format = Some(1);
    item
}

// ── Helpers ─────────────────────────────────────────────────────

fn documentation_from(description: String) -> Option<Documentation> {
    if description.is_empty() {
        None
    } else {
        Some(Documentation {
            kind: "markdown",
            value: description,
        })
    }
}

/// Snippet for a property: `$1` on the value, `$0` as the final tabstop so
/// accepting the completion leaves the cursor at the end of the statement.
///
/// The quotes belong to THIS snippet (`comment="$1"$0`); value completions
/// never re-add them, so an already-typed opening quote is not doubled.
fn get_insert_text(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type == "string" {
        format!("{}=\"{}\"$0", arg.name, "$1")
    } else {
        format!("{}={}$0", arg.name, "$1")
    }
}

fn get_detail(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type.is_empty() {
        "property".to_string()
    } else {
        format!("type: {}", arg.arg_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::MenuData;

    fn synthetic_data() -> MenuData {
        let toml_str = r#"
[[menus]]
path = "/ip/address"
type = "Directory"

[[menus.arguments]]
name = "address"
type = "ipPrefix"
description = "The IP address"

[[menus.arguments]]
name = "interface"
type = "iface_enum"
description = "Interface name"

[[menus.arguments]]
name = "comment"
type = "string"
description = "Comment"

[[menus.flags]]
name = "X"
description = "disabled"

[[menus.flags]]
name = "D"
description = "dynamic"

[[menus]]
path = "/ip/route"
type = "Directory"

[[menus.arguments]]
name = "gateway"
type = "ipAddr"
description = "Gateway address"

[[menus]]
path = "/ip/route/check"
type = "Command"

[[menus]]
path = "/ip/firewall/filter"
type = "Directory"

[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
description = "Chain name"

[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
description = "Action"

[[menus.arguments]]
name = "enabled"
type = "bool"
description = "Enabled flag"

[[menus.arguments]]
name = "src-address"
type = "ipAddr"
description = "Source address"

[[menus]]
path = "/interface/bridge/port"
type = "Directory"

[[menus]]
path = "/system/clock"
type = "Directory"

[[menus.arguments]]
name = "enabled"
type = "bool"

[[menus.arguments]]
name = "time-zone-name"
type = "string"

[[menus]]
path = "/routing/bgp/connection"
type = "Directory"
"#;
        MenuData::from_toml_str(toml_str)
    }

    // ── Root completions ──────────────────────────────────────────

    #[test]
    fn test_root_completions_empty_input() {
        let data = synthetic_data();
        let items = compute_completions(&data, "");
        assert!(!items.is_empty(), "root completions should not be empty");
        assert!(items.iter().any(|i| i.label == "/ip"), "should contain /ip");
        assert!(
            items.iter().any(|i| i.label == "/interface"),
            "should contain /interface"
        );
        assert!(
            items.iter().any(|i| i.label == "/system"),
            "should contain /system"
        );
        // Root menus keep their CLASS kind and detail text…
        for item in items.iter().filter(|i| i.label.starts_with('/')) {
            assert_eq!(item.kind, Some(kind::CLASS));
            assert!(item.detail.as_ref().unwrap().contains("root menu"));
        }
        // …and statement-start snippets are appended on top of them.
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&":if"));
        assert!(labels.contains(&":foreach"));
        assert!(labels.contains(&":for"));
        assert!(labels.contains(&":do"));
    }

    #[test]
    fn test_root_completions_slash_only() {
        // "/" alone must behave like the empty context: parse_line maps it
        // to path "/" which has no child index entry, so compute_completions
        // special-cases it back to ROOT menu completions instead of verbs.
        //
        // Divergence since statement snippets exist: "" is a statement start
        // (nothing typed yet) so it additionally carries the four snippet
        // items; "/" is mid-path navigation (last token "/") so snippets are
        // withheld there. Root menus themselves must stay identical.
        let data = synthetic_data();
        let items_empty = compute_completions(&data, "");
        let items_slash = compute_completions(&data, "/");
        assert!(!items_slash.is_empty());
        let slash_labels: Vec<&str> = items_slash.iter().map(|i| i.label.as_str()).collect();
        assert!(slash_labels.contains(&"/ip"));
        assert!(slash_labels.contains(&"/interface"));
        assert!(slash_labels.contains(&"/system"));
        assert!(!slash_labels.contains(&":if"), "no snippets after '/'");
        // Same ROOT candidate set as the empty context, and NOT verb completions.
        let empty_roots: Vec<&str> = items_empty
            .iter()
            .map(|i| i.label.as_str())
            .filter(|l| l.starts_with('/'))
            .collect();
        assert_eq!(empty_roots, slash_labels);
        assert!(!slash_labels.contains(&"print"));
    }

    #[test]
    fn test_root_completions_are_only_roots() {
        let data = MenuData::load();
        let items = compute_completions(&data, "");
        // All MENU labels should start with / (snippet labels start with ':').
        for item in items.iter().filter(|i| i.label.starts_with('/')) {
            assert!(
                item.label.starts_with('/') && item.kind == Some(kind::CLASS),
                "root label should be a CLASS menu: {}",
                item.label
            );
        }
        // Snippets are the only non-menu additions at statement start.
        let extra: Vec<&str> = items
            .iter()
            .map(|i| i.label.as_str())
            .filter(|l| !l.starts_with('/'))
            .collect();
        assert_eq!(extra, vec![":if", ":foreach", ":for", ":do"]);
        // Should contain all 8 roots at least
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"/ip"));
        assert!(labels.contains(&"/interface"));
        assert!(labels.contains(&"/system"));
        assert!(labels.contains(&"/tool"));
        assert!(labels.contains(&"/queue"));
    }

    // ── Sub-menu completions ──────────────────────────────────────

    #[test]
    fn test_submenu_completions_for_ip() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip ");
        // Should contain sub-menus address, route
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"address"),
            "should contain address sub-menu"
        );
        assert!(labels.contains(&"route"), "should contain route sub-menu");
        // Also contains verb-like path? firewall is implicit? Check child_names_by_parent for /ip should have address, route, firewall
        assert!(
            labels.contains(&"firewall"),
            "should contain implicit firewall child"
        );
    }

    #[test]
    fn test_submenu_completions_include_verbs() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // When no verb yet, should contain both sub-menus (none for /ip/address) and verbs
        assert!(labels.contains(&"add"), "should contain verb add");
        assert!(labels.contains(&"print"), "should contain verb print");
        assert!(labels.contains(&"remove"), "should contain verb remove");
        // Check kind for verbs
        let add_item = items.iter().find(|i| i.label == "add").unwrap();
        assert_eq!(add_item.kind, Some(kind::FUNCTION));
    }

    #[test]
    fn test_submenu_for_unknown_path_returns_verbs_only() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/unknown/path ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        // No sub-menus, but verbs should still be present
        assert!(labels.contains(&"add"));
        assert!(labels.contains(&"print"));
        // No sub-menu specific labels
        assert!(!labels.contains(&"address"));
    }

    #[test]
    fn test_submenu_action_command_included_as_verb() {
        let data = synthetic_data();
        // /ip/route has child /ip/route/check of type Command
        let items = compute_completions(&data, "/ip/route ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"check"),
            "should include action command 'check'"
        );
        let check_item = items.iter().find(|i| i.label == "check").unwrap();
        assert_eq!(check_item.detail.as_deref(), Some("action command"));
    }

    // ── Argument completions (after verb) ─────────────────────────

    #[test]
    fn test_arg_completions_after_verb() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"address"), "should contain address arg");
        assert!(
            labels.contains(&"interface"),
            "should contain interface arg"
        );
        assert!(labels.contains(&"comment"), "should contain comment arg");
        // Should NOT contain verbs
        assert!(
            !labels.contains(&"print"),
            "should not contain verbs when command present"
        );
        assert!(
            !labels.contains(&"add"),
            "should not contain add when command already typed"
        );
        // Check kinds
        let addr_item = items.iter().find(|i| i.label == "address").unwrap();
        assert_eq!(addr_item.kind, Some(kind::PROPERTY));
        assert_eq!(addr_item.insert_text_format, Some(2));
        assert!(addr_item.detail.as_ref().unwrap().contains("ipPrefix"));
    }

    #[test]
    fn test_arg_completions_filter_used_properties() {
        let data = synthetic_data();
        // Already used address=1.1.1.1
        let items = compute_completions(&data, "/ip/address add address=1.1.1.1 ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            !labels.contains(&"address"),
            "already used prop should be filtered"
        );
        assert!(labels.contains(&"interface"), "unused prop should remain");
    }

    #[test]
    fn test_arg_completions_include_flags() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"X"), "should contain flag X");
        assert!(labels.contains(&"D"), "should contain flag D");
        let flag_item = items.iter().find(|i| i.label == "X").unwrap();
        assert_eq!(flag_item.kind, Some(kind::CONSTANT));
    }

    #[test]
    fn test_arg_completions_string_type_has_quoted_insert() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add ");
        let comment_item = items.iter().find(|i| i.label == "comment").unwrap();
        let insert = comment_item.insert_text.as_ref().unwrap();
        assert!(
            insert.contains('"'),
            "string type should have quoted insert_text"
        );
        assert!(insert.contains("$1"), "should be snippet with $1");
    }

    #[test]
    fn test_arg_completions_non_string_type_plain_insert() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add ");
        let addr_item = items.iter().find(|i| i.label == "address").unwrap();
        let insert = addr_item.insert_text.as_ref().unwrap();
        assert_eq!(insert, "address=$1$0");
    }

    #[test]
    fn test_arg_completions_unknown_menu_returns_empty() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/unknown/path add ");
        assert!(items.is_empty(), "unknown menu should return no args");
    }

    // ── Value completions (after "property=") ──────────────────────

    #[test]
    fn test_value_completions_enum_chain() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"input"));
        assert!(labels.contains(&"forward"));
        assert!(labels.contains(&"output"));
        for item in &items {
            assert_eq!(item.kind, Some(kind::ENUM_MEMBER));
        }
    }

    #[test]
    fn test_value_completions_enum_action() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/firewall/filter add action=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"accept"));
        assert!(labels.contains(&"drop"));
        assert!(labels.contains(&"reject"));
    }

    #[test]
    fn test_value_completions_bool() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/firewall/filter add enabled=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"yes"));
        assert!(labels.contains(&"no"));
        assert!(labels.contains(&"true"));
        assert!(labels.contains(&"false"));
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn test_value_completions_bool_system_clock() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/system/clock set enabled=");
        assert!(!items.is_empty());
        assert!(items.iter().any(|i| i.label == "yes"));
    }

    #[test]
    fn test_value_completions_iface_enum_zero_items() {
        // Honest placeholders: interface names are device-specific, so an
        // iface_enum property yields ZERO items rather than fabricated
        // suggestions like ether1/bridge.
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add interface=");
        assert!(
            items.is_empty(),
            "iface_enum should produce no fabricated items, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_value_completions_ipaddr() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add address=");
        // address is ipPrefix -> prefix placeholder
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["0.0.0.0/0"]);
    }

    #[test]
    fn test_value_completions_ipaddr_src_address() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/firewall/filter add src-address=");
        // src-address is ipAddr (NOT ipPrefix) -> host-address placeholder,
        // distinct from the prefix placeholder.
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["0.0.0.0"]);
    }

    #[test]
    fn test_value_completions_unknown_property_empty() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/ip/address add unknownprop=");
        assert!(
            items.is_empty(),
            "unknown property should return empty value completions"
        );
    }

    #[test]
    fn test_value_completions_unknown_menu_empty() {
        let data = synthetic_data();
        let items = compute_completions(&data, "/unknown add prop=");
        assert!(items.is_empty());
    }

    #[test]
    fn test_value_completions_with_space_before_equals_not_triggered() {
        let data = synthetic_data();
        // last_token is "chain" not "chain=" -> should be arg completions, not value
        let items = compute_completions(&data, "/ip/firewall/filter add chain");
        // Should be arg completions (not value), so labels contain property names not enum values
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"chain"), "should be arg completions");
        assert!(
            !labels.contains(&"input"),
            "should not be value completions"
        );
    }

    // ── Helpers ───────────────────────────────────────────────────

    use crate::menus::parse_enum_values;

    #[test]
    fn test_parse_enum_values_normal() {
        let vals = parse_enum_values("enum (input | forward | output)");
        assert_eq!(vals, vec!["input", "forward", "output"]);
    }

    #[test]
    fn test_parse_enum_values_with_spaces() {
        let vals = parse_enum_values("enum (  a  |  b  |c )");
        assert_eq!(vals, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_enum_values_malformed_no_parens() {
        let vals = parse_enum_values("enum input | output");
        assert!(vals.is_empty());
    }

    #[test]
    fn test_parse_enum_values_empty() {
        let vals = parse_enum_values("enum ()");
        assert_eq!(vals, vec![""]);
    }

    #[test]
    fn test_parse_enum_values_not_enum() {
        let vals = parse_enum_values("bool");
        assert!(vals.is_empty());
    }

    #[test]
    fn test_get_insert_text_string() {
        let arg = crate::menus::ArgEntry {
            name: "comment".to_string(),
            arg_type: "string".to_string(),
            enum_values: Vec::new(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_insert_text(&arg), "comment=\"$1\"$0");
    }

    #[test]
    fn test_get_insert_text_non_string() {
        let arg = crate::menus::ArgEntry {
            name: "address".to_string(),
            arg_type: "ipPrefix".to_string(),
            enum_values: Vec::new(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_insert_text(&arg), "address=$1$0");
    }

    #[test]
    fn test_get_detail_empty_type() {
        let arg = crate::menus::ArgEntry {
            name: "foo".to_string(),
            arg_type: "".to_string(),
            enum_values: Vec::new(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_detail(&arg), "property");
    }

    #[test]
    fn test_get_detail_with_type() {
        let arg = crate::menus::ArgEntry {
            name: "foo".to_string(),
            arg_type: "bool".to_string(),
            enum_values: Vec::new(),
            description: "".to_string(),
            required: false,
            unset: false,
        };
        assert_eq!(get_detail(&arg), "type: bool");
    }

    // ── Real data sanity checks ───────────────────────────────────

    #[test]
    fn test_real_data_arg_completions_ip_address() {
        let data = MenuData::load();
        let items = compute_completions(&data, "/ip/address add ");
        assert!(!items.is_empty());
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"address"));
        assert!(labels.contains(&"interface"));
    }

    #[test]
    fn test_real_data_value_completions_chain_and_action() {
        // Real embedded data: `chain` is a bare "enum" upstream (chains are
        // user-definable) → no members, no items. `action` embeds the
        // complete member list extracted from the raw docs type string, so
        // value completions work even though its display type is truncated.
        let data = MenuData::load();
        let chain_items = compute_completions(&data, "/ip/firewall/filter add chain=");
        assert!(chain_items.is_empty(), "chain has no documented members");

        let action_items = compute_completions(&data, "/ip/firewall/filter add action=");
        assert!(
            !action_items.is_empty(),
            "action should complete via embedded enum_values"
        );
        let labels: Vec<&str> = action_items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"accept"));
        assert!(labels.contains(&"drop"));
    }
}

#[cfg(test)]
mod extra_coverage {
    use super::*;
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
        // Menus keep CLASS kind; snippets (kind SNIPPET) are appended at
        // statement start since B3.
        for it in items.iter().filter(|i| i.label.starts_with('/')) {
            assert!(it.label.starts_with('/'), "root label must start with /");
            assert_eq!(it.kind, Some(kind::CLASS));
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

    // ── Values after = ────────────────────────────────────────────────

    #[test]
    fn test_values_after_equals_enum() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"input"));
        assert!(labels.contains(&"forward"));
        assert!(labels.contains(&"output"));
        assert!(items.iter().all(|i| i.kind == Some(kind::ENUM_MEMBER)));
    }

    #[test]
    fn test_values_after_equals_bool() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add enabled=");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(items.len(), 4);
        assert!(labels.contains(&"yes"));
        assert!(labels.contains(&"no"));
        assert!(labels.contains(&"true"));
        assert!(labels.contains(&"false"));
    }

    #[test]
    fn test_values_after_equals_iface_enum_zero_items() {
        // Honest placeholders: iface_enum yields nothing device-specific.
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add interface=");
        assert!(items.is_empty());
    }

    #[test]
    fn test_values_after_equals_ip_prefix() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add address=");
        assert!(items.iter().any(|i| i.label == "0.0.0.0/0"));
        assert!(items[0].detail.as_ref().unwrap().contains("ipPrefix"));
    }

    #[test]
    fn test_values_after_equals_ip_addr() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/route add gateway=");
        // ipAddr gets the HOST placeholder, distinct from ipPrefix's /0 form.
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["0.0.0.0"]);
        let det = items[0].detail.as_ref().unwrap();
        assert!(det.contains("ipAddr"));
    }

    #[test]
    fn test_values_after_equals_requires_trailing_equals() {
        let data = synthetic();
        // Without "=", should be arg completions, not value
        let arg_items = compute_completions(&data, "/ip/firewall/filter add chain");
        assert!(arg_items.iter().any(|i| i.label == "chain"));
        assert!(!arg_items.iter().any(|i| i.label == "input"));
        // With "=", should be value completions
        let val_items = compute_completions(&data, "/ip/firewall/filter add chain=");
        assert!(val_items.iter().any(|i| i.label == "input"));
        assert!(!val_items.iter().any(|i| i.label == "chain"));
    }

    #[test]
    fn test_values_after_equals_unknown_property_empty() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/address add unknown=");
        assert!(items.is_empty());
    }

    #[test]
    fn test_values_after_equals_unknown_menu_empty() {
        let data = synthetic();
        let items = compute_completions(&data, "/unknown add prop=");
        assert!(items.is_empty());
    }

    #[test]
    fn test_values_after_equals_bool_also_triggers_iface_check_independent() {
        // Ensure bool and iface_enum are independent: a bool prop should not get iface values
        let data = synthetic();
        let bool_items = compute_completions(&data, "/system/clock set enabled=");
        // enabled is bool -> should have yes/no/true/false but not ether1
        assert!(bool_items.iter().any(|i| i.label == "yes"));
        assert!(!bool_items.iter().any(|i| i.label == "ether1"));
    }

    #[test]
    fn test_completion_deterministic_no_panic_on_weird_input() {
        let data = synthetic();
        let weird = [
            "",
            " ",
            "/",
            "/ ",
            "///",
            "add",
            "===",
            "address===",
            "/ip/address add address= a",
            "/ip/address add \"comment=\"",
        ];
        for w in weird {
            let items = compute_completions(&data, w);
            // Should not panic, and result is Vec (maybe empty)
            let _ = items.len();
        }
    }

    #[test]
    fn test_completion_with_real_data_smoke() {
        let data = MenuData::load();
        let cases = [
            "",
            "/",
            "/ip ",
            "/ip/address ",
            "/ip/address add ",
            "/ip/address add address=",
            "/ip/firewall/filter add chain=",
            "/system/clock set enabled=",
        ];
        for c in cases {
            let items = compute_completions(&data, c);
            // Ensure no panic and items is vec
            assert!(items.len() < 10000, "unexpected huge completion for {c}");
        }
    }

    // ── Partial value completion ("token contains =") ─────────────────

    #[test]
    fn test_partial_value_prefix_filters_enum() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=in");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["input"], "only 'input' matches prefix 'in'");
    }

    #[test]
    fn test_partial_value_prefix_case_insensitive() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=IN");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["input"]);
    }

    #[test]
    fn test_partial_value_no_match_falls_back_to_unfiltered() {
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=zzz");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"input") && labels.contains(&"forward") && labels.contains(&"output"),
            "non-matching non-empty prefix must fall back to the full set"
        );
    }

    #[test]
    fn test_partial_value_opening_quote_stripped_from_prefix() {
        let data = synthetic();
        // Token ends inside an opened quote: the quote char must not break
        // the case-insensitive prefix filter…
        let items = compute_completions(&data, "/ip/firewall/filter add chain=\"in");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["input"]);

        // …and accepting an item inserts the BARE value so the already-typed
        // opening quote is never doubled.
        assert_eq!(items[0].insert_text.as_deref(), Some("input"));
        let after_quote = compute_completions(&data, "/ip/firewall/filter add chain=\"");
        assert_eq!(after_quote.len(), 3, "empty effective prefix → unfiltered");
        assert!(
            after_quote
                .iter()
                .all(|i| !i.insert_text.as_deref().unwrap_or("").contains('"')),
            "value inserts stay quote-free"
        );
    }

    #[test]
    fn test_partial_value_chain_in_suggests_values_not_args() {
        // Regression guard for the exact scenario in the spec: `chain=in`
        // must suggest chain VALUES, not the argument list.
        let data = synthetic();
        let items = compute_completions(&data, "/ip/firewall/filter add chain=in");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"input"));
        assert!(
            !labels.contains(&"action"),
            "must not be argument completions"
        );
        assert!(!labels.contains(&"enabled"));
    }

    #[test]
    fn test_trailing_space_after_value_stays_argument_completion() {
        let data = synthetic();
        // Cursor AFTER the finished token: value branch must NOT trigger.
        let items = compute_completions(&data, "/ip/firewall/filter add chain=input ");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"action"),
            "finished value + space → next property suggestions"
        );
        assert!(
            !labels.contains(&"forward"),
            "must not be value completions"
        );
        // The used property is filtered out of the argument list.
        assert!(!labels.contains(&"chain"));
    }

    // ── documentation on completion items ─────────────────────────────

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

    // ── sortText: required before optional ────────────────────────────

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
    fn test_sorttext_absent_for_non_property_kinds() {
        let data = synthetic();
        // Verbs and sub-menus keep default ordering (no sortText).
        let verbs = compute_completions(&data, "/ip/address ");
        let add = verbs.iter().find(|i| i.label == "add").unwrap();
        assert!(add.sort_text.is_none());
        // Flags are CONSTANT kind — default order too.
        let args = compute_completions(&data, "/ip/address add ");
        let flag = args.iter().find(|i| i.label == "X").unwrap();
        assert!(flag.sort_text.is_none());
    }

    // ── Root trigger variants ─────────────────────────────────────────

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

    // ── Statement-start snippets (B3) ─────────────────────────────

    const SNIPPET_LABELS: [&str; 4] = [":if", ":foreach", ":for", ":do"];

    fn snippet_items<'a>(items: &'a [CompletionItem]) -> Vec<&'a CompletionItem> {
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

    // ── ':' trigger character ──────────────────────────────────────

    #[test]
    fn test_colon_bare_at_statement_start_returns_only_colon_items() {
        let data = synthetic();
        // ':' alone at a fresh statement fires mid-token: the four
        // statement snippets are the colon-prefixed candidates today, and
        // NOTHING else (no root menus, no verbs) may leak into the menu.
        let items = compute_completions(&data, ":");
        assert_eq!(
            items.len(),
            4,
            "got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        for i in &items {
            assert!(
                i.label.starts_with(':'),
                "only ':'-prefixed labels allowed, got {}",
                i.label
            );
        }
        assert_eq!(snippet_items(&items).len(), 4);
    }

    #[test]
    fn test_colon_prefix_filters_to_matching_script_items() {
        let data = synthetic();
        // ':i' narrows to :if …
        let items = compute_completions(&data, ":i");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec![":if"]);
        // …':fo' keeps :foreach and :for in offer-table order…
        let items = compute_completions(&data, ":fo");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec![":foreach", ":for"]);
        // …and a fully typed unknown script word completes to NOTHING — no
        // fallback to menu noise after a colon.
        let items = compute_completions(&data, ":put");
        assert!(
            items.is_empty(),
            "no fallback after ':put', got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_colon_after_block_opener_still_offers_snippets() {
        let data = synthetic();
        // '{ :' — the brace opened the block; the colon word begins the
        // next statement inside it.
        let items = compute_completions(&data, "{ :");
        assert_eq!(snippet_items(&items).len(), 4);
        for i in &items {
            assert!(i.label.starts_with(':'), "colon context leaked {}", i.label);
        }
    }

    #[test]
    fn test_colon_mid_statement_returns_no_menu_noise() {
        let data = synthetic();
        // A colon word after a verb is NOT a statement start: no snippets,
        // and filtering the argument names leaves an intentionally quiet
        // (empty) result instead of irrelevant property suggestions.
        let items = compute_completions(&data, "/ip/address print :");
        assert!(items.is_empty());
    }

    #[test]
    fn test_trailing_space_after_colon_not_filtered() {
        let data = synthetic();
        // ':' finished with a space starts a NEW (empty) token — that
        // request is not a script-word completion and must behave exactly
        // as before the ':' trigger existed: plain root completions, no
        // snippets (a lone ':' is not a `{`/`;` opener).
        let items = compute_completions(&data, ": ");
        assert!(
            items.iter().any(|i| i.label == "/ip"),
            "finished ':' token keeps ordinary root completions"
        );
        assert_eq!(snippet_items(&items).len(), 0);
    }

    #[test]
    fn test_quoted_colon_is_not_script_word_context() {
        let data = synthetic();
        // A quote opens this token, so it never enters the ':' branch:
        // root menus flow through unfiltered.
        let items = compute_completions(&data, "\"a:b");
        assert!(
            items.iter().any(|i| i.label == "/ip"),
            "quoted colon must not trigger script-word filtering"
        );
    }

    #[test]
    fn test_non_colon_contexts_unchanged_by_colon_trigger() {
        let data = synthetic();
        // Roots + snippets at a plain statement start…
        let empty = compute_completions(&data, "");
        assert!(empty.iter().any(|i| i.label == "/ip"));
        assert_eq!(snippet_items(&empty).len(), 4);
        // …arguments after a verb…
        let args = compute_completions(&data, "/ip/address add ");
        assert!(args.iter().any(|i| i.label == "address"));
        assert!(!args.is_empty());
        // …values after '='…
        let vals = compute_completions(&data, "/ip/firewall/filter add chain=in");
        assert_eq!(vals.len(), 1);
        // …and root navigation via '/'.
        let slash = compute_completions(&data, "/");
        assert!(slash.iter().any(|i| i.label == "/ip"));
    }
}
