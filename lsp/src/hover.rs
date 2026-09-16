// ── Hover logic for the RSC language server ──────────────────────────────
//
// When the user hovers over a word, check:
// 1. Is it a menu path (starts with /)?
// 2. Is it a property name for the current menu?
// 3. Is it a standard RouterOS verb?

use crate::menus::{MenuData, MenuEntry};
// Shared text helpers live in `crate::text_util` (single owner); the
// re-exports below keep historical `hover::` paths resolving for tests.
pub(crate) use crate::text_util::{MAX_HOVER_PROPERTIES, sanitize_markdown_for_hover};
use crate::text_util::{normalize_key, normalize_path, truncate_chars, type_gloss, verb_role};

/// Max chars for one argument description inside a menu hover card.
///
/// Feature-local micro-cap: keeps each `- **name** `type`` bullet on one
/// markdown line while the section stays bounded by
/// [`MAX_HOVER_PROPERTIES`].
const MAX_MENU_ARG_DESC_CHARS: usize = 120;

/// Single-line argument description for a menu hover bullet.
///
/// Sanitizes via [`sanitize_markdown_for_hover`], flattens to one line
/// (no wire break inside the bullet), then caps at
/// [`MAX_MENU_ARG_DESC_CHARS`]. Empty upstream text stays empty so the
/// caller keeps the bare `- **name** `type`` fallback.
fn menu_arg_suffix(description: &str) -> String {
    if description.is_empty() {
        return String::new();
    }
    let clean = sanitize_markdown_for_hover(description);
    let single = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.is_empty() {
        return String::new();
    }
    let short = truncate_chars(&single, MAX_MENU_ARG_DESC_CHARS);
    format!(" — {short}")
}

/// Case/separator-insensitive menu lookup.
///
/// RouterOS paths are case-insensitive and tolerate leading, trailing and
/// repeated `/`; the dataset keys are lowercase and canonical, so the query
/// passes through [`normalize_path`] (the original text stays in the hover
/// card).
fn find_menu<'a>(data: &'a MenuData, path: &str) -> Option<&'a MenuEntry> {
    data.menu_by_path.get(&normalize_path(path))
}

/// Example value line for the most common scalar types.
fn example_for(arg_type: &str) -> Option<&'static str> {
    if arg_type.starts_with("ipPrefix") {
        Some("Example: `192.168.1.1/24`")
    } else if arg_type.starts_with("ipAddr") || arg_type == "address" {
        Some("Example: `192.168.1.1`")
    } else if arg_type == "bool" || arg_type == "boolean" {
        Some("Example: `yes`")
    } else {
        None
    }
}

/// Find word start (including /, -, _).
///
/// `pub(crate)` so navigation (`word_at`) extracts words with the EXACT
/// same rules as hover — go-to-definition, find-references, and hover must
/// never disagree about what "the word at the cursor" is.
pub(crate) fn find_word_start(line: &str, pos: usize) -> usize {
    let pos = pos.min(line.len());
    // Ensure we are at a valid character boundary (RSC is ASCII, but be safe).
    let pos = crate::floor_char_boundary(line, pos);
    let mut i = pos;
    while i > 0 {
        let ch = line.as_bytes()[i - 1] as char;
        if !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' {
            break;
        }
        i -= 1;
    }
    i
}

/// Find word end (including /, -, _) — shared with navigation, see
/// [`find_word_start`].
pub(crate) fn find_word_end(line: &str, pos: usize) -> usize {
    let pos = pos.min(line.len());
    let pos = crate::floor_char_boundary(line, pos);
    let mut i = pos;
    while i < line.len() {
        let ch = line.as_bytes()[i] as char;
        if !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' {
            break;
        }
        i += 1;
    }
    i
}

#[derive(serde::Serialize)]
pub struct HoverContents {
    pub kind: String,
    pub value: String,
}

#[derive(serde::Serialize)]
pub struct Hover {
    pub contents: HoverContents,
}

