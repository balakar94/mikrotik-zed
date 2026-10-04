#!/usr/bin/env python3
"""
Fail when the WASM extension shim uses std APIs that do not work on
`wasm32-wasip2`.

Why this exists: those calls compile. `cargo check --target wasm32-wasip2`,
`clippy`, and `make check-wasm` all pass, then the call panics at runtime
inside Zed. That is exactly how `std::process::id()` reached
`cache::unique_tag` and aborted `language_server_command` on the first
auto-download (0.7.0); the fix kept the pid native-only. No Rust lint catches
this class, because the implementation is `unimplemented!()`, not a missing
symbol.

Scope: the extension crate under `src/` (the WASM component Zed loads from
`extension.wasm`). The native `rsc-ls` server under `lsp/` is out of scope and
may use all of std.

Rule: a banned fully-qualified std path may not appear in code compiled for
wasm32-wasip2. Two guards are honored and skipped: `#[cfg(test)]` modules
(tests run natively) and `#[cfg(not(target_arch = "wasm32"))]` items/blocks
(they are not part of the wasm build). A line carrying
`// wasm-api-ok: <reason>` is a deliberate, reviewed exception. Other `cfg`
shapes are treated as production code.

Limitations: matching is textual and fully-qualified (`std::...`), which is
this crate's convention; a call reached through a `use` alias is not caught.

Usage:
  python3 scripts/check_wasm_api_usage.py [--src DIR]

Exit codes:
  0  no banned usage
  1  violations found
  2  IO or usage error

Stdlib only; requires Python >= 3.12.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import NamedTuple

# Repo root = parent of scripts/, independent of the current working directory.
REPO_ROOT = Path(__file__).resolve().parent.parent

DEFAULT_SRC = REPO_ROOT / "src"

# Reviewed-exception marker: a line ending with this may use a banned API.
OPT_OUT = "// wasm-api-ok:"


class Ban(NamedTuple):
    """One std API that cannot run in the wasm32-wasip2 component."""

    label: str
    pattern: re.Pattern[str]
    rationale: str


BANNED: list[Ban] = [
    Ban(
        label="std::process::id",
        pattern=re.compile(r"\bstd::process::id\b"),
        rationale=(
            "unimplemented on wasm32-wasip2 and panics at runtime; "
            "see platform::process_seed"
        ),
    ),
    Ban(
        label="std::process::Command",
        pattern=re.compile(r"\bstd::process::(?:Command|Child)\b"),
        rationale=(
            "the component has no process model; use the zed_extension_api host calls"
        ),
    ),
    Ban(
        label="std::env::var",
        pattern=re.compile(r"\bstd::env::(?:var|vars)\b"),
        rationale=(
            "the component is sandboxed from the process environment; "
            "use Worktree::shell_env"
        ),
    ),
    Ban(
        label="std::thread::spawn",
        pattern=re.compile(r"\bstd::thread::(?:spawn|Builder|scope)\b"),
        rationale=(
            "the default wasm32-wasip2 target is single-threaded; "
            "spawning threads is unavailable"
        ),
    ),
    Ban(
        label="std::net",
        pattern=re.compile(r"\bstd::net::"),
        rationale=(
            "the component has no sockets; network I/O goes through "
            "host capabilities (e.g. zed::download_file)"
        ),
    ),
]


class Violation(NamedTuple):
    """A banned API found in code compiled for the wasm build."""

    path: Path
    line: int
    label: str
    rationale: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.label}: {self.rationale}"


# Char literal only (one character or one escape): lifetimes such as
# `'static` never match because they have no closing quote.
_CHAR_LITERAL = re.compile(r"'(?:\\.|[^'\\])'")

# Guards whose guarded region is absent from the wasm build: `#[cfg(test)]`
# (native tests) and `#[cfg(not(target_arch = "wasm32"))]` (native-only code).
# Matched against the raw line with `re.match`, so a commented-out attribute
# never triggers and the `"wasm32"` literal survives string stripping.
_SKIPPED_GUARDS = (
    re.compile(r"^\s*#\[cfg\(\s*test\s*\)\]"),
    re.compile(r'^\s*#\[cfg\(\s*not\(\s*target_arch\s*=\s*"wasm32"\s*\)\s*\)\]'),
)


def _is_skipped_guard(raw: str) -> bool:
    """True when `raw` opens a region excluded from the wasm build."""
    return any(guard.match(raw) for guard in _SKIPPED_GUARDS)


def _code_only(line: str) -> str:
    """Return `line` with string/char literals and comments blanked out.

    Removing them keeps brace counting honest: format strings like
    `format!("{a}-{b}")` and URLs inside strings must not be read as code.
    """
    out: list[str] = []
    i = 0
    n = len(line)
    while i < n:
        char = line[i]
        if char == "/" and i + 1 < n and line[i + 1] == "/":
            break
        if char == "/" and i + 1 < n and line[i + 1] == "*":
            end = line.find("*/", i + 2)
            i = n if end == -1 else end + 2
            continue
        if char == '"':
            i += 1
            while i < n and line[i] != '"':
                i += 2 if line[i] == "\\" else 1
            i += 1
            continue
        if char == "'":
            match = _CHAR_LITERAL.match(line, i)
            if match:
                i = match.end()
                continue
        out.append(char)
        i += 1
    return "".join(out)


def _iter_production(text: str) -> list[tuple[int, str, str]]:
    """Return `(lineno, code, raw)` for lines compiled into the wasm build.

    A `#[cfg(test)]` or `#[cfg(not(target_arch = "wasm32"))]` guard opens a
    skipped region: the guarded item or block runs from its opening `{` until
    the matching closing brace returns the depth to where the guard started.
    """
    rows: list[tuple[int, str, str]] = []
    depth = 0
    pending_guard = False
    skip_depth: int | None = None
    for lineno, raw in enumerate(text.splitlines(), start=1):
        code = _code_only(raw)
        opens = code.count("{")
        closes = code.count("}")
        if skip_depth is not None:
            depth += opens - closes
            if depth <= skip_depth:
                skip_depth = None
            continue
        guarded = pending_guard or _is_skipped_guard(raw)
        if guarded and "{" in code:
            # The guarded region opens here; it ends when the braces balance.
            skip_depth = depth
            pending_guard = False
            depth += opens - closes
            if depth <= skip_depth:
                skip_depth = None
            continue
        if _is_skipped_guard(raw):
            pending_guard = True
            depth += opens - closes
            continue
        pending_guard = False
        rows.append((lineno, code, raw))
        depth += opens - closes
    return rows


def _has_opt_out(raw: str) -> bool:
    """True when `raw` carries the opt-out marker with a non-empty reason."""
    if OPT_OUT not in raw:
        return False
    return bool(raw.split(OPT_OUT, 1)[1].strip())


def scan_text(text: str, path: Path) -> list[Violation]:
    """Return every banned-API use in `text`, skipping tests and opt-outs."""
    violations: list[Violation] = []
    for lineno, code, raw in _iter_production(text):
        for ban in BANNED:
            if ban.pattern.search(code) and not _has_opt_out(raw):
                violations.append(Violation(path, lineno, ban.label, ban.rationale))
    return violations


def scan_file(path: Path) -> list[Violation]:
    """Return every banned-API use in one Rust source file."""
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise OSError(f"{path}: unreadable: {exc}") from exc
    return scan_text(text, path)


def run(argv: list[str]) -> int:
    """Run the gate; return the process exit code (0 ok, 1 violations, 2 usage)."""
    parser = argparse.ArgumentParser(
        description="Reject std APIs that work natively but panic on wasm32-wasip2."
    )
    parser.add_argument("--src", default=str(DEFAULT_SRC), help="source directory to scan")
    args = parser.parse_args(argv)

    src = Path(args.src)
    if not src.is_dir():
        print(f"error: source dir not found: {src}", file=sys.stderr)
        return 2

    files = sorted(p for p in src.rglob("*.rs") if p.is_file())
    if not files:
        print(f"error: no *.rs files under {src}", file=sys.stderr)
        return 2

    violations: list[Violation] = []
    for path in files:
        try:
            violations.extend(scan_file(path))
        except OSError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 2

    if not violations:
        print(f"wasm API usage OK ({len(files)} files scanned)")
        return 0

    for violation in violations:
        print(violation.render(), file=sys.stderr)
    print(
        f"error: {len(violations)} banned API use(s) in the WASM shim. "
        "These compile for wasm32-wasip2 but abort at runtime in Zed. "
        f"For a reviewed exception, append `{OPT_OUT} <reason>` to the line.",
        file=sys.stderr,
    )
    return 1


def main() -> int:
    return run(sys.argv[1:])


if __name__ == "__main__":
    sys.exit(main())
