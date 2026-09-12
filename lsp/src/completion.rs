// ── Completion logic for the RSC language server ─────────────────────────
//
// Relevance-ranked completion with `textEdit` shadows. Strategy:
// - Rank every candidate by deterministic tier (`0!live_` device truth
//   first, then required/optional/verb/sub-menu/enum/hint/placeholder/
//   flag/typo/snippet) and match quality (exact, prefix, substring),
//   and truncate at `MAX_COMPLETION_ITEMS` AFTER sorting.
// - Exception: when the cursor sits inside a "property=value" token,
//   switch to value suggestions (enum values, booleans, curated common
//   values, type hints) ranked by a pure relevance function
//   ([`rank`]) over the typed suffix: exact match first, then prefix,
//   then substring. A typed suffix matching nothing keeps the full set
//   as a demoted fallback (tier `8`, detail hint) instead of an empty
//   menu — the client fuzzy matcher remains the authority.
// - Exception: when the cursor sits inside a `:`-prefixed token (`:` is a
//   completion trigger character), keep only candidates whose label starts
//   with that typed token — script globals and statement snippets. Menu
//   paths and property names make no sense after a colon.
//
// Relevance model (ranking): every non-root item carries a
// deterministic `sortText` of the form `<tier><match>_<normalized-label>`
// (legacy shapes `0name` / `1name` / `9*` are preserved when no prefix is
// typed; live values use `0!live_<label>` — the `!` (0x21) sorts before any
// alphanumeric second char, so device truth always ranks above required
// properties regardless of label text). Tier order: `0!live_` (device
// truth) < `0` required property < `1` optional property < `2` verb < `3`
// sub-menu < `4` true enum/bool value < `5` curated common value < `6`
// type placeholder < `7` flag < `8` demoted typo fallback < `9` snippet.
// Truncation at `MAX_COMPLETION_ITEMS` applies AFTER relevance sorting, so
// the first 200 items are the most relevant ones, not the first ones
// constructed.
//
// Snippet-order invariant: every statement snippet shares the single tier
// key `9`; their curated table order (`STATEMENT_SNIPPETS`) is preserved
// ONLY because the final sort (`sort_by`, stable) keeps equal keys in
// construction order. Never replace it with an unstable sort, and never
// give snippets distinct keys, without updating the golden tests.
//
// `filterText` is always populated from the label so clients never match
// against snippet bodies (`address=$1$0`). `textEdit` is populated at this
// layer ONLY for value items and sub-menu/verb items, with a single-line
// (line 0) range assumption: the server rewrites those ranges with the
// physical/logical-line mapping at runtime, so they are unit-test shadows
// here. Property/flag items intentionally carry NO `textEdit` from this
// layer — the server has no positional injector for those kinds and a
// line-0 guess would corrupt multi-line documents (see Risks).

use crate::caps::MAX_COMPLETION_ITEMS;
use crate::live::{LiveCache, live_resource_values_for_property};
use crate::menus::{ArgEntry, LineContext, MenuData};

/// LSP CompletionItemKind values (mirrors the LSP spec)
pub(crate) mod kind {
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

/// LSP range for `textEdit`.
#[derive(serde::Serialize, Clone, Debug)]
pub struct CompletionPosition {
    pub line: u32,
    pub character: u32,
}

/// LSP range for `textEdit`.
#[derive(serde::Serialize, Clone, Debug)]
pub struct CompletionRange {
    pub start: CompletionPosition,
    pub end: CompletionPosition,
}

/// LSP `TextEdit` for `CompletionItem.textEdit`.
///
/// When present, the client replaces `range` with `newText` instead of
/// inserting `insertText` at the cursor. `insertText` is retained as a
/// fallback for clients that ignore `textEdit` (Zed supports it).
#[derive(serde::Serialize, Clone, Debug)]
pub struct TextEdit {
    pub range: CompletionRange,
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// A completion item ready for JSON serialization.
///
/// Newer optional fields (`documentation`, `sortText`, `filterText`) are
/// omitted from the JSON when unset instead of serialized as null —
/// semantically identical for LSP clients and keeps the payload small.
/// Pre-existing optional fields keep their historical null-emitting shape
/// for wire compatibility.
/// `filterText` mirrors `label` so clients match against the visible name
/// rather than the snippet body. `textEdit` is optional: when `Some`, it
/// replaces the typed prefix (e.g. the suffix after `=` or a partial menu
/// token) so that accepting `input` when `in` is already typed yields
/// `input` rather than `ininput`. When `None`, the client falls back to
/// `insertText` at the cursor.
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
    #[serde(rename = "filterText", skip_serializing_if = "Option::is_none")]
    pub filter_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<Documentation>,
    #[serde(rename = "textEdit", skip_serializing_if = "Option::is_none")]
    pub text_edit: Option<TextEdit>,
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
            filter_text: None,
            documentation: None,
            text_edit: None,
        }
    }
}