/// One-line help for `:`-prefixed script keywords (`:put`, `:if`, ...).
/// `find_word_start` deliberately excludes `:` so word extraction stays
/// shared with navigation; hover re-attaches the prefix locally instead.
fn colon_builtin_doc(colon_word: &str) -> Option<&'static str> {
    if colon_word.eq_ignore_ascii_case(":put") {
        Some("`:put <value> — output values to the console.")
    } else if colon_word.eq_ignore_ascii_case(":if") {
        Some("`:if (<cond>) do={...} — conditional execution.")
    } else if colon_word.eq_ignore_ascii_case(":foreach") {
        Some("`:foreach <var> in=<list> do={...} — iterate over a list.")
    } else if colon_word.eq_ignore_ascii_case(":for") {
        Some("`:for <var> from=<n> to=<m> do={...} — counted loop.")
    } else if colon_word.eq_ignore_ascii_case(":do") {
        Some("`:do {...} while=(<cond>) — group commands.")
    } else if colon_word.eq_ignore_ascii_case(":local") {
        Some("`:local <name> [<value>] — declare a local variable.")
    } else if colon_word.eq_ignore_ascii_case(":global") {
        Some("`:global <name> [<value>] — declare or access a global variable.")
    } else if colon_word.eq_ignore_ascii_case(":delay") {
        Some("`:delay <seconds> — pause execution.")
    } else if colon_word.eq_ignore_ascii_case(":error") {
        Some("`:error <message> — raise a script error.")
    } else if colon_word.eq_ignore_ascii_case(":return") {
        Some("`:return [<value>] — return a value from a script.")
    } else if colon_word.eq_ignore_ascii_case(":resolve") {
        Some("`:resolve <host> — resolve a DNS name to an address.")
    } else if colon_word.eq_ignore_ascii_case(":parse") {
        Some("`:parse <text> — parse console commands from text.")
    } else if colon_word.eq_ignore_ascii_case(":pick") {
        Some("`:pick <value> <start> [<end>] — slice a string or array.")
    } else if colon_word.eq_ignore_ascii_case(":tonum") {
        Some("`:tonum <value> — convert a value to a number.")
    } else if colon_word.eq_ignore_ascii_case(":totime") {
        Some("`:totime <value> — convert a value to a time interval.")
    } else {
        None
    }
}

