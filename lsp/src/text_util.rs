// ── Shared text utilities (single owner for deduped helpers) ─────────────
//
// `hover.rs`, `completion.rs`, and `signature.rs` previously carried three
// divergent copies of the markdown sanitizer + strip helpers, two copies of
// `normalize_key` / `verb_role` / `type_gloss` / `collapse_controls`, and
// three private copies of the display-budget caps. All of that lives here
// now; the feature modules keep thin `pub(crate) use` re-exports so their
// tests and call sites keep resolving unchanged.
//
// Verb-glossary reconciliation: `hover` used the default arm
// `"standard RouterOS command"` while `signature` used
// `"is a standard RouterOS command"`. The canonical arm below is the
// signature phrasing (`"is a …"`), because the signature header template
// (`"`{path} {verb}` (…) — {verb} {role}`") needs the copula to read
// grammatically. Hover capitalizes the first letter to form a sentence
// (`"Is a standard RouterOS command."`), which reads fine.

/// Max chars for any single-line completion `detail` string.
pub(crate) const MAX_DETAIL_CHARS: usize = 256;

/// Max chars kept for the type half embedded in a completion `detail`.
pub(crate) const MAX_DETAIL_TYPE_CHARS: usize = 64;

/// Cap on properties shown in a menu hover card; the remainder collapses
/// into a "(+N more — see completion)" footer.
pub(crate) const MAX_HOVER_PROPERTIES: usize = 12;

/// Max description chars embedded in hover markdown after sanitizing.
pub(crate) const MAX_HOVER_DESC_CHARS: usize = 800;

/// Max chars kept for the type half of a `name=type` signature label segment.
pub(crate) const MAX_LABEL_TYPE_CHARS: usize = 64;

/// Total signature label budget (~4KiB). Enforced by stopping segment
/// appends, never by cutting mid-segment (offsets stay exact).
pub(crate) const MAX_SIGNATURE_LABEL_BYTES: usize = 4096;

/// Lowercase lookup key; display casing is always preserved from the dataset.
pub(crate) fn normalize_key(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Canonical lookup key for a RouterOS menu path.
///
/// RouterOS paths are case-insensitive and its console accepts leading,
/// trailing and repeated `/` separators, while the embedded dataset stores
/// exactly one lowercase, separator-canonical key per menu. Every path
/// comparison against `menu_by_path`, `ancestor_prefixes` or
/// `child_names_by_parent` must therefore go through this key. The original
/// string is never replaced: callers keep it for messages, ranges, hover
/// text and completion labels.
///
/// An all-separator input canonicalizes to `/` when it starts with a slash
/// (`/`, `//`), and to the empty string otherwise.
pub(crate) fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        out.push('/');
        for ch in segment.chars() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    if out.is_empty() && path.starts_with('/') {
        return "/".to_string();
    }
    out
}

/// One-line verb glossary (lowercase verb → predicate phrase).
///
/// Canonical default arm: `"is a standard RouterOS command"` (see module
/// docs for the reconciliation rationale).
pub(crate) fn verb_role(verb: &str) -> &'static str {
    match verb.to_ascii_lowercase().as_str() {
        "add" => "creates a new entry",
        "remove" => "deletes entries by number or ID",
        "set" => "mutates an existing entry via selector (number, ID, or [find])",
        "get" => "reads one property value from an entry",
        "print" => "lists entries (read-only)",
        "enable" => "enables disabled entries",
        "disable" => "disables entries without deleting them",
        "find" => "returns IDs matching a filter, for use in [find]",
        "comment" => "attaches a comment to entries",
        "move" => "reorders entries by position",
        "export" => "dumps configuration to script form",
        "import" => "runs a script file to restore configuration",
        "edit" => "opens an entry in the interactive editor",
        "reset" => "restores default values",
        "force-update" => "forces a check for RouterOS updates",
        _ => "is a standard RouterOS command",
    }
}

/// Human-readable gloss for raw upstream types. The raw type stays in
/// backticks; this gloss is appended after it.
pub(crate) fn type_gloss(arg_type: &str) -> Option<&'static str> {
    if arg_type.starts_with("iface") {
        Some("interface name — from device (Live) or type manually")
    } else if arg_type.starts_with("ipPrefix") {
        Some("IP prefix (address + mask)")
    } else if arg_type.starts_with("ipAddr") || arg_type == "address" {
        Some("IP address")
    } else if arg_type == "bool" || arg_type == "boolean" {
        Some("yes | no (true/false also accepted)")
    } else {
        None
    }
}

/// Replace each run of ASCII controls with one space.
///
/// Single-line labels/details can never break layout; normal inputs pass
/// through unchanged.
pub(crate) fn collapse_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_gap = false;
    for ch in s.chars() {
        if ch.is_ascii_control() {
            if !in_gap {
                out.push(' ');
                in_gap = true;
            }
        } else {
            in_gap = false;
            out.push(ch);
        }
    }
    out
}