// ── Relevance ranking (pure, deterministic) ──────────────────────────────

// Shared text helpers live in `crate::text_util` (single owner).
use crate::text_util::{
    MAX_DETAIL_TYPE_CHARS, normalize_key, normalize_path, sanitize_detail_text,
    sanitize_markdown_for_hover,
};

/// Relevance tier of a completion candidate.
///
/// The tier prefix is the major sort key inside `sortText`; see the module
/// header for the full order. Variants are ordered by their prefix so the
/// declaration itself documents the ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RankTier {
    /// Device truth from the live cache (`0!live_<name>` — the `!` major
    /// key sorts before every `0<name>` required property under the
    /// lexicographic `sortText` ordering clients apply).
    Live,
    /// Required property (`0<name>`).
    RequiredProp,
    /// Optional property (`1<name>`).
    OptionalProp,
    /// Standard verb or action command (`2…`).
    Verb,
    /// Sub-menu (`3…`).
    Submenu,
    /// Documented enum/bool member (`4…`).
    EnumValue,
    /// Curated common value, e.g. firewall chains (`5…`).
    CommonHint,
    /// Honest type placeholder such as `0.0.0.0/0` (`6…`).
    Placeholder,
    /// Single-letter flag (`7…`).
    Flag,
    /// Typo fallback: typed prefix matched nothing (`8…`).
    Demoted,
    /// Statement snippet (`9…`).
    Snippet,
}

impl RankTier {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            RankTier::Live => "0!live_",
            RankTier::RequiredProp => "0",
            RankTier::OptionalProp => "1",
            RankTier::Verb => "2",
            RankTier::Submenu => "3",
            RankTier::EnumValue => "4",
            RankTier::CommonHint => "5",
            RankTier::Placeholder => "6",
            RankTier::Flag => "7",
            RankTier::Demoted => "8",
            RankTier::Snippet => "9",
        }
    }
}

/// Match quality of a candidate label against the typed prefix.
///
/// `'0'` exact (case-insensitive) < `'1'` prefix < `'2'` substring < `'3'`
/// no match. An empty typed prefix is neutral (`'1'` for every candidate).
fn match_rank(label: &str, typed_prefix: &str) -> char {
    if typed_prefix.is_empty() {
        return '1';
    }
    let label_key = normalize_key(label);
    let typed_key = normalize_key(typed_prefix);
    if label_key == typed_key {
        '0'
    } else if label_key.starts_with(&typed_key) {
        '1'
    } else if label_key.contains(&typed_key) {
        '2'
    } else {
        '3'
    }
}

/// Pure relevance rank: deterministic `sortText` for one candidate.
///
/// Inputs are the tier (caller-assigned by item kind), the visible label,
/// and the already-typed prefix in the current token. No I/O, no global
/// state — identical inputs always yield identical output, so truncation
/// after sorting is reproducible.
///
/// Ordering rules:
/// - Live uses the frozen shape `0!live_<label>` in BOTH the empty and
///   typed cases: `typed_prefix` is deliberately ignored so cache
///   (device-recency) order survives filtering, and the `!` second byte
///   (0x21, below every alphanumeric) keeps device truth above required
///   properties under client lexicographic ordering regardless of label.
/// - Snippets share one tier key (`9`): their table offer order is curated
///   UX, preserved by the stable sort (see the module-header invariant).
/// - Required/optional properties keep their legacy `0name` / `1name`
///   shapes when nothing is typed; a typed prefix adds the match rank
///   (`0<match>_<name>`) so partial names boost inside the tier. The
///   empty-vs-typed shapes are frozen: only the documented arms below may
///   produce them (golden-tested).
/// - Sub-menus, verbs, and flags always embed the label: their
///   construction order is not deterministic (hash-index iteration), so an
///   alphabetical key is what makes the output reproducible.
/// - Enum/bool members, common hints, and placeholders keep construction
///   (document/curated) order when unfiltered and embed the match rank plus
///   label once a prefix is typed.
pub(crate) fn rank(tier: RankTier, label: &str, typed_prefix: &str) -> String {
    match tier {
        RankTier::Live => format!("0!live_{label}"),
        RankTier::Snippet => RankTier::Snippet.prefix().to_string(),
        RankTier::RequiredProp if typed_prefix.is_empty() => format!("0{label}"),
        RankTier::OptionalProp if typed_prefix.is_empty() => format!("1{label}"),
        RankTier::Demoted => format!("8_{}", normalize_key(label)),
        RankTier::Submenu | RankTier::Verb | RankTier::Flag => format!(
            "{}{}_{}",
            tier.prefix(),
            match_rank(label, typed_prefix),
            normalize_key(label)
        ),
        _ if typed_prefix.is_empty() => tier.prefix().to_string(),
        _ => format!(
            "{}{}_{}",
            tier.prefix(),
            match_rank(label, typed_prefix),
            normalize_key(label)
        ),
    }
}