pub fn compute_hover(
    data: &MenuData,
    line: &str,
    // Byte offset within `line`, already converted from the negotiated wire
    // encoding by the caller at the protocol boundary.
    character: usize,
    full_doc: &str,
    cursor_line: usize,
) -> Option<Hover> {
    let word_start = find_word_start(line, character);
    let word_end = find_word_end(line, character);
    let word = &line[word_start..word_end];
    if word.is_empty() {
        return None;
    }
    // Re-include a leading `:` without touching `find_word_start`.
    let colon_word: Option<String> =
        if word_start > 0 && line.as_bytes().get(word_start - 1) == Some(&b':') {
            Some(format!(":{word}"))
        } else {
            None
        };

    // Check if it's a menu path (case-insensitive; display keeps typed casing)
    // let chains (requires Rust 1.88+, MSRV is 1.94) — collapsed for clippy collapsible_if
    if word.starts_with('/')
        && let Some(menu) = find_menu(data, word)
    {
        let mut md = format!(
            "### {}\n\n**Type:** {}",
            word,
            if menu.menu_type.is_empty() {
                "Directory"
            } else {
                &menu.menu_type
            }
        );

        if !menu.arguments.is_empty() {
            // Required entries render first under their own block, then
            // optional ones; the shared display cap applies across both so
            // the card stays bounded.
            let mut required: Vec<_> = menu.arguments.iter().filter(|a| a.required).collect();
            let mut optional: Vec<_> = menu.arguments.iter().filter(|a| !a.required).collect();
            required.sort_by(|a, b| a.name.cmp(&b.name));
            optional.sort_by(|a, b| a.name.cmp(&b.name));
            let total = required.len() + optional.len();
            let shown_required: Vec<_> = required.into_iter().take(MAX_HOVER_PROPERTIES).collect();
            let rest = MAX_HOVER_PROPERTIES.saturating_sub(shown_required.len());
            let shown_optional: Vec<_> = optional.into_iter().take(rest).collect();
            let bullet = |arg: &crate::menus::ArgEntry| {
                let typ = if arg.arg_type.is_empty() {
                    "any"
                } else {
                    arg.arg_type.as_str()
                };
                let req = if arg.required { " (required)" } else { "" };
                let suffix = menu_arg_suffix(&arg.description);
                format!("\n- **{}** `{}`{}{}", arg.name, typ, req, suffix)
            };
            md.push_str("\n\n**Arguments:**");
            if !shown_required.is_empty() {
                md.push_str("\n\n**Required:**");
                for arg in shown_required {
                    md.push_str(&bullet(arg));
                }
            }
            if !shown_optional.is_empty() {
                md.push_str("\n\n**Optional:**");
                for arg in shown_optional {
                    md.push_str(&bullet(arg));
                }
            }
            if total > MAX_HOVER_PROPERTIES {
                md.push_str(&format!(
                    "\n\n(+{} more — type Space after verb to list)",
                    total - MAX_HOVER_PROPERTIES
                ));
            }
        }

        if !menu.flags.is_empty() {
            md.push_str("\n\n**Flags:**");
            for flag in menu.flags.iter().take(MAX_HOVER_PROPERTIES) {
                let desc = if flag.description.is_empty() {
                    String::new()
                } else {
                    sanitize_markdown_for_hover(&flag.description)
                };
                md.push_str(&format!("\n  {} — {}", flag.name, desc));
            }
            if menu.flags.len() > MAX_HOVER_PROPERTIES {
                md.push_str(&format!(
                    "\n\n(+{} more — type Space after verb to list)",
                    menu.flags.len() - MAX_HOVER_PROPERTIES
                ));
            }
        }

        if !menu.read_only.is_empty() {
            md.push_str("\n\n**Read-only:**");
            for ro in menu.read_only.iter().take(MAX_HOVER_PROPERTIES) {
                let desc = if ro.description.is_empty() {
                    String::new()
                } else {
                    sanitize_markdown_for_hover(&ro.description)
                };
                md.push_str(&format!("\n  {} — {}", ro.name, desc));
            }
            if menu.read_only.len() > MAX_HOVER_PROPERTIES {
                md.push_str(&format!(
                    "\n\n(+{} more — type Space after verb to list)",
                    menu.read_only.len() - MAX_HOVER_PROPERTIES
                ));
            }
        }

        md.push_str("\n\nSource: published reference");

        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: md,
            },
        });
    }

    // Check if it's a property name for the current menu.
    // Rebuild context from the full document at the cursor position so that
    // multiline commands (properties on next line) are correctly resolved.
    let before_cursor = crate::build_before_cursor(full_doc, cursor_line, character);
    let context = crate::parse_line(data, &before_cursor);

    if let Some(menu) = find_menu(data, &context.path) {
        if let Some(arg) = menu
            .arguments
            .iter()
            .find(|a| normalize_key(&a.name) == normalize_key(word))
        {
            let typ = if arg.arg_type.is_empty() {
                "any"
            } else {
                &arg.arg_type
            };
            let mut md = format!("**{}**\n\nType: `{}`", arg.name, typ);
            if let Some(gloss) = type_gloss(&arg.arg_type) {
                md.push_str(&format!(" — {gloss}"));
            }
            // Context line: which command this property belongs to.
            let ctx_line = match context.command.as_deref() {
                Some(verb) => {
                    let req = if arg.required { " · (required)" } else { "" };
                    format!("\n\nin `{} {verb}`{req}", menu.path)
                }
                None => format!("\n\nin `{}`", menu.path),
            };
            md.push_str(&ctx_line);
            if !arg.enum_values.is_empty() {
                md.push_str(&format!("\n\nValues: {}", arg.enum_values.join(" | ")));
            }
            if !arg.description.is_empty() {
                md.push_str(&format!(
                    "\n\n{}",
                    sanitize_markdown_for_hover(&arg.description)
                ));
            }
            if let Some(ex) = example_for(&arg.arg_type) {
                md.push_str(&format!("\n\n{ex}"));
            }
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: md,
                },
            });
        }
        if let Some(flag) = menu
            .flags
            .iter()
            .find(|f| normalize_key(&f.name) == normalize_key(word))
        {
            // Hygiene: ~28% flags have empty description upstream; fallback to type so card never
            // empty
            let md = if flag.description.is_empty() {
                if flag.arg_type.is_empty() {
                    format!("**{}**", flag.name)
                } else {
                    format!("**{}**\n\nType: `{}`", flag.name, flag.arg_type)
                }
            } else {
                format!(
                    "**{}**\n\n{}",
                    flag.name,
                    sanitize_markdown_for_hover(&flag.description)
                )
            };
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: md,
                },
            });
        }
    }

    // Check if it's a standard verb (case-insensitive: RouterOS verbs are case-insensitive)
    if MenuData::STANDARD_VERBS
        .iter()
        .any(|v| word.eq_ignore_ascii_case(v))
    {
        let role = verb_role(word);
        let mut sentence = role.to_string();
        if let Some(first) = sentence.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let md = format!("**{word}**\n\n{sentence}.");
        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: md,
            },
        });
    }

    // `:`-prefixed script keywords (`:put`, `:if`, ...). The extracted `word`
    // excludes the colon by design; `colon_word` above re-attaches it locally.
    if let Some(cw) = colon_word {
        if let Some(doc) = colon_builtin_doc(&cw) {
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: format!("**{cw}**\n\n{doc}"),
                },
            });
        }
        // Fallback for other `:keyword` forms: still a script command.
        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: format!("**{cw}**\n\nScript command."),
            },
        });
    }

    None
}
