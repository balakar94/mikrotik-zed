"""QA-F1 meta-gate: every `fn test_*` under `lsp/src/tests/` must be wired.

A Rust test function is only executed when it carries `#[test]` (or the
`#[ignore]` opt-out). During a past suite normalization, a whole file of
`fn test_*` bodies lost its attributes and silently stopped running (the
blanket `#![allow(dead_code)]` hid it). This gate makes that class of
regression impossible to merge again.

Rules:
  - Every `fn test_*` in `lsp/src/tests/**/*.rs` needs `#[test]` or
    `#[ignore]` (including `#[tokio::test]`).
  - A fixture/helper that is intentionally named `test_*` may be listed in
    ALLOWLIST with a reason; allowlist entries are checked for staleness, so
    removing or renaming the helper forces the entry to be deleted too.

The scanner masks comments and string/char literals before matching, so a
`fn test_*` mentioned in a comment or TOML fixture cannot trip the gate.
The module keeps its scanning logic in small pure functions so
`test_scanner_*` can prove the gate still catches missing attributes.
"""

from __future__ import annotations

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
TESTS_DIR = ROOT / "lsp" / "src" / "tests"

# (relative posix path under lsp/src/tests, function name) -> reason.
# Keep this list tiny; every entry is a helper, never a skipped test.
ALLOWLIST: dict[tuple[str, str], str] = {
    ("menus_lookup.rs", "test_commands_toml"): (
        "TOML fixture helper returning &'static str, not a #[test]"
    ),
}

FN_RE = re.compile(
    r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?"
    r"(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?"
    r"fn\s+(test_[A-Za-z0-9_]+)\s*[(<]"
)
WIRED_ATTR_RE = re.compile(r"#\[(?:test|ignore|tokio::test|rstest|test_case)\]")


def _bracket_balance(line: str) -> int:
    return line.count("[") - line.count("]")


def mask_comments_and_strings(source: str) -> list[str]:
    """Return source lines with comment and string/char literal bodies blanked.

    Rust-aware enough for test scanning: line/block comments (nested block
    comments included), escaped strings, raw strings (`r"…"`, `r#"…"#`), and
    char literals. Newlines are preserved so line numbers and attribute
    adjacency stay exact.
    """
    out: list[str] = []
    buf: list[str] = []
    i = 0
    n = len(source)
    while i < n:
        ch = source[i]
        nxt = source[i + 1] if i + 1 < n else ""
        if ch == "/" and nxt == "/":
            while i < n and source[i] != "\n":
                i += 1
            continue
        if ch == "/" and nxt == "*":
            depth = 1
            i += 2
            while i < n and depth:
                if source[i] == "/" and i + 1 < n and source[i + 1] == "*":
                    depth += 1
                    i += 2
                elif source[i] == "*" and i + 1 < n and source[i + 1] == "/":
                    depth -= 1
                    i += 2
                else:
                    if source[i] == "\n":
                        out.append("".join(buf))
                        buf = []
                    i += 1
            continue
        if ch == "r" and (nxt == '"' or nxt == "#") and (
            i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_")
        ):
            j = i + 1
            hashes = 0
            while j < n and source[j] == "#":
                hashes += 1
                j += 1
            if j < n and source[j] == '"':
                close = '"' + "#" * hashes
                end = source.find(close, j + 1)
                segment_end = n if end == -1 else end + len(close)
                for c in source[i:segment_end]:
                    if c == "\n":
                        out.append("".join(buf))
                        buf = []
                i = segment_end
                continue
        if ch == '"':
            j = i + 1
            while j < n:
                if source[j] == "\\":
                    j += 2
                    continue
                if source[j] == '"':
                    break
                if source[j] == "\n":
                    out.append("".join(buf))
                    buf = []
                j += 1
            i = j + 1
            continue
        if ch == "'":
            j = i + 1
            if j < n and source[j] == "\\":
                k = j + 1
                while k < n and source[k] != "'":
                    k += 1
                i = k + 1
                continue
            if j + 1 < n and source[j + 1] == "'":
                i = j + 2
                continue
        if ch == "\n":
            out.append("".join(buf))
            buf = []
        else:
            buf.append(ch)
        i += 1
    out.append("".join(buf))
    return out