/// Curated common values for the `chain` property.
///
/// Upstream documents `chain` as a bare `enum` (chains are user-definable),
/// so the embedded table carries no members and value completion would stay
/// silent. These three built-in chains are offered one tier BELOW true enum
/// members and one tier ABOVE type placeholders, with an explicit
/// verify-on-device detail — a hint, never fabricated device truth.
/// `iface`-typed properties stay silent unless Live provides values (no
/// fabrication there either).
const COMMON_CHAIN_VALUES: [&str; 3] = ["input", "forward", "output"];

/// Detail text for curated common-value hints.
pub(crate) const COMMON_HINT_DETAIL: &str = "common value — verify on device";

/// Detail suffix marking a demoted typo fallback set.
const FALLBACK_HINT_SUFFIX: &str = " (no prefix match — showing all values)";

// Display-budget caps (`MAX_DETAIL_*`) and text sanitizers
// (`sanitize_detail_text`, `sanitize_label_segment`,
// `sanitize_markdown_for_hover`) live in `crate::text_util` (single owner)
// and are re-exported at the top of this module; no local copies remain.

/// Test-only shim over [`compute_completions_with_live`] with no live cache.
///
/// Production callers use `compute_completions_with_live`; this wrapper
/// exists so unit, golden, and gate tests exercise the static path without
/// threading `None` through every call site. Kept `pub` (not `#[cfg(test)]`)
/// because white-box sanitizer tests also call it.
#[allow(dead_code)]
pub fn compute_completions(data: &MenuData, before_cursor: &str) -> Vec<CompletionItem> {
    compute_completions_with_live(data, before_cursor, None)
}

/// Live-aware completion entry point. When `live_cache` is `Some` and the
/// property being completed is live-enrichable (`interface`/`bridge`/
/// `actual-interface` or any `iface`-typed argument), live interface names
/// are merged into the value suggestions (see [`get_value_completions_with_live`]).
pub fn compute_completions_with_live(
    data: &MenuData,
    before_cursor: &str,
    live_cache: Option<&LiveCache>,
) -> Vec<CompletionItem> {
    let context = crate::parse_line(data, before_cursor);

    let mut items = match_context_with_live(data, &context, before_cursor, live_cache);

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
    // RouterOS is case-insensitive, so match case-insensitively for both
    // properties and colon globals.
    if let Some(typed) = colon_token {
        let lower = typed.to_ascii_lowercase();
        items.retain(|item| item.label.to_ascii_lowercase().starts_with(&lower));
    }

    // Cap the final payload to keep the response bounded; live enrichment
    // has already been merged above so its items participate in the cap
    // rather than being appended after it. Items are STABLY sorted by
    // relevance (`sortText`) BEFORE truncating so the first
    // `MAX_COMPLETION_ITEMS` are the most relevant candidates, not merely
    // the first ones constructed; equal keys keep construction order
    // (snippet table order, enum document order). Every `sortText` is
    // deterministic (see [`rank`]), so truncation is reproducible.
    // `filterText` is backfilled from the label so clients match against
    // the visible name instead of snippet bodies.
    for item in &mut items {
        if item.filter_text.is_none() {
            item.filter_text = Some(item.label.clone());
        }
    }
    items.sort_by(|a, b| match (&a.sort_text, &b.sort_text) {
        // Stable sort: equal keys keep construction order (snippet table
        // order, enum document order, curated hint order). No label
        // tie-break here — it would alphabetize those curated orders.
        (Some(x), Some(y)) => x.cmp(y),
        (None, None) => a.label.cmp(&b.label),
        (None, _) => std::cmp::Ordering::Less,
        (_, None) => std::cmp::Ordering::Greater,
    });
    if items.len() > MAX_COMPLETION_ITEMS {
        items.truncate(MAX_COMPLETION_ITEMS);
    }

    items
}

