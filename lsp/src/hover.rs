// ── Hover logic for the RSC language server ──────────────────────────────
//
// When the user hovers over a word, check:
// 1. Is it a value token of a `name=value` property?
// 2. Is it a menu path (starts with /)?
// 3. Is it a property name for the current menu?
// 4. Is it a standard RouterOS verb?

use crate::menus::{ArgEntry, LineContext, MenuData, MenuEntry};
// Shared text helpers live in `crate::text_util` (single owner); the
// re-exports below keep historical `hover::` paths resolving for tests.
pub(crate) use crate::text_util::{MAX_HOVER_PROPERTIES, sanitize_markdown_for_hover};
use crate::text_util::{
    normalize_key, normalize_path, sanitize_markdown_for_hover_with_truncation, truncate_chars,
    type_gloss, verb_role,
};

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

/// One-line provenance footer shared by every hover card.
///
/// The embedded dataset's RouterOS version comes from the generated
/// `data/commands.toml` header via [`crate::menus::dataset_provenance_cached`]
/// (cached process-wide: the table is immutable).
fn hover_source_line() -> String {
    let prov = crate::menus::dataset_provenance_cached();
    format!(
        "\n\nSource: published reference — RouterOS {}",
        prov.version
    )
}

/// Render the menu card for `menu`, headed by `display`.
///
/// Section budgets are applied per section (`arguments`, `flags`,
/// `read-only`), each with its own footer naming exactly what that section
/// hid — see `MAX_HOVER_PROPERTIES`. Flags are completable, so their footer
/// points at completion; read-only columns are not, so their footer says so
/// without promising a Space-triggered list.
fn menu_hover_card(display: &str, menu: &MenuEntry) -> Hover {
    let mut md = format!(
        "### {}\n\n**Type:** {}",
        display,
        if menu.menu_type.is_empty() {
            "Directory"
        } else {
            &menu.menu_type
        }
    );

    if !menu.arguments.is_empty() {
        // Required entries render first under their own block, then
        // optional ones; the shared per-section cap applies across both so
        // the card stays bounded.
        let mut required: Vec<_> = menu.arguments.iter().filter(|a| a.required).collect();
        let mut optional: Vec<_> = menu.arguments.iter().filter(|a| !a.required).collect();
        required.sort_by(|a, b| a.name.cmp(&b.name));
        optional.sort_by(|a, b| a.name.cmp(&b.name));
        let total = required.len() + optional.len();
        let shown_required: Vec<_> = required.into_iter().take(MAX_HOVER_PROPERTIES).collect();
        let rest = MAX_HOVER_PROPERTIES.saturating_sub(shown_required.len());
        let shown_optional: Vec<_> = optional.into_iter().take(rest).collect();
        let shown = shown_required.len() + shown_optional.len();
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
        if total > shown {
            md.push_str(&format!(
                "\n\n(+{} more — type Space after verb to list)",
                total - shown
            ));
        }
    }

    if !menu.flags.is_empty() {
        let shown = menu.flags.len().min(MAX_HOVER_PROPERTIES);
        md.push_str("\n\n**Flags:**");
        for flag in menu.flags.iter().take(shown) {
            let desc = if flag.description.is_empty() {
                String::new()
            } else {
                sanitize_markdown_for_hover(&flag.description)
            };
            md.push_str(&format!("\n  {} — {}", flag.name, desc));
        }
        if menu.flags.len() > shown {
            md.push_str(&format!(
                "\n\n(+{} more — type Space after verb to list)",
                menu.flags.len() - shown
            ));
        }
    }

    if !menu.read_only.is_empty() {
        let shown = menu.read_only.len().min(MAX_HOVER_PROPERTIES);
        md.push_str("\n\n**Read-only:**");
        for ro in menu.read_only.iter().take(shown) {
            let desc = if ro.description.is_empty() {
                String::new()
            } else {
                sanitize_markdown_for_hover(&ro.description)
            };
            md.push_str(&format!("\n  {} — {}", ro.name, desc));
        }
        if menu.read_only.len() > shown {
            // Read-only columns are never offered by completion, so the
            // footer must not promise a Space-triggered list.
            md.push_str(&format!(
                "\n\n(+{} more read-only columns)",
                menu.read_only.len() - shown
            ));
        }
    }

    md.push_str(&hover_source_line());

    Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: md,
        },
    }
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

