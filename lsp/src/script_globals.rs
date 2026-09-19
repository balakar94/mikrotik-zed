// ── RouterOS script globals (`:verb` builtins) ───────────────────────────
//
// Single source of truth shared by hover and completion: each entry carries
// the canonical label (leading colon, as users type it), a short one-line
// `detail`, and the hover documentation sentence. Keeping one table means a
// builtin can never be documented in hover and missing from completion (or
// vice versa). Lookups are case-insensitive because RouterOS keywords are.
//
// Scope: the common script-language commands plus the structural statements
// completion already offers as snippets. Menu commands (`/ip/...`) are NOT
// here — they come from `data/commands.toml`.

/// One `:`-prefixed script builtin.
pub(crate) struct ScriptGlobal {
    /// Canonical label, e.g. `:put`.
    pub(crate) label: &'static str,
    /// Short completion detail (single line, no backticks).
    pub(crate) detail: &'static str,
    /// Hover documentation sentence; backticked code spans are allowed.
    pub(crate) docs: &'static str,
}

/// Curated RouterOS script globals, in offer order.
pub(crate) const SCRIPT_GLOBALS: &[ScriptGlobal] = &[
    ScriptGlobal {
        label: ":if",
        detail: "conditional block",
        docs: ":if (<cond>) do={...} — conditional execution.",
    },
    ScriptGlobal {
        label: ":foreach",
        detail: "iterate over a list",
        docs: ":foreach <var> in=<list> do={...} — iterate over a list.",
    },
    ScriptGlobal {
        label: ":for",
        detail: "counted loop",
        docs: ":for <var> from=<n> to=<m> — counted loop.",
    },
    ScriptGlobal {
        label: ":do",
        detail: "post-test loop",
        docs: ":do {...} while=(<cond>) — group commands.",
    },
    ScriptGlobal {
        label: ":local",
        detail: "declare local variable",
        docs: ":local <name> [<value>] — declare a local variable.",
    },
    ScriptGlobal {
        label: ":global",
        detail: "declare global variable",
        docs: ":global <name> [<value>] — declare or access a global variable.",
    },
    ScriptGlobal {
        label: ":put",
        detail: "output to console",
        docs: ":put <value> — output values to the console.",
    },
    ScriptGlobal {
        label: ":return",
        detail: "return from script",
        docs: ":return [<value>] — return a value from a script.",
    },
    ScriptGlobal {
        label: ":error",
        detail: "raise a script error",
        docs: ":error <message> — raise a script error.",
    },
    ScriptGlobal {
        label: ":delay",
        detail: "pause execution",
        docs: ":delay <seconds> — pause execution.",
    },
    ScriptGlobal {
        label: ":resolve",
        detail: "DNS lookup",
        docs: ":resolve <host> — resolve a DNS name to an address.",
    },
    ScriptGlobal {
        label: ":parse",
        detail: "parse commands from text",
        docs: ":parse <text> — parse console commands from text.",
    },
    ScriptGlobal {
        label: ":pick",
        detail: "slice string or array",
        docs: ":pick <value> <start> [<end>] — slice a string or array.",
    },
    ScriptGlobal {
        label: ":tonum",
        detail: "convert to number",
        docs: ":tonum <value> — convert a value to a number.",
    },
    ScriptGlobal {
        label: ":totime",
        detail: "convert to time",
        docs: ":totime <value> — convert a value to a time interval.",
    },
];

/// Case-insensitive lookup by `:label` (a bare `label` is accepted too).
///
/// Returns `None` for an empty bare name so callers keep their fallbacks.
pub(crate) fn lookup(word: &str) -> Option<&'static ScriptGlobal> {
    let bare = word.strip_prefix(':').unwrap_or(word);
    if bare.is_empty() {
        return None;
    }
    SCRIPT_GLOBALS
        .iter()
        .find(|g| g.label.len() > 1 && g.label[1..].eq_ignore_ascii_case(bare))
}