fn match_context_with_live(
    data: &MenuData,
    context: &LineContext,
    before_cursor: &str,
    live_cache: Option<&LiveCache>,
) -> Vec<CompletionItem> {
    // No path yet (or a bare "/") → suggest root menus. "/" parses to path
    // "/" which has no child index entry of its own, so it must be treated
    // as the root trigger it is.
    if context.path.is_empty() || context.path == "/" {
        return get_root_completion_items(data);
    }

    // Typing a property VALUE inside the current token ("chain=" or the
    // partial "chain=in") → suggest enum/bool/type values filtered by the
    // already-typed suffix. Tolerant of trailing whitespace after `=` ("chain= ")
    // so that a space does not suppress value suggestions when the value is
    // still empty; the trimmed last token is inspected instead of requiring
    // the cursor to sit strictly inside the token. If the suffix is already
    // non-empty and the cursor sits AFTER whitespace ("chain=input "), the
    // token is considered finished and we fall through to argument completions.
    let trimmed = before_cursor.trim_end();
    let has_trailing_ws = trimmed.len() != before_cursor.len();
    let trimmed_last = crate::parser::tokenize(trimmed)
        .last()
        .cloned()
        .unwrap_or_default();
    if let Some((key_part, value_part)) = crate::parser::split_key_value(&trimmed_last) {
        let trimmed_suffix = value_part.trim_matches(|c| c == '"' || c == '\'');
        // If trailing whitespace present with a non-empty value, the value
        // token is finished — suggest next property, not values.
        if !has_trailing_ws || trimmed_suffix.is_empty() {
            let key = key_part.trim_start_matches(':').to_string();
            let typed_suffix = value_part.to_string();
            let effective = trimmed_suffix.to_string();
            let mut items =
                get_value_completions_with_live(data, context, &key, live_cache, &effective);
            items = filter_by_typed_prefix(items, &typed_suffix);
            attach_value_text_edit(&mut items, before_cursor, &typed_suffix);
            return items;
        }
    }

    // If a verb is already typed (e.g., "add", "print"), only suggest
    // arguments — no more sub-menus or verbs.  This matches real RouterOS
    // terminal behavior where Tab after "add" shows property completions.
    // A partial property name under the cursor (`… add act`) boosts the
    // matching properties via `sortText`/`filterText` but never filters the
    // set: the client fuzzy matcher stays the authority.
    if context.command.is_some() {
        let partial = partial_name_token(before_cursor).filter(|(text, _, _)| {
            context
                .command
                .as_deref()
                .is_none_or(|cmd| !text.eq_ignore_ascii_case(cmd))
        });
        let typed = partial.as_ref().map(|(t, _, _)| t.as_str()).unwrap_or("");
        return get_arg_completion_items(data, context, typed);
    }

    // Before a verb: suggest sub-menus + standard verbs, relevance-ranked
    // against a partial token under the cursor (`…/ip addr` aside, parser
    // usually consumes space-separated partials as the command, so this
    // mainly orders the unfiltered menu; any genuine partial still gets a
    // replacing `textEdit` on prefix-matching items).
    let partial = partial_name_token(before_cursor);
    let typed = partial
        .as_ref()
        .map(|(t, _, _)| t.clone())
        .unwrap_or_default();
    let span = partial.map(|(_, s, e)| (s, e));
    let mut items = Vec::new();
    items.extend(get_sub_menu_completion_items(data, context, &typed));
    items.extend(get_verb_completion_items(data, context, &typed));
    attach_token_text_edit(&mut items, span, &typed);
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
pub(crate) fn statement_snippet_items() -> Vec<CompletionItem> {
    STATEMENT_SNIPPETS
        .iter()
        .map(|s| {
            let mut item = CompletionItem::new(s.label.to_string(), kind::SNIPPET);
            item.detail = Some("statement snippet".to_string());
            item.insert_text = Some(s.snippet.to_string());
            item.insert_text_format = Some(2); // Snippet
            item.sort_text = Some(rank(RankTier::Snippet, s.label, ""));
            item.documentation = Some(Documentation {
                kind: "markdown",
                value: s.doc.to_string(),
            });
            item
        })
        .collect()
}

/// Case-insensitive relevance filter over candidate labels using the value
/// text the user already typed.
///
/// Surrounding quote characters on the typed suffix are ignored so partial
/// input like `chain="in` still filters to `input`. An empty effective
/// prefix returns the candidates unchanged (their `sortText` already encodes
/// the neutral order). A non-empty prefix keeps exact, prefix, AND
/// substring matches — ordered by the `sortText` rank the builders assigned
/// — so the client's fuzzy matcher refines an already relevance-ordered
/// set. A prefix matching NOTHING falls back to the full set (the client's
/// own fuzzy matcher remains the authority) but demotes every item to the
/// lowest tier ([`RankTier::Demoted`]) with a detail hint, so a zero-prefix
/// typo no longer floods an undifferentiated menu.
fn filter_by_typed_prefix(items: Vec<CompletionItem>, typed_suffix: &str) -> Vec<CompletionItem> {
    let trimmed = typed_suffix.trim_matches(|c| c == '"' || c == '\'');
    if trimmed.is_empty() {
        return items;
    }
    let lower = normalize_key(trimmed);
    let matched: Vec<CompletionItem> = items
        .iter()
        .filter(|i| {
            let label_key = normalize_key(&i.label);
            label_key.starts_with(&lower) || label_key.contains(&lower)
        })
        .cloned()
        .collect();
    if matched.is_empty() {
        // Nothing matches — fall back to the unfiltered set rather than
        // returning zero items for a typo'd prefix, but demote it to the
        // lowest tier with a hint so the flooding is visible as such.
        items
            .into_iter()
            .map(|mut item| {
                item.sort_text = Some(rank(RankTier::Demoted, &item.label, ""));
                let hinted = match item.detail.take() {
                    Some(d) => format!("{d}{FALLBACK_HINT_SUFFIX}"),
                    None => FALLBACK_HINT_SUFFIX.trim_start().to_string(),
                };
                item.detail = Some(hinted);
                item
            })
            .collect()
    } else {
        matched
    }
}

// ── Single-line textEdit shadows (unit level) ────────────────────────────
//
// The builders below know only `before_cursor`, never the cursor's line
// number, so the ranges here assume line 0. That is exact for pure
// single-line callers and unit tests; the server rewrites value and
// sub-menu/verb ranges with its physical/logical-line mapping at runtime,
// making these shadows invisible on the wire for those kinds.
// Property/flag items deliberately get NO shadow here: the server has no
// positional injector for those kinds, so a line-0 guess could reach the
// wire and corrupt multi-line documents.

/// The last physical line of `before_cursor` — completion edits apply here.
fn current_line(before_cursor: &str) -> &str {
    before_cursor.rsplit('\n').next().unwrap_or(before_cursor)
}

/// Line-0 `TextEdit` replacing `[start_byte, end_byte)` with `new_text`.
///
/// Byte offsets are exact for ASCII test inputs; the server recomputes them
/// per encoding at runtime.
fn line_zero_edit(start_byte: usize, end_byte: usize, new_text: String) -> TextEdit {
    let (start, end) = if start_byte <= end_byte {
        (start_byte, end_byte)
    } else {
        (end_byte, end_byte)
    };
    TextEdit {
        range: CompletionRange {
            start: CompletionPosition {
                line: 0,
                character: start as u32,
            },
            end: CompletionPosition {
                line: 0,
                character: end as u32,
            },
        },
        new_text,
    }
}

/// Partial bare-word token under the cursor on the current line, if any.
///
/// Returns `(typed_text, byte_start, byte_end)` relative to the current
/// line when the cursor sits mid-token (no trailing whitespace) and the
/// last token looks like a partial property/verb/sub-menu name: it carries
/// no `=`, and does not start with `/ : " ' ( [ $`. Quote-aware spans keep
/// quoted text and block syntax out of name territory.
pub(crate) fn partial_name_token(before_cursor: &str) -> Option<(String, usize, usize)> {
    if before_cursor.ends_with(char::is_whitespace) {
        return None;
    }
    let line = current_line(before_cursor);
    if line.is_empty() {
        return None;
    }
    let tokens = crate::parser::tokenize_with_spans(line);
    let tok = tokens.last().cloned()?;
    if tok.text.contains('=') {
        return None;
    }
    if tok.text.starts_with(['/', ':', '"', '\'', '(', '[', '$']) {
        return None;
    }
    if tok.text.is_empty() {
        return None;
    }
    let end = tok.end.min(line.len());
    let start = tok.start.min(end);
    Some((tok.text, start, end))
}

/// Attach replacing `textEdit`s for the typed value suffix after `=`.
///
/// The range covers exactly the effective suffix (a leading opening quote
/// is preserved: `"in` → `"input`), so accepting `input` when `in` is typed
/// replaces instead of appending (`ininput`). A finished value followed by
/// whitespace, or an empty suffix, yields a zero-length insertion edit at
/// the cursor.
fn attach_value_text_edit(items: &mut [CompletionItem], before_cursor: &str, typed_suffix: &str) {
    let line = current_line(before_cursor);
    let effective = typed_suffix.trim_matches(|c| c == '"' || c == '\'');
    let (start, end) = if effective.is_empty() {
        (line.len(), line.len())
    } else {
        (line.len().saturating_sub(effective.len()), line.len())
    };
    for item in items.iter_mut() {
        let new_text = item
            .insert_text
            .clone()
            .unwrap_or_else(|| item.label.clone());
        item.text_edit = Some(line_zero_edit(start, end, new_text));
    }
}

/// Attach replacing `textEdit`s to prefix-matching sub-menu/verb items.
///
/// `span`/`typed` come from [`partial_name_token`]; `None`/empty means the
/// previous token is finished and every item stays a pure insertion.
fn attach_token_text_edit(items: &mut [CompletionItem], span: Option<(usize, usize)>, typed: &str) {
    let Some((start, end)) = span else {
        return;
    };
    if typed.is_empty() {
        return;
    }
    let lower = normalize_key(typed);
    for item in items.iter_mut() {
        let is_name_kind = item.kind == Some(kind::CLASS) || item.kind == Some(kind::FUNCTION);
        if is_name_kind && normalize_key(&item.label).starts_with(&lower) {
            let new_text = item
                .insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone());
            item.text_edit = Some(line_zero_edit(start, end, new_text));
        }
    }
}