/// Render the standard-verb card for a verb token.
fn verb_hover(verb: &str) -> Hover {
    let role = verb_role(verb);
    let mut sentence = role.to_string();
    if let Some(first) = sentence.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: format!("**{verb}**\n\n{sentence}.{}", hover_source_line()),
        },
    }
}

/// Resolve a slash-joined token (`/a/b/verb`) whose whole text is not a menu.
///
/// RouterOS accepts both `/a/b verb` and `/a/b/verb`, but only the former
/// makes the verb a separate word; the segment under the cursor decides what
/// to show: on a trailing standard verb whose parent resolves, render the
/// same verb card a space-joined command gets; otherwise render the deepest
/// known menu prefix ending at the cursor segment. Returns `None` when no
/// prefix resolves — hover never invents a menu for an unknown path.
fn hover_slash_path(
    data: &MenuData,
    word: &str,
    word_start: usize,
    character: usize,
) -> Option<Hover> {
    let segments = slash_segment_spans(word);
    if segments.is_empty() {
        return None;
    }
    // Byte offset within `word`; a cursor on a `/` or past the end belongs
    // to the segment it follows.
    let rel = character.saturating_sub(word_start).min(word.len());
    let mut cursor_seg = segments.len() - 1;
    for (idx, &(_, end)) in segments.iter().enumerate() {
        if rel <= end {
            cursor_seg = idx;
            break;
        }
    }

    if cursor_seg + 1 == segments.len() {
        let (start, end) = segments[cursor_seg];
        let verb = &word[start..end];
        let parent = &word[..start];
        if MenuData::STANDARD_VERBS
            .iter()
            .any(|v| v.eq_ignore_ascii_case(verb))
            && find_menu(data, parent).is_some()
        {
            return Some(verb_hover(verb));
        }
    }

    for idx in (0..=cursor_seg).rev() {
        let end = segments[idx].1;
        if let Some(menu) = find_menu(data, &word[..end]) {
            return Some(menu_hover_card(&menu.path, menu));
        }
    }
    None
}