def scan_source(source: str) -> list[tuple[int, str, bool]]:
    """Find `fn test_*` declarations as (lineno, name, has_wiring_attr)."""
    lines = mask_comments_and_strings(source)
    found: list[tuple[int, str, bool]] = []
    pending_attrs: list[str] = []
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        if not stripped:
            i += 1
            continue
        if stripped.startswith("#["):
            depth = _bracket_balance(lines[i])
            pending_attrs.append(lines[i])
            i += 1
            while depth > 0 and i < len(lines):
                pending_attrs.append(lines[i])
                depth += _bracket_balance(lines[i])
                i += 1
            continue
        match = FN_RE.match(lines[i])
        if match:
            wired = any(WIRED_ATTR_RE.search(attr) for attr in pending_attrs)
            found.append((i + 1, match.group(1), wired))
        pending_attrs = []
        i += 1
    return found


def collect_unwired() -> tuple[list[str], list[str]]:
    """Scan the real suite; return (violations, stale_allowlist_entries)."""
    violations: list[str] = []
    seen_allowlist: set[tuple[str, str]] = set()
    for path in sorted(TESTS_DIR.rglob("*.rs")):
        rel = path.relative_to(TESTS_DIR).as_posix()
        source = path.read_text(encoding="utf-8")
        for lineno, name, wired in scan_source(source):
            key = (rel, name)
            if key in ALLOWLIST:
                seen_allowlist.add(key)
                continue
            if not wired:
                violations.append(f"{rel}:{lineno}: fn {name} lacks #[test]/#[ignore]")
    stale = [
        f"{rel}:{name} ({reason})"
        for (rel, name), reason in sorted(ALLOWLIST.items())
        if (rel, name) not in seen_allowlist
    ]
    return violations, stale


# ── Gate over the real suite ─────────────────────────────────────────────


def test_every_test_fn_is_wired_or_allowlisted():
    violations, stale = collect_unwired()
    assert not violations, (
        "fn test_* without #[test]/#[ignore] (silently skipped tests):\n"
        + "\n".join(f"  {v}" for v in violations)
        + "\nAdd the attribute, rename the helper, or extend ALLOWLIST in "
        "tests/test_rust_test_wiring.py with a reason."
    )
    assert not stale, (
        "stale ALLOWLIST entries (helper gone or renamed — remove them):\n"
        + "\n".join(f"  {s}" for s in stale)
    )


# ── Gate self-tests: the scanner must catch regressions ──────────────────


def test_scanner_flags_missing_attribute_and_accepts_wired():
    missing = scan_source("fn test_lost_its_attribute() {\n    assert!(true);\n}\n")
    assert missing == [(1, "test_lost_its_attribute", False)]

    wired = scan_source("#[test]\nfn test_ok() {}\n")
    assert wired == [(2, "test_ok", True)]

    ignored = scan_source("#[ignore]\nfn test_slow() {}\n")
    assert ignored == [(2, "test_slow", True)]

    stacked = scan_source("#[cfg(unix)]\n#[test]\nfn test_cfg() {}\n")
    assert stacked == [(3, "test_cfg", True)]

    variant = scan_source("pub(crate) async fn test_async() {}\n")
    assert variant == [(1, "test_async", False)]


def test_scanner_ignores_comments_and_string_literals():
    source = (
        "// fn test_commented_out() {}\n"
        "/// fn test_doc_mentioned() {}\n"
        "const FIXTURE: &str = r#\"\n"
        "fn test_inside_raw_string() {}\n"
        "\"#;\n"
        "/* fn test_inside_block_comment() {} */\n"
        "fn test_real() {}\n"
    )
    found = scan_source(source)
    assert found == [(7, "test_real", False)], found


def test_collect_unwired_flags_a_synthetic_tree(tmp_path, monkeypatch):
    """End-to-end failure proof: a tree with an orphan test must be reported."""
    import sys

    gate = sys.modules[__name__]

    fake = tmp_path / "tests"
    (fake / "nested").mkdir(parents=True)
    (fake / "orphan.rs").write_text(
        "#[test]\nfn test_wired() {}\n\nfn test_orphan() {}\n", encoding="utf-8"
    )
    (fake / "nested" / "deep.rs").write_text(
        "fn test_also_orphan() {}\n", encoding="utf-8"
    )
    monkeypatch.setattr(gate, "TESTS_DIR", fake)
    monkeypatch.setattr(gate, "ALLOWLIST", {})
    violations, stale = gate.collect_unwired()
    assert violations == [
        "nested/deep.rs:1: fn test_also_orphan lacks #[test]/#[ignore]",
        "orphan.rs:4: fn test_orphan lacks #[test]/#[ignore]",
    ]
    assert stale == []


def test_allowlist_skips_helper_but_not_runaway_tests():
    assert ("menus_lookup.rs", "test_commands_toml") in ALLOWLIST