// ── Root menus ───────────────────────────────────────────────────────────

fn get_root_completion_items(data: &MenuData) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Directories / settings directories via the parent index (fast path).
    // The synthetic root index always creates Directory entries, even for
    // Command roots like `/import`. Overwrite the kind/detail when the real
    // menu type (from `menu_by_path`) is `Command` so `/import` etc. appear
    // as `FUNCTION`/`Command` instead of `CLASS`.
    if let Some(roots) = data.child_names_by_parent.get("") {
        for r in roots {
            let real_type = data
                .menu_by_path
                .get(&r.path)
                .map(|m| m.menu_type.as_str())
                .unwrap_or(r.menu_type.as_str());
            if real_type == "Command" {
                let mut item = CompletionItem::new(r.path.clone(), kind::FUNCTION);
                item.detail = Some("Command".to_string());
                item.insert_text = Some(r.path.clone());
                item.insert_text_format = Some(1);
                items.push(item);
            } else {
                let mut item = CompletionItem::new(r.path.clone(), kind::CLASS);
                item.detail = Some(sanitize_detail_text(&format!("root menu — {}", r.path)));
                item.insert_text = Some(r.path.clone());
                item.insert_text_format = Some(1);
                items.push(item);
            }
            seen.insert(r.path.clone());
        }
    }
    // Root-level Commands that have no synthetic entry (defensive; should be
    // rare because the synthetic index creates an entry for every root) are
    // added here so `/import`, `/quit`, `/beep`, `/put`, etc. are never
    // missing even when child_names_by_parent is incomplete.
    for (path, menu) in &data.menu_by_path {
        if path == "/" {
            continue;
        }
        if path.matches('/').count() != 1 {
            continue;
        }
        if menu.menu_type != "Command" {
            continue;
        }
        if seen.contains(path) {
            continue;
        }
        let mut item = CompletionItem::new(path.clone(), kind::FUNCTION);
        item.detail = Some("Command".to_string());
        item.insert_text = Some(path.clone());
        item.insert_text_format = Some(1);
        items.push(item);
        seen.insert(path.clone());
    }
    // Deterministic order: sort by label for stable tests.
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