/// `(start, end)` byte spans of the non-empty `/`-segments in `word`.
fn slash_segment_spans(word: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, byte) in word.bytes().enumerate() {
        if byte == b'/' {
            if let Some(s) = start.take() {
                spans.push((s, idx));
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(s) = start {
        spans.push((s, word.len()));
    }
    spans
}

/// Hover card for the VALUE token of a `name=value` property.
///
/// Returns `None` unless the cursor sits on the value part of a property
/// token whose key resolves to an argument of the current menu context:
/// property keys keep their own card, and unattributable values (unknown
/// keys/menus, positional arguments, comments, bare expressions) stay
/// silent. Quoted values behave like unquoted ones — outer quotes are
/// stripped for matching and for the card label.
fn value_hover(
    data: &MenuData,
    line: &str,
    word_start: usize,
    word_end: usize,
    context: &LineContext,
) -> Option<Hover> {
    // Only a resolvable menu/command context can attribute a value.
    let menu = find_menu(data, &context.path)?;
    let token = crate::parser::tokenize_with_spans(line)
        .into_iter()
        .find(|t| t.start <= word_start && word_end <= t.end)?;
    let (key, raw_value) = crate::parser::split_key_value(&token.text)?;
    // Cursor on the key part of the token: the property card owns it.
    if word_start < token.start + key.len() + 1 {
        return None;
    }
    let bare = value_text(raw_value);
    if bare.is_empty() {
        return None;
    }
    let arg = menu
        .arguments
        .iter()
        .find(|a| normalize_key(&a.name) == normalize_key(key))?;
    let display = display_value(&bare);
    if bare.starts_with('$') {
        return Some(variable_value_card(&display));
    }
    if (arg.arg_type == "bool" || arg.arg_type == "boolean")
        && let Some(card) = boolean_value_card(&bare, &display)
    {
        return Some(card);
    }
    // `alt` wrappers can still carry an enum member list (`new-mss`), so the
    // embedded values decide in addition to the `enum` type prefix.
    if arg.arg_type.starts_with("enum") || !arg.enum_values.is_empty() {
        return Some(enum_value_card(arg, &display));
    }
    Some(typed_value_card(arg, &display))
}

/// Sanitized, length-bounded value text for a card label.
fn display_value(value: &str) -> String {
    truncate_chars(
        &crate::text_util::collapse_controls(value),
        MAX_MENU_ARG_DESC_CHARS,
    )
}

/// Extract the value text from the raw `key=value` tail.
///
/// The tokenizer stops at whitespace only, so a value glued to enclosing
/// expression syntax keeps it (`address=$WgUla]] = 0)`, `[find …
/// address=$WgUla]`). Unquoted values therefore drop one run of leading
/// openers (`[ ( { , ;`) and trailing closers (`] ) } , ;`); quoted values
/// are cut at their matching closing quote — tolerating an unterminated one,
/// as users type incrementally — and their CONTENT is never delimiter
/// trimmed (`comment="a]b"` keeps the bracket, `comment="hi"]` loses the
/// structural `]`).
fn value_text(raw_value: &str) -> String {
    let peeled = raw_value
        .trim()
        .trim_start_matches(['[', '(', '{', ',', ';'])
        .trim_end_matches([']', ')', '}', ',', ';']);
    let Some(quote) = peeled.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return peeled.to_string();
    };
    let rest = &peeled[quote.len_utf8()..];
    let mut escaped = false;
    for (idx, c) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return rest[..idx].to_string();
        }
    }
    // Unterminated string: keep everything after the opening quote.
    rest.to_string()
}

/// Boolean literal card (`yes/no/true/false/on/off`); `None` for any other
/// spelling so the caller can fall back to the typed card.
fn boolean_value_card(value: &str, display: &str) -> Option<Hover> {
    let enables = match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => true,
        "no" | "false" | "off" => false,
        _ => return None,
    };
    let sentence = if enables {
        "Boolean value — enables the option"
    } else {
        "Boolean value — disables the option"
    };
    Some(Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: format!("**{display}**\n\n{sentence}{}", hover_source_line()),
        },
    })
}

/// `$variable` value card: minimal, honest about the runtime resolution.
fn variable_value_card(display: &str) -> Hover {
    Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: format!(
                "**{display}**\n\nVariable reference — resolved at runtime{}",
                hover_source_line()
            ),
        },
    }
}

/// Enum-typed value card with the accepted members, bounded to
/// [`MAX_HOVER_PROPERTIES`] with an explicit `(+N more)` footer.
fn enum_value_card(arg: &ArgEntry, display: &str) -> Hover {
    let typ = if arg.arg_type.is_empty() {
        "any"
    } else {
        arg.arg_type.as_str()
    };
    let mut md = format!("**{display}**\n\nValue of `{}`\n\nType: `{typ}`", arg.name);
    let members = arg.enum_members_ref();
    if !members.is_empty() {
        let shown: Vec<&str> = members
            .iter()
            .take(MAX_HOVER_PROPERTIES)
            .map(String::as_str)
            .collect();
        md.push_str(&format!("\n\nValues: {}", shown.join(" | ")));
        if members.len() > MAX_HOVER_PROPERTIES {
            md.push_str(&format!(
                "\n\n(+{} more)",
                members.len() - MAX_HOVER_PROPERTIES
            ));
        }
    }
    md.push_str(&hover_source_line());
    Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: md,
        },
    }
}