/// Single-line detail scrub: each run of ASCII controls (including
/// `\r`, `\n`, `\t`) becomes one space, trimmed, then capped at
/// [`MAX_DETAIL_CHARS`] chars with a trailing `…`.
pub(crate) fn sanitize_detail_text(s: &str) -> String {
    let trimmed = collapse_controls(s).trim().to_string();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        trimmed
    } else {
        let kept: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{kept}…")
    }
}

/// Build one sanitized `name=type` label segment.
///
/// Control runs become a single space per run so the single-line label can
/// never break layout; the type half is capped at [`MAX_LABEL_TYPE_CHARS`]
/// chars at a char boundary. Normal `name`/`type` inputs pass through
/// unchanged, so `label[start..end]` still slices exactly the segment.
pub(crate) fn sanitize_label_segment(name: &str, typ: &str) -> String {
    let clean_name = collapse_controls(name);
    let clean_type = collapse_controls(typ);
    let capped: String = clean_type.chars().take(MAX_LABEL_TYPE_CHARS).collect();
    format!("{clean_name}={capped}")
}

/// Sanitize upstream description text before embedding in hover markdown.
///
/// Truncate-then-strip (F10): the raw input is pre-truncated to
/// [`MAX_HOVER_DESC_CHARS`] chars (char boundary, trailing `…`) so a long
/// tag or link span cannot evade the strippers, then:
/// - `![alt](url)` image markup is dropped entirely.
/// - `[text](url)` links rewrite to `text` (no length/newline gate — the
///   pre-truncation already bounds the match window; URLs never survive).
/// - `<...>` spans are stripped, including across newlines; a lone `<`
///   with no closing `>` is kept literally.
/// - ASCII controls except `\n` are stripped (popup/log injection guard).
/// - Runs of 3+ newlines collapse to 2.
/// - Output is capped again at [`MAX_HOVER_DESC_CHARS`] chars.
///
/// Pure, no I/O; normal prose passes through unchanged.
pub(crate) fn sanitize_markdown_for_hover(desc: &str) -> String {
    let pre = truncate_chars(desc, MAX_HOVER_DESC_CHARS);
    let no_images = strip_markdown_images(&pre);
    let no_links = strip_markdown_links(&no_images);
    let no_tags = strip_angle_tags(&no_links);
    let no_controls: String = no_tags
        .chars()
        .filter(|c| !c.is_ascii_control() || *c == '\n')
        .collect();
    let collapsed = collapse_newlines(&no_controls);
    truncate_chars(&collapsed, MAX_HOVER_DESC_CHARS)
}

/// Drop `![alt](url)` spans entirely.
fn strip_markdown_images(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rest) = s.get(i..) {
        debug_assert!(s.is_char_boundary(i));
        if rest.starts_with("![")
            && let Some(after_bracket) = s.get(i + 2..)
            && let Some(close) = after_bracket.find("](")
            && let Some(after) = s.get(i + 2 + close + 2..)
            && let Some(end) = after.find(')')
        {
            i += 2 + close + 2 + end + 1;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == '\0' {
            break;
        }
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Rewrite `[text](url)` spans to `text`.
///
/// No length/newline gate: the caller pre-truncates input
/// (truncate-then-strip), so the match window is already bounded and a
/// `](` pair must still be present for a rewrite. URLs never survive
/// into the popup.
fn strip_markdown_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rest) = s.get(i..) {
        debug_assert!(s.is_char_boundary(i));
        if rest.as_bytes().first() == Some(&b'[')
            && let Some(after_open) = s.get(i + 1..)
            && let Some(close) = after_open.find("](")
            && let Some(after) = s.get(i + 1 + close + 2..)
            && let Some(end) = after.find(')')
        {
            out.push_str(&after_open[..close]);
            i = i + 1 + close + 2 + end + 1;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == '\0' {
            break;
        }
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Strip `<...>` spans (upstream HTML/tag debris), including across
/// newlines.
///
/// Every `<` with a later `>` is stripped regardless of span length or
/// embedded newlines — a multiline `<script>\n…\n</script>` must not survive
/// into the popup. A lone `<` with no closing `>` is kept literally. The
/// caller pre-truncates input (truncate-then-strip), so a runaway `<`
/// without a nearby `>` can eat at most the bounded prefix.
fn strip_angle_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(rest) = s.get(i..) {
        debug_assert!(s.is_char_boundary(i));
        if rest.as_bytes().first() == Some(&b'<')
            && let Some(end) = rest.find('>')
        {
            i += end + 1;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch == '\0' {
            break;
        }
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

/// Collapse runs of 3+ newlines to exactly 2.
fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for ch in s.chars() {
        if ch == '\n' {
            run += 1;
            if run <= 2 {
                out.push(ch);
            }
        } else {
            run = 0;
            out.push(ch);
        }
    }
    out
}

/// Truncate to `max` chars at a char boundary, appending `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…")
}