// ── Sub-menus ────────────────────────────────────────────────────────────

fn get_sub_menu_completion_items(
    data: &MenuData,
    ctx: &LineContext,
    typed_prefix: &str,
) -> Vec<CompletionItem> {
    match data.child_names_by_parent.get(&normalize_path(&ctx.path)) {
        Some(children) => children
            .iter()
            .filter(|c| c.menu_type == "Directory" || c.menu_type == "Settings Directory")
            .map(|c| {
                let mut item = CompletionItem::new(c.name.clone(), kind::CLASS);
                item.detail = Some(sanitize_detail_text(&format!("sub-menu — {}", c.path)));
                item.insert_text = Some(c.name.clone());
                item.insert_text_format = Some(1);
                item.sort_text = Some(rank(RankTier::Submenu, &c.name, typed_prefix));
                item
            })
            .collect(),
        None => Vec::new(),
    }
}

// ── Verbs ────────────────────────────────────────────────────────────────

fn get_verb_completion_items(
    data: &MenuData,
    ctx: &LineContext,
    typed_prefix: &str,
) -> Vec<CompletionItem> {
    let menu_type = data
        .menu_by_path
        .get(&normalize_path(&ctx.path))
        .map(|m| m.menu_type.as_str())
        .unwrap_or("Directory");
    // Only Directory / Settings Directory menus support the 15 standard verbs.
    // Command menus (e.g. /tool/ping) have no child operations — only their
    // own arguments/flags — so emitting verbs there is noise.
    let is_directory = menu_type == "Directory" || menu_type == "Settings Directory";
    let mut items: Vec<CompletionItem> = if is_directory {
        MenuData::STANDARD_VERBS
            .iter()
            .map(|verb| {
                let mut item = CompletionItem::new(verb.to_string(), kind::FUNCTION);
                item.detail = Some(format!("{verb} — standard command"));
                item.insert_text = Some(verb.to_string());
                item.insert_text_format = Some(1);
                item.sort_text = Some(rank(RankTier::Verb, verb, typed_prefix));
                item
            })
            .collect()
    } else {
        Vec::new()
    };

    // Action commands (type = "Command" entries under this path)
    if let Some(children) = data.child_names_by_parent.get(&normalize_path(&ctx.path)) {
        for child in children {
            if child.menu_type == "Command" {
                let mut item = CompletionItem::new(child.name.clone(), kind::FUNCTION);
                item.detail = Some("action command".to_string());
                item.insert_text = Some(child.name.clone());
                item.insert_text_format = Some(1);
                item.sort_text = Some(rank(RankTier::Verb, &child.name, typed_prefix));
                items.push(item);
            }
        }
    }

    items
}