/// Non-enum typed value card: value + owning property + declared type and,
/// when the type has one, the shared example hint.
fn typed_value_card(arg: &ArgEntry, display: &str) -> Hover {
    let typ = if arg.arg_type.is_empty() {
        "any"
    } else {
        arg.arg_type.as_str()
    };
    let mut md = format!("**{display}**\n\nValue of `{}=`\n\nType: `{typ}`", arg.name);
    if let Some(example) = example_for(&arg.arg_type) {
        md.push_str(&format!("\n\n{example}"));
    }
    md.push_str(&hover_source_line());
    Hover {
        contents: HoverContents {
            kind: "markdown".to_string(),
            value: md,
        },
    }
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
///
/// Documentation comes from the shared [`crate::script_globals`] table so
/// hover and completion can never disagree about a builtin.
fn colon_builtin_doc(colon_word: &str) -> Option<&'static str> {
    crate::script_globals::lookup(colon_word).map(|g| g.docs)
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

    // Rebuild context from the full document at the cursor position so that
    // multiline commands (properties on next lines) are correctly resolved.
    // Built before the menu-path branch because value attribution needs it
    // and a `/`-leading VALUE (`comment=/ip`) must not render as a menu.
    let before_cursor = crate::build_before_cursor(full_doc, cursor_line, character);
    let context = crate::parse_line(data, &before_cursor);

    // Value token of a `name=value` property: checked before the key-side
    // branches because a value may spell a menu path or an argument name.
    if let Some(card) = value_hover(data, line, word_start, word_end, &context) {
        return Some(card);
    }

    // Check if it's a menu path (case-insensitive; display keeps typed casing)
    if word.starts_with('/') {
        if let Some(menu) = find_menu(data, word) {
            return Some(menu_hover_card(word, menu));
        }
        // A slash-joined command packs path + verb into one token, so the
        // whole word never matches a menu; peel segments around the cursor.
        if let Some(card) = hover_slash_path(data, word, word_start, character) {
            return Some(card);
        }
    }

    // Check if it's a property name for the current menu.

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
                let (clean, was_truncated) =
                    sanitize_markdown_for_hover_with_truncation(&arg.description);
                md.push_str(&format!("\n\n{clean}"));
                if was_truncated {
                    md.push_str(" (truncated)");
                }
            }
            if let Some(ex) = example_for(&arg.arg_type) {
                md.push_str(&format!("\n\n{ex}"));
            }
            md.push_str(&hover_source_line());
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
            let mut md = if flag.description.is_empty() {
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
            md.push_str(&hover_source_line());
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: md,
                },
            });
        }
    }

    // Space-separated sub-menu segment (`/interface bridge`): `parse_line`
    // folds a COMPLETE segment into `context.path`, while a mid-word hover
    // sees only the partial token as a command. Resolve the word against the
    // context path's children first, then against the path's final segment.
    if !word.starts_with('/') && !context.path.is_empty() {
        let child_path = format!("{}/{}", context.path.trim_end_matches('/'), word);
        if let Some(menu) = find_menu(data, &child_path) {
            return Some(menu_hover_card(&menu.path, menu));
        }
        if let Some(last) = context.path.rsplit('/').next()
            && last.eq_ignore_ascii_case(word)
            && let Some(menu) = find_menu(data, &context.path)
        {
            return Some(menu_hover_card(&menu.path, menu));
        }
    }

    // Check if it's a standard verb (case-insensitive: RouterOS verbs are case-insensitive)
    if MenuData::STANDARD_VERBS
        .iter()
        .any(|v| word.eq_ignore_ascii_case(v))
    {
        return Some(verb_hover(word));
    }

    // `:`-prefixed script keywords (`:put`, `:if`, ...). The extracted `word`
    // excludes the colon by design; `colon_word` above re-attaches it locally.
    if let Some(cw) = colon_word {
        if let Some(doc) = colon_builtin_doc(&cw) {
            return Some(Hover {
                contents: HoverContents {
                    kind: "markdown".to_string(),
                    value: format!("**{cw}**\n\n{doc}{}", hover_source_line()),
                },
            });
        }
        // Fallback for other `:keyword` forms: still a script command.
        return Some(Hover {
            contents: HoverContents {
                kind: "markdown".to_string(),
                value: format!("**{cw}**\n\nScript command.{}", hover_source_line()),
            },
        });
    }

    None
}