// ── Arguments ────────────────────────────────────────────────────────────

fn get_arg_completion_items(
    data: &MenuData,
    ctx: &LineContext,
    typed_prefix: &str,
) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&normalize_path(&ctx.path)) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    for arg in &menu.arguments {
        if ctx.properties.contains_key(&normalize_key(&arg.name)) {
            continue; // already used
        }
        let mut item = CompletionItem::new(arg.name.clone(), kind::PROPERTY);
        item.detail = Some(get_detail(arg));
        // Required properties rank before optional ones within the same
        // match quality ("0…" < "1…"); a partial property name under the
        // cursor boosts exact/prefix/substring matches inside each tier.
        // No `textEdit` here by design (see the shadow-edit section): the
        // server owns positional edits and has no injector for this kind.
        let tier = if arg.required {
            RankTier::RequiredProp
        } else {
            RankTier::OptionalProp
        };
        item.sort_text = Some(rank(tier, &arg.name, typed_prefix));
        item.documentation = documentation_from(arg.description.clone());
        item.insert_text = Some(get_insert_text(arg));
        item.insert_text_format = Some(2); // snippet
        items.push(item);
    }

    for flag in &menu.flags {
        let mut item = CompletionItem::new(flag.name.clone(), kind::CONSTANT);
        item.detail = Some(sanitize_detail_text(&format!(
            "{}: {}",
            flag.name, flag.description
        )));
        item.documentation = documentation_from(flag.description.clone());
        item.insert_text = Some(flag.name.clone());
        item.insert_text_format = Some(1);
        item.sort_text = Some(rank(RankTier::Flag, &flag.name, typed_prefix));
        items.push(item);
    }

    items
}

// ── Value completions (inside "property=value" tokens) ───────────────────

fn get_value_completions_with_live(
    data: &MenuData,
    ctx: &LineContext,
    property_key: &str,
    live_cache: Option<&LiveCache>,
    typed_prefix: &str,
) -> Vec<CompletionItem> {
    let menu = match data.menu_by_path.get(&normalize_path(&ctx.path)) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let arg = match menu
        .arguments
        .iter()
        .find(|a| normalize_key(&a.name) == normalize_key(property_key))
    {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut items = Vec::new();

    // Enum values — complete embedded list when present, display-string
    // fallback otherwise (synthetic/test data).
    if arg.arg_type.starts_with("enum") {
        for val in arg.enum_members() {
            let mut item = CompletionItem::new(val.clone(), kind::ENUM_MEMBER);
            item.detail = Some(sanitize_detail_text(&format!(
                "enum value — {}",
                arg.arg_type
            )));
            // Values insert bare: a preceding opening quote in the token is
            // never doubled.
            item.insert_text = Some(val.clone());
            item.insert_text_format = Some(1);
            item.sort_text = Some(rank(RankTier::EnumValue, &val, typed_prefix));
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
            item.sort_text = Some(rank(RankTier::EnumValue, val, typed_prefix));
            items.push(item);
        }
    }

    // Curated common values for `chain`: upstream documents a bare `enum`
    // (chains are user-definable), so without this the menu would stay
    // silent. Offered one tier below true enum members, deduplicated
    // against live/documented values below. `iface`-typed properties stay
    // silent unless Live provides values — no fabrication.
    if normalize_key(property_key) == "chain" {
        for hint in COMMON_CHAIN_VALUES {
            let already = items.iter().any(|it| it.label.eq_ignore_ascii_case(hint));
            if !already {
                let mut item = CompletionItem::new(hint.to_string(), kind::ENUM_MEMBER);
                item.detail = Some(COMMON_HINT_DETAIL.to_string());
                item.insert_text = Some(hint.to_string());
                item.insert_text_format = Some(1);
                item.sort_text = Some(rank(RankTier::CommonHint, hint, typed_prefix));
                items.push(item);
            }
        }
    }

    // IP address / prefix — one honest placeholder per actual type.
    if arg.arg_type.starts_with("ipPrefix") {
        items.push(ip_placeholder(arg, "0.0.0.0/0", typed_prefix));
    } else if arg.arg_type.starts_with("ipAddr") || arg.arg_type == "address" {
        items.push(ip_placeholder(arg, "0.0.0.0", typed_prefix));
    }

    // Live enrichment: merge device values (interfaces, IP addresses, address lists, chains, pools)
    // when the property is live-enrichable and a fresh cache entry exists. Deduplicates against
    // static items and prefers live (sort_text "0!live_<name>" ranks above static placeholders).
    if let Some(cache) = live_cache
        && let Some((resource, live_vals)) =
            live_resource_values_for_property(cache, &ctx.path, property_key, &arg.arg_type)
        && !live_vals.is_empty()
    {
        let live_set: std::collections::HashSet<&String> = live_vals.iter().collect();
        // Prefer live: remove static duplicates. Arc clone is cheap (no 500-item Vec clone per
        // keystroke).
        items.retain(|it| !live_set.contains(&it.label));
        for val in live_vals.iter() {
            let mut item = CompletionItem::new(val.clone(), kind::ENUM_MEMBER);
            item.detail = Some(resource.detail_label().to_string());
            item.insert_text = Some(val.clone());
            item.insert_text_format = Some(1);
            item.sort_text = Some(rank(RankTier::Live, val, ""));
            items.push(item);
        }
    }

    items
}

fn ip_placeholder(arg: &ArgEntry, value: &str, typed_prefix: &str) -> CompletionItem {
    let mut item = CompletionItem::new(value.to_string(), kind::ENUM_MEMBER);
    item.detail = Some(sanitize_detail_text(&format!("type: {}", arg.arg_type)));
    item.insert_text = Some(value.to_string());
    item.insert_text_format = Some(1);
    item.sort_text = Some(rank(RankTier::Placeholder, value, typed_prefix));
    item
}

// ── Helpers ──────────────────────────────────────────────────────────────

pub(crate) fn documentation_from(description: String) -> Option<Documentation> {
    if description.is_empty() {
        None
    } else {
        Some(Documentation {
            kind: "markdown",
            value: sanitize_markdown_for_hover(&description),
        })
    }
}

/// Snippet for a property: `$1` on the value, `$0` as the final tabstop so
/// accepting the completion leaves the cursor at the end of the statement.
///
/// The quotes belong to THIS snippet (`comment="$1"$0`); value completions
/// never re-add them, so an already-typed opening quote is not doubled.
pub(crate) fn get_insert_text(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type == "string" {
        format!("{}=\"{}\"$0", arg.name, "$1")
    } else {
        format!("{}={}$0", arg.name, "$1")
    }
}

pub(crate) fn get_detail(arg: &crate::menus::ArgEntry) -> String {
    if arg.arg_type.is_empty() {
        "property".to_string()
    } else {
        let capped: String = crate::text_util::collapse_controls(&arg.arg_type)
            .chars()
            .take(MAX_DETAIL_TYPE_CHARS)
            .collect();
        sanitize_detail_text(&format!("type: {capped}"))
    }
}
